//! yousj-js · Phase 3：运行时值类型与 ECMAScript 简化语义。
//!
//! - `Value`：Undefined / Null / Bool / Number(f64) / String /
//!   Object / Array / Function / Native（内置函数）。
//! - 对象/数组/函数经 `Rc<RefCell<…>>` 实现引用语义（别名修改互相可见）。
//! - `to_boolean` / `to_number` / `to_js_string` / `==` / `===`
//!   按规范简化实现；Symbol / BigInt 运算等不在子集内。
//! - `FlowError`：解释器内部错误（运行时错误，或用户 `throw` 的值向上传播）。
//! - `Env`：词法作用域环境链（var 提升 / let-const TDZ 在此实现）。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hash, Hasher};
use std::rc::{Rc, Weak};

use crate::ast::{Expr, Param, Span, Stmt};
use crate::bytecode::Chunk;
use crate::dom::DomNode;
use crate::fetch::FetchHost;
use crate::promise::{Microtask, PromiseRef, PromiseSettlerRef};
use crate::regex::RegExpRef;

// ---------------------------------------------------------------------------
// Phase 12：字符串驻留（interning）+ 快速哈希
// ---------------------------------------------------------------------------

/// 驻留字符串：属性键 / 变量名的去重表示。
///
/// - `==` 先比指针（驻留命中时 O(1) 短路），再比内容；
/// - `Hash` 按内容哈希，配合 `Borrow<str>`，`HashMap<IStr, V>` 可直接用
///   `&str` 做键查找，无需调用方先驻留；
/// - 后端是线程局部的驻留表（互异字符串上限 65536，超限后不再驻留新串，
///   已有 `IStr` 保持有效——`Rc` 自持有）。
#[derive(Clone)]
pub struct IStr(Rc<str>);

const INTERN_CAP: usize = 65536;

thread_local! {
    static INTERNER: RefCell<FastMap<Box<str>, Rc<str>>> = RefCell::new(FastMap::default());
}

/// 驻留 `s`（属性键 / 标识符用）。内容相同者共享同一底层分配。
pub fn istr(s: &str) -> IStr {
    INTERNER.with(|cell| {
        let mut m = cell.borrow_mut();
        if let Some(rc) = m.get(s) {
            return IStr(rc.clone());
        }
        let rc: Rc<str> = Rc::from(s);
        if m.len() < INTERN_CAP {
            m.insert(Box::from(&*rc), rc.clone());
        }
        IStr(rc)
    })
}

impl IStr {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for IStr {
    fn from(s: &str) -> Self {
        istr(s)
    }
}

impl std::borrow::Borrow<str> for IStr {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq for IStr {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl Eq for IStr {}

impl Hash for IStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl std::fmt::Debug for IStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IStr({:?})", &*self.0)
    }
}

impl std::fmt::Display for IStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Fx 风格快速哈希（短字符串键用；替代 `RandomState` 的 SipHash，
/// 环境表 / 属性表等高频小 map 的查找更快）。非密码学用途，本引擎不做
/// 网络输入的哈希键，DoS 不是威胁模型。
#[derive(Clone, Default)]
pub struct FxBuild;

#[derive(Clone)]
pub struct FxHasher {
    hash: usize,
}

const FX_SEED: usize = 0x51_7c_c1_b7_27_22_0a_95;

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = (self.hash.rotate_left(5) ^ b as usize).wrapping_mul(FX_SEED);
        }
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write(&i.to_ne_bytes());
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash as u64
    }
}

impl BuildHasher for FxBuild {
    type Hasher = FxHasher;

    fn build_hasher(&self) -> FxHasher {
        FxHasher { hash: FX_SEED }
    }
}

/// 内部高频小 map 的别名（环境槽 / 属性表等）。
pub type FastMap<K, V> = HashMap<K, V, FxBuild>;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// JS 错误子类（phase 8）：运行时错误按规范分类，`Error` 为通用类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Error,
    TypeError,
    ReferenceError,
    SyntaxError,
    RangeError,
}

impl ErrorKind {
    pub fn name(&self) -> &'static str {
        match self {
            ErrorKind::Error => "Error",
            ErrorKind::TypeError => "TypeError",
            ErrorKind::ReferenceError => "ReferenceError",
            ErrorKind::SyntaxError => "SyntaxError",
            ErrorKind::RangeError => "RangeError",
        }
    }
}

/// 调用栈帧（phase 8）：函数名 + 调用点行列号（1-based）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub name: String,
    pub line: usize,
    pub col: usize,
}

