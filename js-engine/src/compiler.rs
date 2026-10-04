//! yousj-js · Phase 11：AST → 字节码编译器。
//!
//! - 纯编译器：不依赖解释器状态（`&mut Interpreter`），只读 AST。
//! - 语义锚点：每条指令的运行时行为都对应解释器既有方法的调用，
//!   此处只负责求值顺序、控制流（跳转回填）与作用域进出。
//! - 函数体在解释器 `call_function` 中首次调用时惰性编译并缓存
//!   （`get_or_compile_chunk`），此时函数体已是 desugar 后的形态
//!   （async/生成器状态机），编译器无需处理 yield/await。
//! - `try/finally`：catch 走异常表；finally 在每个 abrupt 出口处内联，
//!   并在异常表里另有一份以 `Rethrow` 结尾的副本。

use std::rc::Rc;

use crate::ast::{
    AssignOp, BinaryOp, Expr, ExprKind, ForInit, ForLeft, Literal, LogicalOp, Program, Span,
    Stmt, StmtKind, UnaryOp, UpdateOp, VarKind,
};
use crate::bytecode::{
    Chunk, ConstVal, FuncKind, FuncMeta, Handler, HandlerKind, Op, ScopeUnit, CALL_OPTIONAL,
};
use crate::value::number_to_js_string;

#[derive(Debug, Clone)]
pub struct CompileError(pub String);

impl CompileError {
    fn new(msg: impl Into<String>) -> Self {
        CompileError(msg.into())
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compile error: {}", self.0)
    }
}
impl std::error::Error for CompileError {}

/// break 作用域（循环与 switch 共用栈；continue 另起一栈）。
struct BreakScope {
    patches: Vec<usize>,
    env_depth: u32,
    finally_depth: usize,
}

struct ContinueScope {
    /// 待回填的 continue 跳转（目标在 body 之后才确定）。
    patches: Vec<usize>,
    env_depth: u32,
    finally_depth: usize,
}

/// 待内联的 finally 块（try 编译期收集）。
struct FinallyInfo {
    scope_idx: u32,
    stmts: Vec<Stmt>,
}

pub struct Compiler {
    chunk: Chunk,
    break_stack: Vec<BreakScope>,
    continue_stack: Vec<ContinueScope>,
    finally_stack: Vec<Rc<FinallyInfo>>,
    /// 当前静态环境深度（0 = 函数/顶层入口环境；VM 的 env_stack.len() = depth+1）。
    env_depth: u32,
    /// 当前迭代器栈深度（for-in/of 嵌套）。
    iter_depth: u32,
    top_level: bool,
    cur_span: Span,
}

impl Compiler {
    fn new(name: &str, top_level: bool) -> Self {
        Compiler {
            chunk: Chunk::new(name, top_level),
            break_stack: Vec::new(),
            continue_stack: Vec::new(),
            finally_stack: Vec::new(),
            env_depth: 0,
            iter_depth: 0,
            top_level,
            cur_span: Span::point(0, 0),
        }
    }

    /// 编译顶层程序（`run_inner` 的 VM 分支用）。
    pub fn compile_program(prog: &Program) -> Result<Chunk, CompileError> {
        let mut c = Compiler::new("<main>", true);
        // 对应 `exec_block` 入口：hoist + declare_lexicals（不 push env）。
        let scope = c.add_scope(prog.body.clone());
        c.cur_span = prog.span;
        c.emit(Op::HoistLex(scope));
        for s in &prog.body {
            c.compile_stmt(s)?;
        }
        Ok(c.chunk)
    }

    /// 编译函数体（`call_function` 的 VM 分支用；body 已是 desugar 后形态）。
    pub fn compile_body(stmts: &[Stmt], name: &str) -> Result<Chunk, CompileError> {
        let mut c = Compiler::new(name, false);
        let scope = c.add_scope(stmts.to_vec());
        c.emit(Op::HoistLex(scope));
        for s in stmts {
            c.compile_stmt(s)?;
        }
        Ok(c.chunk)
    }

    /// 编译箭头函数表达式体（`x => x + 1`）。
    pub(crate) fn compile_expr_body(e: &Expr, name: &str) -> Result<Chunk, CompileError> {
        let mut c = Compiler::new(name, false);
        c.compile_expr(e)?;
        c.emit(Op::Return);
        Ok(c.chunk)
    }

    // ------------------------------------------------------------------
    // 发射辅助
    // ------------------------------------------------------------------

    fn emit(&mut self, op: Op) {
        let span = self.cur_span;
        self.chunk.code.push(op);
        self.chunk.spans.push(span);
    }

    fn here(&self) -> u32 {
        self.chunk.code.len() as u32
    }

    /// 发射一条跳转指令，返回其下标供回填。
    fn emit_jump(&mut self, op: Op) -> usize {
        let idx = self.chunk.code.len();
        self.emit(op);
        idx
    }

    fn patch(&mut self, idx: usize, target: u32) {
        match &mut self.chunk.code[idx] {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfNullish(t)
            | Op::IterNext(t) => *t = target,
            other => panic!("phase11: patch of non-jump op {:?}", other),
        }
    }

    fn add_const(&mut self, cv: ConstVal) -> u32 {
        // 简单去重（字符串/数字常量复用）。
        for (i, c) in self.chunk.consts.iter().enumerate() {
            let same = match (c, &cv) {
                (ConstVal::Number(a), ConstVal::Number(b)) => a == b,
                (ConstVal::Str(a), ConstVal::Str(b)) => a == b,
                (ConstVal::Bool(a), ConstVal::Bool(b)) => a == b,
                (ConstVal::Null, ConstVal::Null)
                | (ConstVal::Undefined, ConstVal::Undefined) => true,
                _ => false,
            };
            if same {
                return i as u32;
            }
        }
        self.chunk.consts.push(cv);
        self.chunk.consts.len() as u32 - 1
    }

    fn add_name(&mut self, name: String) -> u32 {
        if let Some(i) = self.chunk.names.iter().position(|n| n == &name) {
            return i as u32;
        }
        self.chunk.names.push(name);
        self.chunk.names.len() as u32 - 1
    }

