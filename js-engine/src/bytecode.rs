//! yousj-js · Phase 11：字节码指令集与代码块（Chunk）。
//!
//! 设计原则（与树遍历解释器语义一致是最高优先级）：
//! - 栈式虚拟机：每个函数调用一份操作数栈（`vm.rs` 的 `Vm`）。
//! - 变量仍走 `Env`/`EnvRef` 词法环境链（TDZ、闭包、提升语义全部复用）；
//!   字节码只负责求值顺序、控制流与分发。
//! - 复杂语义（属性读写、函数调用、`new`、二元运算、展开、类、super、
//!   私有字段等）以单条指令回调用解释器既有方法实现，保证行为一致。
//! - 常量池 / 名字池 / 函数元数据表 / 类表 / 作用域单元表；跳转地址编译期回填。
//! - 生成器 / async 函数在 `make_function` 期已被改写为状态机，
//!   编译器看到的永远是改写后的普通控制流，无需 yield/await 指令。

use crate::ast::{
    ArrowBody, AssignOp, BinaryOp, ClassNode, ForLeft, FunctionNode, Param, Span, Stmt,
    UnaryOp, UpdateOp,
};

// ---------------------------------------------------------------------------
// 常量池
// ---------------------------------------------------------------------------

/// 常量池条目（`Op::Const` 的操作数）。
#[derive(Debug, Clone)]
pub enum ConstVal {
    Number(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    /// 正则字面量：VM 在执行到时编译（与解释器"每次求值编译"行为一致）。
    Regex { pattern: String, flags: String },
}

// ---------------------------------------------------------------------------
// 函数 / 作用域元数据
// ---------------------------------------------------------------------------

/// `Op::MakeFunction` / `Op::MakeMethod` 引用的函数元数据。
/// 名字：Decl/Expr 取自 `node.id`；Arrow 为空；Method 在运行时从栈顶 key 取。
#[derive(Debug, Clone)]
pub enum FuncKind {
    /// 函数声明（语句）：`make_function`，调用方再 `ForceSetVar`。
    Decl(FunctionNode),
    /// 函数表达式（含具名函数表达式的内部绑定）。
    Expr(FunctionNode),
    /// 箭头函数（含 async 改写）。
    Arrow {
        params: Vec<Param>,
        body: ArrowBody,
        strict: bool,
        is_async: bool,
    },
    /// 对象字面量方法（`{ f() {} }` / `{ get x() {} }`）。
    Method(FunctionNode),
}

#[derive(Debug, Clone)]
pub struct FuncMeta {
    pub kind: FuncKind,
    pub span: Span,
}

/// 作用域单元：`EnterBlock` / `HoistLex` 的操作数。
/// 语义 = 解释器 `exec_block` 的入口动作：
/// `hoist(stmts, var_env)` + `declare_lexicals(stmts, env)`。
#[derive(Debug, Clone)]
pub struct ScopeUnit {
    pub hoist_stmts: Vec<Stmt>,
    pub lex_stmts: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    Catch,
    Finally,
}

/// 异常表条目：`[start, end)` 区间内抛错 → 跳到 `target`。
/// Catch 只接 `FlowError::Thrown`（与解释器一致：运行时错误不被 catch 捕获，
/// 但 finally 仍然执行）；Finally 接所有错误，执行完 `Rethrow`。
#[derive(Debug, Clone)]
pub struct Handler {
    pub start: u32,
    pub end: u32,
    pub target: u32,
    pub kind: HandlerKind,
    /// 进入 handler 时的环境栈深度（try 语句处）。
    pub env_depth: u32,
    /// 进入 handler 时的迭代器栈深度（try 语句处）。
    pub iter_depth: u32,
}

// ---------------------------------------------------------------------------
// 指令集
// ---------------------------------------------------------------------------

/// 调用标志位（`Call` / `CallPropName` / `CallPropDyn` 共用）。
pub const CALL_OPTIONAL: u32 = 1; // `?.()`：callee 为 nullish 时得 undefined
/// 成员调用标志位（`CallPropName` / `CallPropDyn`）。
pub const CALL_OPTC: u32 = 2; // `?.`：base 为 nullish 时得 undefined

#[derive(Debug, Clone)]
pub enum Op {
    // ===== 常量与栈 =====
    /// push consts[i]。
    Const(u32),
    Undefined,
    Null,
    True,
    False,
    /// 丢弃栈顶。
    Pop,
    /// 复制栈顶。
    Dup,