impl Frame {
    pub fn new(name: impl Into<String>, line: usize, col: usize) -> Self {
        Frame {
            name: name.into(),
            line,
            col,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    pub message: String,
    pub kind: ErrorKind,
    /// 未捕获时解释器回填的调用栈（内层在前）；顶层抛错时为 None。
    pub stack: Option<Vec<Frame>>,
    /// Phase 14：失控保护错误（步数/任务队列上限）不可被 try/catch 捕获，
    /// 避免 `try{无限循环}catch{}` 把保护吞掉导致真卡死。
    pub uncatchable: bool,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>) -> Self {
        RuntimeError {
            message: message.into(),
            kind: ErrorKind::Error,
            stack: None,
            uncatchable: false,
        }
    }

    pub fn typed(kind: ErrorKind, message: impl Into<String>) -> Self {
        RuntimeError {
            message: message.into(),
            kind,
            stack: None,
            uncatchable: false,
        }
    }

    /// 标记为不可捕获（失控保护用）。
    pub fn uncatchable(mut self) -> Self {
        self.uncatchable = true;
        self
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.message)?;
        if let Some(frames) = &self.stack {
            // 内层在前（V8 惯例）。
            for fr in frames.iter().rev() {
                write!(
                    f,
                    "\n    at {} (<anonymous>:{}:{})",
                    fr.name, fr.line, fr.col
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug, Clone, PartialEq)]
pub enum FlowError {
    Runtime(RuntimeError),
    /// 用户代码 `throw v` 抛出的值（向上传播，可被 try/catch 捕获）。
    Thrown(Value),
}

impl From<RuntimeError> for FlowError {
    fn from(e: RuntimeError) -> Self {
        FlowError::Runtime(e)
    }
}

pub(crate) fn rt(message: impl Into<String>) -> FlowError {
    FlowError::Runtime(RuntimeError::new(message))
}

/// phase 8：按规范分类的错误构造器（`rt` 保留作通用类）。
pub(crate) fn type_err(message: impl Into<String>) -> FlowError {
    FlowError::Runtime(RuntimeError::typed(ErrorKind::TypeError, message))
}

pub(crate) fn ref_err(message: impl Into<String>) -> FlowError {
    FlowError::Runtime(RuntimeError::typed(ErrorKind::ReferenceError, message))
}

pub(crate) fn syntax_err(message: impl Into<String>) -> FlowError {
    FlowError::Runtime(RuntimeError::typed(ErrorKind::SyntaxError, message))
}

pub(crate) fn range_err(message: impl Into<String>) -> FlowError {
    FlowError::Runtime(RuntimeError::typed(ErrorKind::RangeError, message))
}

// ---------------------------------------------------------------------------
// 对象 / 数组
// ---------------------------------------------------------------------------

/// JS 普通对象：按插入顺序存属性（`for-in` 顺序与规范一致）。
#[derive(Debug, Clone, Default)]
pub struct JsObject {
    /// 属性键为驻留字符串（phase 12）：相同属性名共享同一分配。
    props: Vec<(IStr, Value)>,
    /// 内部原型槽（phase 4）：属性查找沿此链向上。
    pub proto: Option<ObjectRef>,
    /// 构造标签（phase 3 的 `instanceof` 简化版用；phase 4 起走原型链，保留作调试）。
    pub tag: Option<String>,
    /// phase 9：私有字段/方法存储，键为 (class_id, 私有名)。
    /// 与公开属性隔离，`for-in`/`Object.keys` 不可见。
    pub privates: FastMap<(u64, IStr), Value>,
    /// phase 9：访问器（getter/setter），键为属性名，值为 (getter, setter)。
    /// 类中的 `get x()`/`set x()` 安装于此；`get_prop`/`set_prop` 优先检查。
    pub accessors: FastMap<IStr, (Option<FuncRef>, Option<FuncRef>)>,
    /// phase 12：类原型 → 构造器函数的弱回指。
    /// 打破 `prototype."constructor"` 与 `ctor.prototype` 的 Rc 强环
    /// （每个类声明原本永久泄漏）；读 `.constructor` 时升级。
    /// 用户显式 `set("constructor", …)` 的普通属性优先于此回指。
    pub ctor_backref: Option<Weak<JsFunction>>,
    /// phase 14：包装对象内部槽（`new String/Number/Boolean(x)`、
    /// `Object(原始值)`）。`to_number`/`to_js_string` 优先取此值，
    /// 实现规范的 ToPrimitive 语义。
    pub primitive: Option<Value>,
}

impl JsObject {
    pub fn new() -> Self {
        JsObject {
            props: Vec::new(),
            proto: None,
            tag: None,
            privates: FastMap::default(),
            accessors: FastMap::default(),
            ctor_backref: None,
            primitive: None,
        }
    }

    pub fn with_proto(proto: Option<ObjectRef>) -> Self {
        JsObject {
            props: Vec::new(),
            proto,
            tag: None,
            privates: FastMap::default(),
            accessors: FastMap::default(),
            ctor_backref: None,
            primitive: None,
        }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        if let Some((_, v)) = self
            .props
            .iter()
            .rev()
            .find(|(k, _)| k.as_str() == key)
        {
            return Some(v.clone());
        }
        // phase 12：类原型的弱构造器回指（用户显式设置的属性优先）。
        if key == "constructor" {
            if let Some(f) = self.ctor_backref.as_ref().and_then(|w| w.upgrade()) {
                return Some(Value::Function(f));
            }
        }
        None
    }

    /// 自身属性 → 原型链逐层查找。
    pub fn get_in_chain(&self, key: &str) -> Option<Value> {
        if let Some(v) = self.get(key) {
            return Some(v);
        }
        let mut p = self.proto.clone();
        while let Some(pr) = p {
            let b = pr.borrow();
            if let Some(v) = b.get(key) {
                return Some(v.clone());
            }
            p = b.proto.clone();
        }
        None
    }

    pub fn set(&mut self, key: &str, val: Value) {
        let key = istr(key);
        if let Some(slot) = self.props.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = val;
        } else {
            self.props.push((key, val));
        }
    }

    pub fn delete(&mut self, key: &str) -> bool {
        let before = self.props.len();
        self.props.retain(|(k, _)| k.as_str() != key);
        self.props.len() != before
    }

    pub fn keys(&self) -> Vec<String> {
        self.props.iter().map(|(k, _)| k.to_string()).collect()
    }

    /// Phase 10：自有属性检查（`Object.hasOwn` 用）。
    pub fn has_own(&self, key: &str) -> bool {
        self.props.iter().any(|(k, _)| k.as_str() == key)
    }
}

#[derive(Debug, Clone, Default)]
pub struct JsArray {
    pub elems: Vec<Value>,
    /// Phase 14：稀疏长度补偿（`new Array(huge)` 不实际分配时设为 huge，
    /// 逻辑长度 = max(elems.len(), sparse_extra)；普通数组为 0）。
    pub sparse_extra: usize,
    /// 数组上的非下标属性（`arr.foo = 1`，少见但合法）。
    pub props: JsObject,
    /// 内部原型槽（phase 4）：通常指向 Array.prototype。
    pub proto: Option<ObjectRef>,
    /// 构造标签（phase 3 遗留；phase 4 起 `instanceof` 走原型链）。
    pub tag: Option<String>,
}

impl JsArray {
    pub fn new(elems: Vec<Value>) -> Self {
        JsArray {
            elems,
            sparse_extra: 0,
            props: JsObject::new(),
            proto: None,
            tag: Some("Array".to_string()),
        }
    }

    pub fn with_proto(elems: Vec<Value>, proto: Option<ObjectRef>) -> Self {
        JsArray {
            elems,
            sparse_extra: 0,
            props: JsObject::new(),
            proto,
            tag: Some("Array".to_string()),
        }
    }

    /// Phase 14：创建稀疏大数组（`new Array(huge)` 不实际分配）。
    pub fn with_sparse_len(len: usize, proto: Option<ObjectRef>) -> Self {
        JsArray {
            elems: Vec::new(),
            sparse_extra: len,
            props: JsObject::new(),
            proto,
            tag: Some("Array".to_string()),
        }
    }

    /// 逻辑长度（稀疏补偿）。
    pub fn logical_len(&self) -> usize {
        self.elems.len().max(self.sparse_extra)
    }

    /// 非下标属性 → 自身 props → 原型链。
    pub fn get_in_chain(&self, key: &str) -> Option<Value> {
        if let Some(v) = self.props.get(key) {
            return Some(v);
        }
        let mut p = self.proto.clone();
        while let Some(pr) = p {
            let b = pr.borrow();
            if let Some(v) = b.get(key) {
                return Some(v.clone());
            }
            p = b.proto.clone();
        }
        None
    }
}

pub type ObjectRef = Rc<RefCell<JsObject>>;
pub type ArrayRef = Rc<RefCell<JsArray>>;

// ---------------------------------------------------------------------------
// 函数
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum FuncBody {
    Block(Vec<Stmt>),
    /// 箭头函数表达式体：`x => x + 1`。
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct JsFunction {
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub body: FuncBody,
    /// 定义时捕获的词法环境（闭包）。
    pub closure: EnvRef,
    pub is_arrow: bool,
    /// 构造器的 `.prototype` 对象（`new` 的实例以此为原型）。
    /// 箭头函数没有 prototype，为 None。
    pub prototype: Option<ObjectRef>,
    /// phase 8：有效严格模式（自身指令序言 || 定义时外层 strict）。
    pub strict: bool,
    /// phase 8：函数定义点位置（调用栈帧无调用点信息时回退用）。
    pub def_span: Span,
    /// phase 9：生成器函数（调用返回 Generator，不直接执行函数体）。
    pub is_generator: bool,
    /// phase 15：async 生成器函数（调用返回 AsyncGenerator）。
    pub is_async_generator: bool,
    /// phase 9：class 构造器（不可直接调用，只能 new）。
    pub is_class: bool,
    /// phase 9：派生类构造器的父构造器（`super(...)` 用）。
    /// 用户类为 Function；内建类（Array/Object）为其构造器 Object；无 extends 为 None。
    pub super_ctor: Option<Value>,
    /// phase 9：方法定义所在的宿主对象（`super.x` 查找用）。
    /// phase 12：弱引用——打破"方法 → home_object(prototype) → 方法属性"
    /// 的 Rc 强环（每个类方法原本永久泄漏）；`super.x` 读取时升级。
    pub home_object: Option<Weak<RefCell<JsObject>>>,
    /// phase 9：定义该函数的类的私有作用域（`#x` 访问权限判定）。
    pub private_scope: Option<PrivateScopeRef>,
    /// phase 9：构造器函数自身的原型（静态继承：extends 时指向父构造器对象）。
    pub ctor_proto: Option<ObjectRef>,
    /// phase 9：所属类的 id（0 = 非类相关函数）。
    pub class_id: u64,
    /// phase 9：实例字段定义（构造器用；含私有字段）。
    pub instance_fields: Vec<ClassField>,
    /// phase 9：无显式构造器的派生类（默认转发所有参数给 super）。
    pub is_default_derived: bool,
    /// phase 9：静态公开成员（`static m(){}`/`static x=1` 存这里，
    /// 因 JsFunction 无 props 槽）。键为驻留字符串（phase 12）。
    pub statics: RefCell<FastMap<IStr, Value>>,
    /// phase 11：字节码 VM——函数体的编译缓存（VM 开启且首次调用时填充）。
    pub vm_chunk: RefCell<Option<Rc<Chunk>>>,
    /// phase 9：静态私有成员（`static #x` 方法/字段），键为 (class_id, 私有名)。
    pub private_statics: RefCell<FastMap<(u64, IStr), Value>>,
}

/// phase 9：类实例字段定义（`x = init` / `#x = init`）。
#[derive(Debug, Clone)]
pub struct ClassField {
    pub name: String,
    pub is_private: bool,
    pub init: Option<Expr>,
}

/// phase 9：类的私有作用域（`#x` 的词法归属）。
#[derive(Debug, Clone)]
pub struct PrivateScope {
    /// 类的唯一 id（实例/静态私有存储的键）。
    pub class_id: u64,
    /// 声明的实例私有名（`#x` → `"x"`）。
    pub instance_names: Vec<String>,
    /// 声明的静态私有名。
    pub static_names: Vec<String>,
}

pub type PrivateScopeRef = Rc<PrivateScope>;

/// phase 9 扩展字段的默认值（普通函数构造时用）。
#[derive(Debug, Clone, Default)]
pub struct Phase9FuncFields {
    pub is_generator: bool,
    /// phase 15：async 生成器（`async function*`）。
    pub is_async_generator: bool,
    pub is_class: bool,
    pub super_ctor: Option<Value>,
    pub home_object: Option<Weak<RefCell<JsObject>>>,
    pub private_scope: Option<PrivateScopeRef>,
    pub ctor_proto: Option<ObjectRef>,
    pub class_id: u64,
    pub instance_fields: Vec<ClassField>,
    pub is_default_derived: bool,
    /// phase 9：类构造器预建的 `.prototype` 对象（有则优先采用）。
    pub prototype_override: Option<ObjectRef>,
    /// phase 9：静态公开成员（存入 JsFunction.statics）。
    pub statics: FastMap<IStr, Value>,
}

impl JsFunction {
    /// 用 phase-9 默认扩展字段补全构造（减少各构造点的重复）。
    pub fn with_ext(
        name: Option<String>,
        params: Vec<Param>,
        body: FuncBody,
        closure: EnvRef,
        is_arrow: bool,
        prototype: Option<ObjectRef>,
        strict: bool,
        def_span: Span,
        ext: Phase9FuncFields,
    ) -> Self {
        JsFunction {
            name,
            params,
            body,
            closure,
            is_arrow,
            prototype,
            strict,
            def_span,
            is_generator: ext.is_generator,
            is_async_generator: ext.is_async_generator,
            is_class: ext.is_class,
            super_ctor: ext.super_ctor,
            home_object: ext.home_object,
            private_scope: ext.private_scope,
            ctor_proto: ext.ctor_proto,
            class_id: ext.class_id,
            instance_fields: ext.instance_fields,
            is_default_derived: ext.is_default_derived,
            statics: RefCell::new(ext.statics),
            private_statics: RefCell::new(FastMap::default()),
            vm_chunk: RefCell::new(None),
        }
    }
}

/// phase 9：生成器对象。
#[derive(Debug, Clone)]
pub struct JsGenerator {
    /// desugar 产生的 `__yousj$step` 闭包（`(sv, ab, abv)` 三参数）。
    pub step: FuncRef,
    /// 生成器调用时的 `this`（step 驱动时传入）。
    pub this_value: Value,
    /// 是否已结束（`done: true` 后的方法调用直接返回）。
    pub done: bool,
    /// 是否已开始（第一次 `next()` 之前 suspended-start）。
    pub started: bool,
}

impl JsGenerator {
    pub fn new(step: FuncRef, this_value: Value) -> Self {
        JsGenerator {
            step,
            this_value,
            done: false,
            started: false,
        }
    }
}

pub type GenRef = Rc<RefCell<JsGenerator>>;

/// phase 15：async 生成器。
#[derive(Debug, Clone)]
pub struct JsAsyncGenerator {
    /// desugar 后的函数体（async 状态机），调用时返回 Promise。
    /// 实际驱动通过 `__yousj$AG` 宿主对象协调。
    pub body_func: FuncRef,
    /// 调用时的 `this`。
    pub this_value: Value,
    /// 调用时的参数。
    pub args: Vec<Value>,
    /// 是否已结束。
    pub done: bool,
    /// 是否已开始（第一次 `next()` 之前 suspended-start）。
    pub started: bool,
    /// 挂起的 next/return/throw 请求队列。
    /// 每个元素为 (kind, arg, resolve, reject)，kind: 0=next, 1=throw, 2=return。
    pub queue: Vec<AsyncGenRequest>,
    /// 已挂起（在 `__yousj$yieldOp` 处等待恢复）：存储 park Promise 的 resolve。
    pub parked: Option<Value>,
}

/// phase 15：async 生成器的挂起请求。
#[derive(Debug, Clone)]
pub struct AsyncGenRequest {
    pub kind: u8, // 0=next, 1=throw, 2=return
    pub arg: Value,
    pub resolve: Value, // Promise 的 resolve 函数
    pub reject: Value,  // Promise 的 reject 函数
}

impl JsAsyncGenerator {
    pub fn new(body_func: FuncRef, this_value: Value, args: Vec<Value>) -> Self {
        JsAsyncGenerator {
            body_func,
            this_value,
            args,
            done: false,
            started: false,
            queue: Vec::new(),
            parked: None,
        }
    }
}

pub type AsyncGenRef = Rc<RefCell<JsAsyncGenerator>>;

/// phase 9：Proxy。
#[derive(Debug, Clone)]
pub struct JsProxy {
    pub target: Value,
    pub handler: ObjectRef,
}

pub type ProxyRef = Rc<RefCell<JsProxy>>;

// ---------------------------------------------------------------------------
// Phase 10：集合与二进制视图类型
// ---------------------------------------------------------------------------

/// Phase 10：`Map` —— 插入序 `Vec<(key, value)>`（SameValueZero 比较）。
#[derive(Debug, Clone)]
pub struct JsMap {
    pub entries: Vec<(Value, Value)>,
}
pub type MapRef = Rc<RefCell<JsMap>>;

/// Phase 10：`Set` —— 插入序 `Vec<Value>`。
#[derive(Debug, Clone)]
pub struct JsSet {
    pub entries: Vec<Value>,
}
pub type SetRef = Rc<RefCell<JsSet>>;

/// Phase 10：弱引用键。`ptr` 做同一性比较；`alive` 探测底层 `Weak`
/// 是否仍可升级（不可则条目视为已回收）。
#[derive(Clone)]
pub struct WeakKey {
    ptr: *const (),
    alive: Rc<dyn Fn() -> bool>,
}

impl WeakKey {
    fn new<T: 'static>(w: std::rc::Weak<T>) -> Self {
        let ptr = w.as_ptr() as *const ();
        WeakKey {
            ptr,
            alive: Rc::new(move || w.upgrade().is_some()),
        }
    }

    /// 从值构造弱键；非对象（及不可弱引用的值）返回 `None`。
    pub fn of(v: &Value) -> Option<WeakKey> {
        match v {
            Value::Object(o) => Some(WeakKey::new(Rc::downgrade(o))),
            Value::Array(a) => Some(WeakKey::new(Rc::downgrade(a))),
            Value::Function(f) => Some(WeakKey::new(Rc::downgrade(f))),
            Value::Promise(p) => Some(WeakKey::new(Rc::downgrade(p))),
            Value::RegExp(r) => Some(WeakKey::new(Rc::downgrade(r))),
            Value::Generator(g) => Some(WeakKey::new(Rc::downgrade(g))),
            Value::AsyncGenerator(g) => Some(WeakKey::new(Rc::downgrade(g))),
            Value::Proxy(p) => Some(WeakKey::new(Rc::downgrade(p))),
            Value::Map(m) => Some(WeakKey::new(Rc::downgrade(m))),
            Value::Set(s) => Some(WeakKey::new(Rc::downgrade(s))),
            Value::WeakMap(m) => Some(WeakKey::new(Rc::downgrade(m))),
            Value::WeakSet(s) => Some(WeakKey::new(Rc::downgrade(s))),
            Value::ArrayBuffer(b) => Some(WeakKey::new(Rc::downgrade(b))),
            Value::TypedArray(t) => Some(WeakKey::new(Rc::downgrade(t))),
            Value::DataView(d) => Some(WeakKey::new(Rc::downgrade(d))),
            Value::Date(d) => Some(WeakKey::new(Rc::downgrade(d))),
            _ => None,
        }
    }

    pub fn is_alive(&self) -> bool {
        (self.alive)()
    }
}

impl std::fmt::Debug for WeakKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WeakKey({:p})", self.ptr)
    }
}

