//! yousj-js · Phase 7：async/await 的 AST desugar（状态机改写）。
//!
//! 把 `async function` 体改写为返回 Promise 的普通函数体：
//! - 函数体变成一个 `__yousj$step` 状态机（`while(true)` + `switch(__yousj$st)`）；
//! - 每个 `await` 恰好对应一个 `.then`（tick 语义与规范一致：await 点之前
//!   的求值同步发生，恢复恰好在一个微任务后）；
//! - `break`/`continue`/`return`/`throw` 编译为直接的状态跳转；
//! - `try/catch/finally`：同步抛错由按状态包裹的 `try/catch` 路由，
//!   异步拒绝由 `await` 点的 onRejected 路由，`finally` 经"挂起完成值"
//!  （`__yousj$pc`）统一收口；
//! - 所有 `var/let/const`（不跨函数边界）提升到函数作用域，因为状态
//!   要在多次 `__yousj$step()` 调用之间保持可见。
//!
//! 已知子集偏差（文档化）：
//! - `const` 跨 await 边界后可被重新赋值（提升时按 `let` 处理）；
//! - 块级 `let` 的作用域泄漏到整个函数（`{ let x; } use(x)` 不报错）；
//! - `for (let i...)` 失去每轮新绑定语义（闭包看到同一个 `i`）；
//! - `switch` 的 `case` 测试含 `await`、形参默认值含 `await` 时报编译错误；
//! - 嵌套在块里的函数声明被无条件提升（与本引擎现有 hoist 语义一致）。

use crate::ast::*;

#[derive(Debug, Clone)]
pub struct DesugarError(pub String);

impl std::fmt::Display for DesugarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "async desugar error: {}", self.0)
    }
}

impl std::error::Error for DesugarError {}

// 状态机运行时变量（`__yousj$` 前缀，几乎不可能与用户代码冲突）。
const ST: &str = "__yousj$st";
const STEP_FN: &str = "__yousj$step";
const EX_TMP: &str = "__yousj$ex";
const PC: &str = "__yousj$pc";
const ARGS_TMP: &str = "__yousj$args";
const WRAP_PARAM: &str = "__yousj$ew";
const EPILOGUE_PARAM: &str = "__yousj$err";

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

pub struct AsyncDesugar {
    /// phase 8：外层 async 函数的有效严格模式（生成的状态机箭头继承它）。
    strict: bool,
    states: Vec<State>,
    abrupt: Vec<AbruptCtx>,
    tmp_count: usize,
    /// 函数顶层需要声明的求值暂存（`__yousj$tN`）。
    temps: Vec<String>,
    var_names: Vec<String>,
    let_names: Vec<String>,
    fn_decls: Vec<FunctionNode>,
    need_args: bool,
    prepass_count: usize,
}

// ---------------------------------------------------------------------------
// 纯工具函数（不依赖 self，便于复用与测试）
// ---------------------------------------------------------------------------

/// 表达式是否含有 `await`（不跨函数边界）。
fn has_await_expr(e: &Expr) -> bool {
    match &e.node {
        ExprKind::Await(_) => true,
        ExprKind::Function(_) | ExprKind::ArrowFunction(_) => false,
        ExprKind::Literal(_) | ExprKind::Ident(_) | ExprKind::This => false,
        ExprKind::Array(elems) => elems.iter().any(|el| match el {
            ArrayElem::Expr(x) => has_await_expr(x),
            ArrayElem::Hole => false,
            // phase 9：spread 参数可含 await。
            ArrayElem::Spread(x) => has_await_expr(x),
        }),
        ExprKind::Object(props) => props.iter().any(|p| {
            let key_hit = match &p.key {
                PropKey::Computed(x) => has_await_expr(x),
                _ => false,
            };
            key_hit
                || match &p.value {
                    PropValue::Init(x) => has_await_expr(x),
                    PropValue::Shorthand(_) => false,
                    PropValue::Method(_)
                    | PropValue::Getter(_)
                    | PropValue::Setter(_) => false,
                }
        }),
        ExprKind::Unary { arg, .. } => has_await_expr(arg),
        ExprKind::Update { arg, .. } => has_await_expr(arg),
        ExprKind::Binary { left, right, .. } => {
            has_await_expr(left) || has_await_expr(right)
        }
        ExprKind::Logical { left, right, .. } => {
            has_await_expr(left) || has_await_expr(right)
        }
        ExprKind::Assign { left, right, .. } => {
            has_await_expr(left) || has_await_expr(right)
        }
        ExprKind::Conditional { test, cons, alt } => {
            has_await_expr(test) || has_await_expr(cons) || has_await_expr(alt)
        }
        ExprKind::Call { callee, args, .. } => {
            has_await_expr(callee) || args.iter().any(has_await_expr)
        }
        ExprKind::New { callee, args } => {
            has_await_expr(callee) || args.iter().any(has_await_expr)
        }
        ExprKind::Member { obj, prop, .. } => {
            has_await_expr(obj) || has_await_expr(prop)
        }
        ExprKind::Sequence(exprs) => exprs.iter().any(has_await_expr),
        // phase 9：新表达式变体。Yield 不可能出现在 async 内（parser 拒绝
        // async 生成器）；Class 的方法是函数边界（静态块/字段初始化不允许 await）；
        // Super/PrivateName 不含 await。
        ExprKind::Yield { arg, .. } => arg.as_ref().map_or(false, |a| has_await_expr(a)),
        ExprKind::Super => false,
        ExprKind::Class(_) => false,
        ExprKind::PrivateMember { obj, .. } => has_await_expr(obj),
        ExprKind::PrivateName(_) => false,
    }
}

/// 语句是否含有 `await`（不跨函数边界）。
/// （预留给未来的静态检查；当前由 `check_nested_await` 覆盖。）
#[allow(dead_code)]
fn has_await_stmt(s: &Stmt) -> bool {
    match &s.node {
        StmtKind::Empty | StmtKind::Debugger | StmtKind::Break | StmtKind::Continue => false,
        StmtKind::Expr(e) | StmtKind::Throw(e) => has_await_expr(e),
        StmtKind::Block(b) => b.iter().any(has_await_stmt),
        StmtKind::VarDecl { decls, .. } => {
            decls.iter().any(|d| d.init.as_ref().map_or(false, has_await_expr))
        }
        StmtKind::FunctionDecl(_) => false, // 函数边界（其 body 另行 desugar）
        StmtKind::If { test, cons, alt } => {
            has_await_expr(test)
                || has_await_stmt(cons)
                || alt.as_ref().map_or(false, |a| has_await_stmt(a))
        }
        StmtKind::While { test, body } | StmtKind::DoWhile { test, body } => {
            has_await_expr(test) || has_await_stmt(body)
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            let init_hit = match init {
                Some(ForInit::VarDecl { decls, .. }) => decls
                    .iter()
                    .any(|d| d.init.as_ref().map_or(false, has_await_expr)),
                Some(ForInit::Expr(e)) => has_await_expr(e),
                None => false,
            };
            init_hit
                || test.as_ref().map_or(false, has_await_expr)
                || update.as_ref().map_or(false, has_await_expr)
                || has_await_stmt(body)
        }
        StmtKind::ForInOf { right, body, .. } => {
            has_await_expr(right) || has_await_stmt(body)
        }
        StmtKind::Return(e) => e.as_ref().map_or(false, has_await_expr),
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            block.iter().any(has_await_stmt)
                || handler.as_ref().map_or(false, |h| h.body.iter().any(has_await_stmt))
                || finalizer.as_ref().map_or(false, |f| f.iter().any(has_await_stmt))
        }
        StmtKind::Switch { disc, cases } => {
            has_await_expr(disc)
                || cases.iter().any(|c| {
                    c.test.as_ref().map_or(false, has_await_expr)
                        || c.body.iter().any(has_await_stmt)
                })
        }
        // Phase 7：模块语句（async 函数内不应出现；此处仅做完整性覆盖）。
        StmtKind::Import { .. } | StmtKind::ExportNames(_) => false,
        StmtKind::ExportDecl { decls, .. } => decls
            .iter()
            .any(|d| d.init.as_ref().map_or(false, has_await_expr)),
        StmtKind::ExportFunc(_) => false, // 函数边界
        // phase 9：类声明是函数边界（方法另行 desugar；静态块不允许 await）。
        StmtKind::ClassDecl(_) => false,
    }
}

