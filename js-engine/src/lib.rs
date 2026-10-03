//! yousj-js —— 自研 JavaScript 引擎（秘密项目）。
//!
//! Phase 1: 手写词法分析器（`lexer`），零第三方依赖。
//! 长期目标：1 年内做出可用的 JS 子集引擎。
//! 明面上 v0.3 先用 QuickJS 跑页面脚本；此引擎在后台悄悄推进，
//! 不求全 ECMAScript 合规，只求够用、可控、内存小。

pub mod lexer;

pub use lexer::{Keyword, LexError, Lexer, Punct, Token, TokenKind};
pub use lexer::lex;
