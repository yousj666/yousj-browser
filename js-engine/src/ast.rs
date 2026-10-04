//! yousj-js · Phase 2：抽象语法树（AST）定义。
//!
//! - 每个节点都带 `Span`（起止行列号，从 token 上抄来），方便报错和以后做 sourcemap。
//! - 覆盖 JS 常用子集：变量/函数声明、控制流、完整表达式优先级、对象/数组字面量、箭头函数。
//! - 刻意留到 phase 3（解释器）或以后的：正则字面量、模板字符串求值拆分、
//!   解构、展开（spread）、class、模块（import/export）、语句 label、async/await、生成器。

// ---------------------------------------------------------------------------
// 位置
// ---------------------------------------------------------------------------

/// 源码位置区间（1-based 行列号；半开区间，结束位置指向最后一个字符之后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

impl Span {
    pub fn new(sl: usize, sc: usize, el: usize, ec: usize) -> Self {
        Span {
            start_line: sl,
            start_col: sc,
            end_line: el,
            end_col: ec,
        }
    }

    /// 零宽位置（错误恢复等场景用）。
    pub fn point(line: usize, col: usize) -> Self {
        Span::new(line, col, line, col)
    }
}

/// 带位置信息的 AST 节点。
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub span: Span,
    pub node: T,
}

impl<T> Spanned<T> {
    pub fn new(span: Span, node: T) -> Self {
        Spanned { span, node }
    }
}

/// 表达式节点（含位置）。
pub type Expr = Spanned<ExprKind>;
/// 语句节点（含位置）。
pub type Stmt = Spanned<StmtKind>;

// ---------------------------------------------------------------------------
// 程序
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub span: Span,
    pub body: Vec<Stmt>,
    /// phase 8：脚本级严格模式（开头有 `"use strict"` 指令序言）。
    pub strict: bool,
}

// ---------------------------------------------------------------------------
// 字面量与运算符
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Number(f64),
    String(String),
    Bool(bool),
    Null,
    /// phase 1 模板按整体切分；phase 2 先当普通字面量存原文。
    /// TODO: 拆成 cooked 片段 + `${}` 表达式。
    Template(String),
    /// 正则字面量（phase 7）：词法阶段用"前一个 token"启发式区分 `/`。
    Regex { pattern: String, flags: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,    // -x
    Pos,    // +x
    Not,    // !x
    BitNot, // ~x
    Typeof, // typeof x
    Void,   // void x
    Delete, // delete x.y
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc, // ++
    Dec, // --
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow, // **（右结合）
    Shl,
    Shr,
    UShr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,       // ==
    Ne,       // !=
    StrictEq, // ===
    StrictNe, // !==
    BitAnd,
    BitOr,
    BitXor,
    In,
    Instanceof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,    // &&
    Or,     // ||
    Nullish, // ??
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    PowAssign,
    ShlAssign,
    ShrAssign,
    UShrAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    AndAssign,     // &&=
    OrAssign,      // ||=
    NullishAssign, // ??=
}

