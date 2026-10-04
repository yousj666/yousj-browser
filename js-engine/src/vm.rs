//! yousj-js · Phase 11：栈式字节码虚拟机。
//!
//! - `Vm` 持有 `&mut Interpreter`：所有复杂语义（属性、调用、`new`、
//!   二元运算、宿主方法等）回调用解释器既有方法，保证与树遍历语义一致。
//! - 每个函数调用一份 `Vm`（操作数栈、环境栈、收集器等调用期状态）。
//!   嵌套调用经 `call_value` → `call_function` → 新 `Vm`，Rust 栈自然延伸。
//! - 与解释器并存：`Interpreter::vm_enabled` 开关；`eval_source_vm` 为
//!   VM 入口。默认关闭，测试双跑验证一致后再考虑切换。

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{AssignOp, ForLeft, UpdateOp};
use crate::bytecode::{
    Chunk, ConstVal, Handler, HandlerKind, Op, CALL_OPTIONAL,
};
use crate::compiler::{CompileError, Compiler};
use crate::interpreter::Interpreter;
use crate::regex::{CompiledRegex, JsRegExp};
use crate::value::{
    syntax_err, DeclKind, Env, EnvRef, FlowError, FuncBody, FuncRef, JsArray, JsObject,
    RuntimeError, Value,
};

/// `CompileError` → `FlowError`（编译期内部错误，按运行时错误上报）。
pub fn flow_of_compile(e: CompileError) -> FlowError {
    FlowError::Runtime(RuntimeError::new(format!("phase11: {}", e.0)))
}

/// 惰性编译函数体（`call_function` 的 VM 分支调用）。
/// 函数创建时 body 已是 desugar 后形态；编译结果缓存于 `FuncRef.vm_chunk`。
pub fn get_or_compile_chunk(func: &FuncRef) -> Result<Rc<Chunk>, CompileError> {
    if let Some(c) = func.vm_chunk.borrow().clone() {
        return Ok(c);
    }
    let name = func.name.clone().unwrap_or_else(|| "<anonymous>".to_string());
    let chunk = match &func.body {
        FuncBody::Block(stmts) => Compiler::compile_body(stmts, &name)?,
        FuncBody::Expr(e) => Compiler::compile_expr_body(e, &name)?,
    };
    let rc = Rc::new(chunk);
    *func.vm_chunk.borrow_mut() = Some(rc.clone());
    Ok(rc)
}

/// VM 入口（`eval_source` 的 VM 版）：解析 → 编译 → 执行。
pub fn eval_source_vm(src: &str) -> Result<Value, crate::interpreter::JsError> {
    use crate::interpreter::JsError;
    use crate::parser::parse_source;
    let prog = parse_source(src).map_err(JsError::Parse)?;
    let mut ip = Interpreter::new();
    ip.set_vm_enabled(true);
    ip.run(&prog).map_err(crate::interpreter::flow_to_js)
}

/// 求值并取 console（VM 版）。
pub fn eval_with_console_vm(
    src: &str,
) -> Result<(Value, Vec<String>), crate::interpreter::JsError> {
    use crate::interpreter::JsError;
    use crate::parser::parse_source;
    let prog = parse_source(src).map_err(JsError::Parse)?;
    let mut ip = Interpreter::new();
    ip.set_vm_enabled(true);
    let v = ip.run(&prog).map_err(crate::interpreter::flow_to_js)?;
    Ok((v, ip.take_console()))
}

fn find_handler<'h>(handlers: &'h [Handler], cur: u32, e: &FlowError) -> Option<&'h Handler> {
    // Phase 14：catch 捕获一切可捕获错误（JS throw 值 + 引擎 Runtime 错误）；
    // 失控保护错误（uncatchable）跳过 catch，只进 finally。
    let catchable = !matches!(e, FlowError::Runtime(r) if r.uncatchable);
    handlers.iter().find(|h| {
        h.start <= cur && cur < h.end && (h.kind == HandlerKind::Finally || catchable)
    })
}

/// for-in/of 迭代器状态。
struct IterState {
    items: Vec<Value>,
    idx: usize,
}

enum Step {
    Next,
    Return(Value),
}

