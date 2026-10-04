//! yousj-js · Phase 3：树遍历解释器——让 JS 代码真正跑起来。
//!
//! - 词法作用域环境链：`var` 函数作用域提升，`let`/`const` 块作用域 + 简化版 TDZ。
//! - 函数声明 / 表达式 / 箭头函数均为闭包（捕获定义时环境）；`arguments` 简化版；
//!   `return` 经信号冒泡，嵌套函数正确处理。
//! - 控制流：if / while / do-while / for / for-in-of / try-catch-finally /
//!   throw / switch / break / continue。
//! - 表达式：全套二元 / 一元 / 逻辑 / 三元 / 赋值（含复合）/ 调用 / 成员 /
//!   new（仅普通函数构造）/ 可选链。
//! - 运行时错误全部以 `RuntimeError`（带消息）返回，不 panic；另有 1000 万步
//!   执行上限防死循环 hang 死宿主。
//! - 内置：`console.log/error/warn`（输出收集到 Vec，由调用方取）、`Math`、
//!   `JSON`（基础版）、`parseInt` / `parseFloat` / `isNaN`。
//! - 刻意留到 phase 4：完整标准库（Array/Object/String 方法等）、DOM 绑定、
//!   prototype 链、class、模块、async、正则字面量。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use crate::async_desugar::AsyncDesugar;
use crate::ast::*;
use crate::dom::{would_create_cycle, DomHost, DomNode, NullDom};
use crate::fetch::{FetchHostRef, FetchResponse};
use crate::lexer::LexError;
use crate::object::{instance_of, BuiltinProtos};
use crate::parser::{parse_source, ParseError};
use crate::promise::{
    attach_reaction, settle_promise, AllState, JsPromise, Microtask, PromiseRef,
    PromiseSettler, PromiseState, Reaction, ReactionKind, MAX_MICROTASKS,
};
use crate::regex::{CompiledRegex, JsRegExp, RegExpRef};
use crate::value::*;

/// 单次执行的最大步数（语句 + 表达式求值计数），防用户代码死循环。
pub const STEP_LIMIT: u64 = 10_000_000;

/// drain 阶段最多执行的宏任务数，防 `setTimeout` 自增殖把队列撑爆。
/// （每个任务内部仍受 STEP_LIMIT 约束。）
const MAX_TASKS: usize = 10_000;

/// 语句执行信号（正常 / return / break / continue）。
/// `throw` 不走这里，直接以 `FlowError::Thrown` 向上传播。
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Normal(Value),
    Return(Value),
    Break,
    Continue,
}

// ---------------------------------------------------------------------------
// Phase 8：调试器钩子
// ---------------------------------------------------------------------------

/// 调试器对一条语句的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakAction {
    /// 继续执行。
    Continue,
    /// 暂停：解释器记录调用栈 + 作用域变量快照后继续（非交互式收集）。
    Pause,
    /// 单步跳过：本次暂停，并在此之后跳过更深调用栈的语句回调，
    /// 直到回到当前或更浅深度（标准 step-over 语义，由解释器实现）。
    StepOver,
}

/// 调试宿主。默认实现全部空操作（零开销概念：不安装宿主时
/// `debug_enabled` 为 false，`exec_stmt` 里只有一个分支判断）。
pub trait DebugHost {
    fn on_statement(&mut self, _line: usize, _col: usize) -> BreakAction {
        BreakAction::Continue
    }
    /// 暂停命中时调用（默认空实现）。
    fn on_pause(&mut self, _pause: &DebugPause) {}
}

/// 一次暂停命中的快照：位置 + 调用栈（内层在前）+ 作用域变量。
#[derive(Debug, Clone)]
pub struct DebugPause {
    pub line: usize,
    pub col: usize,
    pub stack: Vec<Frame>,
    /// 变量名 → 调试字符串（`Number(1)` / `String("x")` / …；TDZ 为 `<TDZ>`）。
    pub vars: HashMap<String, String>,
}

/// 内置的极简断点宿主：行号断点列表；命中（Pause）时记录行列。
/// 给 Python `run_js(debug=True, breakpoints=[...])` 与测试用。
#[derive(Debug, Default)]
pub struct BreakpointHost {
    pub breakpoints: Vec<usize>,
    pub hits: Vec<(usize, usize)>,
}

impl BreakpointHost {
    pub fn new(breakpoints: Vec<usize>) -> Self {
        BreakpointHost {
            breakpoints,
            hits: Vec::new(),
        }
    }
}

impl DebugHost for BreakpointHost {
    fn on_statement(&mut self, line: usize, col: usize) -> BreakAction {
        if self.breakpoints.contains(&line) {
            self.hits.push((line, col));
            BreakAction::Pause
        } else {
            BreakAction::Continue
        }
    }
}

pub struct Interpreter {
    global: EnvRef,
    console: Vec<String>,
    steps: u64,
    /// 当前 `this` 栈；箭头函数调用时不压栈（词法捕获外层 this）。
    this_stack: Vec<Value>,
    /// 内置原型表（phase 4）：字面量 / new / 原生方法的原型来源。
    pub(crate) protos: BuiltinProtos,
    /// DOM 绑定（phase 4）：`bind_dom` 后全局出现 `document` / `window`。
    dom: Option<DomBinding>,
    /// 微任务队列（phase 7）：Promise 反应 / `queueMicrotask`。
    microtasks: VecDeque<Microtask>,
    /// fetch 宿主（phase 7）。
    fetch_host: Option<FetchHostRef>,
    /// `Promise` 构造器对象（phase 7）：`eval_new` 拦截用。
    promise_ctor: Option<ObjectRef>,
    /// `RegExp` 构造器对象（phase 7）：`eval_new` 拦截用。
    regexp_ctor: Option<ObjectRef>,
    /// 模块注册表（phase 7）：name → 源码（`register_module` 写入）。
    modules: HashMap<String, String>,
    /// 已求值的模块缓存（phase 7）：name → 其导出表。
    module_cache: HashMap<String, HashMap<String, Value>>,
    /// 正在求值的模块栈（phase 7）：循环依赖检测。
    module_stack: Vec<String>,
    /// 运行结束时尚无人处理的 rejection（phase 7）：记 console 报告。
    unhandled: Vec<PromiseRef>,
    /// phase 8：严格模式栈（每层函数/脚本体的有效 strict；栈顶为当前）。
    strict_stack: Vec<bool>,
    /// phase 8：调用栈（函数名 + 调用点行列；错误堆栈与调试器共用）。
    call_stack: Vec<Frame>,
    /// phase 8：错误构造器（`Error`/`TypeError`/…）：kind → 构造器对象。
    error_ctors: Vec<(ErrorKind, ObjectRef)>,
    /// phase 8：错误原型：kind → `.prototype` 对象
    /// （`TypeError.prototype` → `Error.prototype` → `Object.prototype`）。
    error_protos: HashMap<ErrorKind, ObjectRef>,
    /// phase 8：调试器总开关。关闭时 `exec_stmt` 里只有这一个分支判断，
    /// 不影响求值语义与性能。
    debug_enabled: bool,
    /// phase 8：调试宿主（`None` 则即使开开关也不触发）。
    debug_host: Option<Rc<RefCell<dyn DebugHost>>>,
    /// phase 8：`Pause`/`StepOver` 命中记录（含调用栈与变量快照）。
    debug_pauses: Vec<DebugPause>,
    /// phase 8：StepOver 深度水位——调用栈深于此值时跳过宿主回调。
    step_depth: Option<usize>,
    /// phase 9：当前函数栈（`super` 的 home_object 查找用；箭头函数同样压栈，
    /// `get_super_prop` 会向上跳过无 home_object 的帧）。
    func_stack: Vec<FuncRef>,
    /// phase 9：类 id 分配器（0 保留给非类函数）。
    class_seq: u64,
    /// phase 9：`Proxy` 构造器对象（`eval_new` 按指针识别）。
    proxy_ctor: Option<ObjectRef>,
    /// phase 10：集合 / 二进制 / Date / Intl 构造器（`eval_new` 按指针识别）。
    map_ctor: Option<ObjectRef>,    set_ctor: Option<ObjectRef>,
    weakmap_ctor: Option<ObjectRef>,
    weakset_ctor: Option<ObjectRef>,
    arraybuffer_ctor: Option<ObjectRef>,
    dataview_ctor: Option<ObjectRef>,
    typed_array_ctors: Vec<(TypedKind, ObjectRef)>,
    date_ctor: Option<ObjectRef>,
    intl_numberformat_ctor: Option<ObjectRef>,
    intl_datetimeformat_ctor: Option<ObjectRef>,
    /// Phase 14：字段初始化器求值深度（`init_instance_fields` 重入计数）。
    /// 直接 eval 在字段初始化器里包含 `super` 时按规范抛 SyntaxError
    /// （防止 `eval('super()')` 经 `eval_super_call` 无限重入）。
    field_init_depth: u32,
    /// phase 11：字节码 VM 开关。开启后函数体与顶层程序走 `vm` 执行，
    /// 关闭则走原树遍历解释器。两者语义一致，可随时切换。
    pub(crate) vm_enabled: bool,
    /// phase 13：WebSocket 宿主（`bind_websocket_host` 设置；`None` 时连接
    /// 失败走 `error` + `close` 事件，给出清晰错误而非 panic）。
    pub(crate) ws_host: Option<crate::webapi::WsHostRef>,
    /// phase 13：存活 WebSocket：实例对象指针 → {对象, 连接}。
    /// `close` 事件后移除；从未关闭的由解释器析构时释放（文档化）。
    pub(crate) ws_conns: HashMap<usize, crate::webapi::WsLive>,
    /// phase 13：存活 Worker：实例对象指针 → 状态（含子解释器）。
    pub(crate) workers: HashMap<usize, Box<crate::webapi::WorkerEntry>>,
    /// phase 13：若本解释器是 Worker 子解释器，`postMessage` 投递到此 inbox。
    pub(crate) post_target: Option<crate::webapi::InboxRef>,
    /// phase 13：Web API 构造器（`eval_new` 按指针识别）：名 → 构造器对象。
    pub(crate) web_ctors: HashMap<String, ObjectRef>,
}

/// DOM 绑定状态：宿主 + document / window 对象 + 事件循环。
struct DomBinding {
    host: Rc<dyn DomHost>,
    document: ObjectRef,
    window: ObjectRef,
    next_timeout_id: u64,
    /// 宏任务队列（`setTimeout` / `click` 回调）。主脚本跑完后按 FIFO 排空。
    tasks: VecDeque<Task>,
    /// 已取消的任务 id（`clearTimeout`）。
    cancelled: HashSet<u64>,
    /// phase 13：存活的 interval id（`setInterval`；`clearInterval` 移除）。
    intervals: HashSet<u64>,
    /// 事件监听 token → JS 回调（token 由宿主分配并存储）。
    listener_cbs: HashMap<u64, Value>,
}

/// 待执行的宏任务：id（取消凭据）+ 种类。
struct Task {
    id: u64,
    kind: TaskKind,
}

/// phase 13：宏任务种类。`Callback` 为原有 `setTimeout` / `click` 回调；
/// 其余为 Web API 内部任务（`enqueue_internal` 入队）。
#[derive(Clone)]
pub(crate) enum TaskKind {
    /// 普通 JS 回调（`setTimeout` / `click`）。
    Callback(Value),
    /// `setInterval` 回调：执行后若仍在 `intervals` 中则重新排期。
    Interval { id: u64, callback: Value },
    /// WebSocket 握手成功：`readyState=1` + `open` 事件 + 开始 poll。
    WsOpened { obj: ObjectRef },
    /// WebSocket 握手失败：`readyState=3` + `error` + `close` 事件。
    WsOpenFail { obj: ObjectRef, message: String },
    /// WebSocket 轮询宿主事件并分发。
    WsPoll { obj: ObjectRef },
    /// WebSocket 运行时错误：分发 `error` 事件。
    WsError { obj: ObjectRef, message: String },
    /// 父 → Worker：投递 `data` 给 worker 的 `onmessage`。
    WorkerDeliver { obj: ObjectRef, data: Value },
    /// Worker → 父：排空 inbox，分发父的 `onmessage`。
    WorkerInboxDrain { obj: ObjectRef },
}

/// phase 12：解释器析构时清空全局槽位，打破"全局环境 ↔ 顶层函数/类"
/// 的 Rc 强环。否则整个全局环境（含所有内置）在解释器 drop 后仍然泄漏
/// （实测单个顶层类声明残留约 33KB）。宿主仍持有的值不受影响——它们的
/// 闭包环境保持存活，语义正确。
impl Drop for Interpreter {
    fn drop(&mut self) {
        Env::clear_all(&self.global);
    }
}

impl Interpreter {
    pub fn new() -> Self {
        let mut ip = Interpreter {
            global: Env::new_global(),
            console: Vec::new(),
            steps: 0,
            this_stack: Vec::new(),
            protos: BuiltinProtos::new(),
            dom: None,
            microtasks: VecDeque::new(),
            fetch_host: None,
            promise_ctor: None,
            regexp_ctor: None,
            modules: HashMap::new(),
            module_cache: HashMap::new(),
            module_stack: Vec::new(),
            unhandled: Vec::new(),
            strict_stack: Vec::new(),
            call_stack: Vec::new(),
            error_ctors: Vec::new(),
            error_protos: HashMap::new(),
            debug_enabled: false,
            debug_host: None,
            debug_pauses: Vec::new(),
            step_depth: None,
            func_stack: Vec::new(),
            class_seq: 1,
            proxy_ctor: None,
            map_ctor: None,
            set_ctor: None,
            weakmap_ctor: None,
            weakset_ctor: None,
            arraybuffer_ctor: None,
            dataview_ctor: None,
            typed_array_ctors: Vec::new(),
            date_ctor: None,
            intl_numberformat_ctor: None,
            intl_datetimeformat_ctor: None,
            field_init_depth: 0,
            vm_enabled: false,
            ws_host: None,
            ws_conns: HashMap::new(),
            workers: HashMap::new(),
            post_target: None,
            web_ctors: HashMap::new(),
        };
        ip.install_builtins();
        ip.install_async();
        ip.install_errors();
        ip.install_proxy();
        ip.install_p10();
        ip.install_web();
        ip
    }

    pub fn take_console(&mut self) -> Vec<String> {
        std::mem::take(&mut self.console)
    }

    /// phase 13：向 console 缓冲追加一行（`webapi.rs` 用）。
    pub(crate) fn console_push(&mut self, msg: String) {
        self.console.push(msg);
    }

    /// phase 13：读全局变量（`Worker` 子解释器 `onmessage` 查找用）；
    /// 未声明/TDZ → `None`。
    pub(crate) fn global_lookup(&self, name: &str) -> Option<Value> {
        match Env::lookup(&self.global, name) {
            Ok(v) => v,
            Err(_) => None,
        }
    }

    /// phase 8：当前是否处于严格模式（栈顶函数/脚本体的有效 strict）。
    pub(crate) fn is_strict(&self) -> bool {
        self.strict_stack.last().copied().unwrap_or(false)
    }

    /// phase 8：安装调试宿主并打开总开关。
    pub fn set_debug_host(&mut self, host: Rc<RefCell<dyn DebugHost>>) {
        self.debug_host = Some(host);
        self.debug_enabled = true;
    }

    /// phase 8：调试总开关（无宿主时打开也无效果）。
    pub fn set_debug_enabled(&mut self, on: bool) {
        self.debug_enabled = on && self.debug_host.is_some();
    }

    /// phase 11：字节码 VM 开关。开启后函数体与顶层程序走 VM 执行。
    pub fn set_vm_enabled(&mut self, on: bool) {
        self.vm_enabled = on;
    }

    /// phase 11：VM 用——当前 this（栈空则 undefined）。
    pub(crate) fn get_this(&self) -> Value {
        self.this_stack.last().cloned().unwrap_or(Value::Undefined)
    }

    /// phase 11：VM 用——调试器总开关是否生效。
    pub(crate) fn debug_is_enabled(&self) -> bool {
        self.debug_enabled
    }

    /// phase 8：取走暂停命中记录。
    pub fn take_debug_pauses(&mut self) -> Vec<DebugPause> {
        std::mem::take(&mut self.debug_pauses)
    }

    /// phase 8：语句执行前的调试钩子。关闭时由调用方的一个 `if` 挡掉，
    /// 这里只处理打开的情形。
    pub(crate) fn debug_hook(&mut self, span: &Span, env: &EnvRef) {
        // StepOver 水位：仍在更深的调用里，跳过宿主回调。
        if let Some(depth) = self.step_depth {
            if self.call_stack.len() > depth {
                return;
            }
            self.step_depth = None;
        }
        let action = match &self.debug_host {
            Some(h) => h.borrow_mut().on_statement(span.start_line, span.start_col),
            None => BreakAction::Continue,
        };
        if action == BreakAction::Continue {
            return;
        }
        if action == BreakAction::StepOver {
            // 暂停本次，并在此之后跳过更深调用栈的回调，直到返回。
            self.step_depth = Some(self.call_stack.len());
        }
        let pause = DebugPause {
            line: span.start_line,
            col: span.start_col,
            stack: self.call_stack.clone(),
            vars: Env::debug_snapshot(env),
        };
        if let Some(h) = &self.debug_host {
            h.borrow_mut().on_pause(&pause);
        }
        self.debug_pauses.push(pause);
    }

    pub(crate) fn tick(&mut self) -> Result<(), FlowError> {
        self.steps += 1;
        if self.steps > STEP_LIMIT {
            return Err(FlowError::Runtime(
                RuntimeError::typed(
                    ErrorKind::RangeError,
                    "execution step limit exceeded (possible infinite loop)",
                )
                .uncatchable(),
            ));
        }
        Ok(())
    }

    pub(crate) fn define_global(&mut self, name: &str, val: Value) {
        Env::declare_var(&self.global, name);
        Env::assign_force(&self.global, name, val);
    }

    // ------------------------------------------------------------------
    // 入口
    // ------------------------------------------------------------------

    pub fn run(&mut self, prog: &Program) -> Result<Value, FlowError> {
        // phase 8：脚本级严格模式压栈（函数调用时各自再压自己的）。
        self.strict_stack.push(prog.strict);
        let r = self.run_inner(prog);
        self.strict_stack.pop();
        r
    }

    fn run_inner(&mut self, prog: &Program) -> Result<Value, FlowError> {
        let g = self.global.clone();
        // Phase 7：模块——顶层 import 先解析并求值依赖（快照绑定导入全局）。
        self.process_imports(&prog.body, &g)?;
        self.hoist(&prog.body, &g)?;
        // phase 11：VM 开启时顶层程序走字节码执行。
        let v = if self.vm_enabled {
            let chunk = crate::compiler::Compiler::compile_program(prog)
                .map_err(crate::vm::flow_of_compile)?;
            crate::vm::run_chunk(self, &chunk, g.clone(), g.clone())?
        } else {
            match self.exec_block(&prog.body, g.clone(), &g)? {
                Signal::Normal(v) => v,
                Signal::Return(_) => return Err(syntax_err("return outside of function")),
                Signal::Break => return Err(syntax_err("break outside of loop")),
                Signal::Continue => return Err(syntax_err("continue outside of loop")),
            }
        };
        // Phase 7：事件循环——主脚本结束后先排空微任务，再跑宏任务；
        // 每个宏任务执行后也排空微任务（标准语义）。
        self.drain_microtasks()?;
        // Phase 6：事件循环——主脚本完成后按 FIFO 排空宏任务队列。
        // 任务里新注册的任务继续排，直到队列空或触及 MAX_TASKS。
        self.drain_tasks()?;
        // 未处理的 rejection 报告（浏览器 console 行为的简化版）。
        for p in std::mem::take(&mut self.unhandled) {
            let msg = match &p.borrow().state {
                PromiseState::Rejected(v) => v.to_js_string(),
                _ => continue,
            };
            self.console.push(format!("UnhandledPromiseRejection: {msg}"));
        }
        Ok(v)
    }

    /// 排空微任务队列。微任务抛错记 console 并继续（浏览器行为）；
    /// 队列上限错误向上传播。
    fn drain_microtasks(&mut self) -> Result<(), FlowError> {
        let mut done = 0usize;
        loop {
            let task = match self.microtasks.pop_front() {
                Some(t) => t,
                None => break,
            };
            done += 1;
            if done > MAX_MICROTASKS {
                return Err(FlowError::Runtime(
                    RuntimeError::typed(
                        ErrorKind::RangeError,
                        "microtask queue limit exceeded (possible runaway promise chain)",
                    )
                    .uncatchable(),
                ));
            }
            if let Err(e) = self.run_microtask(task) {
                let msg = match &e {
                    FlowError::Runtime(r) => r.to_string(),
                    FlowError::Thrown(v) => {
                        format!("uncaught exception: {}", v.to_js_string())
                    }
                };
                self.console.push(format!("Microtask error: {msg}"));
            }
        }
        Ok(())
    }

    /// 执行一个微任务：反应分发或普通回调。
    fn run_microtask(&mut self, task: Microtask) -> Result<(), FlowError> {
        match task {
            Microtask::Call { callback } => {
                self.call_value(callback, Value::Undefined, Vec::new())?;
                Ok(())
            }
            Microtask::Dispatch {
                reaction,
                value,
                rejected,
            } => self.dispatch_reaction(&reaction, value, rejected),
        }
    }

    /// 入队一个宏任务，返回任务 id（即 `setTimeout` 的返回值，
    /// 也是 `clearTimeout` 的凭据）。
    fn enqueue_task(&mut self, callback: Value) -> u64 {
        match &mut self.dom {
            Some(d) => {
                let id = d.next_timeout_id;
                d.next_timeout_id += 1;
                d.tasks.push_back(Task {
                    id,
                    kind: TaskKind::Callback(callback),
                });
                id
            }
            None => 0,
        }
    }

    /// phase 13：内部宏任务入队（WebSocket / Worker 用）。无 DOM 绑定时
    /// 懒创建纯任务队列绑定（不定义 `document`/`window` 全局，不影响现有语义）。
    pub(crate) fn enqueue_internal(&mut self, kind: TaskKind) -> u64 {
        let d = self.ensure_binding();
        let id = d.next_timeout_id;
        d.next_timeout_id += 1;
        d.tasks.push_back(Task { id, kind });
        id
    }

    /// phase 13：确保任务队列绑定存在。
    fn ensure_binding(&mut self) -> &mut DomBinding {
        if self.dom.is_none() {
            let dummy = Rc::new(RefCell::new(JsObject::with_proto(Some(
                self.protos.object.clone(),
            ))));
            self.dom = Some(DomBinding {
                host: Rc::new(NullDom),
                document: dummy.clone(),
                window: dummy,
                next_timeout_id: 1,
                tasks: VecDeque::new(),
                cancelled: HashSet::new(),
                intervals: HashSet::new(),
                listener_cbs: HashMap::new(),
            });
        }
        self.dom.as_mut().unwrap()
    }

    /// phase 13：WebSocket / Worker 内部任务分发（实现见 `webapi.rs`）。
    fn internal_task(&mut self, kind: TaskKind) -> Result<(), FlowError> {
        self.webapi_task(kind)
    }

    /// 排空宏任务队列。单个任务抛错时记 console 并继续（浏览器行为）；
    /// 队列本身的上限错误会向上传播。
    fn drain_tasks(&mut self) -> Result<(), FlowError> {
        let mut done = 0usize;
        loop {
            let task = match &mut self.dom {
                Some(d) => d.tasks.pop_front(),
                None => None,
            };
            let Some(t) = task else { break };
            done += 1;
            if done > MAX_TASKS {
                return Err(FlowError::Runtime(
                    RuntimeError::typed(
                        ErrorKind::RangeError,
                        "task queue limit exceeded (possible runaway timers)",
                    )
                    .uncatchable(),
                ));
            }
            let cancelled = self
                .dom
                .as_ref()
                .map(|d| d.cancelled.contains(&t.id))
                .unwrap_or(false);
            if cancelled {
                continue;
            }
            match t.kind {
                TaskKind::Callback(cb) => {
                    if let Err(e) =
                        self.call_value(cb, Value::Undefined, Vec::new())
                    {
                        self.console.push(format!(
                            "uncaught error in task: {}",
                            task_error_msg(&e)
                        ));
                    }
                }
                // phase 13：`setInterval`——执行后若仍存活则重新排期。
                TaskKind::Interval { id, callback } => {
                    let active = self
                        .dom
                        .as_ref()
                        .map(|d| d.intervals.contains(&id))
                        .unwrap_or(false);
                    if !active {
                        continue;
                    }
                    if let Err(e) =
                        self.call_value(callback.clone(), Value::Undefined, Vec::new())
                    {
                        self.console.push(format!(
                            "uncaught error in task: {}",
                            task_error_msg(&e)
                        ));
                    }
                    let still = self
                        .dom
                        .as_ref()
                        .map(|d| d.intervals.contains(&id))
                        .unwrap_or(false);
                    if still {
                        if let Some(d) = &mut self.dom {
                            d.tasks.push_back(Task {
                                id,
                                kind: TaskKind::Interval { id, callback },
                            });
                        }
                    }
                }
                // phase 13：WebSocket / Worker 内部任务。
                kind => {
                    if let Err(e) = self.internal_task(kind) {
                        self.console.push(format!(
                            "uncaught error in task: {}",
                            task_error_msg(&e)
                        ));
                    }
                }
            }
            // Phase 7：每个宏任务执行后排空微任务（标准事件循环语义）。
            self.drain_microtasks()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 提升：函数声明与 var 声明提升到函数（或全局）作用域
    // ------------------------------------------------------------------

    pub(crate) fn hoist(&mut self, stmts: &[Stmt], var_env: &EnvRef) -> Result<(), FlowError> {
        for s in stmts {
            self.hoist_one(s, var_env)?;
        }
        Ok(())
    }

    fn hoist_one(&mut self, s: &Stmt, var_env: &EnvRef) -> Result<(), FlowError> {
        match &s.node {
            StmtKind::FunctionDecl(f) => {
                let name = f.id.clone().unwrap_or_default();
                if !name.is_empty() {
                    let fun = self.make_function(name.clone(), f, var_env.clone(), false, s.span.clone());
                    Env::declare_var(var_env, &name);
                    Env::assign_force(var_env, &name, Value::Function(fun));
                }
            }
            // Phase 7：`export function f` 同普通函数声明一样提升。
            StmtKind::ExportFunc(f) => {
                let name = f.id.clone().unwrap_or_default();
                if !name.is_empty() {
                    let fun = self.make_function(name.clone(), f, var_env.clone(), false, s.span.clone());
                    Env::declare_var(var_env, &name);
                    Env::assign_force(var_env, &name, Value::Function(fun));
                }
            }
            StmtKind::VarDecl {
                kind: VarKind::Var,
                decls,
            } => {
                for d in decls {
                    Env::declare_var(var_env, &d.id);
                }
            }
            // Phase 7：`export var x` 同普通 var 声明一样提升。
            StmtKind::ExportDecl {
                kind: VarKind::Var,
                decls,
            } => {
                for d in decls {
                    Env::declare_var(var_env, &d.id);
                }
            }
            StmtKind::Block(b) => self.hoist(b, var_env)?,
            StmtKind::If { cons, alt, .. } => {
                self.hoist_one(cons, var_env)?;
                if let Some(a) = alt {
                    self.hoist_one(a, var_env)?;
                }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                self.hoist_one(body, var_env)?;
            }
            StmtKind::For { init, body, .. } => {
                if let Some(ForInit::VarDecl {
                    kind: VarKind::Var,
                    decls,
                }) = init
                {
                    for d in decls {
                        Env::declare_var(var_env, &d.id);
                    }
                }
                self.hoist_one(body, var_env)?;
            }
            StmtKind::ForInOf { left, body, .. } => {
                if let ForLeft::VarDecl {
                    kind: VarKind::Var,
                    name,
                } = left
                {
                    Env::declare_var(var_env, name);
                }
                self.hoist_one(body, var_env)?;
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                self.hoist(block, var_env)?;
                if let Some(h) = handler {
                    self.hoist(&h.body, var_env)?;
                }
                if let Some(f) = finalizer {
                    self.hoist(f, var_env)?;
                }
            }
            StmtKind::Switch { cases, .. } => {
                for c in cases {
                    self.hoist(&c.body, var_env)?;
                }
            }
            // 注意：不递归进函数体（FunctionNode / 箭头函数体），它们有自己的作用域。
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 语句
    // ------------------------------------------------------------------

    fn exec_block(
        &mut self,
        stmts: &[Stmt],
        env: EnvRef,
        var_env: &EnvRef,
    ) -> Result<Signal, FlowError> {
        self.hoist(stmts, var_env)?;
        // TDZ：块顶层的 let/const 预先声明为未初始化，
        // 声明语句执行前访问直接报错（`{ console.log(q); let q = 1; }`）。
        self.declare_lexicals(stmts, &env)?;
        let mut last = Value::Undefined;
        for s in stmts {
            match self.exec_stmt(s, env.clone(), var_env) {
                Ok(Signal::Normal(v)) => last = v,
                // phase 12：块退出（正常/非正常/错误）——尝试断开自环。
                // 有外部引用时自动放弃，语义零影响。
                Ok(other) => {
                    Env::break_scope_cycles(&env);
                    return Ok(other);
                }
                Err(e) => {
                    Env::break_scope_cycles(&env);
                    return Err(e);
                }
            }
        }
        Env::break_scope_cycles(&env);
        Ok(Signal::Normal(last))
    }

    /// 预声明本层块顶级的 let/const（不含嵌套块——它们有自己的 exec_block）。
    pub(crate) fn declare_lexicals(&mut self, stmts: &[Stmt], env: &EnvRef) -> Result<(), FlowError> {
        for s in stmts {
            if let StmtKind::VarDecl { kind, decls } = &s.node {
                if matches!(kind, VarKind::Let | VarKind::Const) {
                    for d in decls {
                        Env::declare_lexical(env, &d.id, decl_kind(*kind))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn exec_stmt(
        &mut self,
        s: &Stmt,
        env: EnvRef,
        var_env: &EnvRef,
    ) -> Result<Signal, FlowError> {
        self.tick()?;
        // phase 8：调试钩子——关闭时只有一个分支判断的开销。
        if self.debug_enabled {
            self.debug_hook(&s.span, &env);
        }
        match &s.node {
            StmtKind::Empty | StmtKind::Debugger => Ok(Signal::Normal(Value::Undefined)),

            StmtKind::Expr(e) => Ok(Signal::Normal(self.eval_expr(e, env)?)),

            // Phase 7：模块语句。主程序的 import 已由 process_imports 处理，
            // 此处跳过；export 在主程序中按普通声明执行（值照常定义）。
            StmtKind::Import { .. } => Ok(Signal::Normal(Value::Undefined)),
            StmtKind::ExportDecl { kind, decls } => {
                let synthetic = Stmt {
                    node: StmtKind::VarDecl {
                        kind: kind.clone(),
                        decls: decls.clone(),
                    },
                    span: s.span.clone(),
                };
                self.exec_stmt(&synthetic, env, var_env)
            }
            StmtKind::ExportFunc(f) => {
                let name = f.id.clone().unwrap_or_default();
                if !name.is_empty() {
                    let fun = self.make_function(name.clone(), f, env.clone(), false, s.span.clone());
                    Env::declare_var(var_env, &name);
                    Env::assign_force(var_env, &name, Value::Function(fun));
                }
                Ok(Signal::Normal(Value::Undefined))
            }
            StmtKind::ExportNames(_) => Ok(Signal::Normal(Value::Undefined)),

            StmtKind::Block(b) => {
                let child = Env::child(&env);
                self.exec_block(b, child, var_env)
            }

            StmtKind::VarDecl { kind, decls } => {
                for d in decls {
                    match kind {
                        VarKind::Var => {
                            if let Some(init) = &d.init {
                                let v = self.eval_expr(init, env.clone())?;
                                Env::assign_force(var_env, &d.id, v);
                            }
                            // 无初值：提升时已是 Undefined。
                        }
                        VarKind::Let | VarKind::Const => {
                            // 预声明阶段可能已声明（TDZ），此处幂等初始化。
                            if !Env::is_declared_here(&env, &d.id) {
                                Env::declare_lexical(&env, &d.id, decl_kind(*kind))?;
                            }
                            match &d.init {
                                Some(init) => {
                                    let v = self.eval_expr(init, env.clone())?;
                                    Env::init_lexical(&env, &d.id, v)?;
                                }
                                None => {
                                    if *kind == VarKind::Const {
                                        return Err(syntax_err(
                                            "missing initializer in const declaration",
                                        ));
                                    }
                                    Env::init_lexical(&env, &d.id, Value::Undefined)?;
                                }
                            }
                        }
                    }
                }
                Ok(Signal::Normal(Value::Undefined))
            }

            StmtKind::FunctionDecl(f) => {
                // 已提升；此处重新绑定一次（幂等）。
                let name = f.id.clone().unwrap_or_default();
                if !name.is_empty() {
                    let fun = self.make_function(name.clone(), f, env.clone(), false, s.span.clone());
                    Env::assign_force(var_env, &name, Value::Function(fun));
                }
                Ok(Signal::Normal(Value::Undefined))
            }

            // phase 9：类声明（块级作用域，let-like）。
            // phase 12：绑定在块环境（而非 var_env），使类相关的环
            // （构造器↔环境↔原型/方法/实例）可在块退出时被断开回收。
            StmtKind::ClassDecl(c) => {
                let name = c.id.clone().unwrap_or_default();
                let cls = self.eval_class(c, Some(name.clone()), env.clone(), s.span.clone())?;
                if !name.is_empty() {
                    if !Env::is_declared_here(&env, &name) {
                        Env::declare_lexical(&env, &name, DeclKind::Let)?;
                    }
                    Env::init_lexical(&env, &name, cls)?;
                }
                Ok(Signal::Normal(Value::Undefined))
            }

            StmtKind::If { test, cons, alt } => {
                let t = self.eval_expr(test, env.clone())?;
                if t.to_boolean() {
                    self.exec_stmt(cons, env, var_env)
                } else if let Some(a) = alt {
                    self.exec_stmt(a, env, var_env)
                } else {
                    Ok(Signal::Normal(Value::Undefined))
                }
            }

            StmtKind::While { test, body } => loop {
                let t = self.eval_expr(test, env.clone())?;
                if !t.to_boolean() {
                    break Ok(Signal::Normal(Value::Undefined));
                }
                match self.exec_stmt(body, env.clone(), var_env)? {
                    Signal::Break => break Ok(Signal::Normal(Value::Undefined)),
                    Signal::Continue => continue,
                    Signal::Normal(_) => continue,
                    other => return Ok(other),
                }
            },

            StmtKind::DoWhile { body, test } => loop {
                match self.exec_stmt(body, env.clone(), var_env)? {
                    Signal::Break => break Ok(Signal::Normal(Value::Undefined)),
                    Signal::Continue => {}
                    Signal::Normal(_) => {}
                    other => return Ok(other),
                }
                let t = self.eval_expr(test, env.clone())?;
                if !t.to_boolean() {
                    break Ok(Signal::Normal(Value::Undefined));
                }
            },

            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                // let/const 声明子待在自己的作用域里。
                let loop_env = Env::child(&env);
                if let Some(init) = init {
                    match init {
                        ForInit::Expr(e) => {
                            self.eval_expr(e, env.clone())?;
                        }
                        ForInit::VarDecl { kind, decls } => {
                            for d in decls {
                                let target_env =
                                    if *kind == VarKind::Var { var_env } else { &loop_env };
                                // var 已提升；let/const 在此声明。
                                if *kind != VarKind::Var {
                                    Env::declare_lexical(target_env, &d.id, decl_kind(*kind))?;
                                }
                                let v = match &d.init {
                                    Some(e) => self.eval_expr(e, env.clone())?,
                                    None => Value::Undefined,
                                };
                                if *kind == VarKind::Var {
                                    Env::assign_force(var_env, &d.id, v);
                                } else {
                                    Env::init_lexical(target_env, &d.id, v)?;
                                }
                            }
                        }
                    }
                }
                loop {
                    if let Some(t) = test {
                        if !self.eval_expr(t, loop_env.clone())?.to_boolean() {
                            break;
                        }
                    }
                    match self.exec_stmt(body, loop_env.clone(), var_env)? {
                        Signal::Break => break,
                        Signal::Continue => {}
                        Signal::Normal(_) => {}
                        other => return Ok(other),
                    }
                    if let Some(u) = update {
                        self.eval_expr(u, loop_env.clone())?;
                    }
                }
                Ok(Signal::Normal(Value::Undefined))
            }

            StmtKind::ForInOf {
                is_of,
                left,
                right,
                body,
            } => {
                let rv = self.eval_expr(right, env.clone())?;
                let items = self.for_iter_items(rv, *is_of)?;
                if *is_of {
                    for item in items {
                        let iter_env = self.for_left_env(left, &env, var_env)?;
                        self.for_left_assign(left, item, &iter_env, var_env)?;
                        match self.exec_stmt(body, iter_env, var_env)? {
                            Signal::Break => break,
                            Signal::Continue => continue,
                            Signal::Normal(_) => continue,
                            other => return Ok(other),
                        }
                    }
                } else {
                    for k in items {
                        let iter_env = self.for_left_env(left, &env, var_env)?;
                        self.for_left_assign(left, k, &iter_env, var_env)?;
                        match self.exec_stmt(body, iter_env, var_env)? {
                            Signal::Break => break,
                            Signal::Continue => continue,
                            Signal::Normal(_) => continue,
                            other => return Ok(other),
                        }
                    }
                }
                Ok(Signal::Normal(Value::Undefined))
            }

            StmtKind::Return(e) => {
                let v = match e {
                    Some(x) => self.eval_expr(x, env)?,
                    None => Value::Undefined,
                };
                Ok(Signal::Return(v))
            }
            StmtKind::Break => Ok(Signal::Break),
            StmtKind::Continue => Ok(Signal::Continue),

            StmtKind::Throw(e) => {
                let v = self.eval_expr(e, env)?;
                Err(FlowError::Thrown(v))
            }

            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                let try_env = Env::child(&env);
                let mut outcome: Result<Signal, FlowError> =
                    self.exec_block(block, try_env, var_env);
                // Phase 14：catch 捕获一切错误——JS `throw` 的值原样，
                // 引擎内部 Runtime 错误转为对应 Error 子类对象；
                // 失控保护错误（uncatchable）不进 handler，直接向上传播。
                if let Err(e) = outcome {
                    let uncatchable =
                        matches!(&e, FlowError::Runtime(r) if r.uncatchable);
                    if uncatchable {
                        outcome = Err(e);
                    } else {
                        let tv = self.flow_error_to_value(e);
                        outcome = match handler {
                            Some(h) => {
                                let henv = Env::child(&env);
                                if let Some(p) = &h.param {
                                    Env::declare_lexical(&henv, p, DeclKind::Let)?;
                                    Env::init_lexical(&henv, p, tv)?;
                                }
                                self.exec_block(&h.body, henv, var_env)
                            }
                            None => Err(FlowError::Thrown(tv)),
                        };
                    }
                }
                if let Some(fin) = finalizer {
                    let fenv = Env::child(&env);
                    match self.exec_block(fin, fenv, var_env)? {
                        Signal::Normal(_) => {}
                        // finally 里的 return/throw/break 会覆盖原结果。
                        s => outcome = Ok(s),
                    }
                }
                outcome
            }

            StmtKind::Switch { disc, cases } => {
                let dv = self.eval_expr(disc, env.clone())?;
                let sw_env = Env::child(&env);
                let mut active = false;
                'outer: for c in cases {
                    if !active {
                        match &c.test {
                            Some(t) => {
                                if dv.strict_eq(&self.eval_expr(t, sw_env.clone())?) {
                                    active = true;
                                }
                            }
                            None => active = true, // default:
                        }
                    }
                    if active {
                        match self.exec_block(&c.body, sw_env.clone(), var_env)? {
                            Signal::Break => break 'outer,
                            Signal::Normal(_) => {}
                            // return / continue 向上穿透（continue 归属外层循环，语义正确）。
                            s => return Ok(s),
                        }
                    }
                }
                Ok(Signal::Normal(Value::Undefined))
            }
        }
    }

    /// phase 11：VM 用——for-in/of 右值的迭代展开（由 `exec_stmt` 的
    /// ForInOf 分支提取，行为一致）。返回迭代出的值序列。
    pub(crate) fn for_iter_items(
        &mut self,
        rv: Value,
        is_of: bool,
    ) -> Result<Vec<Value>, FlowError> {
        if is_of {
            match &rv {
                Value::Array(a) => Ok(a.borrow().elems.clone()),
                Value::String(s) => {
                    Ok(s.chars().map(|c| Value::String(c.to_string())).collect())
                }
                // phase 9：生成器可迭代（耗尽）。
                Value::Generator(g) => self.gen_drain(g),
                // phase 10：Map → [k,v] 对；Set → 值；TypedArray → 数值。
                Value::Map(m) => Ok(m
                    .borrow()
                    .entries
                    .iter()
                    .map(|(k, v)| {
                        Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                            vec![k.clone(), v.clone()],
                            Some(self.protos.array.clone()),
                        ))))
                    })
                    .collect()),
                Value::Set(s) => Ok(s.borrow().entries.clone()),
                Value::TypedArray(t) => {
                    let ta = t.borrow();
                    let buf = ta.buffer.borrow();
                    let bytes = buf.bytes.borrow();
                    Ok((0..ta.len)
                        .map(|i| Value::Number(ta.read_at(&bytes, i)))
                        .collect())
                }
                _ => Err(type_err(format!(
                    "for-of: {} is not iterable",
                    rv.type_of()
                ))),
            }
        } else {
            let keys: Vec<String> = match &rv {
                Value::Object(o) => o.borrow().keys(),
                Value::Array(a) => {
                    (0..a.borrow().elems.len()).map(|i| i.to_string()).collect()
                }
                Value::String(s) => {
                    (0..s.chars().count()).map(|i| i.to_string()).collect()
                }
                _ => {
                    return Err(type_err(format!(
                        "for-in: cannot enumerate {}",
                        rv.type_of()
                    )));
                }
            };
            Ok(keys.into_iter().map(Value::String).collect())
        }
    }

    /// for-in/of 左侧的作用域：let/const 每轮迭代一个新环境（闭包语义近似）。
    fn for_left_env(
        &mut self,
        left: &ForLeft,
        env: &EnvRef,
        var_env: &EnvRef,
    ) -> Result<EnvRef, FlowError> {
        match left {
            ForLeft::VarDecl {
                kind: VarKind::Var,
                ..
            } => Ok(var_env.clone()),
            ForLeft::VarDecl { .. } => Ok(Env::child(env)),
            ForLeft::Expr(_) => Ok(env.clone()),
        }
    }

    fn for_left_assign(
        &mut self,
        left: &ForLeft,
        val: Value,
        iter_env: &EnvRef,
        var_env: &EnvRef,
    ) -> Result<(), FlowError> {
        match left {
            ForLeft::VarDecl { kind, name } => {
                if *kind == VarKind::Var {
                    Env::assign_force(var_env, name, val);
                } else {
                    Env::declare_lexical(iter_env, name, decl_kind(*kind))?;
                    Env::init_lexical(iter_env, name, val)?;
                }
                Ok(())
            }
            ForLeft::Expr(e) => {
                let t = self.eval_target(e, iter_env.clone())?;
                self.set_target(&t, val)?;
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------
    // 表达式
    // ------------------------------------------------------------------

    fn eval_expr(&mut self, e: &Expr, env: EnvRef) -> Result<Value, FlowError> {
        self.tick()?;
        match &e.node {
            ExprKind::Literal(l) => Ok(match l {
                Literal::Number(n) => Value::Number(*n),
                Literal::String(s) => Value::String(s.clone()),
                Literal::Bool(b) => Value::Bool(*b),
                Literal::Null => Value::Null,
                // phase 2 注释：模板按整体存原文；phase 3 仍当普通字符串（TODO: 插值求值）。
                Literal::Template(s) => Value::String(s.clone()),
                // Phase 7：正则字面量求值（编译失败 → 运行时错误）。
                Literal::Regex { pattern, flags } => {
                    match CompiledRegex::compile(pattern, flags) {
                        Ok(c) => Value::RegExp(JsRegExp::new(c)),
                        Err(e) => {
                            return Err(syntax_err(format!("invalid regex: {e}")));
                        }
                    }
                }
            }),

            ExprKind::Ident(name) => match Env::lookup(&env, name)? {
                Some(v) => Ok(v),
                None => Err(ref_err(format!("'{}' is not defined", name))),
            },

            ExprKind::This => Ok(self
                .this_stack
                .last()
                .cloned()
                .unwrap_or(Value::Undefined)),

            ExprKind::Array(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for el in elems {
                    match el {
                        ArrayElem::Expr(x) => out.push(self.eval_expr(x, env.clone())?),
                        // 空位 `[1, , 3]`：子集里近似为 Undefined（join 等边角有偏差，见 value.rs）。
                        ArrayElem::Hole => out.push(Value::Undefined),
                        // phase 9：`[...gen()]` —— 数组/字符串/生成器展开。
                        ArrayElem::Spread(x) => {
                            let v = self.eval_expr(x, env.clone())?;
                            out.extend(self.spread_into_vec(&v)?);
                        }
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                    out,
                    Some(self.protos.array.clone()),
                )))))
            }

            ExprKind::Object(props) => {
                let obj = Rc::new(RefCell::new(JsObject::with_proto(Some(
                    self.protos.object.clone(),
                ))));
                obj.borrow_mut().tag = Some("Object".to_string());
                for p in props {
                    let key = match &p.key {
                        PropKey::Ident(s) | PropKey::String(s) => s.clone(),
                        PropKey::Number(n) => number_to_js_string(*n),
                        PropKey::Computed(x) => {
                            self.eval_expr(x, env.clone())?.to_js_string()
                        }
                    };
                    let val = match &p.value {
                        PropValue::Init(x) => self.eval_expr(x, env.clone())?,
                        PropValue::Shorthand(name) => match Env::lookup(&env, name)? {
                            Some(v) => v,
                            None => return Err(ref_err(format!("'{}' is not defined", name))),
                        },
                        PropValue::Method(f) => {
                            Value::Function(self.make_function(key.clone(), f, env.clone(), false, e.span.clone()))
                        }
                        // getter/setter：存为普通函数值，不触发访问器语义（TODO phase 4）。
                        PropValue::Getter(f) | PropValue::Setter(f) => {
                            Value::Function(self.make_function(key.clone(), f, env.clone(), false, e.span.clone()))
                        }
                    };
                    obj.borrow_mut().set(&key, val);
                }
                Ok(Value::Object(obj))
            }

            ExprKind::Function(f) => {
                let name = f.id.clone().unwrap_or_default();
                // 具名函数表达式：名字在内部可见（支持递归），不污染外层。
                let closure = if name.is_empty() {
                    env.clone()
                } else {
                    let inner = Env::child(&env);
                    Env::declare_lexical(&inner, &name, DeclKind::Let)?;
                    let fun = self.make_function(name.clone(), f, inner.clone(), false, e.span.clone());
                    Env::init_lexical(&inner, &name, Value::Function(fun.clone()))?;
                    return Ok(Value::Function(fun));
                };
                Ok(Value::Function(self.make_function(name, f, closure, false, e.span.clone())))
            }

            ExprKind::ArrowFunction(a) => {
                // async 箭头（phase 7）：改写为状态机体（表达式体包一层 return）。
                if a.is_async {
                    let body_stmts: Vec<Stmt> = match &a.body {
                        ArrowBody::Expr(x) => {
                            let x = x.clone();
                            let span = x.span.clone();
                            vec![Stmt {
                                node: StmtKind::Return(Some(*x)),
                                span,
                            }]
                        }
                        ArrowBody::Block(b) => b.clone(),
                    };
                    // 借用原参数做改写（await 在默认值里解析期已拒绝）。
                    let new_body = AsyncDesugar::new()
                        .with_strict(a.strict)
                        .desugar(&a.params, &body_stmts)
                        .map_err(|e| RuntimeError::new(e.0))?;
                    let fun = Rc::new(JsFunction::with_ext(
                        None,
                        a.params.clone(),
                        FuncBody::Block(new_body),
                        env,
                        true,
                        None,
                        // phase 8：箭头函数的严格模式来自解析期折叠。
                        a.strict,
                        e.span.clone(),
                        Phase9FuncFields::default(),
                    ));
                    return Ok(Value::Function(fun));
                }
                let fun = Rc::new(JsFunction::with_ext(
                    None,
                    a.params.clone(),
                    match &a.body {
                        ArrowBody::Expr(x) => FuncBody::Expr(x.clone()),
                        ArrowBody::Block(b) => FuncBody::Block(b.clone()),
                    },
                    env,
                    true,
                    // 箭头函数没有 prototype。
                    None,
                    // phase 8：箭头函数的严格模式来自解析期折叠。
                    a.strict,
                    e.span.clone(),
                    Phase9FuncFields::default(),
                ));
                Ok(Value::Function(fun))
            }

            ExprKind::Unary { op, arg } => self.eval_unary(*op, arg, env),
            // Phase 7：`await` 在求值前已被 async 改写消除；走到这里是内部错误。
            ExprKind::Await(_) => Err(rt(
                "internal: await reached the evaluator (async desugar missed it)",
            )),
            // phase 9：`yield` 在求值前已被 generator 改写消除；走到这里是内部错误。
            ExprKind::Yield { .. } => Err(rt(
                "internal: yield reached the evaluator (generator desugar missed it)",
            )),
            // phase 9：`super` 只在 Call/Member 上下文中有意义（见 eval_call 与
            // Member 分支）；单独求值是内部错误。
            ExprKind::Super => Err(rt("internal: super reached the evaluator")),
            // phase 9：类表达式。
            ExprKind::Class(c) => self.eval_class(c, None, env, e.span.clone()),
            // phase 9：`#x in obj` 的左操作数只在 In 分支处理；单独求值是内部错误。
            ExprKind::PrivateName(_) => Err(rt(
                "internal: private name reached the evaluator outside 'in'",
            )),
            // phase 9：`obj.#x` 私有成员访问。
            ExprKind::PrivateMember { obj, name, optional } => {
                let base = self.eval_expr(obj, env.clone())?;
                if *optional && base.is_nullish() {
                    return Ok(Value::Undefined);
                }
                self.get_private(&base, name)
            }
            ExprKind::Update { op, arg, prefix } => {
                let target = self.eval_target(arg, env.clone())?;
                let cur = self.get_target(&target)?;
                let n = cur.to_number();
                let new_v = Value::Number(match op {
                    UpdateOp::Inc => n + 1.0,
                    UpdateOp::Dec => n - 1.0,
                });
                self.set_target(&target, new_v.clone())?;
                Ok(if *prefix { new_v } else { cur })
            }
            ExprKind::Binary { op, left, right } => {
                // phase 9：`#x in obj` 私有品牌检查——左操作数不求值。
                if *op == BinaryOp::In {
                    if let ExprKind::PrivateName(name) = &left.node {
                        let r = self.eval_expr(right, env)?;
                        return Ok(self.private_in(name, &r)?);
                    }
                }
                let l = self.eval_expr(left, env.clone())?;
                let r = self.eval_expr(right, env)?;
                self.apply_binary(*op, l, r)
            }
            ExprKind::Logical { op, left, right } => {
                let l = self.eval_expr(left, env.clone())?;
                match op {
                    LogicalOp::And => {
                        if l.to_boolean() {
                            self.eval_expr(right, env)
                        } else {
                            Ok(l)
                        }
                    }
                    LogicalOp::Or => {
                        if l.to_boolean() {
                            Ok(l)
                        } else {
                            self.eval_expr(right, env)
                        }
                    }
                    LogicalOp::Nullish => {
                        if l.is_nullish() {
                            self.eval_expr(right, env)
                        } else {
                            Ok(l)
                        }
                    }
                }
            }
            ExprKind::Assign { op, left, right } => {
                let target = self.eval_target(left, env.clone())?;
                let r = self.eval_expr(right, env.clone())?;
                let new_v = match op {
                    AssignOp::Assign => r,
                    // `a &&= b` ≡ `a && (a = b)`：条件赋值，非简单读-改-写。
                    AssignOp::AndAssign => {
                        let cur = self.get_target(&target)?;
                        if !cur.to_boolean() {
                            return Ok(cur);
                        }
                        r
                    }
                    AssignOp::OrAssign => {
                        let cur = self.get_target(&target)?;
                        if cur.to_boolean() {
                            return Ok(cur);
                        }
                        r
                    }
                    AssignOp::NullishAssign => {
                        let cur = self.get_target(&target)?;
                        if !cur.is_nullish() {
                            return Ok(cur);
                        }
                        r
                    }
                    _ => {
                        let cur = self.get_target(&target)?;
                        self.apply_compound(*op, cur, r)?
                    }
                };
                self.set_target(&target, new_v.clone())?;
                Ok(new_v)
            }
            ExprKind::Conditional { test, cons, alt } => {
                if self.eval_expr(test, env.clone())?.to_boolean() {
                    self.eval_expr(cons, env)
                } else {
                    self.eval_expr(alt, env)
                }
            }
            ExprKind::Call {
                callee,
                args,
                optional,
            } => self.eval_call(&e.span, callee, args, *optional, env),
            ExprKind::New { callee, args } => self.eval_new(callee, args, env),
            ExprKind::Member {
                obj,
                prop,
                computed,
                optional,
            } => {
                // phase 9：`super.x` —— 从 home_object 的原型链查找。
                if matches!(&obj.node, ExprKind::Super) {
                    let key = self.member_key(prop, *computed, env)?;
                    return self.get_super_prop(&key);
                }
                let base = self.eval_expr(obj, env.clone())?;
                if *optional && base.is_nullish() {
                    return Ok(Value::Undefined);
                }
                let key = self.member_key(prop, *computed, env)?;
                self.get_prop(&base, &key)
            }
            ExprKind::Sequence(es) => {
                let mut v = Value::Undefined;
                for x in es {
                    v = self.eval_expr(x, env.clone())?;
                }
                Ok(v)
            }
        }
    }

    fn eval_unary(
        &mut self,
        op: UnaryOp,
        arg: &Expr,
        env: EnvRef,
    ) -> Result<Value, FlowError> {
        match op {
            UnaryOp::Typeof => {
                // `typeof 未声明变量` 不报错，得 "undefined"。
                if let ExprKind::Ident(name) = &arg.node {
                    return Ok(Value::String(
                        match Env::lookup(&env, name)? {
                            Some(v) => v.type_of().to_string(),
                            None => "undefined".to_string(),
                        },
                    ));
                }
                Ok(Value::String(
                    self.eval_expr(arg, env)?.type_of().to_string(),
                ))
            }
            UnaryOp::Void => {
                self.eval_expr(arg, env)?;
                Ok(Value::Undefined)
            }
            UnaryOp::Delete => {
                match &arg.node {
                    ExprKind::Member { obj, prop, computed, .. } => {
                        let o = self.eval_expr(obj, env.clone())?;
                        let key = self.member_key(prop, *computed, env)?;
                        self.delete_prop_value(o, key)
                    }
                    ExprKind::Ident(name) => {
                        // phase 8：`delete x`（裸标识符）严格模式是 SyntaxError。
                        if self.is_strict() {
                            return Err(syntax_err(format!(
                                "delete of an unqualified identifier '{name}' in strict mode"
                            )));
                        }
                        self.eval_expr(arg, env)?;
                        Ok(Value::Bool(true))
                    }
                    _ => {
                        self.eval_expr(arg, env)?;
                        Ok(Value::Bool(true))
                    }
                }
            }
            _ => {
                let v = self.eval_expr(arg, env)?;
                Ok(match op {
                    UnaryOp::Neg => Value::Number(-v.to_number()),
                    UnaryOp::Pos => Value::Number(v.to_number()),
                    UnaryOp::Not => Value::Bool(!v.to_boolean()),
                    UnaryOp::BitNot => Value::Number(!(v.to_int32()) as f64),
                    UnaryOp::Typeof | UnaryOp::Void | UnaryOp::Delete => unreachable!(),
                })
            }
        }
    }

    /// phase 11：VM 用——`delete obj.key` 的成员删除语义
    /// （从 `eval_unary` 的 Delete 分支提取，行为一致）。
    pub(crate) fn delete_prop_value(
        &mut self,
        o: Value,
        key: String,
    ) -> Result<Value, FlowError> {
        // phase 8：严格模式下删除不可配置属性 → TypeError；
        // sloppy 模式返回 false（旧行为一律返回 true）。
        if Self::is_non_configurable(&o, &key) {
            if self.is_strict() {
                return Err(type_err(format!(
                    "cannot delete non-configurable property '{key}'"
                )));
            }
            return Ok(Value::Bool(false));
        }
        let deleted = match o {
            Value::Object(ob) => ob.borrow_mut().delete(&key),
            Value::Array(ab) => {
                if let Ok(i) = key.parse::<usize>() {
                    let mut a = ab.borrow_mut();
                    if i < a.elems.len() {
                        a.elems[i] = Value::Undefined;
                    }
                    true
                } else {
                    ab.borrow_mut().props.delete(&key)
                }
            }
            _ => true,
        };
        Ok(Value::Bool(deleted))
    }

    /// phase 8：子集里的不可配置属性（内建只读属性：数组/字符串的
    /// `length`、函数的 `length`/`prototype`）。
    fn is_non_configurable(base: &Value, key: &str) -> bool {
        match base {
            Value::Array(_) => key == "length",
            Value::Function(_) | Value::Native(_) => {
                key == "length" || key == "prototype"
            }
            Value::String(_) => key == "length",
            _ => false,
        }
    }

    // ------------------------------------------------------------------
    // 赋值目标
    // ------------------------------------------------------------------

    pub(crate) fn eval_target(&mut self, e: &Expr, env: EnvRef) -> Result<Target, FlowError> {
        match &e.node {
            ExprKind::Ident(name) => match Env::find_env(&env, name) {
                Some(def_env) => Ok(Target::Var(def_env, name.clone())),
                // phase 8：严格模式给未声明变量赋值 → ReferenceError；
                // sloppy 模式创建全局 var（旧行为）。
                None => {
                    if self.is_strict() {
                        return Err(ref_err(format!("'{name}' is not defined")));
                    }
                    Ok(Target::Var(self.global.clone(), name.clone()))
                }
            },
            ExprKind::Member { obj, prop, computed, .. } => {
                // phase 9：`super.x = v` —— 按规范写到 receiver（当前 this）上。
                let base = if matches!(&obj.node, ExprKind::Super) {
                    self.this_stack.last().cloned().unwrap_or(Value::Undefined)
                } else {
                    self.eval_expr(obj, env.clone())?
                };
                // phase 9：`obj.#x = v` 私有成员赋值（Member 形式的 PrivateName，
                // 主要由改写器生成；手写代码走下面的 PrivateMember 分支）。
                if !computed {
                    if let ExprKind::PrivateName(name) = &prop.node {
                        return Ok(Target::PrivateProp(base, name.clone()));
                    }
                }
                let key = self.member_key(prop, *computed, env)?;
                match &base {
                    Value::Object(_)
                    | Value::Array(_)
                    | Value::String(_)
                    | Value::DomNode(_)
                    // phase 9：Proxy 写走 set trap（set_prop 里分发）。
                    | Value::Proxy(_)
                    // phase 9：函数也是对象（类静态字段 `C.x = 1` 用；set_prop 写 statics）。
                    | Value::Function(_)
                    // phase 10：TypedArray 索引写入（set_prop 里分发）。
                    | Value::TypedArray(_)
                    | Value::DataView(_)
                    | Value::Map(_)
                    | Value::Set(_) => {}
                    _ => {
                        return Err(type_err(format!(
                            "cannot set property '{}' of {}",
                            key,
                            base.type_of()
                        )));
                    }
                }
                Ok(Target::Prop(base, key))
            }
            // phase 9：`obj.#x = v` 私有成员赋值。
            ExprKind::PrivateMember { obj, name, .. } => {
                let base = self.eval_expr(obj, env)?;
                Ok(Target::PrivateProp(base, name.clone()))
            }
            _ => Err(syntax_err("invalid assignment target")),
        }
    }

    pub(crate) fn get_target(&mut self, t: &Target) -> Result<Value, FlowError> {
        match t {
            Target::Var(env, name) => match Env::lookup(env, name)? {
                Some(v) => Ok(v),
                // sloppy 全局：声明了但（理论上）不可能到这里；兜底 undefined。
                None => Ok(Value::Undefined),
            },
            Target::Prop(base, key) => self.get_prop(base, key),
            // phase 9：`obj.#x` 读（`++obj.#x` 等复合赋值用）。
            Target::PrivateProp(base, name) => self.get_private(base, name),
        }
    }

    pub(crate) fn set_target(&mut self, t: &Target, val: Value) -> Result<(), FlowError> {
        match t {
            Target::Var(env, name) => {
                if !Env::assign(env, name, val.clone())? {
                    // 全局新建（sloppy 赋值）。
                    Env::declare_var(env, name);
                    Env::assign_force(env, name, val);
                }
                Ok(())
            }
            Target::Prop(base, key) => self.set_prop(base, key, val),
            // phase 9：`obj.#x = v` 私有成员赋值。
            Target::PrivateProp(base, name) => self.set_private(base, name, val),
        }
    }

    /// phase 11：VM 用——`Ident` 表达式求值（`x` 读）。
    /// 语义与 `eval_expr` 的 `Ident` 分支一致。
    pub(crate) fn lookup_name(&mut self, env: &EnvRef, name: &str) -> Result<Value, FlowError> {
        match Env::lookup(env, name)? {
            Some(v) => Ok(v),
            None => Err(ref_err(format!("'{}' is not defined", name))),
        }
    }

    /// phase 11：VM 用——`x = v` 赋值（`eval_target` Ident 分支 + `set_target`）。
    pub(crate) fn assign_name(
        &mut self,
        env: &EnvRef,
        name: &str,
        val: Value,
    ) -> Result<(), FlowError> {
        let t = match Env::find_env(env, name) {
            Some(def_env) => Target::Var(def_env, name.to_string()),
            // phase 8：严格模式给未声明变量赋值 → ReferenceError；
            // sloppy 模式创建全局 var。
            None => {
                if self.is_strict() {
                    return Err(ref_err(format!("'{}' is not defined", name)));
                }
                Target::Var(self.global.clone(), name.to_string())
            }
        };
        self.set_target(&t, val)
    }

    fn member_key(
        &mut self,
        prop: &Expr,
        computed: bool,
        env: EnvRef,
    ) -> Result<String, FlowError> {
        if computed {
            Ok(self.eval_expr(prop, env)?.to_js_string())
        } else {
            match &prop.node {
                ExprKind::Ident(n) => Ok(n.clone()),
                ExprKind::Literal(Literal::String(s)) => Ok(s.clone()),
                ExprKind::Literal(Literal::Number(n)) => Ok(number_to_js_string(*n)),
                _ => Err(type_err("invalid property name")),
            }
        }
    }

    // ------------------------------------------------------------------
    // 属性读写
    // ------------------------------------------------------------------

    /// phase 9：沿原型链找访问器。`is_get` 为 true 找 getter，否则找 setter。
    /// 返回找到的 FuncRef（原型链上第一个匹配的）。
    fn find_accessor(
        &self,
        obj: &ObjectRef,
        key: &str,
        is_get: bool,
    ) -> Result<Option<FuncRef>, FlowError> {
        let mut link: Option<ObjectRef> = Some(obj.clone());
        while let Some(pr) = link {
            let b = pr.borrow();
            if let Some((getter, setter)) = b.accessors.get(key) {
                let f = if is_get { getter } else { setter };
                if let Some(func) = f {
                    return Ok(Some(func.clone()));
                }
                // 有访问器但对应方向缺失：按规范，读返回 undefined，写忽略（sloppy）。
                // 子集：直接返回 None，让调用方按普通属性处理。
                return Ok(None);
            }
            link = b.proto.clone();
        }
        Ok(None)
    }

    pub(crate) fn get_prop(&mut self, base: &Value, key: &str) -> Result<Value, FlowError> {
        match base {
            // 普通对象：自身 → 原型链。
            // phase 9：访问器优先——找到 getter 则调用（this 绑定为 base）。
            // phase 13：Storage 标签对象——`length` 与具名存储项优先；
            // USP 标签对象——`size` 动态计算。
            Value::Object(o) => {
                let tag = o.borrow().tag.clone();
                if tag.as_deref() == Some("Storage") {
                    return self.storage_get_prop(o, key);
                }
                if tag.as_deref() == Some("USP") && key == "size" {
                    return Ok(Value::Number(
                        crate::webapi::usp_pairs_of(o).len() as f64,
                    ));
                }
                if let Some(getter) = self.find_accessor(o, key, true)? {
                    return self.call_value_at(
                        Value::Function(getter),
                        base.clone(),
                        vec![],
                        None,
                    );
                }
                Ok(o.borrow().get_in_chain(key).unwrap_or(Value::Undefined))
            }
            Value::Array(a) => {
                let arr = a.borrow();
                if key == "length" {
                    return Ok(Value::Number(arr.logical_len() as f64));
                }
                if let Ok(i) = key.parse::<usize>() {
                    return Ok(arr.elems.get(i).cloned().unwrap_or(Value::Undefined));
                }
                Ok(arr.get_in_chain(key).unwrap_or(Value::Undefined))
            }
            Value::String(s) => {
                if key == "length" {
                    return Ok(Value::Number(s.chars().count() as f64));
                }
                if let Ok(i) = key.parse::<usize>() {
                    return Ok(s
                        .chars()
                        .nth(i)
                        .map(|c| Value::String(c.to_string()))
                        .unwrap_or(Value::Undefined));
                }
                // String.prototype 方法（split / slice / …）。
                Ok(self
                    .protos
                    .string
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::Number(_) => Ok(self
                .protos
                .number
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::Function(f) => {
                if key == "name" {
                    return Ok(Value::String(f.name.clone().unwrap_or_default()));
                }
                if key == "length" {
                    return Ok(Value::Number(f.params.len() as f64));
                }
                if key == "prototype" {
                    // 箭头函数没有 prototype。
                    return Ok(f
                        .prototype
                        .clone()
                        .map(Value::Object)
                        .unwrap_or(Value::Undefined));
                }
                // phase 9：类静态成员 → 父构造器静态继承链 → Function.prototype。
                if let Some(v) = f.statics.borrow().get(key) {
                    return Ok(v.clone());
                }
                if let Some(parent) = &f.super_ctor {
                    let v = self.get_prop(parent, key)?;
                    if !v.is_undefined() {
                        return Ok(v);
                    }
                }
                Ok(self
                    .protos
                    .function
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::Native(_) => Ok(self
                .protos
                .function
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::DomNode(n) => Ok(self.get_dom_prop(n, key)),
            // Phase 7：Promise 方法走 Promise.prototype；RegExp 走
            // RegExp.prototype（`source` / `lastIndex` 等自有属性优先）。
            Value::Promise(_) => Ok(self
                .protos
                .promise
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::PromiseSettler(_) => Ok(self
                .protos
                .function
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::RegExp(r) => {
                let b = r.borrow();
                let special = match key {
                    "source" => Some(Value::String(b.compiled.source.clone())),
                    "lastIndex" => Some(Value::Number(b.last_index as f64)),
                    "global" => Some(Value::Bool(b.compiled.flags.global)),
                    "ignoreCase" => Some(Value::Bool(b.compiled.flags.ignore_case)),
                    "multiline" => Some(Value::Bool(b.compiled.flags.multiline)),
                    _ => None,
                };
                drop(b);
                if let Some(v) = special {
                    return Ok(v);
                }
                Ok(self
                    .protos
                    .regexp
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::Bool(_) => Ok(Value::Undefined),
            // phase 9：生成器走 Generator.prototype；Proxy 走 get trap。
            Value::Generator(g) => {
                // `next`/`return`/`throw` 在 try_host_method 拦截（需解释器驱动）；
                // 其余走 Generator.prototype。
                let _ = g;
                Ok(self
                    .protos
                    .generator
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::AsyncGenerator(g) => {
                // `next`/`return`/`throw` 在 try_host_method 拦截；
                // 其余走 AsyncGenerator.prototype。
                let _ = g;
                Ok(self
                    .protos
                    .async_generator
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::Proxy(p) => {
                let (target, handler) = {
                    let pr = p.borrow();
                    (pr.target.clone(), pr.handler.clone())
                };
                // get trap：handler.get 存在且可调用 → trap(target, key, receiver)。
                if let Some(trap) = self.get_trap(&handler, "get")? {
                    return self.call_value(
                        trap,
                        Value::Object(handler),
                        vec![
                            target,
                            Value::String(key.to_string()),
                            Value::Proxy(p.clone()),
                        ],
                    );
                }
                self.get_prop(&target, key)
            }
            // phase 10：集合与二进制视图。
            Value::Map(m) => {
                if key == "size" {
                    return Ok(Value::Number(m.borrow().entries.len() as f64));
                }
                Ok(self
                    .protos
                    .map
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::Set(s) => {
                if key == "size" {
                    return Ok(Value::Number(s.borrow().entries.len() as f64));
                }
                Ok(self
                    .protos
                    .set
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::WeakMap(_) => Ok(self
                .protos
                .weakmap
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::WeakSet(_) => Ok(self
                .protos
                .weakset
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::ArrayBuffer(b) => {
                if key == "byteLength" {
                    let bb = b.borrow();
                    let bytes = bb.bytes.borrow();
                    return Ok(Value::Number(bytes.len() as f64));
                }
                Ok(self
                    .protos
                    .arraybuffer
                    .borrow()
                    .get_in_chain(key)
                    .unwrap_or(Value::Undefined))
            }
            Value::TypedArray(t) => {
                let ta = t.borrow();
                match key {
                    "length" => return Ok(Value::Number(ta.len as f64)),
                    "byteLength" => return Ok(Value::Number((ta.len * ta.kind.bytes()) as f64)),
                    "byteOffset" => return Ok(Value::Number(ta.byte_offset as f64)),
                    "buffer" => return Ok(Value::ArrayBuffer(ta.buffer.clone())),
                    "BYTES_PER_ELEMENT" => {
                        return Ok(Value::Number(ta.kind.bytes() as f64))
                    }
                    _ => {}
                }
                if let Ok(i) = key.parse::<usize>() {
                    if i < ta.len {
                        let buf = ta.buffer.borrow();
                        let bytes = buf.bytes.borrow();
                        return Ok(Value::Number(ta.read_at(&bytes, i)));
                    }
                    return Ok(Value::Undefined);
                }
                let proto = self.protos.typedarray.clone();
                drop(ta);
                Ok(proto.borrow().get_in_chain(key).unwrap_or(Value::Undefined))
            }
            Value::DataView(d) => {
                let dv = d.borrow();
                match key {
                    "byteLength" => return Ok(Value::Number(dv.byte_len as f64)),
                    "byteOffset" => return Ok(Value::Number(dv.byte_offset as f64)),
                    "buffer" => return Ok(Value::ArrayBuffer(dv.buffer.clone())),
                    _ => {}
                }
                let proto = self.protos.dataview.clone();
                drop(dv);
                Ok(proto.borrow().get_in_chain(key).unwrap_or(Value::Undefined))
            }
            Value::Date(_) => Ok(self
                .protos
                .date
                .borrow()
                .get_in_chain(key)
                .unwrap_or(Value::Undefined)),
            Value::Null | Value::Undefined => Err(type_err(format!(
                "cannot read property '{}' of {}",
                key,
                base.type_of()
            ))),
        }
    }

    pub(crate) fn set_prop(&mut self, base: &Value, key: &str, val: Value) -> Result<(), FlowError> {
        // phase 9：Proxy 写走 set trap。
        if let Value::Proxy(p) = base {
            return self.proxy_set(p, key, val);
        }
        match base {
            Value::Object(o) => {
                // phase 9：访问器优先——找到 setter 则调用（this 绑定为 base）。
                // phase 13：Storage 标签对象——具名写走 `setItem` 语义。
                if o.borrow().tag.as_deref() == Some("Storage") {
                    return self.storage_set_prop(o, key, val);
                }
                if let Some(setter) = self.find_accessor(o, key, false)? {
                    self.call_value_at(
                        Value::Function(setter),
                        base.clone(),
                        vec![val],
                        None,
                    )?;
                    return Ok(());
                }
                o.borrow_mut().set(key, val);
                Ok(())
            }
            Value::Array(a) => {
                let mut arr = a.borrow_mut();
                if key == "length" {
                    let n = val.to_number().max(0.0) as usize;
                    // Phase 14：超大 length 走稀疏（避免 OOM）。
                    if n > 10_000_000 {
                        arr.elems.clear();
                        arr.sparse_extra = n;
                    } else {
                        arr.elems.resize(n, Value::Undefined);
                        arr.sparse_extra = 0;
                    }
                    return Ok(());
                }
                if let Ok(i) = key.parse::<usize>() {
                    // Phase 14：稀疏大数组写索引时按需物化（上限内）。
                    if i >= arr.elems.len() {
                        if i >= arr.logical_len() {
                            // 超出逻辑长度：扩展稀疏补偿（罕见）。
                            arr.sparse_extra = i + 1;
                        }
                        // 物化上限：避免 `arr[4e9]=x` 直接 OOM。
                        if i < 10_000_000 {
                            arr.elems.resize(i + 1, Value::Undefined);
                        } else {
                            // 超大索引写入：仅记录到 props（子集近似）。
                            arr.props.set(&i.to_string(), val);
                            return Ok(());
                        }
                    }
                    arr.elems[i] = val;
                    return Ok(());
                }
                arr.props.set(key, val);
                Ok(())
            }
            Value::DomNode(n) => self.set_dom_prop(n, key, val),
            // phase 9：函数也是对象——属性写进 statics（类静态字段 `C.x = 1` 用）。
            Value::Function(f) => {
                f.statics.borrow_mut().insert(istr(key), val);
                Ok(())
            }
            // Phase 7：正则的 lastIndex 可写（`/g` 的 test/exec 推进用）。
            Value::RegExp(r) => {
                if key == "lastIndex" {
                    let n = val.to_number().max(0.0) as usize;
                    r.borrow_mut().last_index = n;
                    Ok(())
                } else {
                    Err(type_err(format!(
                        "cannot set property '{}' of regexp",
                        key
                    )))
                }
            }
            // phase 10：TypedArray 索引写入（越界静默忽略，与规范一致）。
            Value::TypedArray(t) => {
                if let Ok(i) = key.parse::<usize>() {
                    let ta = t.borrow();
                    if i < ta.len {
                        let buf = ta.buffer.borrow();
                        let mut bytes = buf.bytes.borrow_mut();
                        ta.write_at(&mut bytes, i, val.to_number());
                    }
                    return Ok(());
                }
                Err(type_err(format!(
                    "cannot set property '{}' of {}",
                    key,
                    base.type_of()
                )))
            }
            _ => Err(type_err(format!(
                "cannot set property '{}' of {}",
                key,
                base.type_of()
            ))),
        }
    }

    // ------------------------------------------------------------------
    // 函数
    // ------------------------------------------------------------------

    pub(crate) fn make_function(
        &mut self,
        name: String,
        f: &FunctionNode,
        closure: EnvRef,
        is_arrow: bool,
        def_span: Span,
    ) -> FuncRef {
        self.make_function_ext(name, f, closure, is_arrow, def_span, Phase9FuncFields::default())
    }

    /// phase 9：带扩展字段的函数构造（类方法/构造器用）。
    pub(crate) fn make_function_ext(
        &mut self,
        name: String,
        f: &FunctionNode,
        closure: EnvRef,
        is_arrow: bool,
        def_span: Span,
        ext: Phase9FuncFields,
    ) -> FuncRef {
        // async 函数（phase 7）：改写为状态机体；无 prototype（`new` 报错）。
        // 改写后的函数体同步执行，末尾 `return Promise.resolve(__step())`。
        // phase 15：`async function*`（async 生成器）先跑 yield pre-pass，
        // 再走 AsyncDesugar；标记 is_async_generator。
        if f.is_async {
            if f.is_generator {
                let pre = crate::async_gen::AsyncGenPrepass::new().run(&f.body);
                let new_body = AsyncDesugar::new()
                    .with_strict(f.strict)
                    .desugar(&f.params, &pre)
                    .expect("async generator desugar cannot fail");
                let mut ext2 = ext;
                ext2.is_async_generator = true;
                return Rc::new(JsFunction::with_ext(
                    if name.is_empty() { None } else { Some(name) },
                    f.params.clone(),
                    FuncBody::Block(new_body),
                    closure,
                    is_arrow,
                    None,
                    f.strict,
                    def_span,
                    ext2,
                ));
            }
            let new_body = AsyncDesugar::new()
                .with_strict(f.strict)
                .desugar(&f.params, &f.body)
                .expect("async desugar of a parsed function cannot fail");
            return Rc::new(JsFunction::with_ext(
                if name.is_empty() { None } else { Some(name) },
                f.params.clone(),
                FuncBody::Block(new_body),
                closure,
                is_arrow,
                None,
                // phase 8：严格模式与定义点位置随函数走。
                f.strict,
                def_span,
                ext,
            ));
        }
        // 生成器函数（phase 9）：改写为状态机体；调用时不执行用户代码，
        // setup 体返回 `__yousj$step` 闭包，由解释器包装成 Generator。
        if f.is_generator {
            let new_body = crate::gen_desugar::GenDesugar::new()
                .with_strict(f.strict)
                .desugar(&f.params, &f.body)
                .expect("generator desugar of a parsed function cannot fail");
            // 生成器函数的 `.prototype` 存在但不用作 new 目标（`new gen()` 直接报错）。
            let prototype = Some(Rc::new(RefCell::new(JsObject::with_proto(Some(
                self.protos.generator.clone(),
            )))));
            return Rc::new(JsFunction::with_ext(
                if name.is_empty() { None } else { Some(name) },
                f.params.clone(),
                FuncBody::Block(new_body),
                closure,
                false,
                prototype,
                f.strict,
                def_span,
                Phase9FuncFields {
                    is_generator: true,
                    ..ext
                },
            ));
        }
        // 每个（非箭头）构造器函数拥有自己的 `.prototype` 对象，
        // 其原型指向 Object.prototype；`new` 的实例以此为原型。
        // phase 9：类构造器的 prototype 由调用方通过 ext 传入时优先采用。
        let prototype = if ext.is_class {
            ext.prototype_override.clone().or_else(|| {
                if is_arrow {
                    None
                } else {
                    Some(Rc::new(RefCell::new(JsObject::with_proto(Some(
                        self.protos.object.clone(),
                    )))))
                }
            })
        } else if is_arrow {
            None
        } else {
            Some(Rc::new(RefCell::new(JsObject::with_proto(Some(
                self.protos.object.clone(),
            )))))
        };
        Rc::new(JsFunction::with_ext(
            if name.is_empty() { None } else { Some(name) },
            f.params.clone(),
            FuncBody::Block(f.body.clone()),
            closure,
            is_arrow,
            prototype,
            f.strict,
            def_span,
            ext,
        ))
    }

    fn eval_args(&mut self, args_e: &[Expr], env: EnvRef) -> Result<Vec<Value>, FlowError> {
        let mut args = Vec::with_capacity(args_e.len());
        for a in args_e {
            args.push(self.eval_expr(a, env.clone())?);
        }
        Ok(args)
    }

    /// phase 11：VM 用——由字节码元数据创建函数值。
    /// 语义与 `eval_expr` 的 Function / ArrowFunction 分支逐一对应。
    pub(crate) fn make_function_value(
        &mut self,
        meta: &crate::bytecode::FuncMeta,
        method_name: Option<&str>,
        env: EnvRef,
    ) -> Result<Value, FlowError> {
        use crate::bytecode::FuncKind;
        let span = meta.span.clone();
        match &meta.kind {
            FuncKind::Decl(f) => {
                let name = f.id.clone().unwrap_or_default();
                Ok(Value::Function(self.make_function(
                    name,
                    f,
                    env,
                    false,
                    span,
                )))
            }
            FuncKind::Expr(f) => {
                let name = f.id.clone().unwrap_or_default();
                // 具名函数表达式：名字在内部可见（支持递归），不污染外层。
                let closure = if name.is_empty() {
                    env.clone()
                } else {
                    let inner = Env::child(&env);
                    Env::declare_lexical(&inner, &name, DeclKind::Let)?;
                    let fun = self.make_function(name.clone(), f, inner.clone(), false, span);
                    Env::init_lexical(&inner, &name, Value::Function(fun.clone()))?;
                    return Ok(Value::Function(fun));
                };
                Ok(Value::Function(self.make_function(
                    name, f, closure, false, span,
                )))
            }
            FuncKind::Arrow {
                params,
                body,
                strict,
                is_async,
            } => {
                // async 箭头（phase 7）：改写为状态机体（表达式体包一层 return）。
                if *is_async {
                    let body_stmts: Vec<Stmt> = match body {
                        ArrowBody::Expr(x) => {
                            let x = x.clone();
                            let span = x.span.clone();
                            vec![Stmt {
                                node: StmtKind::Return(Some(*x)),
                                span,
                            }]
                        }
                        ArrowBody::Block(b) => b.clone(),
                    };
                    let new_body = AsyncDesugar::new()
                        .with_strict(*strict)
                        .desugar(params, &body_stmts)
                        .map_err(|e| RuntimeError::new(e.0))?;
                    let fun = Rc::new(JsFunction::with_ext(
                        None,
                        params.clone(),
                        FuncBody::Block(new_body),
                        env,
                        true,
                        None,
                        *strict,
                        span,
                        Phase9FuncFields::default(),
                    ));
                    return Ok(Value::Function(fun));
                }
                let fun = Rc::new(JsFunction::with_ext(
                    None,
                    params.clone(),
                    match body {
                        ArrowBody::Expr(x) => FuncBody::Expr(x.clone()),
                        ArrowBody::Block(b) => FuncBody::Block(b.clone()),
                    },
                    env,
                    true,
                    None,
                    *strict,
                    span,
                    Phase9FuncFields::default(),
                ));
                Ok(Value::Function(fun))
            }
            FuncKind::Method(f) => {
                let name = method_name.unwrap_or("").to_string();
                Ok(Value::Function(self.make_function(
                    name, f, env, false, span,
                )))
            }
        }
    }

    fn eval_call(
        &mut self,
        call_span: &Span,
        callee_e: &Expr,
        args_e: &[Expr],
        optional: bool,
        env: EnvRef,
    ) -> Result<Value, FlowError> {
        // phase 9：`super(...)` —— 派生类构造器中的父构造器调用。
        if matches!(&callee_e.node, ExprKind::Super) {
            let args = self.eval_args(args_e, env)?;
            return self.eval_super_call(args);
        }
        // 成员调用 `obj.f()` 时 this 绑定为 obj。
        let (callee, this) = match &callee_e.node {
            ExprKind::Member {
                obj,
                prop,
                computed,
                optional: optc,
            } => {
                // phase 9：`super.m()` —— this 绑定为当前 this，从父原型取方法。
                if matches!(&obj.node, ExprKind::Super) {
                    let key = self.member_key(prop, *computed, env.clone())?;
                    let method = self.get_super_prop(&key)?;
                    let this_val = self
                        .this_stack
                        .last()
                        .cloned()
                        .unwrap_or(Value::Undefined);
                    if let Some(v) =
                        self.try_host_method(&this_val, &key, args_e, env.clone())?
                    {
                        // super 上的宿主方法拦截（极少见，防御性）。
                        return Ok(v);
                    }
                    let args = self.eval_args(args_e, env)?;
                    return self.call_value_at(
                        method,
                        this_val,
                        args,
                        Some(call_span.clone()),
                    );
                }
                let base = self.eval_expr(obj, env.clone())?;
                if *optc && base.is_nullish() {
                    return Ok(Value::Undefined);
                }
                let key = self.member_key(prop, *computed, env.clone())?;
                // 需要解释器参与的方法先拦截：数组高阶函数（回调）、
                // Function.prototype.call/apply（重分发 this）、DOM 方法。
                // 其余走原型链上的 Native（this 经 NativeCtx 传入）。
                if let Some(v) = self.try_host_method(&base, &key, args_e, env.clone())? {
                    return Ok(v);
                }
                (self.get_prop(&base, &key)?, base)
            }
            _ => {
                // Phase 14：直接 `eval(code)` 调用点拦截（需要当前作用域）。
                // 仅当 `eval` 解析到全局 eval 时触发；被用户遮蔽时走普通调用。
                if let ExprKind::Ident(name) = &callee_e.node {
                    if name == "eval" {
                        let is_global_eval = matches!(
                            Env::lookup(&env, "eval"),
                            Ok(Some(Value::Native(f)))
                                if f as *const () == native_eval as *const ()
                        );
                        if is_global_eval {
                            let args = self.eval_args(args_e, env.clone())?;
                            return self.direct_eval(args, env);
                        }
                    }
                }
                (self.eval_expr(callee_e, env.clone())?, Value::Undefined)
            }
        };
        if optional && callee.is_nullish() {
            return Ok(Value::Undefined);
        }
        let args = self.eval_args(args_e, env)?;
        // phase 8：调用点 span 进调用栈帧（错误堆栈用）。
        self.call_value_at(callee, this, args, Some(call_span.clone()))
    }

    /// 拦截需要解释器参与的方法调用。返回 `Some(v)` 表示已处理。
    fn try_host_method(
        &mut self,
        base: &Value,
        key: &str,
        args_e: &[Expr],
        env: EnvRef,
    ) -> Result<Option<Value>, FlowError> {
        // 先做廉价的命中判断（不求值 args），避免无命中时参数被求值两次
        // （eval_call 在返回 None 后会再求值一次）。
        if !self.host_method_maybe(base, key) {
            return Ok(None);
        }
        let args = self.eval_args(args_e, env)?;
        self.try_host_method_vals(base, key, args)
    }

    /// `try_host_method` 的命中预判（不求值）。必须与
    /// `try_host_method_vals` 的分支保持一致，否则会导致参数双求值
    /// 或漏拦截。
    fn host_method_maybe(&self, base: &Value, key: &str) -> bool {
        match base {
            Value::Array(_) => matches!(
                key,
                "map" | "filter" | "forEach" | "find" | "reduce" | "sort" | "some" | "every"
                    | "findIndex" | "flatMap"
            ),
            Value::Function(_) | Value::Native(_) => matches!(key, "call" | "apply"),
            Value::String(_) => key == "replace",
            Value::Generator(_) => matches!(key, "next" | "return" | "throw"),
            Value::AsyncGenerator(_) => matches!(key, "next" | "return" | "throw" | "__yousj$yieldOp"),
            Value::Map(_) | Value::Set(_) => key == "forEach",
            Value::TypedArray(_) => matches!(key, "forEach" | "map" | "filter"),
            Value::Object(o) => {
                // DOM：只有 document（任意方法）/ window（定时器方法）才可能命中；
                // 普通 Object（如 console）直接返回 false，避免参数双求值。
                if let Some(d) = &self.dom {
                    if Rc::ptr_eq(o, &d.document) {
                        return true;
                    }
                    if Rc::ptr_eq(o, &d.window) {
                        return matches!(
                            key,
                            "setTimeout"
                                | "clearTimeout"
                                | "setInterval"
                                | "clearInterval"
                        );
                    }
                }
                // phase 13：标签对象的方法拦截（须与 `try_host_method_vals`
                // 的标签分支保持一致，否则会导致参数双求值）。
                let tag = o.borrow().tag.clone();
                match tag.as_deref() {
                    Some("WebSocket") => matches!(key, "send" | "close"),
                    Some("Worker") => matches!(key, "postMessage" | "terminate"),
                    Some("USP") | Some("FormData") => key == "forEach",
                    _ => false,
                }
            }
            Value::DomNode(_) => matches!(
                key,
                "getAttribute"
                    | "setAttribute"
                    | "appendChild"
                    | "insertBefore"
                    | "removeChild"
                    | "addEventListener"
                    | "click"
            ),
            _ => false,
        }
    }

    /// phase 11：VM 用——宿主方法拦截（参数已求值）。
    /// 由 `try_host_method` 提取，行为一致。
    pub(crate) fn try_host_method_vals(
        &mut self,
        base: &Value,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Option<Value>, FlowError> {
        // 1. 数组高阶函数：回调必须进解释器执行。
        if let Value::Array(arr) = base {
            match key {
                "map" | "filter" | "forEach" | "find" | "reduce" => {
                    return Ok(Some(self.array_higher_order(arr, key, args)?));
                }
                _ => {}
            }
        }
        // 2. Function.prototype.call / apply：重分发 this 与参数。
        if matches!(base, Value::Function(_) | Value::Native(_)) {
            match key {
                "call" | "apply" => {
                    return Ok(Some(self.call_apply(base.clone(), key, args)?));
                }
                _ => {}
            }
        }
        // 3. DOM：document / window / 节点方法。
        if let Some(v) = self.try_dom_method_vals(base, key, args.clone())? {
            return Ok(Some(v));
        }
        // 4. Phase 7：String.prototype.replace —— replacer 为函数时必须进
        //    解释器执行（Native 拿不到解释器）。统一走解释器实现。
        if let Value::String(_) = base {
            if key == "replace" {
                return Ok(Some(self.string_replace_impl(base.clone(), args)?));
            }
        }
        // 5. phase 9：生成器的 next/return/throw 由解释器驱动状态机。
        if let Value::Generator(g) = base {
            match key {
                "next" | "return" | "throw" => {
                    return Ok(Some(self.gen_method(g, key, args)?));
                }
                _ => {}
            }
        }
        // 5b. phase 15：async 生成器的 next/return/throw 返回 Promise；
        // `__yousj$yieldOp` 是 pre-pass 产生的内部调用。
        if let Value::AsyncGenerator(g) = base {
            match key {
                "next" | "return" | "throw" => {
                    return Ok(Some(self.async_gen_method(g, key, args)?));
                }
                "__yousj$yieldOp" => {
                    return Ok(Some(self.async_gen_yield_op(g, args)?));
                }
                _ => {}
            }
        }
        // 6. phase 10：Map/Set/TypedArray 的回调型方法由解释器驱动；
        //    Array 的 sort（带 comparator）/some/every/findIndex/flatMap 同理。
        match base {
            Value::Map(_) => match key {
                "forEach" => {
                    return Ok(Some(self.map_for_each(base.clone(), args)?));
                }
                _ => {}
            },
            Value::Set(_) => match key {
                "forEach" => {
                    return Ok(Some(self.set_for_each(base.clone(), args)?));
                }
                _ => {}
            },
            Value::TypedArray(_) => match key {
                "forEach" | "map" | "filter" => {
                    return Ok(Some(self.ta_higher_order(base.clone(), key, args)?));
                }
                _ => {}
            },
            Value::Array(arr) => match key {
                "sort" => {
                    if args.iter().any(|a| !matches!(a, Value::Undefined)) {
                        return Ok(Some(self.array_sort_with_cmp(arr, args)?));
                    }
                }
                "some" | "every" | "findIndex" | "flatMap" => {
                    return Ok(Some(self.array_higher_order2(arr, key, args)?));
                }
                _ => {}
            },
            _ => {}
        }
        // phase 13：标签对象的方法（WebSocket / Worker / USP / FormData 的
        // forEach）。与 `host_method_maybe` 的标签分支保持一致。
        if let Value::Object(o) = base {
            let tag = o.borrow().tag.clone();
            match tag.as_deref() {
                Some("WebSocket") => match key {
                    "send" | "close" => {
                        return Ok(Some(self.ws_method(o, key, args)?));
                    }
                    _ => {}
                },
                Some("Worker") => match key {
                    "postMessage" | "terminate" => {
                        return Ok(Some(self.worker_method(o, key, args)?));
                    }
                    _ => {}
                },
                Some("USP") => {
                    if key == "forEach" {
                        return Ok(Some(self.usp_for_each(o, args)?));
                    }
                }
                Some("FormData") => {
                    if key == "forEach" {
                        return Ok(Some(self.formdata_for_each(o, args)?));
                    }
                }
                _ => {}
            }
        }
        Ok(None)
    }

    pub(crate) fn call_value(
        &mut self,
        callee: Value,
        this: Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        self.call_value_at(callee, this, args, None)
    }

    /// phase 8：带调用点位置的调用。
    /// `Error("x")` / `TypeError("x")` 等错误构造器是普通 Object
    /// （经 `error_ctors` 按指针识别），调用形式直接构造错误值；
    /// `new` 形式由 `eval_new` 另行拦截。
    pub(crate) fn call_value_at(
        &mut self,
        callee: Value,
        this: Value,
        args: Vec<Value>,
        call_site: Option<Span>,
    ) -> Result<Value, FlowError> {
        // 优化（单轮）：快路径优先——普通函数调用占绝大多数，直接进
        // call_function，跳过下面只针对 Native/Object 的慢速检查
        //（eval 指针比较、错误构造器表遍历、Proxy 构造器克隆+比较、
        // 原型链 "prototype" 查找）。
        if let Value::Function(func) = &callee {
            return self.call_function(func, this, args, call_site);
        }
        if let Value::Native(f) = &callee {
            // Phase 14：间接 `eval(code)`（`(0, eval)(x)`、成员调用、VM 模式等）——
            // 全局作用域求值。直接 `eval(x)` 已在 eval_call 拦截。
            if *f as *const () == native_eval as *const () {
                return self.indirect_eval(args);
            }
            let mut ctx = NativeCtx {
                console: &mut self.console,
                this,
                protos: self.protos.clone(),
                microtasks: &mut self.microtasks,
                fetch_host: self.fetch_host.clone(),
                unhandled: &mut self.unhandled,
                post_target: self.post_target.clone(),
            };
            return f(&mut ctx, args);
        }
        if let Value::Object(o) = &callee {
            if let Some(kind) = self.error_ctor_kind(o) {
                return self.construct_error(kind, args);
            }
            // phase 9：`Proxy()` 直接调用（不带 new）按规范抛 TypeError。
            if let Some(pc) = self.proxy_ctor.clone() {
                if Rc::ptr_eq(o, &pc) {
                    return Err(type_err("Proxy constructor requires 'new'"));
                }
            }
            // phase 10：`String(x)` / `Number(x)` 直接调用 → 类型转换
            // （按构造器的 `.prototype` 指针识别，不破坏 `new` 语义）。
            // phase 14：`Object(x)` / `Array(...)` / `Boolean(x)` 调用形式。
            if let Some(Value::Object(proto)) = o.borrow().get_in_chain("prototype") {
                if Rc::ptr_eq(&proto, &self.protos.string) {
                    let s = args
                        .into_iter()
                        .next()
                        .map(|v| v.to_js_string())
                        .unwrap_or_default();
                    return Ok(Value::String(s));
                }
                if Rc::ptr_eq(&proto, &self.protos.number) {
                    let n = args
                        .into_iter()
                        .next()
                        .map(|v| v.to_number())
                        .unwrap_or(0.0);
                    return Ok(Value::Number(n));
                }
                if Rc::ptr_eq(&proto, &self.protos.object) {
                    return self.construct_object(args);
                }
                if Rc::ptr_eq(&proto, &self.protos.array) {
                    return self.construct_array(args);
                }
            }
        }
        match callee {
            // phase 9：Proxy 调用走 apply trap（无 trap 则透传 target）。
            Value::Proxy(p) => self.proxy_apply(&p, this, args),
            // Phase 7：Promise 的 resolve/reject 函数。
            Value::PromiseSettler(s) => {
                let v = args.into_iter().next().unwrap_or(Value::Undefined);
                let p = &s.promise;
                if s.reject {
                    self.settle_rejected(p, v);
                } else {
                    // resolve：Promise 值则同化（adopt），否则直接 fulfill。
                    if let Value::Promise(inner) = &v {
                        if !Rc::ptr_eq(inner, p) {
                            self.adopt_promise(inner, p);
                        }
                    } else {
                        self.settle_fulfilled(p, v);
                    }
                }
                Ok(Value::Undefined)
            }
            _ => Err(type_err(format!("{} is not a function", callee.to_js_string()))),
        }
    }

    /// phase 8：按指针识别错误构造器对象（`Error`/`TypeError`/…）。
    fn error_ctor_kind(&self, o: &ObjectRef) -> Option<ErrorKind> {
        self.error_ctors
            .iter()
            .find(|(_, ctor)| Rc::ptr_eq(ctor, o))
            .map(|(k, _)| *k)
    }

    /// phase 9：形参绑定 + `arguments`（call_function 与 gen_call 共用）。
    /// 优化（单轮）：批量插入、一次 borrow_mut；有默认值的形参在求值前先把
    /// 已算好的形参落盘——保证默认值表达式能看到前面的形参，语义与原来一致。
    fn bind_params(
        &mut self,
        func: &FuncRef,
        args: Vec<Value>,
        call_env: &EnvRef,
    ) -> Result<(), FlowError> {
        let mut pending: Vec<(IStr, DeclKind, Value)> =
            Vec::with_capacity(func.params.len() + 1);
        for (i, p) in func.params.iter().enumerate() {
            let mut v = args.get(i).cloned().unwrap_or(Value::Undefined);
            if v.is_undefined() {
                if let Some(d) = &p.default {
                    if !pending.is_empty() {
                        Env::bind_call_frame(call_env, std::mem::take(&mut pending));
                    }
                    v = self.eval_expr(d, call_env.clone())?;
                }
            }
            pending.push((istr(&p.name), DeclKind::Let, v));
        }
        // arguments（简化版：普通数组）。
        let arg_arr = Value::Array(Rc::new(RefCell::new(JsArray::new(args))));
        pending.push((istr("arguments"), DeclKind::Var, arg_arr));
        Env::bind_call_frame(call_env, pending);
        Ok(())
    }

    fn call_function(
        &mut self,
        func: &FuncRef,
        this: Value,
        args: Vec<Value>,
        call_site: Option<Span>,
    ) -> Result<Value, FlowError> {
        // phase 9：生成器函数调用不执行体，返回 Generator 对象。
        if func.is_generator && !func.is_async_generator {
            return self.gen_call(func, this, args);
        }
        // phase 15：async 生成器函数调用返回 AsyncGenerator。
        if func.is_async_generator {
            return self.async_gen_call(func, this, args);
        }
        // phase 8：先压调用栈帧 + 严格模式（形参默认值也在被调函数
        // 作用域求值，语义上属于这次调用）。
        let (line, col) = match &call_site {
            Some(s) => (s.start_line, s.start_col),
            None => (func.def_span.start_line, func.def_span.start_col),
        };
        self.call_stack.push(Frame::new(
            func.name.clone().unwrap_or_else(|| "<anonymous>".to_string()),
            line,
            col,
        ));
        self.strict_stack.push(func.strict);
        // phase 9：函数栈（super 查找用）。
        self.func_stack.push(func.clone());

        let call_env = Env::child(&func.closure);
        let params_result: Result<(), FlowError> =
            self.bind_params(func, args, &call_env);
        let result = match params_result {
            Err(e) => Err(e),
            Ok(()) => {
                // 箭头函数不绑定自己的 this（词法捕获）。
                if !func.is_arrow {
                    self.this_stack.push(this);
                }
                // phase 11：VM 开启时函数体走字节码执行（首次调用编译并缓存）。
                let r = if self.vm_enabled {
                    match crate::vm::get_or_compile_chunk(func) {
                        Ok(chunk) => {
                            crate::vm::run_chunk(self, &chunk, call_env.clone(), call_env.clone())
                        }
                        Err(e) => Err(crate::vm::flow_of_compile(e)),
                    }
                } else {
                    match &func.body {
                        FuncBody::Block(stmts) => {
                            match self.exec_block(stmts, call_env.clone(), &call_env) {
                                Ok(Signal::Normal(v)) | Ok(Signal::Return(v)) => Ok(v),
                                Ok(Signal::Break) => Err(syntax_err("break outside of loop")),
                                Ok(Signal::Continue) => Err(syntax_err("continue outside of loop")),
                                Err(e) => Err(e),
                            }
                        }
                        FuncBody::Expr(e) => self.eval_expr(e, call_env.clone()),
                    }
                };
                if !func.is_arrow {
                    self.this_stack.pop();
                }
                r
            }
        };
        // phase 8：恢复现场；运行时错误在最内层调用处回填调用栈
        // （外层调用看到 stack 已有值则不再覆盖）。
        self.strict_stack.pop();
        // phase 9：函数栈出栈。
        self.func_stack.pop();
        // phase 12：调用结束——尝试断开"调用环境 ↔ 以其为闭包的函数"
        // 等 Rc 强环（每调用一次声明的命名函数/类原本泄漏一组对象）。
        // 保守：有外部引用时什么都不做。
        Env::break_scope_cycles(&call_env);
        match result {
            Ok(v) => {
                self.call_stack.pop();
                Ok(v)
            }
            Err(FlowError::Runtime(mut r)) => {
                if r.stack.is_none() {
                    r.stack = Some(self.call_stack.clone());
                }
                self.call_stack.pop();
                Err(FlowError::Runtime(r))
            }
            Err(e) => {
                self.call_stack.pop();
                Err(e)
            }
        }
    }

    fn eval_new(
        &mut self,
        callee_e: &Expr,
        args_e: &[Expr],
        env: EnvRef,
    ) -> Result<Value, FlowError> {
        let c = self.eval_expr(callee_e, env.clone())?;
        let mut args = Vec::with_capacity(args_e.len());
        for a in args_e {
            args.push(self.eval_expr(a, env.clone())?);
        }
        self.eval_new_vals(c, args)
    }

    /// phase 11：VM 用——`new` 的构造分发（callee/args 已求值）。
    /// 由 `eval_new` 提取，行为一致。
    pub(crate) fn eval_new_vals(
        &mut self,
        c: Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        // Phase 7：`new Promise(executor)` / `new RegExp(...)` 拦截
        // （构造器对象是普通 ObjectRef，走解释器逻辑而非 Native）。
        // phase 8：`new Error(...)` / `new TypeError(...)` 等同样拦截。
        if let Value::Object(o) = &c {
            if let Some(pc) = self.promise_ctor.clone() {
                if Rc::ptr_eq(o, &pc) {
                    return self.construct_promise(args);
                }
            }
            if let Some(rc) = self.regexp_ctor.clone() {
                if Rc::ptr_eq(o, &rc) {
                    return self.construct_regexp(args);
                }
            }
            if let Some(kind) = self.error_ctor_kind(o) {
                return self.construct_error(kind, args);
            }
            // phase 9：`new Proxy(t, h)`。
            if let Some(pc) = self.proxy_ctor.clone() {
                if Rc::ptr_eq(o, &pc) {
                    return self.construct_proxy(args);
                }
            }
            // phase 10：集合 / 二进制 / Date / Intl 构造器。
            if let Some(mc) = self.map_ctor.clone()
                && Rc::ptr_eq(o, &mc)
            {
                return self.construct_map(args);
            }
            if let Some(sc) = self.set_ctor.clone()
                && Rc::ptr_eq(o, &sc)
            {
                return self.construct_set(args);
            }
            if let Some(wc) = self.weakmap_ctor.clone()
                && Rc::ptr_eq(o, &wc)
            {
                return self.construct_weakmap(args);
            }
            if let Some(wc) = self.weakset_ctor.clone()
                && Rc::ptr_eq(o, &wc)
            {
                return self.construct_weakset(args);
            }
            if let Some(ac) = self.arraybuffer_ctor.clone()
                && Rc::ptr_eq(o, &ac)
            {
                return self.construct_arraybuffer(args);
            }
            if let Some(dvc) = self.dataview_ctor.clone()
                && Rc::ptr_eq(o, &dvc)
            {
                return self.construct_dataview(args);
            }
            for (kind, tc) in self.typed_array_ctors.clone() {
                if Rc::ptr_eq(o, &tc) {
                    return self.construct_typed_array(kind, args);
                }
            }
            if let Some(dc) = self.date_ctor.clone()
                && Rc::ptr_eq(o, &dc)
            {
                return self.construct_date(args);
            }
            if let Some(nc) = self.intl_numberformat_ctor.clone()
                && Rc::ptr_eq(o, &nc)
            {
                return self.construct_intl_numberformat(args);
            }
            if let Some(dfc) = self.intl_datetimeformat_ctor.clone()
                && Rc::ptr_eq(o, &dfc)
            {
                return self.construct_intl_datetimeformat(args);
            }
            // phase 13：Web API 构造器（WebSocket / Worker / URL /
            // URLSearchParams / TextEncoder / TextDecoder / Blob / FormData）。
            if let Some(ctor) = self.web_ctors.get("WebSocket").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.ws_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("Worker").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.worker_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("URL").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.url_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("URLSearchParams").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.usp_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("TextEncoder").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.te_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("TextDecoder").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.td_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("Blob").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.blob_construct(args);
            }
            if let Some(ctor) = self.web_ctors.get("FormData").cloned()
                && Rc::ptr_eq(o, &ctor)
            {
                return self.fd_construct(args);
            }
            // Phase 14：`new Object/Array/String/Number/Boolean`（这些构造器
            // 是普通 Object，按 `.prototype` 指针识别；用户类是 Function，
            // 不会误命中）。放在具体拦截之后，避免覆盖。
            if let Some(Value::Object(proto)) = o.borrow().get_in_chain("prototype") {
                if Rc::ptr_eq(&proto, &self.protos.object) {
                    return self.construct_object(args);
                }
                if Rc::ptr_eq(&proto, &self.protos.array) {
                    return self.construct_array(args);
                }
                if Rc::ptr_eq(&proto, &self.protos.string) {
                    return self.construct_string_object(args);
                }
                if Rc::ptr_eq(&proto, &self.protos.number) {
                    return self.construct_number_object(args);
                }
            }
        }
        // Phase 14：`new Boolean(x)`（Boolean 全局是 Native，按函数指针识别）。
        if let Value::Native(f) = &c {
            if *f as *const () == native_global_boolean_call as *const () {
                return self.construct_boolean_object(args);
            }
        }
        match c {
            Value::Function(f) => {
                // phase 9：生成器不是构造器；类走 construct_class。
                if f.is_generator {
                    return Err(type_err("generator function is not a constructor"));
                }
                if f.is_class {
                    return self.construct_class(&f, args);
                }
                let obj = Rc::new(RefCell::new(JsObject::new()));
                // 实例的原型 = 构造器的 `.prototype` 对象。
                obj.borrow_mut().proto = f.prototype.clone();
                obj.borrow_mut().tag = f.name.clone();
                let r = self.call_function(&f, Value::Object(obj.clone()), args, None)?;
                // 构造器返回对象则采用，否则用新造的 this。
                match r {
                    Value::Object(_) => Ok(r),
                    _ => Ok(Value::Object(obj)),
                }
            }
            // phase 9：`new (new Proxy(...))` 等——Proxy 值可构造走 construct trap。
            Value::Proxy(p) => self.proxy_construct(&p, args),
            _ => Err(type_err(format!(
                "{} is not a constructor",
                c.to_js_string()
            ))),
        }
    }

    /// Phase 14：`new Object(x)`。`undefined`/`null` → 空对象；
    /// 原始值 → 包装对象（内部 `primitive` 槽）；对象 → 原样返回。
    fn construct_object(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let v = args.into_iter().next().unwrap_or(Value::Undefined);
        match v {
            Value::Undefined | Value::Null => {
                Ok(Value::Object(Rc::new(RefCell::new(JsObject::with_proto(
                    Some(self.protos.object.clone()),
                )))))
            }
            Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                Ok(self.wrap_primitive(v))
            }
            _ => Ok(v),
        }
    }

    /// Phase 14：原始值包装对象（`new String/Number/Boolean(x)`、
    /// `Object(原始值)`）。原型按原始值类型挂，`primitive` 槽存原值。
    fn wrap_primitive(&self, v: Value) -> Value {
        let (proto, tag) = match &v {
            Value::String(_) => (self.protos.string.clone(), "String"),
            Value::Number(_) => (self.protos.number.clone(), "Number"),
            // Boolean 保持 Native 全局（`typeof Boolean === "function"` 不回归），
            // 包装对象原型暂挂 Object.prototype。
            Value::Bool(_) => (self.protos.object.clone(), "Boolean"),
            _ => (self.protos.object.clone(), "Object"),
        };
        let mut o = JsObject::with_proto(Some(proto));
        o.tag = Some(tag.to_string());
        o.primitive = Some(v);
        Value::Object(Rc::new(RefCell::new(o)))
    }

    /// Phase 14：`new Array(...)`。单数字参数 → 定长数组；
    /// 否则 → 逐项数组。`Array(...)` 调用形式语义相同。
    fn construct_array(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        // Phase 14：`new Array(huge)` 不实际分配（稀疏），避免 OOM abort。
        const SPARSE_THRESHOLD: usize = 1_000_000;
        if args.len() == 1 {
            if let Value::Number(n) = &args[0] {
                let len = *n as usize;
                if (*n as f64) != (len as f64) || n.is_nan() {
                    return Err(crate::value::range_err(format!(
                        "invalid array length: {n}"
                    )));
                }
                if len > SPARSE_THRESHOLD {
                    return Ok(Value::Array(Rc::new(RefCell::new(
                        JsArray::with_sparse_len(len, Some(self.protos.array.clone())),
                    ))));
                }
                let elems = vec![Value::Undefined; len];
                return Ok(Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                    elems,
                    Some(self.protos.array.clone()),
                )))));
            }
        }
        let elems = if args.len() == 1 { args } else { args };
        Ok(Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
            elems,
            Some(self.protos.array.clone()),
        )))))
    }

    /// Phase 14：`new String(x)` → String 包装对象。
    fn construct_string_object(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let v = args
            .into_iter()
            .next()
            .map(|x| Value::String(x.to_js_string()))
            .unwrap_or(Value::String(String::new()));
        Ok(self.wrap_primitive(v))
    }

    /// Phase 14：`new Number(x)` → Number 包装对象。
    fn construct_number_object(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let v = args
            .into_iter()
            .next()
            .map(|x| Value::Number(x.to_number()))
            .unwrap_or(Value::Number(0.0));
        Ok(self.wrap_primitive(v))
    }

    /// Phase 14：`new Boolean(x)` → Boolean 包装对象。
    fn construct_boolean_object(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let v = args
            .into_iter()
            .next()
            .map(|x| Value::Bool(x.to_boolean()))
            .unwrap_or(Value::Bool(false));
        Ok(self.wrap_primitive(v))
    }

    /// Phase 14：检查表达式是否包含 `super`（直接 eval 在字段初始化器
    /// 中的早期错误用；规范要求含 SuperCall 的直接 eval 抛 SyntaxError）。
    fn expr_contains_super(e: &Expr) -> bool {
        match &e.node {
            ExprKind::Super => true,
            ExprKind::Unary { arg, .. } | ExprKind::Await(arg) => {
                Self::expr_contains_super(arg)
            }
            ExprKind::Update { arg, .. } => Self::expr_contains_super(arg),
            ExprKind::Binary { left, right, .. }
            | ExprKind::Logical { left, right, .. }
            | ExprKind::Assign { left, right, .. } => {
                Self::expr_contains_super(left) || Self::expr_contains_super(right)
            }
            ExprKind::Conditional { test, cons, alt } => {
                Self::expr_contains_super(test)
                    || Self::expr_contains_super(cons)
                    || Self::expr_contains_super(alt)
            }
            ExprKind::Call { callee, args, .. } => {
                Self::expr_contains_super(callee)
                    || args.iter().any(Self::expr_contains_super)
            }
            ExprKind::New { callee, args } => {
                Self::expr_contains_super(callee) || args.iter().any(Self::expr_contains_super)
            }
            ExprKind::Member { obj, prop, .. } => {
                Self::expr_contains_super(obj) || Self::expr_contains_super(prop)
            }
            ExprKind::Sequence(es) => es.iter().any(Self::expr_contains_super),
            ExprKind::Yield { arg, .. } => {
                arg.as_ref().map_or(false, |a| Self::expr_contains_super(a))
            }
            ExprKind::Array(elems) => elems.iter().any(|el| match el {
                ArrayElem::Expr(x) | ArrayElem::Spread(x) => Self::expr_contains_super(x),
                ArrayElem::Hole => false,
            }),
            // 规范 Contains 语义穿透函数/类边界（箭头里的 super 也算）。
            ExprKind::Function(f) => {
                f.params.iter().any(|p| {
                    p.default.as_ref().map_or(false, Self::expr_contains_super)
                }) || f.body.iter().any(Self::stmt_contains_super)
            }
            ExprKind::ArrowFunction(f) => {
                f.params.iter().any(|p| {
                    p.default.as_ref().map_or(false, Self::expr_contains_super)
                }) || match &f.body {
                    ArrowBody::Expr(e) => Self::expr_contains_super(e),
                    ArrowBody::Block(ss) => ss.iter().any(Self::stmt_contains_super),
                }
            }
            ExprKind::Class(c) => {
                c.super_class.as_ref().map_or(false, |e| Self::expr_contains_super(e))
                    || c.body.iter().any(|el| match el {
                        ClassElem::Method { func, .. } => {
                            func.params.iter().any(|p| {
                                p.default.as_ref().map_or(false, Self::expr_contains_super)
                            }) || func.body.iter().any(Self::stmt_contains_super)
                        }
                        ClassElem::Field { init, .. } => {
                            init.as_ref().map_or(false, Self::expr_contains_super)
                        }
                        ClassElem::StaticBlock(ss) => ss.iter().any(Self::stmt_contains_super),
                    })
            }
            ExprKind::Literal(_)
            | ExprKind::Ident(_)
            | ExprKind::This
            | ExprKind::PrivateName(_) => false,
            ExprKind::Object(props) => props.iter().any(|p| {
                // 方法/访问器体是新作用域，内部 super 不算；计算属性名属于外层。
                let key_super = matches!(&p.key, PropKey::Computed(k) if Self::expr_contains_super(k));
                let val_super = match &p.value {
                    PropValue::Init(v) => Self::expr_contains_super(v),
                    _ => false,
                };
                key_super || val_super
            }),
            ExprKind::PrivateMember { obj, .. } => Self::expr_contains_super(obj),
        }
    }

    /// Phase 14：检查语句是否包含 `super`（直接 eval 早期错误用）。
    fn stmt_contains_super(s: &Stmt) -> bool {
        match &s.node {
            StmtKind::Expr(e) => Self::expr_contains_super(e),
            StmtKind::Block(ss) => ss.iter().any(Self::stmt_contains_super),
            StmtKind::VarDecl { decls, .. } => decls.iter().any(|d| {
                d.init.as_ref().map_or(false, Self::expr_contains_super)
            }),
            // 规范 Contains 穿透函数/类边界。
            StmtKind::FunctionDecl(f) => {
                f.params.iter().any(|p| {
                    p.default.as_ref().map_or(false, Self::expr_contains_super)
                }) || f.body.iter().any(Self::stmt_contains_super)
            }
            StmtKind::ClassDecl(c) => {
                c.super_class.as_ref().map_or(false, |e| Self::expr_contains_super(e))
                    || c.body.iter().any(|el| match el {
                        ClassElem::Method { func, .. } => {
                            func.params.iter().any(|p| {
                                p.default.as_ref().map_or(false, Self::expr_contains_super)
                            }) || func.body.iter().any(Self::stmt_contains_super)
                        }
                        ClassElem::Field { init, .. } => {
                            init.as_ref().map_or(false, Self::expr_contains_super)
                        }
                        ClassElem::StaticBlock(ss) => ss.iter().any(Self::stmt_contains_super),
                    })
            }
            StmtKind::If { test, cons, alt } => {
                Self::expr_contains_super(test)
                    || Self::stmt_contains_super(cons)
                    || alt.as_ref().map_or(false, |a| Self::stmt_contains_super(a))
            }
            StmtKind::While { test, body } | StmtKind::DoWhile { test, body } => {
                Self::expr_contains_super(test) || Self::stmt_contains_super(body)
            }
            StmtKind::For { init, test, update, body } => {
                init.as_ref().map_or(false, |i| match i {
                    ForInit::Expr(e) => Self::expr_contains_super(e),
                    ForInit::VarDecl { decls, .. } => decls
                        .iter()
                        .any(|d| d.init.as_ref().map_or(false, Self::expr_contains_super)),
                }) || test.as_ref().map_or(false, Self::expr_contains_super)
                    || update.as_ref().map_or(false, Self::expr_contains_super)
                    || Self::stmt_contains_super(body)
            }
            StmtKind::ForInOf { right, body, .. } => {
                Self::expr_contains_super(right) || Self::stmt_contains_super(body)
            }
            StmtKind::Return(e) => e.as_ref().map_or(false, Self::expr_contains_super),
            StmtKind::Throw(e) => Self::expr_contains_super(e),
            StmtKind::Try { block, handler, finalizer } => {
                block.iter().any(Self::stmt_contains_super)
                    || handler.as_ref().map_or(false, |c| {
                        c.body.iter().any(Self::stmt_contains_super)
                    })
                    || finalizer.as_ref().map_or(false, |f| {
                        f.iter().any(Self::stmt_contains_super)
                    })
            }
            StmtKind::Switch { disc, cases } => {
                Self::expr_contains_super(disc)
                    || cases.iter().any(|c| {
                        c.test.as_ref().map_or(false, Self::expr_contains_super)
                            || c.body.iter().any(Self::stmt_contains_super)
                    })
            }
            // 类/函数声明边界：内部 super 属于新作用域。
            _ => false,
        }
    }

    /// Phase 14：直接 `eval(code)` —— 在当前调用者作用域求值。
    /// 严格 eval 代码（或严格调用者）用新子作用域；sloppy 下 `var` 声明
    /// 落到当前环境（子集简化：不精确区分函数/块级 var 环境）。
    fn direct_eval(
        &mut self,
        args: Vec<Value>,
        env: EnvRef,
    ) -> Result<Value, FlowError> {
        let code = args
            .into_iter()
            .next()
            .map(|v| v.to_js_string())
            .unwrap_or_default();
        let prog = crate::parser::parse_source(&code)
            .map_err(|e| syntax_err(format!("eval parse error: {e:?}")))?;
        // Phase 14：字段初始化器里的直接 eval 含 super → SyntaxError
        // （规范早期错误；否则经 eval_super_call 无限重入导致栈溢出）。
        if self.field_init_depth > 0
            && prog.body.iter().any(|s| Self::stmt_contains_super(s))
        {
            return Err(syntax_err(
                "super in direct eval inside field initializer",
            ));
        }
        let (eenv, venv) = if prog.strict || self.is_strict() {
            let e = Env::child(&env);
            (e.clone(), e)
        } else {
            (env.clone(), env.clone())
        };
        match self.exec_block(&prog.body, eenv, &venv)? {
            Signal::Normal(v) | Signal::Return(v) => Ok(v),
            Signal::Break => Err(syntax_err("break outside of loop")),
            Signal::Continue => Err(syntax_err("continue outside of loop")),
        }
    }

    /// Phase 14：间接 `eval(code)`（`(0, eval)(x)` 等）—— 全局作用域求值。
    fn indirect_eval(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let code = args
            .into_iter()
            .next()
            .map(|v| v.to_js_string())
            .unwrap_or_default();
        let prog = crate::parser::parse_source(&code)
            .map_err(|e| syntax_err(format!("eval parse error: {e:?}")))?;
        // Phase 14：字段初始化器触发的间接 eval 含 super → SyntaxError
        // （同直接 eval；否则经 eval_super_call 无限重入导致栈溢出）。
        if self.field_init_depth > 0
            && prog.body.iter().any(|s| Self::stmt_contains_super(s))
        {
            return Err(syntax_err(
                "super in indirect eval inside field initializer",
            ));
        }
        let g = self.global.clone();
        let (eenv, venv) = if prog.strict {
            let e = Env::child(&g);
            (e.clone(), e)
        } else {
            (g.clone(), g.clone())
        };
        match self.exec_block(&prog.body, eenv, &venv)? {
            Signal::Normal(v) | Signal::Return(v) => Ok(v),
            Signal::Break => Err(syntax_err("break outside of loop")),
            Signal::Continue => Err(syntax_err("continue outside of loop")),
        }
    }

    /// `new Promise(executor)`：同步执行 executor(resolve, reject)，
    /// executor 抛错 → promise 直接 rejected。
    fn construct_promise(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let executor = args.into_iter().next().unwrap_or(Value::Undefined);
        if !executor.is_callable() {
            return Err(type_err("Promise executor must be callable"));
        }
        let p = JsPromise::pending();
        let resolve = Value::PromiseSettler(PromiseSettler::resolve(p.clone()));
        let reject = Value::PromiseSettler(PromiseSettler::reject(p.clone()));
        match self.call_value(executor, Value::Undefined, vec![resolve, reject]) {
            Ok(_) => {}
            Err(e) => {
                // executor 同步抛错 → 直接 reject（规范行为）；
                // settle_rejected 内部已处理 unhandled 记账。
                let rv = self.flow_error_to_value(e);
                self.settle_rejected(&p, rv);
            }
        }
        Ok(Value::Promise(p))
    }

    /// `new RegExp(pattern, flags)`：编译正则（失败抛运行时错误）。
    fn construct_regexp(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let (pattern, flags) = match args.first() {
            Some(Value::RegExp(r)) if args.len() == 1 => {
                let b = r.borrow();
                (b.compiled.source.clone(), b.compiled.flags.as_string())
            }
            _ => {
                let pattern = args
                    .first()
                    .map(|v| v.to_js_string())
                    .unwrap_or_default();
                let flags = args
                    .get(1)
                    .map(|v| v.to_js_string())
                    .unwrap_or_default();
                (pattern, flags)
            }
        };
        match CompiledRegex::compile(&pattern, &flags) {
            Ok(c) => Ok(Value::RegExp(JsRegExp::new(c))),
            Err(e) => Err(syntax_err(format!("invalid regex: {e}"))),
        }
    }

    // ------------------------------------------------------------------
    // 二元 / 复合赋值
    // ------------------------------------------------------------------

    /// Phase 14：ToPrimitive — 对象转原始值时调用用户定义的
    /// `valueOf`/`toString`（规范语义）。hint 为 "string" | "number" | "default"。
    /// Date 的 default hint 为 string；其余为 number。
    pub(crate) fn to_primitive(
        &mut self,
        v: Value,
        hint: &str,
    ) -> Result<Value, FlowError> {
        // 原始值直接返回（快路径）。
        if matches!(
            &v,
            Value::Undefined
                | Value::Null
                | Value::Bool(_)
                | Value::Number(_)
                | Value::String(_)
        ) {
            return Ok(v);
        }
        // Date 的 default hint 是 string。
        let hint = if hint == "default" && self.is_date_value(&v) {
            "string"
        } else {
            hint
        };
        let order: [&str; 2] = if hint == "string" {
            ["toString", "valueOf"]
        } else {
            ["valueOf", "toString"]
        };
        for name in order {
            let m = self.get_prop(&v, name)?;
            if Self::is_callable_value(&m) {
                let r = self.call_value_at(m, v.clone(), vec![], None)?;
                if matches!(
                    &r,
                    Value::Undefined
                        | Value::Null
                        | Value::Bool(_)
                        | Value::Number(_)
                        | Value::String(_)
                ) {
                    return Ok(r);
                }
            }
        }
        Err(type_err("Cannot convert object to primitive value"))
    }

    /// 是否为 Date 对象（ToPrimitive hint 判定用）。
    fn is_date_value(&self, v: &Value) -> bool {
        matches!(v, Value::Date(_))
    }

    /// 值是否可调用（ToPrimitive 方法查找用）。
    fn is_callable_value(v: &Value) -> bool {
        matches!(
            v,
            Value::Function(_) | Value::Native(_) | Value::Proxy(_)
        )
    }

    pub(crate) fn apply_binary(&mut self, op: BinaryOp, l: Value, r: Value) -> Result<Value, FlowError> {
        use BinaryOp::*;
        let num = |v: &Value| v.to_number();
        match op {
            Add => {
                // Phase 14：规范 ToPrimitive(default)；任一是字符串则拼接。
                let lprim = self.to_primitive(l, "default")?;
                let rprim = self.to_primitive(r, "default")?;
                Ok(
                    if matches!(&lprim, Value::String(_))
                        || matches!(&rprim, Value::String(_))
                    {
                        Value::String(lprim.to_js_string() + &rprim.to_js_string())
                    } else {
                        Value::Number(lprim.to_number() + rprim.to_number())
                    },
                )
            }
            Sub => Ok(Value::Number(num(&l) - num(&r))),
            Mul => Ok(Value::Number(num(&l) * num(&r))),
            Div => Ok(Value::Number(num(&l) / num(&r))),
            Mod => Ok(Value::Number(num(&l) % num(&r))),
            Pow => Ok(Value::Number(num(&l).powf(num(&r)))),
            Shl => Ok(Value::Number((l.to_int32() << (r.to_uint32() & 31)) as f64)),
            Shr => Ok(Value::Number((l.to_int32() >> (r.to_uint32() & 31)) as f64)),
            UShr => Ok(Value::Number((l.to_uint32() >> (r.to_uint32() & 31)) as f64)),
            BitAnd => Ok(Value::Number((l.to_int32() & r.to_int32()) as f64)),
            BitOr => Ok(Value::Number((l.to_int32() | r.to_int32()) as f64)),
            BitXor => Ok(Value::Number((l.to_int32() ^ r.to_int32()) as f64)),
            Lt => Ok(Value::Bool(compare_lt(&l, &r))),
            Le => Ok(Value::Bool(!compare_lt(&r, &l))),
            Gt => Ok(Value::Bool(compare_lt(&r, &l))),
            Ge => Ok(Value::Bool(!compare_lt(&l, &r))),
            Eq => Ok(Value::Bool(l.loose_eq(&r))),
            Ne => Ok(Value::Bool(!l.loose_eq(&r))),
            StrictEq => Ok(Value::Bool(l.strict_eq(&r))),
            StrictNe => Ok(Value::Bool(!l.strict_eq(&r))),
            In => {
                let key = l.to_js_string();
                // phase 9：Proxy 走 has trap；其余走原型链存在性检查。
                match &r {
                    Value::Proxy(p) => Ok(Value::Bool(self.proxy_has(p, &key)?)),
                    Value::Object(_)
                    | Value::Array(_)
                    | Value::String(_)
                    | Value::Function(_)
                    | Value::Native(_) => Ok(Value::Bool(self.prop_exists(&r, &key))),
                    _ => Err(type_err(format!(
                        "right-hand side of 'in' must be an object"
                    ))),
                }
            }
            Instanceof => {
                // 原型链判定：沿 lhs 的原型链找 rhs 的 `.prototype`。
                Ok(Value::Bool(instance_of(&l, &r, &self.protos)))
            }
        }
    }

    pub(crate) fn apply_compound(
        &mut self,
        op: AssignOp,
        cur: Value,
        r: Value,
    ) -> Result<Value, FlowError> {
        use AssignOp::*;
        let bin = match op {
            AddAssign => BinaryOp::Add,
            SubAssign => BinaryOp::Sub,
            MulAssign => BinaryOp::Mul,
            DivAssign => BinaryOp::Div,
            ModAssign => BinaryOp::Mod,
            PowAssign => BinaryOp::Pow,
            ShlAssign => BinaryOp::Shl,
            ShrAssign => BinaryOp::Shr,
            UShrAssign => BinaryOp::UShr,
            BitAndAssign => BinaryOp::BitAnd,
            BitOrAssign => BinaryOp::BitOr,
            BitXorAssign => BinaryOp::BitXor,
            _ => return Err(rt("unsupported assignment operator")),
        };
        self.apply_binary(bin, cur, r)
    }

    // ------------------------------------------------------------------
    // Phase 9：生成器 / Proxy / 类
    // ------------------------------------------------------------------

    // ---------------- 生成器 ----------------

    /// 调用生成器函数：执行 setup 体拿到 `__yousj$step` 闭包，包装成 Generator。
    /// 函数体不直接执行（挂起在 suspended-start）。
    fn gen_call(
        &mut self,
        func: &FuncRef,
        this: Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let call_env = Env::child(&func.closure);
        self.bind_params(func, args, &call_env)?;
        // setup 体求值：声明变量、创建 step 闭包、返回它。
        self.this_stack.push(this.clone());
        let r = match &func.body {
            FuncBody::Block(stmts) => self.exec_block(stmts, call_env.clone(), &call_env),
            FuncBody::Expr(e) => self.eval_expr(e, call_env).map(Signal::Normal),
        };
        self.this_stack.pop();
        let step_val = match r {
            Ok(Signal::Return(v)) | Ok(Signal::Normal(v)) => v,
            Ok(Signal::Break) => return Err(syntax_err("break outside of loop")),
            Ok(Signal::Continue) => return Err(syntax_err("continue outside of loop")),
            Err(e) => return Err(e),
        };
        match step_val {
            Value::Function(step) => Ok(Value::Generator(Rc::new(RefCell::new(
                JsGenerator::new(step, this),
            )))),
            _ => Err(rt("internal: generator setup did not return step closure")),
        }
    }

    /// `{value, done}` 结果对象。
    fn make_result_obj(&mut self, value: Value, done: bool) -> Value {
        let o = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        o.borrow_mut().set("value", value);
        o.borrow_mut().set("done", Value::Bool(done));
        Value::Object(o)
    }

    /// 从 step 返回值读出 (value, done)。
    fn read_result_obj(&self, v: &Value) -> Result<(Value, bool), FlowError> {
        match v {
            Value::Object(o) => {
                let b = o.borrow();
                let value = b.get("value").unwrap_or(Value::Undefined);
                let done = b.get("done").map(|d| d.to_boolean()).unwrap_or(false);
                Ok((value, done))
            }
            _ => Err(rt("internal: generator step did not return {value, done}")),
        }
    }

    /// 驱动 step 闭包一次：`step(sv, ab, abv)`。
    /// ab: 0=next, 1=throw 注入, 2=return 注入（见 gen_desugar）。
    fn gen_drive(
        &mut self,
        g: &GenRef,
        sv: Value,
        ab: f64,
        abv: Value,
    ) -> Result<Value, FlowError> {
        let (step, this_value) = {
            let b = g.borrow();
            (b.step.clone(), b.this_value.clone())
        };
        // step 是普通闭包：this 经参数传入（call_function 会压 this_stack）。
        let r = self.call_value(
            Value::Function(step),
            this_value,
            vec![sv, Value::Number(ab), abv],
        );
        match r {
            Ok(v) => {
                let (_, done) = self.read_result_obj(&v)?;
                let mut b = g.borrow_mut();
                b.started = true;
                if done {
                    b.done = true;
                }
                Ok(v)
            }
            Err(e) => {
                // step 内抛错（用户 throw / throw 注入未被捕获）→ 生成器结束。
                g.borrow_mut().done = true;
                Err(e)
            }
        }
    }

    /// `gen.next(v)` / `gen.return(v)` / `gen.throw(e)`。
    fn gen_method(
        &mut self,
        g: &GenRef,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let arg = args.into_iter().next().unwrap_or(Value::Undefined);
        let done = g.borrow().done;
        if done {
            return match method {
                "next" => Ok(self.make_result_obj(Value::Undefined, true)),
                "return" => Ok(self.make_result_obj(arg, true)),
                "throw" => Err(FlowError::Thrown(arg)),
                _ => Err(rt("internal: bad generator method")),
            };
        }
        let started = g.borrow().started;
        match method {
            "next" => {
                // 规范：suspended-start 时 next(v) 的 v 被忽略。
                let sv = if started { arg } else { Value::Undefined };
                self.gen_drive(g, sv, 0.0, Value::Undefined)
            }
            "return" => {
                if !started {
                    g.borrow_mut().done = true;
                    return Ok(self.make_result_obj(arg, true));
                }
                self.gen_drive(g, Value::Undefined, 2.0, arg)
            }
            "throw" => {
                if !started {
                    g.borrow_mut().done = true;
                    return Err(FlowError::Thrown(arg));
                }
                self.gen_drive(g, Value::Undefined, 1.0, arg)
            }
            _ => Err(rt("internal: bad generator method")),
        }
    }

    /// 耗尽生成器为 Vec（`[...gen()]` / `for-of` 用）。
    fn gen_drain(&mut self, g: &GenRef) -> Result<Vec<Value>, FlowError> {
        let mut out = Vec::new();
        loop {
            let r = self.gen_drive(g, Value::Undefined, 0.0, Value::Undefined)?;
            let (value, done) = self.read_result_obj(&r)?;
            if done {
                break;
            }
            out.push(value);
        }
        Ok(out)
    }

    // ============================================================
    // phase 15：async 生成器。
    // ============================================================

    /// `async function*` 调用 → 返回 AsyncGenerator（惰性，体在首次 next() 时启动）。
    fn async_gen_call(
        &mut self,
        func: &FuncRef,
        this: Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let g = Rc::new(RefCell::new(JsAsyncGenerator::new(
            func.clone(),
            this,
            args,
        )));
        Ok(Value::AsyncGenerator(g))
    }

    /// 启动 async 生成器体（首次 next() 时调用）。
    /// 体是 desugar 后的 async 状态机；执行返回 P_body（Promise），挂载完成反应。
    fn async_gen_start(&mut self, g: &AsyncGenRef) -> Result<(), FlowError> {
        let (func, this_value, args) = {
            let b = g.borrow();
            (
                b.body_func.clone(),
                b.this_value.clone(),
                b.args.clone(),
            )
        };
        // 调用栈设置（仿 call_function 的前半）。
        let (line, col) = (
            func.def_span.start_line,
            func.def_span.start_col,
        );
        self.call_stack.push(Frame::new(
            func.name.clone().unwrap_or_else(|| "<anonymous>".to_string()),
            line,
            col,
        ));
        self.strict_stack.push(func.strict);
        self.func_stack.push(func.clone());

        let call_env = Env::child(&func.closure);
        let params_result = self.bind_params(&func, args, &call_env);
        if let Err(e) = params_result {
            self.func_stack.pop();
            self.strict_stack.pop();
            self.call_stack.pop();
            let err_val = self.flow_error_to_value(e);
            self.async_gen_complete(g, err_val, true);
            return Ok(());
        }
        // 绑定 `__yousj$AG`（pre-pass 产生的 yieldOp 调用目标）。
        Env::declare_var(&call_env, "__yousj$AG");
        Env::assign_force(&call_env, "__yousj$AG", Value::AsyncGenerator(g.clone()));
        if !func.is_arrow {
            self.this_stack.push(this_value);
        }
        let result = match &func.body {
            FuncBody::Block(stmts) => self.exec_block(stmts, call_env.clone(), &call_env),
            FuncBody::Expr(e) => self.eval_expr(e, call_env).map(Signal::Normal),
        };
        if !func.is_arrow {
            self.this_stack.pop();
        }
        self.func_stack.pop();
        self.strict_stack.pop();
        self.call_stack.pop();

        let p_body_val = match result {
            Ok(Signal::Return(v)) | Ok(Signal::Normal(v)) => v,
            Ok(Signal::Break) => return Err(syntax_err("break outside of loop")),
            Ok(Signal::Continue) => return Err(syntax_err("continue outside of loop")),
            Err(e) => {
                let err_val = self.flow_error_to_value(e);
                self.async_gen_complete(g, err_val, true);
                return Ok(());
            }
        };
        // 挂载完成反应：P_body 落定 → async_gen_complete。
        if let Value::Promise(p) = p_body_val {
            let reaction = Reaction {
                kind: ReactionKind::AsyncGenDone { agen: g.clone() },
                next: None,
                source: p.clone(),
            };
            attach_reaction(&p, reaction, &mut self.microtasks);
        } else {
            // 非 Promise（理论上不应发生）→ 直接完成。
            self.async_gen_complete(g, p_body_val, false);
        }
        Ok(())
    }

    /// 体完成（P_body 落定）：标记 done，结算所有挂起请求。
    fn async_gen_complete(&mut self, g: &AsyncGenRef, value: Value, rejected: bool) {
        let requests = {
            let mut b = g.borrow_mut();
            if b.done {
                return;
            }
            b.done = true;
            b.parked = None;
            std::mem::take(&mut b.queue)
        };
        for req in requests {
            if rejected {
                // 体抛错：所有挂起请求都 reject。
                let _ = self.call_value(req.reject, Value::Undefined, vec![value.clone()]);
            } else {
                match req.kind {
                    0 | 2 => {
                        // next/return：{value, done: true}。
                        let result = self.make_result_obj(value.clone(), true);
                        let _ =
                            self.call_value(req.resolve, Value::Undefined, vec![result]);
                    }
                    _ => {
                        // throw：reject。
                        let _ = self.call_value(
                            req.reject,
                            Value::Undefined,
                            vec![req.arg.clone()],
                        );
                    }
                }
            }
        }
    }

    /// `__yousj$AG.__yousj$yieldOp(v)`：体执行中遇到 `yield v` 时调用。
    /// 返回 park Promise（体 await 它而挂起）；同步解决最老的 next 请求。
    fn async_gen_yield_op(
        &mut self,
        g: &AsyncGenRef,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let v = args.into_iter().next().unwrap_or(Value::Undefined);
        // 先检查是否有挂起的 return/throw（优先处理）。
        let abrupt = {
            let mut b = g.borrow_mut();
            let idx = b.queue.iter().position(|r| r.kind == 1 || r.kind == 2);
            idx.map(|i| b.queue.remove(i))
        };
        if let Some(req) = abrupt {
            // 有 return/throw 请求：放弃体，直接结算。
            let mut b = g.borrow_mut();
            b.done = true;
            b.parked = None;
            // 剩余的 next 请求也按完成处理。
            let rest = std::mem::take(&mut b.queue);
            drop(b);
            match req.kind {
                2 => {
                    let result = self.make_result_obj(req.arg.clone(), true);
                    let _ = self.call_value(req.resolve, Value::Undefined, vec![result]);
                }
                _ => {
                    let _ =
                        self.call_value(req.reject, Value::Undefined, vec![req.arg.clone()]);
                }
            }
            for r in rest {
                if r.kind == 0 {
                    let result = self.make_result_obj(Value::Undefined, true);
                    let _ = self.call_value(r.resolve, Value::Undefined, vec![result]);
                }
            }
            // 返回永不解决的 Promise（体被抛弃）。
            let park = JsPromise::pending();
            return Ok(Value::Promise(park));
        }
        // 正常 yield：取出最老的 next 请求并解决。
        let req = {
            let mut b = g.borrow_mut();
            let idx = b.queue.iter().position(|r| r.kind == 0);
            idx.map(|i| b.queue.remove(i))
        };
        if let Some(req) = req {
            let result = self.make_result_obj(v, false);
            let _ = self.call_value(req.resolve, Value::Undefined, vec![result]);
        }
        // 如果队列里还有请求（提前调用的 next），不 park，直接用下一个请求的
        // arg 作为 send 值继续（返回已解决的 Promise）。
        let next_send = {
            let b = g.borrow();
            b.queue.iter().find(|r| r.kind == 0).map(|r| r.arg.clone())
        };
        if let Some(send) = next_send {
            let park = JsPromise::pending();
            self.settle_fulfilled(&park, send);
            return Ok(Value::Promise(park));
        }
        // 创建 park Promise 并存储 resolver。
        let park = JsPromise::pending();
        let park_resolve = Value::PromiseSettler(PromiseSettler::resolve(park.clone()));
        g.borrow_mut().parked = Some(park_resolve);
        Ok(Value::Promise(park))
    }

    /// 从 park 中恢复（next() 在已挂起时调用）。
    fn async_gen_resume(&mut self, g: &AsyncGenRef, send: Value) -> Result<(), FlowError> {
        let park_resolve = {
            let mut b = g.borrow_mut();
            b.parked.take()
        };
        if let Some(resolve) = park_resolve {
            let _ = self.call_value(resolve, Value::Undefined, vec![send]);
        }
        Ok(())
    }

    /// `gen.next(v)` / `gen.return(v)` / `gen.throw(e)` → 返回 Promise。
    fn async_gen_method(
        &mut self,
        g: &AsyncGenRef,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let arg = args.into_iter().next().unwrap_or(Value::Undefined);
        let kind: u8 = match method {
            "next" => 0,
            "throw" => 1,
            "return" => 2,
            _ => return Err(rt("internal: bad async generator method")),
        };
        // 创建返回的 Promise。
        let p = JsPromise::pending();
        let resolve = Value::PromiseSettler(PromiseSettler::resolve(p.clone()));
        let reject = Value::PromiseSettler(PromiseSettler::reject(p.clone()));

        // 已完成：直接结算。
        if g.borrow().done {
            match kind {
                0 => {
                    let result = self.make_result_obj(Value::Undefined, true);
                    self.settle_fulfilled(&p, result);
                }
                2 => {
                    let result = self.make_result_obj(arg, true);
                    self.settle_fulfilled(&p, result);
                }
                _ => {
                    self.settle_rejected(&p, arg);
                }
            }
            return Ok(Value::Promise(p));
        }

        // 入队。
        g.borrow_mut().queue.push(AsyncGenRequest {
            kind,
            arg: arg.clone(),
            resolve,
            reject,
        });

        if !g.borrow().started {
            // 首次 next()：启动体（同步执行到第一个 yieldOp 或完成）。
            g.borrow_mut().started = true;
            // 规范：suspended-start 时 next(v) 的 v 被忽略；但这里简化处理。
            self.async_gen_start(g)?;
        } else if g.borrow().parked.is_some() {
            // 已挂起：恢复执行（send 值为 next/return/throw 的 arg）。
            // return/throw 在 yieldOp 处处理（见 async_gen_yield_op 的 abrupt 检查）；
            // 这里统一恢复，让体继续跑到下一个 yieldOp。
            self.async_gen_resume(g, arg)?;
        }
        // 否则体正在执行中（在 await 或微任务里）：请求已在队列，
        // 等待 yieldOp 或 complete 时处理。

        Ok(Value::Promise(p))
    }

    /// `[...x]`：数组 / 字符串 / 生成器展开。
    pub(crate) fn spread_into_vec(&mut self, v: &Value) -> Result<Vec<Value>, FlowError> {
        match v {
            Value::Array(a) => Ok(a.borrow().elems.clone()),
            Value::String(s) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
            Value::Generator(g) => self.gen_drain(g),
            // phase 10：Set → 值；Map → [k,v] 对；TypedArray → 数值。
            Value::Set(s) => Ok(s.borrow().entries.clone()),
            Value::Map(m) => Ok(m
                .borrow()
                .entries
                .iter()
                .map(|(k, vv)| {
                    Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                        vec![k.clone(), vv.clone()],
                        Some(self.protos.array.clone()),
                    ))))
                })
                .collect()),
            Value::TypedArray(t) => {
                let ta = t.borrow();
                let buf = ta.buffer.borrow();
                let bytes = buf.bytes.borrow();
                Ok((0..ta.len)
                    .map(|i| Value::Number(ta.read_at(&bytes, i)))
                    .collect())
            }
            _ => Err(type_err(format!(
                "spread: {} is not iterable",
                v.type_of()
            ))),
        }
    }

    // ---------------- Proxy ----------------

    /// 安装 `Proxy` 构造器（`eval_new` 按指针识别）。
    fn install_proxy(&mut self) {
        let ctor = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.function.clone(),
        ))));
        ctor.borrow_mut()
            .set("name", Value::String("Proxy".to_string()));
        self.proxy_ctor = Some(ctor.clone());
        self.define_global("Proxy", Value::Object(ctor));
        // 生成器 yield*/for-of 改写用的内部迭代器辅助（合成名，防碰撞）。
        self.define_global("__yousj$iter_of", Value::Native(native_iter_of));
        self.define_global("__yousj$iter_next", Value::Native(native_iter_next));
    }

    // ------------------------------------------------------------------
    // Phase 10：标准库补完
    // ------------------------------------------------------------------

    /// 小 helper：建构造器对象（`__proto__` = Function.prototype，挂
    /// `.prototype` / `.name`）。
    pub(crate) fn make_ctor(&mut self, name: &str, proto: ObjectRef) -> ObjectRef {
        let ctor = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.function.clone(),
        ))));
        {
            let mut o = ctor.borrow_mut();
            o.set("prototype", Value::Object(proto));
            o.set("name", Value::String(name.to_string()));
        }
        ctor
    }

    fn install_p10(&mut self) {
        self.install_p10_collections();
        self.install_p10_binary();
        self.install_p10_date_intl();
        self.install_p10_string();
        self.install_p10_array_extra();
        self.install_p10_object_math_number();
        self.install_p10_globals();
    }

    /// Phase 10：Map / Set / WeakMap / WeakSet。
    fn install_p10_collections(&mut self) {
        // ---- Map.prototype ----
        {
            let mut p = self.protos.map.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("set", native_map_set),
                ("get", native_map_get),
                ("has", native_map_has),
                ("delete", native_map_delete),
                ("clear", native_map_clear),
                ("keys", native_map_keys),
                ("values", native_map_values),
                ("entries", native_map_entries),
                // forEach 回调需进解释器：try_host_method 拦截；
                // 挂桩保证 `typeof m.forEach === 'function'`。
                ("forEach", intercepted_method_stub),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let map_ctor = self.make_ctor("Map", self.protos.map.clone());
        self.map_ctor = Some(map_ctor.clone());
        self.define_global("Map", Value::Object(map_ctor));

        // ---- Set.prototype ----
        {
            let mut p = self.protos.set.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("add", native_set_add),
                ("has", native_set_has),
                ("delete", native_set_delete),
                ("clear", native_set_clear),
                ("keys", native_set_values),
                ("values", native_set_values),
                ("entries", native_set_entries),
                ("forEach", intercepted_method_stub),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let set_ctor = self.make_ctor("Set", self.protos.set.clone());
        self.set_ctor = Some(set_ctor.clone());
        self.define_global("Set", Value::Object(set_ctor));

        // ---- WeakMap.prototype ----
        {
            let mut p = self.protos.weakmap.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("set", native_weakmap_set),
                ("get", native_weakmap_get),
                ("has", native_weakmap_has),
                ("delete", native_weakmap_delete),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let weakmap_ctor = self.make_ctor("WeakMap", self.protos.weakmap.clone());
        self.weakmap_ctor = Some(weakmap_ctor.clone());
        self.define_global("WeakMap", Value::Object(weakmap_ctor));

        // ---- WeakSet.prototype ----
        {
            let mut p = self.protos.weakset.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("add", native_weakset_add),
                ("has", native_weakset_has),
                ("delete", native_weakset_delete),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let weakset_ctor = self.make_ctor("WeakSet", self.protos.weakset.clone());
        self.weakset_ctor = Some(weakset_ctor.clone());
        self.define_global("WeakSet", Value::Object(weakset_ctor));
    }

    /// `new Map(iterable?)`。iterable 接受 Array（`[[k,v],...]`）或 Map。
    fn construct_map(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let m = Rc::new(RefCell::new(JsMap {
            entries: Vec::new(),
        }));
        if let Some(init) = args.first() {
            match init {
                Value::Array(a) => {
                    for pair in a.borrow().elems.clone() {
                        let (k, v) = match pair {
                            Value::Array(pa) => {
                                let e = pa.borrow();
                                (
                                    e.elems.first().cloned().unwrap_or(Value::Undefined),
                                    e.elems.get(1).cloned().unwrap_or(Value::Undefined),
                                )
                            }
                            _ => {
                                return Err(type_err(
                                    "Map initializer must be an array of [k, v] pairs",
                                ))
                            }
                        };
                        map_set_entry(&m, k, v);
                    }
                }
                Value::Map(other) => {
                    let es = other.borrow().entries.clone();
                    m.borrow_mut().entries = es;
                }
                Value::Undefined | Value::Null => {}
                _ => return Err(type_err("Map initializer is not iterable")),
            }
        }
        Ok(Value::Map(m))
    }

    /// `new Set(iterable?)`。iterable 接受 Array 或 Set。
    fn construct_set(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let s = Rc::new(RefCell::new(JsSet {
            entries: Vec::new(),
        }));
        if let Some(init) = args.first() {
            match init {
                Value::Array(a) => {
                    for v in a.borrow().elems.clone() {
                        set_add_entry(&s, v);
                    }
                }
                Value::Set(other) => {
                    let es = other.borrow().entries.clone();
                    s.borrow_mut().entries = es;
                }
                Value::Undefined | Value::Null => {}
                _ => return Err(type_err("Set initializer is not iterable")),
            }
        }
        Ok(Value::Set(s))
    }

    /// `new WeakMap()`。
    fn construct_weakmap(&mut self, _args: Vec<Value>) -> Result<Value, FlowError> {
        Ok(Value::WeakMap(Rc::new(RefCell::new(JsWeakMap {
            entries: Vec::new(),
            sweep_debt: 0,
        }))))
    }

    /// `new WeakSet()`。
    fn construct_weakset(&mut self, _args: Vec<Value>) -> Result<Value, FlowError> {
        Ok(Value::WeakSet(Rc::new(RefCell::new(JsWeakSet {
            entries: Vec::new(),
            sweep_debt: 0,
        }))))
    }

    /// Phase 10：ArrayBuffer / TypedArray / DataView。
    fn install_p10_binary(&mut self) {
        // ---- ArrayBuffer.prototype ----
        {
            let mut p = self.protos.arraybuffer.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[("slice", native_arraybuffer_slice)];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let ab_ctor = self.make_ctor("ArrayBuffer", self.protos.arraybuffer.clone());
        self.arraybuffer_ctor = Some(ab_ctor.clone());
        self.define_global("ArrayBuffer", Value::Object(ab_ctor));

        // ---- DataView.prototype ----
        {
            let mut p = self.protos.dataview.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("getInt8", native_dv_get_int8),
                ("getUint8", native_dv_get_uint8),
                ("getInt16", native_dv_get_int16),
                ("getUint16", native_dv_get_uint16),
                ("getInt32", native_dv_get_int32),
                ("getUint32", native_dv_get_uint32),
                ("getFloat32", native_dv_get_float32),
                ("getFloat64", native_dv_get_float64),
                ("setInt8", native_dv_set_int8),
                ("setUint8", native_dv_set_uint8),
                ("setInt16", native_dv_set_int16),
                ("setUint16", native_dv_set_uint16),
                ("setInt32", native_dv_set_int32),
                ("setUint32", native_dv_set_uint32),
                ("setFloat32", native_dv_set_float32),
                ("setFloat64", native_dv_set_float64),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let dv_ctor = self.make_ctor("DataView", self.protos.dataview.clone());
        self.dataview_ctor = Some(dv_ctor.clone());
        self.define_global("DataView", Value::Object(dv_ctor));

        // ---- TypedArray 全家桶（全部类型共享 `typedarray` 原型；instanceof 按各 ctor 识别） ----
        for kind in TypedKind::all() {
            let kind = *kind;
            let ctor = self.make_ctor(kind.name(), self.protos.typedarray.clone());
            ctor.borrow_mut()
                .set("BYTES_PER_ELEMENT", Value::Number(kind.bytes() as f64));
            ctor.borrow_mut()
                .set("name", Value::String(kind.name().to_string()));
            self.typed_array_ctors.push((kind, ctor.clone()));
            self.define_global(kind.name(), Value::Object(ctor));
        }
        // 共享原型的全部方法（挂一次即可）。
        {
            let mut p = self.protos.typedarray.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("set", native_ta_set),
                ("subarray", native_ta_subarray),
                ("slice", native_ta_slice),
                ("fill", native_ta_fill),
                ("indexOf", native_ta_indexof),
                ("includes", native_ta_includes),
                ("join", native_ta_join),
                ("at", native_ta_at),
                ("values", native_ta_values),
                ("keys", native_ta_keys),
                ("entries", native_ta_entries),
                // forEach/map 回调需进解释器：try_host_method 拦截。
                ("forEach", intercepted_method_stub),
                ("map", intercepted_method_stub),
                ("filter", intercepted_method_stub),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
    }

    /// `new ArrayBuffer(byteLength)`。
    fn construct_arraybuffer(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let n = args.first().map(|v| v.to_number()).unwrap_or(0.0);
        if !n.is_finite() || n < 0.0 {
            return Err(type_err("invalid ArrayBuffer length"));
        }
        let len = n as usize;
        // Phase 14：超大 ArrayBuffer 不实际分配（避免 OOM abort）；
        // 规范要求分配失败抛 RangeError。
        const AB_SPARSE_THRESHOLD: usize = 1_000_000_000;
        if len > AB_SPARSE_THRESHOLD {
            return Err(crate::value::range_err(format!(
                "ArrayBuffer too large: {len}"
            )));
        }
        Ok(Value::ArrayBuffer(Rc::new(RefCell::new(JsArrayBuffer {
            bytes: Rc::new(RefCell::new(vec![0u8; len])),
        }))))
    }

    /// `new DataView(buffer, byteOffset?, byteLength?)`。
    fn construct_dataview(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let buf = match args.first() {
            Some(Value::ArrayBuffer(b)) => b.clone(),
            _ => return Err(type_err("DataView requires an ArrayBuffer")),
        };
        let blen = buf.borrow().bytes.borrow().len();
        let byte_offset = args.get(1).map(|v| v.to_number()).unwrap_or(0.0) as usize;
        if byte_offset > blen {
            return Err(type_err("DataView byteOffset out of range"));
        }
        let byte_len = match args.get(2) {
            Some(v) if !matches!(v, Value::Undefined) => v.to_number() as usize,
            _ => blen - byte_offset,
        };
        if byte_offset + byte_len > blen {
            return Err(type_err("DataView byteLength out of range"));
        }
        Ok(Value::DataView(Rc::new(RefCell::new(JsDataView {
            buffer: buf,
            byte_offset,
            byte_len,
        }))))
    }

    /// `new Uint8Array(...)` 等：重载 length | Array | TypedArray | ArrayBuffer。
    fn construct_typed_array(
        &mut self,
        kind: TypedKind,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        match args.first() {
            None | Some(Value::Undefined) => Ok(Value::TypedArray(ta_new(kind, 0))),
            Some(Value::Number(_)) => {
                let n = args[0].to_number();
                if !n.is_finite() || n < 0.0 {
                    return Err(type_err("invalid TypedArray length"));
                }
                Ok(Value::TypedArray(ta_new(kind, n as usize)))
            }
            Some(Value::Array(a)) => {
                let elems = a.borrow().elems.clone();
                let ta = ta_new(kind, elems.len());
                let t = ta.borrow();
                let tbuf = t.buffer.borrow();
                let mut bytes = tbuf.bytes.borrow_mut();
                for (i, v) in elems.iter().enumerate() {
                    t.write_at(&mut bytes, i, v.to_number());
                }
                drop(bytes);
                drop(tbuf);
                drop(t);
                Ok(Value::TypedArray(ta))
            }
            Some(Value::TypedArray(src)) => {
                let s = src.borrow();
                let sbuf = s.buffer.borrow();
                let sb = sbuf.bytes.borrow();
                let ta = ta_new(kind, s.len);
                let t = ta.borrow();
                let tbuf = t.buffer.borrow();
                let mut bytes = tbuf.bytes.borrow_mut();
                for i in 0..s.len {
                    kind.write(&mut bytes, i, s.read_at(&sb, i));
                }
                drop(bytes);
                drop(tbuf);
                drop(t);
                Ok(Value::TypedArray(ta))
            }
            Some(Value::ArrayBuffer(buf)) => {
                let bbb = buf.borrow();
                let blen = bbb.bytes.borrow().len();
                let byte_offset = args.get(1).map(|v| v.to_number()).unwrap_or(0.0) as usize;
                if byte_offset % kind.bytes() != 0 {
                    return Err(type_err("TypedArray byteOffset must be a multiple of element size"));
                }
                if byte_offset > blen {
                    return Err(type_err("TypedArray byteOffset out of range"));
                }
                let len = match args.get(2) {
                    Some(v) if !matches!(v, Value::Undefined) => {
                        let l = v.to_number() as usize;
                        if byte_offset + l * kind.bytes() > blen {
                            return Err(type_err("TypedArray length out of range"));
                        }
                        l
                    }
                    _ => {
                        let rem = blen - byte_offset;
                        if rem % kind.bytes() != 0 {
                            return Err(type_err(
                                "ArrayBuffer length minus offset is not a multiple of element size",
                            ));
                        }
                        rem / kind.bytes()
                    }
                };
                Ok(Value::TypedArray(Rc::new(RefCell::new(JsTypedArray {
                    buffer: buf.clone(),
                    byte_offset,
                    len,
                    kind,
                }))))
            }
            _ => Err(type_err("invalid TypedArray argument")),
        }
    }

    /// Phase 10：Date 与 Intl（最小可用子集；全部 UTC）。
    fn install_p10_date_intl(&mut self) {
        // ---- Date.prototype ----
        {
            let mut p = self.protos.date.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("getTime", native_date_gettime),
                ("valueOf", native_date_gettime),
                ("toISOString", native_date_toiso),
                ("toString", native_date_tostring),
                ("getUTCFullYear", native_date_getutc_full_year),
                ("getUTCMonth", native_date_getutc_month),
                ("getUTCDate", native_date_getutc_date),
                ("getUTCDay", native_date_getutc_day),
                ("getUTCHours", native_date_getutc_hours),
                ("getUTCMinutes", native_date_getutc_minutes),
                ("getUTCSeconds", native_date_getutc_seconds),
                ("getUTCMilliseconds", native_date_getutc_milliseconds),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }
        let date_ctor = self.make_ctor("Date", self.protos.date.clone());
        date_ctor
            .borrow_mut()
            .set("now", Value::Native(native_date_now));
        date_ctor
            .borrow_mut()
            .set("UTC", Value::Native(native_date_utc));
        self.date_ctor = Some(date_ctor.clone());
        self.define_global("Date", Value::Object(date_ctor));

        // ---- Intl ----
        let intl = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        // Intl.NumberFormat
        let nf_proto = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        let nf_ctor = self.make_ctor("NumberFormat", nf_proto);
        self.intl_numberformat_ctor = Some(nf_ctor.clone());
        intl.borrow_mut()
            .set("NumberFormat", Value::Object(nf_ctor));
        // Intl.DateTimeFormat
        let df_proto = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        let df_ctor = self.make_ctor("DateTimeFormat", df_proto);
        self.intl_datetimeformat_ctor = Some(df_ctor.clone());
        intl.borrow_mut()
            .set("DateTimeFormat", Value::Object(df_ctor));
        self.define_global("Intl", Value::Object(intl));
    }

    /// `new Date()` / `new Date(ms)` / `new Date(y, mo, d, h, mi, s, ms)`（UTC）。
    fn construct_date(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let ms = match args.len() {
            0 => now_ms(),
            1 => match &args[0] {
                Value::Date(d) => d.borrow().ms,
                v => v.to_number(),
            },
            _ => {
                let y = args[0].to_number();
                let mo = args.get(1).map(|v| v.to_number()).unwrap_or(0.0);
                let d = args.get(2).map(|v| v.to_number()).unwrap_or(1.0);
                let h = args.get(3).map(|v| v.to_number()).unwrap_or(0.0);
                let mi = args.get(4).map(|v| v.to_number()).unwrap_or(0.0);
                let s = args.get(5).map(|v| v.to_number()).unwrap_or(0.0);
                let ms2 = args.get(6).map(|v| v.to_number()).unwrap_or(0.0);
                if !y.is_finite() {
                    return Err(type_err("invalid Date"));
                }
                // 年月日 → 天数（Hinnant days_from_civil 逆算法），全部按 UTC。
                // mo 为 0-based，先转 1-based 月份 m1。
                let m1 = (mo as i64).rem_euclid(12) + 1;
                let y2 = if m1 <= 2 { y - 1.0 } else { y };
                let era = (if y2 >= 0.0 { y2 } else { y2 - 399.0 } / 400.0).floor();
                let yoe = y2 - era * 400.0;
                let mp = if m1 > 2 { m1 - 3 } else { m1 + 9 };
                let doy = (153 * mp + 2) / 5 + d as i64 - 1;
                let doe = yoe as i64 * 365 + yoe as i64 / 4 - yoe as i64 / 100 + doy;
                let days = era as i64 * 146097 + doe - 719468;
                days as f64 * 86400000.0 + h * 3600000.0 + mi * 60000.0 + s * 1000.0 + ms2
            }
        };
        Ok(Value::Date(Rc::new(RefCell::new(JsDate { ms }))))
    }

    /// `new Intl.NumberFormat(locales?, options?)` → 格式化器对象（plain object + tag）。
    fn construct_intl_numberformat(
        &mut self,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let locale = match args.first() {
            Some(Value::String(s)) => intl_canon_locale(s),
            _ => "en-US".to_string(),
        };
        let (style, currency, min_fd, max_fd, grouping) = match args.get(1) {
            Some(Value::Object(o)) => {
                let g = |k: &str| o.borrow().get_in_chain(k).unwrap_or(Value::Undefined);
                let style = g("style").to_js_string();
                let style = if ["decimal", "currency", "percent"].contains(&style.as_str()) {
                    style
                } else {
                    "decimal".to_string()
                };
                let currency = g("currency").to_js_string().to_uppercase();
                let min_fd = {
                    let n = g("minimumFractionDigits").to_number();
                    if n.is_finite() && n >= 0.0 {
                        n as u32
                    } else {
                        u32::MAX
                    }
                };
                let max_fd = {
                    let n = g("maximumFractionDigits").to_number();
                    if n.is_finite() && n >= 0.0 {
                        n as u32
                    } else {
                        u32::MAX
                    }
                };
                let grouping = match g("useGrouping") {
                    // 未指定时默认 true（与规范一致）。
                    Value::Undefined => true,
                    v => v.to_boolean(),
                };
                (style, currency, min_fd, max_fd, grouping)
            }
            _ => ("decimal".to_string(), "USD".to_string(), u32::MAX, u32::MAX, true),
        };
        let fmt = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        {
            let mut f = fmt.borrow_mut();
            f.set("__intl_kind", Value::String("NumberFormat".to_string()));
            f.set("__locale", Value::String(locale));
            f.set("__style", Value::String(style));
            f.set("__currency", Value::String(currency));
            f.set(
                "__min_fd",
                Value::Number(if min_fd == u32::MAX { -1.0 } else { min_fd as f64 }),
            );
            f.set(
                "__max_fd",
                Value::Number(if max_fd == u32::MAX { -1.0 } else { max_fd as f64 }),
            );
            f.set("__grouping", Value::Bool(grouping));
            f.set("format", Value::Native(native_intl_nf_format));
            f.set("formatToParts", Value::Native(native_intl_nf_format_to_parts));
            f.set("resolvedOptions", Value::Native(native_intl_nf_resolved));
        }
        Ok(Value::Object(fmt))
    }

    /// `new Intl.DateTimeFormat(locales?, options?)` → 格式化器对象。
    fn construct_intl_datetimeformat(
        &mut self,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let locale = match args.first() {
            Some(Value::String(s)) => intl_canon_locale(s),
            _ => "en-US".to_string(),
        };
        // 选项：year/month/day/hour/minute/second/weekday（"numeric"/"2-digit"/"long" 等）。
        let mut opts: Vec<(String, String)> = Vec::new();
        if let Some(Value::Object(o)) = args.get(1) {
            for k in ["year", "month", "day", "hour", "minute", "second", "weekday"] {
                let v = o.borrow().get_in_chain(k).unwrap_or(Value::Undefined);
                if !matches!(v, Value::Undefined) {
                    opts.push((k.to_string(), v.to_js_string()));
                }
            }
        }
        let fmt = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        {
            let mut f = fmt.borrow_mut();
            f.set("__intl_kind", Value::String("DateTimeFormat".to_string()));
            f.set("__locale", Value::String(locale.clone()));
            let arr = opts
                .iter()
                .map(|(k, v)| {
                    Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                        vec![Value::String(k.clone()), Value::String(v.clone())],
                        Some(self.protos.array.clone()),
                    ))))
                })
                .collect::<Vec<_>>();
            f.set(
                "__fields",
                Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                    arr,
                    Some(self.protos.array.clone()),
                )))),
            );
            f.set("format", Value::Native(native_intl_df_format));
            f.set("formatToParts", Value::Native(native_intl_df_format_to_parts));
            f.set("resolvedOptions", Value::Native(native_intl_df_resolved));
        }
        Ok(Value::Object(fmt))
    }

    /// Phase 10：String.prototype 查漏补缺。
    fn install_p10_string(&mut self) {
        let mut p = self.protos.string.borrow_mut();
        let methods: &[(&str, NativeFn)] = &[
            ("padStart", native_str_padstart),
            ("padEnd", native_str_padend),
            ("repeat", native_str_repeat),
            ("trimStart", native_str_trimstart),
            ("trimEnd", native_str_trimend),
            ("replaceAll", native_str_replaceall),
            ("at", native_str_at),
            ("lastIndexOf", native_str_lastindexof),
            ("codePointAt", native_str_codepointat),
            ("normalize", native_str_normalize),
        ];
        for (name, f) in methods {
            p.set(name, Value::Native(*f));
        }
        drop(p);
        // String.fromCharCode / fromCodePoint 静态（String 构造器已在
        // install_builtins 定义，此处追加静态方法）。
        if let Ok(Some(Value::Object(ctor))) = Env::lookup(&self.global, "String") {
            ctor.borrow_mut()
                .set("fromCharCode", Value::Native(native_str_fromcharcode));
            ctor.borrow_mut()
                .set("fromCodePoint", Value::Native(native_str_fromcharcode));
        }
    }

    /// Phase 10：Array.prototype 查漏补缺。
    fn install_p10_array_extra(&mut self) {
        let mut p = self.protos.array.borrow_mut();
        let methods: &[(&str, NativeFn)] = &[
            // some/every/findIndex/flatMap 回调需进解释器：try_host_method 拦截；
            // 挂桩保证 typeof 正常、detached 调用给清晰报错。
            ("findIndex", intercepted_method_stub),
            ("some", intercepted_method_stub),
            ("every", intercepted_method_stub),
            ("flat", native_arr_flat),
            ("reverse", native_arr_reverse),
            ("sort", native_arr_sort),
            ("splice", native_arr_splice),
            ("fill", native_arr_fill),
            ("at", native_arr_at),
            ("lastIndexOf", native_arr_lastindexof),
            ("copyWithin", native_arr_copywithin),
            // flatMap 回调需进解释器：try_host_method 拦截。
            ("flatMap", intercepted_method_stub),
        ];
        for (name, f) in methods {
            p.set(name, Value::Native(*f));
        }
        drop(p);
    }

    /// Phase 10：Object / Math / Number 查漏补缺。
    fn install_p10_object_math_number(&mut self) {
        // ---- Object 静态 ----
        if let Ok(Some(Value::Object(ctor))) = Env::lookup(&self.global, "Object") {
            let mut o = ctor.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("create", native_obj_create),
                ("hasOwn", native_obj_hasown),
                ("getOwnPropertyNames", native_obj_getownpropertynames),
                ("defineProperty", native_obj_defineproperty),
                ("is", native_obj_is),
                ("freeze", native_obj_freeze),
            ];
            for (name, f) in methods {
                o.set(name, Value::Native(*f));
            }
            drop(o);
        }
        // ---- Math ----
        if let Ok(Some(Value::Object(m))) = Env::lookup(&self.global, "Math") {
            let mut o = m.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("trunc", native_math_trunc),
                ("sign", native_math_sign),
                ("hypot", native_math_hypot),
                ("cbrt", native_math_cbrt),
                ("log", native_math_log),
                ("log2", native_math_log2),
                ("log10", native_math_log10),
                ("exp", native_math_exp),
                ("expm1", native_math_expm1),
                ("sin", native_math_sin),
                ("cos", native_math_cos),
                ("tan", native_math_tan),
                ("asin", native_math_asin),
                ("acos", native_math_acos),
                ("atan", native_math_atan),
                ("atan2", native_math_atan2),
                ("imul", native_math_imul),
                ("clz32", native_math_clz32),
                ("fround", native_math_fround),
                ("log1p", native_math_log1p),
                ("sinh", native_math_sinh),
                ("cosh", native_math_cosh),
                ("tanh", native_math_tanh),
            ];
            for (name, f) in methods {
                o.set(name, Value::Native(*f));
            }
            drop(o);
        }
        // ---- Number ----
        // Number.prototype 方法。
        {
            let mut p = self.protos.number.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("toFixed", native_num_tofixed),
                ("toExponential", native_num_toexponential),
                ("toPrecision", native_num_toprecision),
                ("toString", native_num_tostring),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
            drop(p);
        }
        // Number 静态常量与方法。
        if let Ok(Some(Value::Object(ctor))) = Env::lookup(&self.global, "Number") {
            let mut o = ctor.borrow_mut();
            o.set("MAX_SAFE_INTEGER", Value::Number(9007199254740991.0));
            o.set("MIN_SAFE_INTEGER", Value::Number(-9007199254740991.0));
            o.set("MAX_VALUE", Value::Number(f64::MAX));
            o.set("MIN_VALUE", Value::Number(f64::MIN_POSITIVE));
            o.set("EPSILON", Value::Number(f64::EPSILON));
            o.set("POSITIVE_INFINITY", Value::Number(f64::INFINITY));
            o.set("NEGATIVE_INFINITY", Value::Number(f64::NEG_INFINITY));
            o.set("NaN", Value::Number(f64::NAN));
            let methods: &[(&str, NativeFn)] = &[
                ("isInteger", native_num_isinteger),
                ("isNaN", native_num_isnan),
                ("isFinite", native_num_isfinite),
                ("isSafeInteger", native_num_issafeinteger),
                ("parseInt", native_global_parseint),
                ("parseFloat", native_global_parsefloat),
            ];
            for (name, f) in methods {
                o.set(name, Value::Native(*f));
            }
            drop(o);
        }
    }

    /// Phase 10：全局 NaN/Infinity/undefined/isFinite/Boolean()。
    /// 注：String()/Number() 直接调用的转换语义在 `call_value_at` 按
    /// 构造器指针拦截（不覆盖构造器对象本身）。
    fn install_p10_globals(&mut self) {
        self.define_global("NaN", Value::Number(f64::NAN));
        self.define_global("Infinity", Value::Number(f64::INFINITY));
        self.define_global("undefined", Value::Undefined);
        self.define_global("isFinite", Value::Native(native_global_isfinite));
        self.define_global("Boolean", Value::Native(native_global_boolean_call));
    }

    /// `new Proxy(target, handler)`。
    fn construct_proxy(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let mut it = args.into_iter();
        let target = it.next().unwrap_or(Value::Undefined);
        let handler = it.next().unwrap_or(Value::Undefined);
        match &target {
            Value::Object(_)
            | Value::Array(_)
            | Value::Function(_)
            | Value::Native(_)
            | Value::DomNode(_)
            | Value::Promise(_)
            | Value::RegExp(_)
            | Value::Generator(_)
            | Value::Proxy(_) => {}
            _ => {
                return Err(type_err(format!(
                    "Proxy target must be an object, got {}",
                    target.type_of()
                )))
            }
        }
        let handler_obj = match handler {
            Value::Object(o) => o,
            Value::Null | Value::Undefined => Rc::new(RefCell::new(JsObject::new())),
            _ => return Err(type_err("Proxy handler must be an object")),
        };
        Ok(Value::Proxy(Rc::new(RefCell::new(JsProxy {
            target,
            handler: handler_obj,
        }))))
    }

    /// 取 handler 上的 trap：存在且可调用才返回 Some。
    fn get_trap(
        &mut self,
        handler: &ObjectRef,
        name: &str,
    ) -> Result<Option<Value>, FlowError> {
        let v = handler
            .borrow()
            .get(name)
            .unwrap_or(Value::Undefined);
        if v.is_nullish() {
            return Ok(None);
        }
        if v.is_callable() {
            Ok(Some(v))
        } else {
            Err(type_err(format!("proxy trap '{}' is not callable", name)))
        }
    }

    /// Proxy 的 `set` trap 分发；无 trap 则落到 target。
    fn proxy_set(
        &mut self,
        p: &ProxyRef,
        key: &str,
        val: Value,
    ) -> Result<(), FlowError> {
        let (target, handler) = {
            let pr = p.borrow();
            (pr.target.clone(), pr.handler.clone())
        };
        if let Some(trap) = self.get_trap(&handler, "set")? {
            self.call_value(
                trap,
                Value::Object(handler),
                vec![
                    target,
                    Value::String(key.to_string()),
                    val,
                    Value::Proxy(p.clone()),
                ],
            )?;
            return Ok(());
        }
        self.set_prop(&target, key, val)
    }

    /// Proxy 的 `has` trap 分发；无 trap 则查 target（含原型链）。
    fn proxy_has(&mut self, p: &ProxyRef, key: &str) -> Result<bool, FlowError> {
        let (target, handler) = {
            let pr = p.borrow();
            (pr.target.clone(), pr.handler.clone())
        };
        if let Some(trap) = self.get_trap(&handler, "has")? {
            let r = self.call_value(
                trap,
                Value::Object(handler),
                vec![target, Value::String(key.to_string())],
            )?;
            return Ok(r.to_boolean());
        }
        Ok(self.prop_exists(&target, key))
    }

    /// 属性存在性（含原型链；`in` 运算符用）。
    fn prop_exists(&mut self, base: &Value, key: &str) -> bool {
        match base {
            Value::Object(o) => o.borrow().get_in_chain(key).is_some(),
            Value::Array(a) => {
                let arr = a.borrow();
                key == "length"
                    || key
                        .parse::<usize>()
                        .map(|i| i < arr.elems.len())
                        .unwrap_or(false)
                    || arr.props.get_in_chain(key).is_some()
            }
            Value::String(s) => {
                key == "length"
                    || key
                        .parse::<usize>()
                        .map(|i| i < s.chars().count())
                        .unwrap_or(false)
                    || self.protos.string.borrow().get_in_chain(key).is_some()
            }
            Value::Function(f) => {
                key == "name" || key == "length" || key == "prototype" || f.statics.borrow().contains_key(key)
            }
            _ => false,
        }
    }

    /// Proxy 的 `apply` trap 分发；无 trap 则调用 target。
    fn proxy_apply(
        &mut self,
        p: &ProxyRef,
        this: Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let (target, handler) = {
            let pr = p.borrow();
            (pr.target.clone(), pr.handler.clone())
        };
        if let Some(trap) = self.get_trap(&handler, "apply")? {
            let args_arr = Value::Array(Rc::new(RefCell::new(JsArray::new(args))));
            return self.call_value(
                trap,
                Value::Object(handler),
                vec![target, this, args_arr],
            );
        }
        if target.is_callable() {
            self.call_value(target, this, args)
        } else {
            Err(type_err("proxy target is not callable"))
        }
    }

    /// Proxy 的 `construct` trap 分发；无 trap 则构造 target。
    fn proxy_construct(
        &mut self,
        p: &ProxyRef,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let (target, handler) = {
            let pr = p.borrow();
            (pr.target.clone(), pr.handler.clone())
        };
        if let Some(trap) = self.get_trap(&handler, "construct")? {
            let args_arr = Value::Array(Rc::new(RefCell::new(JsArray::new(args))));
            let r = self.call_value(
                trap,
                Value::Object(handler),
                vec![target, args_arr, Value::Proxy(p.clone())],
            )?;
            // 规范要求 construct trap 返回对象；子集里宽松处理。
            return Ok(r);
        }
        self.construct_value(&target, args)
    }

    /// 以构造语义调用值（Proxy construct 回退 / super 调用共用）。
    fn construct_value(
        &mut self,
        target: &Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        match target {
            Value::Function(f) => {
                if f.is_generator {
                    return Err(type_err("generator function is not a constructor"));
                }
                if f.is_class {
                    return self.construct_class(f, args);
                }
                let obj = Rc::new(RefCell::new(JsObject::new()));
                obj.borrow_mut().proto = f.prototype.clone();
                obj.borrow_mut().tag = f.name.clone();
                let r = self.call_function(f, Value::Object(obj.clone()), args, None)?;
                match r {
                    Value::Object(_) => Ok(r),
                    _ => Ok(Value::Object(obj)),
                }
            }
            Value::Proxy(p) => self.proxy_construct(p, args),
            Value::Object(o) => {
                // 错误构造器等对象型构造器（Promise/RegExp 在 eval_new 已拦截，
                // 此处兜底错误构造器）。
                if let Some(kind) = self.error_ctor_kind(o) {
                    return self.construct_error(kind, args);
                }
                Err(type_err(format!(
                    "{} is not a constructor",
                    target.to_js_string()
                )))
            }
            _ => Err(type_err(format!(
                "{} is not a constructor",
                target.to_js_string()
            ))),
        }
    }

    // ---------------- 类 ----------------

    /// 类键转字符串（公开键；计算键求值）。
    fn class_key_string(
        &mut self,
        key: &ClassKey,
        env: &EnvRef,
    ) -> Result<Option<String>, FlowError> {
        match key {
            ClassKey::Public(PropKey::Ident(s)) | ClassKey::Public(PropKey::String(s)) => {
                Ok(Some(s.clone()))
            }
            ClassKey::Public(PropKey::Number(n)) => Ok(Some(number_to_js_string(*n))),
            ClassKey::Public(PropKey::Computed(e)) => {
                Ok(Some(self.eval_expr(e, env.clone())?.to_js_string()))
            }
            ClassKey::Private(_) => Ok(None),
        }
    }

    /// `class C extends B { ... }` 求值 → 构造器函数值。
    pub(crate) fn eval_class(
        &mut self,
        c: &ClassNode,
        name: Option<String>,
        env: EnvRef,
        span: Span,
    ) -> Result<Value, FlowError> {
        let class_name = name.clone().unwrap_or_default();
        // 1. 父类。
        let (super_ctor, parent_proto): (Option<Value>, Option<ObjectRef>) = match &c.super_class {
            Some(se) => {
                let v = self.eval_expr(se, env.clone())?;
                match &v {
                    Value::Function(f) => {
                        let pp = f.prototype.clone().unwrap_or_else(|| self.protos.object.clone());
                        (Some(v.clone()), Some(pp))
                    }
                    Value::Object(o) if self.error_ctor_kind(o).is_some() => {
                        // `class X extends TypeError`：原型链走错误原型。
                        let pp = self
                            .error_protos
                            .get(&self.error_ctor_kind(o).unwrap())
                            .cloned()
                            .unwrap_or_else(|| self.protos.object.clone());
                        (Some(v.clone()), Some(pp))
                    }
                    Value::Null => (None, Some(self.protos.object.clone())),
                    _ => {
                        return Err(type_err(format!(
                            "superclass must be a constructor, got {}",
                            v.type_of()
                        )))
                    }
                }
            }
            None => (None, Some(self.protos.object.clone())),
        };
        let is_derived = super_ctor.is_some();

        // 2. 类 id 与私有作用域。
        let class_id = self.class_seq;
        self.class_seq += 1;
        let mut instance_names = Vec::new();
        let mut static_names = Vec::new();
        for el in &c.body {
            let (key, is_static) = match el {
                ClassElem::Method { key, is_static, .. } => (key, *is_static),
                ClassElem::Field { key, is_static, .. } => (key, *is_static),
                ClassElem::StaticBlock(_) => continue,
            };
            if let ClassKey::Private(n) = key {
                if is_static {
                    if !static_names.contains(n) {
                        static_names.push(n.clone());
                    }
                } else if !instance_names.contains(n) {
                    instance_names.push(n.clone());
                }
            }
        }
        let private_scope = Rc::new(PrivateScope {
            class_id,
            instance_names,
            static_names,
        });

        // 3. prototype 对象（实例的原型；实例方法装在这里）。
        // phase 12：不再用普通属性存 "constructor" 回指（那会与
        // `ctor.prototype` 形成 Rc 强环、每个类永久泄漏）；改用弱回指，
        // 读 `.constructor` 时经 `JsObject::get` 升级。
        let prototype = Rc::new(RefCell::new(JsObject::with_proto(parent_proto)));

        // 4. 找显式构造器。
        let ctor_elem = c.body.iter().find_map(|el| match el {
            ClassElem::Method {
                kind: MethodKind::Constructor,
                func,
                is_static: false,
                ..
            } => Some(func),
            _ => None,
        });

        // 5. 收集实例字段定义（构造器用；含私有字段；计算键类求值时求值一次）。
        let mut instance_fields = Vec::new();
        for el in &c.body {
            if let ClassElem::Field {
                key,
                init,
                is_static: false,
            } = el
            {
                match key {
                    ClassKey::Public(pk) => {
                        let s = match pk {
                            PropKey::Ident(s) | PropKey::String(s) => s.clone(),
                            PropKey::Number(n) => number_to_js_string(*n),
                            PropKey::Computed(e) => {
                                self.eval_expr(e, env.clone())?.to_js_string()
                            }
                        };
                        instance_fields.push(ClassField {
                            name: s,
                            is_private: false,
                            init: init.clone(),
                        });
                    }
                    ClassKey::Private(n) => instance_fields.push(ClassField {
                        name: n.clone(),
                        is_private: true,
                        init: init.clone(),
                    }),
                }
            }
        }

        // 6. 构造器函数。
        //    无显式构造器：基类为空体；派生类 `is_default_derived`，
        //    构造时直接转发 `super(...args)`（construct_class 处理）。
        let ctor_func: FuncRef = match ctor_elem {
            Some(f) => {
                let ext = Phase9FuncFields {
                    is_class: true,
                    super_ctor: super_ctor.clone(),
                    private_scope: Some(private_scope.clone()),
                    class_id,
                    instance_fields,
                    prototype_override: Some(prototype.clone()),
                    ..Phase9FuncFields::default()
                };
                self.make_function_ext(
                    class_name.clone(),
                    f,
                    env.clone(),
                    false,
                    span.clone(),
                    ext,
                )
            }
            None => {
                let fnode = FunctionNode {
                    id: Some(class_name.clone()),
                    params: vec![],
                    body: vec![],
                    is_generator: false,
                    is_async: false,
                    strict: self.is_strict(),
                };
                let ext = Phase9FuncFields {
                    is_class: true,
                    super_ctor: super_ctor.clone(),
                    private_scope: Some(private_scope.clone()),
                    class_id,
                    instance_fields,
                    is_default_derived: is_derived,
                    prototype_override: Some(prototype.clone()),
                    ..Phase9FuncFields::default()
                };
                self.make_function_ext(
                    class_name.clone(),
                    &fnode,
                    env.clone(),
                    false,
                    span.clone(),
                    ext,
                )
            }
        };

        // 7. 安装方法 / 静态字段 / 静态块（实例字段已进 ctor.instance_fields）。
        self.install_class_elements(
            &ctor_func,
            &prototype,
            &c.body,
            &private_scope,
            class_id,
            &env,
            &span,
        )?;
        prototype.borrow_mut().ctor_backref = Some(Rc::downgrade(&ctor_func));

        // 7. 类名绑定（类表达式自引用；类声明由调用方绑定）。
        //    构造器函数名已在 make_function_ext 里设置。
        Ok(Value::Function(ctor_func))
    }

    /// 安装类体成员：实例方法→prototype；静态→ctor.statics；
    /// 私有方法→privates；字段→ctor.instance_fields（静态字段直接求值）。
    #[allow(clippy::too_many_arguments)]
    fn install_class_elements(
        &mut self,
        ctor: &FuncRef,
        prototype: &ObjectRef,
        body: &[ClassElem],
        private_scope: &PrivateScopeRef,
        class_id: u64,
        env: &EnvRef,
        span: &Span,
    ) -> Result<(), FlowError> {
        for el in body {
            match el {
                ClassElem::Method {
                    key,
                    func,
                    kind,
                    is_static,
                } => {
                    if *kind == MethodKind::Constructor && !is_static {
                        continue; // 构造器已处理
                    }
                    let fname = match key {
                        ClassKey::Public(_) => {
                            self.class_key_string(key, env)?.unwrap_or_default()
                        }
                        ClassKey::Private(n) => format!("#{}", n),
                    };
                    // home_object：实例方法→prototype；静态方法→无（走 super_ctor 查父构造器）。
                    // phase 12：弱引用，打破"方法 → home_object → prototype → 方法"的强环。
                    let home = if *is_static {
                        None
                    } else {
                        Some(Rc::downgrade(&prototype))
                    };
                    let ext = Phase9FuncFields {
                        home_object: home,
                        private_scope: Some(private_scope.clone()),
                        class_id,
                        ..Phase9FuncFields::default()
                    };
                    let m = self.make_function_ext(
                        fname.clone(),
                        func,
                        env.clone(),
                        false,
                        span.clone(),
                        ext,
                    );
                    match key {
                        ClassKey::Public(_) => {
                            let k = self.class_key_string(key, env)?.unwrap();
                            // phase 9：getter/setter 安装为访问器。
                            match kind {
                                MethodKind::Getter | MethodKind::Setter => {
                                    let target = if *is_static {
                                        // 静态访问器：存到 ctor 的 statics 关联的访问器表。
                                        // 子集简化：静态访问器暂按普通方法处理（极少用）。
                                        None
                                    } else {
                                        Some(prototype.clone())
                                    };
                                    if let Some(t) = target {
                                        let mut b = t.borrow_mut();
                                        let entry = b
                                            .accessors
                                            .entry(istr(&k))
                                            .or_insert((None, None));
                                        if *kind == MethodKind::Getter {
                                            entry.0 = Some(m.clone());
                                        } else {
                                            entry.1 = Some(m.clone());
                                        }
                                    } else {
                                        // 静态访问器回退：按普通方法存。
                                        ctor.statics
                                            .borrow_mut()
                                            .insert(istr(&k), Value::Function(m));
                                    }
                                }
                                _ => {
                                    if *is_static {
                                        ctor.statics
                                            .borrow_mut()
                                            .insert(istr(&k), Value::Function(m));
                                    } else {
                                        prototype.borrow_mut().set(&k, Value::Function(m));
                                    }
                                }
                            }
                        }
                        ClassKey::Private(n) => {
                            if *is_static {
                                ctor.private_statics
                                    .borrow_mut()
                                    .insert((class_id, istr(n)), Value::Function(m));
                            } else {
                                prototype.borrow_mut().privates.insert(
                                    (class_id, istr(n)),
                                    Value::Function(m),
                                );
                            }
                        }
                    }
                }
                ClassElem::Field {
                    key,
                    init,
                    is_static,
                } => {
                    if *is_static {
                        // 静态字段：类求值时求值一次。
                        // this 绑定为构造器（规范）；子集里用 undefined（极少用 this）。
                        let v = match init {
                            Some(e) => self.eval_expr(e, env.clone())?,
                            None => Value::Undefined,
                        };
                        match key {
                            ClassKey::Public(_) => {
                                let k = self.class_key_string(key, env)?.unwrap();
                                ctor.statics.borrow_mut().insert(istr(&k), v);
                            }
                            ClassKey::Private(n) => {
                                ctor.private_statics
                                    .borrow_mut()
                                    .insert((class_id, istr(n)), v);
                            }
                        }
                    }
                    // 实例字段已在 eval_class 收集（见 instance_fields）。
                }
                ClassElem::StaticBlock(stmts) => {
                    // 静态块：按序执行一次（this=构造器；子集用 undefined，极少用）。
                    let block_env = Env::child(env);
                    match self.exec_block(stmts, block_env.clone(), &block_env) {
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(())
    }

    /// `new` 类构造器：实例化 + 字段初始化 + 构造器体。
    /// 派生类无显式构造器时（is_default_derived）直接转发 `super(...args)`。
    fn construct_class(
        &mut self,
        ctor: &FuncRef,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let obj = Rc::new(RefCell::new(JsObject::new()));
        obj.borrow_mut().proto = ctor.prototype.clone();
        obj.borrow_mut().tag = ctor.name.clone();
        let is_derived = ctor.super_ctor.is_some();
        if !is_derived {
            // 基类：先装实例字段，再跑构造器体。
            self.init_instance_fields(ctor, &obj)?;
        }
        // this 先压栈（eval_super_call 读栈顶）。
        self.this_stack.push(Value::Object(obj.clone()));
        let r = if ctor.is_default_derived {
            // 等价于 `constructor(...args) { super(...args); }`。
            // func 压栈：eval_super_call 从 func_stack 读当前构造器。
            // （非默认分支由 call_function 自己压栈。）
            self.func_stack.push(ctor.clone());
            let r = self.eval_super_call(args);
            self.func_stack.pop();
            r
        } else {
            self.call_function(ctor, Value::Object(obj.clone()), args, None)
        };
        self.this_stack.pop();
        match r {
            // 构造器返回对象则采用（super() 返回的对象也在此），否则用 obj。
            Ok(Value::Object(o)) => Ok(Value::Object(o)),
            Ok(_) => Ok(Value::Object(obj)),
            // 派生类构造器没调 super() 就返回：规范抛错；子集宽松处理用 obj。
            Err(e) => Err(e),
        }
    }

    /// 初始化实例字段（公开→属性；私有→privates）。
    /// 求值时的 this 为实例 obj。
    fn init_instance_fields(
        &mut self,
        ctor: &FuncRef,
        obj: &ObjectRef,
    ) -> Result<(), FlowError> {
        if ctor.instance_fields.is_empty() {
            return Ok(());
        }
        // Phase 14：标记字段初始化器求值中（直接 eval 的 super 早期错误检查用）。
        self.field_init_depth += 1;
        self.this_stack.push(Value::Object(obj.clone()));
        let mut r = Ok(());
        for f in &ctor.instance_fields.clone() {
            let v = match &f.init {
                Some(e) => match self.eval_expr(e, ctor.closure.clone()) {
                    Ok(v) => v,
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                },
                None => Value::Undefined,
            };
            if f.is_private {
                obj.borrow_mut()
                    .privates
                    .insert((ctor.class_id, istr(&f.name)), v);
            } else {
                obj.borrow_mut().set(&f.name, v);
            }
        }
        self.this_stack.pop();
        self.field_init_depth -= 1;
        r
    }

    /// `super.x`：从当前方法的 home_object 原型链查找
    /// （静态方法则查父构造器）。this 绑定由调用方处理。
    pub(crate) fn get_super_prop(&mut self, key: &str) -> Result<Value, FlowError> {
        // 向上找最近的有 home_object 的函数帧（跳过箭头函数等）。
        // phase 12：home_object 为弱引用，升级失败视为无宿主对象。
        let mut home: Option<ObjectRef> = None;
        let mut parent_ctor: Option<Value> = None;
        for f in self.func_stack.iter().rev() {
            if let Some(h) = f.home_object.as_ref().and_then(|w| w.upgrade()) {
                home = Some(h);
                break;
            }
            if f.super_ctor.is_some() {
                parent_ctor = f.super_ctor.clone();
                break;
            }
        }
        if let Some(h) = home {
            let proto = h.borrow().proto.clone();
            let mut p = proto;
            while let Some(pr) = p {
                let b = pr.borrow();
                if let Some(v) = b.get(key) {
                    return Ok(v.clone());
                }
                p = b.proto.clone();
            }
            return Ok(Value::Undefined);
        }
        if let Some(pc) = parent_ctor {
            // 静态方法里的 super.x：查父构造器（含其静态继承链）。
            return self.get_prop(&pc, key);
        }
        Err(ref_err("super property access outside of method"))
    }

    /// `super(...args)`：派生类构造器调用父构造器，在当前 this 上初始化。
    pub(crate) fn eval_super_call(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let (super_ctor, this_class) = {
            let f = self
                .func_stack
                .last()
                .cloned()
                .ok_or_else(|| ref_err("super() outside of class constructor"))?;
            match &f.super_ctor {
                Some(sc) => (sc.clone(), f.clone()),
                None => return Err(ref_err("'super' call in non-derived constructor")),
            }
        };
        let this_val = self
            .this_stack
            .last()
            .cloned()
            .unwrap_or(Value::Undefined);
        // 父构造器在同一 this 上跑（字段/方法初始化到 obj）。
        let this_obj = match &super_ctor {
            Value::Function(pf) => {
                if pf.is_class {
                    // 父类也是类：基类先装字段；派生父类由递归处理。
                    // 注意：不经过 construct_class（避免重建 obj）；直接调 body。
                    if pf.super_ctor.is_none() {
                        self.init_instance_fields(pf, &expect_obj(&this_val)?)?;
                    }
                    let r = self.call_function(pf, this_val.clone(), args, None)?;
                    // 父类构造器返回对象则采用（规范）；否则沿用 this。
                    match r {
                        Value::Object(o) => Value::Object(o),
                        _ => this_val.clone(),
                    }
                } else {
                    // 父类是普通函数：this 绑定调用（子集简化；精确的
                    // new 语义为 TODO）。
                    let r = self.call_function(pf, this_val.clone(), args, None)?;
                    match r {
                        Value::Object(o) => Value::Object(o),
                        _ => this_val.clone(),
                    }
                }
            }
            Value::Object(o) if self.error_ctor_kind(o).is_some() => {
                // class X extends TypeError：构造错误对象并把 this 换成它。
                self.construct_error(self.error_ctor_kind(o).unwrap(), args)?
            }
            Value::Proxy(p) => self.proxy_construct(p, args)?,
            _ => return Err(type_err("super constructor is not constructible")),
        };
        // 派生类自己的字段在 super() 后初始化（在最终的 this 上）。
        self.init_instance_fields(&this_class, &expect_obj(&this_obj)?)?;
        // super 若返回了不同对象，this 重绑定到它（规范语义）。
        if let Some(top) = self.this_stack.last_mut() {
            *top = this_obj.clone();
        }
        Ok(this_obj)
    }

    /// `obj.#x` 私有成员访问。
    /// 实例私有：权限检查后沿原型链找 privates（字段在实例上，方法在原型上）。
    /// 静态私有：`C.#x`，在构造器（及父构造器链）的 private_statics 里找。
    pub(crate) fn get_private(&mut self, base: &Value, name: &str) -> Result<Value, FlowError> {
        let func = self
            .func_stack
            .last()
            .cloned()
            .ok_or_else(|| type_err(format!("private member #{} accessed outside class", name)))?;
        let scope = func.private_scope.clone().ok_or_else(|| {
            type_err(format!("private member #{} accessed outside class", name))
        })?;
        let key = (scope.class_id, istr(name));
        // 静态私有：base 为构造器函数。
        if let Value::Function(f) = base {
            if !scope.static_names.iter().any(|n| n == name) {
                return Err(type_err(format!(
                    "private member #{} is not a static private of this class",
                    name
                )));
            }
            let mut cur: Option<FuncRef> = Some(f.clone());
            while let Some(cf) = cur {
                if let Some(v) = cf.private_statics.borrow().get(&key) {
                    return Ok(v.clone());
                }
                cur = match &cf.super_ctor {
                    Some(Value::Function(pf)) => Some(pf.clone()),
                    _ => None,
                };
            }
            return Err(type_err(format!(
                "private member #{} not found on constructor",
                name
            )));
        }
        // 实例私有：权限检查。
        if !scope.instance_names.iter().any(|n| n == name) {
            return Err(type_err(format!(
                "private member #{} not declared in this class",
                name
            )));
        }
        // 沿原型链找（实例字段在自身，私有方法在 prototype 上）。
        let mut link: Option<ObjectRef> = match base {
            Value::Object(o) => Some(o.clone()),
            _ => {
                return Err(type_err(format!(
                    "cannot read private member #{} of {}",
                    name,
                    base.type_of()
                )))
            }
        };
        while let Some(pr) = link {
            let b = pr.borrow();
            if let Some(v) = b.privates.get(&key) {
                return Ok(v.clone());
            }
            link = b.proto.clone();
        }
        Err(type_err(format!(
            "private member #{} not found on object",
            name
        )))
    }

    /// `obj.#x = v` 私有成员赋值（权限检查同 get_private）。
    pub(crate) fn set_private(&mut self, base: &Value, name: &str, val: Value) -> Result<(), FlowError> {
        let func = self
            .func_stack
            .last()
            .cloned()
            .ok_or_else(|| type_err(format!("private member #{} assigned outside class", name)))?;
        let scope = func.private_scope.clone().ok_or_else(|| {
            type_err(format!("private member #{} assigned outside class", name))
        })?;
        let key = (scope.class_id, istr(name));
        // 静态私有赋值：base 为构造器函数。
        if let Value::Function(f) = base {
            if !scope.static_names.iter().any(|n| n == name) {
                return Err(type_err(format!(
                    "private member #{} is not a static private of this class",
                    name
                )));
            }
            f.private_statics.borrow_mut().insert(key, val);
            return Ok(());
        }
        // 实例私有：权限检查后沿原型链找已有槽位（字段在实例上）。
        if !scope.instance_names.iter().any(|n| n == name) {
            return Err(type_err(format!(
                "private member #{} not declared in this class",
                name
            )));
        }
        let obj = expect_obj(base)?;
        // 私有方法不可赋值（规范）；子集里方法槽位在原型上，只允许写字段槽位。
        {
            let b = obj.borrow();
            if b.privates.contains_key(&key) {
                // 已有槽位：直接写。
                drop(b);
                obj.borrow_mut().privates.insert(key, val);
                return Ok(());
            }
        }
        // 无槽位：沿原型链确认是方法（不可写）还是确实缺失。
        let mut link: Option<ObjectRef> = obj.borrow().proto.clone();
        while let Some(pr) = link {
            let b = pr.borrow();
            if b.privates.contains_key(&key) {
                return Err(type_err(format!(
                    "cannot assign to private method #{}",
                    name
                )));
            }
            link = b.proto.clone();
        }
        Err(type_err(format!(
            "private member #{} not found on object",
            name
        )))
    }

    /// `#x in obj` 私有品牌检查（沿原型链）。
    pub(crate) fn private_in(&mut self, name: &str, obj: &Value) -> Result<Value, FlowError> {
        let func = self
            .func_stack
            .last()
            .cloned()
            .ok_or_else(|| type_err(format!("private #{} in outside class", name)))?;
        let scope = func.private_scope.clone().ok_or_else(|| {
            type_err(format!("private #{} in outside class", name))
        })?;
        let key = (scope.class_id, istr(name));
        let mut link: Option<ObjectRef> = match obj {
            Value::Object(o) => Some(o.clone()),
            _ => return Ok(Value::Bool(false)),
        };
        while let Some(pr) = link {
            let b = pr.borrow();
            if b.privates.contains_key(&key) {
                return Ok(Value::Bool(true));
            }
            link = b.proto.clone();
        }
        Ok(Value::Bool(false))
    }

    // ------------------------------------------------------------------
    // 内置对象安装
    // ------------------------------------------------------------------

    fn install_builtins(&mut self) {
        self.define_global("undefined", Value::Undefined);

        // console
        let console_obj = Rc::new(RefCell::new(JsObject::new()));
        for m in ["log", "error", "warn"] {
            console_obj
                .borrow_mut()
                .set(m, Value::Native(native_console_log));
        }
        self.define_global("console", Value::Object(console_obj));

        // Math
        let math = Rc::new(RefCell::new(JsObject::new()));
        {
            let mut m = math.borrow_mut();
            m.set("PI", Value::Number(std::f64::consts::PI));
            m.set("E", Value::Number(std::f64::consts::E));
            let fns: &[(&str, NativeFn)] = &[
                ("abs", math_abs),
                ("floor", math_floor),
                ("ceil", math_ceil),
                ("round", math_round),
                ("sqrt", math_sqrt),
                ("max", math_max),
                ("min", math_min),
                ("pow", math_pow),
                ("random", math_random),
            ];
            for (name, f) in fns {
                m.set(name, Value::Native(*f));
            }
        }
        self.define_global("Math", Value::Object(math));

        // JSON
        let json = Rc::new(RefCell::new(JsObject::new()));
        json.borrow_mut()
            .set("stringify", Value::Native(native_json_stringify));
        json.borrow_mut()
            .set("parse", Value::Native(native_json_parse));
        self.define_global("JSON", Value::Object(json));

        // 全局函数
        self.define_global("parseInt", Value::Native(native_parse_int));
        self.define_global("parseFloat", Value::Native(native_parse_float));
        self.define_global("isNaN", Value::Native(native_is_nan));
        // Phase 14：`eval`（直接/间接调用分别在 eval_call / call_value_at 拦截）。
        self.define_global("eval", Value::Native(native_eval));

        // phase 4：原型链标准库（Array/String/Object/Number 方法 + 全局构造器）。
        self.install_stdlib();
    }

    // ------------------------------------------------------------------
    // Phase 8：错误构造器（Error / TypeError / ReferenceError /
    // SyntaxError / RangeError）+ 调用栈
    // ------------------------------------------------------------------

    /// 安装错误构造器与原型链：
    /// `TypeError.prototype` → `Error.prototype` → `Object.prototype`，
    /// 构造器为普通 Object（`call_value_at` / `eval_new` 按指针识别）。
    fn install_errors(&mut self) {
        let object_proto = self.protos.object.clone();
        let function_proto = self.protos.function.clone();

        let error_proto = Rc::new(RefCell::new(JsObject::with_proto(Some(object_proto))));
        error_proto.borrow_mut().set("name", Value::String("Error".to_string()));
        error_proto
            .borrow_mut()
            .set("message", Value::String(String::new()));

        for kind in [
            ErrorKind::Error,
            ErrorKind::TypeError,
            ErrorKind::ReferenceError,
            ErrorKind::SyntaxError,
            ErrorKind::RangeError,
        ] {
            let proto = if kind == ErrorKind::Error {
                error_proto.clone()
            } else {
                let p = Rc::new(RefCell::new(JsObject::with_proto(Some(
                    error_proto.clone(),
                ))));
                p.borrow_mut()
                    .set("name", Value::String(kind.name().to_string()));
                p
            };
            let ctor = Rc::new(RefCell::new(JsObject::new()));
            {
                let mut o = ctor.borrow_mut();
                o.proto = Some(function_proto.clone());
                o.set("prototype", Value::Object(proto.clone()));
                // Phase 14：`TypeError.name === "TypeError"`（函数名属性）。
                o.set("name", Value::String(kind.name().to_string()));
            }
            // Phase 14：`e.constructor === TypeError`（经原型链解析）。
            proto
                .borrow_mut()
                .set("constructor", Value::Object(ctor.clone()));
            self.error_protos.insert(kind, proto);
            self.error_ctors.push((kind, ctor.clone()));
            self.define_global(kind.name(), Value::Object(ctor));
        }
    }

    /// `new Error("msg")` / `Error("msg")`：构造错误对象，自带
    /// `message` / `name` / `stack`（构造时的调用栈快照）。
    fn construct_error(
        &mut self,
        kind: ErrorKind,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let message = args.first().map(|v| v.to_js_string()).unwrap_or_default();
        Ok(self.make_error_object(kind, &message, &self.call_stack.clone()))
    }

    /// 由 kind/message/调用栈帧构造错误对象值（拒绝值转换等共用）。
    fn make_error_object(
        &self,
        kind: ErrorKind,
        message: &str,
        frames: &[Frame],
    ) -> Value {
        let proto = self.error_protos.get(&kind).cloned();
        let obj = Rc::new(RefCell::new(JsObject::with_proto(proto)));
        {
            let mut o = obj.borrow_mut();
            // tag 标记供 `to_js_string` 渲染 `TypeError: msg` 用。
            o.tag = Some("Error".to_string());
            o.set("message", Value::String(message.to_string()));
            o.set("name", Value::String(kind.name().to_string()));
            o.set("stack", Value::String(Self::format_stack(kind.name(), message, frames)));
        }
        Value::Object(obj)
    }

    /// phase 8：`FlowError` 转拒绝值——`throw` 的值原样；运行时错误转为
    /// 对应子类的 Error 对象（带 kind/message/stack，不再是裸字符串）。
    /// phase 14：改为 `pub(crate)`，供 VM 的 catch 处理共用。
    pub(crate) fn flow_error_to_value(&mut self, e: FlowError) -> Value {
        match e {
            FlowError::Thrown(v) => v,
            FlowError::Runtime(r) => {
                let frames = r.stack.clone().unwrap_or_default();
                self.make_error_object(r.kind, &r.message, &frames)
            }
        }
    }

    /// `ErrorName: message\n    at fn (<anonymous>:line:col)…`（内层在前）。
    fn format_stack(name: &str, message: &str, frames: &[Frame]) -> String {
        let mut s = format!("{name}: {message}");
        for fr in frames.iter().rev() {
            s.push_str(&format!(
                "\n    at {} (<anonymous>:{}:{})",
                fr.name, fr.line, fr.col
            ));
        }
        s
    }

    // ------------------------------------------------------------------
    // Phase 7：Promise / 微任务 / fetch / 正则 / 模块
    // ------------------------------------------------------------------

    /// 安装 `Promise` / `RegExp` 构造器与原型、`fetch`、`queueMicrotask`。
    fn install_async(&mut self) {
        let promise_proto = self.protos.promise.clone();
        let regexp_proto = self.protos.regexp.clone();

        // ---- Promise.prototype ----
        {
            let mut p = promise_proto.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("then", native_promise_then),
                ("catch", native_promise_catch),
                ("finally", native_promise_finally),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }

        // ---- Promise 构造器：`new Promise(exec)` 由 eval_new 拦截，
        // ---- 不走 Native construct；直接调用当普通函数则报错。
        let promise_ctor =
            Rc::new(RefCell::new(JsObject::with_proto(Some(promise_proto))));
        {
            let mut c = promise_ctor.borrow_mut();
            c.set("resolve", Value::Native(native_promise_resolve));
            c.set("reject", Value::Native(native_promise_reject));
            c.set("all", Value::Native(native_promise_all));
            c.set("race", Value::Native(native_promise_race));
        }
        self.promise_ctor = Some(promise_ctor.clone());
        self.define_global("Promise", Value::Object(promise_ctor));

        // ---- RegExp.prototype ----
        {
            let mut p = regexp_proto.borrow_mut();
            p.set("test", Value::Native(native_regexp_test));
            p.set("exec", Value::Native(native_regexp_exec));
        }

        // ---- RegExp 构造器：`new RegExp(...)` / `RegExp(...)` 由解释器处理。
        let regexp_ctor =
            Rc::new(RefCell::new(JsObject::with_proto(Some(regexp_proto))));
        self.regexp_ctor = Some(regexp_ctor.clone());
        self.define_global("RegExp", Value::Object(regexp_ctor));

        // ---- 全局 fetch / queueMicrotask ----
        self.define_global("fetch", Value::Native(native_fetch));
        self.define_global(
            "queueMicrotask",
            Value::Native(native_queue_microtask),
        );
    }

    /// 宿主绑定 fetch 实现（engine/ 侧提供真实网络；不绑定则注定 reject）。
    pub fn bind_fetch(&mut self, host: FetchHostRef) {
        self.fetch_host = Some(host);
    }

    /// 注册一个模块（name → 源码）。`import 'name'` 时按依赖序求值。
    pub fn register_module(&mut self, name: &str, source: &str) {
        self.modules.insert(name.to_string(), source.to_string());
    }

    /// 以 fulfilled 结算 promise（反应转微任务）。
    fn settle_fulfilled(&mut self, p: &PromiseRef, v: Value) {
        settle_promise(p, false, v, &mut self.microtasks);
    }

    /// 以 rejected 结算 promise；无人处理则记入 unhandled（运行结束时报告）。
    fn settle_rejected(&mut self, p: &PromiseRef, v: Value) {
        let (_, unhandled) = settle_promise(p, true, v, &mut self.microtasks);
        if unhandled {
            self.unhandled.push(p.clone());
        }
    }

    /// 把 `outer` 的命运交给 promise `p`（`then` 回调返回 Promise 时的同化，
    /// 以及 async 链的解包）。自指则 reject（规范 TypeError 的简化版）。
    fn adopt_promise(&mut self, p: &PromiseRef, outer: &PromiseRef) {
        if Rc::ptr_eq(p, outer) {
            self.settle_rejected(
                outer,
                Value::String("TypeError: promise chaining cycle".to_string()),
            );
            return;
        }
        let r = Reaction {
            kind: ReactionKind::AsyncUnwrap {
                outer: outer.clone(),
            },
            next: None,
            source: p.clone(),
        };
        let was_rejected = matches!(p.borrow().state, PromiseState::Rejected(_));
        let still_unhandled = attach_reaction(p, r, &mut self.microtasks);
        if was_rejected && !still_unhandled {
            self.unhandled.retain(|x| !Rc::ptr_eq(x, p));
        }
    }

    /// 用数组元素构造 JS 数组值（`Promise.all` 结果用）。
    fn make_array_value(&self, elems: Vec<Value>) -> Value {
        Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
            elems,
            Some(self.protos.array.clone()),
        ))))
    }

    /// 执行一个已决定的反应（微任务体）。回调抛错 → 下游 reject 并继续。
    fn dispatch_reaction(
        &mut self,
        reaction: &Reaction,
        value: Value,
        rejected: bool,
    ) -> Result<(), FlowError> {
        match &reaction.kind {
            ReactionKind::Handler {
                on_fulfilled,
                on_rejected,
                is_finally,
            } => {
                let next = reaction
                    .next
                    .clone()
                    .expect("handler reaction always has next");
                // finally：回调不收参数，原值/原因透传；回调抛错则下游 reject。
                if *is_finally {
                    let cb = on_fulfilled.as_ref().or(on_rejected.as_ref()).cloned();
                    match cb {
                        Some(cb) => {
                            match self.call_value(cb, Value::Undefined, vec![]) {
                                Ok(_) => {
                                    if rejected {
                                        self.settle_rejected(&next, value);
                                    } else {
                                        self.settle_fulfilled(&next, value);
                                    }
                                }
                                Err(e) => {
                                    let rv = self.flow_error_to_value(e);
                                    self.settle_rejected(&next, rv)
                                }
                            }
                        }
                        None => {
                            if rejected {
                                self.settle_rejected(&next, value);
                            } else {
                                self.settle_fulfilled(&next, value);
                            }
                        }
                    }
                    return Ok(());
                }
                let handler = if rejected {
                    on_rejected.clone()
                } else {
                    on_fulfilled.clone()
                };
                match handler {
                    Some(h) => match self.call_value(h, Value::Undefined, vec![value]) {
                        Ok(v) => {
                            // thenable 同化：只认 Value::Promise（文档化简化）。
                            if let Value::Promise(p) = v {
                                self.adopt_promise(&p, &next);
                            } else {
                                self.settle_fulfilled(&next, v);
                            }
                        }
                        Err(e) => {
                            let rv = self.flow_error_to_value(e);
                            self.settle_rejected(&next, rv)
                        },
                    },
                    // 无对应处理器：原样透传。拒绝的"携带者"变为 next，
                    // source 从 unhandled 移除（next 另行记账）。
                    None => {
                        if rejected {
                            self.unhandled
                                .retain(|x| !Rc::ptr_eq(x, &reaction.source));
                            self.settle_rejected(&next, value);
                        } else {
                            self.settle_fulfilled(&next, value);
                        }
                    }
                }
                Ok(())
            }
            ReactionKind::All { state, index } => {
                enum AllOutcome {
                    Rejected { result: PromiseRef, reason: Value },
                    Fulfilled { result: PromiseRef, vals: Vec<Value> },
                    Pending,
                }
                let outcome = {
                    let mut st = state.borrow_mut();
                    if st.done {
                        AllOutcome::Pending
                    } else if rejected {
                        st.done = true;
                        AllOutcome::Rejected {
                            result: st.result.clone(),
                            reason: value,
                        }
                    } else {
                        st.values[*index] = Some(value);
                        st.remaining -= 1;
                        if st.remaining == 0 {
                            st.done = true;
                            let vals: Vec<Value> = st
                                .values
                                .iter()
                                .map(|o| o.clone().unwrap_or(Value::Undefined))
                                .collect();
                            AllOutcome::Fulfilled {
                                result: st.result.clone(),
                                vals,
                            }
                        } else {
                            AllOutcome::Pending
                        }
                    }
                };
                match outcome {
                    AllOutcome::Rejected { result, reason } => {
                        // 拒绝被 all 观测并转发：source 不再是 unhandled 候选。
                        self.unhandled
                            .retain(|x| !Rc::ptr_eq(x, &reaction.source));
                        self.settle_rejected(&result, reason);
                    }
                    AllOutcome::Fulfilled { result, vals } => {
                        let arr = self.make_array_value(vals);
                        self.settle_fulfilled(&result, arr);
                    }
                    AllOutcome::Pending => {}
                }
                Ok(())
            }
            ReactionKind::Race { result } => {
                let result = result.clone();
                if rejected {
                    self.unhandled
                        .retain(|x| !Rc::ptr_eq(x, &reaction.source));
                    self.settle_rejected(&result, value);
                } else {
                    self.settle_fulfilled(&result, value);
                }
                Ok(())
            }
            ReactionKind::AsyncUnwrap { outer } => {
                let outer = outer.clone();
                if rejected {
                    self.unhandled
                        .retain(|x| !Rc::ptr_eq(x, &reaction.source));
                    self.settle_rejected(&outer, value);
                } else if let Value::Promise(p) = value {
                    // 嵌套 Promise：继续解包。
                    self.adopt_promise(&p, &outer);
                } else {
                    self.settle_fulfilled(&outer, value);
                }
                Ok(())
            }
            ReactionKind::AsyncGenDone { agen } => {
                let agen = agen.clone();
                self.async_gen_complete(&agen, value, rejected);
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 7：模块系统（两遍式：先收集 export，再按依赖序求值）
    // ------------------------------------------------------------------

    /// 运行一个已注册模块并返回其导出表。依赖 DFS + 环检测（报错）。
    /// 结果缓存；顶层 await 不要求。
    fn run_module(
        &mut self,
        name: &str,
    ) -> Result<HashMap<String, Value>, FlowError> {
        if let Some(cached) = self.module_cache.get(name) {
            return Ok(cached.clone());
        }
        if self.module_stack.contains(&name.to_string()) {
            let mut chain = self.module_stack.clone();
            chain.push(name.to_string());
            return Err(rt(format!(
                "circular module dependency: {}",
                chain.join(" -> ")
            )));
        }
        let source = match self.modules.get(name) {
            Some(s) => s.clone(),
            None => return Err(rt(format!("module not found: '{name}'"))),
        };
        self.module_stack.push(name.to_string());
        let result = self.run_module_inner(name, &source);
        self.module_stack.pop();
        // 求值失败不缓存（下次 import 重试，行为可预测）。
        let exports = result?;
        self.module_cache.insert(name.to_string(), exports.clone());
        Ok(exports)
    }

    fn run_module_inner(
        &mut self,
        name: &str,
        source: &str,
    ) -> Result<HashMap<String, Value>, FlowError> {
        let prog = parse_source(source).map_err(|e| rt(format!("{e}")))?;
        // 第一遍：收集本模块的 export 声明（供 `export {a}` 这种后置形式）。
        let mut export_names: Vec<(String, String)> = Vec::new(); // (exported, local)
        for stmt in &prog.body {
            match &stmt.node {
                StmtKind::ExportNames(names) => {
                    export_names.extend(names.iter().cloned());
                }
                StmtKind::ExportDecl { decls, .. } => {
                    for d in decls {
                        export_names.push((d.id.clone(), d.id.clone()));
                    }
                }
                StmtKind::ExportFunc(f) => {
                    if let Some(id) = &f.id {
                        export_names.push((id.clone(), id.clone()));
                    }
                }
                _ => {}
            }
        }
        // 模块作用域：函数声明提升 + import 依赖先求值。
        let env = Env::child(&self.global);
        self.hoist(&prog.body, &env)?;
        // import 语句：依赖求值后把绑定快照为模块级 const。
        for stmt in &prog.body {
            if let StmtKind::Import { specs, source } = &stmt.node {
                let dep_exports = self.run_module(source)?;
                for spec in specs {
                    match dep_exports.get(&spec.imported) {
                        Some(v) => {
                            Env::declare_lexical(
                                &env,
                                &spec.local,
                                DeclKind::Const,
                            )?;
                            Env::init_lexical(&env, &spec.local, v.clone())?;
                        }
                        None => {
                            return Err(syntax_err(format!(
                                "module '{source}' has no export '{imported}'",
                                imported = spec.imported
                            )))
                        }
                    }
                }
            }
        }
        // 第二遍：执行（export 语句本身把值写入导出表）。
        let mut exports: HashMap<String, Value> = HashMap::new();
        for stmt in &prog.body {
            match &stmt.node {
                StmtKind::Import { .. } => {}
                StmtKind::ExportDecl { kind, decls } => {
                    // 复用普通变量声明的执行逻辑（合成一条 VarDecl 语句）。
                    let synthetic = Stmt {
                        node: StmtKind::VarDecl {
                            kind: kind.clone(),
                            decls: decls.clone(),
                        },
                        span: stmt.span.clone(),
                    };
                    match self.exec_stmt(&synthetic, env.clone(), &env)? {
                        Signal::Normal(_) => {}
                        _ => {
                            return Err(rt("unexpected control signal in module"));
                        }
                    }
                    for d in decls {
                        let v =
                            Env::lookup(&env, &d.id).ok().flatten().unwrap_or(Value::Undefined);
                        exports.insert(d.id.clone(), v);
                    }
                }
                StmtKind::ExportFunc(f) => {
                    // 已由 hoist 提升；取值写入导出表。
                    let id = f.id.clone().unwrap_or_default();
                    let v = Env::lookup(&env, &id).ok().flatten().unwrap_or(Value::Undefined);
                    exports.insert(id, v);
                }
                StmtKind::ExportNames(names) => {
                    for (exported, local) in names {
                        let v = Env::lookup(&env, local).ok().flatten().unwrap_or(Value::Undefined);
                        exports.insert(exported.clone(), v);
                    }
                }
                _ => {
                    match self.exec_stmt(stmt, env.clone(), &env)? {
                        Signal::Normal(_) => {}
                        Signal::Return(_) => {
                            return Err(syntax_err(format!(
                                "return outside of function in module '{name}'"
                            )))
                        }
                        Signal::Break => {
                            return Err(syntax_err(format!(
                                "break outside of loop in module '{name}'"
                            )))
                        }
                        Signal::Continue => {
                            return Err(syntax_err(format!(
                                "continue outside of loop in module '{name}'"
                            )))
                        }
                    }
                }
            }
        }
        // `export {a}` 后置形式：以执行后的值覆盖（快照语义，文档化）。
        for (exported, local) in &export_names {
            if !exports.contains_key(exported) {
                let v = Env::lookup(&env, local).ok().flatten().unwrap_or(Value::Undefined);
                exports.insert(exported.clone(), v);
            }
        }
        Ok(exports)
    }

    /// 主程序顶层的 `import`：依赖求值后把绑定快照为全局 const。
    /// （模块内的 import 由 run_module_inner 处理。）
    fn process_imports(
        &mut self,
        stmts: &[Stmt],
        global: &EnvRef,
    ) -> Result<(), FlowError> {
        for stmt in stmts {
            if let StmtKind::Import { specs, source } = &stmt.node {
                let dep_exports = self.run_module(source)?;
                for spec in specs {
                    match dep_exports.get(&spec.imported) {
                        Some(v) => {
                            Env::declare_lexical(
                                global,
                                &spec.local,
                                DeclKind::Const,
                            )?;
                            Env::init_lexical(global, &spec.local, v.clone())?;
                        }
                        None => {
                            return Err(syntax_err(format!(
                                "module '{source}' has no export '{imported}'",
                                imported = spec.imported
                            )))
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `String.prototype.replace` 的解释器实现（try_host_method 拦截后走这里）。
    /// 支持正则/字符串 pattern；replacer 为函数时逐个调用
    /// `replacer(match, p1…, offset, string)`；字符串 replacer 中的 `$` 模式
    /// 暂不支持，按字面量处理（文档化）。
    fn string_replace_impl(
        &mut self,
        base: Value,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let s = match &base {
            Value::String(s) => s.clone(),
            v => v.to_js_string(),
        };
        let pattern = args.first().cloned().unwrap_or(Value::Undefined);
        let replacer = args.get(1).cloned().unwrap_or(Value::Undefined);
        let replacer_is_fn = replacer.is_callable();

        struct Sub {
            start: usize,
            end: usize,
            text: String,
            groups: Vec<Option<String>>,
        }
        let subs: Vec<Sub> = match &pattern {
            Value::RegExp(r) => {
                let b = r.borrow();
                let global = b.compiled.flags.global;
                let mut out = Vec::new();
                let mut from = 0usize;
                let len = s.chars().count();
                loop {
                    if from > len {
                        break;
                    }
                    match b.compiled.search(&s, from) {
                        Some(m) => {
                            let start = m.index;
                            let end = start + m.text.chars().count();
                            out.push(Sub {
                                start,
                                end,
                                text: m.text,
                                groups: m.groups,
                            });
                            if !global {
                                break;
                            }
                            from = if end == from { from + 1 } else { end };
                        }
                        None => break,
                    }
                }
                out
            }
            _ => {
                let pat = pattern.to_js_string();
                if pat.is_empty() {
                    vec![Sub {
                        start: 0,
                        end: 0,
                        text: String::new(),
                        groups: Vec::new(),
                    }]
                } else {
                    match s.find(pat.as_str()) {
                        Some(byte_idx) => {
                            let start = s[..byte_idx].chars().count();
                            vec![Sub {
                                start,
                                end: start + pat.chars().count(),
                                text: pat,
                                groups: Vec::new(),
                            }]
                        }
                        None => Vec::new(),
                    }
                }
            }
        };

        if subs.is_empty() {
            return Ok(Value::String(s));
        }
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut last = 0usize;
        for sub in &subs {
            out.extend(chars[last..sub.start].iter());
            let rep = if replacer_is_fn {
                let mut call_args = vec![Value::String(sub.text.clone())];
                for g in &sub.groups {
                    call_args.push(
                        g.clone().map(Value::String).unwrap_or(Value::Undefined),
                    );
                }
                call_args.push(Value::Number(sub.start as f64));
                call_args.push(Value::String(s.clone()));
                self.call_value(replacer.clone(), Value::Undefined, call_args)?
                    .to_js_string()
            } else {
                replacer.to_js_string()
            };
            out.push_str(&rep);
            last = sub.end;
        }
        out.extend(chars[last..].iter());
        Ok(Value::String(out))
    }

    // ------------------------------------------------------------------
    // Phase 4：标准库安装（原型方法 + 全局构造器）
    // ------------------------------------------------------------------

    fn install_stdlib(&mut self) {
        let object_proto = self.protos.object.clone();
        let array_proto = self.protos.array.clone();
        let string_proto = self.protos.string.clone();
        let function_proto = self.protos.function.clone();

        // ---- Object.prototype ----
        // Phase 14：`toString`/`valueOf`（ToPrimitive 依赖它们）。
        {
            let mut p = object_proto.borrow_mut();
            p.set("toString", Value::Native(native_object_to_string));
            p.set("valueOf", Value::Native(native_object_value_of));
        }

        // ---- Array.prototype ----
        {
            let mut p = array_proto.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("push", array_push),
                ("pop", array_pop),
                ("shift", array_shift),
                ("unshift", array_unshift),
                ("join", array_join),
                // Phase 14：`toString`（ToPrimitive 用；覆盖 Object.prototype 的）。
                ("toString", native_array_to_string),
                ("slice", array_slice),
                ("concat", array_concat),
                ("indexOf", array_index_of),
                ("includes", array_includes),
                // 高阶函数的方法调用走 eval_call 拦截（需回调进解释器）；
                // 这里挂桩以便 `typeof arr.map === 'function'` 等属性访问正常。
                ("map", intercepted_method_stub),
                ("filter", intercepted_method_stub),
                ("forEach", intercepted_method_stub),
                ("find", intercepted_method_stub),
                ("reduce", intercepted_method_stub),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }

        // ---- String.prototype ----
        {
            let mut p = string_proto.borrow_mut();
            let methods: &[(&str, NativeFn)] = &[
                ("charAt", string_char_at),
                ("charCodeAt", string_char_code_at),
                ("slice", string_slice),
                ("substring", string_substring),
                ("indexOf", string_index_of),
                ("includes", string_includes),
                ("split", string_split),
                ("match", string_match),
                ("replace", string_replace),
                ("trim", string_trim),
                ("toUpperCase", string_to_upper_case),
                ("toLowerCase", string_to_lower_case),
                ("startsWith", string_starts_with),
                ("endsWith", string_ends_with),
            ];
            for (name, f) in methods {
                p.set(name, Value::Native(*f));
            }
        }

        // ---- 全局构造器（持有 .prototype 与静态方法）----
        // Object
        let object_ctor = Rc::new(RefCell::new(JsObject::new()));
        {
            let mut o = object_ctor.borrow_mut();
            o.proto = Some(function_proto.clone());
            o.set("prototype", Value::Object(object_proto.clone()));
            o.set("keys", Value::Native(object_keys));
            o.set("values", Value::Native(object_values));
            o.set("entries", Value::Native(object_entries));
            o.set("assign", Value::Native(object_assign));
            // phase 9：类继承测试需要。
            o.set("getPrototypeOf", Value::Native(object_get_prototype_of));
        }
        self.define_global("Object", Value::Object(object_ctor));

        // Array
        let array_ctor = Rc::new(RefCell::new(JsObject::new()));
        {
            let mut o = array_ctor.borrow_mut();
            o.proto = Some(function_proto.clone());
            o.set("prototype", Value::Object(array_proto.clone()));
            o.set("isArray", Value::Native(array_is_array));
        }
        self.define_global("Array", Value::Object(array_ctor));

        // String
        let string_ctor = Rc::new(RefCell::new(JsObject::new()));
        {
            let mut o = string_ctor.borrow_mut();
            o.proto = Some(function_proto.clone());
            o.set("prototype", Value::Object(string_proto.clone()));
        }
        self.define_global("String", Value::Object(string_ctor));

        // Number
        let number_ctor = Rc::new(RefCell::new(JsObject::new()));
        {
            let mut o = number_ctor.borrow_mut();
            o.proto = Some(function_proto.clone());
            o.set("prototype", Value::Object(self.protos.number.clone()));
            o.set("isInteger", Value::Native(number_is_integer));
            o.set("isNaN", Value::Native(number_is_nan));
        }
        self.define_global("Number", Value::Object(number_ctor));

        // Function
        let function_ctor = Rc::new(RefCell::new(JsObject::new()));
        {
            let mut o = function_ctor.borrow_mut();
            o.proto = Some(function_proto.clone());
            o.set("prototype", Value::Object(function_proto.clone()));
            // call/apply 的方法调用走 eval_call 拦截；挂桩保证属性访问正常。
            function_proto
                .borrow_mut()
                .set("call", Value::Native(intercepted_method_stub));
            function_proto
                .borrow_mut()
                .set("apply", Value::Native(intercepted_method_stub));
        }
        self.define_global("Function", Value::Object(function_ctor));
    }

    // ------------------------------------------------------------------
    // Phase 4：数组高阶函数（回调需进解释器，eval_call 拦截分发）
    // ------------------------------------------------------------------

    fn array_higher_order(
        &mut self,
        arr: &ArrayRef,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err(format!("{} called with non-function callback", name)));
        }
        // phase 12：不再整体克隆 `elems`（大数组时峰值内存翻倍）；
        // 长度取一次（规范：回调中 push 不影响访问范围），每次按下标取
        // 一个元素（短借用，回调内可安全读写数组；删掉的下标读到 undefined，
        // 与规范 `Get(O, Pk)` 一致）。
        let len = arr.borrow().elems.len();
        let arr_val = Value::Array(arr.clone());
        let new_array = |ctx_self: &mut Self, elems: Vec<Value>| {
            Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                elems,
                Some(ctx_self.protos.array.clone()),
            ))))
        };
        // 按下标取第 i 个元素（短借用后即释放）。
        let at = |arr: &ArrayRef, i: usize| -> Value {
            arr.borrow()
                .elems
                .get(i)
                .cloned()
                .unwrap_or(Value::Undefined)
        };
        match name {
            "map" => {
                let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
                let mut out = Vec::with_capacity(len);
                for i in 0..len {
                    let e = at(arr, i);
                    out.push(self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e, Value::Number(i as f64), arr_val.clone()],
                    )?);
                }
                Ok(new_array(self, out))
            }
            "filter" => {
                let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
                let mut out = Vec::new();
                for i in 0..len {
                    let e = at(arr, i);
                    let keep = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e.clone(), Value::Number(i as f64), arr_val.clone()],
                    )?;
                    if keep.to_boolean() {
                        out.push(e);
                    }
                }
                Ok(new_array(self, out))
            }
            "forEach" => {
                let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
                for i in 0..len {
                    let e = at(arr, i);
                    self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e, Value::Number(i as f64), arr_val.clone()],
                    )?;
                }
                Ok(Value::Undefined)
            }
            "find" => {
                let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
                for i in 0..len {
                    let e = at(arr, i);
                    let hit = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e.clone(), Value::Number(i as f64), arr_val.clone()],
                    )?;
                    if hit.to_boolean() {
                        return Ok(e);
                    }
                }
                Ok(Value::Undefined)
            }
            "reduce" => {
                // reduce(cb, initial?)：无初值时用首元素（空数组无初值则报错）。
                let mut acc = match args.get(1) {
                    Some(v) => v.clone(),
                    None => {
                        if len == 0 {
                            return Err(FlowError::Runtime(RuntimeError::new(
                                "reduce of empty array with no initial value",
                            )));
                        }
                        at(arr, 0)
                    }
                };
                // 有初值时从下标 0 开始、无初值时从下标 1 开始（规范索引语义）。
                let start = if args.get(1).is_some() { 0 } else { 1 };
                for i in start..len {
                    let e = at(arr, i);
                    acc = self.call_value(
                        cb.clone(),
                        Value::Undefined,
                        vec![
                            acc,
                            e,
                            Value::Number(i as f64),
                            arr_val.clone(),
                        ],
                    )?;
                }
                Ok(acc)
            }
            _ => Err(rt(format!("unknown array method '{}'", name))),
        }
    }

    // ------------------------------------------------------------------
    // Phase 10：回调型方法（解释器驱动）
    // ------------------------------------------------------------------

    /// `Map.prototype.forEach(cb, thisArg?)`。
    fn map_for_each(&mut self, base: Value, args: Vec<Value>) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("forEach called with non-function callback"));
        }
        let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
        let entries = match &base {
            Value::Map(m) => m.borrow().entries.clone(),
            _ => return Err(type_err("forEach called on non-Map")),
        };
        for (k, v) in entries {
            self.call_value(
                cb.clone(),
                this_arg.clone(),
                vec![v, k, base.clone()],
            )?;
        }
        Ok(Value::Undefined)
    }

    /// `Set.prototype.forEach(cb, thisArg?)`。
    fn set_for_each(&mut self, base: Value, args: Vec<Value>) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("forEach called with non-function callback"));
        }
        let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
        let entries = match &base {
            Value::Set(s) => s.borrow().entries.clone(),
            _ => return Err(type_err("forEach called on non-Set")),
        };
        for v in entries {
            self.call_value(
                cb.clone(),
                this_arg.clone(),
                vec![v.clone(), v, base.clone()],
            )?;
        }
        Ok(Value::Undefined)
    }

    /// TypedArray 的 `forEach` / `map` / `filter`（回调进解释器）。
    fn ta_higher_order(
        &mut self,
        base: Value,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err(format!("{} called with non-function callback", name)));
        }
        let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
        let (kind, vals) = match &base {
            Value::TypedArray(t) => {
                let ta = t.borrow();
                let buf = ta.buffer.borrow();
                let bytes = buf.bytes.borrow();
                let vals: Vec<f64> = (0..ta.len).map(|i| ta.read_at(&bytes, i)).collect();
                (ta.kind, vals)
            }
            _ => return Err(type_err("method called on non-TypedArray")),
        };
        match name {
            "forEach" => {
                for (i, v) in vals.into_iter().enumerate() {
                    self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![
                            Value::Number(v),
                            Value::Number(i as f64),
                            base.clone(),
                        ],
                    )?;
                }
                Ok(Value::Undefined)
            }
            "map" => {
                let mut out = Vec::with_capacity(vals.len());
                for (i, v) in vals.into_iter().enumerate() {
                    let r = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![
                            Value::Number(v),
                            Value::Number(i as f64),
                            base.clone(),
                        ],
                    )?;
                    out.push(r.to_number());
                }
                let ta = ta_new(kind, out.len());
                {
                    let t = ta.borrow();
                    let tbuf = t.buffer.borrow();
                    let mut bytes = tbuf.bytes.borrow_mut();
                    for (i, v) in out.into_iter().enumerate() {
                        t.write_at(&mut bytes, i, v);
                    }
                }
                Ok(Value::TypedArray(ta))
            }
            "filter" => {
                let mut out = Vec::new();
                for (i, v) in vals.into_iter().enumerate() {
                    let keep = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![
                            Value::Number(v),
                            Value::Number(i as f64),
                            base.clone(),
                        ],
                    )?;
                    if keep.to_boolean() {
                        out.push(v);
                    }
                }
                let ta = ta_new(kind, out.len());
                {
                    let t = ta.borrow();
                    let tbuf = t.buffer.borrow();
                    let mut bytes = tbuf.bytes.borrow_mut();
                    for (i, v) in out.into_iter().enumerate() {
                        t.write_at(&mut bytes, i, v);
                    }
                }
                Ok(Value::TypedArray(ta))
            }
            _ => Err(rt(format!("unknown TypedArray method '{}'", name))),
        }
    }

    /// `Array.prototype.sort(comparator)`（带比较器；不带时走 native 默认）。
    fn array_sort_with_cmp(
        &mut self,
        arr: &ArrayRef,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let cb = args.into_iter().next().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("sort called with non-function comparator"));
        }
        let mut elems = arr.borrow().elems.clone();
        // 插入排序（稳定；n 通常不大）。
        for i in 1..elems.len() {
            let mut j = i;
            while j > 0 {
                let c = self.call_value(
                    cb.clone(),
                    Value::Undefined,
                    vec![elems[j].clone(), elems[j - 1].clone()],
                )?;
                if c.to_number() < 0.0 {
                    elems.swap(j, j - 1);
                    j -= 1;
                } else {
                    break;
                }
            }
        }
        arr.borrow_mut().elems = elems;
        Ok(Value::Array(arr.clone()))
    }

    /// `Array.prototype.some/every/findIndex/flatMap`（回调进解释器）。
    fn array_higher_order2(
        &mut self,
        arr: &ArrayRef,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err(format!("{} called with non-function callback", name)));
        }
        let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
        // phase 12：同 array_higher_order——按下标逐个取，不整体克隆。
        let len = arr.borrow().elems.len();
        let arr_val = Value::Array(arr.clone());
        let at = |arr: &ArrayRef, i: usize| -> Value {
            arr.borrow()
                .elems
                .get(i)
                .cloned()
                .unwrap_or(Value::Undefined)
        };
        match name {
            "some" => {
                for i in 0..len {
                    let e = at(arr, i);
                    let hit = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e, Value::Number(i as f64), arr_val.clone()],
                    )?;
                    if hit.to_boolean() {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            "every" => {
                for i in 0..len {
                    let e = at(arr, i);
                    let hit = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e, Value::Number(i as f64), arr_val.clone()],
                    )?;
                    if !hit.to_boolean() {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            "findIndex" => {
                for i in 0..len {
                    let e = at(arr, i);
                    let hit = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e, Value::Number(i as f64), arr_val.clone()],
                    )?;
                    if hit.to_boolean() {
                        return Ok(Value::Number(i as f64));
                    }
                }
                Ok(Value::Number(-1.0))
            }
            "flatMap" => {
                let mut out = Vec::new();
                for i in 0..len {
                    let e = at(arr, i);
                    let r = self.call_value(
                        cb.clone(),
                        this_arg.clone(),
                        vec![e, Value::Number(i as f64), arr_val.clone()],
                    )?;
                    match r {
                        Value::Array(a) => out.extend(a.borrow().elems.iter().cloned()),
                        v => out.push(v),
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                    out,
                    Some(self.protos.array.clone()),
                )))))
            }
            _ => Err(rt(format!("unknown array method '{}'", name))),
        }
    }

    // ------------------------------------------------------------------
    // Phase 4：Function.prototype.call / apply（重分发 this）
    // ------------------------------------------------------------------

    fn call_apply(
        &mut self,
        base: Value,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let this_arg = args.first().cloned().unwrap_or(Value::Undefined);
        let call_args: Vec<Value> = if key == "call" {
            args.into_iter().skip(1).collect()
        } else {
            // apply(thisArg, [args])
            match args.get(1) {
                None | Some(Value::Undefined) | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a.borrow().elems.clone(),
                Some(v) => {
                    return Err(type_err(format!(
                        "apply called with non-array argument ({})",
                        v.type_of()
                    )))
                }
            }
        };
        self.call_value(base, this_arg, call_args)
    }

    // ------------------------------------------------------------------
    // Phase 4：DOM 绑定与分发
    // ------------------------------------------------------------------

    /// 安装宿主 DOM：全局出现 `document` / `window`。
    /// 方法调用经 `try_dom_method` 拦截分发（Native 签名拿不到宿主）。
    pub fn bind_dom(&mut self, host: Rc<dyn DomHost>) {
        let document = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        let window = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        window
            .borrow_mut()
            .set("document", Value::Object(document.clone()));
        self.define_global("document", Value::Object(document.clone()));
        self.define_global("window", Value::Object(window.clone()));
        self.dom = Some(DomBinding {
            host,
            document,
            window,
            next_timeout_id: 1,
            tasks: VecDeque::new(),
            cancelled: HashSet::new(),
            intervals: HashSet::new(),
            listener_cbs: HashMap::new(),
        });
    }

    /// phase 11：VM 用——DOM 方法拦截（参数已求值）。
    pub(crate) fn try_dom_method_vals(
        &mut self,
        base: &Value,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Option<Value>, FlowError> {
        let (document, window) = match &self.dom {
            Some(d) => (d.document.clone(), d.window.clone()),
            None => return Ok(None),
        };
        if let Value::Object(o) = base {
            if Rc::ptr_eq(o, &document) {
                return Ok(Some(self.call_document_method(key, args)?));
            }
            if Rc::ptr_eq(o, &window) {
                match key {
                    "setTimeout" => {
                            return Ok(Some(self.call_set_timeout(args)?));
                    }
                    "clearTimeout" => {
                            return Ok(Some(self.call_clear_timeout(args)?));
                    }
                    // setInterval：TODO phase 7（事件循环已就绪，只差重复调度）。
                    "setInterval" => {
                        return Ok(Some(self.call_set_interval(args)?));
                    }
                    "clearInterval" => {
                        return Ok(Some(self.call_clear_interval(args)?));
                    }
                    _ => {}
                }
            }
            return Ok(None);
        }
        if let Value::DomNode(node) = base {
            match key {
                "getAttribute" | "setAttribute" | "appendChild"
                | "insertBefore" | "removeChild" | "addEventListener"
                | "click" => {
                    return Ok(Some(self.call_node_method(node, key, args)?));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    fn call_document_method(
        &mut self,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let (host, array_proto) = match &self.dom {
            Some(d) => (d.host.clone(), self.protos.array.clone()),
            None => return Err(rt("document is not bound to a DOM host")),
        };
        let node_val = |id: u64| Value::DomNode(DomNode::new(id, host.clone()));
        match key {
            "getElementById" => {
                let id = args.first().map(|v| v.to_js_string()).unwrap_or_default();
                Ok(host
                    .get_element_by_id(&id)
                    .map(|nid| node_val(nid))
                    .unwrap_or(Value::Null))
            }
            "querySelector" => {
                let sel = args.first().map(|v| v.to_js_string()).unwrap_or_default();
                Ok(host
                    .query_selector_all(&sel)
                    .into_iter()
                    .next()
                    .map(|nid| node_val(nid))
                    .unwrap_or(Value::Null))
            }
            "querySelectorAll" => {
                let sel = args.first().map(|v| v.to_js_string()).unwrap_or_default();
                let elems: Vec<Value> = host
                    .query_selector_all(&sel)
                    .into_iter()
                    .map(|nid| node_val(nid))
                    .collect();
                Ok(Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
                    elems,
                    Some(array_proto),
                )))))
            }
            "createElement" => {
                let tag = args.first().map(|v| v.to_js_string()).unwrap_or_default();
                Ok(node_val(host.create_element(&tag)))
            }
            _ => Err(type_err(format!("document.{} is not a function", key))),
        }
    }

    fn call_node_method(
        &mut self,
        node: &DomNode,
        key: &str,
        args: Vec<Value>,
    ) -> Result<Value, FlowError> {
        let host: &Rc<dyn DomHost> = &node.host;
        let id = node.node_id;
        // 参数必须是 DOM 节点（appendChild / insertBefore / removeChild 用）。
        let as_node = |v: Option<&Value>| -> Result<u64, FlowError> {
            match v {
                Some(Value::DomNode(n)) => Ok(n.node_id),
                _ => Err(type_err(format!("{} expects a DOM node argument", key))),
            }
        };
        match key {
            "getAttribute" => {
                let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
                Ok(host
                    .get_attribute(id, &name)
                    .map(Value::String)
                    .unwrap_or(Value::Null))
            }
            "setAttribute" => {
                let name = args.first().map(|v| v.to_js_string()).unwrap_or_default();
                let val = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
                host.set_attribute(id, &name, &val);
                Ok(Value::Undefined)
            }
            "appendChild" => {
                let c = as_node(args.first())?;
                // 成环时静默跳过（真 DOM 会抛 HierarchyRequestError）。
                if !would_create_cycle(host.as_ref(), id, c) {
                    host.append_child(id, c);
                }
                Ok(args.first().cloned().unwrap_or(Value::Undefined))
            }
            "insertBefore" => {
                let c = as_node(args.first())?;
                let b = as_node(args.get(1))?;
                if !would_create_cycle(host.as_ref(), id, c) {
                    host.insert_before(id, c, b);
                }
                Ok(args.first().cloned().unwrap_or(Value::Undefined))
            }
            "removeChild" => {
                let c = as_node(args.first())?;
                host.remove_child(id, c);
                Ok(args.first().cloned().unwrap_or(Value::Undefined))
            }
            "addEventListener" => {
                let event =
                    args.first().map(|v| v.to_js_string()).unwrap_or_default();
                let cb = args.get(1).cloned().unwrap_or(Value::Undefined);
                // 浏览器行为：回调不可调用时静默忽略，不抛错。
                if cb.is_callable() {
                    let tok = host.add_event_listener(id, &event);
                    if tok != 0 {
                        if let Some(d) = &mut self.dom {
                            d.listener_cbs.insert(tok, cb);
                        }
                    }
                }
                Ok(Value::Undefined)
            }
            "click" => {
                // 把 'click' 监听的回调全部推进宏任务队列（异步触发）。
                let tokens = host.event_listeners(id, "click");
                let mut cbs = Vec::new();
                if let Some(d) = &self.dom {
                    for t in tokens {
                        if let Some(cb) = d.listener_cbs.get(&t) {
                            cbs.push(cb.clone());
                        }
                    }
                }
                for cb in cbs {
                    self.enqueue_task(cb);
                }
                Ok(Value::Undefined)
            }
            _ => Err(type_err(format!("node.{} is not a function", key))),
        }
    }

    /// `window.setTimeout`：注册宏任务，主脚本跑完后按 FIFO 执行；
    /// `ms` 在子集里只做语义占位（不做精确计时），返回计时器 id。
    fn call_set_timeout(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("setTimeout called with non-function callback"));
        }
        Ok(Value::Number(self.enqueue_task(cb) as f64))
    }

    /// `window.clearTimeout`：标记取消；drain 时跳过。
    fn call_clear_timeout(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let id = args.first().map(|v| v.to_number() as u64).unwrap_or(0);
        if let Some(d) = &mut self.dom {
            d.cancelled.insert(id);
        }
        Ok(Value::Undefined)
    }

    /// phase 13：`window.setInterval`：注册重复宏任务，主脚本跑完后按 FIFO
    /// 反复执行，直到 `clearInterval`；`ms` 只做语义占位。返回计时器 id。
    fn call_set_interval(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let cb = args.first().cloned().unwrap_or(Value::Undefined);
        if !cb.is_callable() {
            return Err(type_err("setInterval called with non-function callback"));
        }
        let d = self.ensure_binding();
        let id = d.next_timeout_id;
        d.next_timeout_id += 1;
        d.intervals.insert(id);
        d.tasks.push_back(Task {
            id,
            kind: TaskKind::Interval { id, callback: cb },
        });
        Ok(Value::Number(id as f64))
    }

    /// phase 13：`window.clearInterval`：移出存活集合并标记取消。
    fn call_clear_interval(&mut self, args: Vec<Value>) -> Result<Value, FlowError> {
        let id = args.first().map(|v| v.to_number() as u64).unwrap_or(0);
        if let Some(d) = &mut self.dom {
            d.intervals.remove(&id);
            d.cancelled.insert(id);
        }
        Ok(Value::Undefined)
    }

    fn get_dom_prop(&self, node: &DomNode, key: &str) -> Value {
        match key {
            "tagName" => Value::String(node.tag_name().to_uppercase()),
            "textContent" => Value::String(node.host.text_content(node.node_id)),
            "innerHTML" => Value::String(node.host.inner_html(node.node_id)),
            "outerHTML" => Value::String(node.host.outer_html(node.node_id)),
            // 简化版 classList：每次访问返回新对象（`===` 不恒等），
            // 方法经隐藏属性取回节点。
            "classList" => Value::Object(self.make_class_list(node)),
            "id" => Value::String(
                node.host
                    .get_attribute(node.node_id, "id")
                    .unwrap_or_default(),
            ),
            _ => Value::Undefined,
        }
    }

    /// 构造 `classList` 对象：`add` / `remove` / `contains` 三个 Native 方法，
    /// 经隐藏属性拿回宿主节点（Native 签名拿不到宿主）。
    fn make_class_list(&self, node: &DomNode) -> ObjectRef {
        let obj = Rc::new(RefCell::new(JsObject::with_proto(Some(
            self.protos.object.clone(),
        ))));
        obj.borrow_mut()
            .set(CLASS_LIST_NODE, Value::DomNode(node.clone()));
        obj.borrow_mut().set("add", Value::Native(native_class_add));
        obj.borrow_mut()
            .set("remove", Value::Native(native_class_remove));
        obj.borrow_mut()
            .set("contains", Value::Native(native_class_contains));
        obj
    }

    fn set_dom_prop(
        &self,
        node: &DomNode,
        key: &str,
        val: Value,
    ) -> Result<(), FlowError> {
        match key {
            "textContent" => {
                node.host.set_text_content(node.node_id, &val.to_js_string());
                Ok(())
            }
            // innerHTML 可写：调宿主解析片段后替换子节点。
            "innerHTML" => {
                node.host
                    .set_inner_html(node.node_id, &val.to_js_string());
                Ok(())
            }
            "id" => {
                node
                    .host
                    .set_attribute(node.node_id, "id", &val.to_js_string());
                Ok(())
            }
            // outerHTML 只读（子集）；其余属性不支持。
            _ => Err(type_err(format!(
                "cannot set property '{}' of DOM node",
                key
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

pub(crate) fn decl_kind(k: VarKind) -> DeclKind {
    match k {
        VarKind::Var => DeclKind::Var,
        VarKind::Let => DeclKind::Let,
        VarKind::Const => DeclKind::Const,
    }
}

/// phase 9：值必须是对象（私有成员 / super 相关路径用）。
fn expect_obj(v: &Value) -> Result<ObjectRef, FlowError> {
    match v {
        Value::Object(o) => Ok(o.clone()),
        _ => Err(type_err(format!(
            "expected object, got {}",
            v.type_of()
        ))),
    }
}

/// `a < b`：都是字符串则按字典序，否则按数字比。
fn compare_lt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x < y,
        _ => a.to_number() < b.to_number(),
    }
}

/// 赋值目标：环境变量或对象属性。
#[derive(Debug)]
pub(crate) enum Target {
    Var(EnvRef, String),
    Prop(Value, String),
    // phase 9：`obj.#x = v` 私有成员赋值。
    PrivateProp(Value, String),
}

// ---------------------------------------------------------------------------
// 内置函数实现
// ---------------------------------------------------------------------------

/// `classList` 对象上藏节点的属性名（`\0` 开头，防与用户属性冲突）。
const CLASS_LIST_NODE: &str = "\u{0}yousj_dom_node";

/// 从 `classList` 方法的 this 里取回宿主节点。
fn class_list_node(ctx: &NativeCtx) -> Option<DomNode> {
    match &ctx.this {
        Value::Object(o) => match o.borrow().get(CLASS_LIST_NODE) {
            Some(Value::DomNode(n)) => Some(n),
            _ => None,
        },
        _ => None,
    }
}

fn native_class_add(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    if let Some(n) = class_list_node(ctx) {
        let c = args.first().map(|v| v.to_js_string()).unwrap_or_default();
        n.host.class_add(n.node_id, &c);
    }
    Ok(Value::Undefined)
}

fn native_class_remove(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    if let Some(n) = class_list_node(ctx) {
        let c = args.first().map(|v| v.to_js_string()).unwrap_or_default();
        n.host.class_remove(n.node_id, &c);
    }
    Ok(Value::Undefined)
}

fn native_class_contains(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let has = match class_list_node(ctx) {
        Some(n) => {
            let c = args.first().map(|v| v.to_js_string()).unwrap_or_default();
            n.host.class_contains(n.node_id, &c)
        }
        None => false,
    };
    Ok(Value::Bool(has))
}

fn native_console_log(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    ctx.console.push(
        args.iter()
            .map(|v| v.to_js_string())
            .collect::<Vec<_>>()
            .join(" "),
    );
    Ok(Value::Undefined)
}

macro_rules! math1 {
    ($name:ident, $f:expr) => {
        fn $name(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
            let x = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
            Ok(Value::Number($f(x)))
        }
    };
}

math1!(math_abs, f64::abs);
math1!(math_floor, f64::floor);
math1!(math_ceil, f64::ceil);
math1!(math_sqrt, f64::sqrt);

fn math_round(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let x = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
    // JS 语义：Math.round(-2.5) === -2（与 Rust 的 round 不同）。
    Ok(Value::Number(if x.is_nan() {
        f64::NAN
    } else {
        (x + 0.5).floor()
    }))
}

fn math_max(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let mut best = f64::NEG_INFINITY;
    for a in &args {
        let n = a.to_number();
        if n.is_nan() {
            return Ok(Value::Number(f64::NAN));
        }
        if n > best {
            best = n;
        }
    }
    Ok(Value::Number(best))
}

fn math_min(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let mut best = f64::INFINITY;
    for a in &args {
        let n = a.to_number();
        if n.is_nan() {
            return Ok(Value::Number(f64::NAN));
        }
        if n < best {
            best = n;
        }
    }
    Ok(Value::Number(best))
}

fn math_pow(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let x = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
    let y = args.get(1).map(|v| v.to_number()).unwrap_or(f64::NAN);
    Ok(Value::Number(x.powf(y)))
}

static RNG_STATE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0x9E3779B97F4A7C15);

fn math_random(_ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    use std::sync::atomic::Ordering;
    let mut x = RNG_STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = 0x9E3779B97F4A7C15;
    }
    // xorshift64（零依赖的伪随机，够用）。
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG_STATE.store(x, Ordering::Relaxed);
    Ok(Value::Number((x as f64) / (u64::MAX as f64)))
}

// ---------------------------------------------------------------------------
// JSON（基础版）
// ---------------------------------------------------------------------------

fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_stringify_val(v: &Value, depth: usize) -> Result<Option<String>, FlowError> {
    if depth > 200 {
        return Err(type_err("JSON.stringify: structure too deeply nested"));
    }
    match v {
        Value::Undefined
        | Value::Function(_)
        | Value::Native(_)
        | Value::DomNode(_)
        | Value::Promise(_)
        | Value::PromiseSettler(_)
        // phase 9：生成器/Proxy 按规范 JSON.stringify 为 undefined（被忽略）。
        // phase 10：集合与二进制视图同样被忽略。
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
        | Value::Date(_) => Ok(None),
        // 正则转它的字面量形式。
        Value::RegExp(r) => Ok(Some(json_quote(&r.borrow().display()))),
        Value::Null => Ok(Some("null".to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        Value::Number(n) => Ok(Some(if n.is_finite() {
            number_to_js_string(*n)
        } else {
            "null".to_string()
        })),
        Value::String(s) => Ok(Some(json_quote(s))),
        Value::Array(a) => {
            let mut parts = Vec::new();
            for e in a.borrow().elems.iter() {
                parts.push(
                    json_stringify_val(e, depth + 1)?.unwrap_or_else(|| "null".to_string()),
                );
            }
            Ok(Some(format!("[{}]", parts.join(","))))
        }
        Value::Object(o) => {
            let entries: Vec<(String, Value)> = {
                let ob = o.borrow();
                ob.keys()
                    .iter()
                    .map(|k| (k.clone(), ob.get(k).unwrap()))
                    .collect()
            };
            let mut parts = Vec::new();
            for (k, val) in entries {
                if let Some(s) = json_stringify_val(&val, depth + 1)? {
                    parts.push(format!("{}:{}", json_quote(&k), s));
                }
            }
            Ok(Some(format!("{{{}}}", parts.join(","))))
        }
    }
}

fn native_json_stringify(
    _ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    match json_stringify_val(&v, 0)? {
        Some(s) => Ok(Value::String(s)),
        None => Ok(Value::Undefined),
    }
}

struct JsonParser {
    chars: Vec<char>,
    pos: usize,
}

impl JsonParser {
    fn new(s: &str) -> Self {
        JsonParser {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn err(&self, msg: &str) -> FlowError {
        syntax_err(format!("JSON parse error at char {}: {}", self.pos, msg))
    }

    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn expect(&mut self, c: char) -> Result<(), FlowError> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", c)))
        }
    }

    fn parse_value(&mut self) -> Result<Value, FlowError> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(Value::String(self.parse_string()?)),
            Some('t') => self.parse_literal("true", Value::Bool(true)),
            Some('f') => self.parse_literal("false", Value::Bool(false)),
            Some('n') => self.parse_literal("null", Value::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(self.err(&format!("unexpected character '{}'", c))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn parse_literal(&mut self, word: &str, val: Value) -> Result<Value, FlowError> {
        for wc in word.chars() {
            if self.peek() == Some(wc) {
                self.pos += 1;
            } else {
                return Err(self.err(&format!("expected '{}'", word)));
            }
        }
        Ok(val)
    }

    fn parse_string(&mut self) -> Result<String, FlowError> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('/') => out.push('/'),
                        Some('n') => out.push('\n'),
                        Some('r') => out.push('\r'),
                        Some('t') => out.push('\t'),
                        Some('b') => out.push('\x08'),
                        Some('f') => out.push('\x0C'),
                        Some('u') => {
                            self.pos += 1;
                            let mut code: u32 = 0;
                            for _ in 0..4 {
                                match self.peek() {
                                    Some(h) if h.is_ascii_hexdigit() => {
                                        code = code * 16 + h.to_digit(16).unwrap();
                                        self.pos += 1;
                                    }
                                    _ => return Err(self.err("bad \\u escape")),
                                }
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                            continue;
                        }
                        Some(c) => return Err(self.err(&format!("bad escape '\\{}'", c))),
                        None => return Err(self.err("unterminated string")),
                    }
                    self.pos += 1;
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(out)
    }

    fn parse_number(&mut self) -> Result<Value, FlowError> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| self.err("bad number"))
    }

    fn parse_array(&mut self) -> Result<Value, FlowError> {
        self.expect('[')?;
        let mut elems = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Value::Array(Rc::new(RefCell::new(JsArray::new(elems)))));
        }
        loop {
            elems.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
        Ok(Value::Array(Rc::new(RefCell::new(JsArray::new(elems)))))
    }

    fn parse_object(&mut self) -> Result<Value, FlowError> {
        self.expect('{')?;
        let obj = Rc::new(RefCell::new(JsObject::new()));
        obj.borrow_mut().tag = Some("Object".to_string());
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Object(obj));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return Err(self.err("expected string key"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(':')?;
            let val = self.parse_value()?;
            obj.borrow_mut().set(&key, val);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        Ok(Value::Object(obj))
    }
}

fn native_json_parse(
    _ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let s = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_default();
    let mut p = JsonParser::new(&s);
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(p.err("unexpected trailing characters"));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// parseInt / parseFloat / isNaN
// ---------------------------------------------------------------------------

fn native_parse_int(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_default();
    let radix = args.get(1).map(|v| v.to_int32()).unwrap_or(0);
    Ok(Value::Number(js_parse_int(&s, radix)))
}

fn js_parse_int(s: &str, radix: i32) -> f64 {
    let t = s.trim_start_matches(|c: char| c.is_whitespace());
    let (sign, t) = match t.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, t) = if radix == 0 || radix == 16 {
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            (16, h)
        } else if radix == 0 {
            (10, t)
        } else {
            (16, t)
        }
    } else {
        (radix, t)
    };
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let digits: Vec<char> = t
        .chars()
        .take_while(|c| c.to_digit(radix as u32).is_some())
        .collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut v = 0.0;
    for c in digits {
        v = v * radix as f64 + c.to_digit(radix as u32).unwrap() as f64;
    }
    sign * v
}

fn native_parse_float(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_default();
    Ok(Value::Number(js_parse_float(&s)))
}

fn js_parse_float(s: &str) -> f64 {
    let t = s.trim_start_matches(|c: char| c.is_whitespace());
    if t.starts_with("Infinity") {
        return f64::INFINITY;
    }
    if let Some(r) = t.strip_prefix('-') {
        if r.starts_with("Infinity") {
            return f64::NEG_INFINITY;
        }
    }
    // 手动扫描最长合法前缀：[+-]? (digits [. digits?] | . digits) ([eE][+-]?digits)?
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
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let es = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j != es {
            i = j;
        }
    }
    t[..i].parse::<f64>().unwrap_or(f64::NAN)
}

fn native_is_nan(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(
        args.first().map(|v| v.to_number()).unwrap_or(f64::NAN).is_nan(),
    ))
}

// ---------------------------------------------------------------------------
// Phase 4：标准库原生方法（Array/String/Object/Number）
// ---------------------------------------------------------------------------
//
// 全部经 `NativeCtx` 拿 `this`（`arr.push` 的 this 就是 arr），
// 不再需要 phase 3 那种 eval_call 特殊分发；只有需要回调进解释器
// 的高阶函数（map/filter/forEach/find/reduce）与 call/apply 仍走拦截。
//
// 被拦截的方法同时在原型上挂一个桩（`intercepted_method_stub`），
// 以便 `typeof arr.map === 'function'` 这类属性访问正常；
// 桩被实际调用到只可能是分离调用（子集不支持），此时明确报错。

fn intercepted_method_stub(
    _ctx: &mut NativeCtx,
    _args: Vec<Value>,
) -> Result<Value, FlowError> {
    Err(type_err(
        "this method must be called directly as obj.method() \
         (detached calls are not supported in this subset)",
    ))
}

fn new_js_array(ctx: &NativeCtx, elems: Vec<Value>) -> Value {
    Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
        elems,
        Some(ctx.protos.array.clone()),
    ))))
}

fn this_array(ctx: &NativeCtx) -> Result<ArrayRef, FlowError> {
    match &ctx.this {
        Value::Array(a) => Ok(a.clone()),
        v => Err(type_err(format!(
            "array method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_string(ctx: &NativeCtx) -> Result<String, FlowError> {
    match &ctx.this {
        Value::Null | Value::Undefined => {
            Err(type_err("string method called on null/undefined"))
        }
        // 宽松装箱：数字等原始值先转字符串（子集简化）。
        v => Ok(v.to_js_string()),
    }
}

/// `[start, end)` 规范化：负数从尾部计，钳制到 `[0, len]`。
fn norm_index(v: f64, len: usize) -> usize {
    let lenf = len as f64;
    let n = if !v.is_finite() {
        0.0
    } else if v < 0.0 {
        (lenf + v).max(0.0)
    } else {
        v.min(lenf)
    };
    n as usize
}

/// SameValueZero（`includes` 用：NaN 视为相等）。
fn same_value_zero(a: &Value, b: &Value) -> bool {
    if a.strict_eq(b) {
        return true;
    }
    matches!(
        (a, b),
        (Value::Number(x), Value::Number(y)) if x.is_nan() && y.is_nan()
    )
}

// ---- Array.prototype ----

fn array_push(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let mut a = arr.borrow_mut();
    for v in args {
        a.elems.push(v);
    }
    Ok(Value::Number(a.elems.len() as f64))
}

fn array_pop(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(this_array(ctx)?
        .borrow_mut()
        .elems
        .pop()
        .unwrap_or(Value::Undefined))
}

fn array_shift(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let mut a = arr.borrow_mut();
    Ok(if a.elems.is_empty() {
        Value::Undefined
    } else {
        a.elems.remove(0)
    })
}

fn array_unshift(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let mut a = arr.borrow_mut();
    for (i, v) in args.into_iter().enumerate() {
        a.elems.insert(i, v);
    }
    Ok(Value::Number(a.elems.len() as f64))
}

fn array_join(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let sep = match args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(v) => v.to_js_string(),
    };
    let s = arr
        .borrow()
        .elems
        .iter()
        .map(|v| {
            if v.is_nullish() {
                String::new()
            } else {
                v.to_js_string()
            }
        })
        .collect::<Vec<_>>()
        .join(&sep);
    Ok(Value::String(s))
}

fn array_slice(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let a = arr.borrow();
    let len = a.elems.len();
    let start = args
        .first()
        .map(|v| norm_index(v.to_number(), len))
        .unwrap_or(0);
    let end = args
        .get(1)
        .map(|v| norm_index(v.to_number(), len))
        .unwrap_or(len);
    let (start, end) = if start > end { (end, end) } else { (start, end) };
    Ok(new_js_array(ctx, a.elems[start..end].to_vec()))
}

fn array_concat(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let mut out: Vec<Value> = arr.borrow().elems.clone();
    for v in args {
        match v {
            Value::Array(a) => out.extend(a.borrow().elems.iter().cloned()),
            other => out.push(other),
        }
    }
    Ok(new_js_array(ctx, out))
}

fn array_index_of(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let a = arr.borrow();
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let from = args
        .get(1)
        .map(|v| norm_index(v.to_number(), a.elems.len()))
        .unwrap_or(0);
    let idx = a.elems[from..]
        .iter()
        .position(|e| e.strict_eq(&target))
        .map(|i| (i + from) as f64)
        .unwrap_or(-1.0);
    Ok(Value::Number(idx))
}

fn array_includes(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let arr = this_array(ctx)?;
    let a = arr.borrow();
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let from = args
        .get(1)
        .map(|v| norm_index(v.to_number(), a.elems.len()))
        .unwrap_or(0);
    Ok(Value::Bool(
        a.elems[from..].iter().any(|e| same_value_zero(e, &target)),
    ))
}

fn array_is_array(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(matches!(args.first(), Some(Value::Array(_)))))
}

// ---- String.prototype（字符语义；JS 原生是 UTF-16 码元，子集用 char 近似） ----

fn string_char_at(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let n = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    if !n.is_finite() || n < 0.0 {
        return Ok(Value::String(String::new()));
    }
    Ok(s
        .chars()
        .nth(n as usize)
        .map(|c| Value::String(c.to_string()))
        .unwrap_or(Value::String(String::new())))
}

fn string_char_code_at(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let n = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    if !n.is_finite() || n < 0.0 {
        return Ok(Value::Number(f64::NAN));
    }
    Ok(s
        .chars()
        .nth(n as usize)
        .map(|c| Value::Number(c as u32 as f64))
        .unwrap_or(Value::Number(f64::NAN)))
}

fn string_chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn string_slice(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let chars = string_chars(&s);
    let len = chars.len();
    let start = args
        .first()
        .map(|v| norm_index(v.to_number(), len))
        .unwrap_or(0);
    let end = args
        .get(1)
        .map(|v| norm_index(v.to_number(), len))
        .unwrap_or(len);
    let (start, end) = if start > end { (end, end) } else { (start, end) };
    Ok(Value::String(chars[start..end].iter().collect()))
}

fn string_substring(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let chars = string_chars(&s);
    let len = chars.len();
    let clamp = |v: f64| -> usize {
        if !v.is_finite() || v < 0.0 {
            0
        } else {
            (v as usize).min(len)
        }
    };
    let mut start = args.first().map(|v| clamp(v.to_number())).unwrap_or(0);
    let mut end = args.get(1).map(|v| clamp(v.to_number())).unwrap_or(len);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    Ok(Value::String(chars[start..end].iter().collect()))
}

/// 子串按字符下标查找（`from` 为字符下标）。
fn str_index_of(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h: Vec<char> = hay.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    let start = from.min(h.len());
    if n.is_empty() {
        return Some(start);
    }
    if n.len() > h.len() {
        return None;
    }
    (start..=(h.len() - n.len())).find(|&i| h[i..].starts_with(&n))
}

fn string_index_of(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    // `indexOf(undefined)` 按 "undefined" 搜（规范行为）。
    let search = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_else(|| "undefined".to_string());
    let from = args
        .get(1)
        .map(|v| {
            let n = v.to_number();
            if !n.is_finite() || n < 0.0 {
                0
            } else {
                n as usize
            }
        })
        .unwrap_or(0);
    Ok(Value::Number(
        str_index_of(&s, &search, from)
            .map(|i| i as f64)
            .unwrap_or(-1.0),
    ))
}

fn string_includes(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let search = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_else(|| "undefined".to_string());
    let from = args
        .get(1)
        .map(|v| {
            let n = v.to_number();
            if !n.is_finite() || n < 0.0 {
                0
            } else {
                n as usize
            }
        })
        .unwrap_or(0);
    Ok(Value::Bool(str_index_of(&s, &search, from).is_some()))
}

fn string_split(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    match args.first() {
        None | Some(Value::Undefined) => Ok(new_js_array(ctx, vec![Value::String(s)])),
        // Phase 7：分隔符为正则。
        Some(Value::RegExp(r)) => {
            let b = r.borrow();
            let chars: Vec<char> = s.chars().collect();
            let mut out: Vec<Value> = Vec::new();
            let mut last = 0usize;
            let mut from = 0usize;
            loop {
                if from > chars.len() {
                    break;
                }
                match b.compiled.search(&s, from) {
                    Some(m) => {
                        let start = m.index;
                        let end = start + m.text.chars().count();
                        out.push(Value::String(chars[last..start].iter().collect()));
                        last = end;
                        // 空匹配必须前进一步，否则死循环。
                        // （空匹配切分是简化实现：首尾可能多出空串，文档化。）
                        from = if end == from { from + 1 } else { end };
                    }
                    None => break,
                }
            }
            out.push(Value::String(chars[last..].iter().collect()));
            Ok(new_js_array(ctx, out))
        }
        Some(sep_v) => {
            let sep = sep_v.to_js_string();
            if sep.is_empty() {
                // `split("")` → 逐字符（Rust 的 split("") 会在首尾加空串，不符合）。
                Ok(new_js_array(
                    ctx,
                    s.chars().map(|c| Value::String(c.to_string())).collect(),
                ))
            } else {
                Ok(new_js_array(
                    ctx,
                    s.split(sep.as_str())
                        .map(|p| Value::String(p.to_string()))
                        .collect(),
                ))
            }
        }
    }
}

fn string_trim(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::String(this_string(ctx)?.trim().to_string()))
}

fn string_to_upper_case(
    ctx: &mut NativeCtx,
    _args: Vec<Value>,
) -> Result<Value, FlowError> {
    Ok(Value::String(
        this_string(ctx)?.chars().flat_map(|c| c.to_uppercase()).collect(),
    ))
}

fn string_to_lower_case(
    ctx: &mut NativeCtx,
    _args: Vec<Value>,
) -> Result<Value, FlowError> {
    Ok(Value::String(
        this_string(ctx)?.chars().flat_map(|c| c.to_lowercase()).collect(),
    ))
}

fn string_starts_with(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let search = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_else(|| "undefined".to_string());
    let chars: Vec<char> = s.chars().collect();
    let schars: Vec<char> = search.chars().collect();
    let pos = args
        .get(1)
        .map(|v| {
            let n = v.to_number();
            if !n.is_finite() || n < 0.0 {
                0
            } else {
                (n as usize).min(chars.len())
            }
        })
        .unwrap_or(0);
    Ok(Value::Bool(chars[pos..].starts_with(&schars)))
}

fn string_ends_with(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let search = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_else(|| "undefined".to_string());
    let chars: Vec<char> = s.chars().collect();
    let schars: Vec<char> = search.chars().collect();
    let end = args
        .get(1)
        .map(|v| {
            let n = v.to_number();
            if !n.is_finite() || n < 0.0 {
                0
            } else {
                (n as usize).min(chars.len())
            }
        })
        .unwrap_or(chars.len());
    Ok(Value::Bool(chars[..end].ends_with(&schars)))
}

// ---- Object 静态方法 ----

/// 自身可枚举键（子集：对象自身属性 / 数组下标+附加属性 / 字符串下标）。
fn own_keys(v: Option<&Value>) -> Result<Vec<String>, FlowError> {
    match v {
        Some(Value::Object(o)) => Ok(o.borrow().keys()),
        Some(Value::Array(a)) => {
            let arr = a.borrow();
            let mut k: Vec<String> = (0..arr.elems.len()).map(|i| i.to_string()).collect();
            k.extend(arr.props.keys());
            Ok(k)
        }
        Some(Value::String(s)) => Ok((0..s.chars().count()).map(|i| i.to_string()).collect()),
        _ => Err(type_err("Object.keys called on non-object")),
    }
}

fn get_own(v: &Value, key: &str) -> Value {
    match v {
        Value::Object(o) => o.borrow().get(key).unwrap_or(Value::Undefined),
        Value::Array(a) => {
            let arr = a.borrow();
            if let Ok(i) = key.parse::<usize>() {
                arr.elems.get(i).cloned().unwrap_or(Value::Undefined)
            } else {
                arr.props.get(key).unwrap_or(Value::Undefined)
            }
        }
        Value::String(s) => {
            if let Ok(i) = key.parse::<usize>() {
                s.chars()
                    .nth(i)
                    .map(|c| Value::String(c.to_string()))
                    .unwrap_or(Value::Undefined)
            } else {
                Value::Undefined
            }
        }
        _ => Value::Undefined,
    }
}

fn object_keys(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let keys = own_keys(args.first())?;
    Ok(new_js_array(
        ctx,
        keys.into_iter().map(Value::String).collect(),
    ))
}

fn object_values(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let keys = own_keys(args.first())?;
    Ok(new_js_array(
        ctx,
        keys.iter().map(|k| get_own(&target, k)).collect(),
    ))
}

fn object_entries(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let keys = own_keys(args.first())?;
    let pairs: Vec<Value> = keys
        .iter()
        .map(|k| {
            new_js_array(
                ctx,
                vec![Value::String(k.clone()), get_own(&target, k)],
            )
        })
        .collect();
    Ok(new_js_array(ctx, pairs))
}

fn object_assign(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let tref: ObjectRef = match &target {
        Value::Object(o) => o.clone(),
        _ => return Err(type_err("Object.assign called on non-object target")),
    };
    for src in args.iter().skip(1) {
        if src.is_nullish() {
            continue;
        }
        for k in own_keys(Some(src))? {
            let v = get_own(src, &k);
            tref.borrow_mut().set(&k, v);
        }
    }
    Ok(target)
}

/// phase 9：`Object.getPrototypeOf(o)`。
fn object_get_prototype_of(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    match &target {
        Value::Object(o) => Ok(o
            .borrow()
            .proto
            .clone()
            .map(Value::Object)
            .unwrap_or(Value::Null)),
        Value::Array(a) => Ok(a
            .borrow()
            .proto
            .clone()
            .map(Value::Object)
            .unwrap_or(Value::Null)),
        _ => Err(type_err("Object.getPrototypeOf called on non-object")),
    }
}

/// phase 9：生成器 `yield*`/`for-of` 改写用的迭代器构造。
/// 返回记录：生成器 → `{k: 2, g}`；数组/字符串 → `{k: 1, arr/str, i: 0}`。
fn native_iter_of(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let src = args.first().cloned().unwrap_or(Value::Undefined);
    let rec = Rc::new(RefCell::new(JsObject::new()));
    match &src {
        Value::Generator(_) => {
            rec.borrow_mut().set("k", Value::Number(2.0));
            rec.borrow_mut().set("g", src);
        }
        Value::Array(_) => {
            rec.borrow_mut().set("k", Value::Number(1.0));
            rec.borrow_mut().set("arr", src);
            rec.borrow_mut().set("i", Value::Number(0.0));
        }
        Value::String(_) => {
            rec.borrow_mut().set("k", Value::Number(1.0));
            rec.borrow_mut().set("str", src);
            rec.borrow_mut().set("i", Value::Number(0.0));
        }
        _ => {
            return Err(type_err(format!(
                "yield* argument is not iterable: {}",
                src.type_of()
            )))
        }
    }
    Ok(Value::Object(rec))
}

/// phase 9：`__yousj$iter_next(it)` —— 非生成器迭代器的单步。
/// 返回 `{value, done}`；推进 `it.i`。
fn native_iter_next(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let it = args.first().cloned().unwrap_or(Value::Undefined);
    let rec: ObjectRef = match &it {
        Value::Object(o) => o.clone(),
        _ => return Err(type_err("__yousj$iter_next called on non-object")),
    };
    let idx = match rec.borrow().get("i") {
        Some(Value::Number(n)) => n as usize,
        _ => 0,
    };
    let (value, done) = if let Some(Value::Array(a)) = rec.borrow().get("arr") {
        let arr = a.borrow();
        if idx < arr.elems.len() {
            (arr.elems[idx].clone(), false)
        } else {
            (Value::Undefined, true)
        }
    } else if let Some(Value::String(s)) = rec.borrow().get("str") {
        let chars: Vec<char> = s.chars().collect();
        if idx < chars.len() {
            (Value::String(chars[idx].to_string()), false)
        } else {
            (Value::Undefined, true)
        }
    } else {
        return Err(type_err("__yousj$iter_next: bad iterator record"));
    };
    rec.borrow_mut()
        .set("i", Value::Number((idx + 1) as f64));
    let out = Rc::new(RefCell::new(JsObject::new()));
    out.borrow_mut().set("value", value);
    out.borrow_mut().set("done", Value::Bool(done));
    Ok(Value::Object(out))
}

// ---- Number 静态方法 ----

fn number_is_integer(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(matches!(args.first(),
        Some(Value::Number(n)) if n.is_finite() && n.fract() == 0.0)))
}

fn number_is_nan(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    // 与全局 isNaN 不同：不做类型转换，只有真正的 NaN 才 true。
    Ok(Value::Bool(matches!(args.first(),
        Some(Value::Number(n)) if n.is_nan())))
}

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 统一错误：词法 / 解析 / 运行时。
#[derive(Debug, Clone, PartialEq)]
pub enum JsError {
    Lex(LexError),
    Parse(ParseError),
    Runtime(RuntimeError),
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsError::Lex(e) => write!(f, "{}", e),
            JsError::Parse(e) => write!(f, "{}", e),
            JsError::Runtime(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for JsError {}

pub(crate) fn flow_to_js(e: FlowError) -> JsError {
    match e {
        FlowError::Runtime(r) => JsError::Runtime(r),
        FlowError::Thrown(v) => JsError::Runtime(RuntimeError::new(format!(
            "uncaught exception: {}",
            v.to_js_string()
        ))),
    }
}

/// 求值一段 JS 源码，返回最终值。
pub fn eval_source(src: &str) -> Result<Value, JsError> {
    let prog = parse_source(src).map_err(JsError::Parse)?;
    let mut ip = Interpreter::new();
    ip.run(&prog).map_err(flow_to_js)
}

/// 求值并顺带取走 `console.log` 输出：`Ok((value, console_lines))`。
pub fn eval_with_console(src: &str) -> Result<(Value, Vec<String>), JsError> {
    let prog = parse_source(src).map_err(JsError::Parse)?;
    let mut ip = Interpreter::new();
    let v = ip.run(&prog).map_err(flow_to_js)?;
    Ok((v, ip.take_console()))
}

// ---------------------------------------------------------------------------
// Phase 7：Promise / fetch / 正则 / queueMicrotask 的 Native 函数
// ---------------------------------------------------------------------------

/// `Promise.resolve(v)`：已是 Promise 则原样返回，否则 fulfill。
fn native_promise_resolve(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let v = args.into_iter().next().unwrap_or(Value::Undefined);
    if let Value::Promise(p) = &v {
        return Ok(Value::Promise(p.clone()));
    }
    let p = JsPromise::pending();
    settle_promise(&p, false, v, &mut ctx.microtasks);
    Ok(Value::Promise(p))
}

/// `Promise.reject(e)`：已拒绝的 promise；无人处理则记 unhandled。
fn native_promise_reject(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let v = args.into_iter().next().unwrap_or(Value::Undefined);
    let p = JsPromise::pending();
    let (_, unhandled) = settle_promise(&p, true, v, &mut ctx.microtasks);
    if unhandled {
        ctx.unhandled.push(p.clone());
    }
    Ok(Value::Promise(p))
}

/// `Promise.all(arr)`：只接受数组；非 Promise 元素视为已完成值。
fn native_promise_all(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let elems: Vec<Value> = match args.first() {
        Some(Value::Array(a)) => a.borrow().elems.clone(),
        _ => return Err(type_err("Promise.all expects an array")),
    };
    let result = JsPromise::pending();
    if elems.is_empty() {
        let arr = new_js_array(ctx, Vec::new());
        settle_promise(&result, false, arr, &mut ctx.microtasks);
        return Ok(Value::Promise(result));
    }
    let state = Rc::new(RefCell::new(AllState {
        values: vec![None; elems.len()],
        remaining: elems.len(),
        result: result.clone(),
        done: false,
    }));
    for (i, el) in elems.iter().enumerate() {
        match el {
            Value::Promise(p) => {
                let r = Reaction {
                    kind: ReactionKind::All {
                        state: state.clone(),
                        index: i,
                    },
                    next: None,
                    source: p.clone(),
                };
                let was_rejected =
                    matches!(p.borrow().state, PromiseState::Rejected(_));
                let still_unhandled = attach_reaction(p, r, &mut ctx.microtasks);
                if was_rejected && !still_unhandled {
                    ctx.unhandled.retain(|x| !Rc::ptr_eq(x, p));
                }
            }
            _ => {
                // 非 Promise 视为已完成值（同步计入）。
                let mut st = state.borrow_mut();
                if !st.done {
                    st.values[i] = Some(el.clone());
                    st.remaining -= 1;
                    if st.remaining == 0 {
                        st.done = true;
                        let vals: Vec<Value> = st
                            .values
                            .iter()
                            .map(|o| o.clone().unwrap_or(Value::Undefined))
                            .collect();
                        let res = st.result.clone();
                        drop(st);
                        let arr = new_js_array(ctx, vals);
                        settle_promise(&res, false, arr, &mut ctx.microtasks);
                    }
                }
            }
        }
    }
    Ok(Value::Promise(result))
}

/// `Promise.race(arr)`：只接受数组；第一个 settle 的赢。
fn native_promise_race(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let elems: Vec<Value> = match args.first() {
        Some(Value::Array(a)) => a.borrow().elems.clone(),
        _ => return Err(type_err("Promise.race expects an array")),
    };
    let result = JsPromise::pending();
    for el in &elems {
        match el {
            Value::Promise(p) => {
                let r = Reaction {
                    kind: ReactionKind::Race {
                        result: result.clone(),
                    },
                    next: None,
                    source: p.clone(),
                };
                let was_rejected =
                    matches!(p.borrow().state, PromiseState::Rejected(_));
                let still_unhandled = attach_reaction(p, r, &mut ctx.microtasks);
                if was_rejected && !still_unhandled {
                    ctx.unhandled.retain(|x| !Rc::ptr_eq(x, p));
                }
            }
            _ => {
                // 非 Promise 值：若结果仍 pending 则它赢（同步结算，
                // 可观测行为仍是异步的，因为 then 回调走微任务）。
                if JsPromise::is_pending(&result) {
                    settle_promise(&result, false, el.clone(), &mut ctx.microtasks);
                }
            }
        }
    }
    Ok(Value::Promise(result))
}

/// `then` / `catch` / `finally` 的公共实现。
fn promise_then_impl(
    ctx: &mut NativeCtx,
    source: &Value,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
    is_finally: bool,
) -> Result<Value, FlowError> {
    let p = match source {
        Value::Promise(p) => p.clone(),
        _ => {
            return Err(type_err(format!(
                "then called on non-promise ({})",
                source.type_of()
            )))
        }
    };
    // 回调必须是可调用或 undefined（简化校验）。
    for cb in [&on_fulfilled, &on_rejected].into_iter().flatten() {
        if !cb.is_undefined() && !cb.is_callable() {
            return Err(type_err("promise callback must be callable"));
        }
    }
    let next = JsPromise::pending();
    let reaction = Reaction {
        kind: ReactionKind::Handler {
            on_fulfilled,
            on_rejected,
            is_finally,
        },
        next: Some(next.clone()),
        source: p.clone(),
    };
    let was_rejected = matches!(p.borrow().state, PromiseState::Rejected(_));
    let still_unhandled = attach_reaction(&p, reaction, &mut ctx.microtasks);
    if was_rejected && !still_unhandled {
        ctx.unhandled.retain(|x| !Rc::ptr_eq(x, &p));
    }
    Ok(Value::Promise(next))
}

fn native_promise_then(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let mut it = args.into_iter();
    let on_f = it.next().unwrap_or(Value::Undefined);
    let on_r = it.next().unwrap_or(Value::Undefined);
    let on_f = if on_f.is_undefined() { None } else { Some(on_f) };
    let on_r = if on_r.is_undefined() { None } else { Some(on_r) };
    // then 的 this 就是源 promise（get_prop 走原型链，this 透传）。
    let source = ctx.this.clone();
    promise_then_impl(ctx, &source, on_f, on_r, false)
}

fn native_promise_catch(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let on_r = args.into_iter().next().unwrap_or(Value::Undefined);
    let on_r = if on_r.is_undefined() { None } else { Some(on_r) };
    let source = ctx.this.clone();
    promise_then_impl(ctx, &source, None, on_r, false)
}

fn native_promise_finally(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let cb = args.into_iter().next().unwrap_or(Value::Undefined);
    let cb = if cb.is_undefined() { None } else { Some(cb) };
    let source = ctx.this.clone();
    // finally 回调存两份（fulfilled/rejected 都调），is_finally 做透传。
    promise_then_impl(ctx, &source, cb.clone(), cb, true)
}

/// `queueMicrotask(fn)`：入队一个微任务。
fn native_queue_microtask(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let cb = args.into_iter().next().unwrap_or(Value::Undefined);
    if !cb.is_callable() {
        return Err(type_err("queueMicrotask requires a callable"));
    }
    if ctx.microtasks.len() >= MAX_MICROTASKS {
        return Err(FlowError::Runtime(
            RuntimeError::typed(ErrorKind::RangeError, "microtask queue limit exceeded")
                .uncatchable(),
        ));
    }
    ctx.microtasks.push_back(Microtask::Call { callback: cb });
    Ok(Value::Undefined)
}

/// `fetch(url)`：经 FetchHost 同步取回，包装为已决定的 Promise。
/// 未绑定 host → 注定 reject（"fetch not implemented"）。
fn native_fetch(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let url = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let p = JsPromise::pending();
    match &ctx.fetch_host {
        None => {
            let (_, unhandled) = settle_promise(
                &p,
                true,
                Value::String("fetch not implemented".to_string()),
                &mut ctx.microtasks,
            );
            if unhandled {
                ctx.unhandled.push(p.clone());
            }
        }
        Some(host) => match host.fetch(&url) {
            Ok(resp) => {
                let obj = make_fetch_response(ctx, &resp);
                settle_promise(&p, false, obj, &mut ctx.microtasks);
            }
            Err(e) => {
                let (_, unhandled) = settle_promise(
                    &p,
                    true,
                    Value::String(e),
                    &mut ctx.microtasks,
                );
                if unhandled {
                    ctx.unhandled.push(p.clone());
                }
            }
        },
    }
    Ok(Value::Promise(p))
}

/// 构造简化的 Response 对象：`{ ok, status, text() }`。
/// `__body` 为内部属性（text() 读取用；枚举可见，文档化简化）。
fn make_fetch_response(ctx: &NativeCtx, resp: &FetchResponse) -> Value {
    let obj = Rc::new(RefCell::new(JsObject::with_proto(Some(
        ctx.protos.object.clone(),
    ))));
    {
        let mut o = obj.borrow_mut();
        o.set("ok", Value::Bool(resp.ok()));
        o.set("status", Value::Number(resp.status as f64));
        o.set("text", Value::Native(native_response_text));
        o.set("__body", Value::String(resp.body.clone()));
    }
    Value::Object(obj)
}

/// `Response.text()` → `Promise<string>`。
fn native_response_text(
    ctx: &mut NativeCtx,
    _args: Vec<Value>,
) -> Result<Value, FlowError> {
    let body = match &ctx.this {
        Value::Object(o) => match o.borrow().get("__body") {
            Some(Value::String(s)) => s,
            _ => String::new(),
        },
        _ => String::new(),
    };
    let p = JsPromise::pending();
    settle_promise(&p, false, Value::String(body), &mut ctx.microtasks);
    Ok(Value::Promise(p))
}

// ---- 正则 ----

fn this_regexp(ctx: &NativeCtx) -> Result<RegExpRef, FlowError> {
    match &ctx.this {
        Value::RegExp(r) => Ok(r.clone()),
        v => Err(type_err(format!(
            "regexp method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

/// `RegExp.prototype.test(str)`：`/g` 时从 lastIndex 开始，命中推进 lastIndex，
/// 未命中则 lastIndex 归零（规范行为）。
fn native_regexp_test(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let r = this_regexp(ctx)?;
    let text = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let from = {
        let b = r.borrow();
        if b.compiled.flags.global {
            b.last_index
        } else {
            0
        }
    };
    let found = r.borrow().compiled.search(&text, from);
    match found {
        Some(m) => {
            let mut b = r.borrow_mut();
            if b.compiled.flags.global {
                b.last_index = m.index + m.text.chars().count();
            }
            Ok(Value::Bool(true))
        }
        None => {
            let mut b = r.borrow_mut();
            if b.compiled.flags.global {
                b.last_index = 0;
            }
            Ok(Value::Bool(false))
        }
    }
}

/// `RegExp.prototype.exec(str)`：返回 `[full, g1, …]`（带 `index` 属性），
/// 未命中返回 null。`/g` 的 lastIndex 语义同 test。
fn native_regexp_exec(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let r = this_regexp(ctx)?;
    let text = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let from = {
        let b = r.borrow();
        if b.compiled.flags.global {
            b.last_index
        } else {
            0
        }
    };
    let found = r.borrow().compiled.search(&text, from);
    match found {
        Some(m) => {
            {
                let mut b = r.borrow_mut();
                if b.compiled.flags.global {
                    b.last_index = m.index + m.text.chars().count();
                }
            }
            let mut elems = vec![Value::String(m.text)];
            for g in m.groups {
                elems.push(g.map(Value::String).unwrap_or(Value::Undefined));
            }
            let arr = new_js_array(ctx, elems);
            if let Value::Array(a) = &arr {
                a.borrow_mut()
                    .props
                    .set("index", Value::Number(m.index as f64));
            }
            Ok(arr)
        }
        None => {
            let mut b = r.borrow_mut();
            if b.compiled.flags.global {
                b.last_index = 0;
            }
            Ok(Value::Null)
        }
    }
}

/// 把一次正则匹配转成 `exec` 风格的数组值（String 方法共用，不碰 lastIndex）。
fn regexp_match_array(
    ctx: &NativeCtx,
    m: crate::regex::RegexMatch,
) -> Value {
    let mut elems = vec![Value::String(m.text)];
    for g in m.groups {
        elems.push(g.map(Value::String).unwrap_or(Value::Undefined));
    }
    let arr = new_js_array(ctx, elems);
    if let Value::Array(a) = &arr {
        a.borrow_mut()
            .props
            .set("index", Value::Number(m.index as f64));
    }
    arr
}

/// `String.prototype.match(regexp)`：
/// - `/g` → 所有匹配文本的数组（无匹配返回 null）；
/// - 非 `/g` → 等价于 `regexp.exec(str)`（不碰 lastIndex）；
/// - 非正则 → 按字符串字面量找第一个（简化）。
fn string_match(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    match args.first() {
        Some(Value::RegExp(r)) => {
            let global = r.borrow().compiled.flags.global;
            if global {
                let mut out = Vec::new();
                let mut from = 0usize;
                let len = s.chars().count();
                loop {
                    if from > len {
                        break;
                    }
                    match r.borrow().compiled.search(&s, from) {
                        Some(m) => {
                            let end = m.index + m.text.chars().count();
                            out.push(Value::String(m.text));
                            // 空匹配前进一步，防死循环。
                            from = if end == from { from + 1 } else { end };
                        }
                        None => break,
                    }
                }
                if out.is_empty() {
                    Ok(Value::Null)
                } else {
                    Ok(new_js_array(ctx, out))
                }
            } else {
                match r.borrow().compiled.search(&s, 0) {
                    Some(m) => Ok(regexp_match_array(ctx, m)),
                    None => Ok(Value::Null),
                }
            }
        }
        _ => {
            // 非正则：按字面量找第一个，找到返回 [match]，否则 null（简化）。
            let pat = args.first().map(|v| v.to_js_string()).unwrap_or_default();
            match s.find(pat.as_str()) {
                Some(_) => Ok(new_js_array(ctx, vec![Value::String(pat)])),
                None => Ok(Value::Null),
            }
        }
    }
}

/// `String.prototype.split` 接正则（在原函数上扩展，见下）。
/// `String.prototype.replace` 的 Native 形态：处理字符串 replacer；
/// 函数 replacer 必须走方法调用形态（try_host_method 拦截进解释器）。
fn string_replace(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let replacer = args.get(1).cloned().unwrap_or(Value::Undefined);
    if replacer.is_callable() {
        return Err(rt(
            "replace with function replacer needs the interpreter (use str.replace(...) call form)",
        ));
    }
    let s = this_string(ctx)?;
    let pattern = args.first().cloned().unwrap_or(Value::Undefined);
    let rep = replacer.to_js_string();
    let result = match &pattern {
        Value::RegExp(r) => {
            let b = r.borrow();
            let global = b.compiled.flags.global;
            let mut out = String::new();
            let chars: Vec<char> = s.chars().collect();
            let mut last = 0usize;
            let mut from = 0usize;
            let len = chars.len();
            loop {
                if from > len {
                    break;
                }
                match b.compiled.search(&s, from) {
                    Some(m) => {
                        let start = m.index;
                        let end = start + m.text.chars().count();
                        out.extend(chars[last..start].iter());
                        // `$` 模式暂不支持，按字面量处理（文档化）。
                        out.push_str(&rep);
                        last = end;
                        if !global {
                            break;
                        }
                        from = if end == from { from + 1 } else { end };
                    }
                    None => break,
                }
            }
            out.extend(chars[last..].iter());
            out
        }
        _ => {
            let pat = pattern.to_js_string();
            match s.find(pat.as_str()) {
                Some(byte_idx) => {
                    let mut out = s[..byte_idx].to_string();
                    out.push_str(&rep);
                    out.push_str(&s[byte_idx + pat.len()..]);
                    out
                }
                None => s,
            }
        }
    };
    Ok(Value::String(result))
}

// ---- ArrayBuffer / DataView / TypedArray ----

/// 新建指定长度的 TypedArray（自带零初始化 ArrayBuffer）。
/// phase 13：任务回调抛错的 console 文案（`drain_tasks` 各分支共用，
/// `webapi.rs` 的事件分发也用）。
pub(crate) fn task_error_msg(e: &FlowError) -> String {
    match e {
        FlowError::Runtime(r) => r.to_string(),
        FlowError::Thrown(v) => {
            format!("uncaught exception: {}", v.to_js_string())
        }
    }
}

/// phase 13：`pub(crate)` 化（`webapi.rs` 的 `TextEncoder.encode` 用）。
pub(crate) fn ta_new(kind: TypedKind, len: usize) -> TypedArrayRef {
    let buf = Rc::new(RefCell::new(JsArrayBuffer {
        bytes: Rc::new(RefCell::new(vec![0u8; len * kind.bytes()])),
    }));
    Rc::new(RefCell::new(JsTypedArray {
        buffer: buf,
        byte_offset: 0,
        len,
        kind,
    }))
}

/// TypedArray 元素快照（读进 Vec<f64>）。
fn ta_snapshot(t: &TypedArrayRef) -> (TypedKind, usize, Vec<f64>) {
    let ta = t.borrow();
    let buf = ta.buffer.borrow();
    let bytes = buf.bytes.borrow();
    let vals: Vec<f64> = (0..ta.len).map(|i| ta.read_at(&bytes, i)).collect();
    (ta.kind, ta.len, vals)
}

fn native_arraybuffer_slice(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let b = this_arraybuffer(ctx)?;
    let bb = b.borrow();
    let bytes = bb.bytes.borrow();
    let len = bytes.len();
    let start = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let end = args.get(1).map(|v| v.to_number()).unwrap_or(len as f64);
    let s = start.max(0.0).min(len as f64) as usize;
    let e = end.max(0.0).min(len as f64) as usize;
    let s = if s > e { e } else { s };
    let new_bytes = bytes[s..e].to_vec();
    drop(bytes);
    Ok(Value::ArrayBuffer(Rc::new(RefCell::new(JsArrayBuffer {
        bytes: Rc::new(RefCell::new(new_bytes)),
    }))))
}

// ---- DataView 读写 ----

/// DataView 偏移校验：返回 (buffer字节, 绝对偏移)。
fn dv_at(d: &DataViewRef, off: f64, size: usize) -> Result<(ArrayBufferRef, usize), FlowError> {
    let dv = d.borrow();
    if !off.is_finite() || off < 0.0 {
        return Err(type_err("DataView offset out of range"));
    }
    let o = off as usize;
    if o + size > dv.byte_len {
        return Err(type_err("DataView offset out of range"));
    }
    Ok((dv.buffer.clone(), dv.byte_offset + o))
}

macro_rules! dv_get {
    ($name:ident, $ty:ty, $from:ident, $size:expr) => {
        fn $name(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
            let d = this_dataview(ctx)?;
            let off = args.first().map(|v| v.to_number()).unwrap_or(0.0);
            let le = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
            let (buf, abs) = dv_at(&d, off, $size)?;
            let bbb = buf.borrow();
            let bytes = bbb.bytes.borrow();
            let b: &[u8] = &bytes[abs..abs + $size];
            let v: $ty = if le {
                <$ty>::$from(b.try_into().unwrap())
            } else {
                <$ty>::from_be_bytes(b.try_into().unwrap())
            };
            Ok(Value::Number(v as f64))
        }
    };
}

macro_rules! dv_set {
    ($name:ident, $ty:ty, $to:ident, $size:expr, $conv:expr) => {
        fn $name(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
            let d = this_dataview(ctx)?;
            let off = args.first().map(|v| v.to_number()).unwrap_or(0.0);
            let le = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);
            let (buf, abs) = dv_at(&d, off, $size)?;
            let raw = args.get(1).map(|v| v.to_number()).unwrap_or(0.0);
            let v: $ty = $conv(raw);
            let bbb = buf.borrow();
            let mut bytes = bbb.bytes.borrow_mut();
            let dst: &mut [u8] = &mut bytes[abs..abs + $size];
            let enc = if le { v.$to() } else { v.to_be_bytes() };
            dst.copy_from_slice(&enc);
            Ok(Value::Undefined)
        }
    };
}

dv_get!(native_dv_get_int8, i8, from_be_bytes, 1);
dv_get!(native_dv_get_uint8, u8, from_be_bytes, 1);
dv_get!(native_dv_get_int16, i16, from_le_bytes, 2);
dv_get!(native_dv_get_uint16, u16, from_le_bytes, 2);
dv_get!(native_dv_get_int32, i32, from_le_bytes, 4);
dv_get!(native_dv_get_uint32, u32, from_le_bytes, 4);
dv_get!(native_dv_get_float32, f32, from_le_bytes, 4);
dv_get!(native_dv_get_float64, f64, from_le_bytes, 8);

dv_set!(native_dv_set_int8, i8, to_le_bytes, 1, |v: f64| v as i8);
dv_set!(native_dv_set_uint8, u8, to_le_bytes, 1, |v: f64| v as u8);
dv_set!(native_dv_set_int16, i16, to_le_bytes, 2, |v: f64| v as i16);
dv_set!(native_dv_set_uint16, u16, to_le_bytes, 2, |v: f64| v as u16);
dv_set!(native_dv_set_int32, i32, to_le_bytes, 4, |v: f64| v as i32);
dv_set!(native_dv_set_uint32, u32, to_le_bytes, 4, |v: f64| v as u32);
dv_set!(native_dv_set_float32, f32, to_le_bytes, 4, |v: f64| v as f32);
dv_set!(native_dv_set_float64, f64, to_le_bytes, 8, |v: f64| v);

// ---- TypedArray 方法 ----

fn native_ta_set(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let offset = args.get(1).map(|v| v.to_number()).unwrap_or(0.0) as usize;
    let src_vals: Vec<f64> = match args.first() {
        Some(Value::Array(a)) => a.borrow().elems.iter().map(|v| v.to_number()).collect(),
        Some(Value::TypedArray(s)) => ta_snapshot(s).2,
        _ => return Err(type_err("TypedArray.set requires an array or TypedArray")),
    };
    let ta = t.borrow();
    if offset + src_vals.len() > ta.len {
        return Err(type_err("TypedArray.set: source too long"));
    }
    let tabuf = ta.buffer.borrow();
    {
        let mut bytes = tabuf.bytes.borrow_mut();
        for (i, v) in src_vals.into_iter().enumerate() {
            ta.write_at(&mut bytes, offset + i, v);
        }
    }
    Ok(Value::Undefined)
}

fn native_ta_subarray(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let ta = t.borrow();
    let len = ta.len;
    let begin = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let end = args.get(1).map(|v| v.to_number()).unwrap_or(len as f64);
    let b = begin.max(0.0).min(len as f64) as usize;
    let e = end.max(0.0).min(len as f64) as usize;
    let b = b.min(e);
    let view = JsTypedArray {
        buffer: ta.buffer.clone(),
        byte_offset: ta.byte_offset + b * ta.kind.bytes(),
        len: e - b,
        kind: ta.kind,
    };
    drop(ta);
    Ok(Value::TypedArray(Rc::new(RefCell::new(view))))
}

fn native_ta_slice(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let (kind, len, vals) = ta_snapshot(&t);
    let begin = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let end = args.get(1).map(|v| v.to_number()).unwrap_or(len as f64);
    let b = begin.max(0.0).min(len as f64) as usize;
    let e = end.max(0.0).min(len as f64) as usize;
    let b = b.min(e);
    let out = ta_new(kind, e - b);
    {
        let nt = out.borrow();
        let ntbuf = nt.buffer.borrow();
        let mut bytes = ntbuf.bytes.borrow_mut();
        for (i, v) in vals[b..e].iter().enumerate() {
            nt.write_at(&mut bytes, i, *v);
        }
    }
    Ok(Value::TypedArray(out))
}

fn native_ta_fill(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let v = args.first().map(|x| x.to_number()).unwrap_or(0.0);
    let ta = t.borrow();
    let len = ta.len;
    let start = args.get(1).map(|x| x.to_number()).unwrap_or(0.0);
    let end = args.get(2).map(|x| x.to_number()).unwrap_or(len as f64);
    let s = start.max(0.0).min(len as f64) as usize;
    let e = end.max(0.0).min(len as f64) as usize;
    let tabuf = ta.buffer.borrow();
    let mut bytes = tabuf.bytes.borrow_mut();
    for i in s..e.max(s) {
        ta.write_at(&mut bytes, i, v);
    }
    drop(bytes);
    drop(tabuf);
    drop(ta);
    Ok(ctx.this.clone())
}

fn native_ta_indexof(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let target = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let (_, _, vals) = ta_snapshot(&t);
    for (i, v) in vals.iter().enumerate() {
        if *v == target || (v.is_nan() && target.is_nan()) {
            return Ok(Value::Number(i as f64));
        }
    }
    Ok(Value::Number(-1.0))
}

fn native_ta_includes(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let target = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let (_, _, vals) = ta_snapshot(&t);
    Ok(Value::Bool(
        vals.iter().any(|v| *v == target || (v.is_nan() && target.is_nan())),
    ))
}

fn native_ta_join(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let sep = args
        .first()
        .map(|v| v.to_js_string())
        .unwrap_or_else(|| ",".to_string());
    let (_, _, vals) = ta_snapshot(&t);
    Ok(Value::String(
        vals.iter()
            .map(|v| Value::Number(*v).to_js_string())
            .collect::<Vec<_>>()
            .join(&sep),
    ))
}

fn native_ta_at(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let i = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let (_, len, vals) = ta_snapshot(&t);
    let idx = if i < 0.0 { len as f64 + i } else { i } as usize;
    if i < 0.0 && (len as f64 + i) < 0.0 {
        return Ok(Value::Undefined);
    }
    if idx < len {
        Ok(Value::Number(vals[idx]))
    } else {
        Ok(Value::Undefined)
    }
}

fn native_ta_values(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let (_, _, vals) = ta_snapshot(&t);
    Ok(js_array_of(
        ctx,
        vals.into_iter().map(Value::Number).collect(),
    ))
}

fn native_ta_keys(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let (_, len, _) = ta_snapshot(&t);
    Ok(js_array_of(
        ctx,
        (0..len).map(|i| Value::Number(i as f64)).collect(),
    ))
}

fn native_ta_entries(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let t = this_typedarray(ctx)?;
    let (_, _, vals) = ta_snapshot(&t);
    let es: Vec<Value> = vals
        .into_iter()
        .enumerate()
        .map(|(i, v)| js_array_of(ctx, vec![Value::Number(i as f64), Value::Number(v)]))
        .collect();
    Ok(js_array_of(ctx, es))
}

// ---- Date ----

/// 当前 UTC 毫秒（系统时钟）。
fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn native_date_now(_ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Number(now_ms()))
}

fn native_date_utc(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    // Date.UTC(y, mo, ...) —— 复用构造器的多参分支逻辑（UTC）。
    let y = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
    if !y.is_finite() {
        return Ok(Value::Number(f64::NAN));
    }
    let mo = args.get(1).map(|v| v.to_number()).unwrap_or(0.0);
    let d = args.get(2).map(|v| v.to_number()).unwrap_or(1.0);
    let h = args.get(3).map(|v| v.to_number()).unwrap_or(0.0);
    let mi = args.get(4).map(|v| v.to_number()).unwrap_or(0.0);
    let s = args.get(5).map(|v| v.to_number()).unwrap_or(0.0);
    let ms2 = args.get(6).map(|v| v.to_number()).unwrap_or(0.0);
    let m1 = (mo as i64).rem_euclid(12) + 1;
    let y2 = if m1 <= 2 { y - 1.0 } else { y };
    let era = (if y2 >= 0.0 { y2 } else { y2 - 399.0 } / 400.0).floor();
    let yoe = y2 - era * 400.0;
    let mp = if m1 > 2 { m1 - 3 } else { m1 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe as i64 * 365 + yoe as i64 / 4 - yoe as i64 / 100 + doy;
    let days = era as i64 * 146097 + doe - 719468;
    Ok(Value::Number(
        days as f64 * 86400000.0 + h * 3600000.0 + mi * 60000.0 + s * 1000.0 + ms2,
    ))
}

fn native_date_gettime(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Number(this_date(ctx)?.borrow().ms))
}

fn native_date_toiso(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let ms = this_date(ctx)?.borrow().ms;
    let (y, mo, d, hh, mm, ss, _) = utc_civil(ms);
    let millis = (ms.rem_euclid(1000.0)) as u32;
    Ok(Value::String(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, hh, mm, ss, millis
    )))
}

fn native_date_tostring(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::String(format_utc_date(this_date(ctx)?.borrow().ms)))
}

macro_rules! date_get_utc {
    ($name:ident, $conv:expr) => {
        fn $name(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
            let ms = this_date(ctx)?.borrow().ms;
            let parts = utc_civil(ms);
            let v: f64 = $conv(parts);
            Ok(Value::Number(v))
        }
    };
}

date_get_utc!(native_date_getutc_full_year, |(y, _, _, _, _, _, _)| y as f64);
date_get_utc!(native_date_getutc_month, |(_, m, _, _, _, _, _)| (m - 1) as f64);
date_get_utc!(native_date_getutc_date, |(_, _, d, _, _, _, _)| d as f64);
date_get_utc!(native_date_getutc_day, |(_, _, _, _, _, _, w)| w as f64);
date_get_utc!(native_date_getutc_hours, |(_, _, _, h, _, _, _)| h as f64);
date_get_utc!(native_date_getutc_minutes, |(_, _, _, _, mi, _, _)| mi as f64);
date_get_utc!(native_date_getutc_seconds, |(_, _, _, _, _, s, _)| s as f64);
fn native_date_getutc_milliseconds(
    ctx: &mut NativeCtx,
    _args: Vec<Value>,
) -> Result<Value, FlowError> {
    Ok(Value::Number(this_date(ctx)?.borrow().ms.rem_euclid(1000.0)))
}

// ---- Intl ----

/// 读取格式化器对象的 `__*` 配置属性。
fn intl_prop(this: &Value, key: &str) -> Value {
    match this {
        Value::Object(o) => o.borrow().get_in_chain(key).unwrap_or(Value::Undefined),
        _ => Value::Undefined,
    }
}

/// locale 规范化：大小写修正；未知 → en-US。
fn intl_canon_locale(s: &str) -> String {
    let lower = s.to_lowercase().replace('_', "-");
    for cand in ["en-US", "zh-CN", "de-DE", "fr-FR", "ja-JP"] {
        if cand.to_lowercase() == lower {
            return cand.to_string();
        }
    }
    // 前缀匹配（如 "en" → "en-US"）。
    let lang = lower.split('-').next().unwrap_or("");
    match lang {
        "en" => "en-US",
        "zh" => "zh-CN",
        "de" => "de-DE",
        "fr" => "fr-FR",
        "ja" => "ja-JP",
        _ => "en-US",
    }
    .to_string()
}

/// locale 的 (千分位, 小数点)。
fn intl_num_symbols(locale: &str) -> (&'static str, &'static str) {
    match locale {
        "de-DE" => (".", ","),
        "fr-FR" => (" ", ","),
        _ => (",", "."),
    }
}

fn intl_currency_symbol(code: &str) -> &'static str {
    match code {
        "USD" => "$",
        "CNY" => "¥",
        "EUR" => "€",
        "JPY" => "¥",
        "GBP" => "£",
        _ => "",
    }
}

/// 数字 → 分组字符串（整数部分按千分位分组；小数按 max_frac 截断，
/// 尾零只保留到 min_frac 位）。
fn intl_format_decimal(
    abs: f64,
    min_frac: u32,
    max_frac: u32,
    group: &str,
    decimal: &str,
    grouping: bool,
) -> String {
    let mut rounded = format!("{:.*}", max_frac as usize, abs);
    // 去尾零（保留至少 min_frac 位）。
    if max_frac > min_frac {
        if let Some(dot) = rounded.find('.') {
            let mut end = rounded.len();
            while end > dot + 1 + min_frac as usize && rounded.as_bytes()[end - 1] == b'0' {
                end -= 1;
            }
            // 若小数部分被清空到 min_frac==0，顺带去掉小数点。
            if min_frac == 0 && end == dot + 1 {
                end = dot;
            }
            rounded.truncate(end);
        }
    }
    let mut it = rounded.split('.');
    let int_part = it.next().unwrap_or("0");
    let frac_part = it.next().unwrap_or("");
    let grouped = if grouping && int_part.len() > 3 {
        let chars: Vec<char> = int_part.chars().collect();
        let mut out = String::new();
        let first = chars.len() % 3;
        let mut i = 0;
        if first > 0 {
            out.extend(chars[..first].iter());
            i = first;
        }
        while i < chars.len() {
            if !out.is_empty() {
                out.push_str(group);
            }
            out.extend(chars[i..i + 3].iter());
            i += 3;
        }
        out
    } else {
        int_part.to_string()
    };
    if !frac_part.is_empty() || min_frac > 0 {
        let mut fp = frac_part.to_string();
        while fp.len() < min_frac as usize {
            fp.push('0');
        }
        format!("{}{}{}", grouped, decimal, fp)
    } else {
        grouped
    }
}

struct NfConfig {
    locale: String,
    style: String,
    currency: String,
    min_fd: i64,
    max_fd: i64,
    grouping: bool,
}

fn nf_config(this: &Value) -> NfConfig {
    let num = |k: &str, dflt: i64| match intl_prop(this, k) {
        Value::Number(n) if n >= 0.0 => n as i64,
        _ => dflt,
    };
    NfConfig {
        locale: intl_canon_locale(&intl_prop(this, "__locale").to_js_string()),
        style: intl_prop(this, "__style").to_js_string(),
        currency: intl_prop(this, "__currency").to_js_string(),
        min_fd: num("__min_fd", -1),
        max_fd: num("__max_fd", -1),
        grouping: intl_prop(this, "__grouping").to_boolean(),
    }
}

/// NumberFormat 核心：返回 (符号前缀, 主体, 符号后缀) 三段。
fn nf_format_parts(cfg: &NfConfig, x: f64) -> (String, String, String) {
    if x.is_nan() {
        return ("".to_string(), "NaN".to_string(), "".to_string());
    }
    if x.is_infinite() {
        return (
            if x < 0.0 { "-" } else { "" }.to_string(),
            "∞".to_string(),
            "".to_string(),
        );
    }
    let (group, decimal) = intl_num_symbols(&cfg.locale);
    let neg = x < 0.0 || (x == 0.0 && 1.0 / x < 0.0);
    let sign = if neg { "-" } else { "" };
    let (body_x, pre, post, min_fd, max_fd): (f64, String, &str, u32, u32) =
        match cfg.style.as_str() {
            "percent" => {
                let (mn, mx) = (
                    if cfg.min_fd >= 0 { cfg.min_fd as u32 } else { 0 },
                    if cfg.max_fd >= 0 { cfg.max_fd as u32 } else { 0 },
                );
                (x * 100.0, String::new(), "%", mn, mx)
            }
            "currency" => {
                let sym = intl_currency_symbol(&cfg.currency);
                let (mn, mx) = (
                    if cfg.min_fd >= 0 { cfg.min_fd as u32 } else { 2 },
                    if cfg.max_fd >= 0 { cfg.max_fd as u32 } else { 2 },
                );
                // USD/en-US: $1,234.50；简写：符号前缀。
                (x, sym.to_string(), "", mn, mx)
            }
            _ => {
                let (mn, mx) = (
                    if cfg.min_fd >= 0 { cfg.min_fd as u32 } else { 0 },
                    if cfg.max_fd >= 0 { cfg.max_fd as u32 } else { 3 },
                );
                (x, String::new(), "", mn, mx)
            }
        };
    let frac = max_fd.max(min_fd);
    let body = intl_format_decimal(body_x.abs(), min_fd, frac, group, decimal, cfg.grouping);
    (format!("{}{}", sign, pre), body, post.to_string())
}

fn native_intl_nf_format(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let cfg = nf_config(&ctx.this);
    let x = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
    let (pre, body, post) = nf_format_parts(&cfg, x);
    Ok(Value::String(format!("{}{}{}", pre, body, post)))
}

fn native_intl_nf_format_to_parts(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let cfg = nf_config(&ctx.this);
    let x = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
    let (group, decimal) = intl_num_symbols(&cfg.locale);
    let (pre, body, post) = nf_format_parts(&cfg, x);
    let mut parts: Vec<Value> = Vec::new();
    let mut part = |t: &str, v: &str| {
        let o = Rc::new(RefCell::new(JsObject::new()));
        o.borrow_mut().set("type", Value::String(t.to_string()));
        o.borrow_mut().set("value", Value::String(v.to_string()));
        parts.push(Value::Object(o));
    };
    // 前缀：拆 minusSign / currency。
    if pre.starts_with('-') {
        part("minusSign", "-");
    }
    let pre_rest = pre.trim_start_matches('-');
    if !pre_rest.is_empty() {
        part("currency", pre_rest);
    }
    // 主体：按 group/decimal 切分 integer/group/decimal/fraction。
    if body == "NaN" || body == "∞" {
        part(if body == "NaN" { "nan" } else { "infinity" }, &body);
    } else {
        // 先按小数点切。
        let mut sp = body.splitn(2, decimal);
        let int_g = sp.next().unwrap_or("");
        let frac = sp.next();
        // 整数部分按千分位切。
        let mut first = true;
        for chunk in int_g.split(group) {
            if !first {
                part("group", group);
            }
            part("integer", chunk);
            first = false;
        }
        if let Some(f) = frac {
            part("decimal", decimal);
            part("fraction", f);
        }
    }
    if post == "%" {
        part("percentSign", "%");
    } else if !post.is_empty() {
        part("literal", &post);
    }
    Ok(js_array_of(ctx, parts))
}

fn native_intl_nf_resolved(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let cfg = nf_config(&ctx.this);
    let o = Rc::new(RefCell::new(JsObject::new()));
    {
        let mut oo = o.borrow_mut();
        oo.set("locale", Value::String(cfg.locale));
        oo.set("style", Value::String(cfg.style.clone()));
        if cfg.style == "currency" {
            oo.set("currency", Value::String(cfg.currency));
        }
        let (dmin, dmax) = match cfg.style.as_str() {
            "currency" => (2, 2),
            "percent" => (0, 0),
            _ => (0, 3),
        };
        oo.set(
            "minimumFractionDigits",
            Value::Number(if cfg.min_fd >= 0 { cfg.min_fd as f64 } else { dmin as f64 }),
        );
        oo.set(
            "maximumFractionDigits",
            Value::Number(if cfg.max_fd >= 0 { cfg.max_fd as f64 } else { dmax as f64 }),
        );
        oo.set("useGrouping", Value::Bool(cfg.grouping));
        oo.set("numberingSystem", Value::String("latn".to_string()));
    }
    Ok(Value::Object(o))
}

// ---- Intl.DateTimeFormat ----

const MONTH_LONG_EN: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const MONTH_SHORT_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WD_LONG_EN: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];
const WD_SHORT_EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn df_fields(this: &Value) -> Vec<(String, String)> {
    match intl_prop(this, "__fields") {
        Value::Array(a) => a
            .borrow()
            .elems
            .iter()
            .filter_map(|p| match p {
                Value::Array(pa) => {
                    let e = pa.borrow();
                    Some((
                        e.elems.first().map(|v| v.to_js_string()).unwrap_or_default(),
                        e.elems.get(1).map(|v| v.to_js_string()).unwrap_or_default(),
                    ))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// DateTimeFormat 核心：返回 parts（(type, value) 序列）。
fn df_format_parts(
    locale: &str,
    fields: &[(String, String)],
    ms: f64,
) -> Vec<(String, String)> {
    let (y, mo, d, hh, mm, ss, wd) = utc_civil(ms);
    let get = |k: &str| fields.iter().find(|(fk, _)| fk == k).map(|(_, v)| v.as_str());
    let has_date = ["year", "month", "day", "weekday"].iter().any(|k| get(k).is_some());
    let has_time = ["hour", "minute", "second"].iter().any(|k| get(k).is_some());
    let mut parts: Vec<(String, String)> = Vec::new();

    // weekday 前缀（en-US: "Monday, ..."）。
    if let Some(style) = get("weekday") {
        let name = match style {
            "long" => WD_LONG_EN[wd as usize],
            _ => WD_SHORT_EN[wd as usize],
        };
        parts.push(("weekday".to_string(), name.to_string()));
        parts.push(("literal".to_string(), ", ".to_string()));
    }
    // 日期。
    if get("year").is_some() || get("month").is_some() || get("day").is_some() {
        let ys = match get("year").unwrap_or("numeric") {
            "2-digit" => format!("{:02}", (y % 100).abs()),
            _ => format!("{}", y),
        };
        let ms_ = match get("month").unwrap_or("numeric") {
            "2-digit" => format!("{:02}", mo),
            "long" => MONTH_LONG_EN[(mo - 1) as usize].to_string(),
            "short" => MONTH_SHORT_EN[(mo - 1) as usize].to_string(),
            _ => format!("{}", mo),
        };
        let ds = match get("day").unwrap_or("numeric") {
            "2-digit" => format!("{:02}", d),
            _ => format!("{}", d),
        };
        match locale {
            "zh-CN" | "ja-JP" => {
                parts.push(("year".to_string(), ys));
                parts.push(("literal".to_string(), "/".to_string()));
                parts.push(("month".to_string(), ms_));
                parts.push(("literal".to_string(), "/".to_string()));
                parts.push(("day".to_string(), ds));
            }
            "de-DE" => {
                parts.push(("day".to_string(), ds));
                parts.push(("literal".to_string(), ".".to_string()));
                parts.push(("month".to_string(), ms_));
                parts.push(("literal".to_string(), ".".to_string()));
                parts.push(("year".to_string(), ys));
            }
            "fr-FR" => {
                parts.push(("day".to_string(), ds));
                parts.push(("literal".to_string(), "/".to_string()));
                parts.push(("month".to_string(), ms_));
                parts.push(("literal".to_string(), "/".to_string()));
                parts.push(("year".to_string(), ys));
            }
            _ => {
                // en-US: M/D/YYYY
                parts.push(("month".to_string(), ms_));
                parts.push(("literal".to_string(), "/".to_string()));
                parts.push(("day".to_string(), ds));
                parts.push(("literal".to_string(), "/".to_string()));
                parts.push(("year".to_string(), ys));
            }
        }
    }
    if has_date && has_time {
        parts.push(("literal".to_string(), ", ".to_string()));
    }
    // 时间。
    if has_time {
        let use_12h = locale == "en-US";
        let hs = match get("hour").unwrap_or("numeric") {
            "2-digit" => {
                if use_12h {
                    let h12 = if hh % 12 == 0 { 12 } else { hh % 12 };
                    format!("{:02}", h12)
                } else {
                    format!("{:02}", hh)
                }
            }
            _ => {
                if use_12h {
                    format!("{}", if hh % 12 == 0 { 12 } else { hh % 12 })
                } else {
                    format!("{}", hh)
                }
            }
        };
        parts.push(("hour".to_string(), hs));
        if get("minute").is_some() {
            parts.push(("literal".to_string(), ":".to_string()));
            parts.push(("minute".to_string(), format!("{:02}", mm)));
        }
        if get("second").is_some() {
            parts.push(("literal".to_string(), ":".to_string()));
            parts.push(("second".to_string(), format!("{:02}", ss)));
        }
        if use_12h {
            parts.push(("literal".to_string(), " ".to_string()));
            parts.push((
                "dayPeriod".to_string(),
                if hh < 12 { "AM" } else { "PM" }.to_string(),
            ));
        }
    }
    // 无任何字段 → 默认短日期（en-US M/D/YYYY）。
    if parts.is_empty() {
        parts.push(("month".to_string(), format!("{}", mo)));
        parts.push(("literal".to_string(), "/".to_string()));
        parts.push(("day".to_string(), format!("{}", d)));
        parts.push(("literal".to_string(), "/".to_string()));
        parts.push(("year".to_string(), format!("{}", y)));
    }
    parts
}

fn df_ms_arg(args: &[Value]) -> f64 {
    match args.first() {
        Some(Value::Date(d)) => d.borrow().ms,
        Some(v) => v.to_number(),
        None => now_ms(),
    }
}

fn native_intl_df_format(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let locale = intl_canon_locale(&intl_prop(&ctx.this, "__locale").to_js_string());
    let fields = df_fields(&ctx.this);
    let ms = df_ms_arg(&args);
    let s: String = df_format_parts(&locale, &fields, ms)
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    Ok(Value::String(s))
}

fn native_intl_df_format_to_parts(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let locale = intl_canon_locale(&intl_prop(&ctx.this, "__locale").to_js_string());
    let fields = df_fields(&ctx.this);
    let ms = df_ms_arg(&args);
    let parts: Vec<Value> = df_format_parts(&locale, &fields, ms)
        .into_iter()
        .map(|(t, v)| {
            let o = Rc::new(RefCell::new(JsObject::new()));
            o.borrow_mut().set("type", Value::String(t));
            o.borrow_mut().set("value", Value::String(v));
            Value::Object(o)
        })
        .collect();
    Ok(js_array_of(ctx, parts))
}

fn native_intl_df_resolved(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let locale = intl_canon_locale(&intl_prop(&ctx.this, "__locale").to_js_string());
    let fields = df_fields(&ctx.this);
    let o = Rc::new(RefCell::new(JsObject::new()));
    {
        let mut oo = o.borrow_mut();
        oo.set("locale", Value::String(locale));
        oo.set("timeZone", Value::String("UTC".to_string()));
        for (k, v) in fields {
            oo.set(&k, Value::String(v));
        }
    }
    Ok(Value::Object(o))
}

// ---- String 查漏补缺 ----

fn native_str_padstart(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let target = args.first().map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
    let pad = args.get(1).map(|v| v.to_js_string()).unwrap_or_else(|| " ".to_string());
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= target || pad.is_empty() {
        return Ok(Value::String(s));
    }
    let need = target - chars.len();
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut out = String::new();
    for i in 0..need {
        out.push(pad_chars[i % pad_chars.len()]);
    }
    out.push_str(&s);
    Ok(Value::String(out))
}

fn native_str_padend(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let target = args.first().map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
    let pad = args.get(1).map(|v| v.to_js_string()).unwrap_or_else(|| " ".to_string());
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= target || pad.is_empty() {
        return Ok(Value::String(s));
    }
    let need = target - chars.len();
    let pad_chars: Vec<char> = pad.chars().collect();
    let mut out = s;
    for i in 0..need {
        out.push(pad_chars[i % pad_chars.len()]);
    }
    Ok(Value::String(out))
}

fn native_str_repeat(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let n = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    if !n.is_finite() || n < 0.0 {
        return Err(type_err("repeat count must be a non-negative finite number"));
    }
    Ok(Value::String(s.repeat(n as usize)))
}

fn native_str_trimstart(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::String(this_string(ctx)?.trim_start().to_string()))
}

fn native_str_trimend(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::String(this_string(ctx)?.trim_end().to_string()))
}

fn native_str_replaceall(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let pat = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    let rep = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
    if pat.is_empty() {
        return Ok(Value::String(s));
    }
    Ok(Value::String(s.replace(&pat, &rep)))
}

fn native_str_at(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let chars: Vec<char> = s.chars().collect();
    let i = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let idx = if i < 0.0 {
        chars.len() as i64 + i as i64
    } else {
        i as i64
    };
    if idx < 0 || idx as usize >= chars.len() {
        return Ok(Value::Undefined);
    }
    Ok(Value::String(chars[idx as usize].to_string()))
}

fn native_str_lastindexof(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let pat = args.first().map(|v| v.to_js_string()).unwrap_or_default();
    if pat.is_empty() {
        return Ok(Value::Number(s.chars().count() as f64));
    }
    match s.rfind(&pat) {
        Some(byte_idx) => Ok(Value::Number(s[..byte_idx].chars().count() as f64)),
        None => Ok(Value::Number(-1.0)),
    }
}

fn native_str_codepointat(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_string(ctx)?;
    let i = args.first().map(|v| v.to_number()).unwrap_or(0.0) as usize;
    match s.chars().nth(i) {
        Some(c) => Ok(Value::Number(c as u32 as f64)),
        None => Ok(Value::Undefined),
    }
}

fn native_str_normalize(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    // 子集：NFC 归一化未实现，直返原串（文档化偏差）。
    Ok(Value::String(this_string(ctx)?))
}

fn native_str_fromcharcode(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s: String = args
        .iter()
        .map(|v| {
            let n = v.to_number() as u16;
            char::from_u32(n as u32).unwrap_or('\u{FFFD}')
        })
        .collect();
    Ok(Value::String(s))
}

// ---- Array 查漏补缺 ----

fn native_arr_flat(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    let depth = args.first().map(|v| v.to_number()).unwrap_or(1.0);
    let depth = if depth.is_infinite() && depth > 0.0 {
        usize::MAX
    } else {
        depth.max(0.0) as usize
    };
    fn flatten(elems: &[Value], depth: usize, out: &mut Vec<Value>) {
        for e in elems {
            match e {
                Value::Array(a) if depth > 0 => {
                    flatten(&a.borrow().elems.clone(), depth - 1, out)
                }
                v => out.push(v.clone()),
            }
        }
    }
    let mut out = Vec::new();
    flatten(&a.borrow().elems.clone(), depth, &mut out);
    Ok(js_array_of(ctx, out))
}

fn native_arr_reverse(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    a.borrow_mut().elems.reverse();
    Ok(ctx.this.clone())
}

fn native_arr_sort(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    // 默认字典序；带 comparator 时 try_host_method 已提前拦截走解释器。
    let a = this_array(ctx)?;
    let mut elems = a.borrow().elems.clone();
    elems.sort_by(|x, y| x.to_js_string().cmp(&y.to_js_string()));
    a.borrow_mut().elems = elems;
    Ok(ctx.this.clone())
}

fn native_arr_splice(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    let len = a.borrow().elems.len();
    let start = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let start = norm_index(start, len);
    let delete_count = match args.get(1) {
        Some(v) if !matches!(v, Value::Undefined) => {
            v.to_number().max(0.0).min((len - start) as f64) as usize
        }
        _ => len - start,
    };
    let items: Vec<Value> = args.iter().skip(2).cloned().collect();
    let mut elems = a.borrow().elems.clone();
    let removed: Vec<Value> = elems.drain(start..start + delete_count).collect();
    for (i, it) in items.into_iter().enumerate() {
        elems.insert(start + i, it);
    }
    a.borrow_mut().elems = elems;
    Ok(js_array_of(ctx, removed))
}

fn native_arr_fill(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    let len = a.borrow().elems.len();
    let start = norm_index(args.get(1).map(|x| x.to_number()).unwrap_or(0.0), len);
    let end = norm_index(args.get(2).map(|x| x.to_number()).unwrap_or(len as f64), len);
    let mut elems = a.borrow().elems.clone();
    for i in start..end.max(start) {
        elems[i] = v.clone();
    }
    a.borrow_mut().elems = elems;
    Ok(ctx.this.clone())
}

fn native_arr_at(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    let elems = a.borrow();
    let len = elems.elems.len();
    let i = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    let idx = if i < 0.0 { len as i64 + i as i64 } else { i as i64 };
    if idx < 0 || idx as usize >= len {
        return Ok(Value::Undefined);
    }
    Ok(elems.elems[idx as usize].clone())
}

fn native_arr_lastindexof(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let from = args.get(1).map(|v| v.to_number());
    let elems = a.borrow();
    let len = elems.elems.len();
    let start = match from {
        Some(f) if f < 0.0 => (len as i64 + f as i64).max(0) as usize,
        Some(f) => (f as usize).min(len.saturating_sub(1)),
        None => len.saturating_sub(1),
    };
    for i in (0..=start.min(len.saturating_sub(1))).rev() {
        if elems.elems[i].strict_eq(&target) {
            return Ok(Value::Number(i as f64));
        }
    }
    Ok(Value::Number(-1.0))
}

fn native_arr_copywithin(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = this_array(ctx)?;
    let len = a.borrow().elems.len();
    let target = norm_index(args.first().map(|v| v.to_number()).unwrap_or(0.0), len);
    let start = norm_index(args.get(1).map(|v| v.to_number()).unwrap_or(0.0), len);
    let end = norm_index(
        args.get(2).map(|v| v.to_number()).unwrap_or(len as f64),
        len,
    );
    let count = (end.saturating_sub(start)).min(len.saturating_sub(target));
    let mut elems = a.borrow().elems.clone();
    let tmp: Vec<Value> = elems[start..start + count].to_vec();
    for (i, v) in tmp.into_iter().enumerate() {
        elems[target + i] = v;
    }
    a.borrow_mut().elems = elems;
    Ok(ctx.this.clone())
}

// ---- Object 静态 ----

fn native_obj_create(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    match args.first() {
        Some(Value::Object(p)) => Ok(Value::Object(Rc::new(RefCell::new(
            JsObject::with_proto(Some(p.clone())),
        )))),
        Some(Value::Null) | None | Some(Value::Undefined) => Ok(Value::Object(Rc::new(
            RefCell::new(JsObject::with_proto(None)),
        ))),
        _ => Err(type_err("Object.create requires an object or null")),
    }
}

fn native_obj_hasown(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    match args.first() {
        Some(Value::Object(o)) => {
            let k = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
            Ok(Value::Bool(o.borrow().has_own(&k)))
        }
        _ => Err(type_err("Object.hasOwn called on non-object")),
    }
}

fn native_obj_getownpropertynames(
    ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    match args.first() {
        Some(Value::Object(o)) => {
            let names: Vec<Value> = o
                .borrow()
                .keys()
                .into_iter()
                .map(Value::String)
                .collect();
            Ok(js_array_of(ctx, names))
        }
        _ => Err(type_err("Object.getOwnPropertyNames called on non-object")),
    }
}

fn native_obj_defineproperty(
    _ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    let o = match args.first() {
        Some(Value::Object(o)) => o.clone(),
        _ => return Err(type_err("Object.defineProperty called on non-object")),
    };
    let key = args.get(1).map(|v| v.to_js_string()).unwrap_or_default();
    // 基础数据描述符：只处理 value（writable/enumerable/configurable 忽略）。
    let val = match args.get(2) {
        Some(Value::Object(d)) => d
            .borrow()
            .get_in_chain("value")
            .unwrap_or(Value::Undefined),
        _ => Value::Undefined,
    };
    o.borrow_mut().set(&key, val);
    Ok(Value::Object(o))
}

fn native_obj_is(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = args.first().cloned().unwrap_or(Value::Undefined);
    let b = args.get(1).cloned().unwrap_or(Value::Undefined);
    Ok(Value::Bool(match (&a, &b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() && y.is_nan() {
                true
            } else if *x == 0.0 && *y == 0.0 {
                // Object.is 区分 +0 / -0。
                x.is_sign_positive() == y.is_sign_positive()
            } else {
                x == y
            }
        }
        _ => a.strict_eq(&b),
    }))
}

fn native_obj_freeze(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    // 子集：冻结语义未实现，直返原对象（文档化偏差）。
    Ok(args.first().cloned().unwrap_or(Value::Undefined))
}

// ---- Math ----

macro_rules! math1 {
    ($name:ident, $f:expr) => {
        fn $name(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
            let x = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
            Ok(Value::Number($f(x)))
        }
    };
}

math1!(native_math_trunc, |x: f64| x.trunc());
math1!(native_math_sign, |x: f64| if x.is_nan() {
    f64::NAN
} else if x == 0.0 {
    x
} else {
    x.signum()
});
math1!(native_math_cbrt, |x: f64| x.cbrt());
math1!(native_math_log, |x: f64| x.ln());
math1!(native_math_log2, |x: f64| x.log2());
math1!(native_math_log10, |x: f64| x.log10());
math1!(native_math_log1p, |x: f64| x.ln_1p());
math1!(native_math_exp, |x: f64| x.exp());
math1!(native_math_expm1, |x: f64| x.exp_m1());
math1!(native_math_sin, |x: f64| x.sin());
math1!(native_math_cos, |x: f64| x.cos());
math1!(native_math_tan, |x: f64| x.tan());
math1!(native_math_asin, |x: f64| x.asin());
math1!(native_math_acos, |x: f64| x.acos());
math1!(native_math_atan, |x: f64| x.atan());
math1!(native_math_sinh, |x: f64| x.sinh());
math1!(native_math_cosh, |x: f64| x.cosh());
math1!(native_math_tanh, |x: f64| x.tanh());
math1!(native_math_fround, |x: f64| x as f32 as f64);

fn native_math_hypot(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Number(
        args.iter()
            .map(|v| v.to_number())
            .fold(0.0f64, |a, x| (a * a + x * x).sqrt()),
    ))
}

fn native_math_atan2(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let y = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
    let x = args.get(1).map(|v| v.to_number()).unwrap_or(f64::NAN);
    Ok(Value::Number(y.atan2(x)))
}

fn native_math_imul(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let a = args.first().map(|v| v.to_number()).unwrap_or(0.0) as i32;
    let b = args.get(1).map(|v| v.to_number()).unwrap_or(0.0) as i32;
    Ok(Value::Number(a.wrapping_mul(b) as f64))
}

fn native_math_clz32(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let x = args.first().map(|v| v.to_number()).unwrap_or(0.0) as u32;
    Ok(Value::Number(x.leading_zeros() as f64))
}

// ---- Number ----

fn this_num(ctx: &NativeCtx) -> Result<f64, FlowError> {
    match &ctx.this {
        Value::Number(n) => Ok(*n),
        v => Err(type_err(format!(
            "Number method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn native_num_tofixed(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let n = this_num(ctx)?;
    let d = args.first().map(|v| v.to_number()).unwrap_or(0.0);
    if !d.is_finite() || d < 0.0 || d > 100.0 {
        return Err(type_err("toFixed digits out of range"));
    }
    if !n.is_finite() {
        return Ok(Value::String(Value::Number(n).to_js_string()));
    }
    Ok(Value::String(format!("{:.*}", d as usize, n)))
}

fn native_num_toexponential(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let n = this_num(ctx)?;
    if !n.is_finite() {
        return Ok(Value::String(Value::Number(n).to_js_string()));
    }
    match args.first() {
        Some(v) if !matches!(v, Value::Undefined) => {
            let d = v.to_number();
            if !d.is_finite() || d < 0.0 || d > 100.0 {
                return Err(type_err("toExponential digits out of range"));
            }
            Ok(Value::String(format!("{:.*e}", d as usize, n)))
        }
        _ => Ok(Value::String(format!("{:e}", n))),
    }
}

fn native_num_toprecision(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let n = this_num(ctx)?;
    if !n.is_finite() {
        return Ok(Value::String(Value::Number(n).to_js_string()));
    }
    match args.first() {
        Some(v) if !matches!(v, Value::Undefined) => {
            let p = v.to_number();
            if !p.is_finite() || p < 1.0 || p > 100.0 {
                return Err(type_err("toPrecision precision out of range"));
            }
            // 简化：用 Rust 精度格式化（文档化近似）。
            Ok(Value::String(format!("{:.*}", p as usize - 1, n)))
        }
        _ => Ok(Value::String(Value::Number(n).to_js_string())),
    }
}

fn native_num_tostring(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let n = this_num(ctx)?;
    let radix = args.first().map(|v| v.to_number()).unwrap_or(10.0) as u32;
    if !(2..=36).contains(&radix) {
        return Err(type_err("radix out of range"));
    }
    if radix == 10 {
        return Ok(Value::String(Value::Number(n).to_js_string()));
    }
    if !n.is_finite() {
        return Ok(Value::String(Value::Number(n).to_js_string()));
    }
    // 整数部分的进制转换（小数部分忽略，文档化简化）。
    let neg = n < 0.0;
    let mut int = n.abs().trunc() as u64;
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    if int == 0 {
        out.push(b'0');
    }
    while int > 0 {
        out.push(digits[(int % radix as u64) as usize]);
        int /= radix as u64;
    }
    out.reverse();
    let mut s = String::from_utf8(out).unwrap();
    if neg {
        s.insert(0, '-');
    }
    Ok(Value::String(s))
}

fn native_num_isinteger(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(matches!(
        args.first(),
        Some(Value::Number(n)) if n.fract() == 0.0 && n.is_finite()
    )))
}

fn native_num_isnan(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(matches!(
        args.first(),
        Some(Value::Number(n)) if n.is_nan()
    )))
}

fn native_num_isfinite(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(matches!(
        args.first(),
        Some(Value::Number(n)) if n.is_finite()
    )))
}

fn native_num_issafeinteger(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(matches!(
        args.first(),
        Some(Value::Number(n)) if n.fract() == 0.0 && n.abs() <= 9007199254740991.0
    )))
}

// ---- 全局 ----

/// Phase 14：`Array.prototype.toString` —— 等价于 `join()`（无参数）。
fn native_array_to_string(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    array_join(ctx, vec![])
}

/// Phase 14：`Object.prototype.toString` —— 返回 "[object Object]"
/// （子集简化：不处理 Symbol.toStringTag）。
fn native_object_to_string(_ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::String("[object Object]".to_string()))
}

/// Phase 14：`Object.prototype.valueOf` —— 包装对象返回内部原始值，
/// 否则返回 this 本体。
fn native_object_value_of(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    if let Value::Object(o) = &ctx.this {
        if let Some(p) = o.borrow().primitive.clone() {
            return Ok(p);
        }
    }
    Ok(ctx.this.clone())
}

fn native_global_isfinite(_ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Bool(
        args.first().map(|v| v.to_number()).unwrap_or(f64::NAN).is_finite(),
    ))
}

fn native_global_parseint(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    native_parse_int(ctx, args)
}

fn native_global_parsefloat(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    native_parse_float(ctx, args)
}

fn native_global_boolean_call(
    _ctx: &mut NativeCtx,
    args: Vec<Value>,
) -> Result<Value, FlowError> {
    Ok(Value::Bool(
        args.first().map(|v| v.to_boolean()).unwrap_or(false),
    ))
}

/// Phase 14：全局 `eval` 标记。直接调用 `eval(x)` 由 `eval_call` 在调用点
/// 拦截（需当前作用域）；间接调用 `(0, eval)(x)` 由 `call_value_at` 按函数
/// 指针拦截，走全局作用域。Native 本体仅作兜底（理论上不可达）。
fn native_eval(_ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    Ok(Value::Undefined)
}

// ---------------------------------------------------------------------------
// Phase 10：标准库补完的 native 函数
// ---------------------------------------------------------------------------

// ---- 接收者提取 ----

fn this_map(ctx: &NativeCtx) -> Result<MapRef, FlowError> {
    match &ctx.this {
        Value::Map(m) => Ok(m.clone()),
        v => Err(type_err(format!(
            "Map method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_set(ctx: &NativeCtx) -> Result<SetRef, FlowError> {
    match &ctx.this {
        Value::Set(s) => Ok(s.clone()),
        v => Err(type_err(format!(
            "Set method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_weakmap(ctx: &NativeCtx) -> Result<WeakMapRef, FlowError> {
    match &ctx.this {
        Value::WeakMap(m) => Ok(m.clone()),
        v => Err(type_err(format!(
            "WeakMap method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_weakset(ctx: &NativeCtx) -> Result<WeakSetRef, FlowError> {
    match &ctx.this {
        Value::WeakSet(s) => Ok(s.clone()),
        v => Err(type_err(format!(
            "WeakSet method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_typedarray(ctx: &NativeCtx) -> Result<TypedArrayRef, FlowError> {
    match &ctx.this {
        Value::TypedArray(t) => Ok(t.clone()),
        v => Err(type_err(format!(
            "TypedArray method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_dataview(ctx: &NativeCtx) -> Result<DataViewRef, FlowError> {
    match &ctx.this {
        Value::DataView(d) => Ok(d.clone()),
        v => Err(type_err(format!(
            "DataView method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_arraybuffer(ctx: &NativeCtx) -> Result<ArrayBufferRef, FlowError> {
    match &ctx.this {
        Value::ArrayBuffer(b) => Ok(b.clone()),
        v => Err(type_err(format!(
            "ArrayBuffer method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

fn this_date(ctx: &NativeCtx) -> Result<DateRef, FlowError> {
    match &ctx.this {
        Value::Date(d) => Ok(d.clone()),
        v => Err(type_err(format!(
            "Date method called on incompatible receiver ({})",
            v.type_of()
        ))),
    }
}

/// 构造 `Value::Array`（挂 Array.prototype）。
fn js_array_of(ctx: &NativeCtx, elems: Vec<Value>) -> Value {
    Value::Array(Rc::new(RefCell::new(JsArray::with_proto(
        elems,
        Some(ctx.protos.array.clone()),
    ))))
}

// ---- Map ----

/// 共享的 set 条目逻辑（构造器初始化也用）。
fn map_set_entry(m: &MapRef, k: Value, v: Value) {
    let mut mm = m.borrow_mut();
    if let Some(e) = mm.entries.iter_mut().find(|(ek, _)| same_value_zero(ek, &k)) {
        e.1 = v;
    } else {
        mm.entries.push((k, v));
    }
}

fn native_map_set(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let k = args.first().cloned().unwrap_or(Value::Undefined);
    let v = args.get(1).cloned().unwrap_or(Value::Undefined);
    map_set_entry(&m, k, v);
    Ok(Value::Map(m))
}

fn native_map_get(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let k = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(m
        .borrow()
        .entries
        .iter()
        .find(|(ek, _)| same_value_zero(ek, &k))
        .map(|(_, v)| v.clone())
        .unwrap_or(Value::Undefined))
}

fn native_map_has(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let k = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Bool(
        m.borrow()
            .entries
            .iter()
            .any(|(ek, _)| same_value_zero(ek, &k)),
    ))
}

fn native_map_delete(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let k = args.first().cloned().unwrap_or(Value::Undefined);
    let mut mm = m.borrow_mut();
    let before = mm.entries.len();
    mm.entries.retain(|(ek, _)| !same_value_zero(ek, &k));
    Ok(Value::Bool(mm.entries.len() < before))
}

fn native_map_clear(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    m.borrow_mut().entries.clear();
    Ok(Value::Undefined)
}

fn native_map_keys(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let ks: Vec<Value> = m.borrow().entries.iter().map(|(k, _)| k.clone()).collect();
    Ok(js_array_of(ctx, ks))
}

fn native_map_values(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let vs: Vec<Value> = m.borrow().entries.iter().map(|(_, v)| v.clone()).collect();
    Ok(js_array_of(ctx, vs))
}

fn native_map_entries(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_map(ctx)?;
    let es: Vec<Value> = m
        .borrow()
        .entries
        .iter()
        .map(|(k, v)| js_array_of(ctx, vec![k.clone(), v.clone()]))
        .collect();
    Ok(js_array_of(ctx, es))
}

// ---- Set ----

/// 共享的 add 条目逻辑。
fn set_add_entry(s: &SetRef, v: Value) {
    let mut ss = s.borrow_mut();
    if !ss.entries.iter().any(|e| same_value_zero(e, &v)) {
        ss.entries.push(v);
    }
}

fn native_set_add(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_set(ctx)?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    set_add_entry(&s, v);
    Ok(Value::Set(s))
}

fn native_set_has(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_set(ctx)?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Bool(
        s.borrow().entries.iter().any(|e| same_value_zero(e, &v)),
    ))
}

fn native_set_delete(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_set(ctx)?;
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    let mut ss = s.borrow_mut();
    let before = ss.entries.len();
    ss.entries.retain(|e| !same_value_zero(e, &v));
    Ok(Value::Bool(ss.entries.len() < before))
}

fn native_set_clear(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_set(ctx)?;
    s.borrow_mut().entries.clear();
    Ok(Value::Undefined)
}

fn native_set_values(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_set(ctx)?;
    Ok(js_array_of(ctx, s.borrow().entries.clone()))
}

fn native_set_entries(ctx: &mut NativeCtx, _args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_set(ctx)?;
    let es: Vec<Value> = s
        .borrow()
        .entries
        .iter()
        .map(|v| js_array_of(ctx, vec![v.clone(), v.clone()]))
        .collect();
    Ok(js_array_of(ctx, es))
}

// ---- WeakMap ----

/// 清扫已回收条目（phase 12：摊销——每 64 次操作清扫一次，而非每次 O(n)。
/// 查找路径额外校验 `is_alive`，防止已回收条目的地址被新对象复用时的
/// ABA 误判）。
fn weakmap_sweep(m: &WeakMapRef) {
    let mut mm = m.borrow_mut();
    mm.sweep_debt += 1;
    if mm.sweep_debt >= 64 {
        mm.sweep_debt = 0;
        mm.entries.retain(|(k, _)| k.is_alive());
    }
}

/// 弱键匹配：指针相等且键仍存活（防 ABA）。
fn weak_key_match(ek: &WeakKey, k: &WeakKey) -> bool {
    ek == k && ek.is_alive()
}

fn weakmap_key(args: &[Value]) -> Result<WeakKey, FlowError> {
    let k = args.first().cloned().unwrap_or(Value::Undefined);
    WeakKey::of(&k).ok_or_else(|| type_err("WeakMap key must be an object"))
}

fn native_weakmap_set(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_weakmap(ctx)?;
    let k = weakmap_key(&args)?;
    let v = args.get(1).cloned().unwrap_or(Value::Undefined);
    weakmap_sweep(&m);
    let mut mm = m.borrow_mut();
    if let Some(e) = mm.entries.iter_mut().find(|(ek, _)| weak_key_match(ek, &k)) {
        e.1 = v;
    } else {
        mm.entries.push((k, v));
    }
    drop(mm);
    Ok(ctx.this.clone())
}

fn native_weakmap_get(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_weakmap(ctx)?;
    let k = weakmap_key(&args)?;
    weakmap_sweep(&m);
    Ok(m
        .borrow()
        .entries
        .iter()
        .find(|(ek, _)| weak_key_match(ek, &k))
        .map(|(_, v)| v.clone())
        .unwrap_or(Value::Undefined))
}

fn native_weakmap_has(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_weakmap(ctx)?;
    let k = weakmap_key(&args)?;
    weakmap_sweep(&m);
    Ok(Value::Bool(
        m.borrow().entries.iter().any(|(ek, _)| weak_key_match(ek, &k)),
    ))
}

fn native_weakmap_delete(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let m = this_weakmap(ctx)?;
    let k = weakmap_key(&args)?;
    weakmap_sweep(&m);
    let mut mm = m.borrow_mut();
    let before = mm.entries.len();
    mm.entries.retain(|(ek, _)| !weak_key_match(ek, &k));
    Ok(Value::Bool(mm.entries.len() < before))
}

// ---- WeakSet ----

fn weakset_sweep(s: &WeakSetRef) {
    let mut ss = s.borrow_mut();
    ss.sweep_debt += 1;
    if ss.sweep_debt >= 64 {
        ss.sweep_debt = 0;
        ss.entries.retain(|k| k.is_alive());
    }
}

fn native_weakset_add(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_weakset(ctx)?;
    let k = weakmap_key(&args).map_err(|_| type_err("WeakSet value must be an object"))?;
    weakset_sweep(&s);
    let mut ss = s.borrow_mut();
    if !ss.entries.iter().any(|ek| weak_key_match(ek, &k)) {
        ss.entries.push(k);
    }
    drop(ss);
    Ok(ctx.this.clone())
}

fn native_weakset_has(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_weakset(ctx)?;
    let k = match WeakKey::of(&args.first().cloned().unwrap_or(Value::Undefined)) {
        Some(k) => k,
        None => return Ok(Value::Bool(false)),
    };
    weakset_sweep(&s);
    Ok(Value::Bool(
        s.borrow().entries.iter().any(|ek| weak_key_match(ek, &k)),
    ))
}

fn native_weakset_delete(ctx: &mut NativeCtx, args: Vec<Value>) -> Result<Value, FlowError> {
    let s = this_weakset(ctx)?;
    let k = match WeakKey::of(&args.first().cloned().unwrap_or(Value::Undefined)) {
        Some(k) => k,
        None => return Ok(Value::Bool(false)),
    };
    weakset_sweep(&s);
    let mut ss = s.borrow_mut();
    let before = ss.entries.len();
    ss.entries.retain(|ek| !weak_key_match(ek, &k));
    Ok(Value::Bool(ss.entries.len() < before))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // phase 11：VM 双跑开关。`vm_<name>` 生成的测试把它打开，
    // 使所有 ev* 辅助走字节码 VM，从而全量 217 测试在 VM 模式下重跑。
    thread_local! {
        static USE_VM: Cell<bool> = const { Cell::new(false) };
    }

    /// RAII：vm_* 测试开启 VM 模式，结束（即使 panic）时复位，
    /// 避免测试线程复用导致污染。
    struct VmGuard;
    impl VmGuard {
        fn new() -> Self {
            USE_VM.set(true);
            VmGuard
        }
    }
    impl Drop for VmGuard {
        fn drop(&mut self) {
            USE_VM.set(false);
        }
    }

    fn ev(src: &str) -> Value {
        if USE_VM.get() {
            return crate::vm::eval_source_vm(src)
                .unwrap_or_else(|e| panic!("vm eval failed for {:?}: {}", src, e));
        }
        eval_source(src).unwrap_or_else(|e| panic!("eval failed for {:?}: {}", src, e))
    }

    fn ev_num(src: &str) -> f64 {
        match ev(src) {
            Value::Number(n) => n,
            v => panic!("expected number, got {:?}", v),
        }
    }

    fn ev_str(src: &str) -> String {
        match ev(src) {
            Value::String(s) => s,
            v => panic!("expected string, got {:?}", v),
        }
    }

    fn ev_bool(src: &str) -> bool {
        match ev(src) {
            Value::Bool(b) => b,
            v => panic!("expected bool, got {:?}", v),
        }
    }

    fn ev_err(src: &str) -> String {
        let r = if USE_VM.get() {
            crate::vm::eval_source_vm(src)
        } else {
            eval_source(src)
        };
        match r {
            Ok(v) => panic!("expected error, got {:?}", v),
            Err(e) => e.to_string(),
        }
    }

    // ---- Phase 4：原型链 / 标准库 / DOM ----

    use crate::dom::MockDom;

    /// 带 MockDom 的解释器：body 下有 div#t(hello)、p.x(world)、span#s。
    fn mock_interpreter() -> (Interpreter, Rc<MockDom>) {
        let dom = Rc::new(MockDom::new());
        let body = dom.add_element(0, "body", &[], "");
        dom.add_element(body, "div", &[("id", "t")], "hello");
        dom.add_element(body, "p", &[("class", "x")], "world");
        dom.add_element(body, "span", &[("id", "s")], "span-text");
        let mut ip = Interpreter::new();
        ip.bind_dom(dom.clone());
        ip.set_vm_enabled(USE_VM.get());
        (ip, dom)
    }

    fn evd(src: &str) -> Value {
        let (mut ip, _dom) = mock_interpreter();
        let prog = parse_source(src).expect("parse failed");
        ip.run(&prog)
            .unwrap_or_else(|e| panic!("eval failed for {:?}: {:?}", src, e))
    }

    fn evd_bool(src: &str) -> bool {
        match evd(src) {
            Value::Bool(b) => b,
            v => panic!("expected bool, got {:?}", v),
        }
    }

    fn evd_num(src: &str) -> f64 {
        match evd(src) {
            Value::Number(n) => n,
            v => panic!("expected number, got {:?}", v),
        }
    }

    fn evd_str(src: &str) -> String {
        match evd(src) {
            Value::String(s) => s,
            v => panic!("expected string, got {:?}", v),
        }
    }

    #[test]
    fn proto_chain_inheritance() {
        // 构造器 prototype 上的属性，实例沿原型链读到。
        let src = r#"
            function A() {}
            A.prototype.x = 5;
            const a = new A();
            a.x
        "#;
        assert_eq!(evd_num(src), 5.0);
        // 实例自身属性优先于原型。
        let src2 = r#"
            function A() {}
            A.prototype.x = 5;
            const a = new A();
            a.x = 9;
            a.x
        "#;
        assert_eq!(evd_num(src2), 9.0);
    }

    #[test]
    fn instanceof_via_proto_chain() {
        assert!(evd_bool("[] instanceof Array"));
        assert!(evd_bool("[] instanceof Object")); // 原型链：Array.proto -> Object.proto
        assert!(evd_bool("({}) instanceof Object"));
        assert!(!evd_bool("({}) instanceof Array"));
        assert!(evd_bool(r#""x" instanceof String"#));
        assert!(evd_bool(
            "function A() {} ; const a = new A(); a instanceof A"
        ));
        assert!(!evd_bool("function A() {} ; function B() {} ; new A() instanceof B"));
    }

    #[test]
    fn array_higher_order() {
        assert_eq!(evd_str("[1,2,3].map(x => x * 2).join(',')"), "2,4,6");
        assert_eq!(evd_str("[1,2,3,4].filter(x => x % 2 === 0).join(',')"), "2,4");
        assert_eq!(evd_num("[1,2,3].find(x => x > 1)"), 2.0);
        assert_eq!(evd_num("[1,2,3,4].reduce((a, b) => a + b, 0)"), 10.0);
        assert_eq!(evd_num("[1,2,3].reduce((a, b) => a + b)"), 6.0); // 无初值
        assert_eq!(evd_num("let s = 0; [1,2,3].forEach(x => { s += x; }); s"), 6.0);
        // 回调拿到 (value, index)
        assert_eq!(evd_str("[10,20].map((v, i) => v + i).join(',')"), "10,21");
    }

    #[test]
    fn array_mutators() {
        assert_eq!(evd_str("const a=[1,2]; a.push(3,4); a.join(',')"), "1,2,3,4");
        assert_eq!(evd_num("const a=[1,2]; a.push(3)"), 3.0); // 返回新 length
        assert_eq!(evd_num("const a=[1,2,3]; a.pop()"), 3.0);
        assert_eq!(evd_str("const a=[1,2,3]; a.shift(); a.join(',')"), "2,3");
        assert_eq!(evd_str("const a=[2,3]; a.unshift(1); a.join(',')"), "1,2,3");
        assert_eq!(evd_str("[1,2,3,4].slice(1,3).join(',')"), "2,3");
        assert_eq!(evd_str("[1,2,3].slice(-2).join(',')"), "2,3");
        assert_eq!(evd_str("[1,2].concat([3],[4,5]).join(',')"), "1,2,3,4,5");
        assert_eq!(evd_num("[1,2,3].indexOf(2)"), 1.0);
        assert_eq!(evd_num("[1,2,3].indexOf(9)"), -1.0);
        assert!(evd_bool("[1,2,3].includes(2)"));
        assert!(!evd_bool("[1,2,3].includes(9)"));
    }

    #[test]
    fn string_prototype_methods() {
        assert_eq!(evd_str(r#""abc".split("").join("-")"#), "a-b-c");
        assert_eq!(evd_num(r#""a,b,c".split(",").length"#), 3.0);
        assert_eq!(evd_str(r#""abc".charAt(1)"#), "b");
        assert_eq!(evd_num(r#""abc".charCodeAt(0)"#), 97.0);
        assert_eq!(evd_str(r#""hello".slice(1, 4)"#), "ell");
        assert_eq!(evd_str(r#""hello".slice(-3)"#), "llo");
        assert_eq!(evd_str(r#""hello".substring(4, 1)"#), "ell");
        assert_eq!(evd_num(r#""hello".indexOf("l")"#), 2.0);
        assert_eq!(evd_num(r#""hello".indexOf("z")"#), -1.0);
        assert!(evd_bool(r#""hello".includes("ell")"#));
        assert_eq!(evd_str(r#""  hi  ".trim()"#), "hi");
        assert_eq!(evd_str(r#""AbC".toUpperCase()"#), "ABC");
        assert_eq!(evd_str(r#""AbC".toLowerCase()"#), "abc");
        assert!(evd_bool(r#""abc".startsWith("ab")"#));
        assert!(evd_bool(r#""abc".endsWith("bc")"#));
        assert!(!evd_bool(r#""abc".startsWith("bc")"#));
    }

    #[test]
    fn object_statics() {
        assert_eq!(evd_str("Object.keys({a:1,b:2}).join(',')"), "a,b");
        assert_eq!(evd_str("Object.values({a:1,b:2}).join(',')"), "1,2");
        assert_eq!(evd_str("Object.entries({a:1})[0].join(':')"), "a:1");
        assert_eq!(evd_num("const t = {}; Object.assign(t, {a:1}, {b:2}); t.a + t.b"), 3.0);
        assert_eq!(evd_str("Object.keys([10,20]).join(',')"), "0,1");
    }

    #[test]
    fn number_and_array_statics() {
        assert!(evd_bool("Number.isInteger(42)"));
        assert!(!evd_bool("Number.isInteger(4.2)"));
        assert!(!evd_bool("Number.isInteger('42')"));
        assert!(evd_bool("Number.isNaN(0/0)"));
        assert!(!evd_bool("Number.isNaN('x')"));
        assert!(evd_bool("Array.isArray([])"));
        assert!(!evd_bool("Array.isArray({})"));
    }

    #[test]
    fn function_call_apply() {
        assert_eq!(
            evd_num("function f(a, b) { return this.x + a + b; } f.call({x:1}, 2, 3)"),
            6.0
        );
        assert_eq!(
            evd_num("function f(a, b) { return a * b; } f.apply(null, [6, 7])"),
            42.0
        );
    }

    #[test]
    fn dom_read() {
        assert_eq!(evd_str("document.getElementById('t').textContent"), "hello");
        assert_eq!(evd_str("document.querySelector('p').textContent"), "world");
        assert_eq!(evd_str("document.getElementById('t').tagName"), "DIV");
        assert_eq!(evd_num("document.querySelectorAll('span').length"), 1.0);
        assert_eq!(evd_str("document.querySelectorAll('div')[0].textContent"), "hello");
        // 不存在的 id → null
        assert!(evd_bool("document.getElementById('nope') === null"));
    }

    #[test]
    fn dom_write() {
        // setAttribute / getAttribute 回写
        assert_eq!(
            evd_str(
                "const el = document.getElementById('t'); \
                 el.setAttribute('data-v', '42'); \
                 el.getAttribute('data-v')"
            ),
            "42"
        );
        // textContent 赋值写回宿主
        let (mut ip, dom) = mock_interpreter();
        let prog = parse_source(
            "document.getElementById('t').textContent = 'changed';",
        )
        .expect("parse failed");
        ip.run(&prog).expect("eval failed");
        let id = dom.get_element_by_id("t").unwrap();
        assert_eq!(dom.text_content(id), "changed");
    }

    #[test]
    fn dom_window() {
        // window.document 别名
        assert_eq!(
            evd_str("window.document.getElementById('s').textContent"),
            "span-text"
        );
        // setTimeout：宏任务——主脚本先跑完，回调后执行；返回数字 id
        assert_eq!(
            evd_num("let hit = 0; const id = window.setTimeout(() => { hit = 7; }, 100); hit"),
            0.0
        );
        assert!(evd_bool(
            "typeof window.setTimeout(() => {}, 0) === 'number'"
        ));
        // clearTimeout 取消后回调不执行（宿主侧可观测）
        let (mut ip, dom) = mock_interpreter();
        let prog = parse_source(
            r#"const el = document.getElementById('t');
               const id = window.setTimeout(() => el.setAttribute('data-x', 'ran'), 0);
               window.clearTimeout(id);"#,
        )
        .expect("parse failed");
        ip.run(&prog).expect("eval failed");
        let t = dom.get_element_by_id("t").unwrap();
        assert_eq!(dom.get_attribute(t, "data-x"), None);
        // 不取消则执行
        let (mut ip2, dom2) = mock_interpreter();
        let prog2 = parse_source(
            r#"const el = document.getElementById('t');
               window.setTimeout(() => el.setAttribute('data-x', 'ran'), 0);"#,
        )
        .expect("parse failed");
        ip2.run(&prog2).expect("eval failed");
        let t2 = dom2.get_element_by_id("t").unwrap();
        assert_eq!(dom2.get_attribute(t2, "data-x").as_deref(), Some("ran"));
        // 非函数回调抛错
        let (mut ip3, _d3) = mock_interpreter();
        let prog3 = parse_source("window.setTimeout(42, 0);").unwrap();
        assert!(ip3.run(&prog3).is_err());
    }

    /// console 断言用：跑完（含 drain）后取 console 输出。
    fn evd_console(src: &str) -> Vec<String> {
        let (mut ip, _dom) = mock_interpreter();
        let prog = parse_source(src).expect("parse failed");
        ip.run(&prog)
            .unwrap_or_else(|e| panic!("eval failed for {:?}: {:?}", src, e));
        ip.take_console()
    }

    #[test]
    fn task_fifo_order_after_main() {
        // 注册顺序执行（ms 只做占位，不做精确计时）；主脚本先跑完。
        let out = evd_console(
            r#"const a = [];
               a.push('main');
               window.setTimeout(() => a.push('t1'), 30);
               window.setTimeout(() => a.push('t2'), 5);
               console.log(a.join(','));
               window.setTimeout(() => console.log('drained:' + a.join(',')), 0);"#,
        );
        assert_eq!(out, vec!["main", "drained:main,t1,t2"]);
    }

    #[test]
    fn task_nested_scheduling() {
        // 任务里注册的新任务继续排（drain 不提前结束）。
        let out = evd_console(
            r#"console.log('main');
               window.setTimeout(() => {
                   console.log('outer');
                   window.setTimeout(() => console.log('inner'), 0);
               }, 0);"#,
        );
        assert_eq!(out, vec!["main", "outer", "inner"]);
    }

    #[test]
    fn task_error_does_not_stop_queue() {
        // 单个任务抛错记 console，队列继续排空（浏览器行为）。
        let out = evd_console(
            r#"window.setTimeout(() => { throw 'boom'; }, 0);
               window.setTimeout(() => console.log('still-alive'), 0);"#,
        );
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("uncaught error in task:"), "got: {:?}", out[0]);
        assert!(out[0].contains("boom"), "got: {:?}", out[0]);
        assert_eq!(out[1], "still-alive");
    }

    #[test]
    fn dom_append_child_mounts() {
        // 挂载后 querySelector 可见
        assert_eq!(
            evd_num(
                r#"const el = document.createElement('span');
                   document.getElementById('t').appendChild(el);
                   document.querySelectorAll('span').length"#
            ),
            2.0
        );
        // 返回被挂载的节点本身
        assert!(evd_bool(
            r#"const el = document.createElement('div');
               document.getElementById('t').appendChild(el) === el"#
        ));
        // 重复挂载是移动而非复制
        let (mut ip, dom) = mock_interpreter();
        let prog = parse_source(
            r#"const s = document.getElementById('s');
               const t = document.getElementById('t');
               t.appendChild(s); t.appendChild(s);"#,
        )
        .unwrap();
        ip.run(&prog).unwrap();
        let t = dom.get_element_by_id("t").unwrap();
        let s = dom.get_element_by_id("s").unwrap();
        assert_eq!(dom.children(t).iter().filter(|&&c| c == s).count(), 1);
    }

    #[test]
    fn dom_remove_child_detaches() {
        assert!(evd_bool(
            r#"const b = document.querySelector('body');
               b.removeChild(document.getElementById('s'));
               document.getElementById('s') === null"#
        ));
        assert_eq!(evd_num("document.querySelectorAll('span').length"), 1.0);
    }

    #[test]
    fn dom_insert_before_order() {
        let (mut ip, dom) = mock_interpreter();
        let prog = parse_source(
            r#"const b = document.querySelector('body');
               const n = document.createElement('i');
               b.insertBefore(n, document.getElementById('s'));"#,
        )
        .unwrap();
        ip.run(&prog).unwrap();
        let body = dom.query_selector_all("body")[0];
        let tags: Vec<String> =
            dom.children(body).iter().map(|&c| dom.tag_name(c)).collect();
        assert_eq!(tags, vec!["div", "p", "i", "span"]);
    }

    #[test]
    fn dom_append_cycle_is_ignored() {
        // 把祖先挂到自己子孙下会成环：静默跳过，不 hang。
        let (mut ip, dom) = mock_interpreter();
        let prog = parse_source(
            r#"const b = document.querySelector('body');
               const t = document.getElementById('t');
               t.appendChild(b);"#,
        )
        .unwrap();
        ip.run(&prog).unwrap();
        let body = dom.query_selector_all("body")[0];
        assert!(!dom.children(dom.get_element_by_id("t").unwrap()).contains(&body));
    }

    #[test]
    fn dom_inner_html_setter_parses() {
        assert_eq!(
            evd_num(
                r#"document.getElementById('t').innerHTML = '<b>hi</b><i class="z">x</i>';
                   document.querySelectorAll('b').length"#
            ),
            1.0
        );
        assert_eq!(
            evd_str(
                r#"document.getElementById('t').innerHTML = '<b>hi</b>';
                   document.querySelector('b').textContent"#
            ),
            "hi"
        );
        assert_eq!(
            evd_num(
                r#"document.getElementById('t').innerHTML = '<i class="z">x</i>';
                   document.querySelectorAll('i.z').length"#
            ),
            1.0
        );
        // 旧子节点被替换
        assert_eq!(
            evd_str(
                r#"const t = document.getElementById('t');
                   t.innerHTML = '<b>new</b>';
                   t.textContent"#
            ),
            "new"
        );
    }

    #[test]
    fn dom_outer_html_getter() {
        let html = evd_str("document.getElementById('t').outerHTML");
        assert!(html.starts_with("<div"), "got: {}", html);
        assert!(html.contains("hello"), "got: {}", html);
        assert!(html.ends_with("</div>"), "got: {}", html);
    }

    #[test]
    fn dom_class_list() {
        assert_eq!(
            evd_str(
                r#"const t = document.getElementById('t');
                   t.classList.add('a');
                   t.classList.add('b');
                   const hasA = t.classList.contains('a');
                   t.classList.remove('a');
                   [hasA, t.classList.contains('a'), t.classList.contains('b')].join(',')"#
            ),
            "true,false,true"
        );
        // 写回的是 class 属性
        assert_eq!(
            evd_str(
                r#"const t = document.getElementById('t');
                   t.classList.add('x');
                   t.getAttribute('class')"#
            ),
            "x"
        );
    }

    #[test]
    fn dom_compound_selector() {
        assert_eq!(evd_num("document.querySelectorAll('p.x').length"), 1.0);
        assert_eq!(evd_num("document.querySelectorAll('div.x').length"), 0.0);
        assert_eq!(evd_num("document.querySelectorAll('span#s').length"), 1.0);
    }

    #[test]
    fn dom_descendant_selector() {
        let src = r#"const wrap = document.createElement('div');
                     wrap.setAttribute('id', 'wrap');
                     document.querySelector('body').appendChild(wrap);
                     const inner = document.createElement('span');
                     inner.setAttribute('class', 'in');
                     wrap.appendChild(inner);
                     [document.querySelectorAll('#wrap .in').length,
                      document.querySelectorAll('body div span').length,
                      document.querySelectorAll('#wrap .missing').length].join(',')"#;
        assert_eq!(evd_str(src), "1,1,0");
    }

    #[test]
    fn dom_invalid_selector_returns_empty() {
        assert_eq!(evd_num("document.querySelectorAll('div[').length"), 0.0);
        assert!(evd_bool("document.querySelector('div[') === null"));
        assert_eq!(evd_num("document.querySelectorAll('').length"), 0.0);
    }

    #[test]
    fn dom_click_fires_listener() {
        // click 把回调推进任务队列，drain 时执行。
        let (mut ip, dom) = mock_interpreter();
        let prog = parse_source(
            r#"const t = document.getElementById('t');
               t.addEventListener('click', () => t.setAttribute('data-c', 'yes'));
               t.click();"#,
        )
        .unwrap();
        ip.run(&prog).unwrap();
        let t = dom.get_element_by_id("t").unwrap();
        assert_eq!(dom.get_attribute(t, "data-c").as_deref(), Some("yes"));
    }

    #[test]
    fn dom_click_without_listener_is_noop() {
        let (mut ip, _dom) = mock_interpreter();
        let prog =
            parse_source("document.getElementById('t').click();").unwrap();
        ip.run(&prog).expect("click without listener should not error");
    }

    #[test]
    fn dom_add_event_listener_ignores_non_function() {
        // 浏览器行为：不可调用的回调静默忽略，不抛错。
        let (mut ip, _dom) = mock_interpreter();
        let prog = parse_source(
            "document.getElementById('t').addEventListener('click', null);",
        )
        .unwrap();
        ip.run(&prog).expect("should ignore non-function listener");
    }

    #[test]
    fn dom_create_element() {
        assert_eq!(
            evd_str("const el = document.createElement('section'); el.tagName"),
            "SECTION"
        );
    }

    #[test]
    fn proto_methods_visible_via_chain() {
        // 方法挂在原型上：实例自身没有，但沿链能找到。
        assert!(evd_bool("typeof [1].map === 'function'"));
        assert!(evd_bool("typeof 'x'.split === 'function'"));
        assert!(evd_bool("typeof Object.keys === 'function'"));
    }

    #[test]
    fn arithmetic_precedence() {
        assert_eq!(ev_num("1 + 2 * 3 - 4 / 2"), 5.0);
        assert_eq!(ev_num("(1 + 2) * 3"), 9.0);
        assert_eq!(ev_num("2 ** 3 ** 2"), 512.0); // 右结合
        assert_eq!(ev_num("17 % 5"), 2.0);
    }

    #[test]
    fn string_concat_vs_add() {
        assert_eq!(ev_str(r#""a" + 1 + 2"#), "a12");
        assert_eq!(ev_str(r#"1 + 2 + "!""#), "3!");
        assert_eq!(ev_str(r#""x" + [1, 2]"#), "x1,2");
    }

    #[test]
    fn closure_counter() {
        let src = r#"
            function counter() {
                let n = 0;
                return () => ++n;
            }
            const c = counter();
            c(); c(); c();
        "#;
        assert_eq!(ev_num(src), 3.0);
    }

    #[test]
    fn fib_recursion() {
        let src = r#"
            function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
            fib(10)
        "#;
        assert_eq!(ev_num(src), 55.0);
    }

    #[test]
    fn while_accumulate() {
        assert_eq!(ev_num("let s=0,i=1; while(i<=100){s+=i;i++;} s"), 5050.0);
    }

    #[test]
    fn for_accumulate() {
        assert_eq!(ev_num("let s=0; for(let i=0;i<10;i++){s+=i;} s"), 45.0);
    }

    #[test]
    fn try_catch_finally() {
        let src = r#"
            const log = [];
            try { throw "boom"; }
            catch(e) { log.push("caught:" + e); }
            finally { log.push("fin"); }
            log.join(",")
        "#;
        assert_eq!(ev_str(src), "caught:boom,fin");
    }

    #[test]
    fn var_hoisting() {
        // 函数声明提升：先调用后声明也能跑（补足值是最后一条语句的，这里把调用放最后）。
        assert_eq!(ev_str(r#"function greet() { return "hi"; } greet()"#), "hi");
        assert_eq!(ev_num("a = 5; var a; a"), 5.0);
        // var 声明提升后、赋值前是 undefined
        assert_eq!(ev_str("typeof early"), "undefined");
    }

    #[test]
    fn let_block_scope_and_tdz() {
        assert_eq!(ev_num("let x = 1; { let x = 2; } x"), 1.0);
        let err = ev_err("{ console.log(q); let q = 1; }");
        assert!(err.contains("before initialization"), "got: {}", err);
        // typeof 未声明变量不报错
        assert_eq!(ev_str("typeof someUndeclaredVar123"), "undefined");
    }

    #[test]
    fn arrow_functions() {
        assert_eq!(ev_num("const add = (a, b) => a + b; add(2, 3)"), 5.0);
        assert_eq!(ev_num("const inc = x => x + 1; inc(41)"), 42.0);
        assert_eq!(
            ev_num("const f = (x) => { const y = x * 2; return y + 1; }; f(10)"),
            21.0
        );
    }

    #[test]
    fn object_array_access() {
        assert_eq!(ev_num("const o = {a:1, b:{c:2}}; o.b.c + o['a']"), 3.0);
        assert_eq!(ev_num("const a=[1,2,3]; a[1]=9; a[0]+a[1]+a[2]"), 13.0);
        assert_eq!(ev_num("const a=[1,2,3]; a.length"), 3.0);
        assert_eq!(ev_str("const o={x:1}; o.y=2; o.x+','+o.y"), "1,2");
    }

    #[test]
    fn console_log_captured() {
        let (v, out) = evwc(r#"console.log("hello", 42); console.log([1,2]);"#)
            .expect("eval failed");
        assert!(matches!(v, Value::Undefined));
        assert_eq!(out, vec!["hello 42", "1,2"]);
    }

    #[test]
    fn runtime_error_undefined_var() {
        let err = ev_err("zzz_not_defined_here");
        assert!(err.contains("not defined"), "got: {}", err);
    }

    #[test]
    fn runtime_error_call_non_function() {
        let err = ev_err("const x = 1; x()");
        assert!(err.contains("not a function"), "got: {}", err);
    }

    #[test]
    fn math_builtins() {
        assert_eq!(ev_num("Math.floor(3.7) + Math.pow(2, 3) + Math.max(1, 9)"), 20.0);
        assert_eq!(ev_num("Math.round(-2.5)"), -2.0); // JS 语义，非 Rust round
        assert_eq!(ev_num("Math.sqrt(16) + Math.abs(-3)"), 7.0);
        assert_eq!(ev_num("Math.min(5, 2, 8)"), 2.0);
    }

    #[test]
    fn json_roundtrip() {
        assert_eq!(
            ev_str(r#"JSON.stringify({a:[1,"x",true]})"#),
            r#"{"a":[1,"x",true]}"#
        );
        assert_eq!(ev_num(r#"JSON.parse('{"a":[1,2]}').a[1]"#), 2.0);
        assert_eq!(ev_str(r#"JSON.parse('"hi"')"#), "hi");
    }

    #[test]
    fn loose_equality() {
        assert!(ev_bool(r#"1 == "1""#));
        assert!(ev_bool("null == undefined"));
        assert!(ev_bool("0 == false"));
        assert!(!ev_bool("0 === false"));
        assert!(ev_bool(r#""abc" != "abd""#));
    }

    #[test]
    fn for_in_for_of() {
        assert_eq!(
            ev_str(r#"const k=[]; const o={a:1,b:2}; for (const key in o) k.push(key); k.join("")"#),
            "ab"
        );
        assert_eq!(
            ev_num("let s=0; for (const v of [1,2,3]) s+=v; s"),
            6.0
        );
        assert_eq!(
            ev_str(r#"let t=""; for (const ch of "hey") t+=ch+"-"; t"#),
            "h-e-y-"
        );
    }

    #[test]
    fn switch_fallthrough() {
        let src = r#"
            let x = 2, s = "";
            switch (x) {
                case 1: s = "one"; break;
                case 2: s = "two";
                case 3: s += "+three"; break;
                default: s = "?";
            }
            s
        "#;
        assert_eq!(ev_str(src), "two+three");
    }

    #[test]
    fn new_constructor_and_this() {
        let src = r#"
            function Dog(name) { this.name = name; }
            const d = new Dog("wang");
            d.name
        "#;
        assert_eq!(ev_str(src), "wang");
    }

    #[test]
    fn step_limit_stops_infinite_loop() {
        let err = ev_err("while (true) {}");
        assert!(err.contains("step limit"), "got: {}", err);
    }

    #[test]
    fn const_reassign_errors() {
        let err = ev_err("const c = 1; c = 2;");
        assert!(err.contains("constant"), "got: {}", err);
    }

    #[test]
    fn uncaught_throw_message() {
        let err = ev_err(r#"throw "boom""#);
        assert!(err.contains("uncaught exception"), "got: {}", err);
        assert!(err.contains("boom"), "got: {}", err);
    }

    #[test]
    fn parse_int_float() {
        assert_eq!(ev_num(r#"parseInt("42px")"#), 42.0);
        assert_eq!(ev_num(r#"parseInt("0x10")"#), 16.0);
        assert_eq!(ev_num(r#"parseFloat("3.14abc")"#), 3.14);
        assert!(ev_bool("isNaN(parseInt('abc'))"));
        assert!(!ev_bool("isNaN(5)"));
    }

    // ---- Phase 7：Promise / 微任务 / async / fetch / 正则 / 模块 ----

    fn evc(src: &str) -> Vec<String> {
        let (_, console) = if USE_VM.get() {
            crate::vm::eval_with_console_vm(src)
                .unwrap_or_else(|e| panic!("vm eval failed for {:?}: {:?}", src, e))
        } else {
            evwc(src).unwrap_or_else(|e| panic!("eval failed for {:?}: {:?}", src, e))
        };
        console
    }

    /// eval_with_console 的双模式版（直接调用点用）。
    fn evwc(src: &str) -> Result<(Value, Vec<String>), JsError> {
        if USE_VM.get() {
            crate::vm::eval_with_console_vm(src)
        } else {
            eval_with_console(src)
        }
    }

    #[test]
    fn microtask_before_macrotask() {
        // 微任务在主脚本后、宏任务前执行。
        let c = evc(
            r#"
            console.log('a');
            Promise.resolve().then(() => console.log('b'));
            queueMicrotask(() => console.log('c'));
            console.log('d');
            "#,
        );
        assert_eq!(c, vec!["a", "d", "b", "c"]);
    }

    #[test]
    fn promise_chain() {
        let c = evc(
            r#"
            Promise.resolve(1)
                .then(x => x + 1)
                .then(x => x * 10)
                .then(x => console.log(x));
            "#,
        );
        assert_eq!(c, vec!["20"]);
    }

    #[test]
    fn promise_catch() {
        let c = evc(
            r#"
            Promise.reject('boom').catch(e => console.log('caught:' + e));
            "#,
        );
        assert_eq!(c, vec!["caught:boom"]);
    }

    #[test]
    fn promise_finally_passthrough() {
        let c = evc(
            r#"
            Promise.resolve(5)
                .finally(() => console.log('fin'))
                .then(x => console.log('val:' + x));
            "#,
        );
        assert_eq!(c, vec!["fin", "val:5"]);
    }

    #[test]
    fn promise_all_basic() {
        let c = evc(
            r#"
            Promise.all([Promise.resolve(1), 2, Promise.resolve(3)])
                .then(a => console.log(a[0] + ',' + a[1] + ',' + a[2]));
            "#,
        );
        assert_eq!(c, vec!["1,2,3"]);
    }

    #[test]
    fn promise_all_rejects() {
        let c = evc(
            r#"
            Promise.all([Promise.resolve(1), Promise.reject('bad')])
                .then(() => console.log('no'))
                .catch(e => console.log('all-rejected:' + e));
            "#,
        );
        assert_eq!(c, vec!["all-rejected:bad"]);
    }

    #[test]
    fn promise_race_basic() {
        let c = evc(
            r#"
            Promise.race([Promise.resolve('win'), Promise.resolve('lose')])
                .then(x => console.log(x));
            "#,
        );
        assert_eq!(c, vec!["win"]);
    }

    #[test]
    fn new_promise_executor() {
        let c = evc(
            r#"
            new Promise((resolve, reject) => resolve(42))
                .then(x => console.log(x));
            new Promise((resolve, reject) => reject('e1'))
                .catch(e => console.log('caught:' + e));
            "#,
        );
        assert_eq!(c, vec!["42", "caught:e1"]);
    }

    #[test]
    fn async_await_order() {
        let c = evc(
            r#"
            async function f() {
                console.log('a');
                await Promise.resolve();
                console.log('b');
                await Promise.resolve();
                console.log('c');
            }
            console.log('0');
            f();
            console.log('1');
            "#,
        );
        assert_eq!(c, vec!["0", "a", "1", "b", "c"]);
    }

    #[test]
    fn await_non_promise_value() {
        let c = evc(
            r#"
            async function f() {
                let x = await 42;
                console.log('x=' + x);
            }
            f();
            console.log('done');
            "#,
        );
        assert_eq!(c, vec!["done", "x=42"]);
    }

    #[test]
    fn async_try_catch() {
        let c = evc(
            r#"
            async function f() {
                try {
                    await Promise.reject('oops');
                    console.log('no');
                } catch (e) {
                    console.log('caught:' + e);
                }
            }
            f();
            "#,
        );
        assert_eq!(c, vec!["caught:oops"]);
    }

    #[test]
    fn async_loop() {
        let c = evc(
            r#"
            async function f() {
                for (let i = 0; i < 3; i++) {
                    await Promise.resolve();
                    console.log('i=' + i);
                }
            }
            f();
            console.log('start');
            "#,
        );
        assert_eq!(c, vec!["start", "i=0", "i=1", "i=2"]);
    }

    #[test]
    fn async_arrow() {
        let c = evc(
            r#"
            const f = async () => {
                await Promise.resolve();
                return 99;
            };
            f().then(x => console.log('r=' + x));
            "#,
        );
        assert_eq!(c, vec!["r=99"]);
    }

    #[test]
    fn unhandled_rejection_reported() {
        let c = evc(r#"Promise.reject('unhandled-boom');"#);
        assert!(
            c.iter().any(|l| l.contains("UnhandledPromiseRejection") && l.contains("unhandled-boom")),
            "got: {:?}",
            c
        );
    }

    #[test]
    fn fetch_no_host_rejects() {
        let c = evc(
            r#"
            fetch('http://example.com/').then(
                () => console.log('no'),
                e => console.log('fetch-err:' + e)
            );
            "#,
        );
        assert_eq!(c, vec!["fetch-err:fetch not implemented"]);
    }

    #[test]
    fn fetch_mock_host() {
        struct MockFetch;
        impl crate::fetch::FetchHost for MockFetch {
            fn fetch(&self, url: &str) -> Result<crate::fetch::FetchResponse, String> {
                if url == "http://example.com/hi" {
                    Ok(crate::fetch::FetchResponse::new(200, "hello-body"))
                } else {
                    Err("mock: unknown url".to_string())
                }
            }
        }
        let prog = parse_source(
            r#"
            fetch('http://example.com/hi').then(r => {
                console.log('ok=' + r.ok + ',status=' + r.status);
                return r.text();
            }).then(t => console.log('body=' + t));
            "#,
        )
        .expect("parse failed");
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        ip.bind_fetch(std::rc::Rc::new(MockFetch));
        ip.run(&prog).expect("run failed");
        let c = ip.take_console();
        assert_eq!(c, vec!["ok=true,status=200", "body=hello-body"]);
    }

    #[test]
    fn regex_test_basic() {
        assert!(ev_bool(r#"/ab+c/.test('abbbc')"#));
        assert!(!ev_bool(r#"/ab+c/.test('ac')"#));
        assert!(ev_bool(r#"/^hello$/.test('hello')"#));
        assert!(!ev_bool(r#"/^hello$/.test('hello!')"#));
    }

    #[test]
    fn regex_exec_groups() {
        let c = evc(
            r#"
            let m = /(\d+)-(\d+)/.exec('12-34');
            console.log(m[0] + '|' + m[1] + '|' + m[2] + '|' + m.index);
            "#,
        );
        assert_eq!(c, vec!["12-34|12|34|0"]);
    }

    #[test]
    fn regex_char_class() {
        assert!(ev_bool(r#"/[a-z]+/.test('hello')"#));
        assert!(!ev_bool(r#"/[a-z]+/.test('HELLO')"#));
        assert!(ev_bool(r#"/[a-z]+/i.test('HELLO')"#));
        assert!(ev_bool(r#"/\d\w\s/.test('1a ')"#));
    }

    #[test]
    fn regex_global_last_index() {
        let c = evc(
            r#"
            let re = /a/g;
            console.log(re.test('aab'));
            console.log(re.lastIndex);
            console.log(re.test('aab'));
            console.log(re.lastIndex);
            console.log(re.test('aab'));
            "#,
        );
        assert_eq!(c, vec!["true", "1", "true", "2", "false"]);
    }

    #[test]
    fn regex_no_redos_hang() {
        // 灾难回溯模式必须熔断，不能 hang。
        let c = evc(
            r#"
            console.log(/(a+)+b/.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaac'));
            "#,
        );
        assert_eq!(c, vec!["false"]);
    }

    #[test]
    fn string_match_regex() {
        let c = evc(
            r#"
            let m = 'a1b2'.match(/\d/g);
            console.log(m[0] + m[1]);
            let m2 = 'abc123'.match(/(\d+)/);
            console.log(m2[0] + ':' + m2[1]);
            "#,
        );
        assert_eq!(c, vec!["12", "123:123"]);
    }

    #[test]
    fn string_replace_regex() {
        assert_eq!(ev_str(r#"'hello'.replace(/l/g, 'L')"#), "heLLo");
        assert_eq!(ev_str(r#"'aaa'.replace(/a/, 'b')"#), "baa");
        // 函数 replacer。
        let c = evc(
            r#"
            console.log('a1b2'.replace(/\d/g, d => '[' + d + ']'));
            "#,
        );
        assert_eq!(c, vec!["a[1]b[2]"]);
    }

    #[test]
    fn string_split_regex() {
        let c = evc(
            r#"
            let p = 'a1b2c3'.split(/\d/);
            console.log(p.join(','));
            "#,
        );
        assert_eq!(c, vec!["a,b,c,"]);
    }

    #[test]
    fn module_import_export() {
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        ip.register_module("math", "export const pi = 3; export function add(a, b) { return a + b; }");
        let prog = parse_source(
            r#"
            import { pi, add } from 'math';
            console.log('pi=' + pi);
            console.log('add=' + add(2, 3));
            "#,
        )
        .expect("parse failed");
        ip.run(&prog).expect("run failed");
        let c = ip.take_console();
        assert_eq!(c, vec!["pi=3", "add=5"]);
    }

    #[test]
    fn module_missing_errors() {
        let err = ev_err(r#"import { a } from 'nonexistent';"#);
        assert!(err.contains("module not found"), "got: {}", err);
    }

    #[test]
    fn module_circular_errors() {
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        ip.register_module("a", "import { x } from 'b'; export const y = 1;");
        ip.register_module("b", "import { y } from 'a'; export const x = 2;");
        let prog = parse_source(r#"import { x } from 'a';"#).expect("parse failed");
        let err = ip.run(&prog).unwrap_err();
        let msg = format!("{:?}", err);
        assert!(msg.contains("circular"), "got: {}", msg);
    }

    // ---- Phase 8：严格模式 ----

    #[test]
    fn strict_assign_undeclared_throws() {
        let err = ev_err(r#""use strict"; x = 1;"#);
        assert!(err.contains("ReferenceError"), "got: {}", err);
        assert!(err.contains("not defined"), "got: {}", err);
        // sloppy 模式旧行为不变：隐式创建全局。
        assert_eq!(ev_num("x = 1; x"), 1.0);
    }

    #[test]
    fn strict_compound_assign_undeclared_throws() {
        let err = ev_err(r#""use strict"; x += 1;"#);
        assert!(err.contains("ReferenceError"), "got: {}", err);
    }

    #[test]
    fn strict_nested_function_inherits() {
        // 外层 strict，内层函数默认 strict。
        let err = ev_err(r#""use strict"; function f() { y = 2; } f();"#);
        assert!(err.contains("ReferenceError"), "got: {}", err);
    }

    #[test]
    fn strict_function_level_directive() {
        let err = ev_err(r#"function f() { "use strict"; z = 1; } f();"#);
        assert!(err.contains("ReferenceError"), "got: {}", err);
        // 外层仍是 sloppy，不受影响。
        assert_eq!(ev_num(r#"function f() { "use strict"; } w = 3; w"#), 3.0);
    }

    #[test]
    fn strict_arrow_inherits() {
        let err = ev_err(r#""use strict"; const f = () => { q = 1; }; f();"#);
        assert!(err.contains("ReferenceError"), "got: {}", err);
    }

    #[test]
    fn strict_with_is_syntax_error() {
        let err = ev_err(r#""use strict"; with (o) {}"#);
        assert!(err.contains("SyntaxError"), "got: {}", err);
        assert!(err.contains("with"), "got: {}", err);
        // sloppy 下 with 同样不支持（显式报错而非含糊的 expected ';'）。
        let err2 = ev_err("with (o) {}");
        assert!(err2.contains("with"), "got: {}", err2);
    }

    #[test]
    fn strict_delete_non_configurable() {
        let err = ev_err(r#""use strict"; const a = [1]; delete a.length;"#);
        assert!(err.contains("TypeError"), "got: {}", err);
        // sloppy 返回 false。
        assert!(!ev_bool("const a = [1]; delete a.length"));
        // 普通属性删除不受影响。
        assert!(ev_bool(r#""use strict"; const o = {x: 1}; delete o.x"#));
    }

    #[test]
    fn strict_delete_bare_identifier() {
        let err = ev_err(r#""use strict"; let q = 1; delete q;"#);
        assert!(err.contains("SyntaxError"), "got: {}", err);
    }

    #[test]
    fn strict_octal_literal() {
        let err = ev_err(r#""use strict"; 010;"#);
        assert!(err.contains("SyntaxError"), "got: {}", err);
        assert!(err.contains("octal"), "got: {}", err);
        // sloppy 下 legacy 八进制仍按十进制求值（旧行为）。
        assert_eq!(ev_num("010"), 10.0);
    }

    #[test]
    fn strict_duplicate_params() {
        let err = ev_err(r#""use strict"; function f(a, a) {}"#);
        assert!(err.contains("SyntaxError"), "got: {}", err);
        assert!(err.contains("duplicate"), "got: {}", err);
        // sloppy 下重复形参合法（后者赢）。
        assert_eq!(ev_num("function f(a, a) { return a; } f(1, 2)"), 2.0);
    }

    #[test]
    fn strict_this_stays_undefined() {
        // 普通调用的 this 保持 undefined（本引擎 sloppy 下本就不转全局对象，
        // 此处锁定该行为）。
        assert!(ev_bool(
            r#""use strict"; function f() { return this === undefined; } f();"#
        ));
        assert!(ev_bool(
            "function f() { return this === undefined; } f();"
        ));
    }

    // ---- Phase 8：错误堆栈与子类 ----

    #[test]
    fn error_stack_format() {
        let src = "function foo() {\n  bar();\n}\nfunction bar() {\n  null.x;\n}\nfoo();";
        let err = ev_err(src);
        assert!(err.contains("TypeError:"), "got: {}", err);
        // 内层在前（V8 惯例），行列号指向调用点。
        let i_bar = err.find("at bar (<anonymous>:2:3").expect("bar frame");
        let i_foo = err.find("at foo (<anonymous>:7:1").expect("foo frame");
        assert!(i_bar < i_foo, "got: {}", err);
    }

    #[test]
    fn error_object_props() {
        assert_eq!(ev_str("new TypeError('bad').message"), "bad");
        assert_eq!(ev_str("new TypeError('bad').name"), "TypeError");
        let s = ev_str("new TypeError('bad').stack");
        assert!(s.contains("TypeError: bad"), "got: {}", s);
        // 调用形式同样构造。
        assert_eq!(ev_str("ReferenceError('x').message"), "x");
    }

    #[test]
    fn error_instanceof_chain() {
        assert!(ev_bool("new TypeError('x') instanceof TypeError"));
        assert!(ev_bool("new TypeError('x') instanceof Error"));
        assert!(ev_bool("new RangeError('x') instanceof Error"));
        assert!(!ev_bool("new Error('x') instanceof TypeError"));
    }

    #[test]
    fn error_subclass_classification() {
        assert!(ev_err("null.x").starts_with("TypeError:"), "{}", ev_err("null.x"));
        assert!(
            ev_err("zzz_not_defined_either").starts_with("ReferenceError:"),
            "{}",
            ev_err("zzz_not_defined_either")
        );
        assert!(
            ev_err("const c = 1; c = 2;").starts_with("TypeError:"),
            "{}",
            ev_err("const c = 1; c = 2;")
        );
        assert!(
            ev_err(r#"JSON.parse("{")"#).starts_with("SyntaxError:"),
            "{}",
            ev_err(r#"JSON.parse("{")"#)
        );
        assert!(
            ev_err("while (true) {}").starts_with("RangeError:"),
            "{}",
            ev_err("while (true) {}")
        );
    }

    #[test]
    fn uncaught_microtask_error_carries_stack() {
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        let prog = parse_source("queueMicrotask(() => { null.x; });").unwrap();
        ip.run(&prog).unwrap();
        let console = ip.take_console();
        let line = console
            .iter()
            .find(|l| l.contains("Microtask error:"))
            .expect("microtask error logged");
        assert!(line.contains("TypeError:"), "got: {}", line);
        assert!(line.contains("at <anonymous>"), "got: {}", line);
    }

    #[test]
    fn rejected_promise_carries_error_object() {
        // phase 8：运行时错误转拒绝值时不再是裸字符串，而是对应子类的
        // Error 对象（`e instanceof TypeError` 可用）。
        let src = r#"
            Promise.resolve().then(() => { null.x; })
                .catch(e => { console.log(e.name + "|" + (e instanceof TypeError)); });
        "#;
        let prog = parse_source(src).unwrap();
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        ip.run(&prog).unwrap();
        let console = ip.take_console();
        assert!(
            console.iter().any(|l| l == "TypeError|true"),
            "got: {:?}",
            console
        );
    }

    #[test]
    fn thrown_error_value_keeps_stack() {
        // try/catch 捕获 new 出来的 Error：stack 属性随身带。
        let src = r#"
            let s = "";
            try { throw new TypeError("boom"); }
            catch (e) { s = e.stack; }
            s;
        "#;
        let s = ev_str(src);
        assert!(s.contains("TypeError: boom"), "got: {}", s);
    }

    // ---- Phase 8：调试器钩子 ----

    #[test]
    fn debug_breakpoint_hits() {
        let src = "let a = 1;\na = 2;\na = 3;";
        let prog = parse_source(src).expect("parse failed");
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        let host = Rc::new(RefCell::new(BreakpointHost::new(vec![2])));
        ip.set_debug_host(host.clone());
        ip.run(&prog).unwrap();
        // 断点命中行列。
        assert_eq!(host.borrow().hits, vec![(2, 1)]);
        // 暂停快照：位置 + 调用栈 + 变量。
        let pauses = ip.take_debug_pauses();
        assert_eq!(pauses.len(), 1);
        assert_eq!(pauses[0].line, 2);
        assert_eq!(pauses[0].stack.len(), 0); // 顶层，无调用栈
        assert_eq!(
            pauses[0].vars.get("a").map(|s| s.as_str()),
            Some("Number(1)")
        );
    }

    #[test]
    fn debug_disabled_is_noop() {
        let src = "let a = 1;\na = 2;";
        // 不装宿主：行为完全一致，无记录。
        let prog = parse_source(src).expect("parse failed");
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        ip.run(&prog).unwrap();
        assert!(ip.take_debug_pauses().is_empty());
        // 装了宿主但关掉开关：同样无记录，结果一致。
        let prog = parse_source(src).expect("parse failed");
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        let host = Rc::new(RefCell::new(BreakpointHost::new(vec![1])));
        ip.set_debug_host(host.clone());
        ip.set_debug_enabled(false);
        ip.run(&prog).unwrap();
        assert!(host.borrow().hits.is_empty());
        assert!(ip.take_debug_pauses().is_empty());
    }

    #[test]
    fn debug_pause_inside_function() {
        let src = "function g() {\n  let v = 42;\n  return v;\n}\ng();";
        let prog = parse_source(src).expect("parse failed");
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        let host = Rc::new(RefCell::new(BreakpointHost::new(vec![2])));
        ip.set_debug_host(host.clone());
        ip.run(&prog).unwrap();
        let pauses = ip.take_debug_pauses();
        assert_eq!(pauses.len(), 1);
        // 调用栈暴露函数名。
        assert_eq!(pauses[0].stack.len(), 1);
        assert_eq!(pauses[0].stack[0].name, "g");
        // 暂停点在 `let v = 42;` 执行前：v 处于 TDZ。
        assert_eq!(
            pauses[0].vars.get("v").map(|s| s.as_str()),
            Some("<TDZ>")
        );
    }

    #[test]
    fn debug_step_over() {
        struct StepHost {
            calls: Vec<(usize, usize)>,
            stepped: bool,
        }
        impl DebugHost for StepHost {
            fn on_statement(&mut self, line: usize, col: usize) -> BreakAction {
                self.calls.push((line, col));
                if !self.stepped && line == 5 {
                    self.stepped = true;
                    return BreakAction::StepOver;
                }
                BreakAction::Continue
            }
        }
        let src = "function f() {\n  let q = 1;\n  q = 2;\n}\nf();\nlet z = 9;";
        let prog = parse_source(src).expect("parse failed");
        let mut ip = Interpreter::new();
        ip.set_vm_enabled(USE_VM.get());
        let host = Rc::new(RefCell::new(StepHost {
            calls: Vec::new(),
            stepped: false,
        }));
        ip.set_debug_host(host.clone());
        ip.run(&prog).unwrap();
        // 第 5 行 step-over：函数体内的 2、3 行被跳过，第 6 行恢复回调。
        assert_eq!(host.borrow().calls, vec![(1, 1), (5, 1), (6, 1)]);
        let pauses = ip.take_debug_pauses();
        assert_eq!(pauses.len(), 1);
        assert_eq!(pauses[0].line, 5);
    }

    // ==================== phase 9：生成器 ====================

    #[test]
    fn p9_gen_basic_next() {
        let s = ev_str(
            r#"function* g() { yield 1; yield 2; yield 3; }
               const it = g();
               const a = it.next(); const b = it.next(); const c = it.next(); const d = it.next();
               [a.value, b.value, c.value, d.value, d.done].join(",")"#,
        );
        assert_eq!(s, "1,2,3,,true");
    }

    #[test]
    fn p9_gen_yield_value_passing() {
        let s = ev_str(
            r#"function* g() { const x = yield 1; const y = yield x + 10; yield y + 100; }
               const it = g();
               it.next(); const b = it.next(5); const c = it.next(7);
               [b.value, c.value].join(",")"#,
        );
        assert_eq!(s, "15,107");
    }

    #[test]
    fn p9_gen_return_early() {
        let s = ev_str(
            r#"function* g() { yield 1; yield 2; yield 3; }
               const it = g();
               it.next(); const r = it.return(99); const n = it.next();
               [r.value, r.done, n.done].join(",")"#,
        );
        assert_eq!(s, "99,true,true");
    }

    #[test]
    fn p9_gen_throw_into() {
        let s = ev_str(
            r#"function* g() { try { yield 1; yield 2; } catch (e) { yield "caught:" + e; } yield 3; }
               const it = g();
               it.next(); const t = it.throw("boom"); const n = it.next();
               [t.value, n.value].join(",")"#,
        );
        assert_eq!(s, "caught:boom,3");
    }

    #[test]
    fn p9_gen_throw_uncaught() {
        let s = ev_str(
            r#"function* g() { yield 1; yield 2; }
               const it = g();
               it.next();
               try { it.throw("kaboom"); "no-throw"; } catch (e) { "threw:" + e; }"#,
        );
        assert_eq!(s, "threw:kaboom");
    }

    #[test]
    fn p9_gen_yield_star() {
        let s = ev_str(
            r#"function* inner() { yield 1; yield 2; return 2; }
               function* outer() { yield 0; const r = yield* inner(); yield r + 10; }
               [...outer()].join(",")"#,
        );
        assert_eq!(s, "0,1,2,12");
    }

    #[test]
    fn p9_gen_for_of() {
        let s = ev_str(
            r#"function* g() { yield "a"; yield "b"; }
               let s = "";
               for (const x of g()) { s += x; }
               s"#,
        );
        assert_eq!(s, "ab");
    }

    #[test]
    fn p9_gen_try_finally() {
        let s = ev_str(
            r#"const log = [];
               function* g() { try { yield 1; yield 2; } finally { log.push("fin"); } }
               const it = g();
               it.next(); it.return(0);
               log.join(",")"#,
        );
        assert_eq!(s, "fin");
    }

    #[test]
    fn p9_gen_method_shorthand() {
        let n = ev_num(
            r#"const o = { *gen() { yield 7; } };
               o.gen().next().value"#,
        );
        assert_eq!(n, 7.0);
    }

    // ==================== phase 9：Proxy ====================

    #[test]
    fn p9_proxy_get_trap() {
        let s = ev_str(
            r#"const p = new Proxy({a: 1}, { get(t, k) { return "got:" + k; } });
               p.a"#,
        );
        assert_eq!(s, "got:a");
    }

    #[test]
    fn p9_proxy_no_trap_passthrough() {
        let n = ev_num(
            r#"const p = new Proxy({a: 42}, {});
               p.a"#,
        );
        assert_eq!(n, 42.0);
    }

    #[test]
    fn p9_proxy_set_trap() {
        let s = ev_str(
            r#"const seen = [];
               const p = new Proxy({}, { set(t, k, v) { seen.push(k + "=" + v); t[k] = v; return true; } });
               p.x = 5;
               seen.join(",")"#,
        );
        assert_eq!(s, "x=5");
    }

    #[test]
    fn p9_proxy_has_trap() {
        let b = ev_bool(
            r#"const p = new Proxy({}, { has(t, k) { return k === "magic"; } });
               ("magic" in p) && !("other" in p)"#,
        );
        assert!(b);
    }

    #[test]
    fn p9_proxy_apply_trap() {
        let s = ev_str(
            r#"function add(a, b) { return a + b; }
               const p = new Proxy(add, { apply(t, th, args) { return "sum=" + (args[0] + args[1]); } });
               p(3, 4)"#,
        );
        assert_eq!(s, "sum=7");
    }

    #[test]
    fn p9_proxy_construct_trap() {
        let s = ev_str(
            r#"function C(x) { this.x = x; }
               const P = new Proxy(C, { construct(t, args) { return { trapped: args[0] }; } });
               const o = new P(123);
               "trapped:" + o.trapped"#,
        );
        assert_eq!(s, "trapped:123");
    }

    #[test]
    fn p9_proxy_construct_passthrough() {
        let s = ev_str(
            r#"function C(x) { this.x = x; }
               const P = new Proxy(C, {});
               const o = new P(9);
               "x=" + o.x"#,
        );
        assert_eq!(s, "x=9");
    }

    #[test]
    fn p9_proxy_target_must_be_object() {
        let r = eval_source(r#"new Proxy(42, {})"#);
        assert!(r.is_err());
    }

    // ==================== phase 9：类 ====================

    #[test]
    fn p9_class_basic() {
        let s = ev_str(
            r#"class Point { constructor(x, y) { this.x = x; this.y = y; }
                 sum() { return this.x + this.y; } }
               const p = new Point(3, 4);
               p.sum() + "," + (p instanceof Point)"#,
        );
        assert_eq!(s, "7,true");
    }

    #[test]
    fn p9_class_inheritance_super() {
        let s = ev_str(
            r#"class A { constructor(x) { this.x = x; } getX() { return this.x; } }
               class B extends A { constructor(x, y) { super(x); this.y = y; }
                 sum() { return super.getX() + this.y; } }
               const b = new B(3, 4);
               b.sum() + "," + (b instanceof A) + "," + (b instanceof B)"#,
        );
        assert_eq!(s, "7,true,true");
    }

    #[test]
    fn p9_class_default_derived_ctor() {
        let s = ev_str(
            r#"class A { constructor(x) { this.x = x; } }
               class B extends A {}
               const b = new B(42);
               b.x + "," + (b instanceof B)"#,
        );
        assert_eq!(s, "42,true");
    }

    #[test]
    fn p9_class_static() {
        let s = ev_str(
            r#"class C { static count = 0; static inc() { return ++C.count; } }
               C.inc() + "," + C.inc() + "," + C.count"#,
        );
        assert_eq!(s, "1,2,2");
    }

    #[test]
    fn p9_class_static_inheritance() {
        let s = ev_str(
            r#"class A { static who() { return "A"; } }
               class B extends A {}
               B.who()"#,
        );
        assert_eq!(s, "A");
    }

    #[test]
    fn p9_class_private_field() {
        let n = ev_num(
            r#"class C { #x = 10; get() { return this.#x; } set(v) { this.#x = v; } }
               const c = new C();
               c.set(99);
               c.get()"#,
        );
        assert_eq!(n, 99.0);
    }

    #[test]
    fn p9_class_private_method() {
        let n = ev_num(
            r#"class C { #dbl(x) { return x * 2; } calc(x) { return this.#dbl(x); } }
               new C().calc(21)"#,
        );
        assert_eq!(n, 42.0);
    }

    #[test]
    fn p9_class_private_in() {
        let b = ev_bool(
            r#"class C { #x = 1; static hasX(o) { return #x in o; } }
               C.hasX(new C()) && !C.hasX({})"#,
        );
        assert!(b);
    }

    #[test]
    fn p9_class_extends_null() {
        let b = ev_bool(
            r#"class C extends null {}
               const c = new C();
               Object.getPrototypeOf(c) === C.prototype"#,
        );
        assert!(b);
    }

    #[test]
    fn p9_class_getter_setter() {
        let n = ev_num(
            r#"class C { constructor() { this._v = 0; }
                 get v() { return this._v; } set v(x) { this._v = x * 2; } }
               const c = new C();
               c.v = 21;
               c.v"#,
        );
        assert_eq!(n, 42.0);
    }

    #[test]
    fn p9_class_expr() {
        let n = ev_num(
            r#"const K = class { constructor(n) { this.n = n; } };
               new K(5).n"#,
        );
        assert_eq!(n, 5.0);
    }

    #[test]
    fn p9_class_field_init_order() {
        let s = ev_str(
            r#"class C { a = 1; b = this.a + 1; constructor() { this.c = this.b + 1; } }
               const c = new C();
               [c.a, c.b, c.c].join(",")"#,
        );
        assert_eq!(s, "1,2,3");
    }

    #[test]
    fn p9_super_assign_to_this() {
        let s = ev_str(
            r#"class A { constructor() { this.base = "base"; } }
               class B extends A { constructor() { super(); super.extra = 7; } }
               const b = new B();
               b.base + "," + b.extra"#,
        );
        assert_eq!(s, "base,7");
    }

    // ------------------------------------------------------------------
    // Phase 12：GC 优化
    // ------------------------------------------------------------------

    #[test]
    fn p12_constructor_backref_weak() {
        // phase 12：`prototype.constructor` 改为弱回指，语义不变。
        assert!(evd_bool(
            r#"class P { constructor(x) { this.x = x; } get() { return this.x; } }
               P.prototype.constructor === P"#
        ));
        assert!(evd_bool(
            r#"class P { constructor(x) { this.x = x; } }
               new P(1).constructor === P"#
        ));
        // 用户显式设置的 constructor 属性优先于弱回指。
        assert!(evd_bool(
            r#"class P {}
               P.prototype.constructor = 42;
               P.prototype.constructor === 42"#
        ));
    }

    #[test]
    fn p12_class_block_scoped() {
        // phase 12：类声明是块级作用域（此前泄漏到 var 环境）。
        assert_eq!(evd_str(r#"{ class Q {} } typeof Q"#), "undefined");
        assert!(evd_bool(
            r#"function f() { class R {} return new R(); }
               f() instanceof Object"#
        ));
    }

    #[test]
    fn p12_escaped_closure_survives_breaker() {
        // 逃逸的闭包不能被环断开器破坏（断开器必须保守放弃）。
        assert_eq!(
            evd_num(r#"let f; { function g() { return 42; } f = g; } f()"#),
            42.0
        );
        assert_eq!(
            evd_num(
                r#"function outer() { let v = 7; return function() { return v * 2; }; }
                   const h = outer(); h()"#
            ),
            14.0
        );
    }

    #[test]
    fn p12_super_via_weak_home_object() {
        // phase 12：home_object 改为弱引用后 super 仍正常。
        assert_eq!(
            evd_num(
                r#"class A { greet() { return 1; } }
                   class B extends A { greet() { return super.greet() + 10; } }
                   new B().greet()"#
            ),
            11.0
        );
    }

    #[test]
    fn p12_user_object_cycle_semantics() {
        // 用户手写的对象环语义不变（作用域退出后由断开器回收，不影响求值）。
        assert!(evd_bool(r#"let o = {}; o.self = o; o.self === o"#));
        assert!(evd_bool(r#"let a = {}; let b = { ref: a }; a.ref = b; a.ref.ref === a"#));
    }

    #[test]
    fn p12_break_scope_cycles_unit() {
        use crate::value::{Env, Value};
        // 命名函数自环：断开器应返回 true 且函数仍可调用。
        let env = Env::new_global();
        let f = crate::interpreter::eval_source("function foo() { return 9; } foo()")
            .expect("eval failed");
        assert!(matches!(f, Value::Number(n) if n == 9.0));
        // 全局环境的环由 Drop 兜底；这里直接验证块级环境。
        let block = Env::child(&env);
        Env::declare_lexical(&block, "bar", crate::value::DeclKind::Let).unwrap();
        // 无引用值的槽位 → 快路径返回 false。
        assert!(!Env::break_scope_cycles(&block));
    }

    #[test]
    fn p12_weakmap_sweep_amortized_correct() {
        // phase 12：清扫摊销后 WeakMap 语义不变（含键回收后的行为）。
        assert!(evd_bool(
            r#"const wm = new WeakMap();
               let k1 = { id: 1 }, k2 = { id: 2 };
               wm.set(k1, "a"); wm.set(k2, "b");
               let ok = wm.get(k1) === "a" && wm.has(k2);
               k1 = null;
               // 触发多次操作（摊销清扫），已回收的键不应误判。
               for (let i = 0; i < 200; i++) { wm.set({ t: i }, i); }
               ok && wm.get(k2) === "b""#
        ));
    }

    #[test]
    fn p12_reduce_index_arg_spec() {
        // phase 12：reduce 回调的 index 参数改为规范的数组下标
        // （无初值时从 1 开始；此前是去掉首元素后的相对下标）。
        assert_eq!(
            evd_str(r#"[10, 20, 30].reduce((a, b, i) => a + ":" + i, "s")"#),
            "s:0:1:2"
        );
        assert_eq!(
            evd_str(r#"[10, 20, 30].reduce((a, b, i) => a + ":" + i)"#),
            "10:1:2"
        );
    }

    #[test]
    fn p12_array_higher_order_mutation_safe() {
        // phase 12：高阶函数改为按下标逐个取值（不再整体克隆），
        // 回调内 push 不影响访问范围（规范：长度取一次）。
        assert_eq!(
            evd_str(
                r#"let a = [1, 2, 3];
                   let seen = [];
                   a.forEach((x, i, arr) => { seen.push(i); if (i === 0) arr.push(99); });
                   seen.join(",") + "|" + a.length"#
            ),
            "0,1,2|4"
        );
        // 回调内删除元素：已删下标读到 undefined（规范 Get 语义）。
        assert_eq!(
            evd_num(
                r#"let a = [1, 2, 3];
                   a.map((x, i, arr) => { if (i === 1) arr.pop(); return x; });
                   a.length"#
            ),
            2.0
        );
    }

    // ------------------------------------------------------------------
    // Phase 10：标准库补完
    // ------------------------------------------------------------------

    #[test]
    fn p10_map_basic() {
        let s = ev_str(
            r#"const m = new Map();
               m.set("a", 1); m.set("b", 2); m.set("a", 10);
               [m.size, m.get("a"), m.get("b"), String(m.get("zzz")), m.has("b"), m.has("zzz")].join(",")"#,
        );
        assert_eq!(s, "2,10,2,undefined,true,false");
    }

    #[test]
    fn p10_map_delete_clear() {
        let s = ev_str(
            r#"const m = new Map([[1,"x"],[2,"y"]]);
               const d1 = m.delete(1), d2 = m.delete(99);
               m.clear();
               [d1, d2, m.size, m.has(2)].join(",")"#,
        );
        assert_eq!(s, "true,false,0,false");
    }

    #[test]
    fn p10_map_nan_key() {
        // SameValueZero：NaN 键可查回。
        assert_eq!(ev_bool(r#"const m = new Map(); m.set(NaN, "n"); m.get(NaN) === "n""#), true);
        assert_eq!(ev_bool(r#"const m = new Map(); m.set(0, "z"); m.get(-0) === "z""#), true);
    }

    #[test]
    fn p10_map_iter() {
        let s = ev_str(
            r#"const m = new Map([["a",1],["b",2]]);
               const ks = [], vs = [], es = [];
               m.forEach((v,k) => { ks.push(k); vs.push(v); });
               for (const e of m) es.push(e[0] + "=" + e[1]);
               [m.keys().join(","), m.values().join(","), m.entries().map(e=>e.join(":")).join(";"),
                ks.join(","), vs.join(","), es.join(";")].join("|")"#,
        );
        assert_eq!(s, "a,b|1,2|a:1;b:2|a,b|1,2|a=1;b=2");
    }

    #[test]
    fn p10_map_typeof() {
        assert_eq!(ev_str(r#"typeof new Map()"#), "object");
        assert_eq!(ev_bool(r#"new Map() instanceof Map"#), true);
        assert_eq!(ev_str(r#"String(new Map())"#), "[object Map]");
    }

    #[test]
    fn p10_set_basic() {
        let s = ev_str(
            r#"const s = new Set([1,2,2,3]);
               s.add(4); s.add(2);
               const d = s.delete(3);
               [s.size, s.has(2), s.has(9), d, [...s].join(",")].join("|")"#,
        );
        assert_eq!(s, "3|true|false|true|1,2,4");
    }

    #[test]
    fn p10_set_foreach() {
        let s = ev_str(
            r#"const s = new Set(["a","b"]); const out = [];
               s.forEach(v => out.push(v));
               out.join(",")"#,
        );
        assert_eq!(s, "a,b");
    }

    #[test]
    fn p10_weakmap_basic() {
        let s = ev_str(
            r#"const wm = new WeakMap(); const k1 = {}, k2 = {};
               wm.set(k1, "v1"); wm.set(k2, "v2");
               const h1 = wm.has(k1), g1 = wm.get(k1), d1 = wm.delete(k1);
               [h1, g1, d1, wm.has(k1), String(wm.get(k1))].join(",")"#,
        );
        assert_eq!(s, "true,v1,true,false,undefined");
    }

    #[test]
    fn p10_weakmap_key_must_be_object() {
        assert_eq!(ev_err(r#"new WeakMap().set(1, "x")"#).contains("object"), true);
        assert_eq!(ev_bool(r#"const ws = new WeakSet(); ws.has(1) === false"#), true);
    }

    #[test]
    fn p10_weakset_basic() {
        let s = ev_str(
            r#"const ws = new WeakSet(); const o = {};
               ws.add(o); ws.add(o);
               const h = ws.has(o), d = ws.delete(o);
               [h, d, ws.has(o)].join(",")"#,
        );
        assert_eq!(s, "true,true,false");
    }

    #[test]
    fn p10_arraybuffer_basic() {
        let s = ev_str(
            r#"const b = new ArrayBuffer(16);
               [b.byteLength, b.slice(4, 8).byteLength].join(",")"#,
        );
        assert_eq!(s, "16,4");
    }

    #[test]
    fn p10_typedarray_all_kinds() {
        // 9 种全建出来，读写一圈。
        let s = ev_str(
            r#"const ctors = [Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Uint16Array,
                             Int32Array,Uint32Array,Float32Array,Float64Array];
               const names = [];
               for (const C of ctors) {
                 const a = new C(2); a[0] = 1; a[1] = 2.5;
                 names.push(a.length + ":" + a[0] + ":" + C.BYTES_PER_ELEMENT);
               }
               names.join("|")"#,
        );
        assert_eq!(
            s,
            "2:1:1|2:1:1|2:1:1|2:1:2|2:1:2|2:1:4|2:1:4|2:1:4|2:1:8"
        );
    }

    #[test]
    fn p10_typedarray_clamp() {
        let s = ev_str(
            r#"const a = new Uint8ClampedArray(3);
               a[0] = 300; a[1] = -5; a[2] = 127.6;
               [a[0], a[1], a[2]].join(",")"#,
        );
        assert_eq!(s, "255,0,128");
    }

    #[test]
    fn p10_typedarray_wrap() {
        // Uint8 回绕、Int8 符号。
        let s = ev_str(
            r#"const u = new Uint8Array(1); u[0] = 256;
               const i = new Int8Array(1); i[0] = 128;
               [u[0], i[0]].join(",")"#,
        );
        assert_eq!(s, "0,-128");
    }

    #[test]
    fn p10_typedarray_from_buffer() {
        // 视图共享内存。
        let s = ev_str(
            r#"const b = new ArrayBuffer(8);
               const u = new Uint8Array(b); u[0] = 42;
               const v = new DataView(b);
               const u2 = new Uint8Array(b, 2, 3); u2[0] = 7;
               [u.length, v.getUint8(0), u2.length, u[2], u.buffer === b].join(",")"#,
        );
        assert_eq!(s, "8,42,3,7,true");
    }

    #[test]
    fn p10_typedarray_methods() {
        let s = ev_str(
            r#"const a = new Uint8Array([3,1,2]);
               const doubled = []; a.forEach(x => doubled.push(x*2));
               const mapped = a.map(x => x + 10);
               const filtered = a.filter(x => x > 1);
               const sub = a.subarray(1, 3);
               const sl = a.slice(0, 2);
               a.fill(9, 0, 1);
               [doubled.join(","), mapped.join(","), filtered.join(","),
                sub.join(","), sl.join(","), a.join(","),
                a.indexOf(9), a.includes(2), a.at(-1)].join("|")"#,
        );
        assert_eq!(s, "6,2,4|13,11,12|3,2|1,2|3,1|9,1,2|0|true|2");
    }

    #[test]
    fn p10_typedarray_forof() {
        assert_eq!(
            ev_num(r#"let sum = 0; for (const x of new Int16Array([1,2,3])) sum += x; sum"#),
            6.0
        );
    }

    #[test]
    fn p10_dataview_basic() {
        let s = ev_str(
            r#"const b = new ArrayBuffer(8); const v = new DataView(b);
               v.setInt16(0, 0x1234); v.setFloat32(4, 1.5, true);
               [v.getInt16(0).toString(16), v.getUint8(1).toString(16),
                v.getFloat32(4, true), v.byteLength].join(",")"#,
        );
        assert_eq!(s, "1234,34,1.5,8");
    }

    #[test]
    fn p10_dataview_little_endian() {
        let s = ev_str(
            r#"const v = new DataView(new ArrayBuffer(4));
               v.setUint32(0, 0x01020304, true);
               [v.getUint8(0), v.getUint8(3), v.getUint32(0, true)].join(",")"#,
        );
        assert_eq!(s, "4,1,16909060");
    }

    #[test]
    fn p10_date_basic() {
        assert_eq!(ev_num(r#"new Date(0).getTime()"#), 0.0);
        assert_eq!(ev_str(r#"new Date(0).toISOString()"#), "1970-01-01T00:00:00.000Z");
        assert_eq!(ev_num(r#"new Date(2026, 0, 1).getUTCFullYear()"#), 2026.0);
        assert_eq!(ev_num(r#"new Date(2026, 0, 1).getUTCMonth()"#), 0.0);
        assert_eq!(ev_num(r#"Date.UTC(1970,0,1)"#), 0.0);
        assert!(ev_num(r#"Date.now()"#) > 1700000000000.0);
    }

    #[test]
    fn p10_date_components() {
        // 2026-10-04 是周日。
        let s = ev_str(
            r#"const d = new Date(Date.UTC(2026, 9, 4));
               [d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), d.getUTCDay(),
                d.getUTCHours(), d.toISOString()].join(",")"#,
        );
        assert_eq!(s, "2026,9,4,0,0,2026-10-04T00:00:00.000Z");
    }

    #[test]
    fn p10_intl_numberformat_decimal() {
        assert_eq!(ev_str(r#"new Intl.NumberFormat("en-US").format(1234567.891)"#), "1,234,567.891");
        assert_eq!(ev_str(r#"new Intl.NumberFormat("de-DE").format(1234567.891)"#), "1.234.567,891");
        assert_eq!(ev_str(r#"new Intl.NumberFormat("en-US").format(-42.5)"#), "-42.5");
    }

    #[test]
    fn p10_intl_numberformat_currency_percent() {
        assert_eq!(
            ev_str(r#"new Intl.NumberFormat("en-US", {style:"currency", currency:"USD"}).format(1234.5)"#),
            "$1,234.50"
        );
        assert_eq!(
            ev_str(r#"new Intl.NumberFormat("en-US", {style:"currency", currency:"CNY"}).format(99)"#),
            "¥99.00"
        );
        assert_eq!(
            ev_str(r#"new Intl.NumberFormat("en-US", {style:"percent"}).format(0.456)"#),
            "46%"
        );
    }

    #[test]
    fn p10_intl_numberformat_options() {
        assert_eq!(
            ev_str(r#"new Intl.NumberFormat("en-US", {minimumFractionDigits:2}).format(1.5)"#),
            "1.50"
        );
        assert_eq!(
            ev_str(r#"new Intl.NumberFormat("en-US", {maximumFractionDigits:1}).format(1.56)"#),
            "1.6"
        );
        assert_eq!(
            ev_str(r#"new Intl.NumberFormat("en-US", {useGrouping:false}).format(1234567)"#),
            "1234567"
        );
    }

    #[test]
    fn p10_intl_numberformat_parts_resolved() {
        let s = ev_str(
            r#"const nf = new Intl.NumberFormat("en-US", {style:"currency", currency:"EUR"});
               const parts = nf.formatToParts(1234.5).map(p => p.type + "=" + p.value).join(";");
               const ro = nf.resolvedOptions();
               parts + "|" + ro.locale + "," + ro.style + "," + ro.currency"#,
        );
        assert!(s.contains("currency=€"), "{}", s);
        assert!(s.contains("integer=1"), "{}", s);
        assert!(s.contains("group=,"), "{}", s);
        assert!(s.ends_with("en-US,currency,EUR"), "{}", s);
    }

    #[test]
    fn p10_intl_datetimeformat() {
        let s = ev_str(
            r#"const df = new Intl.DateTimeFormat("en-US", {year:"numeric", month:"2-digit", day:"2-digit"});
               df.format(Date.UTC(2026, 9, 4))"#,
        );
        assert_eq!(s, "10/04/2026");
    }

    #[test]
    fn p10_intl_datetimeformat_time() {
        let s = ev_str(
            r#"const df = new Intl.DateTimeFormat("en-US", {hour:"numeric", minute:"2-digit"});
               df.format(Date.UTC(2026, 9, 4))"#,
        );
        assert_eq!(s, "12:00 AM");
    }

    #[test]
    fn p10_intl_datetimeformat_zh() {
        let s = ev_str(
            r#"const df = new Intl.DateTimeFormat("zh-CN", {year:"numeric", month:"numeric", day:"numeric"});
               df.format(Date.UTC(2026, 9, 4))"#,
        );
        assert_eq!(s, "2026/10/4");
    }

    #[test]
    fn p10_string_extra() {
        let s = ev_str(
            r#"["5".padStart(3,"0"), "5".padEnd(3,"0"), "ab".repeat(3),
               "  x  ".trimStart(), "  x  ".trimEnd(),
               "aaa".replaceAll("a","b"), "hello".at(-1), "hello".lastIndexOf("l"),
               String.fromCharCode(65,66)].join("|")"#,
        );
        assert_eq!(s, "005|500|ababab|x  |  x|bbb|o|3|AB");
    }

    #[test]
    fn p10_array_extra() {
        let s = ev_str(
            r##"const a = [1,2,3,4];
               const parts = [
                 a.findIndex(x => x > 2),
                 a.some(x => x > 3), a.every(x => x > 0),
                 [1,[2,[3]]].flat(2).join(","),
                 [3,1,2].sort().join(","),
                 [3,1,2].sort((x,y) => x - y).join(","),
                 a.at(-1), a.lastIndexOf(2),
                 [1,2].flatMap(x => [x, x*10]).join(",")
               ];
               const sp = [1,2,3,4,5]; const removed = sp.splice(1, 2, 9);
               const f = [0,0,0]; f.fill(7, 1);
               const r = [1,2,3]; r.reverse();
               parts.join("|") + "#" + removed.join(",") + "#" + sp.join(",") +
                 "#" + f.join(",") + "#" + r.join(",")"##,
        );
        assert_eq!(
            s,
            "2|true|true|1,2,3|1,2,3|1,2,3|4|1|1,10,2,20#2,3#1,9,4,5#0,7,7#3,2,1"
        );
    }

    #[test]
    fn p10_object_extra() {
        let s = ev_str(
            r#"const proto = {greet() { return "hi"; }};
               const o = Object.create(proto);
               Object.defineProperty(o, "x", {value: 42});
               [o.greet(), o.x, Object.hasOwn(o,"x"), Object.hasOwn(o,"greet"),
                Object.getOwnPropertyNames(o).join(","),
                Object.is(NaN, NaN), Object.is(0, -0)].join("|")"#,
        );
        assert_eq!(s, "hi|42|true|false|x|true|false");
    }

    #[test]
    fn p10_math_extra() {
        let s = ev_str(
            r#"[Math.trunc(1.9), Math.trunc(-1.9), Math.sign(-5), Math.sign(0),
               Math.hypot(3,4), Math.cbrt(27), Math.log2(8), Math.log10(1000),
               Math.imul(3,5), Math.clz32(1), Math.fround(1.5)].join(",")"#,
        );
        assert_eq!(s, "1,-1,-1,0,5,3,3,3,15,31,1.5");
    }

    #[test]
    fn p10_number_extra() {
        let s = ev_str(
            r#"[(1.567).toFixed(2), (1234.5).toExponential(2), Number.MAX_SAFE_INTEGER,
               Number.isSafeInteger(9007199254740991), Number.isSafeInteger(9007199254740992),
               (255).toString(16), Number.parseInt("42"), Number.parseFloat("3.14")].join("|")"#,
        );
        assert_eq!(s, "1.57|1.23e3|9007199254740991|true|false|ff|42|3.14");
    }

    #[test]
    fn p10_globals() {
        assert_eq!(ev_bool(r#"Number.isNaN(NaN)"#), true);
        assert_eq!(ev_bool(r#"isFinite(42) && !isFinite(Infinity)"#), true);
        assert_eq!(ev_str(r#"String(123)"#), "123");
        assert_eq!(ev_str(r#"String(null)"#), "null");
        assert_eq!(ev_num(r#"Number("42")"#), 42.0);
        assert_eq!(ev_bool(r#"Boolean(0) === false && Boolean("x") === true"#), true);
    }

    #[test]
    fn p10_typeof_all() {
        let s = ev_str(
            r#"[typeof new Map(), typeof new Set(), typeof new WeakMap(), typeof new WeakSet(),
               typeof new ArrayBuffer(1), typeof new Uint8Array(1), typeof new DataView(new ArrayBuffer(1)),
               typeof new Date(), typeof Intl, typeof Map].join(",")"#,
        );
        // 注：构造器本身 typeof 为 object（与本引擎 typeof Array 一致的历史偏差）。
        assert_eq!(s, "object,object,object,object,object,object,object,object,object,object");
    }

    // ---- Phase 11：VM 双跑（每个既有测试在 VM 模式下重跑） ----

    #[test]
    fn vm_proto_chain_inheritance() {
        let _g = VmGuard::new();
        proto_chain_inheritance();
    }

    #[test]
    fn vm_instanceof_via_proto_chain() {
        let _g = VmGuard::new();
        instanceof_via_proto_chain();
    }

    #[test]
    fn vm_array_higher_order() {
        let _g = VmGuard::new();
        array_higher_order();
    }

    #[test]
    fn vm_array_mutators() {
        let _g = VmGuard::new();
        array_mutators();
    }

    #[test]
    fn vm_string_prototype_methods() {
        let _g = VmGuard::new();
        string_prototype_methods();
    }

    #[test]
    fn vm_object_statics() {
        let _g = VmGuard::new();
        object_statics();
    }

    #[test]
    fn vm_number_and_array_statics() {
        let _g = VmGuard::new();
        number_and_array_statics();
    }

    #[test]
    fn vm_function_call_apply() {
        let _g = VmGuard::new();
        function_call_apply();
    }

    #[test]
    fn vm_dom_read() {
        let _g = VmGuard::new();
        dom_read();
    }

    #[test]
    fn vm_dom_write() {
        let _g = VmGuard::new();
        dom_write();
    }

    #[test]
    fn vm_dom_window() {
        let _g = VmGuard::new();
        dom_window();
    }

    #[test]
    fn vm_task_fifo_order_after_main() {
        let _g = VmGuard::new();
        task_fifo_order_after_main();
    }

    #[test]
    fn vm_task_nested_scheduling() {
        let _g = VmGuard::new();
        task_nested_scheduling();
    }

    #[test]
    fn vm_task_error_does_not_stop_queue() {
        let _g = VmGuard::new();
        task_error_does_not_stop_queue();
    }

    #[test]
    fn vm_dom_append_child_mounts() {
        let _g = VmGuard::new();
        dom_append_child_mounts();
    }

    #[test]
    fn vm_dom_remove_child_detaches() {
        let _g = VmGuard::new();
        dom_remove_child_detaches();
    }

    #[test]
    fn vm_dom_insert_before_order() {
        let _g = VmGuard::new();
        dom_insert_before_order();
    }

    #[test]
    fn vm_dom_append_cycle_is_ignored() {
        let _g = VmGuard::new();
        dom_append_cycle_is_ignored();
    }

    #[test]
    fn vm_dom_inner_html_setter_parses() {
        let _g = VmGuard::new();
        dom_inner_html_setter_parses();
    }

    #[test]
    fn vm_dom_outer_html_getter() {
        let _g = VmGuard::new();
        dom_outer_html_getter();
    }

    #[test]
    fn vm_dom_class_list() {
        let _g = VmGuard::new();
        dom_class_list();
    }

    #[test]
    fn vm_dom_compound_selector() {
        let _g = VmGuard::new();
        dom_compound_selector();
    }

    #[test]
    fn vm_dom_descendant_selector() {
        let _g = VmGuard::new();
        dom_descendant_selector();
    }

    #[test]
    fn vm_dom_invalid_selector_returns_empty() {
        let _g = VmGuard::new();
        dom_invalid_selector_returns_empty();
    }

    #[test]
    fn vm_dom_click_fires_listener() {
        let _g = VmGuard::new();
        dom_click_fires_listener();
    }

    #[test]
    fn vm_dom_click_without_listener_is_noop() {
        let _g = VmGuard::new();
        dom_click_without_listener_is_noop();
    }

    #[test]
    fn vm_dom_add_event_listener_ignores_non_function() {
        let _g = VmGuard::new();
        dom_add_event_listener_ignores_non_function();
    }

    #[test]
    fn vm_dom_create_element() {
        let _g = VmGuard::new();
        dom_create_element();
    }

    #[test]
    fn vm_proto_methods_visible_via_chain() {
        let _g = VmGuard::new();
        proto_methods_visible_via_chain();
    }

    #[test]
    fn vm_arithmetic_precedence() {
        let _g = VmGuard::new();
        arithmetic_precedence();
    }

    #[test]
    fn vm_string_concat_vs_add() {
        let _g = VmGuard::new();
        string_concat_vs_add();
    }

    #[test]
    fn vm_closure_counter() {
        let _g = VmGuard::new();
        closure_counter();
    }

    #[test]
    fn vm_fib_recursion() {
        let _g = VmGuard::new();
        fib_recursion();
    }

    #[test]
    fn vm_while_accumulate() {
        let _g = VmGuard::new();
        while_accumulate();
    }

    #[test]
    fn vm_for_accumulate() {
        let _g = VmGuard::new();
        for_accumulate();
    }

    #[test]
    fn vm_try_catch_finally() {
        let _g = VmGuard::new();
        try_catch_finally();
    }

    #[test]
    fn vm_var_hoisting() {
        let _g = VmGuard::new();
        var_hoisting();
    }

    #[test]
    fn vm_let_block_scope_and_tdz() {
        let _g = VmGuard::new();
        let_block_scope_and_tdz();
    }

    #[test]
    fn vm_arrow_functions() {
        let _g = VmGuard::new();
        arrow_functions();
    }

    #[test]
    fn vm_object_array_access() {
        let _g = VmGuard::new();
        object_array_access();
    }

    #[test]
    fn vm_console_log_captured() {
        let _g = VmGuard::new();
        console_log_captured();
    }

    #[test]
    fn vm_runtime_error_undefined_var() {
        let _g = VmGuard::new();
        runtime_error_undefined_var();
    }

    #[test]
    fn vm_runtime_error_call_non_function() {
        let _g = VmGuard::new();
        runtime_error_call_non_function();
    }

    #[test]
    fn vm_math_builtins() {
        let _g = VmGuard::new();
        math_builtins();
    }

    #[test]
    fn vm_json_roundtrip() {
        let _g = VmGuard::new();
        json_roundtrip();
    }

    #[test]
    fn vm_loose_equality() {
        let _g = VmGuard::new();
        loose_equality();
    }

    #[test]
    fn vm_for_in_for_of() {
        let _g = VmGuard::new();
        for_in_for_of();
    }

    #[test]
    fn vm_switch_fallthrough() {
        let _g = VmGuard::new();
        switch_fallthrough();
    }

    #[test]
    fn vm_new_constructor_and_this() {
        let _g = VmGuard::new();
        new_constructor_and_this();
    }

    #[test]
    fn vm_step_limit_stops_infinite_loop() {
        let _g = VmGuard::new();
        step_limit_stops_infinite_loop();
    }

    #[test]
    fn vm_const_reassign_errors() {
        let _g = VmGuard::new();
        const_reassign_errors();
    }

    #[test]
    fn vm_uncaught_throw_message() {
        let _g = VmGuard::new();
        uncaught_throw_message();
    }

    #[test]
    fn vm_parse_int_float() {
        let _g = VmGuard::new();
        parse_int_float();
    }

    #[test]
    fn vm_microtask_before_macrotask() {
        let _g = VmGuard::new();
        microtask_before_macrotask();
    }

    #[test]
    fn vm_promise_chain() {
        let _g = VmGuard::new();
        promise_chain();
    }

    #[test]
    fn vm_promise_catch() {
        let _g = VmGuard::new();
        promise_catch();
    }

    #[test]
    fn vm_promise_finally_passthrough() {
        let _g = VmGuard::new();
        promise_finally_passthrough();
    }

    #[test]
    fn vm_promise_all_basic() {
        let _g = VmGuard::new();
        promise_all_basic();
    }

    #[test]
    fn vm_promise_all_rejects() {
        let _g = VmGuard::new();
        promise_all_rejects();
    }

    #[test]
    fn vm_promise_race_basic() {
        let _g = VmGuard::new();
        promise_race_basic();
    }

    #[test]
    fn vm_new_promise_executor() {
        let _g = VmGuard::new();
        new_promise_executor();
    }

    #[test]
    fn vm_async_await_order() {
        let _g = VmGuard::new();
        async_await_order();
    }

    #[test]
    fn vm_await_non_promise_value() {
        let _g = VmGuard::new();
        await_non_promise_value();
    }

    #[test]
    fn vm_async_try_catch() {
        let _g = VmGuard::new();
        async_try_catch();
    }

    #[test]
    fn vm_async_loop() {
        let _g = VmGuard::new();
        async_loop();
    }

    #[test]
    fn vm_async_arrow() {
        let _g = VmGuard::new();
        async_arrow();
    }

    #[test]
    fn vm_unhandled_rejection_reported() {
        let _g = VmGuard::new();
        unhandled_rejection_reported();
    }

    #[test]
    fn vm_fetch_no_host_rejects() {
        let _g = VmGuard::new();
        fetch_no_host_rejects();
    }

    #[test]
    fn vm_fetch_mock_host() {
        let _g = VmGuard::new();
        fetch_mock_host();
    }

    #[test]
    fn vm_regex_test_basic() {
        let _g = VmGuard::new();
        regex_test_basic();
    }

    #[test]
    fn vm_regex_exec_groups() {
        let _g = VmGuard::new();
        regex_exec_groups();
    }

    #[test]
    fn vm_regex_char_class() {
        let _g = VmGuard::new();
        regex_char_class();
    }

    #[test]
    fn vm_regex_global_last_index() {
        let _g = VmGuard::new();
        regex_global_last_index();
    }

    #[test]
    fn vm_regex_no_redos_hang() {
        let _g = VmGuard::new();
        regex_no_redos_hang();
    }

    #[test]
    fn vm_string_match_regex() {
        let _g = VmGuard::new();
        string_match_regex();
    }

    #[test]
    fn vm_string_replace_regex() {
        let _g = VmGuard::new();
        string_replace_regex();
    }

    #[test]
    fn vm_string_split_regex() {
        let _g = VmGuard::new();
        string_split_regex();
    }

    #[test]
    fn vm_module_import_export() {
        let _g = VmGuard::new();
        module_import_export();
    }

    #[test]
    fn vm_module_missing_errors() {
        let _g = VmGuard::new();
        module_missing_errors();
    }

    #[test]
    fn vm_module_circular_errors() {
        let _g = VmGuard::new();
        module_circular_errors();
    }

    #[test]
    fn vm_strict_assign_undeclared_throws() {
        let _g = VmGuard::new();
        strict_assign_undeclared_throws();
    }

    #[test]
    fn vm_strict_compound_assign_undeclared_throws() {
        let _g = VmGuard::new();
        strict_compound_assign_undeclared_throws();
    }

    #[test]
    fn vm_strict_nested_function_inherits() {
        let _g = VmGuard::new();
        strict_nested_function_inherits();
    }

    #[test]
    fn vm_strict_function_level_directive() {
        let _g = VmGuard::new();
        strict_function_level_directive();
    }

    #[test]
    fn vm_strict_arrow_inherits() {
        let _g = VmGuard::new();
        strict_arrow_inherits();
    }

    #[test]
    fn vm_strict_with_is_syntax_error() {
        let _g = VmGuard::new();
        strict_with_is_syntax_error();
    }

    #[test]
    fn vm_strict_delete_non_configurable() {
        let _g = VmGuard::new();
        strict_delete_non_configurable();
    }

    #[test]
    fn vm_strict_delete_bare_identifier() {
        let _g = VmGuard::new();
        strict_delete_bare_identifier();
    }

    #[test]
    fn vm_strict_octal_literal() {
        let _g = VmGuard::new();
        strict_octal_literal();
    }

    #[test]
    fn vm_strict_duplicate_params() {
        let _g = VmGuard::new();
        strict_duplicate_params();
    }

    #[test]
    fn vm_strict_this_stays_undefined() {
        let _g = VmGuard::new();
        strict_this_stays_undefined();
    }

    #[test]
    fn vm_error_stack_format() {
        let _g = VmGuard::new();
        error_stack_format();
    }

    #[test]
    fn vm_error_object_props() {
        let _g = VmGuard::new();
        error_object_props();
    }

    #[test]
    fn vm_error_instanceof_chain() {
        let _g = VmGuard::new();
        error_instanceof_chain();
    }

    #[test]
    fn vm_error_subclass_classification() {
        let _g = VmGuard::new();
        error_subclass_classification();
    }

    #[test]
    fn vm_uncaught_microtask_error_carries_stack() {
        let _g = VmGuard::new();
        uncaught_microtask_error_carries_stack();
    }

    #[test]
    fn vm_rejected_promise_carries_error_object() {
        let _g = VmGuard::new();
        rejected_promise_carries_error_object();
    }

    #[test]
    fn vm_thrown_error_value_keeps_stack() {
        let _g = VmGuard::new();
        thrown_error_value_keeps_stack();
    }

    #[test]
    fn vm_debug_breakpoint_hits() {
        let _g = VmGuard::new();
        debug_breakpoint_hits();
    }

    #[test]
    fn vm_debug_disabled_is_noop() {
        let _g = VmGuard::new();
        debug_disabled_is_noop();
    }

    #[test]
    fn vm_debug_pause_inside_function() {
        let _g = VmGuard::new();
        debug_pause_inside_function();
    }

    #[test]
    fn vm_debug_step_over() {
        let _g = VmGuard::new();
        debug_step_over();
    }

    #[test]
    fn vm_p9_gen_basic_next() {
        let _g = VmGuard::new();
        p9_gen_basic_next();
    }

    #[test]
    fn vm_p9_gen_yield_value_passing() {
        let _g = VmGuard::new();
        p9_gen_yield_value_passing();
    }

    #[test]
    fn vm_p9_gen_return_early() {
        let _g = VmGuard::new();
        p9_gen_return_early();
    }

    #[test]
    fn vm_p9_gen_throw_into() {
        let _g = VmGuard::new();
        p9_gen_throw_into();
    }

    #[test]
    fn vm_p9_gen_throw_uncaught() {
        let _g = VmGuard::new();
        p9_gen_throw_uncaught();
    }

    #[test]
    fn vm_p9_gen_yield_star() {
        let _g = VmGuard::new();
        p9_gen_yield_star();
    }

    #[test]
    fn vm_p9_gen_for_of() {
        let _g = VmGuard::new();
        p9_gen_for_of();
    }

    #[test]
    fn vm_p9_gen_try_finally() {
        let _g = VmGuard::new();
        p9_gen_try_finally();
    }

    #[test]
    fn vm_p9_gen_method_shorthand() {
        let _g = VmGuard::new();
        p9_gen_method_shorthand();
    }

    #[test]
    fn vm_p9_proxy_get_trap() {
        let _g = VmGuard::new();
        p9_proxy_get_trap();
    }

    #[test]
    fn vm_p9_proxy_no_trap_passthrough() {
        let _g = VmGuard::new();
        p9_proxy_no_trap_passthrough();
    }

    #[test]
    fn vm_p9_proxy_set_trap() {
        let _g = VmGuard::new();
        p9_proxy_set_trap();
    }

    #[test]
    fn vm_p9_proxy_has_trap() {
        let _g = VmGuard::new();
        p9_proxy_has_trap();
    }

    #[test]
    fn vm_p9_proxy_apply_trap() {
        let _g = VmGuard::new();
        p9_proxy_apply_trap();
    }

    #[test]
    fn vm_p9_proxy_construct_trap() {
        let _g = VmGuard::new();
        p9_proxy_construct_trap();
    }

    #[test]
    fn vm_p9_proxy_construct_passthrough() {
        let _g = VmGuard::new();
        p9_proxy_construct_passthrough();
    }

    #[test]
    fn vm_p9_proxy_target_must_be_object() {
        let _g = VmGuard::new();
        p9_proxy_target_must_be_object();
    }

    #[test]
    fn vm_p9_class_basic() {
        let _g = VmGuard::new();
        p9_class_basic();
    }

    #[test]
    fn vm_p9_class_inheritance_super() {
        let _g = VmGuard::new();
        p9_class_inheritance_super();
    }

    #[test]
    fn vm_p9_class_default_derived_ctor() {
        let _g = VmGuard::new();
        p9_class_default_derived_ctor();
    }

    #[test]
    fn vm_p9_class_static() {
        let _g = VmGuard::new();
        p9_class_static();
    }

    #[test]
    fn vm_p9_class_static_inheritance() {
        let _g = VmGuard::new();
        p9_class_static_inheritance();
    }

    #[test]
    fn vm_p9_class_private_field() {
        let _g = VmGuard::new();
        p9_class_private_field();
    }

    #[test]
    fn vm_p9_class_private_method() {
        let _g = VmGuard::new();
        p9_class_private_method();
    }

    #[test]
    fn vm_p9_class_private_in() {
        let _g = VmGuard::new();
        p9_class_private_in();
    }

    #[test]
    fn vm_p9_class_extends_null() {
        let _g = VmGuard::new();
        p9_class_extends_null();
    }

    #[test]
    fn vm_p9_class_getter_setter() {
        let _g = VmGuard::new();
        p9_class_getter_setter();
    }

    #[test]
    fn vm_p9_class_expr() {
        let _g = VmGuard::new();
        p9_class_expr();
    }

    #[test]
    fn vm_p9_class_field_init_order() {
        let _g = VmGuard::new();
        p9_class_field_init_order();
    }

    #[test]
    fn vm_p9_super_assign_to_this() {
        let _g = VmGuard::new();
        p9_super_assign_to_this();
    }

    #[test]
    fn vm_p12_constructor_backref_weak() {
        let _g = VmGuard::new();
        p12_constructor_backref_weak();
    }

    #[test]
    fn vm_p12_class_block_scoped() {
        let _g = VmGuard::new();
        p12_class_block_scoped();
    }

    #[test]
    fn vm_p12_escaped_closure_survives_breaker() {
        let _g = VmGuard::new();
        p12_escaped_closure_survives_breaker();
    }

    #[test]
    fn vm_p12_super_via_weak_home_object() {
        let _g = VmGuard::new();
        p12_super_via_weak_home_object();
    }

    #[test]
    fn vm_p12_user_object_cycle_semantics() {
        let _g = VmGuard::new();
        p12_user_object_cycle_semantics();
    }

    #[test]
    fn vm_p12_break_scope_cycles_unit() {
        let _g = VmGuard::new();
        p12_break_scope_cycles_unit();
    }

    #[test]
    fn vm_p12_weakmap_sweep_amortized_correct() {
        let _g = VmGuard::new();
        p12_weakmap_sweep_amortized_correct();
    }

    #[test]
    fn vm_p12_reduce_index_arg_spec() {
        let _g = VmGuard::new();
        p12_reduce_index_arg_spec();
    }

    #[test]
    fn vm_p12_array_higher_order_mutation_safe() {
        let _g = VmGuard::new();
        p12_array_higher_order_mutation_safe();
    }

    #[test]
    fn vm_p10_map_basic() {
        let _g = VmGuard::new();
        p10_map_basic();
    }

    #[test]
    fn vm_p10_map_delete_clear() {
        let _g = VmGuard::new();
        p10_map_delete_clear();
    }

    #[test]
    fn vm_p10_map_nan_key() {
        let _g = VmGuard::new();
        p10_map_nan_key();
    }

    #[test]
    fn vm_p10_map_iter() {
        let _g = VmGuard::new();
        p10_map_iter();
    }

    #[test]
    fn vm_p10_map_typeof() {
        let _g = VmGuard::new();
        p10_map_typeof();
    }

    #[test]
    fn vm_p10_set_basic() {
        let _g = VmGuard::new();
        p10_set_basic();
    }

    #[test]
    fn vm_p10_set_foreach() {
        let _g = VmGuard::new();
        p10_set_foreach();
    }

    #[test]
    fn vm_p10_weakmap_basic() {
        let _g = VmGuard::new();
        p10_weakmap_basic();
    }

    #[test]
    fn vm_p10_weakmap_key_must_be_object() {
        let _g = VmGuard::new();
        p10_weakmap_key_must_be_object();
    }

    #[test]
    fn vm_p10_weakset_basic() {
        let _g = VmGuard::new();
        p10_weakset_basic();
    }

    #[test]
    fn vm_p10_arraybuffer_basic() {
        let _g = VmGuard::new();
        p10_arraybuffer_basic();
    }

    #[test]
    fn vm_p10_typedarray_all_kinds() {
        let _g = VmGuard::new();
        p10_typedarray_all_kinds();
    }

    #[test]
    fn vm_p10_typedarray_clamp() {
        let _g = VmGuard::new();
        p10_typedarray_clamp();
    }

    #[test]
    fn vm_p10_typedarray_wrap() {
        let _g = VmGuard::new();
        p10_typedarray_wrap();
    }

    #[test]
    fn vm_p10_typedarray_from_buffer() {
        let _g = VmGuard::new();
        p10_typedarray_from_buffer();
    }

    #[test]
    fn vm_p10_typedarray_methods() {
        let _g = VmGuard::new();
        p10_typedarray_methods();
    }

    #[test]
    fn vm_p10_typedarray_forof() {
        let _g = VmGuard::new();
        p10_typedarray_forof();
    }

    #[test]
    fn vm_p10_dataview_basic() {
        let _g = VmGuard::new();
        p10_dataview_basic();
    }

    #[test]
    fn vm_p10_dataview_little_endian() {
        let _g = VmGuard::new();
        p10_dataview_little_endian();
    }

    #[test]
    fn vm_p10_date_basic() {
        let _g = VmGuard::new();
        p10_date_basic();
    }

    #[test]
    fn vm_p10_date_components() {
        let _g = VmGuard::new();
        p10_date_components();
    }

    #[test]
    fn vm_p10_intl_numberformat_decimal() {
        let _g = VmGuard::new();
        p10_intl_numberformat_decimal();
    }

    #[test]
    fn vm_p10_intl_numberformat_currency_percent() {
        let _g = VmGuard::new();
        p10_intl_numberformat_currency_percent();
    }

    #[test]
    fn vm_p10_intl_numberformat_options() {
        let _g = VmGuard::new();
        p10_intl_numberformat_options();
    }

    #[test]
    fn vm_p10_intl_numberformat_parts_resolved() {
        let _g = VmGuard::new();
        p10_intl_numberformat_parts_resolved();
    }

    #[test]
    fn vm_p10_intl_datetimeformat() {
        let _g = VmGuard::new();
        p10_intl_datetimeformat();
    }

    #[test]
    fn vm_p10_intl_datetimeformat_time() {
        let _g = VmGuard::new();
        p10_intl_datetimeformat_time();
    }

    #[test]
    fn vm_p10_intl_datetimeformat_zh() {
        let _g = VmGuard::new();
        p10_intl_datetimeformat_zh();
    }

    #[test]
    fn vm_p10_string_extra() {
        let _g = VmGuard::new();
        p10_string_extra();
    }

    #[test]
    fn vm_p10_array_extra() {
        let _g = VmGuard::new();
        p10_array_extra();
    }

    #[test]
    fn vm_p10_object_extra() {
        let _g = VmGuard::new();
        p10_object_extra();
    }

    #[test]
    fn vm_p10_math_extra() {
        let _g = VmGuard::new();
        p10_math_extra();
    }

    #[test]
    fn vm_p10_number_extra() {
        let _g = VmGuard::new();
        p10_number_extra();
    }

    #[test]
    fn vm_p10_globals() {
        let _g = VmGuard::new();
        p10_globals();
    }

    #[test]
    fn vm_p10_typeof_all() {
        let _g = VmGuard::new();
        p10_typeof_all();
    }

    // ============ Phase 15：解构 ============
    #[test]
    fn p15_destructure_object() {
        assert_eq!(evd_num(r#"var {a, b} = {a: 1, b: 2}; a + b"#), 3.0);
        assert_eq!(evd_num(r#"var {a: {b}} = {a: {b: 42}}; b"#), 42.0);
        assert_eq!(evd_num(r#"var {a = 5} = {}; a"#), 5.0);
        assert_eq!(evd_num(r#"var {a, ...rest} = {a: 1, b: 2}; rest.b"#), 2.0);
    }

    #[test]
    fn p15_destructure_array() {
        assert_eq!(evd_num(r#"var [x, y] = [10, 20]; x * y"#), 200.0);
        assert_eq!(evd_num(r#"var [a, [b, c]] = [1, [2, 3]]; a + b + c"#), 6.0);
        assert_eq!(evd_num(r#"var [a = 7] = []; a"#), 7.0);
        assert_eq!(evd_num(r#"var [a, ...rest] = [1, 2, 3]; rest.length"#), 2.0);
    }

    #[test]
    fn p15_destructure_params_for_catch() {
        assert_eq!(evd_num(r#"function f({a}, [b]) { return a + b; } f({a: 1}, [2])"#), 3.0);
        assert_eq!(
            evd_num(r#"var s = 0; for (var {x} of [{x: 1}, {x: 2}]) { s += x; } s"#),
            3.0
        );
        assert_eq!(
            evd_str(r#"try { throw {message: "boom"}; } catch ({message}) { message }"#),
            "boom".to_string()
        );
    }

    #[test]
    fn p15_destructure_assign() {
        assert_eq!(evd_num(r#"var a, b; [a, b] = [3, 4]; a + b"#), 7.0);
        assert_eq!(evd_num(r#"var o = {x: 1}; ({x} = {x: 99}); o.x"#), 1.0); // o.x unchanged, x is global
    }

    // ============ Phase 15：async 生成器 ============
    #[test]
    fn p15_async_gen_basic() {
        // 基本 yield：用 console 捕获异步结果（Promise 回调赋值外层变量有旧 bug，见注）。
        let src = r#"
async function* g() {
  yield 1;
  yield 2;
  return 3;
}
var gen = g();
gen.next().then(function(r) { console.log("v:" + r.value + ",d:" + r.done); });
gen.next().then(function(r) { console.log("v:" + r.value + ",d:" + r.done); });
gen.next().then(function(r) { console.log("v:" + r.value + ",d:" + r.done); });
"#;
        let (_, console) = eval_with_console(src).unwrap();
        assert!(console.iter().any(|l| l.contains("v:1,d:false")), "got {:?}", console);
        assert!(console.iter().any(|l| l.contains("v:2,d:false")), "got {:?}", console);
        assert!(console.iter().any(|l| l.contains("v:3,d:true")), "got {:?}", console);
    }

    #[test]
    fn p15_async_gen_await() {
        // async 生成器内的 await。
        let src = r#"
async function* g() {
  var x = await Promise.resolve(41);
  yield x + 1;
}
var gen = g();
gen.next().then(function(r) { console.log("v:" + r.value); });
"#;
        let (_, console) = eval_with_console(src).unwrap();
        assert!(console.iter().any(|l| l.contains("v:42")), "got {:?}", console);
    }

    #[test]
    fn p15_async_gen_returns_promise() {
        // next() 返回 Promise。
        let src = r#"
async function* g() { yield 1; }
var gen = g();
var p = gen.next();
console.log(typeof p);
"#;
        let (_, console) = eval_with_console(src).unwrap();
        // typeof Promise is "object" in this engine
        assert!(console.iter().any(|l| l == "object"), "got {:?}", console);
    }
}