impl PartialEq for WeakKey {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

/// Phase 10：`WeakMap` —— 键为弱引用；清扫摊销（phase 12：每 64 次操作
/// 清扫一次，而非每次操作 O(n) 扫描）。
#[derive(Debug, Clone)]
pub struct JsWeakMap {
    pub entries: Vec<(WeakKey, Value)>,
    pub(crate) sweep_debt: u64,
}
pub type WeakMapRef = Rc<RefCell<JsWeakMap>>;

/// Phase 10：`WeakSet`（清扫摊销同 WeakMap）。
#[derive(Debug, Clone)]
pub struct JsWeakSet {
    pub entries: Vec<WeakKey>,
    pub(crate) sweep_debt: u64,
}
pub type WeakSetRef = Rc<RefCell<JsWeakSet>>;

/// Phase 10：`ArrayBuffer` —— 字节存储；TypedArray/DataView 共享之。
#[derive(Debug, Clone)]
pub struct JsArrayBuffer {
    pub bytes: Rc<RefCell<Vec<u8>>>,
}
pub type ArrayBufferRef = Rc<RefCell<JsArrayBuffer>>;

/// Phase 10：TypedArray 元素类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypedKind {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float32,
    Float64,
}

impl TypedKind {
    pub fn bytes(&self) -> usize {
        match self {
            TypedKind::Int8 | TypedKind::Uint8 | TypedKind::Uint8Clamped => 1,
            TypedKind::Int16 | TypedKind::Uint16 => 2,
            TypedKind::Int32 | TypedKind::Uint32 | TypedKind::Float32 => 4,
            TypedKind::Float64 => 8,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            TypedKind::Int8 => "Int8Array",
            TypedKind::Uint8 => "Uint8Array",
            TypedKind::Uint8Clamped => "Uint8ClampedArray",
            TypedKind::Int16 => "Int16Array",
            TypedKind::Uint16 => "Uint16Array",
            TypedKind::Int32 => "Int32Array",
            TypedKind::Uint32 => "Uint32Array",
            TypedKind::Float32 => "Float32Array",
            TypedKind::Float64 => "Float64Array",
        }
    }

    pub fn all() -> &'static [TypedKind] {
        &[
            TypedKind::Int8,
            TypedKind::Uint8,
            TypedKind::Uint8Clamped,
            TypedKind::Int16,
            TypedKind::Uint16,
            TypedKind::Int32,
            TypedKind::Uint32,
            TypedKind::Float32,
            TypedKind::Float64,
        ]
    }

    /// 读第 i 个元素（小端；调用方保证越界检查）。
    pub fn read(&self, bytes: &[u8], i: usize) -> f64 {
        let o = i * self.bytes();
        let b: &[u8] = &bytes[o..o + self.bytes()];
        match self {
            TypedKind::Int8 => b[0] as i8 as f64,
            TypedKind::Uint8 | TypedKind::Uint8Clamped => b[0] as f64,
            TypedKind::Int16 => i16::from_le_bytes([b[0], b[1]]) as f64,
            TypedKind::Uint16 => u16::from_le_bytes([b[0], b[1]]) as f64,
            TypedKind::Int32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TypedKind::Uint32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TypedKind::Float32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TypedKind::Float64 => {
                f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            }
        }
    }

    /// 写第 i 个元素（小端；Uint8Clamped 做 clamp；其余按 ToNumber→截断→模）。
    pub fn write(&self, bytes: &mut [u8], i: usize, v: f64) {
        let o = i * self.bytes();
        let b: &mut [u8] = &mut bytes[o..o + self.bytes()];
        match self {
            TypedKind::Int8 => b[0] = to_int_n(v, 8) as i8 as u8,
            TypedKind::Uint8 => b[0] = to_int_n(v, 8) as u8,
            TypedKind::Uint8Clamped => {
                b[0] = if v.is_nan() {
                    0
                } else {
                    (v.round().clamp(0.0, 255.0)) as u8
                }
            }
            TypedKind::Int16 => {
                b.copy_from_slice(&(to_int_n(v, 16) as i16).to_le_bytes())
            }
            TypedKind::Uint16 => {
                b.copy_from_slice(&(to_int_n(v, 16) as u16).to_le_bytes())
            }
            TypedKind::Int32 => {
                b.copy_from_slice(&(to_int_n(v, 32) as i32).to_le_bytes())
            }
            TypedKind::Uint32 => {
                b.copy_from_slice(&(to_int_n(v, 32) as u32).to_le_bytes())
            }
            TypedKind::Float32 => b.copy_from_slice(&(v as f32).to_le_bytes()),
            TypedKind::Float64 => b.copy_from_slice(&v.to_le_bytes()),
        }
    }
}

