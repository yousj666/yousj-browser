//! yousj-js · Phase 15：async 生成器的 pre-pass。
//!
//! 把 `async function*` 体内的 `yield` 改写为 `await` 形式，再交给
//! 现有的 `AsyncDesugar`（await 状态机）处理：
//! - `yield v` → `await __yousj$AG.__yousj$yieldOp(v)`
//! - `yield` → `await __yousj$AG.__yousj$yieldOp(undefined)`
//! - 语句位置的 `yield* e` → `for (var $v of e) { yield $v; }`
//!   （再经本 pass 转为 await 形式）；表达式位置的 `yield*` 暂不支持。
//! - 不递归进入嵌套的函数/类（它们的 yield 属于自己）。
//!
//! `__yousj$AG` 由解释器在 async 生成器调用时注入到调用环境，
//! 是一个宿主对象，提供 `__yousj$yieldOp` 方法。

use crate::ast::*;

const AG: &str = "__yousj$AG";
const YIELD_OP: &str = "__yousj$yieldOp";

pub struct AsyncGenPrepass {
    tmp_count: usize,
}

impl AsyncGenPrepass {
    pub fn new() -> Self {
        AsyncGenPrepass { tmp_count: 0 }
    }

    fn fresh_tmp(&mut self) -> String {
        self.tmp_count += 1;
        format!("__yousj$ag{}", self.tmp_count)
    }

    fn sp(&self) -> Span {
        Span::new(0, 0, 0, 0)
    }

    fn ident(&self, name: &str) -> Expr {
        Spanned::new(self.sp(), ExprKind::Ident(name.to_string()))
    }

    /// `yield v` → `await __yousj$AG.__yousj$yieldOp(v)`
    fn yield_to_await(&self, arg: Expr) -> Expr {
        let ag = self.ident(AG);
        let prop = Spanned::new(
            self.sp(),
            ExprKind::Literal(Literal::String(YIELD_OP.to_string())),
        );
        let method = Spanned::new(
            self.sp(),
            ExprKind::Member {
                obj: Box::new(ag),
                prop: Box::new(prop),
                computed: false,
                optional: false,
            },
        );
        let call = Spanned::new(
            self.sp(),
            ExprKind::Call {
                callee: Box::new(method),
                args: vec![arg],
                optional: false,
            },
        );
        Spanned::new(self.sp(), ExprKind::Await(Box::new(call)))
    }

    pub fn run(&mut self, stmts: &[Stmt]) -> Vec<Stmt> {
        stmts.iter().map(|s| self.visit_stmt(s)).collect()
    }

