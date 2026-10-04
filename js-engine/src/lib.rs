//! yousj-js —— 自研 JavaScript 引擎。
//!
//! Phase 1: 手写词法分析器（`lexer`），零第三方依赖。
//! Phase 2: 手写递归下降解析器（`parser`）+ AST 定义（`ast`）。
//! Phase 3: 树遍历解释器（`interpreter`）+ 运行时值（`value`），JS 代码可真正跑起来。
//! Phase 4: 内置原型链 + 标准库方法（`object`）+ DOM 宿主接口（`dom`，mock 可测）。
//! 长期目标：1 年内做出可用的 JS 子集引擎。
//! 明面上 v0.3 先用 QuickJS 跑页面脚本；此引擎在后台悄悄推进，
//! 不求全 ECMAScript 合规，只求够用、可控、内存小。

pub mod ast;
pub mod async_desugar; // · Phase 7：async/await → 状态机改写。
pub mod async_gen; // · Phase 15：async 生成器 pre-pass。
pub mod bytecode; // · Phase 11：字节码指令集与 Chunk。
pub mod compiler; // · Phase 11：AST → 字节码编译器。
pub mod dom;
pub mod fetch; // · Phase 7：FetchHost trait（宿主提供网络实现）。
pub mod gen_desugar; // · Phase 9：生成器 → 状态机改写。
pub mod interpreter;
pub mod lexer;
pub mod object;
pub mod parser;
pub mod promise; // · Phase 7：Promise 状态机与微任务类型。
pub mod regex; // · Phase 7：手写极简正则引擎（零依赖）。
pub mod selector;
pub mod value;
pub mod vm; // · Phase 11：栈式字节码虚拟机。
pub mod webapi; // · Phase 13：Web API 补完（WebSocket/Worker/Storage/URL/…）。

pub use lexer::{Keyword, LexError, Lexer, Punct, Token, TokenKind};
pub use lexer::lex;
pub use ast::{
    ArrowBody, ArrowFunction, ArrayElem, AssignOp, BinaryOp, CatchClause, ClassElem,
    ClassKey, ClassNode, Expr, ExprKind, ForInit, ForLeft, FunctionNode, ImportSpec,
    Literal, LogicalOp, MethodKind, Param, Program, Prop, PropKey, PropValue, Span,
    Spanned, Stmt, StmtKind, SwitchCase, UnaryOp, UpdateOp, VarDeclarator, VarKind,
};
pub use parser::{parse_program, parse_source, ParseError, Parser};
pub use value::{
    number_to_js_string, ArrayRef, ClassField, DeclKind, Env, EnvRef, ErrorKind, FlowError,
    Frame, FuncBody, FuncRef, GenRef, JsArray, JsFunction, JsGenerator, JsObject, JsProxy,
    NativeCtx, NativeFn, ObjectRef, Phase9FuncFields, PrivateScope, PrivateScopeRef,
    ProxyRef, RuntimeError, Value,
};
pub use object::{instance_of, proto_head_of, BuiltinProtos};
pub use dom::{would_create_cycle, DomHost, DomNode, MockDom, NullDom};
pub use promise::{
    settle_promise, AllStateRef, JsPromise, Microtask, PromiseRef, PromiseSettler,
    PromiseSettlerRef, Reaction, ReactionKind, MAX_MICROTASKS,
};
pub use regex::{CompiledRegex, JsRegExp, RegExpRef};
pub use fetch::{FetchHost, FetchHostRef, FetchResponse, NullFetch};
pub use interpreter::{
    eval_source, eval_with_console, BreakAction, BreakpointHost, DebugHost, DebugPause,
    Interpreter, JsError, Signal, STEP_LIMIT,
};
