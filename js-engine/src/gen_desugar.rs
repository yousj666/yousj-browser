//! yousj-js · Phase 9：生成器的 AST desugar（状态机改写）。
//!
//! 把 `function*` 体改写为状态机：
//! - 函数体变成 `__yousj$step(__yousj$sv, __yousj$ab, __yousj$abv)` 状态机
//!   （`while(true)` + `switch(__yousj$st)`）；
//! - 每个 `yield expr` 对应一个挂起点：求值 expr → 保存状态 →
//!   `return {value, done: false}`；恢复时 `next(v)` 的 v 成为 yield 表达式的值；
//! - `yield* iter` 展开为迭代器循环（数组/字符串/生成器），内层 yield 同样挂起；
//! - `break`/`continue`/`return`/`throw` 编译为状态跳转；
//! - `try/catch/finally`：同步抛错由按状态包裹的 try/catch 路由；
//!   `gen.return(v)`/`gen.throw(e)` 经 `__yousj$ab`/`__yousj$abv` 参数在挂起点
//!   的 prologue 注入，finally 经"挂起完成值"（`__yousj$pc`）统一收口，
//!   嵌套 finally 按外层链正确依次执行；
//! - 所有 `var/let/const`（不跨函数边界）提升到函数作用域，因为状态
//!   要在多次 `__yousj$step()` 调用之间保持可见。
//!
//! 已知子集偏差（文档化）：
//! - `const` 跨 yield 边界后可被重新赋值（提升时按 `let` 处理）；
//! - 块级 `let` 的作用域泄漏到整个函数；
//! - `for (let i...)` 失去每轮新绑定语义；
//! - `switch` 的 `case` 测试含 `yield`、形参默认值含 `yield` 时报编译错误；
//! - 嵌套在块里的函数声明被无条件提升（与本引擎现有 hoist 语义一致）；
//! - `yield*` 委托时，外层 `return()`/`throw()` 只对内层生成器调一次无参
//!   `return()`，不完全按规范传递完成值；
//! - `for-of`/`[...]` 只接受数组/字符串/生成器（无 Symbol.iterator 协议）。

use crate::ast::*;

#[derive(Debug, Clone)]
pub struct DesugarError(pub String);

impl std::fmt::Display for DesugarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "generator desugar error: {}", self.0)
    }
}

impl std::error::Error for DesugarError {}

// 状态机运行时变量（`__yousj$` 前缀，几乎不可能与用户代码冲突）。
const ST: &str = "__yousj$st";
const PC: &str = "__yousj$pc";
const STEP_FN: &str = "__yousj$step";
const SV: &str = "__yousj$sv"; // step 参数：next(v) 的 v
const AB: &str = "__yousj$ab"; // step 参数：0=无，1=throw 注入，2=return 注入
const ABV: &str = "__yousj$abv"; // step 参数：注入的值
const EX_TMP: &str = "__yousj$ex";
/// wrap_state 的 catch 参数名（与持久的 EX_TMP 区分，避免自赋值遮蔽）。
const CATCH_PARAM: &str = "__yousj$caught";
const ARGS_TMP: &str = "__yousj$args";

type StateId = usize;

/// abrupt 完成路由栈（`break`/`continue`/`return`/`throw` 查找用）。
#[derive(Debug, Clone)]
enum AbruptCtx {
    Loop {
        break_to: StateId,
        continue_to: StateId,
    },
    Switch {
        break_to: StateId,
    },
    Try {
        fin: Option<StateId>,
        catch: Option<StateId>,
        ex_tmp: String,
    },
}

/// try-body 状态的同步抛错包装（状态落定后统一包裹）。
#[derive(Debug, Clone)]
struct StateWrap {
    catch_to: Option<StateId>,
    fin_to: Option<StateId>,
    ex_tmp: String,
}

struct State {
    stmts: Vec<Stmt>,
    wrap: Option<StateWrap>,
}

pub struct GenDesugar {
    strict: bool,
    states: Vec<State>,
    abrupt: Vec<AbruptCtx>,
    tmp_count: usize,
    temps: Vec<String>,
    var_names: Vec<String>,
    let_names: Vec<String>,
    fn_decls: Vec<FunctionNode>,
    need_args: bool,
}

impl GenDesugar {
    pub fn new() -> Self {
        GenDesugar {
            strict: false,
            states: Vec::new(),
            abrupt: Vec::new(),
            tmp_count: 0,
            temps: Vec::new(),
            var_names: Vec::new(),
            let_names: Vec::new(),
            fn_decls: Vec::new(),
            need_args: false,
        }
    }

    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// 改写生成器函数体 → setup 体（返回 `__yousj$step` 闭包）。
    pub fn desugar(
        &mut self,
        params: &[Param],
        body: &[Stmt],
    ) -> Result<Vec<Stmt>, DesugarError> {
        // 1. 形参默认值不允许 yield。
        for p in params {
            if let Some(d) = &p.default {
                if has_yield_expr(d) {
                    return Err(DesugarError(
                        "yield in parameter default is not supported".to_string(),
                    ));
                }
            }
        }
        // 2. 变量提升 + arguments 改写。
        let mut hoisted = Vec::new();
        for s in body {
            self.hoist_stmt(s, &mut hoisted)?;
        }
        // 3. 状态机编译。
        let terminal = self.new_state();
        // terminal：函数正常落定 → {value: undefined, done: true}。
        self.states[terminal].stmts.push(Self::return_result_stmt(
            Self::ident_expr("undefined"),
            true,
        ));
        let entry = self.compile_stmts(&hoisted, terminal)?;
        // 4. 组装。
        Ok(self.assemble(entry))
    }

    // ------------------------------------------------------------------
    // 状态管理
    // ------------------------------------------------------------------

    fn new_state(&mut self) -> StateId {
        let wrap = self.current_wrap();
        self.states.push(State {
            stmts: Vec::new(),
            wrap,
        });
        self.states.len() - 1
    }

    /// 当前 innermost try 的同步抛错包装（状态创建时捕获）。
    fn current_wrap(&self) -> Option<StateWrap> {
        for ctx in self.abrupt.iter().rev() {
            if let AbruptCtx::Try { fin, catch, ex_tmp } = ctx {
                if catch.is_none() && fin.is_none() {
                    continue;
                }
                return Some(StateWrap {
                    catch_to: *catch,
                    fin_to: *fin,
                    ex_tmp: ex_tmp.clone(),
                });
            }
        }
        None
    }

    /// 当前 innermost 带 finally 的 try 入口（yield 的 return 注入用）。
    fn innermost_fin(&self) -> Option<StateId> {
        for ctx in self.abrupt.iter().rev() {
            if let AbruptCtx::Try { fin: Some(f), .. } = ctx {
                return Some(*f);
            }
        }
        None
    }

    fn fresh_tmp(&mut self) -> String {
        let n = self.tmp_count;
        self.tmp_count += 1;
        let name = format!("__yousj$t{}", n);
        self.temps.push(name.clone());
        name
    }

    fn goto(&mut self, from: StateId, to: StateId) {
        self.states[from].stmts.push(Self::goto_stmt(to));
    }

    // ------------------------------------------------------------------
    // AST 构造小件
    // ------------------------------------------------------------------

    fn sp() -> Span {
        Span::point(0, 0)
    }

    fn expr(kind: ExprKind) -> Expr {
        Spanned::new(Self::sp(), kind)
    }

    fn stmt(kind: StmtKind) -> Stmt {
        Spanned::new(Self::sp(), kind)
    }

    fn ident_expr(name: &str) -> Expr {
        Self::expr(ExprKind::Ident(name.to_string()))
    }

    fn num_expr(n: f64) -> Expr {
        Self::expr(ExprKind::Literal(Literal::Number(n)))
    }

    fn str_expr(s: &str) -> Expr {
        Self::expr(ExprKind::Literal(Literal::String(s.to_string())))
    }

    fn bool_expr(b: bool) -> Expr {
        Self::expr(ExprKind::Literal(Literal::Bool(b)))
    }

    fn expr_stmt(e: Expr) -> Stmt {
        Self::stmt(StmtKind::Expr(e))
    }

    fn block_stmt(stmts: Vec<Stmt>) -> Stmt {
        Self::stmt(StmtKind::Block(stmts))
    }

    fn return_stmt(e: Expr) -> Stmt {
        Self::stmt(StmtKind::Return(Some(e)))
    }

    fn throw_stmt(e: Expr) -> Stmt {
        Self::stmt(StmtKind::Throw(e))
    }

    fn assign_expr(name: Expr, val: Expr) -> Expr {
        Self::expr(ExprKind::Assign {
            op: AssignOp::Assign,
            left: Box::new(name),
            right: Box::new(val),
        })
    }

    fn assign_stmt(name: &str, e: Expr) -> Stmt {
        Self::expr_stmt(Self::assign_expr(Self::ident_expr(name), e))
    }

    fn goto_stmt(to: StateId) -> Stmt {
        Self::block_stmt(vec![
            Self::assign_stmt(ST, Self::num_expr(to as f64)),
            Self::stmt(StmtKind::Continue),
        ])
    }

    fn obj_expr(fields: Vec<(&str, Expr)>) -> Expr {
        Self::expr(ExprKind::Object(
            fields
                .into_iter()
                .map(|(k, v)| Prop {
                    key: PropKey::Ident(k.to_string()),
                    value: PropValue::Init(v),
                })
                .collect(),
        ))
    }