    fn add_func(&mut self, meta: FuncMeta) -> u32 {
        self.chunk.funcs.push(meta);
        self.chunk.funcs.len() as u32 - 1
    }

    fn add_class(&mut self, node: crate::ast::ClassNode) -> u32 {
        self.chunk.classes.push(node);
        self.chunk.classes.len() as u32 - 1
    }

    fn add_scope(&mut self, stmts: Vec<Stmt>) -> u32 {
        self.chunk.scopes.push(ScopeUnit {
            hoist_stmts: stmts.clone(),
            lex_stmts: stmts,
        });
        self.chunk.scopes.len() as u32 - 1
    }

    fn add_for_left(&mut self, left: ForLeft) -> u32 {
        self.chunk.for_lefts.push(left);
        self.chunk.for_lefts.len() as u32 - 1
    }

    /// 非计算属性键的静态求值（与解释器 `member_key` 的非计算分支一致）。
    fn member_key_static(&mut self, prop: &Expr) -> Result<String, CompileError> {
        match &prop.node {
            ExprKind::Ident(n) => Ok(n.clone()),
            ExprKind::Literal(Literal::String(s)) => Ok(s.clone()),
            ExprKind::Literal(Literal::Number(n)) => Ok(number_to_js_string(*n)),
            _ => Err(CompileError::new("internal: invalid static property name")),
        }
    }