/// ToInteger → 模 2^n（TypedArray 元素写入用）。
fn to_int_n(v: f64, bits: u32) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    let m = 2f64.powi(bits as i32);
    let t = v.trunc() % m;
    let t = if t < 0.0 { t + m } else { t };
    // 转回有符号（8/16/32 位时高位为符号位）。
    let half = 2f64.powi(bits as i32 - 1);
    if bits > 1 && t >= half {
        (t - m) as i64
    } else {
        t as i64
    }
}

/// Phase 10：TypedArray 视图。
#[derive(Debug, Clone)]
pub struct JsTypedArray {
    pub buffer: ArrayBufferRef,
    pub byte_offset: usize,
    pub len: usize,
    pub kind: TypedKind,
}

impl JsTypedArray {
    /// 读第 i 个元素（自动加 byte_offset；调用方保证越界检查）。
    pub fn read_at(&self, bytes: &[u8], i: usize) -> f64 {
        self.kind.read(&bytes[self.byte_offset..], i)
    }

    /// 写第 i 个元素（自动加 byte_offset）。
    pub fn write_at(&self, bytes: &mut [u8], i: usize, v: f64) {
        let b = &mut bytes[self.byte_offset..];
        self.kind.write(b, i, v)
    }
}
pub type TypedArrayRef = Rc<RefCell<JsTypedArray>>;

/// Phase 10：`DataView` 视图。
#[derive(Debug, Clone)]
pub struct JsDataView {
    pub buffer: ArrayBufferRef,
    pub byte_offset: usize,
    pub byte_len: usize,
}
pub type DataViewRef = Rc<RefCell<JsDataView>>;

/// Phase 10：`Date` —— UTC 毫秒时间戳。
#[derive(Debug, Clone)]
pub struct JsDate {
    pub ms: f64,
}
pub type DateRef = Rc<RefCell<JsDate>>;

pub type FuncRef = Rc<JsFunction>;

/// 内置函数调用上下文：`console` 输出缓冲 + 调用时的 `this` +
/// 内置原型表（供新建对象/数组时挂原型用）。
pub struct NativeCtx<'a> {
    pub console: &'a mut Vec<String>,
    pub this: Value,
    pub protos: crate::object::BuiltinProtos,
    /// 微任务队列（phase 7）：`queueMicrotask` / Promise 反应入队用。
    pub microtasks: &'a mut VecDeque<Microtask>,
    /// fetch 宿主（phase 7）：`None` → `fetch()` 注定 reject。
    pub fetch_host: Option<Rc<dyn FetchHost>>,
    /// 未处理拒绝记账（phase 7）：`Promise.reject` / then 挂载时读写。
    pub unhandled: &'a mut Vec<PromiseRef>,
    /// phase 13：Worker 子解释器的 `postMessage` 投递目标；
    /// 主解释器为 `None`。
    pub post_target: Option<crate::webapi::InboxRef>,
}

/// 内置函数：经 `NativeCtx` 拿到 console / this / 原型表。
pub type NativeFn = fn(&mut NativeCtx, Vec<Value>) -> Result<Value, FlowError>;

// ---------------------------------------------------------------------------
// 值
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Object(ObjectRef),
    Array(ArrayRef),
    Function(FuncRef),
    Native(NativeFn),
    /// DOM 节点句柄（phase 4）：宿主 DOM 的轻量引用，方法经解释器分发。
    DomNode(DomNode),
    /// Promise 本体（phase 7）。
    Promise(PromiseRef),
    /// `new Promise(executor)` 传给 executor 的 resolve/reject（phase 7）。
    PromiseSettler(PromiseSettlerRef),
    /// 正则值（phase 7）：编译产物 + lastIndex。
    RegExp(RegExpRef),
    /// 生成器对象（phase 9）：`function*` 调用返回；`next/return/throw` 由
    /// 解释器驱动（`try_host_method` 拦截方法调用）。
    Generator(GenRef),
    /// async 生成器对象（phase 15）：`async function*` 调用返回；
    /// `next/return/throw` 返回 Promise，由解释器驱动。
    AsyncGenerator(AsyncGenRef),
    /// `new Proxy(target, handler)`（phase 9）。
    Proxy(ProxyRef),
    /// `new Map()`（phase 10）。
    Map(MapRef),
    /// `new Set()`（phase 10）。
    Set(SetRef),
    /// `new WeakMap()`（phase 10）：键为真弱引用。
    WeakMap(WeakMapRef),
    /// `new WeakSet()`（phase 10）。
    WeakSet(WeakSetRef),
    /// `new ArrayBuffer(n)`（phase 10）。
    ArrayBuffer(ArrayBufferRef),
    /// `new Uint8Array(...)` 等（phase 10）。
    TypedArray(TypedArrayRef),
    /// `new DataView(...)`（phase 10）。
    DataView(DataViewRef),
    /// `new Date(...)`（phase 10，UTC 时间戳）。
    Date(DateRef),
}

// 手写 Debug，避免循环引用时无穷递归。
impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Undefined => write!(f, "undefined"),
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Number(n) => write!(f, "Number({})", number_to_js_string(*n)),
            Value::String(s) => write!(f, "String({:?})", s),
            Value::Object(_) => write!(f, "Object(...)"),
            Value::Array(a) => write!(f, "Array(len={})", a.borrow().elems.len()),
            Value::Function(func) => write!(
                f,
                "Function({})",
                func.name.as_deref().unwrap_or("anonymous")
            ),
            Value::Native(_) => write!(f, "Native(...)"),
            Value::DomNode(n) => write!(f, "DomNode({})", n.describe()),
            Value::Promise(_) => write!(f, "Promise(...)"),
            Value::PromiseSettler(s) => {
                write!(f, "PromiseSettler(reject={})", s.reject)
            }
            Value::RegExp(r) => write!(f, "RegExp({})", r.borrow().display()),
            Value::Generator(_) => write!(f, "Generator(...)"),
            Value::AsyncGenerator(_) => write!(f, "AsyncGenerator(...)"),
            Value::Proxy(_) => write!(f, "Proxy(...)"),
            Value::Map(_) => write!(f, "Map(...)"),
            Value::Set(_) => write!(f, "Set(...)"),
            Value::WeakMap(_) => write!(f, "WeakMap(...)"),
            Value::WeakSet(_) => write!(f, "WeakSet(...)"),
            Value::ArrayBuffer(b) => {
                let bb = b.borrow();
                let bytes = bb.bytes.borrow();
                write!(f, "ArrayBuffer({} bytes)", bytes.len())
            }
            Value::TypedArray(t) => {
                let t = t.borrow();
                write!(f, "{}({})", t.kind.name(), t.len)
            }
            Value::DataView(d) => write!(f, "DataView({} bytes)", d.borrow().byte_len),
            Value::Date(d) => write!(f, "Date({})", d.borrow().ms),
        }
    }
}

/// `PartialEq` 即 JS `===` 语义（NaN 不等于自身）。
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.strict_eq(other)
    }
}