/// 校验：非 async 函数内的 `await` 是语法错误。
fn check_nested_await(stmts: &[Stmt], in_async: bool) -> Result<(), DesugarError> {
    for s in stmts {
        check_stmt_nested(s, in_async)?;
    }
    Ok(())
}

fn check_stmt_nested(s: &Stmt, in_async: bool) -> Result<(), DesugarError> {
    match &s.node {
        StmtKind::Expr(e) | StmtKind::Throw(e) => check_expr_nested(e, in_async),
        StmtKind::Block(b) => check_nested_await(b, in_async),
        StmtKind::VarDecl { decls, .. } => decls.iter().try_for_each(|d| {
            d.init
                .as_ref()
                .map_or(Ok(()), |e| check_expr_nested(e, in_async))
        }),
        StmtKind::FunctionDecl(f) => check_nested_await(&f.body, f.is_async),
        StmtKind::If { test, cons, alt } => {
            check_expr_nested(test, in_async)?;
            check_stmt_nested(cons, in_async)?;
            if let Some(a) = alt {
                check_stmt_nested(a, in_async)?;
            }
            Ok(())
        }
        StmtKind::While { test, body } | StmtKind::DoWhile { test, body } => {
            check_expr_nested(test, in_async)?;
            check_stmt_nested(body, in_async)
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            if let Some(i) = init {
                match i {
                    ForInit::VarDecl { decls, .. } => decls.iter().try_for_each(|d| {
                        d.init
                            .as_ref()
                            .map_or(Ok(()), |e| check_expr_nested(e, in_async))
                    })?,
                    ForInit::Expr(e) => check_expr_nested(e, in_async)?,
                }
            }
            if let Some(t) = test {
                check_expr_nested(t, in_async)?;
            }
            if let Some(u) = update {
                check_expr_nested(u, in_async)?;
            }
            check_stmt_nested(body, in_async)
        }
        StmtKind::ForInOf { right, body, .. } => {
            check_expr_nested(right, in_async)?;
            check_stmt_nested(body, in_async)
        }
        StmtKind::Return(e) => e
            .as_ref()
            .map_or(Ok(()), |x| check_expr_nested(x, in_async)),
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            check_nested_await(block, in_async)?;
            if let Some(h) = handler {
                check_nested_await(&h.body, in_async)?;
            }
            if let Some(f) = finalizer {
                check_nested_await(f, in_async)?;
            }
            Ok(())
        }
        StmtKind::Switch { disc, cases } => {
            check_expr_nested(disc, in_async)?;
            for c in cases {
                if let Some(t) = &c.test {
                    check_expr_nested(t, in_async)?;
                }
                check_nested_await(&c.body, in_async)?;
            }
            Ok(())
        }
        StmtKind::Empty | StmtKind::Debugger | StmtKind::Break | StmtKind::Continue => Ok(()),
        // Phase 7：模块语句不应出现在函数内。
        StmtKind::Import { .. }
        | StmtKind::ExportDecl { .. }
        | StmtKind::ExportFunc(_)
        | StmtKind::ExportNames(_) => Err(DesugarError(
            "import/export is only allowed at the top level".to_string(),
        )),
        // phase 9：类声明的方法是函数边界（静态块/字段初始化里的 await
        // 按规范非法，这里按非 async 上下文检查）。
        StmtKind::ClassDecl(_) => Ok(()),
    }
}