pub struct Vm<'a> {
    ip: &'a mut Interpreter,
    env_stack: Vec<EnvRef>,
    var_env: EnvRef,
    /// 函数入口时的环境栈长度（Return 截断到此）。
    base_depth: usize,
    ip_code: usize,
    stack: Vec<Value>,
    /// 最近一条正常完成的语句的值（函数体/顶层的求值结果）。
    last: Value,
    last_save: Vec<Value>,
    pending_ret: Option<Value>,
    pending_exc: Option<FlowError>,
    switch_disc: Value,
    arg_builders: Vec<Vec<Value>>,
    arr_builders: Vec<Vec<Value>>,
    obj_builders: Vec<crate::value::ObjectRef>,
    iters: Vec<IterState>,
}

/// 执行一个已编译的 chunk（`call_function` / `run_inner` 的 VM 分支调用）。
pub fn run_chunk(
    ip: &mut Interpreter,
    chunk: &Chunk,
    env: EnvRef,
    var_env: EnvRef,
) -> Result<Value, FlowError> {
    let mut vm = Vm {
        ip,
        env_stack: vec![env],
        var_env,
        base_depth: 1,
        ip_code: 0,
        stack: Vec::new(),
        last: Value::Undefined,
        last_save: Vec::new(),
        pending_ret: None,
        pending_exc: None,
        switch_disc: Value::Undefined,
        arg_builders: Vec::new(),
        arr_builders: Vec::new(),
        obj_builders: Vec::new(),
        iters: Vec::new(),
    };
    vm.run(chunk)
}