    /// 成员键求值：计算属性求值后压栈；静态键压字符串常量。
    fn compile_member_key(&mut self, prop: &Expr, computed: bool) -> Result<(), CompileError> {
        if computed {
            self.compile_expr(prop)?;
        } else {
            let key = self.member_key_static(prop)?;
            let ci = self.add_const(ConstVal::Str(key));
            self.emit(Op::Const(ci));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 语句
    // ------------------------------------------------------------------

    fn compile_stmt(&mut self, s: &Stmt) -> Result<(), CompileError> {
        let saved = std::mem::replace(&mut self.cur_span, s.span);
        let r = self.compile_stmt_inner(s);
        self.cur_span = saved;
        r
    }

    fn compile_stmt_inner(&mut self, s: &Stmt) -> Result<(), CompileError> {
        // 调试器钩子（关闭时只是一次标志检查）。
        self.emit(Op::DebugPoint);
        match &s.node {
            StmtKind::Empty | StmtKind::Debugger => {
                self.emit(Op::Undefined);
                self.emit(Op::SetLast);
            }
            StmtKind::Expr(e) => {
                self.compile_expr(e)?;
                self.emit(Op::SetLast);
            }
            StmtKind::Import { .. } | StmtKind::ExportNames(_) => {
                // import 已由 process_imports 处理；export 列表无运行时动作。
                self.emit(Op::Undefined);
                self.emit(Op::SetLast);
            }
            StmtKind::VarDecl { kind, decls } | StmtKind::ExportDecl { kind, decls } => {
                self.compile_var_decl(kind, decls)?;
            }
            StmtKind::FunctionDecl(f) | StmtKind::ExportFunc(f) => {
                let fi = self.add_func(FuncMeta {
                    kind: FuncKind::Decl(f.as_ref().clone()),
                    span: s.span,
                });
                self.emit(Op::MakeFunction(fi));
                let name = f.id.clone().unwrap_or_default();
                let ni = self.add_name(name);
                self.emit(Op::ForceSetVar(ni));
                self.emit(Op::Undefined);
                self.emit(Op::SetLast);
            }
            StmtKind::ClassDecl(c) => {
                let ci = self.add_class(c.as_ref().clone());
                self.emit(Op::MakeClass(ci));
                let name = c.id.clone().unwrap_or_default();
                let ni = self.add_name(name);
                // phase 12：类声明是块级作用域（let-like），绑定在当前环境
                // （与树遍历一致；也使类环可在块退出时断开）。
                self.emit(Op::DeclLex(ni));
                self.emit(Op::InitLex(ni));
                self.emit(Op::Undefined);
                self.emit(Op::SetLast);
            }
            StmtKind::Block(b) => {
                if b.is_empty() {
                    self.emit(Op::Undefined);
                    self.emit(Op::SetLast);
                    return Ok(());
                }
                let scope = self.add_scope(b.clone());
                self.emit(Op::EnterBlock(scope));
                self.env_depth += 1;
                for st in b {
                    self.compile_stmt(st)?;
                }
                self.emit(Op::PopEnv);
                self.env_depth -= 1;
                // 值：内部语句已 SetLast。
            }
            StmtKind::If { test, cons, alt } => {
                self.compile_expr(test)?;
                let else_j = self.emit_jump(Op::JumpIfFalse(0));
                self.compile_stmt(cons)?;
                let end_j = self.emit_jump(Op::Jump(0));
                self.patch(else_j, self.here());
                match alt {
                    Some(a) => self.compile_stmt(a)?,
                    None => {
                        self.emit(Op::Undefined);
                        self.emit(Op::SetLast);
                    }
                }
                self.patch(end_j, self.here());
            }
            StmtKind::While { test, body } => {
                let loop_start = self.here();
                self.compile_expr(test)?;
                let end_j = self.emit_jump(Op::JumpIfFalse(0));
                self.push_loop();
                self.compile_stmt(body)?;
                let (cs, bs) = self.pop_loop();
                self.emit(Op::Jump(loop_start));
                let end = self.here();
                self.patch(end_j, end);
                self.patch_continues(&cs, loop_start);
                for p in bs.patches {
                    self.patch(p, end);
                }
                self.emit(Op::Undefined);
                self.emit(Op::SetLast);
            }
            StmtKind::DoWhile { body, test } => {
                let loop_start = self.here();
                // continue 跳到 test 求值处（body 之后回填）。
                self.continue_stack.push(ContinueScope {
                    patches: Vec::new(),
                    env_depth: self.env_depth,
                    finally_depth: self.finally_stack.len(),
                });
                self.break_stack.push(BreakScope {
                    patches: Vec::new(),
                    env_depth: self.env_depth,
                    finally_depth: self.finally_stack.len(),
                });
                self.compile_stmt(body)?;
                let cont_target = self.here();
                let cs = self.continue_stack.pop().expect("loop scope");
                let bs = self.break_stack.pop().expect("loop scope");
                self.patch_continues(&cs, cont_target);
                self.compile_expr(test)?;
                self.emit(Op::JumpIfTrue(loop_start));
                let end = self.here();
                for p in bs.patches {
                    self.patch(p, end);
                }
                self.emit(Op::Undefined);
                self.emit(Op::SetLast);
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => self.compile_for(init.as_ref(), test.as_ref(), update.as_ref(), body)?,
            StmtKind::ForInOf {
                is_of,
                left,
                right,
                body,
            } => self.compile_for_in_of(*is_of, left, right, body)?,
            StmtKind::Return(e) => self.compile_return(e.as_ref())?,
            StmtKind::Break => self.compile_break()?,
            StmtKind::Continue => self.compile_continue()?,
            StmtKind::Throw(e) => {
                self.compile_expr(e)?;
                self.emit(Op::Throw);
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_ref(), finalizer.as_deref())?,
            StmtKind::Switch { disc, cases } => self.compile_switch(disc, cases)?,
        }
        Ok(())
    }

    fn compile_var_decl(
        &mut self,
        kind: &VarKind,
        decls: &[crate::ast::VarDeclarator],
    ) -> Result<(), CompileError> {
        for d in decls {
            let ni = self.add_name(d.id.clone());
            match kind {
                VarKind::Var => {
                    // 已提升；有初值则求值后 assign_force(var_env)。
                    if let Some(init) = &d.init {
                        self.compile_expr(init)?;
                        self.emit(Op::ForceSetVar(ni));
                    }
                }
                VarKind::Let | VarKind::Const => {
                    // 预声明阶段可能已声明（TDZ），Decl* 幂等。
                    if *kind == VarKind::Let {
                        self.emit(Op::DeclLex(ni));
                    } else {
                        self.emit(Op::DeclConst(ni));
                    }
                    match &d.init {
                        Some(init) => {
                            self.compile_expr(init)?;
                            self.emit(Op::InitLex(ni));
                        }
                        None => {
                            if *kind == VarKind::Const {
                                self.emit(Op::MissingConstInit);
                            } else {
                                self.emit(Op::Undefined);
                                self.emit(Op::InitLex(ni));
                            }
                        }
                    }
                }
            }
        }
        self.emit(Op::Undefined);
        self.emit(Op::SetLast);
        Ok(())
    }

    // ------------------------------------------------------------------
    // for / for-in-of
    // ------------------------------------------------------------------

    fn compile_for(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Result<(), CompileError> {
        // 解释器：loop_env = child(env) 恒成立。
        self.emit(Op::PushEnv);
        self.env_depth += 1;
        if let Some(init) = init {
            match init {
                ForInit::Expr(e) => {
                    self.compile_expr(e)?;
                    self.emit(Op::Pop);
                }
                ForInit::VarDecl { kind, decls } => {
                    for d in decls {
                        let ni = self.add_name(d.id.clone());
                        match kind {
                            VarKind::Var => {
                                if let Some(e) = &d.init {
                                    self.compile_expr(e)?;
                                    self.emit(Op::ForceSetVar(ni));
                                }
                            }
                            VarKind::Let | VarKind::Const => {
                                if *kind == VarKind::Let {
                                    self.emit(Op::DeclLex(ni));
                                } else {
                                    self.emit(Op::DeclConst(ni));
                                }
                                match &d.init {
                                    Some(e) => self.compile_expr(e)?,
                                    None => {
                                        if *kind == VarKind::Const {
                                            self.emit(Op::MissingConstInit);
                                        } else {
                                            self.emit(Op::Undefined);
                                        }
                                    }
                                }
                                self.emit(Op::InitLex(ni));
                            }
                        }
                    }
                }
            }
        }
        let loop_start = self.here();
        let mut end_j = None;
        if let Some(t) = test {
            self.compile_expr(t)?;
            end_j = Some(self.emit_jump(Op::JumpIfFalse(0)));
        }
        // continue 跳到 update 处（body 之后回填）。
        self.continue_stack.push(ContinueScope {
            patches: Vec::new(),
            env_depth: self.env_depth,
            finally_depth: self.finally_stack.len(),
        });
        self.break_stack.push(BreakScope {
            patches: Vec::new(),
            env_depth: self.env_depth,
            finally_depth: self.finally_stack.len(),
        });
        self.compile_stmt(body)?;
        let cont_target = self.here();
        let cs = self.continue_stack.pop().expect("loop scope");
        let bs = self.break_stack.pop().expect("loop scope");
        self.patch_continues(&cs, cont_target);
        if let Some(u) = update {
            self.compile_expr(u)?;
            self.emit(Op::Pop);
        }
        self.emit(Op::Jump(loop_start));
        let end = self.here();
        if let Some(j) = end_j {
            self.patch(j, end);
        }
        for p in bs.patches {
            self.patch(p, end);
        }
        self.emit(Op::PopEnv);
        self.env_depth -= 1;
        self.emit(Op::Undefined);
        self.emit(Op::SetLast);
        Ok(())
    }

    fn compile_for_in_of(
        &mut self,
        is_of: bool,
        left: &ForLeft,
        right: &Expr,
        body: &Stmt,
    ) -> Result<(), CompileError> {
        self.compile_expr(right)?;
        self.emit(Op::IterBegin(is_of as u32));
        self.iter_depth += 1;
        // 每轮迭代的左侧绑定是否需要新环境（let/const）。
        let per_iter_env = matches!(
            left,
            ForLeft::VarDecl {
                kind: VarKind::Let | VarKind::Const,
                ..
            }
        );
        let loop_start = self.here();
        let end_j = self.emit_jump(Op::IterNext(0));
        // 循环作用域先于每轮迭代环境入栈：break/continue 能正确弹掉迭代环境。
        self.push_loop();
        match left {
            ForLeft::VarDecl { kind, name } => {
                let ni = self.add_name(name.clone());
                match kind {
                    VarKind::Var => {
                        self.emit(Op::ForceSetVar(ni));
                    }
                    VarKind::Let | VarKind::Const => {
                        self.emit(Op::PushEnv);
                        self.env_depth += 1;
                        if *kind == VarKind::Let {
                            self.emit(Op::DeclLex(ni));
                        } else {
                            self.emit(Op::DeclConst(ni));
                        }
                        self.emit(Op::InitLex(ni));
                    }
                }
            }
            ForLeft::Expr(_) => {
                let fli = self.add_for_left(left.clone());
                self.emit(Op::SetForLeft(fli));
            }
        }
        self.compile_stmt(body)?;
        if per_iter_env {
            self.emit(Op::PopEnv);
            self.env_depth -= 1;
        }
        let (cs, bs) = self.pop_loop();
        self.emit(Op::Jump(loop_start));
        let end = self.here();
        self.patch(end_j, end);
        self.patch_continues(&cs, loop_start);
        for p in bs.patches {
            self.patch(p, end);
        }
        self.iter_depth -= 1;
        self.emit(Op::Undefined);
        self.emit(Op::SetLast);
        Ok(())
    }

    // ------------------------------------------------------------------
    // break / continue / return（含 finally 内联）
    // ------------------------------------------------------------------

    fn push_loop(&mut self) {
        self.continue_stack.push(ContinueScope {
            patches: Vec::new(),
            env_depth: self.env_depth,
            finally_depth: self.finally_stack.len(),
        });
        self.break_stack.push(BreakScope {
            patches: Vec::new(),
            env_depth: self.env_depth,
            finally_depth: self.finally_stack.len(),
        });
    }

    fn pop_loop(&mut self) -> (ContinueScope, BreakScope) {
        let c = self.continue_stack.pop().expect("loop scope");
        let b = self.break_stack.pop().expect("loop scope");
        (c, b)
    }

    /// 回填 continue 跳转到 target。
    fn patch_continues(&mut self, cs: &ContinueScope, target: u32) {
        for p in &cs.patches {
            self.patch(*p, target);
        }
    }

    /// 内联一个 finally 块（含 SaveLast/RestoreLast 与独立子环境）。
    fn compile_finally_inline(&mut self, fin: &Rc<FinallyInfo>) -> Result<(), CompileError> {
        self.emit(Op::SaveLast);
        self.emit(Op::PushEnv);
        self.env_depth += 1;
        self.emit(Op::HoistLex(fin.scope_idx));
        // 注意：fin 自身不压 finally_stack（其内部的 return/break 只穿透外层）。
        let stmts = fin.stmts.clone();
        for s in &stmts {
            self.compile_stmt(s)?;
        }
        self.emit(Op::PopEnv);
        self.env_depth -= 1;
        self.emit(Op::RestoreLast);
        Ok(())
    }

    /// 从 finally_depth 起（不含），由内向外内联所有待执行的 finally。
    /// 内联时逐个弹出：finally 体内部的 return/break 只穿透外层 finally，
    /// 不会重新内联自身（否则无穷递归）。
    fn inline_finallys_from(&mut self, finally_depth: usize) -> Result<(), CompileError> {
        while self.finally_stack.len() > finally_depth {
            let fin = self.finally_stack.pop().expect("finally scope");
            self.compile_finally_inline(&fin)?;
        }
        Ok(())
    }

    /// 发射 PopEnv 到目标深度（动态路径用；不改变编译器的静态 env_depth，
    /// 因为 break/continue 之后的代码仍在原深度编译）。
    fn unwind_env_to(&mut self, depth: u32) {
        let n = self.env_depth.saturating_sub(depth);
        for _ in 0..n {
            self.emit(Op::PopEnv);
        }
    }

    fn compile_break(&mut self) -> Result<(), CompileError> {
        let (env_depth, finally_depth) = match self.break_stack.last() {
            Some(b) => (b.env_depth, b.finally_depth),
            None => {
                if self.top_level {
                    self.emit(Op::BadBreak);
                    return Ok(());
                }
                return Err(CompileError::new("internal: break outside of loop"));
            }
        };
        self.inline_finallys_from(finally_depth)?;
        self.unwind_env_to(env_depth);
        let p = self.emit_jump(Op::Jump(0));
        self.break_stack
            .last_mut()
            .expect("break scope")
            .patches
            .push(p);
        Ok(())
    }

    fn compile_continue(&mut self) -> Result<(), CompileError> {
        let (env_depth, finally_depth) = match self.continue_stack.last() {
            Some(c) => (c.env_depth, c.finally_depth),
            None => {
                if self.top_level {
                    self.emit(Op::BadContinue);
                    return Ok(());
                }
                return Err(CompileError::new("internal: continue outside of loop"));
            }
        };
        self.inline_finallys_from(finally_depth)?;
        self.unwind_env_to(env_depth);
        let p = self.emit_jump(Op::Jump(0));
        self.continue_stack
            .last_mut()
            .expect("continue scope")
            .patches
            .push(p);
        Ok(())
    }

    fn compile_return(&mut self, e: Option<&Expr>) -> Result<(), CompileError> {
        if self.top_level {
            self.emit(Op::BadReturn);
            return Ok(());
        }
        match e {
            Some(x) => self.compile_expr(x)?,
            None => self.emit(Op::Undefined),
        }
        // 值暂存，依次内联所有 pending finally（由内向外），再真正返回。
        // finally 内部的 return 会覆盖（StashRet 重写）。
        self.emit(Op::StashRet);
        self.inline_finallys_from(0)?;
        self.emit(Op::UnstashRet);
        self.emit(Op::Return);
        Ok(())
    }

    // ------------------------------------------------------------------
    // try / switch
    // ------------------------------------------------------------------

    fn compile_try(
        &mut self,
        block: &[Stmt],
        handler: Option<&crate::ast::CatchClause>,
        finalizer: Option<&[Stmt]>,
    ) -> Result<(), CompileError> {
        let try_depth = self.env_depth;
        let iter_depth = self.iter_depth;
        // try 子环境（解释器：try_env = child(env)）。
        self.emit(Op::PushEnv);
        self.env_depth += 1;
        let try_scope = self.add_scope(block.to_vec());
        self.emit(Op::HoistLex(try_scope));
        let region_start = self.here();

        // finally 在 try 体与 catch 体编译期间 pending（return/break 穿透时内联）。
        let fin_info: Option<Rc<FinallyInfo>> = finalizer.map(|fin| {
            let scope_idx = self.add_scope(fin.to_vec());
            Rc::new(FinallyInfo {
                scope_idx,
                stmts: fin.to_vec(),
            })
        });
        if let Some(fi) = &fin_info {
            self.finally_stack.push(fi.clone());
        }
        for s in block {
            self.compile_stmt(s)?;
        }
        if fin_info.is_some() {
            self.finally_stack.pop();
        }
        let region_end = self.here();
        self.emit(Op::PopEnv);
        self.env_depth -= 1;
        // 正常路径的 finally。
        if let Some(fi) = &fin_info {
            self.compile_finally_inline(fi)?;
        }
        let done_j = self.emit_jump(Op::Jump(0));

        // ---- catch handler（只接 Thrown；栈顶为抛出的值） ----
        let mut catch_target = None;
        let mut catch_done_j = None;
        if let Some(h) = handler {
            catch_target = Some(self.here());
            // VM 已把环境截断到 try_depth；此处建 henv。
            self.emit(Op::PushEnv);
            self.env_depth += 1;
            // 解释器顺序：先绑定 catch 参数，再 hoist/declare_lexicals。
            match &h.param {
                Some(p) => {
                    let ni = self.add_name(p.clone());
                    self.emit(Op::DeclLex(ni));
                    self.emit(Op::InitLex(ni));
                }
                None => self.emit(Op::Pop),
            }
            let catch_scope = self.add_scope(h.body.clone());
            self.emit(Op::HoistLex(catch_scope));
            if let Some(fi) = &fin_info {
                self.finally_stack.push(fi.clone());
            }
            for s in &h.body {
                self.compile_stmt(s)?;
            }
            if fin_info.is_some() {
                self.finally_stack.pop();
            }
            self.emit(Op::PopEnv);
            self.env_depth -= 1;
            if let Some(fi) = &fin_info {
                self.compile_finally_inline(fi)?;
            }
            catch_done_j = Some(self.emit_jump(Op::Jump(0)));
        }

        // ---- finally handler（接所有错误；pending_exc 由 VM 置位） ----
        let mut finally_target = None;
        if let Some(fi) = &fin_info {
            finally_target = Some(self.here());
            self.emit(Op::PushEnv);
            self.env_depth += 1;
            self.emit(Op::HoistLex(fi.scope_idx));
            let stmts = fi.stmts.clone();
            for s in &stmts {
                self.compile_stmt(s)?;
            }
            self.emit(Op::PopEnv);
            self.env_depth -= 1;
            self.emit(Op::Rethrow);
        }

        let done = self.here();
        self.patch(done_j, done);
        if let Some(j) = catch_done_j {
            self.patch(j, done);
        }
        if let Some(ct) = catch_target {
            self.chunk.handlers.push(Handler {
                start: region_start,
                end: region_end,
                target: ct,
                kind: HandlerKind::Catch,
                env_depth: try_depth,
                iter_depth,
            });
        }
        if let Some(ft) = finally_target {
            self.chunk.handlers.push(Handler {
                start: region_start,
                end: region_end,
                target: ft,
                kind: HandlerKind::Finally,
                env_depth: try_depth,
                iter_depth,
            });
        }
        // 值：内部语句已 SetLast。
        Ok(())
    }

    fn compile_switch(&mut self, disc: &Expr, cases: &[crate::ast::SwitchCase]) -> Result<(), CompileError> {
        self.compile_expr(disc)?;
        self.emit(Op::SetSwitchDisc);
        self.emit(Op::PushEnv);
        self.env_depth += 1;
        // 分发：逐个 case 测试。
        let mut body_jumps: Vec<(usize, usize)> = Vec::new(); // (case_idx, jump_idx)
        let mut default_case: Option<usize> = None;
        for (i, c) in cases.iter().enumerate() {
            match &c.test {
                Some(t) => {
                    self.emit(Op::GetSwitchDisc);
                    self.compile_expr(t)?;
                    self.emit(Op::StrictEq);
                    let j = self.emit_jump(Op::JumpIfTrue(0));
                    body_jumps.push((i, j));
                }
                None => {
                    if default_case.is_none() {
                        default_case = Some(i);
                    }
                }
            }
        }
        let default_j = self.emit_jump(Op::Jump(0));
        // switch 提供 break 目标（不提供 continue）。
        self.break_stack.push(BreakScope {
            patches: Vec::new(),
            env_depth: self.env_depth,
            finally_depth: self.finally_stack.len(),
        });
        let mut body_pos: Vec<u32> = vec![0; cases.len()];
        for (i, c) in cases.iter().enumerate() {
            body_pos[i] = self.here();
            // 解释器：每个 case 体 exec_block(c.body, sw_env)（同一 env）。
            let scope = self.add_scope(c.body.clone());
            self.emit(Op::HoistLex(scope));
            for s in &c.body {
                self.compile_stmt(s)?;
            }
            // fallthrough：自然落入下一个 case。
        }
        let bs = self.break_stack.pop().unwrap();
        let end = self.here();
        for (case_idx, j) in body_jumps {
            self.patch(j, body_pos[case_idx]);
        }
        match default_case {
            Some(di) => self.patch(default_j, body_pos[di]),
            None => self.patch(default_j, end),
        }
        for p in bs.patches {
            self.patch(p, end);
        }
        self.emit(Op::PopEnv);
        self.env_depth -= 1;
        self.emit(Op::Undefined);
        self.emit(Op::SetLast);
        Ok(())
    }

    // ------------------------------------------------------------------
    // 表达式
    // ------------------------------------------------------------------

    fn compile_expr(&mut self, e: &Expr) -> Result<(), CompileError> {
        let saved = std::mem::replace(&mut self.cur_span, e.span);
        let r = self.compile_expr_inner(e);
        self.cur_span = saved;
        r
    }

    fn compile_expr_inner(&mut self, e: &Expr) -> Result<(), CompileError> {
        match &e.node {
            ExprKind::Literal(l) => {
                let cv = match l {
                    Literal::Number(n) => ConstVal::Number(*n),
                    Literal::String(s) => ConstVal::Str(s.clone()),
                    Literal::Bool(b) => ConstVal::Bool(*b),
                    Literal::Null => ConstVal::Null,
                    // 模板按整体存原文（解释器同）。
                    Literal::Template(s) => ConstVal::Str(s.clone()),
                    Literal::Regex { pattern, flags } => ConstVal::Regex {
                        pattern: pattern.clone(),
                        flags: flags.clone(),
                    },
                };
                let ci = self.add_const(cv);
                self.emit(Op::Const(ci));
            }
            ExprKind::Ident(name) => {
                let ni = self.add_name(name.clone());
                self.emit(Op::GetName(ni));
            }
            ExprKind::This => self.emit(Op::GetThis),
            ExprKind::Super => self.emit(Op::BadSuper),
            ExprKind::Await(_) => self.emit(Op::BadAwait),
            ExprKind::Yield { .. } => self.emit(Op::BadYield),
            ExprKind::PrivateName(_) => self.emit(Op::BadPrivateName),
            ExprKind::Array(elems) => {
                self.emit(Op::ArrNew);
                for el in elems {
                    match el {
                        crate::ast::ArrayElem::Expr(x) => {
                            self.compile_expr(x)?;
                            self.emit(Op::ArrPush);
                        }
                        crate::ast::ArrayElem::Hole => self.emit(Op::ArrHole),
                        crate::ast::ArrayElem::Spread(x) => {
                            self.compile_expr(x)?;
                            self.emit(Op::ArrSpread);
                        }
                    }
                }
                self.emit(Op::ArrDone);
            }
            ExprKind::Object(props) => {
                self.emit(Op::ObjNew);
                for p in props {
                    // 键（解释器先求键再求值）。
                    match &p.key {
                        crate::ast::PropKey::Computed(x) => self.compile_expr(x)?,
                        _ => {
                            let key = self.member_key_static_key(&p.key)?;
                            let ci = self.add_const(ConstVal::Str(key));
                            self.emit(Op::Const(ci));
                        }
                    }
                    match &p.value {
                        crate::ast::PropValue::Init(x) => self.compile_expr(x)?,
                        crate::ast::PropValue::Shorthand(name) => {
                            let ni = self.add_name(name.clone());
                            self.emit(Op::GetName(ni));
                        }
                        crate::ast::PropValue::Method(f)
                        | crate::ast::PropValue::Getter(f)
                        | crate::ast::PropValue::Setter(f) => {
                            // 方法名取自栈顶 key（与解释器一致：先求出的 key）。
                            let fi = self.add_func(FuncMeta {
                                kind: FuncKind::Method(f.as_ref().clone()),
                                span: e.span,
                            });
                            self.emit(Op::MakeMethod(fi));
                        }
                    }
                    self.emit(Op::ObjSet);
                }
                self.emit(Op::ObjDone);
            }
            ExprKind::Function(f) => {
                let fi = self.add_func(FuncMeta {
                    kind: FuncKind::Expr(f.as_ref().clone()),
                    span: e.span,
                });
                self.emit(Op::MakeFunction(fi));
            }
            ExprKind::ArrowFunction(a) => {
                let fi = self.add_func(FuncMeta {
                    kind: FuncKind::Arrow {
                        params: a.params.clone(),
                        body: a.body.clone(),
                        strict: a.strict,
                        is_async: a.is_async,
                    },
                    span: e.span,
                });
                self.emit(Op::MakeFunction(fi));
            }
            ExprKind::Class(c) => {
                let ci = self.add_class(c.as_ref().clone());
                self.emit(Op::MakeClass(ci));
            }
            ExprKind::Unary { op, arg } => self.compile_unary(*op, arg)?,
            ExprKind::Update { op, arg, prefix } => self.compile_update(*op, arg, *prefix)?,
            ExprKind::Binary { op, left, right } => {
                // `#x in obj`：左操作数不求值。
                if *op == BinaryOp::In {
                    if let ExprKind::PrivateName(name) = &left.node {
                        self.compile_expr(right)?;
                        let ni = self.add_name(name.clone());
                        self.emit(Op::PrivateIn(ni));
                        return Ok(());
                    }
                }
                self.compile_expr(left)?;
                self.compile_expr(right)?;
                self.emit(Op::Binary(*op));
            }
            ExprKind::Logical { op, left, right } => {
                self.compile_expr(left)?;
                match op {
                    LogicalOp::And => {
                        self.emit(Op::Dup);
                        let end = self.emit_jump(Op::JumpIfFalse(0));
                        self.emit(Op::Pop);
                        self.compile_expr(right)?;
                        self.patch(end, self.here());
                    }
                    LogicalOp::Or => {
                        self.emit(Op::Dup);
                        let end = self.emit_jump(Op::JumpIfTrue(0));
                        self.emit(Op::Pop);
                        self.compile_expr(right)?;
                        self.patch(end, self.here());
                    }
                    LogicalOp::Nullish => {
                        self.emit(Op::Dup);
                        let is_null = self.emit_jump(Op::JumpIfNullish(0));
                        let end = self.emit_jump(Op::Jump(0));
                        self.patch(is_null, self.here());
                        self.emit(Op::Pop);
                        self.compile_expr(right)?;
                        self.patch(end, self.here());
                    }
                }
            }
            ExprKind::Assign { op, left, right } => self.compile_assign(*op, left, right)?,
            ExprKind::Conditional { test, cons, alt } => {
                self.compile_expr(test)?;
                let else_j = self.emit_jump(Op::JumpIfFalse(0));
                self.compile_expr(cons)?;
                let end_j = self.emit_jump(Op::Jump(0));
                self.patch(else_j, self.here());
                self.compile_expr(alt)?;
                self.patch(end_j, self.here());
            }
            ExprKind::Call {
                callee,
                args,
                optional,
            } => self.compile_call(callee, args, *optional)?,
            ExprKind::New { callee, args } => {
                self.compile_expr(callee)?;
                for a in args {
                    self.compile_expr(a)?;
                }
                self.emit(Op::New(args.len() as u32));
            }
            ExprKind::Member {
                obj,
                prop,
                computed,
                optional,
            } => self.compile_member(obj, prop, *computed, *optional)?,
            ExprKind::PrivateMember { obj, name, optional } => {
                self.compile_expr(obj)?;
                let ni = self.add_name(name.clone());
                if *optional {
                    let lnull = self.emit_jump(Op::JumpIfNullish(0));
                    self.emit(Op::GetPrivate(ni));
                    let lend = self.emit_jump(Op::Jump(0));
                    self.patch(lnull, self.here());
                    self.emit(Op::Pop);
                    self.emit(Op::Undefined);
                    self.patch(lend, self.here());
                } else {
                    self.emit(Op::GetPrivate(ni));
                }
            }
            ExprKind::Sequence(es) => {
                let mut first = true;
                for x in es {
                    if !first {
                        self.emit(Op::Pop);
                    }
                    self.compile_expr(x)?;
                    first = false;
                }
                if first {
                    // 空序列（解析器不应产生）：压 undefined。
                    self.emit(Op::Undefined);
                }
            }
        }
        Ok(())
    }

    fn member_key_static_key(
        &mut self,
        key: &crate::ast::PropKey,
    ) -> Result<String, CompileError> {
        match key {
            crate::ast::PropKey::Ident(s) | crate::ast::PropKey::String(s) => Ok(s.clone()),
            crate::ast::PropKey::Number(n) => Ok(number_to_js_string(*n)),
            crate::ast::PropKey::Computed(_) => {
                Err(CompileError::new("internal: computed key in static position"))
            }
        }
    }

    fn compile_unary(&mut self, op: UnaryOp, arg: &Expr) -> Result<(), CompileError> {
        match op {
            UnaryOp::Typeof => match &arg.node {
                ExprKind::Ident(name) => {
                    let ni = self.add_name(name.clone());
                    self.emit(Op::TypeofName(ni));
                }
                _ => {
                    self.compile_expr(arg)?;
                    self.emit(Op::Typeof);
                }
            },
            UnaryOp::Void => {
                self.compile_expr(arg)?;
                self.emit(Op::Pop);
                self.emit(Op::Undefined);
            }
            UnaryOp::Delete => match &arg.node {
                ExprKind::Member { obj, prop, computed, .. } => {
                    self.compile_expr(obj)?;
                    self.compile_member_key(prop, *computed)?;
                    self.emit(Op::DeleteProp);
                }
                ExprKind::Ident(name) => {
                    let ni = self.add_name(name.clone());
                    self.emit(Op::DeleteIdent(ni));
                }
                _ => {
                    self.compile_expr(arg)?;
                    self.emit(Op::Pop);
                    self.emit(Op::True);
                }
            },
            _ => {
                self.compile_expr(arg)?;
                self.emit(Op::Unary(op));
            }
        }
        Ok(())
    }

    /// `++x` / `x--` / `++obj.k` 等。
    fn compile_update(
        &mut self,
        op: UpdateOp,
        arg: &Expr,
        prefix: bool,
    ) -> Result<(), CompileError> {
        match &arg.node {
            ExprKind::Ident(name) => {
                let ni = self.add_name(name.clone());
                self.emit(Op::UpdateName(ni, op, prefix));
            }
            ExprKind::Member { obj, prop, computed, .. } => {
                if matches!(&obj.node, ExprKind::Super) {
                    self.emit(Op::GetThis);
                } else {
                    self.compile_expr(obj)?;
                }
                if !computed {
                    if let Ok(key) = self.member_key_static(prop) {
                        let ni = self.add_name(key);
                        self.emit(Op::UpdatePropName(ni, op, prefix));
                        return Ok(());
                    }
                }
                self.compile_member_key(prop, *computed)?;
                self.emit(Op::UpdateProp(op, prefix));
            }
            ExprKind::PrivateMember { obj, name, .. } => {
                self.compile_expr(obj)?;
                let ni = self.add_name(name.clone());
                self.emit(Op::UpdatePrivate(ni, op, prefix));
            }
            _ => self.emit(Op::BadTarget),
        }
        Ok(())
    }

    /// 赋值（含复合赋值）。赋值表达式的值留在栈顶。
    fn compile_assign(
        &mut self,
        op: AssignOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<(), CompileError> {
        match &left.node {
            ExprKind::Ident(name) => {
                let ni = self.add_name(name.clone());
                self.compile_expr(right)?;
                if op == AssignOp::Assign {
                    self.emit(Op::SetName(ni));
                } else {
                    self.emit(Op::CompoundName(ni, op));
                }
            }
            ExprKind::Member { obj, prop, computed, .. } => {
                let is_super = matches!(&obj.node, ExprKind::Super);
                if is_super {
                    // `super.x = v`：写到 receiver（当前 this）。
                    self.emit(Op::GetThis);
                } else {
                    self.compile_expr(obj)?;
                }
                // 私有写法 `super.#x = v`（改写器生成）：走 SetPrivate。
                if !computed {
                    if let ExprKind::PrivateName(pname) = &prop.node {
                        let ni = self.add_name(pname.clone());
                        self.compile_expr(right)?;
                        if op == AssignOp::Assign {
                            self.emit(Op::SetPrivate(ni));
                        } else {
                            self.emit(Op::CompoundPrivate(ni, op));
                        }
                        return Ok(());
                    }
                }
                let static_key = if !computed {
                    self.member_key_static(prop).ok()
                } else {
                    None
                };
                match static_key {
                    Some(key) => {
                        let ni = self.add_name(key);
                        self.compile_expr(right)?;
                        if op == AssignOp::Assign {
                            self.emit(Op::SetPropName(ni));
                        } else {
                            self.emit(Op::CompoundPropName(ni, op));
                        }
                    }
                    None => {
                        self.compile_member_key(prop, *computed)?;
                        self.compile_expr(right)?;
                        if op == AssignOp::Assign {
                            self.emit(Op::SetProp);
                        } else {
                            self.emit(Op::CompoundProp(op));
                        }
                    }
                }
            }
            ExprKind::PrivateMember { obj, name, .. } => {
                self.compile_expr(obj)?;
                let ni = self.add_name(name.clone());
                self.compile_expr(right)?;
                if op == AssignOp::Assign {
                    self.emit(Op::SetPrivate(ni));
                } else {
                    self.emit(Op::CompoundPrivate(ni, op));
                }
            }
            _ => {
                // 解释器在运行时报 "invalid assignment target"；此处同样延迟到运行时。
                // 为保持求值顺序（先算 target 报错），直接发射 BadTarget
                //（target 为非法时解释器也不求 right）。
                let _ = right;
                self.emit(Op::BadTarget);
            }
        }
        Ok(())
    }

    fn compile_member(
        &mut self,
        obj: &Expr,
        prop: &Expr,
        computed: bool,
        optional: bool,
    ) -> Result<(), CompileError> {
        // `super.x` / `super[k]`。
        if matches!(&obj.node, ExprKind::Super) {
            if computed {
                self.compile_expr(prop)?;
                self.emit(Op::SuperPropDyn);
            } else {
                let key = self.member_key_static(prop)?;
                let ni = self.add_name(key);
                self.emit(Op::SuperPropName(ni));
            }
            return Ok(());
        }
        self.compile_expr(obj)?;
        // 静态键用 Name 变体（省一次常量压栈）。
        let use_name = !computed && self.member_key_static(prop).is_ok();
        if optional {
            let lnull = self.emit_jump(Op::JumpIfNullish(0));
            if use_name {
                let key = self.member_key_static(prop)?;
                let ni = self.add_name(key);
                self.emit(Op::GetPropName(ni));
            } else {
                self.compile_member_key(prop, computed)?;
                self.emit(Op::GetProp);
            }
            let lend = self.emit_jump(Op::Jump(0));
            self.patch(lnull, self.here());
            self.emit(Op::Pop);
            self.emit(Op::Undefined);
            self.patch(lend, self.here());
        } else if use_name {
            let key = self.member_key_static(prop)?;
            let ni = self.add_name(key);
            self.emit(Op::GetPropName(ni));
        } else {
            self.compile_member_key(prop, computed)?;
            self.emit(Op::GetProp);
        }
        Ok(())
    }

    fn compile_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        optional: bool,
    ) -> Result<(), CompileError> {
        // `super(...)`。
        if matches!(&callee.node, ExprKind::Super) {
            for a in args {
                self.compile_expr(a)?;
            }
            self.emit(Op::SuperCall(args.len() as u32));
            return Ok(());
        }
        // 成员调用 `obj.m(...)` / `obj[k](...)`（this 绑定为 obj）。
        if let ExprKind::Member {
            obj,
            prop,
            computed,
            optional: optc,
        } = &callee.node
        {
            // `super.m(...)`。
            if matches!(&obj.node, ExprKind::Super) {
                if *computed {
                    self.compile_expr(prop)?;
                }
                for a in args {
                    self.compile_expr(a)?;
                }
                if *computed {
                    self.emit(Op::SuperMethodDyn(args.len() as u32));
                } else {
                    let key = self.member_key_static(prop)?;
                    let ni = self.add_name(key);
                    self.emit(Op::SuperMethodName(args.len() as u32, ni));
                }
                return Ok(());
            }
            self.compile_expr(obj)?;
            // `a?.b(...)`：base 为 nullish 则整个调用得 undefined（参数不求值）。
            if *optc {
                let lnull = self.emit_jump(Op::JumpIfNullish(0));
                let use_name = !computed && self.member_key_static(prop).is_ok();
                if use_name {
                    let key = self.member_key_static(prop)?;
                    let ni = self.add_name(key);
                    for a in args {
                        self.compile_expr(a)?;
                    }
                    let flags = if optional { CALL_OPTIONAL } else { 0 };
                    self.emit(Op::CallPropName(args.len() as u32, ni, flags));
                } else {
                    self.compile_member_key(prop, *computed)?;
                    for a in args {
                        self.compile_expr(a)?;
                    }
                    let flags = if optional { CALL_OPTIONAL } else { 0 };
                    self.emit(Op::CallPropDyn(args.len() as u32, flags));
                }
                let lend = self.emit_jump(Op::Jump(0));
                self.patch(lnull, self.here());
                self.emit(Op::Pop);
                self.emit(Op::Undefined);
                self.patch(lend, self.here());
                return Ok(());
            }
            let use_name = !computed && self.member_key_static(prop).is_ok();
            if use_name {
                let key = self.member_key_static(prop)?;
                let ni = self.add_name(key);
                for a in args {
                    self.compile_expr(a)?;
                }
                let flags = if optional { CALL_OPTIONAL } else { 0 };
                self.emit(Op::CallPropName(args.len() as u32, ni, flags));
            } else {
                self.compile_member_key(prop, *computed)?;
                for a in args {
                    self.compile_expr(a)?;
                }
                let flags = if optional { CALL_OPTIONAL } else { 0 };
                self.emit(Op::CallPropDyn(args.len() as u32, flags));
            }
            return Ok(());
        }
        // 普通调用（含 `f?.(...)`）。
        self.compile_expr(callee)?;
        if optional {
            let lnull = self.emit_jump(Op::JumpIfNullish(0));
            for a in args {
                self.compile_expr(a)?;
            }
            self.emit(Op::Call(args.len() as u32, CALL_OPTIONAL));
            let lend = self.emit_jump(Op::Jump(0));
            self.patch(lnull, self.here());
            self.emit(Op::Pop);
            self.emit(Op::Undefined);
            self.patch(lend, self.here());
        } else {
            for a in args {
                self.compile_expr(a)?;
            }
            self.emit(Op::Call(args.len() as u32, 0));
        }
        Ok(())
    }
}