fn check_expr_nested(e: &Expr, in_async: bool) -> Result<(), DesugarError> {
    match &e.node {
        ExprKind::Await(inner) => {
            if !in_async {
                return Err(DesugarError(
                    "await is only valid in async functions".to_string(),
                ));
            }
            check_expr_nested(inner, in_async)
        }
        ExprKind::Function(f) => check_nested_await(&f.body, f.is_async),
        ExprKind::ArrowFunction(a) => match &a.body {
            ArrowBody::Expr(x) => check_expr_nested(x, a.is_async),
            ArrowBody::Block(b) => check_nested_await(b, a.is_async),
        },
        _ => {
            // 通用递归：复用 has_await 的结构太啰嗦，这里用"含 await 即错"
            // 的保守检查——只要子树含 await 且 !in_async 就报错。
            if !in_async && has_await_expr(e) {
                return Err(DesugarError(
                    "await is only valid in async functions".to_string(),
                ));
            }
            // in_async 时：子函数边界已在上面处理，直接放行（深层检查
            // 由各函数的 desugar 负责）。
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

impl AsyncDesugar {
    pub fn new() -> Self {
        AsyncDesugar {
            strict: false,
            states: Vec::new(),
            abrupt: Vec::new(),
            tmp_count: 0,
            temps: Vec::new(),
            var_names: Vec::new(),
            let_names: Vec::new(),
            fn_decls: Vec::new(),
            need_args: false,
            prepass_count: 0,
        }
    }

    /// phase 8：设置外层严格模式（生成的内部箭头函数继承）。
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// 改写 async 函数体 → 新函数体（同步执行返回 Promise）。
    pub fn desugar(
        &mut self,
        params: &[Param],
        body: &[Stmt],
    ) -> Result<Vec<Stmt>, DesugarError> {
        // 1. 形参默认值不允许 await。
        for p in params {
            if let Some(d) = &p.default {
                if has_await_expr(d) {
                    return Err(DesugarError(
                        "await in parameter default is not supported".to_string(),
                    ));
                }
            }
        }
        // 2. 嵌套校验。
        check_nested_await(body, true)?;
        // 3. for-in/of 预改写。
        let body = self.prepass_stmts(body)?;
        // 4. 变量提升 + arguments 改写。
        let mut hoisted = Vec::new();
        for s in &body {
            self.hoist_stmt(s, &mut hoisted)?;
        }
        // 5. 状态机编译。
        let terminal = self.new_state();
        self.states[terminal]
            .stmts
            .push(Self::return_stmt(Self::ident_expr("undefined")));
        let entry = self.compile_stmts(&hoisted, terminal)?;
        // 6. 组装函数体。
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

    fn fresh_tmp(&mut self) -> String {
        let n = self.tmp_count;
        self.tmp_count += 1;
        let name = format!("__yousj$t{}", n);
        self.temps.push(name.clone());
        name
    }

    fn goto(&mut self, from: StateId, to: StateId) {
        let s = Self::goto_stmt(to);
        self.states[from].stmts.push(s);
    }

    // ------------------------------------------------------------------
    // AST 构造小件
    // ------------------------------------------------------------------

    fn sp() -> Span {
        Span::point(0, 0)
    }

    fn expr( kind: ExprKind) -> Expr {
        Spanned::new(Self::sp(), kind)
    }

    fn stmt( kind: StmtKind) -> Stmt {
        Spanned::new(Self::sp(), kind)
    }

    fn ident_expr( name: &str) -> Expr {
        Self::expr(ExprKind::Ident(name.to_string()))
    }

    fn num_expr( n: f64) -> Expr {
        Self::expr(ExprKind::Literal(Literal::Number(n)))
    }

    fn str_expr( s: &str) -> Expr {
        Self::expr(ExprKind::Literal(Literal::String(s.to_string())))
    }

    fn bool_expr( b: bool) -> Expr {
        Self::expr(ExprKind::Literal(Literal::Bool(b)))
    }

    fn expr_stmt( e: Expr) -> Stmt {
        Self::stmt(StmtKind::Expr(e))
    }

    fn return_stmt( e: Expr) -> Stmt {
        Self::stmt(StmtKind::Return(Some(e)))
    }

    fn throw_stmt( e: Expr) -> Stmt {
        Self::stmt(StmtKind::Throw(e))
    }

    fn block_stmt( stmts: Vec<Stmt>) -> Stmt {
        Self::stmt(StmtKind::Block(stmts))
    }

    fn if_stmt( test: Expr, cons: Stmt, alt: Stmt) -> Stmt {
        Self::stmt(StmtKind::If {
            test,
            cons: Box::new(cons),
            alt: Some(Box::new(alt)),
        })
    }

    fn assign_expr( left: Expr, right: Expr) -> Expr {
        Self::expr(ExprKind::Assign {
            op: AssignOp::Assign,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn assign_stmt( name: &str, e: Expr) -> Stmt {
        Self::expr_stmt(Self::assign_expr(Self::ident_expr(name), e))
    }

    fn goto_stmt( to: StateId) -> Stmt {
        Self::block_stmt(vec![
            Self::assign_stmt(ST, Self::num_expr(to as f64)),
            Self::stmt(StmtKind::Continue),
        ])
    }

    fn call_expr( callee: Expr, args: Vec<Expr>) -> Expr {
        Self::expr(ExprKind::Call {
            callee: Box::new(callee),
            args,
            optional: false,
        })
    }

    fn member_expr( obj: Expr, prop: &str) -> Expr {
        Self::expr(ExprKind::Member {
            obj: Box::new(obj),
            prop: Box::new(Self::str_expr(prop)),
            computed: false,
            optional: false,
        })
    }

    fn member_idx( obj: Expr, idx: Expr) -> Expr {
        Self::expr(ExprKind::Member {
            obj: Box::new(obj),
            prop: Box::new(idx),
            computed: true,
            optional: false,
        })
    }

    fn obj_expr( props: Vec<(&str, Expr)>) -> Expr {
        Self::expr(ExprKind::Object(
            props
                .into_iter()
                .map(|(k, v)| Prop {
                    key: PropKey::Ident(k.to_string()),
                    value: PropValue::Init(v),
                })
                .collect(),
        ))
    }

    // ------------------------------------------------------------------
    // for-in/of 预改写（→ while）
    // ------------------------------------------------------------------

    fn prepass_stmts(&mut self, stmts: &[Stmt]) -> Result<Vec<Stmt>, DesugarError> {
        let mut out = Vec::new();
        for s in stmts {
            let mut r = self.prepass_stmt(s)?;
            out.append(&mut r);
        }
        Ok(out)
    }

    fn prepass_stmt(&mut self, s: &Stmt) -> Result<Vec<Stmt>, DesugarError> {
        match &s.node {
            StmtKind::ForInOf {
                is_of,
                left,
                right,
                body,
            } => {
                let n = self.prepass_count;
                self.prepass_count += 1;
                let src = format!("__yousj$src{}", n);
                let idx = format!("__yousj$i{}", n);
                let len = format!("__yousj$n{}", n);
                let mut out = Vec::new();
                if *is_of {
                    // const src = right;
                    out.push(Self::var_decl_stmt(VarKind::Const, &src, Some(right.clone())));
                    // if (!Array.isArray(src) && typeof src !== "string") throw "...";
                    let not_arr = Self::expr(ExprKind::Unary {
                        op: UnaryOp::Not,
                        arg: Box::new(Self::call_expr(
                            Self::member_expr(Self::ident_expr("Array"), "isArray"),
                            vec![Self::ident_expr(&src)],
                        )),
                    });
                    let not_str = Self::expr(ExprKind::Binary {
                        op: BinaryOp::StrictNe,
                        left: Box::new(Self::expr(ExprKind::Unary {
                            op: UnaryOp::Typeof,
                            arg: Box::new(Self::ident_expr(&src)),
                        })),
                        right: Box::new(Self::str_expr("string")),
                    });
                    let test = Self::expr(ExprKind::Logical {
                        op: LogicalOp::And,
                        left: Box::new(not_arr),
                        right: Box::new(not_str),
                    });
                    out.push(Self::if_stmt(
                        test,
                        Self::throw_stmt(Self::str_expr("for-of: not iterable")),
                        Self::block_stmt(vec![]),
                    ));
                } else {
                    // const src = Object.keys(right);
                    out.push(Self::var_decl_stmt(
                        VarKind::Const,
                        &src,
                        Some(Self::call_expr(
                            Self::member_expr(Self::ident_expr("Object"), "keys"),
                            vec![right.clone()],
                        )),
                    ));
                }
                out.push(Self::var_decl_stmt(VarKind::Let, &idx, Some(Self::num_expr(0.0))));
                out.push(Self::var_decl_stmt(
                    VarKind::Const,
                    &len,
                    Some(Self::member_expr(Self::ident_expr(&src), "length")),
                ));
                // 绑定语句
                let elem = Self::member_idx(Self::ident_expr(&src), Self::ident_expr(&idx));
                let bind: Stmt = match left {
                    ForLeft::VarDecl { kind, name } => {
                        Self::var_decl_stmt(*kind, name, Some(elem))
                    }
                    ForLeft::Expr(e) => Self::expr_stmt(Self::assign_expr(e.clone(), elem)),
                };
                let mut loop_body = vec![
                    bind,
                    Self::expr_stmt(Self::assign_expr(
                        Self::ident_expr(&idx),
                        Self::expr(ExprKind::Binary {
                            op: BinaryOp::Add,
                            left: Box::new(Self::ident_expr(&idx)),
                            right: Box::new(Self::num_expr(1.0)),
                        }),
                    )),
                ];
                match &body.node {
                    StmtKind::Block(b) => {
                        for x in b {
                            loop_body.extend(self.prepass_stmt(x)?);
                        }
                    }
                    _ => loop_body.extend(self.prepass_stmt(body)?),
                }
                out.push(Self::stmt(StmtKind::While {
                    test: Self::expr(ExprKind::Binary {
                        op: BinaryOp::Lt,
                        left: Box::new(Self::ident_expr(&idx)),
                        right: Box::new(Self::ident_expr(&len)),
                    }),
                    body: Box::new(Self::block_stmt(loop_body)),
                }));
                Ok(vec![Self::block_stmt(out)])
            }
            // 其他语句：递归进子结构（不跨函数边界）。
            _ => {
                let mut out = Vec::new();
                self.prepass_recurse(s, &mut out)?;
                Ok(out)
            }
        }
    }

    fn prepass_recurse(&mut self, s: &Stmt, out: &mut Vec<Stmt>) -> Result<(), DesugarError> {
        match &s.node {
            StmtKind::Block(b) => out.push(Self::block_stmt(self.prepass_stmts(b)?)),
            StmtKind::If { test, cons, alt } => {
                // if 的分支保持语句形态（prepass 只改 for-in/of，
                // 单条语句的改写结果包回 Block 即可）。
                let cons_b = Self::block_stmt(self.prepass_one_to_block(cons)?);
                let alt_b = match alt {
                    Some(a) => Some(Box::new(Self::block_stmt(self.prepass_one_to_block(a)?))),
                    None => None,
                };
                out.push(Self::stmt(StmtKind::If {
                    test: test.clone(),
                    cons: Box::new(cons_b),
                    alt: alt_b,
                }));
            }
            StmtKind::While { test, body } => out.push(Self::stmt(StmtKind::While {
                test: test.clone(),
                body: Box::new(Self::block_stmt(self.prepass_one_to_block(body)?)),
            })),
            StmtKind::DoWhile { body, test } => out.push(Self::stmt(StmtKind::DoWhile {
                body: Box::new(Self::block_stmt(self.prepass_one_to_block(body)?)),
                test: test.clone(),
            })),
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => out.push(Self::stmt(StmtKind::For {
                init: init.clone(),
                test: test.clone(),
                update: update.clone(),
                body: Box::new(Self::block_stmt(self.prepass_one_to_block(body)?)),
            })),
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => out.push(Self::stmt(StmtKind::Try {
                block: self.prepass_stmts(block)?,
                handler: match handler {
                    Some(h) => Some(CatchClause {
                        param: h.param.clone(),
                        body: self.prepass_stmts(&h.body)?,
                    }),
                    None => None,
                },
                finalizer: match finalizer {
                    Some(f) => Some(self.prepass_stmts(f)?),
                    None => None,
                },
            })),
            StmtKind::Switch { disc, cases } => {
                let mut cases2 = Vec::new();
                for c in cases {
                    cases2.push(SwitchCase {
                        test: c.test.clone(),
                        body: self.prepass_stmts(&c.body)?,
                    });
                }
                out.push(Self::stmt(StmtKind::Switch {
                    disc: disc.clone(),
                    cases: cases2,
                }));
            }
            // 其余（FunctionDecl 含函数边界，不进）原样保留。
            _ => out.push(s.clone()),
        }
        Ok(())
    }

    fn prepass_one_to_block(&mut self, s: &Stmt) -> Result<Vec<Stmt>, DesugarError> {
        match &s.node {
            StmtKind::Block(b) => self.prepass_stmts(b),
            _ => self.prepass_stmt(s),
        }
    }

    fn var_decl_stmt( kind: VarKind, name: &str, init: Option<Expr>) -> Stmt {
        Self::stmt(StmtKind::VarDecl {
            kind,
            decls: vec![VarDeclarator {
                id: name.to_string(),
                init,
            }],
        })
    }

    // ------------------------------------------------------------------
    // 变量提升 + arguments 改写
    // ------------------------------------------------------------------

    fn declare_name(&mut self, kind: VarKind, name: &str) {
        match kind {
            VarKind::Var => {
                if !self.var_names.contains(&name.to_string()) {
                    self.var_names.push(name.to_string());
                }
            }
            VarKind::Let | VarKind::Const => {
                if !self.let_names.contains(&name.to_string())
                    && !self.var_names.contains(&name.to_string())
                {
                    self.let_names.push(name.to_string());
                }
            }
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
                // 与引擎现有 hoist 语义一致：无条件提升。
                self.fn_decls.push(f.as_ref().clone());
            }
            // phase 9：类声明——类名按 let 提升（TDZ），类体是函数边界。
            // 求值语句保留在流中（compile 阶段按普通表达式语句处理）。
            StmtKind::ClassDecl(c) => {
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
                let cons2 = self.hoist_one_to_block(cons)?;
                let alt2 = match alt {
                    Some(a) => Some(Box::new(self.hoist_one_to_block(a)?)),
                    None => None,
                };
                out.push(Self::stmt(StmtKind::If {
                    test: self.hoist_expr(test),
                    cons: Box::new(cons2),
                    alt: alt2,
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
            StmtKind::ForInOf { .. } => {
                // 预改写已消除；防御性报错。
                return Err(DesugarError(
                    "internal: ForInOf survived prepass".to_string(),
                ));
            }
            StmtKind::Return(e) => out.push(
                Self::stmt(StmtKind::Return(e.as_ref().map(|x| self.hoist_expr(x)))),
            ),
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
                            // catch 参数也提升（跨 await 可见）。
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
            // Phase 7：模块语句不应出现在函数内（check 阶段已报错；防御）。
            StmtKind::Import { .. }
            | StmtKind::ExportDecl { .. }
            | StmtKind::ExportFunc(_)
            | StmtKind::ExportNames(_) => {
                return Err(DesugarError(
                    "import/export is only allowed at the top level".to_string(),
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

    fn seq_or_single( mut exprs: Vec<Expr>) -> Expr {
        if exprs.len() == 1 {
            exprs.pop().unwrap()
        } else {
            Self::expr(ExprKind::Sequence(exprs))
        }
    }

    /// 表达式改写：`arguments` → 提升暂存（不跨函数边界）。
    fn hoist_expr(&mut self, e: &Expr) -> Expr {
        let kind = match &e.node {
            ExprKind::Ident(name) if name == "arguments" => {
                self.need_args = true;
                ExprKind::Ident(ARGS_TMP.to_string())
            }
            ExprKind::Function(_) | ExprKind::ArrowFunction(_) => return e.clone(),
            ExprKind::Array(elems) => ExprKind::Array(
                elems
                    .iter()
                    .map(|el| match el {
                        ArrayElem::Expr(x) => ArrayElem::Expr(self.hoist_expr(x)),
                        ArrayElem::Hole => ArrayElem::Hole,
                        // phase 9
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
            ExprKind::Await(inner) => ExprKind::Await(Box::new(self.hoist_expr(inner))),
            _ => return e.clone(),
        };
        Self::expr(kind)
    }

    // ------------------------------------------------------------------
    // 状态机编译
    // ------------------------------------------------------------------

    /// 倒序编译语句序列；返回入口状态。
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

    /// 编译单条语句；`next` 为正常完成后的入口；返回本语句的入口。
    fn compile_stmt(&mut self, s: &Stmt, next: StateId) -> Result<StateId, DesugarError> {
        match &s.node {
            StmtKind::Empty | StmtKind::Debugger => {
                let e = self.new_state();
                self.goto(e, next);
                Ok(e)
            }
            StmtKind::Expr(e) => {
                let e_state = self.new_state();
                let target = self.fresh_tmp();
                self.compile_expr_to(e, &target, e_state, next)?;
                Ok(e_state)
            }
            StmtKind::Block(b) => self.compile_stmts(b, next),
            StmtKind::VarDecl { .. } => {
                // 提升阶段已消除；防御。
                Err(DesugarError("internal: VarDecl survived hoisting".to_string()))
            }
            StmtKind::FunctionDecl(_) => {
                // 已提升到顶层；此处无操作。
                let e = self.new_state();
                self.goto(e, next);
                Ok(e)
            }
            StmtKind::Return(e) => self.compile_return(e.as_ref(), next),
            StmtKind::Throw(e) => self.compile_throw(e, next),
            StmtKind::Break => Ok(self.compile_break(false, next)),
            StmtKind::Continue => Ok(self.compile_break(true, next)),
            StmtKind::If { test, cons, alt } => self.compile_if(test, cons, alt.as_deref(), next),
            StmtKind::While { test, body } => self.compile_while(test, body, next),
            StmtKind::DoWhile { body, test } => self.compile_do_while(body, test, next),
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => self.compile_for(init.as_ref(), test.as_ref(), update.as_ref(), body, next),
            StmtKind::ForInOf { .. } => Err(DesugarError(
                "internal: ForInOf survived prepass".to_string(),
            )),
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_ref(), finalizer.as_deref(), next),
            StmtKind::Switch { disc, cases } => self.compile_switch(disc, cases, next),
            // Phase 7：模块语句不应出现在函数内（check 阶段已报错；防御）。
            StmtKind::Import { .. }
            | StmtKind::ExportDecl { .. }
            | StmtKind::ExportFunc(_)
            | StmtKind::ExportNames(_) => Err(DesugarError(
                "import/export is only allowed at the top level".to_string(),
            )),
            // phase 9：类声明在 hoist 阶段已转为赋值；防御。
            StmtKind::ClassDecl(_) => Err(DesugarError(
                "internal: ClassDecl survived hoisting".to_string(),
            )),
        }
    }

    fn compile_branch(&mut self, s: &Stmt, next: StateId) -> Result<StateId, DesugarError> {
        match &s.node {
            StmtKind::Block(b) => self.compile_stmts(b, next),
            _ => self.compile_stmt(s, next),
        }
    }

    /// 求值 `e`，结果存入 `target`，然后 goto `next`。
    /// `await` 按求值顺序拆成 then 链（每个 await 恰好一个微任务）。
    fn compile_expr_to(
        &mut self,
        e: &Expr,
        target: &str,
        cur: StateId,
        next: StateId,
    ) -> Result<(), DesugarError> {
        match self.split_first_await(e)? {
            None => {
                self.states[cur]
                    .stmts
                    .push(Self::assign_stmt(target, e.clone()));
                self.goto(cur, next);
            }
            Some((operand, pattern, tmp)) => {
                let mid = self.new_state();
                // mid：target = <pattern>；pattern 里还可能有 await → 递归。
                self.compile_expr_to(&pattern, target, mid, next)?;
                self.emit_await(cur, &operand, &tmp, mid)?;
            }
        }
        Ok(())
    }

    /// 在 `cur` 状态发射一个 await 点：
    /// `return Promise.resolve(<operand>).then(v => { <tmp> = v; __st = <mid>; return __step(); }[, onRejected]);`
    fn emit_await(
        &mut self,
        cur: StateId,
        operand: &Expr,
        tmp: &str,
        mid: StateId,
    ) -> Result<(), DesugarError> {
        // 拒绝路由：最近的 try。
        let mut route: Option<(String, Option<StateId>, Option<StateId>)> = None;
        for ctx in self.abrupt.iter().rev() {
            if let AbruptCtx::Try { fin, catch, ex_tmp } = ctx {
                route = Some((ex_tmp.clone(), *catch, *fin));
                break;
            }
        }
        // onFulfilled：__v => { <tmp> = __v; __st = mid; return __step(); }
        let v_param = "__yousj$v";
        let on_f = Self::expr(ExprKind::ArrowFunction(Box::new(ArrowFunction {
            params: vec![Param {
                name: v_param.to_string(),
                default: None,
            }],
            body: ArrowBody::Block(vec![
                Self::assign_stmt(tmp, Self::ident_expr(v_param)),
                Self::assign_stmt(ST, Self::num_expr(mid as f64)),
                Self::return_stmt(Self::call_expr(Self::ident_expr(STEP_FN), vec![])),
            ]),
            is_async: false,
            strict: self.strict,
        })));
        let mut args = vec![on_f];
        if let Some((ex_tmp, catch, fin)) = route {
            let e_param = "__yousj$e";
            let body: Vec<Stmt> = match (catch, fin) {
                (Some(c), _) => vec![
                    Self::assign_stmt(&ex_tmp, Self::ident_expr(e_param)),
                    Self::assign_stmt(ST, Self::num_expr(c as f64)),
                    Self::return_stmt(Self::call_expr(Self::ident_expr(STEP_FN), vec![])),
                ],
                (None, Some(f)) => vec![
                    Self::assign_stmt(
                        PC,
                        Self::obj_expr(vec![
                            ("type", Self::str_expr("throw")),
                            ("value", Self::ident_expr(e_param)),
                        ]),
                    ),
                    Self::assign_stmt(ST, Self::num_expr(f as f64)),
                    Self::return_stmt(Self::call_expr(Self::ident_expr(STEP_FN), vec![])),
                ],
                (None, None) => {
                    return Err(DesugarError(
                        "internal: try without handler".to_string(),
                    ))
                }
            };
            args.push(Self::expr(ExprKind::ArrowFunction(Box::new(ArrowFunction {
                params: vec![Param {
                    name: e_param.to_string(),
                    default: None,
                }],
                body: ArrowBody::Block(body),
                is_async: false,
                strict: self.strict,
            }))));
        }
        let promise_resolve = Self::call_expr(
            Self::member_expr(Self::ident_expr("Promise"), "resolve"),
            vec![operand.clone()],
        );
        let then_call = Self::call_expr(
            Self::member_expr(promise_resolve, "then"),
            args,
        );
        self.states[cur].stmts.push(Self::return_stmt(then_call));
        Ok(())
    }

    /// 按求值顺序找第一个 `await`，返回 (操作数, 模板, 暂存名)。
    /// 模板 = 原表达式把该 await 替换为 `Ident(暂存名)` 后的样子。
    fn split_first_await(
        &mut self,
        e: &Expr,
    ) -> Result<Option<(Expr, Expr, String)>, DesugarError> {
        let span = e.span;
        match &e.node {
            ExprKind::Await(inner) => {
                if let Some((op, pat, tmp)) = self.split_first_await(inner)? {
                    let rebuilt = Spanned::new(
                        span,
                        ExprKind::Await(Box::new(pat)),
                    );
                    return Ok(Some((op, rebuilt, tmp)));
                }
                let tmp = self.fresh_tmp();
                Ok(Some((
                    inner.as_ref().clone(),
                    Spanned::new(span, ExprKind::Ident(tmp.clone())),
                    tmp,
                )))
            }
            ExprKind::Function(_) | ExprKind::ArrowFunction(_) => Ok(None),
            ExprKind::Unary { op, arg } => self.split_unary(*op, arg, span, |op, a| {
                Spanned::new(
                    span,
                    ExprKind::Unary {
                        op,
                        arg: Box::new(a),
                    },
                )
            }),
            _ => self.split_first_await_generic(e),
        }
    }

    /// 单操作数表达式的通用拆分（Unary 形状）。
    fn split_unary(
        &mut self,
        op: UnaryOp,
        arg: &Expr,
        _span: Span,
        rebuild: impl Fn(UnaryOp, Expr) -> Expr,
    ) -> Result<Option<(Expr, Expr, String)>, DesugarError> {
        if let Some((o, p, t)) = self.split_first_await(arg)? {
            Ok(Some((o, rebuild(op, p), t)))
        } else {
            Ok(None)
        }
    }

    /// 其余表达式形状的拆分。
    fn split_first_await_generic(
        &mut self,
        e: &Expr,
    ) -> Result<Option<(Expr, Expr, String)>, DesugarError> {
        let span = e.span;
        // 辅助：依次尝试子表达式。
        macro_rules! try_each {
            ($($sub:expr => $rebuild:expr),* $(,)?) => {{
                $(
                    if let Some((o, p, t)) = self.split_first_await($sub)? {
                        return Ok(Some((o, $rebuild(p), t)));
                    }
                )*
                Ok(None)
            }};
        }
        match &e.node {
            ExprKind::Update { op, arg, prefix } => {
                let (op, prefix) = (*op, *prefix);
                try_each!(
                    arg => |p: Expr| Spanned::new(span, ExprKind::Update { op, arg: Box::new(p), prefix }),
                )
            }
            ExprKind::Binary { op, left, right } => {
                let op = *op;
                let l = left.as_ref().clone();
                let r = right.as_ref().clone();
                try_each!(
                    left => |p: Expr| Spanned::new(span, ExprKind::Binary { op, left: Box::new(p), right: Box::new(r.clone()) }),
                    right => |p: Expr| Spanned::new(span, ExprKind::Binary { op, left: Box::new(l.clone()), right: Box::new(p) }),
                )
            }
            ExprKind::Logical { op, left, right } => {
                let op = *op;
                let l = left.as_ref().clone();
                let r = right.as_ref().clone();
                try_each!(
                    left => |p: Expr| Spanned::new(span, ExprKind::Logical { op, left: Box::new(p), right: Box::new(r.clone()) }),
                    right => |p: Expr| Spanned::new(span, ExprKind::Logical { op, left: Box::new(l.clone()), right: Box::new(p) }),
                )
            }
            ExprKind::Assign { op, left, right } => {
                let op = *op;
                let l = left.as_ref().clone();
                let r = right.as_ref().clone();
                try_each!(
                    left => |p: Expr| Spanned::new(span, ExprKind::Assign { op, left: Box::new(p), right: Box::new(r.clone()) }),
                    right => |p: Expr| Spanned::new(span, ExprKind::Assign { op, left: Box::new(l.clone()), right: Box::new(p) }),
                )
            }
            ExprKind::Conditional { test, cons, alt } => {
                let c = cons.as_ref().clone();
                let a = alt.as_ref().clone();
                let t = test.as_ref().clone();
                try_each!(
                    test => |p: Expr| Spanned::new(span, ExprKind::Conditional { test: Box::new(p), cons: Box::new(c.clone()), alt: Box::new(a.clone()) }),
                    cons => |p: Expr| Spanned::new(span, ExprKind::Conditional { test: Box::new(t.clone()), cons: Box::new(p), alt: Box::new(a.clone()) }),
                    alt => |p: Expr| Spanned::new(span, ExprKind::Conditional { test: Box::new(t.clone()), cons: Box::new(c.clone()), alt: Box::new(p) }),
                )
            }
            ExprKind::Call {
                callee,
                args,
                optional,
            } => {
                let optional = *optional;
                let cal = callee.as_ref().clone();
                if let Some((o, p, t)) = self.split_first_await(callee)? {
                    return Ok(Some((
                        o,
                        Spanned::new(
                            span,
                            ExprKind::Call {
                                callee: Box::new(p),
                                args: args.clone(),
                                optional,
                            },
                        ),
                        t,
                    )));
                }
                for (i, arg) in args.iter().enumerate() {
                    if let Some((o, p, t)) = self.split_first_await(arg)? {
                        let mut args2 = args.clone();
                        args2[i] = p;
                        return Ok(Some((
                            o,
                            Spanned::new(
                                span,
                                ExprKind::Call {
                                    callee: Box::new(cal.clone()),
                                    args: args2,
                                    optional,
                                },
                            ),
                            t,
                        )));
                    }
                }
                Ok(None)
            }
            ExprKind::New { callee, args } => {
                let cal = callee.as_ref().clone();
                if let Some((o, p, t)) = self.split_first_await(callee)? {
                    return Ok(Some((
                        o,
                        Spanned::new(
                            span,
                            ExprKind::New {
                                callee: Box::new(p),
                                args: args.clone(),
                            },
                        ),
                        t,
                    )));
                }
                for (i, arg) in args.iter().enumerate() {
                    if let Some((o, p, t)) = self.split_first_await(arg)? {
                        let mut args2 = args.clone();
                        args2[i] = p;
                        return Ok(Some((
                            o,
                            Spanned::new(
                                span,
                                ExprKind::New {
                                    callee: Box::new(cal.clone()),
                                    args: args2,
                                },
                            ),
                            t,
                        )));
                    }
                }
                Ok(None)
            }
            ExprKind::Member {
                obj,
                prop,
                computed,
                optional,
            } => {
                let computed = *computed;
                let optional = *optional;
                let pr = prop.as_ref().clone();
                if let Some((o, p, t)) = self.split_first_await(obj)? {
                    return Ok(Some((
                        o,
                        Spanned::new(
                            span,
                            ExprKind::Member {
                                obj: Box::new(p),
                                prop: Box::new(pr),
                                computed,
                                optional,
                            },
                        ),
                        t,
                    )));
                }
                if computed {
                    if let Some((o, p, t)) = self.split_first_await(prop)? {
                        return Ok(Some((
                            o,
                            Spanned::new(
                                span,
                                ExprKind::Member {
                                    obj: Box::new(obj.as_ref().clone()),
                                    prop: Box::new(p),
                                    computed,
                                    optional,
                                },
                            ),
                            t,
                        )));
                    }
                }
                Ok(None)
            }
            ExprKind::Sequence(exprs) => {
                for (i, x) in exprs.iter().enumerate() {
                    if let Some((o, p, t)) = self.split_first_await(x)? {
                        let mut e2 = exprs.clone();
                        e2[i] = p;
                        return Ok(Some((
                            o,
                            Spanned::new(span, ExprKind::Sequence(e2)),
                            t,
                        )));
                    }
                }
                Ok(None)
            }
            ExprKind::Array(elems) => {
                for (i, el) in elems.iter().enumerate() {
                    let x = match el {
                        ArrayElem::Expr(x) => x,
                        // phase 9：spread 参数同样可含 await。
                        ArrayElem::Spread(x) => x,
                        ArrayElem::Hole => continue,
                    };
                    if let Some((o, p, t)) = self.split_first_await(x)? {
                        let mut e2 = elems.clone();
                        e2[i] = match el {
                            ArrayElem::Expr(_) => ArrayElem::Expr(p),
                            ArrayElem::Spread(_) => ArrayElem::Spread(p),
                            ArrayElem::Hole => ArrayElem::Hole,
                        };
                        return Ok(Some((
                            o,
                            Spanned::new(span, ExprKind::Array(e2)),
                            t,
                        )));
                    }
                }
                Ok(None)
            }
            ExprKind::Object(props) => {
                for (i, p) in props.iter().enumerate() {
                    let key_hit = match &p.key {
                        PropKey::Computed(x) => self.split_first_await(x)?,
                        _ => None,
                    };
                    if let Some((o, kp, t)) = key_hit {
                        let mut p2 = props.clone();
                        p2[i].key = PropKey::Computed(kp);
                        return Ok(Some((
                            o,
                            Spanned::new(span, ExprKind::Object(p2)),
                            t,
                        )));
                    }
                    if let PropValue::Init(x) = &p.value {
                        if let Some((o, vp, t)) = self.split_first_await(x)? {
                            let mut p2 = props.clone();
                            p2[i].value = PropValue::Init(vp);
                            return Ok(Some((
                                o,
                                Spanned::new(span, ExprKind::Object(p2)),
                                t,
                            )));
                        }
                    }
                }
                Ok(None)
            }
            // phase 9：`obj.#x` 的 obj 可含 await；Yield/Class/Super/PrivateName
            // 在 async 内不可能含 await（Class 是函数边界），走默认 None。
            ExprKind::PrivateMember { obj, name, optional } => {
                let (name, optional) = (name.clone(), *optional);
                try_each!(
                    obj => |p: Expr| Spanned::new(span, ExprKind::PrivateMember { obj: Box::new(p), name: name.clone(), optional }),
                )
            }
            ExprKind::Yield { arg, delegate } => {
                // async 内不可能出现 yield（parser 拒绝 async 生成器）；防御性处理。
                if let Some(a) = arg {
                    let delegate = *delegate;
                    if let Some((o, p, t)) = self.split_first_await(a)? {
                        return Ok(Some((
                            o,
                            Spanned::new(
                                span,
                                ExprKind::Yield {
                                    arg: Some(Box::new(p)),
                                    delegate,
                                },
                            ),
                            t,
                        )));
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // 控制流编译
    // ------------------------------------------------------------------

    fn compile_return(
        &mut self,
        e: Option<&Expr>,
        _next: StateId,
    ) -> Result<StateId, DesugarError> {
        let tmp = self.fresh_tmp();
        let mid = self.new_state();
        // mid：按 abrupt 栈决定是直接返回还是经 finally。
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
                self.states[mid]
                    .stmts
                    .push(Self::return_stmt(Self::ident_expr(&tmp)));
            }
        }
        let entry = self.new_state();
        let e = e.cloned().unwrap_or_else(|| Self::ident_expr("undefined"));
        self.compile_expr_to(&e, &tmp, entry, mid)?;
        Ok(entry)
    }

    fn compile_throw(&mut self, e: &Expr, _next: StateId) -> Result<StateId, DesugarError> {
        // 同步 throw 由状态的 try/catch 包装路由（catch/finally 统一处理）。
        let tmp = self.fresh_tmp();
        let mid = self.new_state();
        self.states[mid]
            .stmts
            .push(Self::throw_stmt(Self::ident_expr(&tmp)));
        let entry = self.new_state();
        self.compile_expr_to(e, &tmp, entry, mid)?;
        Ok(entry)
    }

    /// `is_continue` 区分 break / continue。
    fn compile_break(&mut self, is_continue: bool, _next: StateId) -> StateId {
        let mut fin = None;
        let mut target = None;
        for ctx in self.abrupt.iter().rev() {
            match ctx {
                AbruptCtx::Try { fin: Some(f), .. } if fin.is_none() => fin = Some(*f),
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
        match (target, fin) {
            (Some(t), Some(f)) => {
                self.states[e].stmts.push(Self::assign_stmt(
                    PC,
                    Self::obj_expr(vec![
                        ("type", Self::str_expr("goto")),
                        ("target", Self::num_expr(t as f64)),
                    ]),
                ));
                self.goto(e, f);
            }
            (Some(t), None) => self.goto(e, t),
            (None, _) => {
                let msg = if is_continue {
                    "continue outside of loop"
                } else {
                    "break outside of loop"
                };
                self.states[e]
                    .stmts
                    .push(Self::throw_stmt(Self::str_expr(msg)));
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
        let cons_entry = self.compile_branch(cons, next)?;
        let alt_entry = match alt {
            Some(a) => self.compile_branch(a, next)?,
            None => next,
        };
        let dispatch = self.new_state();
        let tmp = self.fresh_tmp();
        let entry = self.new_state();
        self.compile_expr_to(test, &tmp, entry, dispatch)?;
        self.states[dispatch].stmts.push(Self::if_stmt(
            Self::ident_expr(&tmp),
            Self::goto_stmt(cons_entry),
            Self::goto_stmt(alt_entry),
        ));
        Ok(entry)
    }

    fn compile_while(
        &mut self,
        test: &Expr,
        body: &Stmt,
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        let l_test = self.new_state();
        let l_check = self.new_state();
        let l_body = self.new_state();
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_test,
        });
        let body_entry = self.compile_branch(body, l_test)?;
        self.abrupt.pop();
        let tmp = self.fresh_tmp();
        self.compile_expr_to(test, &tmp, l_test, l_check)?;
        self.states[l_check].stmts.push(Self::if_stmt(
            Self::ident_expr(&tmp),
            Self::goto_stmt(l_body),
            Self::goto_stmt(next),
        ));
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
        let l_check = self.new_state();
        let l_decide = self.new_state();
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_check,
        });
        let body_entry = self.compile_branch(body, l_check)?;
        self.abrupt.pop();
        let tmp = self.fresh_tmp();
        self.compile_expr_to(test, &tmp, l_check, l_decide)?;
        self.states[l_decide].stmts.push(Self::if_stmt(
            Self::ident_expr(&tmp),
            Self::goto_stmt(l_body),
            Self::goto_stmt(next),
        ));
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
        let l_test = self.new_state();
        let l_check = self.new_state();
        let l_body = self.new_state();
        let l_cont = self.new_state();
        self.abrupt.push(AbruptCtx::Loop {
            break_to: next,
            continue_to: l_cont,
        });
        let body_entry = self.compile_branch(body, l_cont)?;
        self.abrupt.pop();
        // l_cont：update 求值 → l_test
        match update {
            Some(u) => {
                let tmp = self.fresh_tmp();
                self.compile_expr_to(u, &tmp, l_cont, l_test)?;
            }
            None => self.goto(l_cont, l_test),
        }
        // l_test → l_check
        match test {
            Some(t) => {
                let tmp = self.fresh_tmp();
                self.compile_expr_to(t, &tmp, l_test, l_check)?;
                self.states[l_check].stmts.push(Self::if_stmt(
                    Self::ident_expr(&tmp),
                    Self::goto_stmt(l_body),
                    Self::goto_stmt(next),
                ));
            }
            None => self.goto(l_test, l_body),
        }
        self.goto(l_body, body_entry);
        // init → l_test
        match init {
            Some(ForInit::Expr(e)) => {
                let tmp = self.fresh_tmp();
                let entry = self.new_state();
                self.compile_expr_to(e, &tmp, entry, l_test)?;
                Ok(entry)
            }
            Some(ForInit::VarDecl { .. }) => Err(DesugarError(
                "internal: ForInit::VarDecl survived hoisting".to_string(),
            )),
            None => Ok(l_test),
        }
    }

    fn compile_switch(
        &mut self,
        disc: &Expr,
        cases: &[SwitchCase],
        next: StateId,
    ) -> Result<StateId, DesugarError> {
        for c in cases {
            if let Some(t) = &c.test {
                if has_await_expr(t) {
                    return Err(DesugarError(
                        "await in switch case test is not supported".to_string(),
                    ));
                }
            }
        }
        // 倒序建 case 入口（fallthrough 链接）。
        let mut entries: Vec<StateId> = vec![next; cases.len()];
        self.abrupt.push(AbruptCtx::Switch { break_to: next });
        for (i, c) in cases.iter().enumerate().rev() {
            let fall = if i + 1 < cases.len() {
                entries[i + 1]
            } else {
                next
            };
            entries[i] = self.compile_stmts(&c.body, fall)?;
        }
        self.abrupt.pop();
        // 找到 default。
        let default_entry = cases
            .iter()
            .zip(entries.iter())
            .find(|(c, _)| c.test.is_none())
            .map(|(_, e)| *e)
            .unwrap_or(next);
        // dispatch：if (tmp === c1) goto e1 else if ... else goto default
        let l_dispatch = self.new_state();
        let tmp = self.fresh_tmp();
        let entry = self.new_state();
        self.compile_expr_to(disc, &tmp, entry, l_dispatch)?;
        let mut else_br: Stmt = Self::goto_stmt(default_entry);
        for (c, e) in cases.iter().zip(entries.iter()).rev() {
            if let Some(t) = &c.test {
                let test = Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(Self::ident_expr(&tmp)),
                    right: Box::new(t.clone()),
                });
                else_br = Self::if_stmt(test, Self::goto_stmt(*e), else_br);
            }
        }
        self.states[l_dispatch].stmts.push(else_br);
        Ok(entry)
    }

    /// `pending → finally` 跳板状态。
    fn pending_goto(&mut self, fin: StateId, after: StateId) -> StateId {
        let s = self.new_state();
        self.states[s].stmts.push(Self::assign_stmt(
            PC,
            Self::obj_expr(vec![
                ("type", Self::str_expr("goto")),
                ("target", Self::num_expr(after as f64)),
            ]),
        ));
        self.goto(s, fin);
        s
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
            // handler body 编译（try ctx 已 pop，外层 ctx 生效）。
            let h_entry = self.compile_stmts(&h.body, h_next)?;
            // l_catch：绑定参数 → h_entry
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
            // l_fin_end：按 pending 分发
            let pc = Self::ident_expr(PC);
            let pc_type = Self::member_expr(pc.clone(), "type");
            self.states[fe].stmts.push(Self::if_stmt(
                Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(pc_type.clone()),
                    right: Box::new(Self::str_expr("ret")),
                }),
                Self::return_stmt(Self::member_expr(pc.clone(), "value")),
                Self::block_stmt(vec![]),
            ));
            self.states[fe].stmts.push(Self::if_stmt(
                Self::expr(ExprKind::Binary {
                    op: BinaryOp::StrictEq,
                    left: Box::new(pc_type),
                    right: Box::new(Self::str_expr("throw")),
                }),
                Self::throw_stmt(Self::member_expr(pc.clone(), "value")),
                Self::block_stmt(vec![]),
            ));
            // goto：__st = __pc.target; continue;
            self.states[fe].stmts.push(Self::block_stmt(vec![
                Self::assign_stmt(ST, Self::member_expr(Self::ident_expr(PC), "target")),
                Self::stmt(StmtKind::Continue),
            ]));
        }
        Ok(body_entry)
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
                id: EX_TMP.to_string(),
                init: None,
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
        // 4. __step 函数。
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
        out.push(Self::stmt(StmtKind::VarDecl {
            kind: VarKind::Const,
            decls: vec![VarDeclarator {
                id: STEP_FN.to_string(),
                init: Some(Self::expr(ExprKind::ArrowFunction(Box::new(ArrowFunction {
                    params: vec![],
                    body: ArrowBody::Block(step_body),
                    is_async: false,
                    strict: self.strict,
                })))),
            }],
        }));
        // 5. 入口：__st = <entry>（初始状态；声明时已为 0，
        //    但 entry 未必是 0，显式赋值）。
        out.push(Self::assign_stmt(ST, Self::num_expr(entry as f64)));
        // 6. 收尾：try { return Promise.resolve(__step()); }
        //         catch (__err) { return Promise.reject(__err); }
        let step_call = Self::call_expr(Self::ident_expr(STEP_FN), vec![]);
        let promise_resolve = Self::call_expr(
            Self::member_expr(Self::ident_expr("Promise"), "resolve"),
            vec![step_call],
        );
        out.push(Self::stmt(StmtKind::Try {
            block: vec![Self::return_stmt(promise_resolve)],
            handler: Some(CatchClause {
                param: Some(EPILOGUE_PARAM.to_string()),
                body: vec![Self::return_stmt(Self::call_expr(
                    Self::member_expr(Self::ident_expr("Promise"), "reject"),
                    vec![Self::ident_expr(EPILOGUE_PARAM)],
                ))],
            }),
            finalizer: None,
        }));
        out
    }

    /// try-body 状态的同步抛错包装。
    fn wrap_state(&self, w: &StateWrap, body: Vec<Stmt>) -> Stmt {
        let action: Vec<Stmt> = match (w.catch_to, w.fin_to) {
            (Some(c), _) => vec![
                Self::assign_stmt(&w.ex_tmp, Self::ident_expr(WRAP_PARAM)),
                Self::goto_stmt(c),
            ],
            (None, Some(f)) => vec![
                Self::assign_stmt(
                    PC,
                    Self::obj_expr(vec![
                        ("type", Self::str_expr("throw")),
                        ("value", Self::ident_expr(WRAP_PARAM)),
                    ]),
                ),
                Self::goto_stmt(f),
            ],
            (None, None) => vec![Self::throw_stmt(Self::ident_expr(WRAP_PARAM))],
        };
        Self::stmt(StmtKind::Try {
            block: body,
            handler: Some(CatchClause {
                param: Some(WRAP_PARAM.to_string()),
                body: action,
            }),
            finalizer: None,
        })
    }
}