impl Value {
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Object(_) | Value::Array(_) => "object",
            Value::Function(_) | Value::Native(_) => "function",
            Value::DomNode(_) => "object",
            Value::Promise(_) => "object",
            // resolve/reject 可调用。
            Value::PromiseSettler(_) => "function",
            Value::RegExp(_) => "object",
            Value::Generator(_) => "object",
            Value::AsyncGenerator(_) => "object",
            // Proxy 的 typeof 透传 target（`typeof new Proxy(function(){},{})` 为 "function"）。
            Value::Proxy(p) => p.borrow().target.type_of(),
            // phase 10：集合与二进制视图皆为 object。
            Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::ArrayBuffer(_)
            | Value::TypedArray(_)
            | Value::DataView(_)
            | Value::Date(_) => "object",
        }
    }

    pub fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    pub fn is_callable(&self) -> bool {
        match self {
            Value::Function(_) | Value::Native(_) | Value::PromiseSettler(_) => true,
            // Proxy 可调用性透传 target（无 apply trap 且 target 不可调用时调用期报错）。
            Value::Proxy(p) => p.borrow().target.is_callable(),
            _ => false,
        }
    }

    // -- ToBoolean --
    pub fn to_boolean(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
            Value::Object(_)
            | Value::Array(_)
            | Value::Function(_)
            | Value::Native(_)
            | Value::DomNode(_)
            | Value::Promise(_)
            | Value::PromiseSettler(_)
            | Value::RegExp(_)
            | Value::Generator(_)
            | Value::AsyncGenerator(_)
            | Value::Proxy(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::ArrayBuffer(_)
            | Value::TypedArray(_)
            | Value::DataView(_)
            | Value::Date(_) => true,
        }
    }

    // -- ToNumber --
    pub fn to_number(&self) -> f64 {
        match self {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Number(n) => *n,
            Value::String(s) => string_to_number(s),
            // 简化：对象先 ToPrimitive（数组 join / 对象 "[object Object]"）再转数字。
            Value::Object(_)
            | Value::Array(_)
            | Value::Function(_)
            | Value::Native(_)
            | Value::DomNode(_)
            | Value::Promise(_)
            | Value::PromiseSettler(_)
            | Value::RegExp(_)
            | Value::Generator(_)
            | Value::AsyncGenerator(_)
            | Value::Proxy(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::ArrayBuffer(_)
            | Value::TypedArray(_)
            | Value::DataView(_)
            | Value::Date(_) => string_to_number(&self.to_primitive_string()),
        }
    }

    // -- ToInt32 / ToUint32（位运算用） --
    pub fn to_int32(&self) -> i32 {
        let n = self.to_number();
        if !n.is_finite() || n == 0.0 {
            return 0;
        }
        let int = n.trunc();
        let m = int % 4294967296.0;
        let m = if m < 0.0 { m + 4294967296.0 } else { m };
        let m = if m >= 2147483648.0 {
            m - 4294967296.0
        } else {
            m
        };
        m as i32
    }

    pub fn to_uint32(&self) -> u32 {
        self.to_int32() as u32
    }

    // -- ToString --
    pub fn to_js_string(&self) -> String {
        match self {
            Value::Undefined => "undefined".to_string(),
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_to_js_string(*n),
            Value::String(s) => s.clone(),
            Value::Object(o) => {
                // phase 8：Error 实例渲染为 `TypeError: msg`（与 V8 的
                // String(new TypeError("x")) 一致）；普通对象仍是旧行为。
                // phase 14：包装对象（new String/Number/Boolean）先取内部原始值。
                let b = o.borrow();
                if let Some(p) = b.primitive.clone() {
                    drop(b);
                    return p.to_js_string();
                }
                if b.tag.as_deref() == Some("Error") {
                    let name = b
                        .get("name")
                        .map(|v| v.to_js_string())
                        .unwrap_or_else(|| "Error".to_string());
                    let msg = b
                        .get("message")
                        .map(|v| v.to_js_string())
                        .unwrap_or_default();
                    return format!("{name}: {msg}");
                }
                "[object Object]".to_string()
            }
            Value::Array(a) => a
                .borrow()
                .elems
                .iter()
                .map(|v| {
                    // 稀疏数组的空位 join 时视为空字符串（子集里空位存为 Undefined，
                    // 此处近似处理；TODO: 保留 Hole 语义）。
                    if v.is_nullish() {
                        String::new()
                    } else {
                        v.to_js_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            Value::Function(f) => format!(
                "function {}() {{ [code] }}",
                f.name.as_deref().unwrap_or("anonymous")
            ),
            Value::Native(_) => "function () { [native code] }".to_string(),
            // 形如 "[object HTMLDivElement]"，与浏览器 console 一致。
            Value::DomNode(n) => n.js_class_string(),
            Value::Promise(_) => "[object Promise]".to_string(),
            Value::PromiseSettler(_) => "function () { [native code] }".to_string(),
            Value::RegExp(r) => r.borrow().display(),
            Value::Generator(_) => "[object Generator]".to_string(),
            Value::AsyncGenerator(_) => "[object AsyncGenerator]".to_string(),
            // Proxy 的字符串化透传 target（子集简化：不触发 get trap）。
            Value::Proxy(p) => p.borrow().target.to_js_string(),
            Value::Map(_) => "[object Map]".to_string(),
            Value::Set(_) => "[object Set]".to_string(),
            Value::WeakMap(_) => "[object WeakMap]".to_string(),
            Value::WeakSet(_) => "[object WeakSet]".to_string(),
            Value::ArrayBuffer(_) => "[object ArrayBuffer]".to_string(),
            Value::TypedArray(t) => format!("[object {}]", t.borrow().kind.name()),
            Value::DataView(_) => "[object DataView]".to_string(),
            Value::Date(d) => format_utc_date(d.borrow().ms),
        }
    }

    /// ToPrimitive（default hint）简化版：对象转字符串。
    /// TODO: 先调 valueOf 再调 toString 的完整流程。
    fn to_primitive_string(&self) -> String {
        self.to_js_string()
    }

    // -- Strict Equality (===) --
    pub fn strict_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Undefined, Value::Undefined) => true,
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b, // NaN != NaN ✓
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => Rc::ptr_eq(a, b),
            (Value::Array(a), Value::Array(b)) => Rc::ptr_eq(a, b),
            (Value::Function(a), Value::Function(b)) => Rc::ptr_eq(a, b),
            // fn 指针直接 == 会 warning（地址不保证唯一）；转 usize 比较地址。
            (Value::Native(a), Value::Native(b)) => *a as usize == *b as usize,
            // DOM 节点：同一宿主 + 同一节点 id 视为同一。
            (Value::DomNode(a), Value::DomNode(b)) => a.same_node(b),
            // Promise / 正则 / settler：引用同一性。
            (Value::Promise(a), Value::Promise(b)) => Rc::ptr_eq(a, b),
            (Value::PromiseSettler(a), Value::PromiseSettler(b)) => Rc::ptr_eq(a, b),
            (Value::RegExp(a), Value::RegExp(b)) => Rc::ptr_eq(a, b),
            // 生成器 / Proxy：引用同一性。
            (Value::Generator(a), Value::Generator(b)) => Rc::ptr_eq(a, b),
            (Value::AsyncGenerator(a), Value::AsyncGenerator(b)) => Rc::ptr_eq(a, b),
            (Value::Proxy(a), Value::Proxy(b)) => Rc::ptr_eq(a, b),
            // phase 10：集合与视图——引用同一性。
            (Value::Map(a), Value::Map(b)) => Rc::ptr_eq(a, b),
            (Value::Set(a), Value::Set(b)) => Rc::ptr_eq(a, b),
            (Value::WeakMap(a), Value::WeakMap(b)) => Rc::ptr_eq(a, b),
            (Value::WeakSet(a), Value::WeakSet(b)) => Rc::ptr_eq(a, b),
            (Value::ArrayBuffer(a), Value::ArrayBuffer(b)) => Rc::ptr_eq(a, b),
            (Value::TypedArray(a), Value::TypedArray(b)) => Rc::ptr_eq(a, b),
            (Value::DataView(a), Value::DataView(b)) => Rc::ptr_eq(a, b),
            (Value::Date(a), Value::Date(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    // -- Loose Equality (==) 简化版 --
    // 覆盖常用情形：null==undefined、同类型走 ===、Number/String/Bool 互转、
    // 对象与原始值比较（对象先 ToPrimitive）。Symbol/BigInt 不在子集内。
    pub fn loose_eq(&self, other: &Value) -> bool {
        if self.strict_eq(other) {
            return true;
        }
        match (self, other) {
            (Value::Null, Value::Undefined) | (Value::Undefined, Value::Null) => true,
            (Value::Number(_), Value::String(_)) => {
                self.to_number() == other.to_number()
            }
            (Value::String(_), Value::Number(_)) => {
                self.to_number() == other.to_number()
            }
            (Value::Bool(_), _) => Value::Number(self.to_number()).loose_eq(other),
            (_, Value::Bool(_)) => self.loose_eq(&Value::Number(other.to_number())),
            // 对象 vs 原始值：对象先 ToPrimitive
            (Value::Object(_) | Value::Array(_), Value::String(_) | Value::Number(_)) => {
                Value::String(self.to_primitive_string()).loose_eq(other)
            }
            (Value::String(_) | Value::Number(_), Value::Object(_) | Value::Array(_)) => {
                self.loose_eq(&Value::String(other.to_primitive_string()))
            }
            _ => false,
        }
    }
}

/// JS 数字转字符串（`String(1e21) === "1e+21"` 等边角）。
pub fn number_to_js_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n == f64::INFINITY {
        return "Infinity".to_string();
    }
    if n == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if n == 0.0 {
        return "0".to_string(); // String(-0) === "0"
    }
    let a = n.abs();
    if a >= 1e21 || a < 1e-6 {
        // 指数形式：Rust `{:e}` 给 "1e21"，JS 要 "1e+21"。
        let s = format!("{:e}", n);
        if let Some(pos) = s.find('e') {
            let (m, e) = s.split_at(pos);
            let exp = &e[1..];
            if exp.starts_with('-') {
                return format!("{}e{}", m, exp);
            }
            return format!("{}e+{}", m, exp);
        }
        return s;
    }
    if n.fract() == 0.0 {
        if a < 9007199254740992.0 {
            // 安全整数内直接转 i64。
            return format!("{}", n as i64);
        }
        return format!("{:.0}", n);
    }
    // Rust 的 `{}` 对 f64 已是最短往返表示（如 0.1 -> "0.1"）。
    format!("{}", n)
}

/// JS 字符串转数字（`Number(" 0x10 ") === 16`，`Number("") === 0`）。
fn string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| c.is_whitespace());
    if t.is_empty() {
        return 0.0;
    }
    if t.eq_ignore_ascii_case("infinity") || t == "+Infinity" {
        return f64::INFINITY;
    }
    if t == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    // 十六 / 二 / 八进制
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0b", 2), ("0B", 2), ("0o", 8), ("0O", 8)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut v = 0.0f64;
            for c in digits.chars() {
                match c.to_digit(radix) {
                    Some(d) => v = v * radix as f64 + d as f64,
                    None => return f64::NAN,
                }
            }
            return v;
        }
    }
    // 十进制：先做严格校验（Rust 的 parse 会接受 "inf"/"nan"，JS 里它们是 NaN）。
    match parse_decimal_strict(t) {
        Some(v) => v,
        None => f64::NAN,
    }
}