// ---------------------------------------------------------------------------
// 表达式
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Literal(Literal),
    Ident(String),
    This,
    // TODO(phase 3): SuperExpression。
    Array(Vec<ArrayElem>),
    Object(Vec<Prop>),
    Function(Box<FunctionNode>),
    ArrowFunction(Box<ArrowFunction>),
    Unary {
        op: UnaryOp,
        arg: Box<Expr>,
    },
    Update {
        op: UpdateOp,
        arg: Box<Expr>,
        prefix: bool,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Logical {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Assign {
        op: AssignOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Conditional {
        test: Box<Expr>,
        cons: Box<Expr>,
        alt: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        optional: bool, // ?.()
    },
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Member {
        obj: Box<Expr>,
        prop: Box<Expr>,
        computed: bool,  // a[b] vs a.b
        optional: bool,  // ?. 
    },
    /// 逗号序列表达式：`a, b, c`。
    Sequence(Vec<Expr>),
    /// `await e`（phase 7）：只允许出现在 async 函数内（parser 保证）。
    Await(Box<Expr>),
    /// `yield` / `yield e` / `yield* e`（phase 9）：只允许出现在生成器内
    /// （parser 保证）。解释器求值到它说明 desugar 漏了，直接报错。
    Yield {
        arg: Option<Box<Expr>>,
        /// `yield*` 委托。
        delegate: bool,
    },
    /// `super`（phase 9）：`super(...)` 走 Call callee 分支；`super.x` 走
    /// Member obj 分支。只允许出现在类方法/派生构造器内（parser 宽松处理，
    /// 解释器在无 home 时报错）。
    Super,
    /// `class C extends B { ... }` 表达式（phase 9）。
    Class(Box<ClassNode>),
    /// `obj.#x` 私有字段/方法访问（phase 9）。
    PrivateMember {
        obj: Box<Expr>,
        name: String,
        optional: bool, // ?.#x（罕见，支持）
    },
    /// `#x` 私有存在检查的左操作数（phase 9）：只出现在 `#x in obj` 中，
    /// parser 保证；解释器在 Binary::In 分支特殊处理。
    PrivateName(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayElem {
    Expr(Expr),
    /// 空位：`[1, , 3]`。
    Hole,
    /// `[...iter]`（phase 9）：求值时消费可迭代对象（数组/字符串/生成器）。
    Spread(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropKey {
    Ident(String),
    String(String),
    Number(f64),
    Computed(Expr), // {[k]: v}
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    Init(Expr),              // {a: 1}
    Shorthand(String),       // {a}
    Method(Box<FunctionNode>), // {f() {}}
    Getter(Box<FunctionNode>), // {get x() {}}
    Setter(Box<FunctionNode>), // {set x(v) {}}
    // TODO: Spread(Expr)（`{...x}`）。
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub key: PropKey,
    pub value: PropValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub default: Option<Expr>,
    // TODO: 解构参数、rest 参数（`...args`）。
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionNode {
    /// 函数声明要求 Some；函数表达式可为 None（匿名）。
    pub id: Option<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    /// `function*`（生成器语义留到 phase 9）。
    pub is_generator: bool,
    /// async 函数（phase 7）：解释器侧经状态机 desugar 后执行。
    pub is_async: bool,
    /// phase 8：有效严格模式（自身指令序言 || 定义时外层 strict，解析期已折叠）。
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrowFunction {
    pub params: Vec<Param>,
    pub body: ArrowBody,
    /// `async (...) => ...`（phase 7）。
    pub is_async: bool,
    /// phase 8：有效严格模式（块体自身的指令序言 || 定义时外层 strict）。
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrowBody {
    /// `x => x + 1`（隐式 return）。
    Expr(Box<Expr>),
    /// `x => { return x + 1; }`。
    Block(Vec<Stmt>),
}

/// `import {a, b as c} from 'mod'` 的单项：本地名 + 导出名。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSpec {
    pub local: String,
    pub imported: String,
}

// ---------------------------------------------------------------------------
// 类（phase 9）
// ---------------------------------------------------------------------------

/// `class C extends B { ... }`。
#[derive(Debug, Clone, PartialEq)]
pub struct ClassNode {
    /// 类声明要求 Some；类表达式可为 None（匿名）。
    pub id: Option<String>,
    pub super_class: Option<Box<Expr>>,
    pub body: Vec<ClassElem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Constructor,
    Method,
    Getter,
    Setter,
}

/// 类体成员键：公开键复用 PropKey，`#x` 为私有。
#[derive(Debug, Clone, PartialEq)]
pub enum ClassKey {
    Public(PropKey),
    Private(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClassElem {
    Method {
        key: ClassKey,
        func: FunctionNode,
        kind: MethodKind,
        is_static: bool,
    },
    Field {
        key: ClassKey,
        init: Option<Expr>,
        is_static: bool,
    },
    /// `static { ... }` 静态块：类求值时按序执行一次。
    StaticBlock(Vec<Stmt>),
}

// ---------------------------------------------------------------------------
// 语句
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarDeclarator {
    pub id: String,
    pub init: Option<Expr>,
    // TODO: 解构声明（`var {a} = o`）。
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForInit {
    VarDecl {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
    },
    Expr(Expr),
}

/// `for-in` / `for-of` 左侧。
#[derive(Debug, Clone, PartialEq)]
pub enum ForLeft {
    /// `for (var/let/const x in obj)`（只允许单个声明子）。
    VarDecl { kind: VarKind, name: String },
    /// `for (x in obj)` / `for (a.b of arr)`。
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    /// `catch {}` 时为 None。
    pub param: Option<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// None 表示 `default:`。
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// 空语句 `;`。
    Empty,
    Debugger,
    Expr(Expr),
    Block(Vec<Stmt>),
    VarDecl {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
    },
    /// 函数声明（`id` 必为 `Some`）。
    FunctionDecl(Box<FunctionNode>),
    If {
        test: Expr,
        cons: Box<Stmt>,
        alt: Option<Box<Stmt>>,
    },
    While {
        test: Expr,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Expr,
    },
    For {
        init: Option<ForInit>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
    },
    ForInOf {
        is_of: bool, // true = for-of，false = for-in
        left: ForLeft,
        right: Expr,
        body: Box<Stmt>,
    },
    Return(Option<Expr>),
    /// label 暂不支持（TODO）。
    Break,
    /// label 暂不支持（TODO）。
    Continue,
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        handler: Option<CatchClause>,
        finalizer: Option<Vec<Stmt>>,
    },
    Switch {
        disc: Expr,
        cases: Vec<SwitchCase>,
    },
    // ---- Phase 7：模块 ----
    /// `import {a, b as c} from 'mod'` / `import 'mod'`。
    Import {
        specs: Vec<ImportSpec>,
        source: String,
    },
    /// `export const x = 1` / `export function f() {}`。
    ExportDecl {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
    },
    /// `export function f() {}`（单列一项，保持 FunctionDecl 形态）。
    ExportFunc(Box<FunctionNode>),
    /// `export {a, b as c}`（(导出名, 本地名)）。
    ExportNames(Vec<(String, String)>),
    // ---- Phase 9：类 ----
    /// `class C extends B { ... }` 声明（`id` 必为 `Some`）。
    ClassDecl(Box<ClassNode>),
}