    // ===== 变量（Env） =====
    /// names[i]：`Env::lookup`，未找到 → ReferenceError。
    GetName(u32),
    /// 栈顶为 v：赋值（`eval_target` Ident 语义 + `set_target`）；v 留在栈顶。
    SetName(u32),
    /// pop v：`Env::assign_force(var_env, name, v)`（var 声明 / 函数声明用）。
    ForceSetVar(u32),
    /// names[i]：未声明则 `declare_lexical(Let)`（幂等，供 TDZ 预声明后复用）。
    DeclLex(u32),
    /// 同上，DeclKind::Const。
    DeclConst(u32),
    /// pop v：`init_lexical`。
    InitLex(u32),
    /// `const x;`（无初值）→ SyntaxError。
    MissingConstInit,
    /// [rhs]：cur=lookup(names[i])；复合赋值（含 &&=/||=/??= 的解释器语义：
    /// rhs 已求值）；push 结果。
    CompoundName(u32, AssignOp),
    /// lookup；++/--；push（prefix ? new : cur）。
    UpdateName(u32, UpdateOp, bool),

    // ===== this =====
    GetThis,

    // ===== 属性 =====
    /// [obj, key] → `get_prop`。
    GetProp,
    /// [obj] → `get_prop(names[i])`。
    GetPropName(u32),
    /// [obj, key, val] → `set_prop`；val 留在栈顶（赋值表达式的值）。
    SetProp,
    /// [obj, val] → `set_prop(names[i])`；val 留在栈顶。
    SetPropName(u32),
    /// [obj, key, rhs] → 复合赋值；push 结果。
    CompoundProp(AssignOp),
    /// [obj, rhs] → 复合赋值（names[i] 为键）；push 结果。
    CompoundPropName(u32, AssignOp),
    /// [obj, key] → ++/--；push（prefix ? new : cur）。
    UpdateProp(UpdateOp, bool),
    /// [obj] → ++/--（names[i] 为键）；push 结果。
    UpdatePropName(u32, UpdateOp, bool),
    /// [obj, rhs]（names[i] 为私有名）→ 复合赋值；push 结果。
    CompoundPrivate(u32, AssignOp),
    /// [obj] → 私有 ++/--；push 结果。
    UpdatePrivate(u32, UpdateOp, bool),
    /// [obj, key] → delete 语义；push Bool。
    DeleteProp,
    /// `delete ident`：严格模式 → SyntaxError；否则求值后 push true。
    DeleteIdent(u32),
    /// [obj] → `get_private(names[i])`。
    GetPrivate(u32),
    /// [obj, val] → `set_private`；val 留在栈顶。
    SetPrivate(u32),
    /// [obj] → `private_in(names[i])`；push Bool。
    PrivateIn(u32),

    // ===== 一元 / 二元 =====
    /// -x / +x / !x / ~x（Typeof/Delete/Void 由编译器展开为专用指令）。
    Unary(UnaryOp),
    /// [v] → typeof 字符串。
    Typeof,
    /// `typeof ident`：未声明 → "undefined"。
    TypeofName(u32),
    /// [l, r] → `apply_binary`。
    Binary(BinaryOp),
    /// [l, r] → `l.strict_eq(&r)`（switch 用）。
    StrictEq,

    // ===== 跳转 =====
    Jump(u32),
    /// pop v；!to_boolean → 跳。
    JumpIfFalse(u32),
    /// pop v；to_boolean → 跳。
    JumpIfTrue(u32),
    /// peek；nullish → 跳（栈不变，供 `?.` / `??`）。
    JumpIfNullish(u32),

    // ===== 调用 =====
    /// [callee, a1..an] → `call_value_at`（span 取自本指令的 span）。
    Call(u32, u32),
    /// [obj, a1..an] → 成员调用（nargs, name_idx, flags）。
    CallPropName(u32, u32, u32),
    /// [obj, key, a1..an] → 成员调用（nargs, flags）。
    CallPropDyn(u32, u32),
    /// [a1..an] → `eval_super_call`（`super(...)`）。
    SuperCall(u32),
    /// [a1..an] → `super.name(...)`（nargs, name_idx）。
    SuperMethodName(u32, u32),
    /// [key, a1..an] → `super[k](...)`。
    SuperMethodDyn(u32),
    /// `super.name` 读。
    SuperPropName(u32),
    /// [key] → `super[k]` 读。
    SuperPropDyn,
    /// [callee, a1..an] → `eval_new_vals`。
    New(u32),

    // ===== 数组 / 对象字面量（收集器模式，支持嵌套） =====
    ArrNew,
    /// pop v → 当前数组收集器。
    ArrPush,
    /// 收集 Undefined（空位）。
    ArrHole,
    /// pop iterable → `spread_into_vec` 后收集。
    ArrSpread,
    /// 收集器 → JsArray（`protos.array` 为原型）→ push。
    ArrDone,
    ObjNew,
    /// [key, val] → key.to_js_string() 后收集。
    ObjSet,
    /// [val] → 收集 names[i]。
    ObjSetName(u32),
    /// 收集器 → push Object。
    ObjDone,
    /// funcs[i] → Function 值（`make_function_value`）。
    MakeFunction(u32),
    /// funcs[i]（Method）：name 取自栈顶 key（peek），push Function。
    MakeMethod(u32),
    /// classes[i] → `eval_class`。
    MakeClass(u32),