    fn member_expr(obj: Expr, prop: &str) -> Expr {
        Self::expr(ExprKind::Member {
            obj: Box::new(obj),
            prop: Box::new(Self::expr(ExprKind::Ident(prop.to_string()))),
            computed: false,
            optional: false,
        })
    }

    fn call_expr(callee: Expr, args: Vec<Expr>) -> Expr {
        Self::expr(ExprKind::Call {
            callee: Box::new(callee),
            args,
            optional: false,
        })
    }

    fn if_stmt(test: Expr, cons: Stmt, alt: Stmt) -> Stmt {
        Self::stmt(StmtKind::If {
            test,
            cons: Box::new(cons),
            alt: Some(Box::new(alt)),
        })
    }

    /// `return {value: <v>, done: <d>}`。
    fn return_result_stmt(value: Expr, done: bool) -> Stmt {
        Self::return_stmt(Self::obj_expr(vec![
            ("value", value),
            ("done", Self::bool_expr(done)),
        ]))
    }

    // ------------------------------------------------------------------
    // 提升（hoist）
    // ------------------------------------------------------------------

    fn declare_name(&mut self, kind: VarKind, name: &str) {
        match kind {
            VarKind::Var => {
                if !self.var_names.contains(&name.to_string()) {
                    self.var_names.push(name.to_string());
                }
            }
            VarKind::Let | VarKind::Const => {
                // const 按 let 提升（跨 yield 可重赋，文档化偏差）。
                if !self.let_names.contains(&name.to_string()) {
                    self.let_names.push(name.to_string());
                }
            }
        }
    }

    fn seq_or_single(exprs: Vec<Expr>) -> Expr {
        if exprs.len() == 1 {
            exprs.into_iter().next().unwrap()
        } else {
            Self::expr(ExprKind::Sequence(exprs))
        }
    }