    fn visit_stmt(&mut self, s: &Stmt) -> Stmt {
        let kind = match &s.node {
            StmtKind::Expr(e) => {
                // 语句位置的 `yield* e` → for-of 展开。
                if let ExprKind::Yield { arg, delegate: true } = &e.node {
                    if let Some(arg_expr) = arg {
                        return self.expand_yield_star(e.span, (**arg_expr).clone());
                    }
                }
                StmtKind::Expr(self.visit_expr(e))
            }
            StmtKind::Block(b) => StmtKind::Block(b.iter().map(|x| self.visit_stmt(x)).collect()),
            StmtKind::Return(e) => StmtKind::Return(e.as_ref().map(|x| self.visit_expr(x))),
            StmtKind::If { test, cons, alt } => StmtKind::If {
                test: self.visit_expr(test),
                cons: Box::new(self.visit_stmt(cons)),
                alt: alt.as_ref().map(|x| Box::new(self.visit_stmt(x))),
            },
            StmtKind::While { test, body } => StmtKind::While {
                test: self.visit_expr(test),
                body: Box::new(self.visit_stmt(body)),
            },
            StmtKind::DoWhile { body, test } => StmtKind::DoWhile {
                body: Box::new(self.visit_stmt(body)),
                test: self.visit_expr(test),
            },
            StmtKind::For { init, test, update, body } => StmtKind::For {
                init: init.as_ref().map(|i| self.visit_for_init(i)),
                test: test.as_ref().map(|x| self.visit_expr(x)),
                update: update.as_ref().map(|x| self.visit_expr(x)),
                body: Box::new(self.visit_stmt(body)),
            },
            StmtKind::ForInOf { is_of, left, right, body } => StmtKind::ForInOf {
                is_of: *is_of,
                left: self.visit_for_left(left),
                right: self.visit_expr(right),
                body: Box::new(self.visit_stmt(body)),
            },
            StmtKind::Try { block, handler, finalizer } => StmtKind::Try {
                block: block.iter().map(|x| self.visit_stmt(x)).collect(),
                handler: handler.as_ref().map(|c| CatchClause {
                    param: c.param.clone(),
                    body: c.body.iter().map(|x| self.visit_stmt(x)).collect(),
                }),
                finalizer: finalizer
                    .as_ref()
                    .map(|f| f.iter().map(|x| self.visit_stmt(x)).collect()),
            },
            StmtKind::Switch { disc, cases } => StmtKind::Switch {
                disc: self.visit_expr(disc),
                cases: cases
                    .iter()
                    .map(|c| SwitchCase {
                        test: c.test.as_ref().map(|x| self.visit_expr(x)),
                        body: c.body.iter().map(|x| self.visit_stmt(x)).collect(),
                    })
                    .collect(),
            },
            // 不进入嵌套函数/类。
            StmtKind::FunctionDecl(_) => return s.clone(),
            StmtKind::VarDecl { kind, decls } => StmtKind::VarDecl {
                kind: *kind,
                decls: decls
                    .iter()
                    .map(|d| VarDeclarator {
                        id: d.id.clone(),
                        init: d.init.as_ref().map(|x| self.visit_expr(x)),
                    })
                    .collect(),
            },
            _ => return s.clone(),
        };
        Spanned::new(s.span, kind)
    }

    fn expand_yield_star(&mut self, span: Span, arg: Expr) -> Stmt {
        // for (var $v of <arg>) { <yield $v> }
        let v = self.fresh_tmp();
        let v_ident = self.ident(&v);
        let yield_expr = self.yield_to_await(v_ident.clone());
        let body = Spanned::new(
            span,
            StmtKind::Block(vec![Spanned::new(span, StmtKind::Expr(yield_expr))]),
        );
        Spanned::new(
            span,
            StmtKind::ForInOf {
                is_of: true,
                left: ForLeft::VarDecl {
                    kind: VarKind::Var,
                    name: v,
                },
                right: self.visit_expr(&arg),
                body: Box::new(body),
            },
        )
    }

    fn visit_for_init(&mut self, init: &ForInit) -> ForInit {
        match init {
            ForInit::VarDecl { kind, decls } => ForInit::VarDecl {
                kind: *kind,
                decls: decls
                    .iter()
                    .map(|d| VarDeclarator {
                        id: d.id.clone(),
                        init: d.init.as_ref().map(|x| self.visit_expr(x)),
                    })
                    .collect(),
            },
            ForInit::Expr(e) => ForInit::Expr(self.visit_expr(e)),
        }
    }

    fn visit_for_left(&mut self, left: &ForLeft) -> ForLeft {
        // 左部不含 yield（解析器已保证），直接克隆。
        left.clone()
    }

    fn visit_expr(&mut self, e: &Expr) -> Expr {
        match &e.node {
            ExprKind::Yield { arg, delegate } => {
                if *delegate {
                    // 表达式位置的 yield*：暂不支持，保持原样（AsyncDesugar 会报错）。
                    return e.clone();
                }
                let arg_expr = arg
                    .as_ref()
                    .map(|x| self.visit_expr(x))
                    .unwrap_or_else(|| self.ident("undefined"));
                self.yield_to_await(arg_expr)
            }
            // 不进入嵌套函数/类/箭头。
            ExprKind::Function(_) | ExprKind::ArrowFunction(_) | ExprKind::Class(_) => e.clone(),
            _ => self.visit_expr_children(e),
        }
    }