    // ===== 作用域 =====
    /// scopes[i]：push child env；hoist + declare_lexicals。
    EnterBlock(u32),
    /// scopes[i]：hoist + declare_lexicals（当前 env，不 push）。
    HoistLex(u32),
    /// push child env（无声明动作，供 for / switch）。
    PushEnv,
    /// pop env。
    PopEnv,

    // ===== 控制流 =====
    /// pop v → 结束本次 `run`，返回 v（环境栈截断到函数基址）。
    Return,
    /// pop v → vm.pending_ret（finally 内联时的返回值暂存）。
    StashRet,
    /// push vm.pending_ret.take()。
    UnstashRet,
    /// pop v → Err(Thrown(v))。
    Throw,
    /// Err(pending_exc.take())（finally handler 尾部重抛）。
    Rethrow,
    /// vm.last_save.push(vm.last)（finally 内联保护语句值）。
    SaveLast,
    /// vm.last = vm.last_save.pop()。
    RestoreLast,
    /// pop v → vm.last = v（语句值）。
    SetLast,
    /// 顶层 return → SyntaxError。
    BadReturn,
    /// 顶层 break → SyntaxError。
    BadBreak,
    /// 顶层 continue → SyntaxError。
    BadContinue,
    /// 非法赋值目标 → SyntaxError（运行时，与解释器一致）。
    BadTarget,
    /// 防御性：单独求值的 super/await/yield/私有左值 → 内部错误。
    BadSuper,
    BadAwait,
    BadYield,
    BadPrivateName,

    // ===== for-in / for-of =====
    /// [iterable] → 建迭代器压栈（0=for-in keys，1=for-of items）。
    IterBegin(u32),
    /// 迭代器耗尽 → 弹栈并跳 target；否则 push 下一项。
    IterNext(u32),
    /// [v]：for_lefts[i] 为 Expr 时 `eval_target` + `set_target`。
    SetForLeft(u32),

    // ===== switch =====
    /// pop → vm.switch_disc。
    SetSwitchDisc,
    /// push vm.switch_disc.clone()。
    GetSwitchDisc,

    // ===== 调试器 =====
    /// debug_enabled 时 `debug_hook(本指令 span, 当前 env)`。
    DebugPoint,
}

// ---------------------------------------------------------------------------
// Chunk
// ---------------------------------------------------------------------------

/// 编译产物：一份函数体 / 顶层程序。
#[derive(Debug, Clone)]
pub struct Chunk {
    /// 指令流（`Op` 即"字节码"，线性 IR）。
    pub code: Vec<Op>,
    /// 与 code 一一对应的源码位置（调用点 span 用）。
    pub spans: Vec<Span>,
    pub consts: Vec<ConstVal>,
    pub names: Vec<String>,
    pub funcs: Vec<FuncMeta>,
    pub classes: Vec<ClassNode>,
    pub scopes: Vec<ScopeUnit>,
    pub for_lefts: Vec<ForLeft>,
    pub handlers: Vec<Handler>,
    /// 顶层程序块（return/break/continue 非法）。
    pub top_level: bool,
    /// 调试用名（函数名 / `<main>`）。
    pub name: String,
}

impl Chunk {
    pub fn new(name: impl Into<String>, top_level: bool) -> Self {
        Chunk {
            code: Vec::new(),
            spans: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            funcs: Vec::new(),
            classes: Vec::new(),
            scopes: Vec::new(),
            for_lefts: Vec::new(),
            handlers: Vec::new(),
            top_level,
            name: name.into(),
        }
    }

    /// 简易反汇编（调试用）。
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "== chunk {} ({} ops, {} consts, {} names, {} funcs, {} handlers) ==\n",
            self.name,
            self.code.len(),
            self.consts.len(),
            self.names.len(),
            self.funcs.len(),
            self.handlers.len()
        ));
        for (i, op) in self.code.iter().enumerate() {
            let sp = self.spans.get(i).map(|s| format!("{}:{}", s.start_line, s.start_col)).unwrap_or_default();
            out.push_str(&format!("{:04} {:12} {:?}\n", i, sp, op));
        }
        if !self.consts.is_empty() {
            out.push_str("-- consts --\n");
            for (i, c) in self.consts.iter().enumerate() {
                out.push_str(&format!("  [{}] {:?}\n", i, c));
            }
        }
        if !self.names.is_empty() {
            out.push_str("-- names --\n");
            for (i, n) in self.names.iter().enumerate() {
                out.push_str(&format!("  [{}] {}\n", i, n));
            }
        }
        if !self.handlers.is_empty() {
            out.push_str("-- handlers --\n");
            for h in &self.handlers {
                out.push_str(&format!(
                    "  [{}, {}) -> {} {:?} env_depth={}\n",
                    h.start, h.end, h.target, h.kind, h.env_depth
                ));
            }
        }
        out
    }
}