    /// 提升并改写单条语句，结果追加到 `out`。
    fn hoist_stmt(&mut self, s: &Stmt, out: &mut Vec<Stmt>) -> Result<(), DesugarError> {
        match &s.node {
            StmtKind::VarDecl { kind, decls } => {
                let mut assigns = Vec::new();
                for d in decls {
                    self.declare_name(*kind, &d.id);
                    if let Some(init) = &d.init {
                        let init = self.hoist_expr(init);
                        assigns.push(Self::assign_expr(Self::ident_expr(&d.id), init));
                    }
                }
                if !assigns.is_empty() {
                    out.push(Self::expr_stmt(Self::seq_or_single(assigns)));
                }
            }
            StmtKind::FunctionDecl(f) => {
                self.fn_decls.push(f.as_ref().clone());
            }
            StmtKind::ClassDecl(c) => {
                // 类名按 let 提升；求值语句保留在流中。
                let name = match &c.id {
                    Some(n) => n.clone(),
                    None => {
                        return Err(DesugarError(
                            "internal: anonymous class declaration".to_string(),
                        ))
                    }
                };
                self.declare_name(VarKind::Let, &name);
                out.push(Self::expr_stmt(Self::assign_expr(
                    Self::ident_expr(&name),
                    Spanned::new(s.span, ExprKind::Class(c.clone())),
                )));
            }
            StmtKind::Block(b) => {
                let mut inner = Vec::new();
                for x in b {
                    self.hoist_stmt(x, &mut inner)?;
                }
                out.push(Self::block_stmt(inner));
            }
            StmtKind::Expr(e) => out.push(Self::expr_stmt(self.hoist_expr(e))),
            StmtKind::If { test, cons, alt } => {
                out.push(Self::stmt(StmtKind::If {
                    test: self.hoist_expr(test),
                    cons: Box::new(self.hoist_one_to_block(cons)?),
                    alt: alt
                        .as_ref()
                        .map(|a| self.hoist_one_to_block(a))
                        .transpose()?
                        .map(Box::new),
                }));
            }
            StmtKind::While { test, body } => out.push(Self::stmt(StmtKind::While {
                test: self.hoist_expr(test),
                body: Box::new(self.hoist_one_to_block(body)?),
            })),
            StmtKind::DoWhile { body, test } => out.push(Self::stmt(StmtKind::DoWhile {
                body: Box::new(self.hoist_one_to_block(body)?),
                test: self.hoist_expr(test),
            })),
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                let init2 = match init {
                    Some(ForInit::VarDecl { kind, decls }) => {
                        let mut assigns = Vec::new();
                        for d in decls {
                            self.declare_name(*kind, &d.id);
                            if let Some(i) = &d.init {
                                assigns.push(Self::assign_expr(
                                    Self::ident_expr(&d.id),
                                    self.hoist_expr(i),
                                ));
                            }
                        }
                        if assigns.is_empty() {
                            None
                        } else {
                            Some(ForInit::Expr(Self::seq_or_single(assigns)))
                        }
                    }
                    Some(ForInit::Expr(e)) => Some(ForInit::Expr(self.hoist_expr(e))),
                    None => None,
                };
                out.push(Self::stmt(StmtKind::For {
                    init: init2,
                    test: test.as_ref().map(|e| self.hoist_expr(e)),
                    update: update.as_ref().map(|e| self.hoist_expr(e)),
                    body: Box::new(self.hoist_one_to_block(body)?),
                }));
            }
            StmtKind::ForInOf { left, right, body, is_of } => {
                // for-in/of 的 left 声明也提升（每轮新绑定语义丢失，文档化）。
                if let ForLeft::VarDecl { kind, name } = left {
                    self.declare_name(*kind, name);
                }
                out.push(Self::stmt(StmtKind::ForInOf {
                    is_of: *is_of,
                    left: left.clone(),
                    right: self.hoist_expr(right),
                    body: Box::new(self.hoist_one_to_block(body)?),
                }));
            }
            StmtKind::Return(e) => out.push(Self::stmt(StmtKind::Return(
                e.as_ref().map(|x| self.hoist_expr(x)),
            ))),
            StmtKind::Throw(e) => out.push(Self::throw_stmt(self.hoist_expr(e))),
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                let mut b2 = Vec::new();
                for x in block {
                    self.hoist_stmt(x, &mut b2)?;
                }
                let h2 = match handler {
                    Some(h) => {
                        if let Some(p) = &h.param {
                            self.declare_name(VarKind::Let, p);
                        }
                        let mut hb = Vec::new();
                        for x in &h.body {
                            self.hoist_stmt(x, &mut hb)?;
                        }
                        Some(CatchClause {
                            param: h.param.clone(),
                            body: hb,
                        })
                    }
                    None => None,
                };
                let f2 = match finalizer {
                    Some(f) => {
                        let mut fb = Vec::new();
                        for x in f {
                            self.hoist_stmt(x, &mut fb)?;
                        }
                        Some(fb)
                    }
                    None => None,
                };
                out.push(Self::stmt(StmtKind::Try {
                    block: b2,
                    handler: h2,
                    finalizer: f2,
                }));
            }
            StmtKind::Switch { disc, cases } => {
                let mut c2 = Vec::new();
                for case in cases {
                    let mut cb = Vec::new();
                    for x in &case.body {
                        self.hoist_stmt(x, &mut cb)?;
                    }
                    c2.push(SwitchCase {
                        test: case.test.as_ref().map(|e| self.hoist_expr(e)),
                        body: cb,
                    });
                }
                out.push(Self::stmt(StmtKind::Switch {
                    disc: self.hoist_expr(disc),
                    cases: c2,
                }));
            }
            StmtKind::Empty | StmtKind::Debugger | StmtKind::Break | StmtKind::Continue => {
                out.push(s.clone())
            }
            StmtKind::Import { .. }
            | StmtKind::ExportDecl { .. }
            | StmtKind::ExportFunc(_)
            | StmtKind::ExportNames(_) => {
                return Err(DesugarError(
                    "import/export is not supported in generators".to_string(),
                ))
            }
        }
        Ok(())
    }

    fn hoist_one_to_block(&mut self, s: &Stmt) -> Result<Stmt, DesugarError> {
        match &s.node {
            StmtKind::Block(_) => {
                let mut inner = Vec::new();
                if let StmtKind::Block(b) = &s.node {
                    for x in b {
                        self.hoist_stmt(x, &mut inner)?;
                    }
                }
                Ok(Self::block_stmt(inner))
            }
            _ => {
                let mut inner = Vec::new();
                self.hoist_stmt(s, &mut inner)?;
                Ok(Self::block_stmt(inner))
            }
        }
    }

    /// 表达式提升改写：只做 `arguments` 改写与递归；yield 保留给编译阶段。
    fn hoist_expr(&mut self, e: &Expr) -> Expr {
        let kind = match &e.node {
            ExprKind::Ident(name) if name == "arguments" => {
                self.need_args = true;
                ExprKind::Ident(ARGS_TMP.to_string())
            }
            // 函数边界：内部另行 desugar。
            ExprKind::Function(_)
            | ExprKind::ArrowFunction(_)
            | ExprKind::Class(_) => return e.clone(),
            ExprKind::Array(elems) => ExprKind::Array(
                elems
                    .iter()
                    .map(|el| match el {
                        ArrayElem::Expr(x) => ArrayElem::Expr(self.hoist_expr(x)),
                        ArrayElem::Hole => ArrayElem::Hole,
                        ArrayElem::Spread(x) => ArrayElem::Spread(self.hoist_expr(x)),
                    })
                    .collect(),
            ),
            ExprKind::Object(props) => ExprKind::Object(
                props
                    .iter()
                    .map(|p| {
                        let key = match &p.key {
                            PropKey::Computed(x) => PropKey::Computed(self.hoist_expr(x)),
                            k => k.clone(),
                        };
                        let value = match &p.value {
                            PropValue::Init(x) => PropValue::Init(self.hoist_expr(x)),
                            PropValue::Shorthand(n) if n == "arguments" => {
                                self.need_args = true;
                                PropValue::Init(Self::ident_expr(ARGS_TMP))
                            }
                            v => v.clone(),
                        };
                        Prop { key, value }
                    })
                    .collect(),
            ),
            ExprKind::Unary { op, arg } => ExprKind::Unary {
                op: *op,
                arg: Box::new(self.hoist_expr(arg)),
            },
            ExprKind::Update { op, arg, prefix } => ExprKind::Update {
                op: *op,
                arg: Box::new(self.hoist_expr(arg)),
                prefix: *prefix,
            },
            ExprKind::Binary { op, left, right } => ExprKind::Binary {
                op: *op,
                left: Box::new(self.hoist_expr(left)),
                right: Box::new(self.hoist_expr(right)),
            },
            ExprKind::Logical { op, left, right } => ExprKind::Logical {
                op: *op,
                left: Box::new(self.hoist_expr(left)),
                right: Box::new(self.hoist_expr(right)),
            },
            ExprKind::Assign { op, left, right } => ExprKind::Assign {
                op: *op,
                left: Box::new(self.hoist_expr(left)),
                right: Box::new(self.hoist_expr(right)),
            },
            ExprKind::Conditional { test, cons, alt } => ExprKind::Conditional {
                test: Box::new(self.hoist_expr(test)),
                cons: Box::new(self.hoist_expr(cons)),
                alt: Box::new(self.hoist_expr(alt)),
            },
            ExprKind::Call {
                callee,
                args,
                optional,
            } => ExprKind::Call {
                callee: Box::new(self.hoist_expr(callee)),
                args: args.iter().map(|a| self.hoist_expr(a)).collect(),
                optional: *optional,
            },
            ExprKind::New { callee, args } => ExprKind::New {
                callee: Box::new(self.hoist_expr(callee)),
                args: args.iter().map(|a| self.hoist_expr(a)).collect(),
            },
            ExprKind::Member {
                obj,
                prop,
                computed,
                optional,
            } => ExprKind::Member {
                obj: Box::new(self.hoist_expr(obj)),
                prop: Box::new(self.hoist_expr(prop)),
                computed: *computed,
                optional: *optional,
            },
            ExprKind::Sequence(exprs) => {
                ExprKind::Sequence(exprs.iter().map(|x| self.hoist_expr(x)).collect())
            }
            ExprKind::Yield { arg, delegate } => ExprKind::Yield {
                arg: arg.as_ref().map(|a| Box::new(self.hoist_expr(a))),
                delegate: *delegate,
            },
            ExprKind::PrivateMember { obj, name, optional } => ExprKind::PrivateMember {
                obj: Box::new(self.hoist_expr(obj)),
                name: name.clone(),
                optional: *optional,
            },
            _ => return e.clone(),
        };
        Spanned::new(e.span, kind)
    }

    // ------------------------------------------------------------------
    // yield 拆分：表达式 → (结束状态, 无 yield 的简单表达式)
    // ------------------------------------------------------------------

    /// 编译表达式 `e`（可含 yield），返回 `(结束状态, 简单表达式)`。
    /// 简单表达式不含 yield，可直接由解释器求值；求值顺序与源码一致。
    fn split_yield(
        &mut self,
        e: &Expr,
        cur: StateId,
    ) -> Result<(StateId, Expr), DesugarError> {
        let span = e.span;
        // 小件：把简单表达式存入新 temp，返回 (状态, Ident(temp))。
        // 调用方用此将中间值暂存。
        match &e.node {
            ExprKind::Literal(_)
            | ExprKind::Ident(_)
            | ExprKind::This
            | ExprKind::Super
            | ExprKind::PrivateName(_) => Ok((cur, e.clone())),
            // 函数/类边界：内部另行处理。
            ExprKind::Function(_)
            | ExprKind::ArrowFunction(_)
            | ExprKind::Class(_) => Ok((cur, e.clone())),
            ExprKind::Await(_) => Err(DesugarError(
                "internal: await reached generator desugar".to_string(),
            )),
            ExprKind::Yield { arg, delegate } => {
                if *delegate {
                    self.split_yield_star(arg.as_deref(), cur, span)
                } else {
                    self.split_yield_point(arg.as_deref(), cur, span, None)
                }
            }
            ExprKind::Unary { op, arg } => {
                let (st1, a) = self.split_yield(arg, cur)?;
                let t = self.fresh_tmp();
                self.states[st1].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Unary {
                        op: *op,
                        arg: Box::new(a),
                    }),
                ));
                Ok((st1, Self::ident_expr(&t)))
            }
            ExprKind::Update { op, arg, prefix } => {
                let (st1, a) = self.split_yield(arg, cur)?;
                let t = self.fresh_tmp();
                self.states[st1].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Update {
                        op: *op,
                        arg: Box::new(a),
                        prefix: *prefix,
                    }),
                ));
                Ok((st1, Self::ident_expr(&t)))
            }
            ExprKind::Binary { op, left, right } => {
                let (st1, l) = self.split_yield(left, cur)?;
                let (st2, r) = self.split_yield(right, st1)?;
                let t = self.fresh_tmp();
                self.states[st2].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Binary {
                        op: *op,
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                ));
                Ok((st2, Self::ident_expr(&t)))
            }
            ExprKind::Logical { op, left, right } => {
                self.split_logical(*op, left, right, cur, span)
            }
            ExprKind::Assign { op, left, right } => {
                // 先左引用后右值（与解释器求值顺序一致）。
                let (st1, l) = self.split_yield(left, cur)?;
                let (st2, r) = self.split_yield(right, st1)?;
                let t = self.fresh_tmp();
                self.states[st2].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Assign {
                        op: *op,
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                ));
                Ok((st2, Self::ident_expr(&t)))
            }
            ExprKind::Conditional { test, cons, alt } => {
                let (st1, t_simple) = self.split_yield(test, cur)?;
                let t_c = self.fresh_tmp();
                self.states[st1]
                    .stmts
                    .push(Self::assign_stmt(&t_c, t_simple));
                let t = self.fresh_tmp();
                let s_cons = self.new_state();
                let s_alt = self.new_state();
                let s_join = self.new_state();
                self.states[st1].stmts.push(Self::if_stmt(
                    Self::ident_expr(&t_c),
                    Self::goto_stmt(s_cons),
                    Self::goto_stmt(s_alt),
                ));
                let (st2, c_simple) = self.split_yield(cons, s_cons)?;
                self.states[st2]
                    .stmts
                    .push(Self::assign_stmt(&t, c_simple));
                self.goto(st2, s_join);
                let (st3, a_simple) = self.split_yield(alt, s_alt)?;
                self.states[st3]
                    .stmts
                    .push(Self::assign_stmt(&t, a_simple));
                self.goto(st3, s_join);
                Ok((s_join, Self::ident_expr(&t)))
            }
            ExprKind::Call {
                callee,
                args,
                optional,
            } => {
                // 方法调用 `obj.m(...)`：callee 为 Member 时不简化为 Ident，
                // 保留 Member 结构以便 eval_call 提取 base 作为 this。
                // （split_yield(Member) 会把 `log.push` 存入临时变量丢掉 base。）
                let (mut st, c_simple) = match &callee.node {
                    ExprKind::Member {
                        obj,
                        prop,
                        computed,
                        optional: optc,
                    } => {
                        let (st1, o) = self.split_yield(obj, cur)?;
                        let (st2, p) = self.split_yield(prop, st1)?;
                        let member = Spanned::new(
                            callee.span,
                            ExprKind::Member {
                                obj: Box::new(o),
                                prop: Box::new(p),
                                computed: *computed,
                                optional: *optc,
                            },
                        );
                        (st2, member)
                    }
                    _ => self.split_yield(callee, cur)?,
                };
                let mut arg_simples = Vec::with_capacity(args.len());
                for a in args {
                    let (st2, a_simple) = self.split_yield(a, st)?;
                    st = st2;
                    arg_simples.push(a_simple);
                }
                let t = self.fresh_tmp();
                self.states[st].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Call {
                        callee: Box::new(c_simple),
                        args: arg_simples,
                        optional: *optional,
                    }),
                ));
                Ok((st, Self::ident_expr(&t)))
            }
            ExprKind::New { callee, args } => {
                let (mut st, c_simple) = self.split_yield(callee, cur)?;
                let mut arg_simples = Vec::with_capacity(args.len());
                for a in args {
                    let (st2, a_simple) = self.split_yield(a, st)?;
                    st = st2;
                    arg_simples.push(a_simple);
                }
                let t = self.fresh_tmp();
                self.states[st].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::New {
                        callee: Box::new(c_simple),
                        args: arg_simples,
                    }),
                ));
                Ok((st, Self::ident_expr(&t)))
            }
            ExprKind::Member {
                obj,
                prop,
                computed,
                optional,
            } => {
                let (st1, o) = self.split_yield(obj, cur)?;
                let (st2, p) = self.split_yield(prop, st1)?;
                let t = self.fresh_tmp();
                self.states[st2].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Member {
                        obj: Box::new(o),
                        prop: Box::new(p),
                        computed: *computed,
                        optional: *optional,
                    }),
                ));
                Ok((st2, Self::ident_expr(&t)))
            }
            ExprKind::PrivateMember { obj, name, optional } => {
                let (st1, o) = self.split_yield(obj, cur)?;
                let t = self.fresh_tmp();
                self.states[st1].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::PrivateMember {
                        obj: Box::new(o),
                        name: name.clone(),
                        optional: *optional,
                    }),
                ));
                Ok((st1, Self::ident_expr(&t)))
            }
            ExprKind::Sequence(exprs) => {
                let mut st = cur;
                let mut last = Self::ident_expr("undefined");
                for x in exprs {
                    let (st2, s) = self.split_yield(x, st)?;
                    st = st2;
                    last = s;
                }
                Ok((st, last))
            }
            ExprKind::Array(elems) => {
                let mut st = cur;
                let mut out = Vec::with_capacity(elems.len());
                for el in elems {
                    match el {
                        ArrayElem::Expr(x) => {
                            let (st2, s) = self.split_yield(x, st)?;
                            st = st2;
                            out.push(ArrayElem::Expr(s));
                        }
                        ArrayElem::Hole => out.push(ArrayElem::Hole),
                        ArrayElem::Spread(x) => {
                            let (st2, s) = self.split_yield(x, st)?;
                            st = st2;
                            out.push(ArrayElem::Spread(s));
                        }
                    }
                }
                let t = self.fresh_tmp();
                self.states[st].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Array(out)),
                ));
                Ok((st, Self::ident_expr(&t)))
            }
            ExprKind::Object(props) => {
                let mut st = cur;
                let mut out = Vec::with_capacity(props.len());
                for p in props {
                    let key = match &p.key {
                        PropKey::Computed(x) => {
                            let (st2, s) = self.split_yield(x, st)?;
                            st = st2;
                            PropKey::Computed(s)
                        }
                        k => k.clone(),
                    };
                    let value = match &p.value {
                        PropValue::Init(x) => {
                            let (st2, s) = self.split_yield(x, st)?;
                            st = st2;
                            PropValue::Init(s)
                        }
                        // 方法/getter/setter 是函数边界。
                        v => v.clone(),
                    };
                    out.push(Prop { key, value });
                }
                let t = self.fresh_tmp();
                self.states[st].stmts.push(Self::assign_stmt(
                    &t,
                    Spanned::new(span, ExprKind::Object(out)),
                ));
                Ok((st, Self::ident_expr(&t)))
            }
        }
    }

    /// `&&` / `||` / `??` 的短路拆分。
    fn split_logical(
        &mut self,
        op: LogicalOp,
        left: &Expr,
        right: &Expr,
        cur: StateId,
        span: Span,
    ) -> Result<(StateId, Expr), DesugarError> {
        let (st1, l_simple) = self.split_yield(left, cur)?;
        let t_c = self.fresh_tmp();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_c, l_simple));
        let t = self.fresh_tmp();
        let s_right = self.new_state();
        let s_join = self.new_state();
        // 分支条件：
        //  && : t_c 真 → 右；假 → 结果为 t_c
        //  || : t_c 真 → 结果为 t_c；假 → 右
        //  ?? : t_c 非空 → 结果为 t_c；空 → 右
        let test: Expr = match op {
            LogicalOp::And | LogicalOp::Or => Self::ident_expr(&t_c),
            LogicalOp::Nullish => Spanned::new(
                span,
                ExprKind::Logical {
                    op: LogicalOp::Or,
                    left: Box::new(Spanned::new(
                        span,
                        ExprKind::Binary {
                            op: BinaryOp::StrictEq,
                            left: Box::new(Self::ident_expr(&t_c)),
                            right: Box::new(Self::expr(ExprKind::Literal(Literal::Null))),
                        },
                    )),
                    right: Box::new(Spanned::new(
                        span,
                        ExprKind::Binary {
                            op: BinaryOp::StrictEq,
                            left: Box::new(Self::ident_expr(&t_c)),
                            right: Box::new(Self::ident_expr("undefined")),
                        },
                    )),
                },
            ),
        };
        let (goto_right, goto_join_with_tc) = match op {
            LogicalOp::And => (true, false),
            LogicalOp::Or | LogicalOp::Nullish => (false, true),
        };
        // st1: if (test) goto X else goto Y
        let (cons_goto, alt_goto) = if goto_right {
            (Self::goto_stmt(s_right), {
                let s = self.new_state();
                self.states[s]
                    .stmts
                    .push(Self::assign_stmt(&t, Self::ident_expr(&t_c)));
                self.goto(s, s_join);
                Self::goto_stmt(s)
            })
        } else {
            ({
                let s = self.new_state();
                self.states[s]
                    .stmts
                    .push(Self::assign_stmt(&t, Self::ident_expr(&t_c)));
                self.goto(s, s_join);
                Self::goto_stmt(s)
            }, Self::goto_stmt(s_right))
        };
        let _ = goto_join_with_tc;
        self.states[st1]
            .stmts
            .push(Self::if_stmt(test, cons_goto, alt_goto));
        let (st2, r_simple) = self.split_yield(right, s_right)?;
        self.states[st2].stmts.push(Self::assign_stmt(&t, r_simple));
        self.goto(st2, s_join);
        Ok((s_join, Self::ident_expr(&t)))
    }

    /// 在 `cur` 发射一个普通 yield 点。
    /// `arg`: yield 的参数（已 strip 为 Option<&Expr>）；`deleg_it`: yield* 时的
    /// 迭代器 temp（挂起点的 abrupt prologue 需先收尾内层迭代器）。
    /// 返回 (resume 状态, 代表 yield 表达式值的简单表达式)。
    fn split_yield_point(
        &mut self,
        arg: Option<&Expr>,
        cur: StateId,
        _span: Span,
        deleg_it: Option<String>,
    ) -> Result<(StateId, Expr), DesugarError> {
        // 1. 求值参数。
        let (st1, arg_simple) = match arg {
            Some(a) => self.split_yield(a, cur)?,
            None => (cur, Self::ident_expr("undefined")),
        };
        let t_val = self.fresh_tmp();
        // st1: t_val = arg; st = RESUME; return {value: t_val, done: false}
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_val, arg_simple));
        let resume = self.new_state();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(ST, Self::num_expr(resume as f64)));
        self.states[st1].stmts.push(Self::return_result_stmt(
            Self::ident_expr(&t_val),
            false,
        ));
        // 2. resume 的 prologue：abrupt 注入处理 + 接收 sent 值。
        let t_recv = self.fresh_tmp();
        self.emit_resume_prologue(resume, &t_recv, deleg_it.as_deref());
        Ok((resume, Self::ident_expr(&t_recv)))
    }

    /// resume 状态的 prologue：
    /// ```text
    /// if (__yousj$ab === 1 || __yousj$ab === 2) { <deleg 时先调内层 return> }
    /// if (__yousj$ab === 1) { throw __yousj$abv; }
    /// if (__yousj$ab === 2) {
    ///   __yousj$pc = {type: "ret", value: __yousj$abv};
    ///   __yousj$st = <innermost fin>; continue;   // 或直接 return {value, done: true}
    /// }
    /// <t_recv> = __yousj$sv;
    /// ```
    fn emit_resume_prologue(
        &mut self,
        resume: StateId,
        t_recv: &str,
        deleg_it: Option<&str>,
    ) {
        let ab = Self::ident_expr(AB);
        let abv = Self::ident_expr(ABV);
        // 委托 yield* 时：abrupt 注入先让内层迭代器收尾（子集：无参 return）。
        if let Some(it) = deleg_it {
            let it_e = Self::ident_expr(it);
            // t_it.k === 2 → t_it.g.return()
            let inner_ret = Self::expr_stmt(Self::call_expr(
                Self::member_expr(Self::member_expr(it_e, "g"), "return"),
                vec![],
            ));
            let cond = Self::expr(ExprKind::Logical {
                op: LogicalOp::Or,
                left: Box::new(Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(ab.clone()),
                    right: Box::new(Self::num_expr(1.0)),
                })),
                right: Box::new(Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(ab.clone()),
                    right: Box::new(Self::num_expr(2.0)),
                })),
            });
            let k_is_2 = Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(Self::member_expr(Self::ident_expr(it), "k")),
                right: Box::new(Self::num_expr(2.0)),
            });
            self.states[resume].stmts.push(Self::if_stmt(
                cond,
                Self::if_stmt(k_is_2, inner_ret, Self::block_stmt(vec![])),
                Self::block_stmt(vec![]),
            ));
        }
        // if (ab === 1) throw abv;
        self.states[resume].stmts.push(Self::if_stmt(
            Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(ab.clone()),
                right: Box::new(Self::num_expr(1.0)),
            }),
            Self::throw_stmt(abv.clone()),
            Self::block_stmt(vec![]),
        ));
        // if (ab === 2) { pc = {type: "ret", value: abv}; st = fin; continue; }
        //             或 { return {value: abv, done: true}; }
        let mut ret_branch = vec![Self::assign_stmt(
            PC,
            Self::obj_expr(vec![
                ("type", Self::str_expr("ret")),
                ("value", abv.clone()),
            ]),
        )];
        match self.innermost_fin() {
            Some(fin) => {
                ret_branch.push(Self::assign_stmt(ST, Self::num_expr(fin as f64)));
                ret_branch.push(Self::stmt(StmtKind::Continue));
            }
            None => {
                ret_branch.push(Self::return_result_stmt(abv.clone(), true));
            }
        }
        self.states[resume].stmts.push(Self::if_stmt(
            Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(ab.clone()),
                right: Box::new(Self::num_expr(2.0)),
            }),
            Self::block_stmt(ret_branch),
            Self::block_stmt(vec![]),
        ));
        // t_recv = sv;
        self.states[resume]
            .stmts
            .push(Self::assign_stmt(t_recv, Self::ident_expr(SV)));
    }

    /// `yield* expr` 展开为迭代器循环。
    /// 返回 (结束状态, yield* 表达式值的简单表达式)。
    fn split_yield_star(
        &mut self,
        arg: Option<&Expr>,
        cur: StateId,
        span: Span,
    ) -> Result<(StateId, Expr), DesugarError> {
        let a = arg.ok_or_else(|| {
            DesugarError("yield* requires an argument".to_string())
        })?;
        // 1. 求值源表达式。
        let (st1, src_simple) = self.split_yield(a, cur)?;
        let t_src = self.fresh_tmp();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_src, src_simple));
        // 2. t_it = __yousj$iter_of(t_src)
        let t_it = self.fresh_tmp();
        self.states[st1].stmts.push(Self::assign_stmt(
            &t_it,
            Self::call_expr(
                Self::ident_expr("__yousj$iter_of"),
                vec![Self::ident_expr(&t_src)],
            ),
        ));
        let t_r = self.fresh_tmp();
        let t_sent = self.fresh_tmp();
        // 3. 循环：
        //   L_head:
        //     if (t_it.k === 2) { t_r = t_it.g.next(t_sent); }
        //     else { t_r = __yousj$iter_next(t_it); }
        //     if (t_r.done) goto L_end; else goto L_yield;
        //   L_yield: <yield t_r.value> (deleg)
        //   R_deleg: prologue; t_sent = recv; goto L_head;
        //   L_end: (result = t_r.value)
        let l_head = self.new_state();
        let l_yield = self.new_state();
        let l_end = self.new_state();
        self.goto(st1, l_head);
        // L_head 的 next 分发。
        let it_e = Self::ident_expr(&t_it);
        let next_via_gen = Self::assign_stmt(
            &t_r,
            Self::call_expr(
                Self::member_expr(Self::member_expr(it_e.clone(), "g"), "next"),
                vec![Self::ident_expr(&t_sent)],
            ),
        );
        let next_via_native = Self::assign_stmt(
            &t_r,
            Self::call_expr(
                Self::ident_expr("__yousj$iter_next"),
                vec![it_e.clone()],
            ),
        );
        self.states[l_head].stmts.push(Self::if_stmt(
            Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(Self::member_expr(it_e, "k")),
                right: Box::new(Self::num_expr(2.0)),
            }),
            next_via_gen,
            next_via_native,
        ));
        self.states[l_head].stmts.push(Self::if_stmt(
            Self::member_expr(Self::ident_expr(&t_r), "done"),
            Self::goto_stmt(l_end),
            Self::goto_stmt(l_yield),
        ));
        // L_yield：yield t_r.value（委托）。
        let t_v = self.fresh_tmp();
        self.states[l_yield].stmts.push(Self::assign_stmt(
            &t_v,
            Self::member_expr(Self::ident_expr(&t_r), "value"),
        ));
        // 复用 split_yield_point 的挂起逻辑，但参数已求值。
        // 手工展开：st = R; return {value: t_v, done: false}
        let resume = self.new_state();
        self.states[l_yield]
            .stmts
            .push(Self::assign_stmt(ST, Self::num_expr(resume as f64)));
        self.states[l_yield].stmts.push(Self::return_result_stmt(
            Self::ident_expr(&t_v),
            false,
        ));
        let t_recv = self.fresh_tmp();
        self.emit_resume_prologue(resume, &t_recv, Some(&t_it));
        // resume 后续：t_sent = recv; goto L_head
        self.states[resume]
            .stmts
            .push(Self::assign_stmt(&t_sent, Self::ident_expr(&t_recv)));
        self.goto(resume, l_head);
        // L_end：yield* 的值 = t_r.value。
        let _ = span;
        Ok((
            l_end,
            Self::member_expr(Self::ident_expr(&t_r), "value"),
        ))
    }

    // ------------------------------------------------------------------
    // 语句编译
    // ------------------------------------------------------------------

    fn compile_stmts(
        &mut self,
        stmts: &[Stmt],
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let mut next = next;
        for s in stmts.iter().rev() {
            next = self.compile_stmt(s, next)?;
        }
        Ok(next)
    }

    fn compile_branch(
        &mut self,
        s: &Stmt,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        match &s.node {
            StmtKind::Block(b) => self.compile_stmts(b, next),
            _ => self.compile_stmt(s, next),
        }
    }

    /// 编译单条语句；`next` 为正常完成后的入口；返回本语句的入口。
    fn compile_stmt(
        &mut self,
        s: &Stmt,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        match &s.node {
            StmtKind::Empty | StmtKind::Debugger => {
                let e = self.new_state();
                self.goto(e, next);
                Ok(e)
            }
            StmtKind::Expr(e) => {
                let e_state = self.new_state();
                let (st1, simple) = self.split_yield(e, e_state)?;
                self.states[st1].stmts.push(Self::expr_stmt(simple));
                self.goto(st1, next);
                Ok(e_state)
            }
            StmtKind::Block(b) => self.compile_stmts(b, next),
            StmtKind::VarDecl { .. } => Err(DesugarError(
                "internal: VarDecl survived hoisting".to_string(),
            )),
            StmtKind::FunctionDecl(_) => {
                // 已提升到顶层；此处无操作。
                let e = self.new_state();
                self.goto(e, next);
                Ok(e)
            }
            StmtKind::ClassDecl(_) => Err(DesugarError(
                "internal: ClassDecl survived hoisting".to_string(),
            )),
            StmtKind::Return(e) => self.compile_return(e.as_ref(), next),
            StmtKind::Throw(e) => {
                let e_state = self.new_state();
                let (st1, simple) = self.split_yield(e, e_state)?;
                self.states[st1].stmts.push(Self::throw_stmt(simple));
                Ok(e_state)
            }
            StmtKind::Break => Ok(self.compile_break(false)),
            StmtKind::Continue => Ok(self.compile_break(true)),
            StmtKind::If { test, cons, alt } => {
                self.compile_if(test, cons, alt.as_deref(), next)
            }
            StmtKind::While { test, body } => self.compile_while(test, body, next),
            StmtKind::DoWhile { body, test } => {
                self.compile_do_while(body, test, next)
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => self.compile_for(init.as_ref(), test.as_ref(), update.as_ref(), body, next),
            StmtKind::ForInOf {
                is_of,
                left,
                right,
                body,
            } => self.compile_for_in_of(*is_of, left, right, body, next),
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_ref(), finalizer.as_deref(), next),
            StmtKind::Switch { disc, cases } => {
                self.compile_switch(disc, cases, next)
            }
            StmtKind::Import { .. }
            | StmtKind::ExportDecl { .. }
            | StmtKind::ExportFunc(_)
            | StmtKind::ExportNames(_) => Err(DesugarError(
                "import/export is not supported in generators".to_string(),
            )),
        }
    }

    fn compile_return(
        &mut self,
        e: Option<&Expr>,
        _next: StateId,
    ) -> Result<StateId, DesugarError> {
        let tmp = self.fresh_tmp();
        let mid = self.new_state();
        // mid：按 abrupt 栈决定是直接完成还是经 finally。
        let mut fin_target: Option<StateId> = None;
        for ctx in self.abrupt.iter().rev() {
            if let AbruptCtx::Try { fin: Some(f), .. } = ctx {
                fin_target = Some(*f);
                break;
            }
        }
        match fin_target {
            Some(f) => {
                self.states[mid].stmts.push(Self::assign_stmt(
                    PC,
                    Self::obj_expr(vec![
                        ("type", Self::str_expr("ret")),
                        ("value", Self::ident_expr(&tmp)),
                    ]),
                ));
                self.goto(mid, f);
            }
            None => {
                self.states[mid].stmts.push(Self::return_result_stmt(
                    Self::ident_expr(&tmp),
                    true,
                ));
            }
        }
        let entry = self.new_state();
        match e {
            Some(x) => {
                let (st1, simple) = self.split_yield(x, entry)?;
                self.states[st1].stmts.push(Self::assign_stmt(&tmp, simple));
                self.goto(st1, mid);
            }
            None => {
                self.states[entry]
                    .stmts
                    .push(Self::assign_stmt(&tmp, Self::ident_expr("undefined")));
                self.goto(entry, mid);
            }
        }
        Ok(entry)
    }

    /// `is_continue` 区分 break / continue。
    /// break/continue 穿过 finally 时：收集其间的 finally 链，
    /// 经 pc.fins 依次执行，最后跳到目标。
    fn compile_break(&mut self, is_continue: bool) -> StateId {
        let mut fins: Vec<StateId> = Vec::new();
        let mut target: Option<StateId> = None;
        for ctx in self.abrupt.iter().rev() {
            match ctx {
                AbruptCtx::Try { fin: Some(f), .. } => {
                    // 只收集目标 loop/switch 之前的 finally。
                    if target.is_none() {
                        fins.push(*f);
                    }
                }
                AbruptCtx::Loop {
                    break_to,
                    continue_to,
                } => {
                    target = Some(if is_continue { *continue_to } else { *break_to });
                    break;
                }
                AbruptCtx::Switch { break_to } if !is_continue => {
                    target = Some(*break_to);
                    break;
                }
                _ => {}
            }
        }
        let e = self.new_state();
        match target {
            Some(t) => {
                if fins.is_empty() {
                    self.goto(e, t);
                } else {
                    // pc = {type: "goto", target: t, fins: [f2, f3, ...]}; st = f1.
                    let mut fin_arr = Vec::new();
                    for f in fins.iter().skip(1) {
                        fin_arr.push(Self::num_expr(*f as f64));
                    }
                    self.states[e].stmts.push(Self::assign_stmt(
                        PC,
                        Self::obj_expr(vec![
                            ("type", Self::str_expr("goto")),
                            ("target", Self::num_expr(t as f64)),
                            ("fins", Self::expr(ExprKind::Array(
                                fin_arr.into_iter().map(ArrayElem::Expr).collect(),
                            ))),
                        ]),
                    ));
                    self.goto(e, fins[0]);
                }
            }
            None => {
                let msg = if is_continue {
                    "continue outside of loop"
                } else {
                    "break outside of loop"
                };
                self.states[e].stmts.push(Self::throw_stmt(Self::str_expr(msg)));
            }
        }
        e
    }

    fn compile_if(
        &mut self,
        test: &Expr,
        cons: &Stmt,
        alt: Option<&Stmt>,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let e_state = self.new_state();
        let (st1, t_simple) = self.split_yield(test, e_state)?;
        let t_c = self.fresh_tmp();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_c, t_simple));
        let cons_entry = self.compile_branch(cons, next)?;
        let alt_entry = match alt {
            Some(a) => self.compile_branch(a, next)?,
            None => next,
        };
        self.states[st1].stmts.push(Self::if_stmt(
            Self::ident_expr(&t_c),
            Self::goto_stmt(cons_entry),
            Self::goto_stmt(alt_entry),
        ));
        Ok(e_state)
    }

    fn compile_while(
        &mut self,
        test: &Expr,
        body: &Stmt,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let l_test = self.new_state();
        let l_body = self.new_state();
        // test 求值可能含 yield：在 l_test 内拆分。
        let (st1, t_simple) = self.split_yield(test, l_test)?;
        let t_c = self.fresh_tmp();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_c, t_simple));
        self.states[st1].stmts.push(Self::if_stmt(
            Self::ident_expr(&t_c),
            Self::goto_stmt(l_body),
            Self::goto_stmt(next),
        ));
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_test,
        });
        let body_entry = self.compile_branch(body, l_test)?;
        self.abrupt.pop();
        self.goto(l_body, body_entry);
        Ok(l_test)
    }

    fn compile_do_while(
        &mut self,
        body: &Stmt,
        test: &Expr,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let l_body = self.new_state();
        let l_test = self.new_state();
        let (st1, t_simple) = self.split_yield(test, l_test)?;
        let t_c = self.fresh_tmp();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_c, t_simple));
        self.states[st1].stmts.push(Self::if_stmt(
            Self::ident_expr(&t_c),
            Self::goto_stmt(l_body),
            Self::goto_stmt(next),
        ));
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_test,
        });
        let body_entry = self.compile_branch(body, l_test)?;
        self.abrupt.pop();
        self.goto(l_body, body_entry);
        Ok(l_body)
    }

    fn compile_for(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        // init 已被 hoist 转为普通表达式（或无）。
        let l_init = self.new_state();
        let l_test = self.new_state();
        let l_body = self.new_state();
        let l_update = self.new_state();
        // init
        let init_end = match init {
            Some(ForInit::Expr(e)) => {
                let (st1, simple) = self.split_yield(e, l_init)?;
                self.states[st1].stmts.push(Self::expr_stmt(simple));
                self.goto(st1, l_test);
                st1
            }
            Some(ForInit::VarDecl { .. }) => {
                return Err(DesugarError(
                    "internal: ForInit::VarDecl survived hoisting".to_string(),
                ))
            }
            None => {
                self.goto(l_init, l_test);
                l_init
            }
        };
        let _ = init_end;
        // test
        match test {
            Some(t) => {
                let (st1, t_simple) = self.split_yield(t, l_test)?;
                let t_c = self.fresh_tmp();
                self.states[st1]
                    .stmts
                    .push(Self::assign_stmt(&t_c, t_simple));
                self.states[st1].stmts.push(Self::if_stmt(
                    Self::ident_expr(&t_c),
                    Self::goto_stmt(l_body),
                    Self::goto_stmt(next),
                ));
            }
            None => {
                self.goto(l_test, l_body);
            }
        }
        // update
        match update {
            Some(u) => {
                let (st1, u_simple) = self.split_yield(u, l_update)?;
                self.states[st1].stmts.push(Self::expr_stmt(u_simple));
                self.goto(st1, l_test);
            }
            None => {
                self.goto(l_update, l_test);
            }
        }
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_update,
        });
        let body_entry = self.compile_branch(body, l_update)?;
        self.abrupt.pop();
        self.goto(l_body, body_entry);
        Ok(l_init)
    }

    /// `for-in` / `for-of` 编译为迭代器循环。
    /// for-of 复用 `__yousj$iter_of`/`__yousj$iter_next`；
    /// for-in 用 `Object.keys` 快照。
    fn compile_for_in_of(
        &mut self,
        is_of: bool,
        left: &ForLeft,
        right: &Expr,
        body: &Stmt,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let e_state = self.new_state();
        let (st1, r_simple) = self.split_yield(right, e_state)?;
        let t_r = self.fresh_tmp();
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_r, r_simple));
        let l_head = self.new_state();
        let l_body = self.new_state();
        if is_of {
            // t_it = __yousj$iter_of(t_r)
            let t_it = self.fresh_tmp();
            self.states[st1].stmts.push(Self::assign_stmt(
                &t_it,
                Self::call_expr(
                    Self::ident_expr("__yousj$iter_of"),
                    vec![Self::ident_expr(&t_r)],
                ),
            ));
            self.goto(st1, l_head);
            // L_head:
            //   if (t_it.k === 2) { t_v = t_it.g.next().value; t_done = t_it.g... }
            //   简化：t_n = (t_it.k === 2) ? t_it.g.next() : __yousj$iter_next(t_it);
            //   if (t_n.done) goto next; t_loop = t_n.value; goto l_body;
            let t_n = self.fresh_tmp();
            let t_loop = self.fresh_tmp();
            let it_e = Self::ident_expr(&t_it);
            let next_via_gen = Self::call_expr(
                Self::member_expr(Self::member_expr(it_e.clone(), "g"), "next"),
                vec![],
            );
            let next_via_native = Self::call_expr(
                Self::ident_expr("__yousj$iter_next"),
                vec![it_e.clone()],
            );
            // t_n = (k===2) ? gen.next() : iter_next(it) —— 用 if/else 避免三元。
            let s_gen = self.new_state();
            let s_nat = self.new_state();
            let s_join = self.new_state();
            self.states[s_gen]
                .stmts
                .push(Self::assign_stmt(&t_n, next_via_gen));
            self.goto(s_gen, s_join);
            self.states[s_nat]
                .stmts
                .push(Self::assign_stmt(&t_n, next_via_native));
            self.goto(s_nat, s_join);
            self.states[l_head].stmts.push(Self::if_stmt(
                Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(Self::member_expr(it_e, "k")),
                    right: Box::new(Self::num_expr(2.0)),
                }),
                Self::goto_stmt(s_gen),
                Self::goto_stmt(s_nat),
            ));
            self.states[s_join].stmts.push(Self::if_stmt(
                Self::member_expr(Self::ident_expr(&t_n), "done"),
                Self::goto_stmt(next),
                Self::block_stmt(vec![
                    Self::assign_stmt(
                        &t_loop,
                        Self::member_expr(Self::ident_expr(&t_n), "value"),
                    ),
                    Self::goto_stmt(l_body),
                ]),
            ));
            self.goto(l_head, s_gen); // 占位，实际由上面的 if 覆盖
            // 修正：l_head 的 if 已经处理分支，删除多余的 goto。
            self.states[l_head].stmts.pop();
            self.compile_for_body(left, &t_loop, l_body, l_head, next, body)?;
        } else {
            // for-in：t_keys = Object.keys(t_r); t_i = 0;
            //   L_head: if (t_i >= t_keys.length) goto next;
            //           t_loop = t_keys[t_i]; t_i = t_i + 1; goto l_body;
            let t_keys = self.fresh_tmp();
            let t_i = self.fresh_tmp();
            let t_loop = self.fresh_tmp();
            self.states[st1].stmts.push(Self::assign_stmt(
                &t_keys,
                Self::call_expr(
                    Self::member_expr(Self::ident_expr("Object"), "keys"),
                    vec![Self::ident_expr(&t_r)],
                ),
            ));
            self.states[st1]
                .stmts
                .push(Self::assign_stmt(&t_i, Self::num_expr(0.0)));
            self.goto(st1, l_head);
            let s_body = self.new_state();
            self.states[l_head].stmts.push(Self::if_stmt(
                Self::expr(ExprKind::Binary {
                    op: BinaryOp::Ge,
                    left: Box::new(Self::ident_expr(&t_i)),
                    right: Box::new(Self::member_expr(
                        Self::ident_expr(&t_keys),
                        "length",
                    )),
                }),
                Self::goto_stmt(next),
                Self::goto_stmt(s_body),
            ));
            self.states[s_body].stmts.push(Self::assign_stmt(
                &t_loop,
                Self::expr(ExprKind::Member {
                    obj: Box::new(Self::ident_expr(&t_keys)),
                    prop: Box::new(Self::ident_expr(&t_i)),
                    computed: true,
                    optional: false,
                }),
            ));
            self.states[s_body].stmts.push(Self::assign_stmt(
                &t_i,
                Self::expr(ExprKind::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Self::ident_expr(&t_i)),
                    right: Box::new(Self::num_expr(1.0)),
                }),
            ));
            self.goto(s_body, l_body);
            self.compile_for_body(left, &t_loop, l_body, l_head, next, body)?;
        }
        Ok(e_state)
    }

    /// for-in/of 循环体的公共部分：绑定循环变量 → 编译 body。
    fn compile_for_body(
        &mut self,
        left: &ForLeft,
        t_loop: &str,
        l_body: StateId,
        l_head: StateId,
        next: StateId,
        body: &Stmt,
    ) -> Result<(), DesugarError> {
        // l_body: <left> = t_loop; <body>; goto l_head
        let s_bind = self.new_state();
        match left {
            ForLeft::VarDecl { name, .. } => {
                // 已提升；直接赋值。
                self.states[s_bind]
                    .stmts
                    .push(Self::assign_stmt(name, Self::ident_expr(t_loop)));
            }
            ForLeft::Expr(e) => {
                self.states[s_bind].stmts.push(Self::expr_stmt(
                    Spanned::new(
                        e.span,
                        ExprKind::Assign {
                            op: AssignOp::Assign,
                            left: Box::new(e.clone()),
                            right: Box::new(Self::ident_expr(t_loop)),
                        },
                    ),
                ));
            }
        }
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_head,
        });
        let body_entry = self.compile_branch(body, l_head)?;
        self.abrupt.pop();
        self.goto(s_bind, body_entry);
        self.goto(l_body, s_bind);
        Ok(())
    }

    fn compile_try(
        &mut self,
        block: &[Stmt],
        handler: Option<&CatchClause>,
        finalizer: Option<&[Stmt]>,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        if handler.is_none() && finalizer.is_none() {
            return self.compile_stmts(block, next);
        }
        let l_fin = finalizer.map(|_| self.new_state());
        let l_fin_end = finalizer.map(|_| self.new_state());
        let l_catch = handler.map(|_| self.new_state());
        let ex_tmp = EX_TMP.to_string();
        // __yousj$ex 必须声明为持久变量（catch 状态在 wrap 的 catch 参数作用域之外读它）。
        if !self.temps.contains(&ex_tmp) {
            self.temps.push(ex_tmp.clone());
        }
        // 注意：l_fin/l_catch 在 push try ctx 之前创建（它们不在 try 内）。
        let try_ctx = AbruptCtx::Try {
            fin: l_fin,
            catch: l_catch,
            ex_tmp: ex_tmp.clone(),
        };
        self.abrupt.push(try_ctx);
        let body_next = match l_fin {
            Some(f) => self.pending_goto(f, next),
            None => next,
        };
        let body_entry = self.compile_stmts(block, body_next)?;
        self.abrupt.pop();
        // catch
        if let Some(h) = handler {
            let c = l_catch.unwrap();
            let h_next = match l_fin {
                Some(f) => self.pending_goto(f, next),
                None => next,
            };
            let h_entry = self.compile_stmts(&h.body, h_next)?;
            if let Some(p) = &h.param {
                self.states[c]
                    .stmts
                    .push(Self::assign_stmt(p, Self::ident_expr(&ex_tmp)));
            }
            self.goto(c, h_entry);
        }
        // finally
        if let Some(fin_stmts) = finalizer {
            let f = l_fin.unwrap();
            let fe = l_fin_end.unwrap();
            let f_entry = self.compile_stmts(fin_stmts, fe)?;
            self.goto(f, f_entry);
            // l_fin_end：按 pc 分发；嵌套 finally 链式执行。
            // outer_fin：在当前 abrupt 栈（try 已 pop）中找更外层的 finally。
            let mut outer_fin: Option<StateId> = None;
            for ctx in self.abrupt.iter().rev() {
                if let AbruptCtx::Try { fin: Some(of), .. } = ctx {
                    outer_fin = Some(*of);
                    break;
                }
            }
            let pc = Self::ident_expr(PC);
            let pc_type = Self::member_expr(pc.clone(), "type");
            let is_ret = Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(pc_type.clone()),
                right: Box::new(Self::str_expr("ret")),
            });
            let is_throw = Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(pc_type.clone()),
                right: Box::new(Self::str_expr("throw")),
            });
            let is_goto = Self::expr(ExprKind::Binary {
                op: BinaryOp::StrictEq,
                left: Box::new(pc_type),
                right: Box::new(Self::str_expr("goto")),
            });
            // ret 分支：还有外层 finally → 继续；否则完成 {value, done: true}。
            let mut ret_branch = Vec::new();
            match outer_fin {
                Some(of) => {
                    ret_branch.push(Self::assign_stmt(ST, Self::num_expr(of as f64)));
                    ret_branch.push(Self::stmt(StmtKind::Continue));
                }
                None => {
                    ret_branch.push(Self::return_result_stmt(
                        Self::member_expr(pc.clone(), "value"),
                        true,
                    ));
                }
            }
            self.states[fe]
                .stmts
                .push(Self::if_stmt(is_ret, Self::block_stmt(ret_branch), Self::block_stmt(vec![])));
            // throw/goto 分支：还有外层 finally → 继续；否则 throw / 跳 target。
            // goto 还有 pc.fins 链（break/continue 穿过多层 finally）。
            let mut abrupt_branch = Vec::new();
            // if (fins.length > 0) { st = fins.shift(); continue; }
            let fins_len = Self::member_expr(
                Self::member_expr(pc.clone(), "fins"),
                "length",
            );
            // 防御：fins 可能为 undefined（非 goto 的 pc）→ 用条件保护。
            let has_fins = Self::expr(ExprKind::Logical {
                op: LogicalOp::And,
                left: Box::new(Self::member_expr(pc.clone(), "fins")),
                right: Box::new(Self::expr(ExprKind::Binary {
                    op: BinaryOp::Gt,
                    left: Box::new(fins_len),
                    right: Box::new(Self::num_expr(0.0)),
                })),
            });
            let shift_next = Self::assign_stmt(
                ST,
                Self::call_expr(
                    Self::member_expr(Self::member_expr(pc.clone(), "fins"), "shift"),
                    vec![],
                ),
            );
            abrupt_branch.push(Self::if_stmt(
                has_fins,
                Self::block_stmt(vec![shift_next, Self::stmt(StmtKind::Continue)]),
                Self::block_stmt(vec![]),
            ));
            match outer_fin {
                Some(of) => {
                    abrupt_branch.push(Self::assign_stmt(ST, Self::num_expr(of as f64)));
                    abrupt_branch.push(Self::stmt(StmtKind::Continue));
                }
                None => {
                    // throw → 抛出；goto → 跳 target。
                    abrupt_branch.push(Self::if_stmt(
                        is_throw.clone(),
                        Self::throw_stmt(Self::member_expr(pc.clone(), "value")),
                        Self::block_stmt(vec![
                            Self::assign_stmt(
                                ST,
                                Self::member_expr(pc.clone(), "target"),
                            ),
                            Self::stmt(StmtKind::Continue),
                        ]),
                    ));
                }
            }
            self.states[fe].stmts.push(Self::if_stmt(
                Self::expr(ExprKind::Logical {
                    op: LogicalOp::Or,
                    left: Box::new(is_throw),
                    right: Box::new(is_goto),
                }),
                Self::block_stmt(abrupt_branch),
                Self::block_stmt(vec![]),
            ));
            // normal：st = pc.target; continue。
            self.states[fe].stmts.push(Self::block_stmt(vec![
                Self::assign_stmt(ST, Self::member_expr(Self::ident_expr(PC), "target")),
                Self::stmt(StmtKind::Continue),
            ]));
        }
        Ok(body_entry)
    }

    /// 创建"设置 pc={type:normal,target} 后跳 finally"的 pending 状态。
    fn pending_goto(&mut self, fin: StateId, next: StateId) -> StateId {
        let s = self.new_state();
        self.states[s].stmts.push(Self::assign_stmt(
            PC,
            Self::obj_expr(vec![
                ("type", Self::str_expr("normal")),
                ("target", Self::num_expr(next as f64)),
            ]),
        ));
        self.goto(s, fin);
        s
    }

    fn compile_switch(
        &mut self,
        disc: &Expr,
        cases: &[SwitchCase],
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let e_state = self.new_state();
        let (st1, d_simple) = self.split_yield(disc, e_state)?;
        let t_d = self.fresh_tmp();
        // switch 的 discriminant 含 yield 时报编译错误（文档化偏差）。
        // 此处已拆分求值，case 测试仍可能含 yield——在下面检查。
        for c in cases {
            if let Some(t) = &c.test {
                if has_yield_expr(t) {
                    return Err(DesugarError(
                        "yield in switch case test is not supported".to_string(),
                    ));
                }
            }
        }
        self.states[st1]
            .stmts
            .push(Self::assign_stmt(&t_d, d_simple));
        let l_end = self.new_state();
        self.abrupt.push(AbruptCtx::Switch { break_to: l_end });
        // 编译为 if/else 链（fallthrough 语义近似：匹配后顺序执行）。
        // 为简单起见：每个 case 体编译后 goto 下一个 case（fallthrough）。
        let mut case_entries: Vec<StateId> = Vec::new();
        let mut next_case = l_end;
        for c in cases.iter().rev() {
            let body_entry = self.compile_stmts(&c.body, next_case)?;
            case_entries.push(body_entry);
            next_case = body_entry;
        }
        case_entries.reverse();
        // 匹配链：if (t_d === test1) goto case1; else if ...
        let mut else_br: Stmt = Self::goto_stmt(l_end);
        for (c, e) in cases.iter().zip(case_entries.iter()).rev() {
            let test = match &c.test {
                Some(t) => Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(Self::ident_expr(&t_d)),
                    right: Box::new(t.clone()),
                }),
                None => Self::bool_expr(true), // default
            };
            else_br = Self::if_stmt(test, Self::goto_stmt(*e), else_br);
        }
        self.states[st1].stmts.push(else_br);
        self.abrupt.pop();
        self.goto(l_end, next);
        Ok(e_state)
    }

    // ------------------------------------------------------------------
    // 组装
    // ------------------------------------------------------------------

    fn assemble(&mut self, entry: StateId) -> Vec<Stmt> {
        let mut out = Vec::new();
        // 1. 提升的变量声明。
        if !self.var_names.is_empty() {
            out.push(Self::stmt(StmtKind::VarDecl {
                kind: VarKind::Var,
                decls: self
                    .var_names
                    .iter()
                    .map(|n| VarDeclarator {
                        id: n.clone(),
                        init: None,
                    })
                    .collect(),
            }));
        }
        if !self.let_names.is_empty() {
            out.push(Self::stmt(StmtKind::VarDecl {
                kind: VarKind::Let,
                decls: self
                    .let_names
                    .iter()
                    .map(|n| VarDeclarator {
                        id: n.clone(),
                        init: None,
                    })
                    .collect(),
            }));
        }
        // 提升的函数声明。
        for f in std::mem::take(&mut self.fn_decls) {
            out.push(Self::stmt(StmtKind::FunctionDecl(Box::new(f))));
        }
        // 2. arguments 捕获。
        if self.need_args {
            out.push(Self::stmt(StmtKind::VarDecl {
                kind: VarKind::Const,
                decls: vec![VarDeclarator {
                    id: ARGS_TMP.to_string(),
                    init: Some(Self::ident_expr("arguments")),
                }],
            }));
        }
        // 3. 状态机运行时变量。
        let mut state_vars = vec![
            VarDeclarator {
                id: ST.to_string(),
                init: Some(Self::num_expr(0.0)),
            },
            VarDeclarator {
                id: PC.to_string(),
                init: None,
            },
        ];
        for t in std::mem::take(&mut self.temps) {
            state_vars.push(VarDeclarator { id: t, init: None });
        }
        out.push(Self::stmt(StmtKind::VarDecl {
            kind: VarKind::Let,
            decls: state_vars,
        }));
        // 4. __step 函数（普通函数，驱动时传入 this）。
        let cases: Vec<SwitchCase> = self
            .states
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut body = s.stmts.clone();
                if let Some(w) = &s.wrap {
                    body = vec![self.wrap_state(w, body)];
                }
                SwitchCase {
                    test: Some(Self::num_expr(i as f64)),
                    body: vec![Self::block_stmt(body)],
                }
            })
            .collect();
        let step_body = vec![Self::stmt(StmtKind::While {
            test: Self::bool_expr(true),
            body: Box::new(Self::block_stmt(vec![Self::stmt(StmtKind::Switch {
                disc: Self::ident_expr(ST),
                cases,
            })])),
        })];
        let step_fn = FunctionNode {
            id: Some(STEP_FN.to_string()),
            params: vec![
                Param {
                    name: SV.to_string(),
                    default: None,
                },
                Param {
                    name: AB.to_string(),
                    default: None,
                },
                Param {
                    name: ABV.to_string(),
                    default: None,
                },
            ],
            body: step_body,
            is_generator: false,
            is_async: false,
            strict: self.strict,
        };
        out.push(Self::stmt(StmtKind::VarDecl {
            kind: VarKind::Const,
            decls: vec![VarDeclarator {
                id: STEP_FN.to_string(),
                init: Some(Self::expr(ExprKind::Function(Box::new(step_fn)))),
            }],
        }));
        // 5. 入口：__st = <entry>。
        out.push(Self::assign_stmt(ST, Self::num_expr(entry as f64)));
        // 6. 返回 step 闭包（解释器包装成 Generator）。
        out.push(Self::return_stmt(Self::ident_expr(STEP_FN)));
        out
    }

    /// try-body 状态的同步抛错包装。
    fn wrap_state(&self, w: &StateWrap, body: Vec<Stmt>) -> Stmt {
        let action: Vec<Stmt> = match (w.catch_to, w.fin_to) {
            (Some(c), _) => vec![
                // catch 参数 (__yousj$caught) 存入持久变量 (__yousj$ex)。
                Self::assign_stmt(&w.ex_tmp, Self::ident_expr(CATCH_PARAM)),
                Self::goto_stmt(c),
            ],
            (None, Some(f)) => vec![
                Self::assign_stmt(
                    PC,
                    Self::obj_expr(vec![
                        ("type", Self::str_expr("throw")),
                        ("value", Self::ident_expr(CATCH_PARAM)),
                    ]),
                ),
                Self::goto_stmt(f),
            ],
            (None, None) => vec![Self::throw_stmt(Self::ident_expr(CATCH_PARAM))],
        };
        Self::stmt(StmtKind::Try {
            block: body,
            handler: Some(CatchClause {
                param: Some(CATCH_PARAM.to_string()),
                body: action,
            }),
            finalizer: None,
        })
    }
}