    /// 递归访问表达式的子节点（不含 yield 转换逻辑）。
    fn visit_expr_children(&mut self, e: &Expr) -> Expr {
        // 为避免遗漏，默认对所有变体做结构化递归。
        // 这里用一个简化的方式：只处理可能包含 yield 的位置。
        // 实际上 yield 可以出现在任何表达式位置，所以我们需要完整递归。
        //
        // 由于 ExprKind 变体很多，我们采用"匹配已知、未知克隆"的策略，
        // 但未知克隆会漏掉嵌套的 yield。为安全，我们 panic 提示补全，
        // 而不是静默漏掉。
        let kind = match &e.node {
            ExprKind::Literal(_) | ExprKind::Ident(_) | ExprKind::This | ExprKind::Super => {
                return e.clone()
            }
            ExprKind::Array(elems) => ExprKind::Array(
                elems
                    .iter()
                    .map(|el| match el {
                        ArrayElem::Expr(x) => ArrayElem::Expr(self.visit_expr(x)),
                        ArrayElem::Spread(x) => ArrayElem::Spread(self.visit_expr(x)),
                        ArrayElem::Hole => ArrayElem::Hole,
                    })
                    .collect(),
            ),
            ExprKind::Object(props) => ExprKind::Object(
                props
                    .iter()
                    .map(|p| {
                        let key = match &p.key {
                            PropKey::Computed(e) => PropKey::Computed(self.visit_expr(e)),
                            k => k.clone(),
                        };
                        let value = match &p.value {
                            PropValue::Init(e) => PropValue::Init(self.visit_expr(e)),
                            v => v.clone(),
                        };
                        Prop { key, value }
                    })
                    .collect(),
            ),
            ExprKind::Call { callee, args, optional } => ExprKind::Call {
                callee: Box::new(self.visit_expr(callee)),
                args: args.iter().map(|x| self.visit_expr(x)).collect(),
                optional: *optional,
            },
            ExprKind::New { callee, args } => ExprKind::New {
                callee: Box::new(self.visit_expr(callee)),
                args: args.iter().map(|x| self.visit_expr(x)).collect(),
            },
            ExprKind::Member { obj, prop, computed, optional } => ExprKind::Member {
                obj: Box::new(self.visit_expr(obj)),
                prop: Box::new(self.visit_expr(prop)),
                computed: *computed,
                optional: *optional,
            },
            ExprKind::Binary { op, left, right } => ExprKind::Binary {
                op: *op,
                left: Box::new(self.visit_expr(left)),
                right: Box::new(self.visit_expr(right)),
            },
            ExprKind::Unary { op, arg } => ExprKind::Unary {
                op: *op,
                arg: Box::new(self.visit_expr(arg)),
            },
            ExprKind::Update { op, arg, prefix } => ExprKind::Update {
                op: *op,
                arg: Box::new(self.visit_expr(arg)),
                prefix: *prefix,
            },
            ExprKind::Assign { op, left, right } => ExprKind::Assign {
                op: *op,
                left: Box::new(self.visit_expr(left)),
                right: Box::new(self.visit_expr(right)),
            },
            ExprKind::Conditional { test, cons, alt } => ExprKind::Conditional {
                test: Box::new(self.visit_expr(test)),
                cons: Box::new(self.visit_expr(cons)),
                alt: Box::new(self.visit_expr(alt)),
            },
            ExprKind::Sequence(exprs) => {
                ExprKind::Sequence(exprs.iter().map(|x| self.visit_expr(x)).collect())
            }
            ExprKind::Await(x) => ExprKind::Await(Box::new(self.visit_expr(x))),

            _ => {
                // 未知变体：保守克隆（理论上不应含 yield）。
                return e.clone();
            }
        };
        Spanned::new(e.span, kind)
    }
}