/// 严格校验十进制数字字面量形式，通过才交给 Rust 解析。
fn parse_decimal_strict(t: &str) -> Option<f64> {
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits = true;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits = true;
        }
    }
    if !digits {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let es = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == es {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    t.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// 环境（词法作用域链）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone)]
struct Slot {
    kind: DeclKind,
    /// false = TDZ（已声明未初始化），访问直接报错。
    initialized: bool,
    value: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Env {
    parent: Option<EnvRef>,
    /// 变量槽：键为驻留字符串（phase 12）+ Fx 快速哈希。
    slots: FastMap<IStr, Slot>,
}

pub type EnvRef = Rc<RefCell<Env>>;

impl Env {
    pub fn new_global() -> EnvRef {
        Rc::new(RefCell::new(Env {
            parent: None,
            slots: FastMap::default(),
        }))
    }

    pub fn child(parent: &EnvRef) -> EnvRef {
        Rc::new(RefCell::new(Env {
            parent: Some(parent.clone()),
            slots: FastMap::default(),
        }))
    }

    /// 声明 var（提升用；已存在则保持原值不动——函数声明优先于 var）。
    pub fn declare_var(env: &EnvRef, name: &str) {
        let mut e = env.borrow_mut();
        e.slots.entry(istr(name)).or_insert(Slot {
            kind: DeclKind::Var,
            initialized: true,
            value: Value::Undefined,
        });
    }

    /// 声明 let/const（TDZ）。同作用域重复声明报错。
    pub fn declare_lexical(
        env: &EnvRef,
        name: &str,
        kind: DeclKind,
    ) -> Result<(), RuntimeError> {
        let mut e = env.borrow_mut();
        if e.slots.contains_key(name) {
            // phase 8：重复声明是 SyntaxError（早期错误）。
            return Err(RuntimeError::typed(
                ErrorKind::SyntaxError,
                format!("identifier '{}' has already been declared", name),
            ));
        }
        e.slots.insert(
            istr(name),
            Slot {
                kind,
                initialized: false,
                value: Value::Undefined,
            },
        );
        Ok(())
    }

    /// 初始化当前作用域的 lexical 绑定。
    pub fn init_lexical(env: &EnvRef, name: &str, val: Value) -> Result<(), RuntimeError> {
        let mut e = env.borrow_mut();
        match e.slots.get_mut(name) {
            Some(slot) => {
                slot.value = val;
                slot.initialized = true;
                Ok(())
            }
            None => Err(RuntimeError::new(format!(
                "internal error: initializing undeclared '{}'",
                name
            ))),
        }
    }

    /// 优化（单轮）：调用帧批量绑定——调用环境总是全新的，一次 borrow_mut
    /// 插入全部形参槽位，省掉逐个 declare/init 的多次借用与重复哈希查找。
    /// 重复形参（sloppy 模式）后者覆盖前者，与原来 init 语义一致。
    pub(crate) fn bind_call_frame(env: &EnvRef, bindings: Vec<(IStr, DeclKind, Value)>) {
        let mut e = env.borrow_mut();
        e.slots.reserve(bindings.len());
        for (name, kind, val) in bindings {
            e.slots.insert(
                name,
                Slot {
                    kind,
                    initialized: true,
                    value: val,
                },
            );
        }
    }

    /// 直接覆写已存在的绑定（函数声明提升后回填用；找不到则在给定作用域新建）。
    pub fn assign_force(env: &EnvRef, name: &str, val: Value) {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            let mut b = e.borrow_mut();
            if let Some(slot) = b.slots.get_mut(name) {
                slot.value = val;
                slot.initialized = true;
                return;
            }
            cur = b.parent.clone();
        }
        env.borrow_mut().slots.insert(
            istr(name),
            Slot {
                kind: DeclKind::Var,
                initialized: true,
                value: val,
            },
        );
    }

    /// 赋值：沿作用域链找。返回 Ok(true)=已写入；Ok(false)=没找到（调用方决定，
    /// 如 sloppy 模式建全局 var）。TDZ / const 赋值直接报错。
    pub fn assign(env: &EnvRef, name: &str, val: Value) -> Result<bool, RuntimeError> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            // 先只读检查，避免 borrow 冲突。
            let found = {
                let b = e.borrow();
                b.slots.get(name).map(|s| (s.kind, s.initialized))
            };
            match found {
                Some((_, false)) => {
                    return Err(RuntimeError::typed(
                        ErrorKind::ReferenceError,
                        format!("cannot access '{}' before initialization", name),
                    ));
                }
                Some((DeclKind::Const, true)) => {
                    // phase 8：给 const 赋值是 TypeError。
                    return Err(RuntimeError::typed(
                        ErrorKind::TypeError,
                        "assignment to constant variable",
                    ));
                }
                Some(_) => {
                    e.borrow_mut().slots.get_mut(name).unwrap().value = val;
                    return Ok(true);
                }
                None => {
                    cur = e.borrow().parent.clone();
                }
            }
        }
        Ok(false)
    }

    /// 读取：沿链找。返回 Ok(None)=未声明（调用方报 ReferenceError）；
    /// TDZ 直接报错。
    pub fn lookup(env: &EnvRef, name: &str) -> Result<Option<Value>, RuntimeError> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            let found = {
                let b = e.borrow();
                b.slots.get(name).cloned()
            };
            match found {
                Some(slot) => {
                    if !slot.initialized {
                        return Err(RuntimeError::typed(
                            ErrorKind::ReferenceError,
                            format!("cannot access '{}' before initialization", name),
                        ));
                    }
                    return Ok(Some(slot.value));
                }
                None => {
                    cur = e.borrow().parent.clone();
                }
            }
        }
        Ok(None)
    }

    /// 找到声明该名字的环境（赋值目标定位用）。
    pub fn find_env(env: &EnvRef, name: &str) -> Option<EnvRef> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            let has = e.borrow().slots.contains_key(name);
            if has {
                return Some(e);
            }
            cur = e.borrow().parent.clone();
        }
        None
    }

    /// 当前作用域是否已声明该名字（TDZ 预声明后的幂等初始化用）。
    pub fn is_declared_here(env: &EnvRef, name: &str) -> bool {
        env.borrow().slots.contains_key(name)
    }

    /// phase 8：调试快照——沿作用域链收集变量名 → 调试字符串
    /// （内层遮蔽外层；TDZ 记为 `<TDZ>`）。只读，不影响求值。
    pub fn debug_snapshot(env: &EnvRef) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            let b = e.borrow();
            for (k, slot) in b.slots.iter() {
                out.entry(k.to_string()).or_insert_with(|| {
                    if slot.initialized {
                        format!("{:?}", slot.value)
                    } else {
                        "<TDZ>".to_string()
                    }
                });
            }
            cur = b.parent.clone();
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Phase 12：作用域退出时的保守环断开（"循环引用检测与处理"）
// ---------------------------------------------------------------------------

/// 岛节点：环断开遍历中持有的一份 `Rc`（计数时每节点折算 1）。
#[derive(Clone)]
enum IslandNode {
    Env(EnvRef),
    Func(FuncRef),
    Obj(ObjectRef),
    Arr(ArrayRef),
    Promise(PromiseRef),
    Settler(PromiseSettlerRef),
    Gen(GenRef),
    AsyncGen(AsyncGenRef),
    Proxy(ProxyRef),
    Map(MapRef),
    Set(SetRef),
    WeakMap(WeakMapRef),
    WeakSet(WeakSetRef),
    /// `Promise.all` 的共享状态（`ReactionKind::All` 经此边）。
    AllState(crate::promise::AllStateRef),
}