// ---------------------------------------------------------------------------
// 纯工具函数
// ---------------------------------------------------------------------------

/// 表达式是否含有 `yield`（不跨函数/类边界）。
fn has_yield_expr(e: &Expr) -> bool {
    match &e.node {
        ExprKind::Yield { .. } => true,
        ExprKind::Function(_) | ExprKind::ArrowFunction(_) | ExprKind::Class(_) => false,
        ExprKind::Literal(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::PrivateName(_) => false,
        ExprKind::Array(elems) => elems.iter().any(|el| match el {
            ArrayElem::Expr(x) => has_yield_expr(x),
            ArrayElem::Hole => false,
            ArrayElem::Spread(x) => has_yield_expr(x),
        }),
        ExprKind::Object(props) => props.iter().any(|p| {
            let key_hit = match &p.key {
                PropKey::Computed(x) => has_yield_expr(x),
                _ => false,
            };
            key_hit
                || match &p.value {
                    PropValue::Init(x) => has_yield_expr(x),
                    PropValue::Shorthand(_) => false,
                    PropValue::Method(_)
                    | PropValue::Getter(_)
                    | PropValue::Setter(_) => false,
                }
        }),
        ExprKind::Unary { arg, .. } => has_yield_expr(arg),
        ExprKind::Update { arg, .. } => has_yield_expr(arg),
        ExprKind::Binary { left, right, .. } => {
            has_yield_expr(left) || has_yield_expr(right)
        }
        ExprKind::Logical { left, right, .. } => {
            has_yield_expr(left) || has_yield_expr(right)
        }
        ExprKind::Assign { left, right, .. } => {
            has_yield_expr(left) || has_yield_expr(right)
        }
        ExprKind::Conditional { test, cons, alt } => {
            has_yield_expr(test) || has_yield_expr(cons) || has_yield_expr(alt)
        }
        ExprKind::Call { callee, args, .. } => {
            has_yield_expr(callee) || args.iter().any(has_yield_expr)
        }
        ExprKind::New { callee, args } => {
            has_yield_expr(callee) || args.iter().any(has_yield_expr)
        }
        ExprKind::Member { obj, prop, .. } => {
            has_yield_expr(obj) || has_yield_expr(prop)
        }
        ExprKind::PrivateMember { obj, .. } => has_yield_expr(obj),
        ExprKind::Sequence(exprs) => exprs.iter().any(has_yield_expr),
        ExprKind::Await(_) => false,
    }
}