impl<'a> Vm<'a> {
    fn run(&mut self, chunk: &Chunk) -> Result<Value, FlowError> {
        loop {
            if self.ip_code >= chunk.code.len() {
                return Ok(std::mem::replace(&mut self.last, Value::Undefined));
            }
            let cur = self.ip_code;
            self.ip_code = cur + 1;
            // 无限循环保护（解释器 tick 的 VM 版；计数口径略有不同，但同为上限保护）。
            self.ip.tick()?;
            let op = chunk.code[cur].clone();
            match self.exec_op(chunk, &op, cur) {
                Ok(Step::Next) => {}
                Ok(Step::Return(v)) => {
                    // phase 12：函数返回时丢弃的环境逐个尝试断开自环。
                    while self.env_stack.len() > self.base_depth {
                        if let Some(popped) = self.env_stack.pop() {
                            Env::break_scope_cycles(&popped);
                        }
                    }
                    return Ok(v);
                }
                Err(e) => {
                    if let Some(h) = find_handler(&chunk.handlers, cur as u32, &e) {
                        // 进入 handler：环境/迭代器截断到 try 处；表达式级收集器丢弃
                        //（它们属于被放弃的求值）；pending 状态由 handler 指令消费。
                        // phase 12：被截断丢弃的环境同样尝试断开自环。
                        while self.env_stack.len() > h.env_depth as usize + 1 {
                            if let Some(popped) = self.env_stack.pop() {
                                Env::break_scope_cycles(&popped);
                            }
                        }
                        self.iters.truncate(h.iter_depth as usize);
                        self.arg_builders.clear();
                        self.arr_builders.clear();
                        self.obj_builders.clear();
                        match h.kind {
                            // Phase 14：Runtime 错误转为对应 Error 子类对象再压栈。
                            HandlerKind::Catch => {
                                let v = self.ip.flow_error_to_value(e);
                                self.stack.push(v);
                            }
                            HandlerKind::Finally => {
                                self.pending_exc = Some(e);
                            }
                        }
                        self.ip_code = h.target as usize;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    // -- 栈/环境辅助 --
    fn pop(&mut self) -> Value {
        self.stack.pop().expect("phase11: value stack underflow")
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    fn pop_n(&mut self, n: usize) -> Vec<Value> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.pop());
        }
        out.reverse();
        out
    }

    fn cur_env(&self) -> EnvRef {
        self.env_stack
            .last()
            .expect("phase11: env stack underflow")
            .clone()
    }

    #[allow(clippy::too_many_lines)]
    fn exec_op(&mut self, chunk: &Chunk, op: &Op, cur: usize) -> Result<Step, FlowError> {
        match op {
            // ===== 常量与栈 =====
            Op::Const(ci) => {
                let v = match &chunk.consts[*ci as usize] {
                    ConstVal::Number(n) => Value::Number(*n),
                    ConstVal::Str(s) => Value::String(s.clone()),
                    ConstVal::Bool(b) => Value::Bool(*b),
                    ConstVal::Null => Value::Null,
                    ConstVal::Undefined => Value::Undefined,
                    ConstVal::Regex { pattern, flags } => {
                        match CompiledRegex::compile(pattern, flags) {
                            Ok(c) => Value::RegExp(JsRegExp::new(c)),
                            Err(e) => {
                                return Err(syntax_err(format!("invalid regex: {e}")));
                            }
                        }
                    }
                };
                self.push(v);
            }
            Op::Undefined => self.push(Value::Undefined),
            Op::Null => self.push(Value::Null),
            Op::True => self.push(Value::Bool(true)),
            Op::False => self.push(Value::Bool(false)),
            Op::Pop => {
                self.pop();
            }
            Op::Dup => {
                let v = self.stack.last().expect("phase11: dup on empty").clone();
                self.push(v);
            }

            // ===== 变量 =====
            Op::GetName(ni) => {
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                let v = self.ip.lookup_name(&env, &name)?;
                self.push(v);
            }
            Op::SetName(ni) => {
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                let v = self.stack.last().expect("phase11: setname empty").clone();
                self.ip.assign_name(&env, &name, v)?;
            }
            Op::ForceSetVar(ni) => {
                let v = self.pop();
                let var_env = self.var_env.clone();
                let name = chunk.names[*ni as usize].clone();
                Env::assign_force(&var_env, &name, v);
            }
            Op::DeclLex(ni) => {
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                if !Env::is_declared_here(&env, &name) {
                    Env::declare_lexical(&env, &name, DeclKind::Let).map_err(FlowError::from)?;
                }
            }
            Op::DeclConst(ni) => {
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                if !Env::is_declared_here(&env, &name) {
                    Env::declare_lexical(&env, &name, DeclKind::Const)
                        .map_err(FlowError::from)?;
                }
            }
            Op::InitLex(ni) => {
                let v = self.pop();
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                Env::init_lexical(&env, &name, v).map_err(FlowError::from)?;
            }
            Op::MissingConstInit => {
                return Err(syntax_err("missing initializer in const declaration"));
            }
            Op::CompoundName(ni, aop) => {
                let rhs = self.pop();
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                let cur_v = self.ip.lookup_name(&env, &name)?;
                let new_v = apply_assign_op(self.ip, *aop, cur_v, rhs)?;
                self.ip.assign_name(&env, &name, new_v.clone())?;
                self.push(new_v);
            }
            Op::UpdateName(ni, uop, prefix) => {
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                let cur_v = self.ip.lookup_name(&env, &name)?;
                let n = cur_v.to_number();
                let new_v = Value::Number(match uop {
                    UpdateOp::Inc => n + 1.0,
                    UpdateOp::Dec => n - 1.0,
                });
                self.ip.assign_name(&env, &name, new_v.clone())?;
                self.push(if *prefix { new_v } else { cur_v });
            }

            // ===== this =====
            Op::GetThis => {
                let v = self.ip.get_this();
                self.push(v);
            }

            // ===== 属性 =====
            Op::GetProp => {
                let key = self.pop().to_js_string();
                let obj = self.pop();
                let v = self.ip.get_prop(&obj, &key)?;
                self.push(v);
            }
            Op::GetPropName(ni) => {
                let obj = self.pop();
                let key = chunk.names[*ni as usize].clone();
                let v = self.ip.get_prop(&obj, &key)?;
                self.push(v);
            }
            Op::SetProp => {
                let val = self.pop();
                let key = self.pop().to_js_string();
                let obj = self.pop();
                self.ip.set_prop(&obj, &key, val.clone())?;
                self.push(val);
            }
            Op::SetPropName(ni) => {
                let val = self.pop();
                let obj = self.pop();
                let key = chunk.names[*ni as usize].clone();
                self.ip.set_prop(&obj, &key, val.clone())?;
                self.push(val);
            }
            Op::CompoundProp(aop) => {
                let rhs = self.pop();
                let key = self.pop().to_js_string();
                let obj = self.pop();
                let cur_v = self.ip.get_prop(&obj, &key)?;
                let new_v = apply_assign_op(self.ip, *aop, cur_v, rhs)?;
                self.ip.set_prop(&obj, &key, new_v.clone())?;
                self.push(new_v);
            }
            Op::CompoundPropName(ni, aop) => {
                let rhs = self.pop();
                let obj = self.pop();
                let key = chunk.names[*ni as usize].clone();
                let cur_v = self.ip.get_prop(&obj, &key)?;
                let new_v = apply_assign_op(self.ip, *aop, cur_v, rhs)?;
                self.ip.set_prop(&obj, &key, new_v.clone())?;
                self.push(new_v);
            }
            Op::UpdateProp(uop, prefix) => {
                let key = self.pop().to_js_string();
                let obj = self.pop();
                let cur_v = self.ip.get_prop(&obj, &key)?;
                let new_v = apply_update(self.ip, *uop, &cur_v)?;
                self.ip.set_prop(&obj, &key, new_v.clone())?;
                self.push(if *prefix { new_v } else { cur_v });
            }
            Op::UpdatePropName(ni, uop, prefix) => {
                let obj = self.pop();
                let key = chunk.names[*ni as usize].clone();
                let cur_v = self.ip.get_prop(&obj, &key)?;
                let new_v = apply_update(self.ip, *uop, &cur_v)?;
                self.ip.set_prop(&obj, &key, new_v.clone())?;
                self.push(if *prefix { new_v } else { cur_v });
            }
            Op::CompoundPrivate(ni, aop) => {
                let rhs = self.pop();
                let obj = self.pop();
                let name = chunk.names[*ni as usize].clone();
                let cur_v = self.ip.get_private(&obj, &name)?;
                let new_v = apply_assign_op(self.ip, *aop, cur_v, rhs)?;
                self.ip.set_private(&obj, &name, new_v.clone())?;
                self.push(new_v);
            }
            Op::UpdatePrivate(ni, uop, prefix) => {
                let obj = self.pop();
                let name = chunk.names[*ni as usize].clone();
                let cur_v = self.ip.get_private(&obj, &name)?;
                let new_v = apply_update(self.ip, *uop, &cur_v)?;
                self.ip.set_private(&obj, &name, new_v.clone())?;
                self.push(if *prefix { new_v } else { cur_v });
            }
            Op::DeleteProp => {
                let key = self.pop().to_js_string();
                let obj = self.pop();
                let v = self.ip.delete_prop_value(obj, key)?;
                self.push(v);
            }
            Op::DeleteIdent(ni) => {
                let name = chunk.names[*ni as usize].clone();
                if self.ip.is_strict() {
                    return Err(syntax_err(format!(
                        "delete of an unqualified identifier '{name}' in strict mode"
                    )));
                }
                let env = self.cur_env();
                // 解释器：先求值（未声明 → ReferenceError），再返回 true。
                let _ = self.ip.lookup_name(&env, &name)?;
                self.push(Value::Bool(true));
            }
            Op::GetPrivate(ni) => {
                let obj = self.pop();
                let name = chunk.names[*ni as usize].clone();
                let v = self.ip.get_private(&obj, &name)?;
                self.push(v);
            }
            Op::SetPrivate(ni) => {
                let val = self.pop();
                let obj = self.pop();
                let name = chunk.names[*ni as usize].clone();
                self.ip.set_private(&obj, &name, val.clone())?;
                self.push(val);
            }
            Op::PrivateIn(ni) => {
                let obj = self.pop();
                let name = chunk.names[*ni as usize].clone();
                let v = self.ip.private_in(&name, &obj)?;
                self.push(v);
            }

            // ===== 一元 / 二元 =====
            Op::Unary(uop) => {
                let v = self.pop();
                let r = match uop {
                    crate::ast::UnaryOp::Neg => Value::Number(-v.to_number()),
                    crate::ast::UnaryOp::Pos => Value::Number(v.to_number()),
                    crate::ast::UnaryOp::Not => Value::Bool(!v.to_boolean()),
                    crate::ast::UnaryOp::BitNot => Value::Number(!(v.to_int32()) as f64),
                    _ => {
                        return Err(FlowError::Runtime(RuntimeError::new(
                            "phase11: unexpected unary op",
                        )))
                    }
                };
                self.push(r);
            }
            Op::Typeof => {
                let v = self.pop();
                self.push(Value::String(v.type_of().to_string()));
            }
            Op::TypeofName(ni) => {
                let env = self.cur_env();
                let name = chunk.names[*ni as usize].clone();
                let t = match Env::lookup(&env, &name).map_err(FlowError::from)? {
                    Some(v) => v.type_of().to_string(),
                    None => "undefined".to_string(),
                };
                self.push(Value::String(t));
            }
            Op::Binary(bop) => {
                let r = self.pop();
                let l = self.pop();
                let v = self.ip.apply_binary(*bop, l, r)?;
                self.push(v);
            }
            Op::StrictEq => {
                let r = self.pop();
                let l = self.pop();
                self.push(Value::Bool(l.strict_eq(&r)));
            }

            // ===== 跳转 =====
            Op::Jump(t) => {
                self.ip_code = *t as usize;
            }
            Op::JumpIfFalse(t) => {
                let v = self.pop();
                if !v.to_boolean() {
                    self.ip_code = *t as usize;
                }
            }
            Op::JumpIfTrue(t) => {
                let v = self.pop();
                if v.to_boolean() {
                    self.ip_code = *t as usize;
                }
            }
            Op::JumpIfNullish(t) => {
                let is_nullish = self
                    .stack
                    .last()
                    .map(|v| v.is_nullish())
                    .unwrap_or(false);
                if is_nullish {
                    self.ip_code = *t as usize;
                }
            }

            // ===== 调用 =====
            Op::Call(nargs, flags) => {
                let args = self.pop_n(*nargs as usize);
                let callee = self.pop();
                if flags & CALL_OPTIONAL != 0 && callee.is_nullish() {
                    self.push(Value::Undefined);
                } else {
                    let span = chunk.spans[cur].clone();
                    let v =
                        self.ip
                            .call_value_at(callee, Value::Undefined, args, Some(span))?;
                    self.push(v);
                }
            }
            Op::CallPropName(nargs, ni, flags) => {
                let args = self.pop_n(*nargs as usize);
                let obj = self.pop();
                let key = chunk.names[*ni as usize].clone();
                let span = chunk.spans[cur].clone();
                let v = self.call_prop(obj, key, args, *flags, span)?;
                self.push(v);
            }
            Op::CallPropDyn(nargs, flags) => {
                let args = self.pop_n(*nargs as usize);
                let key = self.pop().to_js_string();
                let obj = self.pop();
                let span = chunk.spans[cur].clone();
                let v = self.call_prop(obj, key, args, *flags, span)?;
                self.push(v);
            }
            Op::SuperCall(nargs) => {
                let args = self.pop_n(*nargs as usize);
                let v = self.ip.eval_super_call(args)?;
                self.push(v);
            }
            Op::SuperMethodName(nargs, ni) => {
                let args = self.pop_n(*nargs as usize);
                let key = chunk.names[*ni as usize].clone();
                let span = chunk.spans[cur].clone();
                let v = self.super_method_call(key, args, span)?;
                self.push(v);
            }
            Op::SuperMethodDyn(nargs) => {
                let args = self.pop_n(*nargs as usize);
                let key = self.pop().to_js_string();
                let span = chunk.spans[cur].clone();
                let v = self.super_method_call(key, args, span)?;
                self.push(v);
            }
            Op::SuperPropName(ni) => {
                let key = chunk.names[*ni as usize].clone();
                let v = self.ip.get_super_prop(&key)?;
                self.push(v);
            }
            Op::SuperPropDyn => {
                let key = self.pop().to_js_string();
                let v = self.ip.get_super_prop(&key)?;
                self.push(v);
            }
            Op::New(nargs) => {
                let args = self.pop_n(*nargs as usize);
                let callee = self.pop();
                let v = self.ip.eval_new_vals(callee, args)?;
                self.push(v);
            }

            // ===== 数组 / 对象 =====
            Op::ArrNew => self.arr_builders.push(Vec::new()),
            Op::ArrPush => {
                let v = self.pop();
                self.arr_builders
                    .last_mut()
                    .expect("phase11: arr builder underflow")
                    .push(v);
            }
            Op::ArrHole => {
                self.arr_builders
                    .last_mut()
                    .expect("phase11: arr builder underflow")
                    .push(Value::Undefined);
            }
            Op::ArrSpread => {
                let v = self.pop();
                let mut vs = self.ip.spread_into_vec(&v)?;
                self.arr_builders
                    .last_mut()
                    .expect("phase11: arr builder underflow")
                    .append(&mut vs);
            }
            Op::ArrDone => {
                let elems = self.arr_builders.pop().expect("phase11: arr done underflow");
                let arr = JsArray::with_proto(elems, Some(self.ip.protos.array.clone()));
                self.push(Value::Array(Rc::new(RefCell::new(arr))));
            }
            Op::ObjNew => {
                let mut o = JsObject::with_proto(Some(self.ip.protos.object.clone()));
                o.tag = Some("Object".to_string());
                self.obj_builders.push(Rc::new(RefCell::new(o)));
            }
            Op::ObjSet => {
                let val = self.pop();
                let key = self.pop().to_js_string();
                self.obj_builders
                    .last()
                    .expect("phase11: obj builder underflow")
                    .borrow_mut()
                    .set(&key, val);
            }
            Op::ObjSetName(ni) => {
                let val = self.pop();
                let key = chunk.names[*ni as usize].clone();
                self.obj_builders
                    .last()
                    .expect("phase11: obj builder underflow")
                    .borrow_mut()
                    .set(&key, val);
            }
            Op::ObjDone => {
                let o = self.obj_builders.pop().expect("phase11: obj done underflow");
                self.push(Value::Object(o));
            }
            Op::MakeFunction(fi) => {
                let env = self.cur_env();
                let meta = chunk.funcs[*fi as usize].clone();
                let v = self.ip.make_function_value(&meta, None, env)?;
                self.push(v);
            }
            Op::MakeMethod(fi) => {
                let env = self.cur_env();
                let name = self
                    .stack
                    .last()
                    .expect("phase11: method key missing")
                    .to_js_string();
                let meta = chunk.funcs[*fi as usize].clone();
                let v = self.ip.make_function_value(&meta, Some(&name), env)?;
                self.push(v);
            }
            Op::MakeClass(ci) => {
                let env = self.cur_env();
                let span = chunk.spans[cur].clone();
                let node = chunk.classes[*ci as usize].clone();
                let v = self.ip.eval_class(&node, None, env, span)?;
                self.push(v);
            }

            // ===== 作用域 =====
            Op::EnterBlock(si) => {
                let unit = chunk.scopes[*si as usize].clone();
                let child = Env::child(&self.cur_env());
                let var_env = self.var_env.clone();
                self.ip.hoist(&unit.hoist_stmts, &var_env)?;
                self.ip.declare_lexicals(&unit.lex_stmts, &child)?;
                self.env_stack.push(child);
            }
            Op::HoistLex(si) => {
                let unit = chunk.scopes[*si as usize].clone();
                let env = self.cur_env();
                let var_env = self.var_env.clone();
                self.ip.hoist(&unit.hoist_stmts, &var_env)?;
                self.ip.declare_lexicals(&unit.lex_stmts, &env)?;
            }
            Op::PushEnv => {
                let child = Env::child(&self.cur_env());
                self.env_stack.push(child);
            }
            Op::PopEnv => {
                // phase 12：块作用域退出——尝试断开自环（pop 出的值是
                // 解释器持有的唯一句柄，计数精确）。
                if let Some(popped) = self.env_stack.pop() {
                    Env::break_scope_cycles(&popped);
                }
            }

            // ===== 控制流 =====
            Op::Return => {
                let v = self.pop();
                return Ok(Step::Return(v));
            }
            Op::StashRet => {
                self.pending_ret = Some(self.pop());
            }
            Op::UnstashRet => {
                let v = self
                    .pending_ret
                    .take()
                    .expect("phase11: unstash without stash");
                self.push(v);
            }
            Op::Throw => {
                let v = self.pop();
                return Err(FlowError::Thrown(v));
            }
            Op::Rethrow => {
                let e = self
                    .pending_exc
                    .take()
                    .expect("phase11: rethrow without pending");
                return Err(e);
            }
            Op::SaveLast => {
                let old = std::mem::replace(&mut self.last, Value::Undefined);
                self.last_save.push(old);
            }
            Op::RestoreLast => {
                self.last = self
                    .last_save
                    .pop()
                    .expect("phase11: restore without save");
            }
            Op::SetLast => {
                self.last = self.pop();
            }
            Op::BadReturn => {
                return Err(syntax_err("return outside of function"));
            }
            Op::BadBreak => {
                return Err(syntax_err("break outside of loop"));
            }
            Op::BadContinue => {
                return Err(syntax_err("continue outside of loop"));
            }
            Op::BadTarget => {
                return Err(syntax_err("invalid assignment target"));
            }
            Op::BadSuper => {
                return Err(FlowError::Runtime(RuntimeError::new(
                    "internal: super reached the evaluator",
                )));
            }
            Op::BadAwait => {
                return Err(FlowError::Runtime(RuntimeError::new(
                    "internal: await reached the evaluator (async desugar missed it)",
                )));
            }
            Op::BadYield => {
                return Err(FlowError::Runtime(RuntimeError::new(
                    "internal: yield reached the evaluator (generator desugar missed it)",
                )));
            }
            Op::BadPrivateName => {
                return Err(FlowError::Runtime(RuntimeError::new(
                    "internal: private name reached the evaluator outside 'in'",
                )));
            }
            // ===== for-in/of =====
            Op::IterBegin(is_of) => {
                let v = self.pop();
                let items = self.ip.for_iter_items(v, *is_of == 1)?;
                self.iters.push(IterState { items, idx: 0 });
            }
            Op::IterNext(t) => {
                let (exhausted, item) = {
                    let it = self.iters.last_mut().expect("phase11: iter underflow");
                    if it.idx >= it.items.len() {
                        (true, None)
                    } else {
                        let v = it.items[it.idx].clone();
                        it.idx += 1;
                        (false, Some(v))
                    }
                };
                if exhausted {
                    self.iters.pop();
                    self.ip_code = *t as usize;
                } else {
                    self.push(item.unwrap());
                }
            }
            Op::SetForLeft(fli) => {
                let v = self.pop();
                let env = self.cur_env();
                match &chunk.for_lefts[*fli as usize] {
                    ForLeft::Expr(e) => {
                        let t = self.ip.eval_target(e, env)?;
                        self.ip.set_target(&t, v)?;
                    }
                    _ => {
                        return Err(FlowError::Runtime(RuntimeError::new(
                            "phase11: SetForLeft on non-expr",
                        )))
                    }
                }
            }

            // ===== switch =====
            Op::SetSwitchDisc => {
                self.switch_disc = self.pop();
            }
            Op::GetSwitchDisc => {
                self.push(self.switch_disc.clone());
            }

            // ===== 调试器 =====
            Op::DebugPoint => {
                if self.ip.debug_is_enabled() {
                    let env = self.cur_env();
                    let span = chunk.spans[cur].clone();
                    self.ip.debug_hook(&span, &env);
                }
            }
        }
        Ok(Step::Next)
    }

    /// 成员调用 `obj.key(args)`：宿主方法拦截 → get_prop → call_value_at。
    /// 与 `eval_call` 的 Member 分支一致。
    fn call_prop(
        &mut self,
        obj: Value,
        key: String,
        args: Vec<Value>,
        flags: u32,
        span: crate::ast::Span,
    ) -> Result<Value, FlowError> {
        if let Some(v) = self.ip.try_host_method_vals(&obj, &key, args.clone())? {
            return Ok(v);
        }
        let callee = self.ip.get_prop(&obj, &key)?;
        if flags & CALL_OPTIONAL != 0 && callee.is_nullish() {
            return Ok(Value::Undefined);
        }
        self.ip.call_value_at(callee, obj, args, Some(span))
    }

    /// `super.key(args)`：get_super_prop → this 绑定当前 this → 调用。
    fn super_method_call(
        &mut self,
        key: String,
        args: Vec<Value>,
        span: crate::ast::Span,
    ) -> Result<Value, FlowError> {
        let method = self.ip.get_super_prop(&key)?;
        let this_val = self.ip.get_this();
        if let Some(v) = self
            .ip
            .try_host_method_vals(&this_val, &key, args.clone())?
        {
            return Ok(v);
        }
        self.ip.call_value_at(method, this_val, args, Some(span))
    }
}

/// 复合赋值（含 &&=/||=/??=；rhs 已求值——与解释器一致）。
fn apply_assign_op(
    ip: &mut Interpreter,
    op: AssignOp,
    cur: Value,
    rhs: Value,
) -> Result<Value, FlowError> {
    match op {
        AssignOp::Assign => Ok(rhs),
        // `a &&= b` ≡ `a && (a = b)`：条件赋值（解释器先求 rhs，此处 rhs 已传入）。
        AssignOp::AndAssign => Ok(if cur.to_boolean() { rhs } else { cur }),
        AssignOp::OrAssign => Ok(if cur.to_boolean() { cur } else { rhs }),
        AssignOp::NullishAssign => Ok(if cur.is_nullish() { rhs } else { cur }),
        _ => ip.apply_compound(op, cur, rhs),
    }
}

/// `++` / `--` 的新值计算（set 由调用方完成）。
fn apply_update(
    _ip: &mut Interpreter,
    op: UpdateOp,
    cur: &Value,
) -> Result<Value, FlowError> {
    let n = cur.to_number();
    Ok(Value::Number(match op {
        UpdateOp::Inc => n + 1.0,
        UpdateOp::Dec => n - 1.0,
    }))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ev_vm(src: &str) -> Value {
        eval_source_vm(src).unwrap_or_else(|e| panic!("vm eval failed for {:?}: {}", src, e))
    }

    fn ev_vm_num(src: &str) -> f64 {
        match ev_vm(src) {
            Value::Number(n) => n,
            v => panic!("expected number, got {:?}", v),
        }
    }

    fn ev_vm_bool(src: &str) -> bool {
        match ev_vm(src) {
            Value::Bool(b) => b,
            v => panic!("expected bool, got {:?}", v),
        }
    }

    fn ev_vm_str(src: &str) -> String {
        match ev_vm(src) {
            Value::String(s) => s,
            v => panic!("expected string, got {:?}", v),
        }
    }

    #[test]
    fn vm_smoke_arithmetic() {
        assert_eq!(ev_vm_num("1 + 2 * 3"), 7.0);
        assert_eq!(ev_vm_num("(1 + 2) * 3"), 9.0);
        assert_eq!(ev_vm_num("10 % 3"), 1.0);
        assert_eq!(ev_vm_num("2 ** 10"), 1024.0);
    }

    #[test]
    fn vm_variables_and_scope() {
        assert_eq!(ev_vm_num("var x = 5; x * 2"), 10.0);
        assert_eq!(ev_vm_num("let a = 1; { let a = 2; } a"), 1.0);
        assert_eq!(
            ev_vm_str("var fns = []; for (var i = 0; i < 3; i++) { fns.push(i); } fns.join(',')"),
            "0,1,2"
        );
    }

    #[test]
    fn vm_functions_and_closures() {
        assert_eq!(
            ev_vm_num("function add(a, b) { return a + b; } add(3, 4)"),
            7.0
        );
        assert_eq!(
            ev_vm_num(
                "function outer() { var x = 10; return function() { return x + 1; }; } outer()()"
            ),
            11.0
        );
        assert_eq!(ev_vm_num("(x => x * 2)(21)"), 42.0);
    }

    #[test]
    fn vm_control_flow() {
        assert_eq!(ev_vm_num("var s = 0; for (var i = 1; i <= 10; i++) { s += i; } s"), 55.0);
        assert_eq!(ev_vm_num("var i = 0; while (i < 5) { i++; } i"), 5.0);
        assert_eq!(
            ev_vm_str("var s = ''; for (var i = 0; i < 5; i++) { if (i == 2) continue; if (i == 4) break; s += i; } s"),
            "013"
        );
    }

    #[test]
    fn vm_try_catch_finally() {
        assert_eq!(
            ev_vm_num("var x = 0; try { throw 1; } catch (e) { x = e; } finally { x += 10; } x"),
            11.0
        );
        assert_eq!(
            ev_vm_num("function f() { try { return 1; } finally { return 2; } } f()"),
            2.0
        );
    }

    #[test]
    fn vm_switch_forinof() {
        assert_eq!(
            ev_vm_num("var x = 2; var y = 0; switch (x) { case 1: y = 1; break; case 2: y = 20; break; default: y = 99; } y"),
            20.0
        );
        assert_eq!(
            ev_vm_num("var s = 0; for (var k of [1, 2, 3]) { s += k; } s"),
            6.0
        );
    }

    #[test]
    fn vm_objects_arrays() {
        assert_eq!(ev_vm_num("var o = {a: 1, b: 2}; o.a + o.b"), 3.0);
        assert_eq!(ev_vm_num("var a = [1, 2, ...[3, 4]]; a.length"), 4.0);
        assert_eq!(ev_vm_str("var o = {m() { return 'hi'; }}; o.m()"), "hi");
    }

    #[test]
    fn vm_classes() {
        assert_eq!(
            ev_vm_num(
                "class A { constructor(x) { this.x = x; } get() { return this.x * 2; } } new A(21).get()"
            ),
            42.0
        );
        assert_eq!(ev_vm_bool("class A {} class B extends A {} new B() instanceof A"), true);
    }

    #[test]
    fn vm_generators_proxy() {
        assert_eq!(
            ev_vm_num(
                "function* g() { yield 1; yield 2; } var r = []; for (var x of g()) { r.push(x); } r.length"
            ),
            2.0
        );
    }

    #[test]
    fn vm_disassemble_smoke() {
        let prog = crate::parser::parse_source("var x = 1 + 2;").unwrap();
        let chunk = Compiler::compile_program(&prog).unwrap();
        let d = chunk.disassemble();
        assert!(d.contains("Const"));
    }
}