impl IslandNode {
    /// 节点身份（`Rc` 分配地址）。
    fn ptr(&self) -> *const () {
        match self {
            IslandNode::Env(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Func(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Obj(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Arr(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Promise(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Settler(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Gen(r) => Rc::as_ptr(r) as *const (),
            IslandNode::AsyncGen(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Proxy(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Map(r) => Rc::as_ptr(r) as *const (),
            IslandNode::Set(r) => Rc::as_ptr(r) as *const (),
            IslandNode::WeakMap(r) => Rc::as_ptr(r) as *const (),
            IslandNode::WeakSet(r) => Rc::as_ptr(r) as *const (),
            IslandNode::AllState(r) => Rc::as_ptr(r) as *const (),
        }
    }

    /// 节点强引用数（含遍历持有的 1 份）。
    fn strong_count(&self) -> usize {
        match self {
            IslandNode::Env(r) => Rc::strong_count(r),
            IslandNode::Func(r) => Rc::strong_count(r),
            IslandNode::Obj(r) => Rc::strong_count(r),
            IslandNode::Arr(r) => Rc::strong_count(r),
            IslandNode::Promise(r) => Rc::strong_count(r),
            IslandNode::Settler(r) => Rc::strong_count(r),
            IslandNode::Gen(r) => Rc::strong_count(r),
            IslandNode::AsyncGen(r) => Rc::strong_count(r),
            IslandNode::Proxy(r) => Rc::strong_count(r),
            IslandNode::Map(r) => Rc::strong_count(r),
            IslandNode::Set(r) => Rc::strong_count(r),
            IslandNode::WeakMap(r) => Rc::strong_count(r),
            IslandNode::WeakSet(r) => Rc::strong_count(r),
            IslandNode::AllState(r) => Rc::strong_count(r),
        }
    }
}

/// `value_has_rc`：值是否携带强引用边（环断开预检用）。
fn value_has_rc(v: &Value) -> bool {
    matches!(
        v,
        Value::Object(_)
            | Value::Array(_)
            | Value::Function(_)
            | Value::Promise(_)
            | Value::PromiseSettler(_)
            | Value::Generator(_)
            | Value::AsyncGenerator(_)
            | Value::Proxy(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
    )
}

/// 环断开器：从 `env` 出发的保守可达岛分析。
struct CycleBreaker {
    /// ptr → (节点, 岛内入边数, 是否豁免检查）。
    /// 经 `parent` 边到达的节点标记为豁免：它们是存活的外层作用域，
    /// 不展开、不检查（展开它们会一路走到全局环境，既爆炸又必然放弃；
    /// 它们指向岛内节点的边不计入——少计只会导致保守放弃，安全）。
    nodes: HashMap<*const (), (IslandNode, usize, bool)>,
    /// 前向邻接（存活传播用）：src ptr → dst ptr 列表。
    outgoing: HashMap<*const (), Vec<*const ()>>,
    /// 当前正在访问的节点（边登记用）。
    visiting: *const (),
    /// 遇到未知强边类型时置 true（直接放弃，不断开）。
    bailed: bool,
    /// 节点预算：超限则放弃（防病态大图的遍历开销）。
    budget: usize,
}

impl CycleBreaker {
    fn new() -> Self {
        CycleBreaker {
            nodes: HashMap::new(),
            outgoing: HashMap::new(),
            visiting: std::ptr::null(),
            bailed: false,
            budget: 4096,
        }
    }

    /// 登记一条强边。`via_parent` 为 true 时目标标记为豁免（已存在则保持原标记）。
    /// 返回 true 表示目标是新节点、需要继续遍历（豁免节点不遍历）。
    fn edge(&mut self, node: IslandNode, via_parent: bool) -> bool {
        if self.nodes.len() >= self.budget {
            self.bailed = true;
            return false;
        }
        let p = node.ptr();
        // 记录前向边（存活传播用）。
        if !self.visiting.is_null() {
            self.outgoing.entry(self.visiting).or_default().push(p);
        }
        match self.nodes.get_mut(&p) {
            Some((_, incoming, _)) => {
                *incoming += 1;
                false
            }
            None => {
                let fresh = !via_parent;
                self.nodes.insert(p, (node, 1, via_parent));
                fresh
            }
        }
    }

    fn value_edges(&mut self, v: &Value, stack: &mut Vec<IslandNode>) {
        match v {
            Value::Object(o) => {
                if self.edge(IslandNode::Obj(o.clone()), false) {
                    stack.push(IslandNode::Obj(o.clone()));
                }
            }
            Value::Array(a) => {
                if self.edge(IslandNode::Arr(a.clone()), false) {
                    stack.push(IslandNode::Arr(a.clone()));
                }
            }
            Value::Function(f) => {
                if self.edge(IslandNode::Func(f.clone()), false) {
                    stack.push(IslandNode::Func(f.clone()));
                }
            }
            Value::Promise(p) => {
                if self.edge(IslandNode::Promise(p.clone()), false) {
                    stack.push(IslandNode::Promise(p.clone()));
                }
            }
            Value::PromiseSettler(s) => {
                if self.edge(IslandNode::Settler(s.clone()), false) {
                    stack.push(IslandNode::Settler(s.clone()));
                }
            }
            Value::Generator(g) => {
                if self.edge(IslandNode::Gen(g.clone()), false) {
                    stack.push(IslandNode::Gen(g.clone()));
                }
            }
            Value::AsyncGenerator(g) => {
                if self.edge(IslandNode::AsyncGen(g.clone()), false) {
                    stack.push(IslandNode::AsyncGen(g.clone()));
                }
            }
            Value::Proxy(p) => {
                if self.edge(IslandNode::Proxy(p.clone()), false) {
                    stack.push(IslandNode::Proxy(p.clone()));
                }
            }
            Value::Map(m) => {
                if self.edge(IslandNode::Map(m.clone()), false) {
                    stack.push(IslandNode::Map(m.clone()));
                }
            }
            Value::Set(s) => {
                if self.edge(IslandNode::Set(s.clone()), false) {
                    stack.push(IslandNode::Set(s.clone()));
                }
            }
            // WeakMap/WeakSet：键是弱引用，只遍历值。
            Value::WeakMap(m) => {
                if self.edge(IslandNode::WeakMap(m.clone()), false) {
                    stack.push(IslandNode::WeakMap(m.clone()));
                }
            }
            Value::WeakSet(s) => {
                if self.edge(IslandNode::WeakSet(s.clone()), false) {
                    stack.push(IslandNode::WeakSet(s.clone()));
                }
            }
            // 宿主相关：边未知 → 保守放弃。
            Value::DomNode(_) => {
                self.bailed = true;
            }
            // 其余为叶子（数字/字符串/正则编译产物无 Rc 回边等）。
            _ => {}
        }
    }

    fn jsobject_edges(&mut self, o: &JsObject, stack: &mut Vec<IslandNode>) {
        for (_, v) in o.props.iter() {
            self.value_edges(v, stack);
            if self.bailed {
                return;
            }
        }
        if let Some(p) = &o.proto {
            if self.edge(IslandNode::Obj(p.clone()), false) {
                stack.push(IslandNode::Obj(p.clone()));
            }
        }
        for (g, s) in o.accessors.values() {
            if let Some(f) = g {
                if self.edge(IslandNode::Func(f.clone()), false) {
                    stack.push(IslandNode::Func(f.clone()));
                }
            }
            if let Some(f) = s {
                if self.edge(IslandNode::Func(f.clone()), false) {
                    stack.push(IslandNode::Func(f.clone()));
                }
            }
        }
        for v in o.privates.values() {
            self.value_edges(v, stack);
            if self.bailed {
                return;
            }
        }
        // ctor_backref 为 Weak，不计。
    }

    /// 访问节点，枚举其强边。`is_root` 仅对根环境为 true（跳过 `arguments`
    /// 槽位——调用参数在根环境创建前求值，不可能参与以根为起点的环；
    /// 跳过它避免把调用方传的大对象图卷入遍历；安全性由外部引用计数兜底）。
    fn visit(&mut self, node: &IslandNode, stack: &mut Vec<IslandNode>, is_root: bool) {
        // 借用期间不持有跨节点的克隆；edge() 的临时克隆要么入库（成为
        // 节点的那 1 份持有），要么立即丢弃，不影响最终计数。
        match node {
            IslandNode::Env(e) => {
                let b = e.borrow();
                for (k, slot) in b.slots.iter() {
                    if is_root && k.as_str() == "arguments" {
                        continue;
                    }
                    self.value_edges(&slot.value, stack);
                    if self.bailed {
                        return;
                    }
                }
                if let Some(p) = &b.parent {
                    // parent 边：目标豁免（存活的外层作用域，不展开不检查）。
                    if self.edge(IslandNode::Env(p.clone()), true) {
                        stack.push(IslandNode::Env(p.clone()));
                    }
                }
            }
            IslandNode::Func(f) => {
                if self.edge(IslandNode::Env(f.closure.clone()), false) {
                    stack.push(IslandNode::Env(f.closure.clone()));
                }
                if let Some(p) = &f.prototype {
                    if self.edge(IslandNode::Obj(p.clone()), false) {
                        stack.push(IslandNode::Obj(p.clone()));
                    }
                }
                if let Some(s) = &f.super_ctor {
                    self.value_edges(s, stack);
                    if self.bailed {
                        return;
                    }
                }
                for v in f.statics.borrow().values() {
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
                for v in f.private_statics.borrow().values() {
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
                // home_object 为 Weak；vm_chunk 无回边；private_scope 无 Rc 边。
            }
            IslandNode::Obj(o) => {
                let b = o.borrow();
                self.jsobject_edges(&b, stack);
            }
            IslandNode::Arr(a) => {
                let b = a.borrow();
                for v in b.elems.iter() {
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
                self.jsobject_edges(&b.props, stack);
                if self.bailed {
                    return;
                }
                if let Some(p) = &b.proto {
                    if self.edge(IslandNode::Obj(p.clone()), false) {
                        stack.push(IslandNode::Obj(p.clone()));
                    }
                }
            }
            IslandNode::Promise(p) => {
                let b = p.borrow();
                match &b.state {
                    crate::promise::PromiseState::Fulfilled(v)
                    | crate::promise::PromiseState::Rejected(v) => {
                        self.value_edges(v, stack);
                        if self.bailed {
                            return;
                        }
                    }
                    _ => {}
                }
                for r in b.reactions.iter() {
                    if let Some(n) = &r.next {
                        if self.edge(IslandNode::Promise(n.clone()), false) {
                            stack.push(IslandNode::Promise(n.clone()));
                        }
                    }
                    if self.edge(IslandNode::Promise(r.source.clone()), false) {
                        stack.push(IslandNode::Promise(r.source.clone()));
                    }
                    match &r.kind {
                        crate::promise::ReactionKind::Handler {
                            on_fulfilled,
                            on_rejected,
                            ..
                        } => {
                            if let Some(v) = on_fulfilled {
                                self.value_edges(v, stack);
                                if self.bailed {
                                    return;
                                }
                            }
                            if let Some(v) = on_rejected {
                                self.value_edges(v, stack);
                                if self.bailed {
                                    return;
                                }
                            }
                        }
                        crate::promise::ReactionKind::All { state, .. } => {
                            if self.edge(IslandNode::AllState(state.clone()), false) {
                                stack.push(IslandNode::AllState(state.clone()));
                            }
                        }
                        crate::promise::ReactionKind::Race { result } => {
                            if self.edge(IslandNode::Promise(result.clone()), false) {
                                stack.push(IslandNode::Promise(result.clone()));
                            }
                        }
                        crate::promise::ReactionKind::AsyncUnwrap { outer } => {
                            if self.edge(IslandNode::Promise(outer.clone()), false) {
                                stack.push(IslandNode::Promise(outer.clone()));
                            }
                        }
                        crate::promise::ReactionKind::AsyncGenDone { agen } => {
                            if self.edge(IslandNode::AsyncGen(agen.clone()), false) {
                                stack.push(IslandNode::AsyncGen(agen.clone()));
                            }
                        }
                    }
                }
            }
            IslandNode::Settler(s) => {
                if self.edge(IslandNode::Promise(s.promise.clone()), false) {
                    stack.push(IslandNode::Promise(s.promise.clone()));
                }
            }
            IslandNode::Gen(g) => {
                let b = g.borrow();
                if self.edge(IslandNode::Func(b.step.clone()), false) {
                    stack.push(IslandNode::Func(b.step.clone()));
                }
                self.value_edges(&b.this_value, stack);
            }
            IslandNode::AsyncGen(g) => {
                let b = g.borrow();
                if self.edge(IslandNode::Func(b.body_func.clone()), false) {
                    stack.push(IslandNode::Func(b.body_func.clone()));
                }
                self.value_edges(&b.this_value, stack);
                for req in b.queue.iter() {
                    self.value_edges(&req.arg, stack);
                    self.value_edges(&req.resolve, stack);
                    self.value_edges(&req.reject, stack);
                }
            }
            IslandNode::Proxy(p) => {
                let b = p.borrow();
                self.value_edges(&b.target, stack);
                if self.bailed {
                    return;
                }
                if self.edge(IslandNode::Obj(b.handler.clone()), false) {
                    stack.push(IslandNode::Obj(b.handler.clone()));
                }
            }
            IslandNode::Map(m) => {
                let b = m.borrow();
                for (k, v) in b.entries.iter() {
                    self.value_edges(k, stack);
                    if self.bailed {
                        return;
                    }
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
            }
            IslandNode::Set(s) => {
                let b = s.borrow();
                for v in b.entries.iter() {
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
            }
            // WeakMap/WeakSet：键为弱引用，只计值边。
            IslandNode::WeakMap(m) => {
                let b = m.borrow();
                for (_, v) in b.entries.iter() {
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
            }
            IslandNode::WeakSet(_) => {
                // 键弱引用，无强边。
            }
            IslandNode::AllState(s) => {
                let b = s.borrow();
                for v in b.values.iter().flatten() {
                    self.value_edges(v, stack);
                    if self.bailed {
                        return;
                    }
                }
                if self.edge(IslandNode::Promise(b.result.clone()), false) {
                    stack.push(IslandNode::Promise(b.result.clone()));
                }
            }
        }
    }

    /// 从 `root` 出发收集岛；遇到未知边则 `bailed = true`。
    /// 豁免节点（经 parent 到达）不展开。
    fn collect(&mut self, root: &EnvRef) {
        let root_ptr = Rc::as_ptr(root) as *const ();
        self.nodes
            .insert(root_ptr, (IslandNode::Env(root.clone()), 0, false));
        let mut stack = vec![IslandNode::Env(root.clone())];
        let mut first = true;
        while let Some(node) = stack.pop() {
            if self.bailed {
                return;
            }
            let is_root = first && matches!(node, IslandNode::Env(_));
            first = false;
            // 豁免节点不展开（parent 边的目标）。
            let exempt = self
                .nodes
                .get(&node.ptr())
                .map(|(_, _, e)| *e)
                .unwrap_or(false);
            if exempt {
                continue;
            }
            self.visiting = node.ptr();
            self.visit(&node, &mut stack, is_root);
        }
        self.visiting = std::ptr::null();
    }

    /// 存活判定（mark-sweep 传播）：
    /// 1. 外部引用数 = strong_count - 遍历持有(1) - 岛内入边数
    ///    - (根节点 ? 调用方句柄(1) : 0)；> 0 者为存活种子
    ///    （被岛外引用的节点：共享原型、逃逸的闭包等）。
    /// 2. 从种子沿岛内前向边传播存活。
    /// 3. 根若存活 → 不可断开（返回 false）；否则清空根槽位，返回 true。
    ///
    /// 关键正确性：只有"从存活节点不可达"的岛节点才会被释放；
    /// 任何被岛外引用的节点都会成为种子并保护其可达闭包。
    fn sweep(&self, root_ptr: *const ()) -> bool {
        if self.bailed {
            return false;
        }
        // 种子：有外部强引用的非豁免节点。
        let mut live: Vec<*const ()> = Vec::new();
        let mut marked: std::collections::HashSet<*const ()> =
            std::collections::HashSet::new();
        for (ptr, (node, incoming, exempt)) in self.nodes.iter() {
            if *exempt {
                continue;
            }
            let mut ext = node.strong_count() as isize - 1 - *incoming as isize;
            if *ptr == root_ptr {
                ext -= 1; // 调用方的 env 句柄（作用域退出时即将释放）。
            }
            if ext < 0 {
                // 计数异常（边枚举 bug 的信号）→ 保守放弃。
                return false;
            }
            if ext > 0 {
                live.push(*ptr);
                marked.insert(*ptr);
            }
        }
        // 传播。
        let mut stack = live;
        while let Some(p) = stack.pop() {
            if let Some(dsts) = self.outgoing.get(&p) {
                for d in dsts {
                    if marked.insert(*d) {
                        stack.push(*d);
                    }
                }
            }
        }
        !marked.contains(&root_ptr)
    }
}

impl Env {
    /// phase 12：在作用域退出时尝试断开 `Rc` 强环（"循环引用检测与处理"）。
    ///
    /// 保守的 mark-sweep 式分析：以 `env` 为根收集可达岛；有岛外强引用的
    /// 节点为存活种子，沿岛内边传播存活；若根最终存活则返回 false，
    /// 语义零影响，否则清空根槽位、断开环（级联释放），返回 true。
    ///
    /// 覆盖：命名函数 ↔ 定义环境、类（原型/方法/实例）环、用户对象环
    /// （`o.self = o`）等。`parent` 边目标豁免（存活外层，不展开）；
    /// 根的 `arguments` 槽位跳过（调用参数先于环境创建，不成环）；
    /// `DomNode`（宿主边未知）导致放弃。
    ///
    /// 调用方保证：调用后不再使用 `env`（作用域退出路径）。
    pub(crate) fn break_scope_cycles(env: &EnvRef) -> bool {
        // 快路径：
        // 1. 全局/根环境（无 parent）在解释器存活期间永不退出，
        //    其环由 Drop 兜底清理，此处直接跳过（也避免每次顶层求值
        //    都遍历整个全局环境）。
        // 2. 根槽位里除 `arguments` 外没有任何带引用的值 → 不可能成环。
        //    （`arguments` 的元素先于环境创建，不参与以根为起点的环；
        //    详见 visit() 的 is_root 说明。）
        {
            let b = env.borrow();
            if b.parent.is_none() {
                return false;
            }
            if !b
                .slots
                .iter()
                .any(|(k, s)| k.as_str() != "arguments" && value_has_rc(&s.value))
            {
                return false;
            }
        }
        let mut cb = CycleBreaker::new();
        let root_ptr = Rc::as_ptr(env) as *const ();
        cb.collect(env);
        if !cb.sweep(root_ptr) {
            return false;
        }
        // 断开：清空根槽位。存活分析已保证根从任何存活节点不可达；
        // 环断后级联释放；极少数不经过根的子环（若有）保持原样，不更差。
        env.borrow_mut().slots.clear();
        true
    }

    /// phase 12：解释器析构时的兜底——清空全局槽位，打破"全局环境 ↔
    /// 顶层函数/类"的环（否则整个全局环境在解释器 drop 后仍然泄漏）。
    /// 宿主仍持有的值不受影响（它们的闭包环境保持存活，语义正确）。
    pub(crate) fn clear_all(env: &EnvRef) {
        env.borrow_mut().slots.clear();
    }
}

// ---------------------------------------------------------------------------
// Phase 10：SameValueZero 与 UTC 日期工具
// ---------------------------------------------------------------------------

/// Phase 10：SameValueZero（Map/Set 键比较用）：NaN 等于 NaN，
/// +0/-0 相等（Rust `f64 ==` 已满足），其余走 `===`。
pub fn same_value_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x == y || (x.is_nan() && y.is_nan()),
        _ => a.strict_eq(b),
    }
}

/// Phase 10：毫秒时间戳 → UTC 民用日期（Howard Hinnant 算法，纯整数运算）。
/// 返回 (year, month 1-12, day 1-31, hour, min, sec, weekday 0=周日..6=周六)。
pub fn utc_civil(ms: f64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let mut secs = (ms / 1000.0).floor() as i64;
    let days = secs.div_euclid(86400);
    secs = secs.rem_euclid(86400);
    let hour = (secs / 3600) as u32;
    let min = ((secs % 3600) / 60) as u32;
    let sec = (secs % 60) as u32;
    // 1970-01-01 是周四；days=0 → 4。
    let weekday = ((days + 4).rem_euclid(7)) as u32;
    // days → civil（Hinnant）。
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec, weekday)
}

/// Phase 10：`String(date)` 用的 UTC 可读形式。
pub fn format_utc_date(ms: f64) -> String {
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss, wd) = utc_civil(ms);
    format!(
        "{} {} {:02} {:04} {:02}:{:02}:{:02} GMT+0000 (UTC)",
        WD[wd as usize],
        MO[(m - 1) as usize],
        d,
        y,
        hh,
        mm,
        ss
    )
}
