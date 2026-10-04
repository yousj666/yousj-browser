//! yousj-js · Phase 2：手写递归下降解析器。
//!
//! - 零第三方依赖；二元表达式用 precedence climbing（Pratt），保证优先级与结合性正确。
//! - ASI（自动分号插入）简化版：换行 / `}` / EOF 处可省略分号；
//!   `return` / `throw` / `break` / `continue` 的受限产生式按规范处理。
//! - 解析错误返回带行列号的 `ParseError`，不 panic。
//! - 刻意留到以后的：正则字面量、模板拆分、解构、spread、class、模块、label、async。

use crate::ast::*;
use crate::lexer::{Keyword, LexError, Punct, Token, TokenKind};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl ParseError {
    pub fn new(line: usize, col: usize, message: impl Into<String>) -> Self {
        ParseError {
            line,
            col,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // phase 8：所有解析错误按规范都是 SyntaxError。
        write!(f, "SyntaxError at {}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::new(e.line, e.col, format!("lex error: {}", e.message))
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// 回溯标记（箭头函数形参试探用）。
#[derive(Debug, Clone, Copy)]
struct Mark {
    pos: usize,
    prev_end_line: usize,
    prev_end_col: usize,
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// 上一个已消费 token 的结束行号（ASI 换行检测用）。
    prev_end_line: usize,
    /// 上一个已消费 token 的结束列号。
    prev_end_col: usize,
    /// `for (init; …)` 顶层是否允许 `in` 运算符（避免和 for-in 混淆）。
    allow_in: bool,
    /// async 函数嵌套深度（`await` 表达式解析用）。
    in_async: u32,
    /// phase 9：生成器函数嵌套深度（`yield` 表达式解析用）。
    /// 非生成器 `function` 会重置为 0（箭头函数继承）。
    in_generator: u32,
    /// phase 9：类嵌套深度（`#x` 私有访问解析用；类外出现 `#x` 直接报错）。
    in_class: u32,
    /// phase 8：当前解析位置是否处于严格模式（外层 strict 会继承进内层函数）。
    strict: bool,
    /// phase 8：每层函数/脚本作用域是否出现过 legacy 八进制字面量
    /// （`010`）。栈顶为当前作用域；严格模式下出现即为 SyntaxError。
    octal_scopes: Vec<bool>,
    /// phase 15：解构脱糖的临时变量计数器（`__yousj$d<N>` 全局唯一）。
    destructure_tmp: usize,
}

/// peek_binop 的内部返回类型：二元 vs 逻辑运算符。
#[derive(Debug, Clone, Copy)]
enum BinOp {
    Binary(BinaryOp),
    Logical(LogicalOp),
}

// ---------------------------------------------------------------------------
// Phase 14：模板字面量 `${}` 插值（解析期脱糖为字符串拼接）。
// ---------------------------------------------------------------------------

/// 模板片段：已烹制的字符串 / 插值表达式源码（raw）。
enum TemplatePart {
    Str(String),
    Expr(String),
}

/// 烹制模板原始文本（处理转义）。规则同字符串字面量，另加：
/// `` \` `` → 反引号，`\$` → 美元符，行继续 `\<换行>` → 空。
fn cook_template(raw: &str) -> Result<String, String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            return Err("bad escape at end of template".to_string());
        }
        let e = chars[i];
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '0' => out.push('\0'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            '`' => out.push('`'),
            '$' => out.push('$'),
            '\n' => {}
            '\r' => {
                if i + 1 < chars.len() && chars[i + 1] == '\n' {
                    i += 1;
                }
            }
            'x' => {
                if i + 2 >= chars.len() {
                    return Err("bad \\x escape".to_string());
                }
                let hex: String = chars[i + 1..i + 3].iter().collect();
                let v = u32::from_str_radix(&hex, 16)
                    .map_err(|_| "bad \\x escape".to_string())?;
                out.push(char::from_u32(v).ok_or("bad \\x escape".to_string())?);
                i += 2;
            }
            'u' => {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    let mut j = i + 2;
                    let mut hex = String::new();
                    while j < chars.len() && chars[j] != '}' {
                        hex.push(chars[j]);
                        j += 1;
                    }
                    if j >= chars.len() || hex.is_empty() || hex.len() > 6 {
                        return Err("bad unicode escape".to_string());
                    }
                    let v = u32::from_str_radix(&hex, 16)
                        .map_err(|_| "bad unicode escape".to_string())?;
                    out.push(char::from_u32(v).ok_or("bad unicode escape".to_string())?);
                    i = j;
                } else {
                    if i + 4 >= chars.len() {
                        return Err("bad unicode escape".to_string());
                    }
                    let hex: String = chars[i + 1..i + 5].iter().collect();
                    let v = u32::from_str_radix(&hex, 16)
                        .map_err(|_| "bad unicode escape".to_string())?;
                    out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                    i += 4;
                }
            }
            _ => out.push(e),
        }
        i += 1;
    }
    Ok(out)
}

/// 扫描 `${...}` 的表达式源码（调用时 i 指向 `{` 之后）。
/// 处理嵌套花括号、字符串、嵌套模板、注释。返回 (源码, `}` 之后的位置)。
fn scan_braced(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut depth = 1usize;
    let mut i = start;
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            out.push(c);
            if i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            let (s, ni) = scan_quoted(chars, i, c)?;
            out.push_str(&s);
            i = ni;
            continue;
        }
        if c == '`' {
            let (s, ni) = scan_nested_template(chars, i)?;
            out.push_str(&s);
            i = ni;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            let mut j = i;
            while j < chars.len() && chars[j] != '\n' {
                out.push(chars[j]);
                j += 1;
            }
            i = j;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            let mut j = i;
            while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                out.push(chars[j]);
                j += 1;
            }
            if j + 1 < chars.len() {
                out.push(chars[j]);
                out.push(chars[j + 1]);
                j += 2;
            }
            i = j;
            continue;
        }
        if c == '{' {
            depth += 1;
        }
        if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Ok((out, i + 1));
            }
        }
        out.push(c);
        i += 1;
    }
    Err("unterminated ${} in template".to_string())
}

/// 扫描引号字符串（含转义），返回 (源码, 结束引号之后的位置)。
fn scan_quoted(chars: &[char], start: usize, quote: char) -> Result<(String, usize), String> {
    let mut i = start;
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        i += 1;
        if c == '\\' && i < chars.len() {
            out.push(chars[i]);
            i += 1;
        } else if c == quote {
            return Ok((out, i));
        }
    }
    Err("unterminated string in template expression".to_string())
}

/// 扫描嵌套模板（调用时 i 指向开反引号），返回 (源码含反引号, 之后的位置)。
fn scan_nested_template(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut i = start;
    let mut out = String::new();
    let mut depth = 0usize; // `${` 嵌套深度
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        if c == '\\' && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '`' && depth == 0 {
            return Ok((out, i + 1));
        }
        if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            out.push(chars[i + 1]);
            i += 2;
            depth += 1;
            continue;
        }
        if c == '}' && depth > 0 {
            depth -= 1;
        }
        i += 1;
    }
    Err("unterminated nested template".to_string())
}

/// 把模板 raw 文本切分为字符串/表达式片段。
fn split_template(raw: &str) -> Result<Vec<TemplatePart>, String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let flush = |cur: &mut String,
                 parts: &mut Vec<TemplatePart>|
     -> Result<(), String> {
        if !cur.is_empty() {
            parts.push(TemplatePart::Str(cook_template(cur)?));
            cur.clear();
        }
        Ok(())
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            // 转义原样保留（cook 时处理）；`\${` 不会误触发插值。
            cur.push(c);
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            flush(&mut cur, &mut parts)?;
            let (expr_src, ni) = scan_braced(&chars, i + 2)?;
            parts.push(TemplatePart::Expr(expr_src));
            i = ni;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut cur, &mut parts)?;
    Ok(parts)
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            prev_end_line: 1,
            prev_end_col: 1,
            allow_in: true,
            in_async: 0,
            in_generator: 0,
            in_class: 0,
            strict: false,
            octal_scopes: Vec::new(),
            destructure_tmp: 0,
        }
    }

    /// 在 async 上下文中执行解析（函数体解析用）。
    fn with_async<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R, ParseError>) -> Result<R, ParseError> {
        self.in_async += 1;
        let r = f(self);
        self.in_async -= 1;
        r
    }

    /// phase 9：在生成器上下文中执行解析（生成器函数体解析用）。
    fn with_generator<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R, ParseError>) -> Result<R, ParseError> {
        self.in_generator += 1;
        let r = f(self);
        self.in_generator -= 1;
        r
    }

    /// phase 9：非生成器 `function` 进入时重置 yield 上下文
    /// （`yield` 在普通函数内是标识符；箭头函数不走这里，继承外层）。
    fn with_generator_reset<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R, ParseError>) -> Result<R, ParseError> {
        let saved = std::mem::replace(&mut self.in_generator, 0);
        let r = f(self);
        self.in_generator = saved;
        r
    }

    /// phase 9：在类上下文中执行解析（`#x` 私有访问解析用）。
    fn with_class<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R, ParseError>) -> Result<R, ParseError> {
        self.in_class += 1;
        let r = f(self);
        self.in_class -= 1;
        r
    }

    // ---------- 基础游标 ----------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, n: usize) -> &Token {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// 前进一步；在 EOF 处饱和（不再前进）。
    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if !self.at_eof() {
            self.pos += 1;
        }
        // 计算 token 结束位置（处理 \r\n 只算一个换行）。
        let chars: Vec<char> = t.lexeme.chars().collect();
        let mut el = t.line;
        let mut ec = t.col;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\r' {
                el += 1;
                ec = 1;
                if chars.get(i + 1) == Some(&'\n') {
                    i += 1;
                }
            } else if c == '\n' {
                el += 1;
                ec = 1;
            } else {
                ec += 1;
            }
            i += 1;
        }
        self.prev_end_line = el;
        self.prev_end_col = ec;
        t
    }

    fn cur_pos(&self) -> (usize, usize) {
        let t = self.peek();
        (t.line, t.col)
    }

    fn spanned<T>(&self, sl: usize, sc: usize, node: T) -> Spanned<T> {
        Spanned::new(
            Span::new(sl, sc, self.prev_end_line, self.prev_end_col),
            node,
        )
    }

    fn mark(&self) -> Mark {
        Mark {
            pos: self.pos,
            prev_end_line: self.prev_end_line,
            prev_end_col: self.prev_end_col,
        }
    }

    fn restore(&mut self, m: Mark) {
        self.pos = m.pos;
        self.prev_end_line = m.prev_end_line;
        self.prev_end_col = m.prev_end_col;
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        let t = self.peek();
        Err(ParseError::new(t.line, t.col, msg))
    }

    fn err_at<T>(&self, span: Span, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError::new(span.start_line, span.start_col, msg))
    }

    // ---------- token 判定 ----------

    fn at_punct(&self, p: Punct) -> bool {
        matches!(&self.peek().kind, TokenKind::Punct(q) if *q == p)
    }

    fn at_keyword(&self, k: Keyword) -> bool {
        matches!(&self.peek().kind, TokenKind::Keyword(q) if *q == k)
    }

    fn peek_at_is(&self, n: usize, p: Punct) -> bool {
        matches!(&self.peek_at(n).kind, TokenKind::Punct(q) if *q == p)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.at_punct(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, k: Keyword) -> bool {
        if self.at_keyword(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// 若当前 token 是指定文本的标识符（如 `as` / `from`），吃掉并返回 true。
    fn eat_ident(&mut self, name: &str) -> bool {
        match &self.peek().kind {
            TokenKind::Ident(s) if s == name => {
                self.bump();
                true
            }
            _ => false,
        }
    }

    fn expect_punct(&mut self, p: Punct) -> Result<(), ParseError> {
        if self.eat_punct(p.clone()) {
            Ok(())
        } else {
            let found = self.peek().lexeme.clone();
            self.err(format!("expected '{}', found '{}'", p.as_str(), found))
        }
    }

    fn expect_keyword(&mut self, k: Keyword) -> Result<(), ParseError> {
        if self.eat_keyword(k.clone()) {
            Ok(())
        } else {
            let found = self.peek().lexeme.clone();
            self.err(format!("expected keyword '{}', found '{}'", k.as_str(), found))
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Ident(name) => {
                self.bump();
                Ok(name)
            }
            _ => {
                let found = self.peek().lexeme.clone();
                self.err(format!("expected identifier, found '{}'", found))
            }
        }
    }

    /// 绑定标识符：普通标识符 + 上下文关键字（`yield`/`await`/`async`）。
    /// `yield` 仅在非生成器上下文中可作绑定名。
    fn expect_binding_ident(&mut self) -> Result<String, ParseError> {
        let t = self.peek().clone();
        match &t.kind {
            TokenKind::Ident(name) => {
                let n = name.clone();
                self.bump();
                Ok(n)
            }
            TokenKind::Keyword(k)
                if matches!(k.as_str(), "await" | "async" | "let" | "static") =>
            {
                let n = k.as_str().to_string();
                self.bump();
                Ok(n)
            }
            TokenKind::Keyword(k) if k.as_str() == "yield" && self.in_generator == 0 => {
                self.bump();
                Ok("yield".to_string())
            }
            _ => {
                let found = t.lexeme.clone();
                self.err(format!("expected identifier, found '{}'", found))
            }
        }
    }

    /// 属性名：标识符或任意关键字（`a.if` 合法）。
    fn expect_prop_name(&mut self) -> Result<(String, Span), ParseError> {
        let t = self.peek().clone();
        let name = match t.kind {
            TokenKind::Ident(n) => n,
            TokenKind::Keyword(k) => k.as_str().to_string(),
            _ => return self.err(format!("expected property name, found '{}'", t.lexeme)),
        };
        let (sl, sc) = (t.line, t.col);
        self.bump();
        Ok((name, Span::new(sl, sc, self.prev_end_line, self.prev_end_col)))
    }

    fn peek_is_prop_key_start(&self) -> bool {
        matches!(
            &self.peek().kind,
            TokenKind::Ident(_)
                | TokenKind::Keyword(_)
                | TokenKind::Str(_)
                | TokenKind::Number(_)
                | TokenKind::Punct(Punct::LBracket)
        )
    }

    /// 当前 token 与上一个 token 之间是否有换行（ASI 用）。
    /// 注：注释里的换行 lexer 已跳过，但行号差仍在——按规范注释中的
    /// LineTerminator 同样触发受限产生式，这里行为恰好一致。
    fn newline(&self) -> bool {
        self.peek().line > self.prev_end_line
    }

    /// 分号：显式 `;`，或 ASI（`}` / EOF / 换行）。
    fn expect_semi(&mut self) -> Result<(), ParseError> {
        if self.eat_punct(Punct::Semi) {
            return Ok(());
        }
        if self.at_punct(Punct::RBrace) || self.at_eof() || self.newline() {
            return Ok(());
        }
        self.err("expected ';'")
    }

    fn var_kind_of_peek(&self) -> VarKind {
        match &self.peek().kind {
            TokenKind::Keyword(Keyword::Var) => VarKind::Var,
            TokenKind::Keyword(Keyword::Const) => VarKind::Const,
            _ => VarKind::Let,
        }
    }

    fn with_allow_in<R>(
        &mut self,
        v: bool,
        f: impl FnOnce(&mut Self) -> Result<R, ParseError>,
    ) -> Result<R, ParseError> {
        let old = self.allow_in;
        self.allow_in = v;
        let r = f(self);
        self.allow_in = old;
        r
    }

    // ---------- 程序入口 ----------

    /// phase 8：指令序言嗅探——当前位置是否为 `"use strict";` 形式的
    /// StringLiteral 表达式语句。ASI 细节：字符串后必须是 `;` / `}` / EOF，
    /// 或下一个 token 在下一行（`"use strict"\nfoo()` 经 ASI 仍是序言，
    /// 而 `"use strict" + x` 同行则不是）。
    fn peek_directive(&self) -> bool {
        let t = self.peek();
        let is_use_strict = matches!(&t.kind, TokenKind::Str(s) if s == "use strict");
        if !is_use_strict {
            return false;
        }
        let n = self.peek_at(1);
        match &n.kind {
            TokenKind::Punct(Punct::Semi)
            | TokenKind::Punct(Punct::RBrace)
            | TokenKind::Eof => true,
            _ => n.line > t.line,
        }
    }

    /// phase 8：严格模式下重复形参是 SyntaxError（函数/箭头/对象方法共用）。
    fn check_dup_params(&self, params: &[Param]) -> Result<(), ParseError> {
        let mut seen = HashSet::new();
        for p in params {
            if !seen.insert(p.name.clone()) {
                return self.err(format!(
                    "duplicate parameter name '{}' in strict mode",
                    p.name
                ));
            }
        }
        Ok(())
    }
    /// phase 8：legacy 八进制字面量判定（`010`、`077`；`0x`/`0o`/`0b`/
    /// `0.5`/`0` 不算）。词法器已按十进制求值，这里只做严格模式拦截用。
    fn is_legacy_octal_raw(raw: &str) -> bool {
        let b = raw.as_bytes();
        b.len() > 1 && b[0] == b'0' && b[1].is_ascii_digit()
    }

    /// phase 8：标记当前作用域出现过 legacy 八进制字面量。
    fn flag_octal(&mut self, raw: &str) {
        if Self::is_legacy_octal_raw(raw) {
            if let Some(top) = self.octal_scopes.last_mut() {
                *top = true;
            }
        }
    }

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let (sl, sc) = self.cur_pos();
        // phase 8：脚本级指令序言（必须在任何语句解析之前嗅探）。
        let prog_strict = self.peek_directive();
        let saved_strict = std::mem::replace(&mut self.strict, prog_strict);
        self.octal_scopes.push(false);
        let mut body = Vec::new();
        while !self.at_eof() {
            body.push(self.parse_stmt()?);
        }
        let saw_octal = self.octal_scopes.pop().unwrap_or(false);
        self.strict = saved_strict;
        if prog_strict && saw_octal {
            return self.err("octal literals are not allowed in strict mode");
        }
        let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
        Ok(Program {
            span,
            body,
            strict: prog_strict,
        })
    }

    // ---------- 语句 ----------

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        // `async function` 声明（phase 7）。`async` 与 `function` 之间不允许换行，
        // 否则按普通标识符处理（`async` 当表达式语句）。
        if matches!(self.peek().kind, TokenKind::Keyword(Keyword::Async))
            && matches!(self.peek_at(1).kind, TokenKind::Keyword(Keyword::Function))
            && self.peek().line == self.peek_at(1).line
        {
            let (sl, sc) = self.cur_pos();
            self.bump(); // async
            let f = self.parse_function(true, true)?;
            // Phase 15：async 生成器已支持。
            return Ok(self.spanned(sl, sc, StmtKind::FunctionDecl(Box::new(f))));
        }
        // debugger 语句（lexer 里 debugger 是普通标识符，不是关键字）。
        if matches!(&self.peek().kind, TokenKind::Ident(n) if n == "debugger") {
            let (sl, sc) = self.cur_pos();
            self.bump();
            self.expect_semi()?;
            return Ok(self.spanned(sl, sc, StmtKind::Debugger));
        }
        // phase 8：`with` 语句——引擎不支持（严格模式下它是 SyntaxError）。
        // 在表达式路径之前显式拦截，否则会报含糊的 "expected ';'"。
        if matches!(&self.peek().kind, TokenKind::Ident(n) if n == "with")
            && matches!(self.peek_at(1).kind, TokenKind::Punct(Punct::LParen))
        {
            return self.err("with statement is not supported (SyntaxError in strict mode)");
        }
        let (sl, sc) = self.cur_pos();
        let head = self.peek().kind.clone();
        let kind = match head {
            TokenKind::Punct(Punct::Semi) => {
                self.bump();
                StmtKind::Empty
            }
            TokenKind::Punct(Punct::LBrace) => StmtKind::Block(self.parse_block_stmts()?),
            TokenKind::Keyword(Keyword::Var)
            | TokenKind::Keyword(Keyword::Let)
            | TokenKind::Keyword(Keyword::Const) => {
                let vk = self.var_kind_of_peek();
                self.bump();
                let decls = self.parse_var_declarators(vk)?;
                self.expect_semi()?;
                StmtKind::VarDecl { kind: vk, decls }
            }
            TokenKind::Keyword(Keyword::Function) => {
                let f = self.parse_function(true, false)?;
                StmtKind::FunctionDecl(Box::new(f))
            }
            TokenKind::Keyword(Keyword::If) => self.parse_if()?,
            TokenKind::Keyword(Keyword::For) => self.parse_for()?,
            TokenKind::Keyword(Keyword::While) => {
                self.bump();
                self.expect_punct(Punct::LParen)?;
                let test = self.parse_expr()?;
                self.expect_punct(Punct::RParen)?;
                let body = self.parse_stmt()?;
                StmtKind::While {
                    test,
                    body: Box::new(body),
                }
            }
            TokenKind::Keyword(Keyword::Do) => {
                self.bump();
                let body = self.parse_stmt()?;
                self.expect_keyword(Keyword::While)?;
                self.expect_punct(Punct::LParen)?;
                let test = self.parse_expr()?;
                self.expect_punct(Punct::RParen)?;
                self.expect_semi()?;
                StmtKind::DoWhile {
                    body: Box::new(body),
                    test,
                }
            }
            TokenKind::Keyword(Keyword::Return) => self.parse_return()?,
            TokenKind::Keyword(Keyword::Break) => {
                self.bump();
                if matches!(self.peek().kind, TokenKind::Ident(_)) && !self.newline() {
                    return self.err("statement labels are not yet supported (TODO)");
                }
                self.expect_semi()?;
                StmtKind::Break
            }
            TokenKind::Keyword(Keyword::Continue) => {
                self.bump();
                if matches!(self.peek().kind, TokenKind::Ident(_)) && !self.newline() {
                    return self.err("statement labels are not yet supported (TODO)");
                }
                self.expect_semi()?;
                StmtKind::Continue
            }
            TokenKind::Keyword(Keyword::Throw) => {
                self.bump();
                // 受限产生式：throw 后不允许换行。
                if self.newline() {
                    return self.err("throw must be followed by an expression on the same line");
                }
                let e = self.parse_expr()?;
                self.expect_semi()?;
                StmtKind::Throw(e)
            }
            TokenKind::Keyword(Keyword::Try) => self.parse_try()?,
            TokenKind::Keyword(Keyword::Switch) => self.parse_switch()?,
            TokenKind::Keyword(Keyword::Class) => {
                let c = self.parse_class(true)?;
                StmtKind::ClassDecl(Box::new(c))
            }
            TokenKind::Keyword(Keyword::Import) => self.parse_import()?,
            TokenKind::Keyword(Keyword::Export) => self.parse_export()?,
            _ => {
                let e = self.parse_expr()?;
                self.expect_semi()?;
                StmtKind::Expr(e)
            }
        };
        Ok(self.spanned(sl, sc, kind))
    }

    fn parse_block_stmts(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect_punct(Punct::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at_punct(Punct::RBrace) {
            if self.at_eof() {
                return self.err("unterminated block");
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(stmts)
    }

    fn parse_var_declarators(&mut self, kind: VarKind) -> Result<Vec<VarDeclarator>, ParseError> {
        let mut decls = Vec::new();
        loop {
            if self.at_punct(Punct::LBrace) || self.at_punct(Punct::LBracket) {
                // Phase 15：解构声明 → 展开为多个 declarator（`var $t = o, a = $t.a`）。
                let pat = self.parse_pat(false)?;
                self.expect_punct(Punct::Assign)?;
                let init = self.parse_assignment()?;
                self.emit_pat_decl(&pat, init, kind, &mut decls)?;
            } else {
                let id = self.expect_binding_ident()?;
                let init = if self.eat_punct(Punct::Assign) {
                    Some(self.parse_assignment()?)
                } else {
                    None
                };
                decls.push(VarDeclarator { id, init });
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(decls)
    }

    fn parse_if(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::If)?;
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expr()?;
        self.expect_punct(Punct::RParen)?;
        let cons = self.parse_stmt()?;
        let alt = if self.eat_keyword(Keyword::Else) {
            Some(Box::new(self.parse_stmt()?))
        } else {
            None
        };
        Ok(StmtKind::If {
            test,
            cons: Box::new(cons),
            alt,
        })
    }

    fn parse_for(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::For)?;
        self.expect_punct(Punct::LParen)?;

        enum InitTmp {
            None,
            Var(VarKind, Vec<VarDeclarator>),
            Expr(Expr),
            /// Phase 15：for-in/of 的解构左部（kind, 临时变量名, 模式）。
            PatFor(VarKind, String, Pat),
        }

        // init 部分顶层禁用 `in`（for-in 判定用）。
        let init_tmp = self.with_allow_in(false, |p| {
            if p.at_punct(Punct::Semi) {
                Ok(InitTmp::None)
            } else if matches!(
                p.peek().kind,
                TokenKind::Keyword(Keyword::Var)
                    | TokenKind::Keyword(Keyword::Let)
                    | TokenKind::Keyword(Keyword::Const)
            ) {
                let k = p.var_kind_of_peek();
                // Phase 15：`for (var {a} of b)` / `for (var [a] = x;;)` —— 模式开头。
                if matches!(
                    p.peek_at(1).kind,
                    TokenKind::Punct(Punct::LBrace) | TokenKind::Punct(Punct::LBracket)
                ) {
                    p.bump(); // var/let/const
                    let pat = p.parse_pat(false)?;
                    if p.eat_punct(Punct::Assign) {
                        // 普通 for：`for (var {a} = o;;)` → 展开 declarator。
                        let init = p.parse_assignment()?;
                        let mut decls = Vec::new();
                        p.emit_pat_decl(&pat, init, k, &mut decls)?;
                        return Ok(InitTmp::Var(k, decls));
                    }
                    // for-in/of：左部改写为临时变量，模式脱糖前置到循环体。
                    let temp = p.fresh_dtmp();
                    return Ok(InitTmp::PatFor(k, temp, pat));
                }
                p.bump();
                let decls = p.parse_var_declarators(k)?;
                Ok(InitTmp::Var(k, decls))
            } else {
                let e = p.parse_expr()?;
                Ok(InitTmp::Expr(e))
            }
        })?;

        // for-in / for-of 判定
        let is_in = self.at_keyword(Keyword::In);
        let is_of = self.at_keyword(Keyword::Of);
        if is_in || is_of {
            self.bump();
            // Phase 15：解构左部 → `for (var $t of b) { var {a} = $t; <body> }`。
            let mut pat_for: Option<(VarKind, String, Pat)> = None;
            let left = match init_tmp {
                InitTmp::Var(k, mut decls) => {
                    if decls.len() != 1 || decls[0].init.is_some() {
                        return self.err(
                            "for-in/of allows only a single var/let/const declarator without initializer",
                        );
                    }
                    ForLeft::VarDecl {
                        kind: k,
                        name: decls.pop().unwrap().id,
                    }
                }
                InitTmp::PatFor(k, temp, pat) => {
                    pat_for = Some((k, temp.clone(), pat));
                    ForLeft::VarDecl { kind: k, name: temp }
                }
                InitTmp::Expr(e) => {
                    if !is_assign_target(&e) {
                        return self.err_at(e.span, "invalid for-in/of left-hand side");
                    }
                    ForLeft::Expr(e)
                }
                InitTmp::None => return self.err("expected a left-hand side for for-in/of"),
            };
            let right = self.parse_expr()?;
            self.expect_punct(Punct::RParen)?;
            let body = self.parse_stmt()?;
            let body = match pat_for {
                Some((k, temp, pat)) => {
                    let mut decls = Vec::new();
                    self.emit_pat_decl(&pat, self.d_ident(&temp), k, &mut decls)?;
                    let (bsl, bsc) = self.cur_pos();
                    Box::new(self.spanned(
                        bsl,
                        bsc,
                        StmtKind::Block(vec![self.d_var_decl(k, decls), body]),
                    ))
                }
                None => Box::new(body),
            };
            return Ok(StmtKind::ForInOf {
                is_of,
                left,
                right,
                body,
            });
        }

        // 普通 for(;;)
        let init = match init_tmp {
            InitTmp::None => {
                self.expect_punct(Punct::Semi)?;
                None
            }
            InitTmp::Var(k, decls) => {
                self.expect_punct(Punct::Semi)?;
                Some(ForInit::VarDecl { kind: k, decls })
            }
            InitTmp::Expr(e) => {
                self.expect_punct(Punct::Semi)?;
                Some(ForInit::Expr(e))
            }
            InitTmp::PatFor(..) => {
                return self.err("destructuring pattern in for(;;) requires an initializer");
            }
        };
        let test = if self.at_punct(Punct::Semi) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_punct(Punct::Semi)?;
        let update = if self.at_punct(Punct::RParen) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_punct(Punct::RParen)?;
        let body = self.parse_stmt()?;
        Ok(StmtKind::For {
            init,
            test,
            update,
            body: Box::new(body),
        })
    }

    fn parse_return(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::Return)?;
        // 受限产生式：return 后换行 → 直接结束，不消费下一行。
        if self.at_punct(Punct::Semi) {
            self.bump();
            return Ok(StmtKind::Return(None));
        }
        if self.at_punct(Punct::RBrace) || self.at_eof() || self.newline() {
            return Ok(StmtKind::Return(None));
        }
        let e = self.parse_expr()?;
        self.expect_semi()?;
        Ok(StmtKind::Return(Some(e)))
    }

    fn parse_try(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::Try)?;
        let block = self.parse_block_stmts()?;
        let handler = if self.eat_keyword(Keyword::Catch) {
            let (param, pat_prefix) = if self.eat_punct(Punct::LParen) {
                if self.at_punct(Punct::LBrace) || self.at_punct(Punct::LBracket) {
                    // Phase 15：`catch ({message})` → `catch ($t) { var {message} = $t; ... }`。
                    let pat = self.parse_pat(false)?;
                    self.expect_punct(Punct::RParen)?;
                    let temp = self.fresh_dtmp();
                    let mut decls = Vec::new();
                    self.emit_pat_decl(&pat, self.d_ident(&temp), VarKind::Let, &mut decls)?;
                    (Some(temp), Some(self.d_var_decl(VarKind::Let, decls)))
                } else {
                    let n = self.expect_ident()?;
                    self.expect_punct(Punct::RParen)?;
                    (Some(n), None)
                }
            } else {
                (None, None)
            };
            let mut body = self.parse_block_stmts()?;
            if let Some(prefix) = pat_prefix {
                let mut new_body = vec![prefix];
                new_body.extend(body);
                body = new_body;
            }
            Some(CatchClause { param, body })
        } else {
            None
        };
        let finalizer = if self.eat_keyword(Keyword::Finally) {
            Some(self.parse_block_stmts()?)
        } else {
            None
        };
        if handler.is_none() && finalizer.is_none() {
            return self.err("try must have a catch or finally clause");
        }
        Ok(StmtKind::Try {
            block,
            handler,
            finalizer,
        })
    }

    fn parse_switch(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::Switch)?;
        self.expect_punct(Punct::LParen)?;
        let disc = self.parse_expr()?;
        self.expect_punct(Punct::RParen)?;
        self.expect_punct(Punct::LBrace)?;
        let mut cases = Vec::new();
        while !self.at_punct(Punct::RBrace) {
            if self.at_eof() {
                return self.err("unterminated switch body");
            }
            let test = if self.eat_keyword(Keyword::Case) {
                Some(self.parse_expr()?)
            } else if self.eat_keyword(Keyword::Default) {
                None
            } else {
                return self.err("expected 'case' or 'default' in switch body");
            };
            self.expect_punct(Punct::Colon)?;
            let mut body = Vec::new();
            while !self.at_punct(Punct::RBrace)
                && !self.at_eof()
                && !self.at_keyword(Keyword::Case)
                && !self.at_keyword(Keyword::Default)
            {
                body.push(self.parse_stmt()?);
            }
            cases.push(SwitchCase { test, body });
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(StmtKind::Switch { disc, cases })
    }

    // ---------- 模块（phase 7） ----------

    /// `import {a, b as c} from 'mod'` / `import 'mod'`。
    /// 只支持命名导入（default / namespace import 暂不支持）。
    /// 注意：`as` / `from` 是标识符（非关键字），按 Ident 匹配。
    fn parse_import(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::Import)?;
        let mut specs = Vec::new();
        if self.at_punct(Punct::LBrace) {
            self.bump(); // {
            loop {
                if self.at_punct(Punct::RBrace) {
                    break;
                }
                let imported = self.expect_ident()?;
                let local = if self.eat_ident("as") {
                    self.expect_ident()?
                } else {
                    imported.clone()
                };
                specs.push(ImportSpec { local, imported });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RBrace)?;
            if !self.eat_ident("from") {
                return self.err("expected 'from' after import list");
            }
        } else if matches!(self.peek().kind, TokenKind::Str(_)) {
            // `import 'mod'`（副作用导入）：specs 为空。
        } else {
            return self.err(
                "only named imports (`import {a} from 'm'`) and side-effect imports are supported",
            );
        }
        let source = match &self.peek().kind {
            TokenKind::Str(s) => {
                let src = s.clone();
                self.bump();
                src
            }
            _ => return self.err("import source must be a string literal"),
        };
        self.expect_semi()?;
        Ok(StmtKind::Import { specs, source })
    }

    /// `export const x = 1` / `export function f() {}` / `export {a, b as c}`。
    fn parse_export(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_keyword(Keyword::Export)?;
        match &self.peek().kind {
            TokenKind::Keyword(Keyword::Const)
            | TokenKind::Keyword(Keyword::Let)
            | TokenKind::Keyword(Keyword::Var) => {
                let vk = self.var_kind_of_peek();
                self.bump();
                let decls = self.parse_var_declarators(vk)?;
                self.expect_semi()?;
                Ok(StmtKind::ExportDecl { kind: vk, decls })
            }
            TokenKind::Keyword(Keyword::Function) => {
                let f = self.parse_function(true, false)?;
                Ok(StmtKind::ExportFunc(Box::new(f)))
            }
            TokenKind::Keyword(Keyword::Async)
                if matches!(self.peek_at(1).kind, TokenKind::Keyword(Keyword::Function)) =>
            {
                self.bump(); // async
                let mut f = self.parse_function(true, true)?;
                f.is_async = true;
                Ok(StmtKind::ExportFunc(Box::new(f)))
            }
            TokenKind::Punct(Punct::LBrace) => {
                self.bump(); // {
                let mut names = Vec::new();
                loop {
                    if self.at_punct(Punct::RBrace) {
                        break;
                    }
                    let local = self.expect_ident()?;
                    let exported = if self.eat_ident("as") {
                        self.expect_ident()?
                    } else {
                        local.clone()
                    };
                    names.push((exported, local));
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
                self.expect_punct(Punct::RBrace)?;
                self.expect_semi()?;
                Ok(StmtKind::ExportNames(names))
            }
            _ => self.err("unsupported export form (expected const/let/var/function/{...})"),
        }
    }

    // ---------- 函数 ----------
    /// 解析 `function`（声明/表达式共用）。`is_decl` 为 true 时名字必填。
    /// `is_async` 为 true 时函数体内允许 `await`（phase 7）。
    fn parse_function(
        &mut self,
        is_decl: bool,
        is_async: bool,
    ) -> Result<FunctionNode, ParseError> {
        self.expect_keyword(Keyword::Function)?;
        let is_generator = self.eat_punct(Punct::Star);
        let id = if matches!(self.peek().kind, TokenKind::Ident(_)) {
            Some(self.expect_ident()?)
        } else if is_decl {
            return self.err("function declaration requires a name");
        } else {
            None
        };
        let mut pat_params = Vec::new();
        let params = self.parse_params(&mut pat_params)?;
        // phase 8：函数体自带严格作用域（指令序言 || 继承外层）。
        // phase 9：生成器上下文——生成器函数体内 `yield` 为关键字；
        // 非生成器 function 重置（箭头函数不走这里，继承外层）。
        let (mut body, eff_strict) = if is_generator {
            self.with_generator(|p| p.parse_function_body(is_async))?
        } else {
            self.with_generator_reset(|p| p.parse_function_body(is_async))?
        };
        // Phase 15：解构形参脱糖前置到函数体。
        self.prepend_pat_params(&pat_params, &mut body)?;
        // phase 8：严格模式下重复形参是 SyntaxError（解构绑定名同样检查）。
        if eff_strict {
            self.check_dup_params(&params)?;
            self.check_dup_pat_params(&pat_params)?;
        }
        Ok(FunctionNode {
            id,
            params,
            body,
            is_generator,
            is_async,
            strict: eff_strict,
        })
    }

    /// phase 8：解析函数体 `{ ... }`，返回（语句，有效严格模式）。
    /// 指令序言在 `{` 之后嗅探；legacy 八进制只污染本层作用域。
    fn parse_function_body(
        &mut self,
        is_async: bool,
    ) -> Result<(Vec<Stmt>, bool), ParseError> {
        self.expect_punct(Punct::LBrace)?;
        self.parse_strict_body(is_async)
    }

    /// phase 8：`{` 已消费，解析语句直到 `}`。
    /// 调用方负责 `{` 的消费与严格作用域管理。
    fn parse_stmts_until_rbrace(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        while !self.at_punct(Punct::RBrace) {
            if self.at_eof() {
                return self.err("unterminated block");
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(stmts)
    }

    /// phase 8：在 `{` 已消费的前提下，以"指令序言 || 继承"的严格度解析
    /// 函数体，返回（语句，有效严格度）。octal 作用域按函数隔离。
    fn parse_strict_body(
        &mut self,
        is_async: bool,
    ) -> Result<(Vec<Stmt>, bool), ParseError> {
        let eff_strict = self.strict || self.peek_directive();
        let saved_strict = std::mem::replace(&mut self.strict, eff_strict);
        self.octal_scopes.push(false);
        let body = if is_async {
            self.with_async(|p| p.parse_stmts_until_rbrace())?
        } else {
            self.parse_stmts_until_rbrace()?
        };
        let saw_octal = self.octal_scopes.pop().unwrap_or(false);
        self.strict = saved_strict;
        if eff_strict && saw_octal {
            return self.err("octal literals are not allowed in strict mode");
        }
        Ok((body, eff_strict))
    }

    fn parse_params(&mut self, pat_params: &mut Vec<PatParam>) -> Result<Vec<Param>, ParseError> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        while !self.at_punct(Punct::RParen) {
            if self.at_eof() {
                return self.err("unterminated parameter list");
            }
            // Phase 15：解构形参 → 临时形参名 + 体前脱糖。
            if self.at_punct(Punct::LBrace) || self.at_punct(Punct::LBracket) {
                let pat = self.parse_pat(false)?;
                let default = if self.eat_punct(Punct::Assign) {
                    Some(self.parse_assignment()?)
                } else {
                    None
                };
                let temp = self.fresh_dtmp();
                pat_params.push(PatParam {
                    temp: temp.clone(),
                    pat,
                    default,
                });
                params.push(Param {
                    name: temp,
                    default: None,
                });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
                continue;
            }
            if self.eat_punct(Punct::Ellipsis) {
                return self.err("rest parameters are not yet supported (TODO)");
            }
            let name = self.expect_ident()?;
            let default = if self.eat_punct(Punct::Assign) {
                Some(self.parse_assignment()?)
            } else {
                None
            };
            params.push(Param { name, default });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(params)
    }

    fn parse_arg_list(&mut self) -> Result<Vec<Expr>, ParseError> {
        // 调用方已消费 `(`。
        let mut args = Vec::new();
        while !self.at_punct(Punct::RParen) {
            if self.at_eof() {
                return self.err("unterminated argument list");
            }
            if self.at_punct(Punct::Ellipsis) {
                return self.err("spread in call arguments is not yet supported (TODO)");
            }
            args.push(self.parse_assignment()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(args)
    }

    // ---------- 表达式 ----------

    /// 顶层表达式（含逗号序列）。
    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        let first = self.parse_assignment()?;
        if !self.at_punct(Punct::Comma) {
            return Ok(first);
        }
        let mut exprs = vec![first];
        while self.eat_punct(Punct::Comma) {
            exprs.push(self.parse_assignment()?);
        }
        Ok(self.spanned(sl, sc, ExprKind::Sequence(exprs)))
    }

    fn parse_assignment(&mut self) -> Result<Expr, ParseError> {
        // async 箭头：`async x => ...`（必须在 `x =>` 分支之前判定，
        // 否则 `async` 会被当成参数名）。
        if matches!(self.peek().kind, TokenKind::Keyword(Keyword::Async))
            && matches!(self.peek_at(1).kind, TokenKind::Ident(_))
            && self.peek_at_is(2, Punct::Arrow)
        {
            let (sl, sc) = self.cur_pos();
            self.bump(); // async
            let t = self.bump(); // 参数名
            let name = match t.kind {
                TokenKind::Ident(n) => n,
                _ => t.lexeme.clone(),
            };
            self.bump(); // =>
            let (body, arrow_strict) = self.with_async(|p| p.parse_arrow_body(true))?;
            return Ok(self.spanned(
                sl,
                sc,
                ExprKind::ArrowFunction(Box::new(ArrowFunction {
                    params: vec![Param { name, default: None }],
                    body,
                    is_async: true,
                    strict: arrow_strict,
                })),
            ));
        }
        // async 箭头：`async (params) => ...`（回溯试探；失败则把 async 吐回）。
        if matches!(self.peek().kind, TokenKind::Keyword(Keyword::Async))
            && matches!(self.peek_at(1).kind, TokenKind::Punct(Punct::LParen))
        {
            let m = self.mark();
            self.bump(); // async
            match self.try_parse_arrow_paren(true) {
                Ok(Some(e)) => return Ok(e),
                Ok(None) => self.restore(m),
                Err(e) => return Err(e),
            }
        }
        // 箭头函数：`x => ...`（x 可为 async 关键字这种"假标识符"）
        let head = self.peek().kind.clone();
        let head_is_ident = matches!(head, TokenKind::Ident(_))
            || matches!(head, TokenKind::Keyword(Keyword::Async));
        if head_is_ident && self.peek_at_is(1, Punct::Arrow) {
            let t = self.bump();
            let name = match t.kind {
                TokenKind::Ident(n) => n,
                _ => t.lexeme.clone(), // async
            };
            let (sl, sc) = (t.line, t.col);
            self.bump(); // =>
            let (body, arrow_strict) = self.parse_arrow_body(false)?;
            return Ok(self.spanned(
                sl,
                sc,
                ExprKind::ArrowFunction(Box::new(ArrowFunction {
                    params: vec![Param { name, default: None }],
                    body,
                    is_async: false,
                    strict: arrow_strict,
                })),
            ));
        }
        // 箭头函数：`(params) => ...`（回溯试探，失败则当普通括号表达式）
        if self.at_punct(Punct::LParen) {
            let m = self.mark();
            match self.try_parse_arrow_paren(false) {
                Ok(Some(e)) => return Ok(e),
                Ok(None) => self.restore(m),
                Err(e) => return Err(e),
            }
        }

        let (sl, sc) = self.cur_pos();
        // Phase 15：解构赋值 `([a] = x)` / `({a} = x)`（回溯试探；
        // 不是解构则恢复现场走普通字面量路径）。
        if self.at_punct(Punct::LBracket) || self.at_punct(Punct::LBrace) {
            if let Some(e) = self.try_parse_destructure_assign()? {
                return Ok(e);
            }
        }
        let left = self.parse_conditional()?;
        let op = match self.peek().kind.clone() {
            TokenKind::Punct(Punct::Assign) => Some(AssignOp::Assign),
            TokenKind::Punct(Punct::PlusAssign) => Some(AssignOp::AddAssign),
            TokenKind::Punct(Punct::MinusAssign) => Some(AssignOp::SubAssign),
            TokenKind::Punct(Punct::StarAssign) => Some(AssignOp::MulAssign),
            TokenKind::Punct(Punct::SlashAssign) => Some(AssignOp::DivAssign),
            TokenKind::Punct(Punct::PercentAssign) => Some(AssignOp::ModAssign),
            TokenKind::Punct(Punct::PowAssign) => Some(AssignOp::PowAssign),
            TokenKind::Punct(Punct::ShlAssign) => Some(AssignOp::ShlAssign),
            TokenKind::Punct(Punct::ShrAssign) => Some(AssignOp::ShrAssign),
            TokenKind::Punct(Punct::UShrAssign) => Some(AssignOp::UShrAssign),
            TokenKind::Punct(Punct::AmpAssign) => Some(AssignOp::BitAndAssign),
            TokenKind::Punct(Punct::PipeAssign) => Some(AssignOp::BitOrAssign),
            TokenKind::Punct(Punct::CaretAssign) => Some(AssignOp::BitXorAssign),
            TokenKind::Punct(Punct::AmpAmpAssign) => Some(AssignOp::AndAssign),
            TokenKind::Punct(Punct::PipePipeAssign) => Some(AssignOp::OrAssign),
            TokenKind::Punct(Punct::QuestionQuestionAssign) => Some(AssignOp::NullishAssign),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            if !is_assign_target(&left) {
                return self.err_at(left.span, "invalid assignment target");
            }
            let right = self.parse_assignment()?; // 右结合
            return Ok(self.spanned(
                sl,
                sc,
                ExprKind::Assign {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            ));
        }
        Ok(left)
    }

    /// 试探 `(params) =>` 形状。Ok(None) = 不是箭头（调用方回溯）；
    /// Ok(Some) = 箭头解析成功；Err = 已确定是箭头但内部出错。
    fn try_parse_arrow_paren(&mut self, is_async: bool) -> Result<Option<Expr>, ParseError> {
        let (sl, sc) = self.cur_pos();
        self.bump(); // (
        let mut params = Vec::new();
        // Phase 15：箭头函数的解构形参。
        let mut pat_params: Vec<PatParam> = Vec::new();
        loop {
            if self.at_punct(Punct::RParen) {
                break;
            }
            if self.at_eof() {
                return Ok(None);
            }
            // Phase 15：解构形参（`({a}) =>` / `([b]) =>`）。
            if self.at_punct(Punct::LBrace) || self.at_punct(Punct::LBracket) {
                let pat = match self.parse_pat(false) {
                    Ok(p) => p,
                    Err(_) => return Ok(None),
                };
                let default = if self.eat_punct(Punct::Assign) {
                    match self.parse_assignment() {
                        Ok(e) => Some(e),
                        Err(_) => return Ok(None),
                    }
                } else {
                    None
                };
                let temp = self.fresh_dtmp();
                pat_params.push(PatParam {
                    temp: temp.clone(),
                    pat,
                    default,
                });
                params.push(Param {
                    name: temp,
                    default: None,
                });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
                continue;
            }
            // 只接受简单形参；rest 交给正常表达式路径报错。
            if self.at_punct(Punct::Ellipsis) {
                return Ok(None);
            }
            let name = match self.expect_ident() {
                Ok(n) => n,
                Err(_) => return Ok(None),
            };
            let default = if self.eat_punct(Punct::Assign) {
                match self.parse_assignment() {
                    Ok(e) => Some(e),
                    Err(_) => return Ok(None),
                }
            } else {
                None
            };
            params.push(Param { name, default });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if !self.eat_punct(Punct::RParen) {
            return Ok(None);
        }
        if !self.at_punct(Punct::Arrow) {
            return Ok(None);
        }
        self.bump(); // =>
        let (body, arrow_strict) = if is_async {
            self.with_async(|p| p.parse_arrow_body(true))?
        } else {
            self.parse_arrow_body(false)?
        };
        // phase 8：严格模式下箭头函数重复形参同样是 SyntaxError。
        if arrow_strict {
            self.check_dup_params(&params)?;
            if self.check_dup_pat_params(&pat_params).is_err() {
                return Ok(None);
            }
        }
        // Phase 15：解构形参脱糖前置（表达式体包成块）。
        let body = if pat_params.is_empty() {
            body
        } else {
            let mut stmts = match body {
                ArrowBody::Expr(e) => vec![Spanned::new(
                    self.d_sp(),
                    StmtKind::Return(Some(*e)),
                )],
                ArrowBody::Block(s) => s,
            };
            if self.prepend_pat_params(&pat_params, &mut stmts).is_err() {
                return Ok(None);
            }
            ArrowBody::Block(stmts)
        };
        Ok(Some(self.spanned(
            sl,
            sc,
            ExprKind::ArrowFunction(Box::new(ArrowFunction {
                params,
                body,
                is_async,
                strict: arrow_strict,
            })),
        )))
    }

    /// phase 8：解析箭头函数体，返回（体，有效严格模式）。
    /// 块体 `{ "use strict"; ... }` 自带指令序言检测；表达式体直接继承外层。
    fn parse_arrow_body(&mut self, is_async: bool) -> Result<(ArrowBody, bool), ParseError> {
        if self.at_punct(Punct::LBrace) {
            self.bump(); // {
            let (stmts, eff_strict) = self.parse_strict_body(is_async)?;
            Ok((ArrowBody::Block(stmts), eff_strict))
        } else {
            let e = self.parse_assignment()?;
            Ok((ArrowBody::Expr(Box::new(e)), self.strict))
        }
    }

    fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        let test = self.parse_binary(3)?; // 从 ?? 层开始
        if !self.eat_punct(Punct::Question) {
            return Ok(test);
        }
        let cons = self.parse_assignment()?;
        self.expect_punct(Punct::Colon)?;
        let alt = self.parse_assignment()?;
        Ok(self.spanned(
            sl,
            sc,
            ExprKind::Conditional {
                test: Box::new(test),
                cons: Box::new(cons),
                alt: Box::new(alt),
            },
        ))
    }

    /// 查看下一个 token 是否为二元运算符；返回 (优先级, 右结合?, op)。
    /// 优先级（数字越大越紧）：??=3, ||=4, &&=5, |=6, ^=7, &=8,
    /// ==族=9, 关系=10, 位移=11, 加减=12, 乘除=13, **=14(右结合)。
    fn peek_binop(&self) -> Option<(u8, bool, BinOp)> {
        let (prec, right, op) = match &self.tokens[self.pos.min(self.tokens.len() - 1)].kind {
            TokenKind::Punct(Punct::QuestionQuestion) => (3, false, BinOp::Logical(LogicalOp::Nullish)),
            TokenKind::Punct(Punct::PipePipe) => (4, false, BinOp::Logical(LogicalOp::Or)),
            TokenKind::Punct(Punct::AmpAmp) => (5, false, BinOp::Logical(LogicalOp::And)),
            TokenKind::Punct(Punct::Pipe) => (6, false, BinOp::Binary(BinaryOp::BitOr)),
            TokenKind::Punct(Punct::Caret) => (7, false, BinOp::Binary(BinaryOp::BitXor)),
            TokenKind::Punct(Punct::Amp) => (8, false, BinOp::Binary(BinaryOp::BitAnd)),
            TokenKind::Punct(Punct::Eq) => (9, false, BinOp::Binary(BinaryOp::Eq)),
            TokenKind::Punct(Punct::Ne) => (9, false, BinOp::Binary(BinaryOp::Ne)),
            TokenKind::Punct(Punct::StrictEq) => (9, false, BinOp::Binary(BinaryOp::StrictEq)),
            TokenKind::Punct(Punct::StrictNe) => (9, false, BinOp::Binary(BinaryOp::StrictNe)),
            TokenKind::Punct(Punct::Lt) => (10, false, BinOp::Binary(BinaryOp::Lt)),
            TokenKind::Punct(Punct::Le) => (10, false, BinOp::Binary(BinaryOp::Le)),
            TokenKind::Punct(Punct::Gt) => (10, false, BinOp::Binary(BinaryOp::Gt)),
            TokenKind::Punct(Punct::Ge) => (10, false, BinOp::Binary(BinaryOp::Ge)),
            TokenKind::Keyword(Keyword::Instanceof) => {
                (10, false, BinOp::Binary(BinaryOp::Instanceof))
            }
            TokenKind::Keyword(Keyword::In) if self.allow_in => {
                (10, false, BinOp::Binary(BinaryOp::In))
            }
            TokenKind::Punct(Punct::Shl) => (11, false, BinOp::Binary(BinaryOp::Shl)),
            TokenKind::Punct(Punct::Shr) => (11, false, BinOp::Binary(BinaryOp::Shr)),
            TokenKind::Punct(Punct::UShr) => (11, false, BinOp::Binary(BinaryOp::UShr)),
            TokenKind::Punct(Punct::Plus) => (12, false, BinOp::Binary(BinaryOp::Add)),
            TokenKind::Punct(Punct::Minus) => (12, false, BinOp::Binary(BinaryOp::Sub)),
            TokenKind::Punct(Punct::Star) => (13, false, BinOp::Binary(BinaryOp::Mul)),
            TokenKind::Punct(Punct::Slash) => (13, false, BinOp::Binary(BinaryOp::Div)),
            TokenKind::Punct(Punct::Percent) => (13, false, BinOp::Binary(BinaryOp::Mod)),
            TokenKind::Punct(Punct::Pow) => (14, true, BinOp::Binary(BinaryOp::Pow)),
            _ => return None,
        };
        Some((prec, right, op))
    }

    /// precedence climbing：解析优先级 >= min_prec 的二元表达式。
    fn parse_binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        let mut left = self.parse_unary()?;
        loop {
            let (prec, right_assoc, op) = match self.peek_binop() {
                Some(x) => x,
                None => break,
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_binary(next_min)?;
            let kind = match op {
                BinOp::Binary(b) => ExprKind::Binary {
                    op: b,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                BinOp::Logical(l) => ExprKind::Logical {
                    op: l,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            };
            left = self.spanned(sl, sc, kind);
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        // `#x in obj` 私有字段存在检查（phase 9）：`#x` 后紧跟 `in` 时。
        // 类外出现 `#` 直接报错（`a.#x` 走 parse_postfix_rest）。
        if matches!(self.peek().kind, TokenKind::Punct(Punct::Hash)) {
            if self.in_class == 0 {
                return self.err("private name # outside of class");
            }
            if matches!(self.peek_at(1).kind, TokenKind::Ident(_))
                && matches!(self.peek_at(2).kind, TokenKind::Keyword(Keyword::In))
            {
                self.bump(); // #
                let name = self.expect_ident()?;
                return Ok(self.spanned(sl, sc, ExprKind::PrivateName(name)));
            }
            return self.err("unexpected '#' here (private names only appear as obj.#x or #x in obj)");
        }
        // `await` 一元表达式（phase 7）：只在 async 函数内识别，
        // 其他位置 `await` 仍是普通标识符（parse_primary 处理）。
        if self.in_async > 0 && matches!(self.peek().kind, TokenKind::Keyword(Keyword::Await)) {
            self.bump();
            let arg = self.parse_unary()?;
            return Ok(self.spanned(sl, sc, ExprKind::Await(Box::new(arg))));
        }
        // `yield` / `yield expr` / `yield* expr`（phase 9）：只在生成器内识别，
        // 其他位置 `yield` 仍是普通标识符（parse_primary 处理）。
        // 规范要求 yield 与参数间无换行；无参数时直接结束。
        if self.in_generator > 0
            && matches!(self.peek().kind, TokenKind::Keyword(Keyword::Yield))
        {
            self.bump();
            let delegate = self.eat_punct(Punct::Star);
            let no_arg = self.newline()
                || matches!(
                    self.peek().kind,
                    TokenKind::Punct(Punct::Semi)
                        | TokenKind::Punct(Punct::RBrace)
                        | TokenKind::Punct(Punct::RParen)
                        | TokenKind::Punct(Punct::RBracket)
                        | TokenKind::Punct(Punct::Comma)
                        | TokenKind::Punct(Punct::Colon)
                        | TokenKind::Eof
                );
            if delegate && no_arg {
                return self.err("yield* requires an argument");
            }
            let arg = if no_arg {
                None
            } else {
                // 参数为 AssignmentExpression（不含逗号序列）：`yield a, b` 即 `(yield a), b`。
                Some(Box::new(self.parse_assignment()?))
            };
            return Ok(self.spanned(sl, sc, ExprKind::Yield { arg, delegate }));
        }
        let op = match self.peek().kind.clone() {
            TokenKind::Punct(Punct::Minus) => Some(UnaryOp::Neg),
            TokenKind::Punct(Punct::Plus) => Some(UnaryOp::Pos),
            TokenKind::Punct(Punct::Bang) => Some(UnaryOp::Not),
            TokenKind::Punct(Punct::Tilde) => Some(UnaryOp::BitNot),
            TokenKind::Keyword(Keyword::Typeof) => Some(UnaryOp::Typeof),
            TokenKind::Keyword(Keyword::Void) => Some(UnaryOp::Void),
            TokenKind::Keyword(Keyword::Delete) => Some(UnaryOp::Delete),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let arg = self.parse_unary()?;
            return Ok(self.spanned(
                sl,
                sc,
                ExprKind::Unary {
                    op,
                    arg: Box::new(arg),
                },
            ));
        }
        // 前缀 ++ / --
        if self.at_punct(Punct::PlusPlus) || self.at_punct(Punct::MinusMinus) {
            let op = if self.at_punct(Punct::PlusPlus) {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            self.bump();
            let arg = self.parse_unary()?;
            return Ok(self.spanned(
                sl,
                sc,
                ExprKind::Update {
                    op,
                    arg: Box::new(arg),
                    prefix: true,
                },
            ));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        let e = self.parse_lhs()?;
        // 后缀 ++/--：前面不允许换行（受限产生式）。
        if !self.newline()
            && (self.at_punct(Punct::PlusPlus) || self.at_punct(Punct::MinusMinus))
        {
            let op = if self.at_punct(Punct::PlusPlus) {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            self.bump();
            return Ok(self.spanned(
                sl,
                sc,
                ExprKind::Update {
                    op,
                    arg: Box::new(e),
                    prefix: false,
                },
            ));
        }
        Ok(e)
    }

    fn parse_lhs(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        if self.at_keyword(Keyword::New) {
            self.bump();
            let callee = self.parse_new_callee()?;
            let args = if self.at_punct(Punct::LParen) {
                self.bump();
                self.parse_arg_list()?
            } else {
                Vec::new()
            };
            let e = self.spanned(
                sl,
                sc,
                ExprKind::New {
                    callee: Box::new(callee),
                    args,
                },
            );
            // new 之后还可以继续后缀链：(new A()).b、new A().b()
            return self.parse_postfix_rest(e, sl, sc, true);
        }
        let e = self.parse_primary()?;
        self.parse_postfix_rest(e, sl, sc, true)
    }

    /// `new` 的 callee：成员表达式（可嵌套 `new`，但不含调用括号）。
    fn parse_new_callee(&mut self) -> Result<Expr, ParseError> {
        if self.at_keyword(Keyword::New) {
            return self.parse_lhs();
        }
        let (sl, sc) = self.cur_pos();
        let e = self.parse_primary()?;
        self.parse_postfix_rest(e, sl, sc, false)
    }

    /// 统一的后缀链：调用 `(…)`、成员 `.x` / `[x]`、可选链 `?.`。
    /// `allow_call` 为 false 时（`new` 的 callee 内）不消费调用括号。
    fn parse_postfix_rest(
        &mut self,
        mut e: Expr,
        sl: usize,
        sc: usize,
        allow_call: bool,
    ) -> Result<Expr, ParseError> {
        loop {
            if allow_call && self.at_punct(Punct::LParen) {
                self.bump();
                let args = self.parse_arg_list()?;
                let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                e = Spanned::new(
                    span,
                    ExprKind::Call {
                        callee: Box::new(e),
                        args,
                        optional: false,
                    },
                );
            } else if allow_call
                && self.at_punct(Punct::QuestionDot)
                && self.peek_at_is(1, Punct::LParen)
            {
                // ?.(args)
                self.bump();
                self.bump();
                let args = self.parse_arg_list()?;
                let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                e = Spanned::new(
                    span,
                    ExprKind::Call {
                        callee: Box::new(e),
                        args,
                        optional: true,
                    },
                );
            } else if self.eat_punct(Punct::Dot) {
                // phase 9：`obj.#x` 私有字段/方法访问。
                if self.at_punct(Punct::Hash) {
                    if self.in_class == 0 {
                        return self.err("private name # outside of class");
                    }
                    self.bump();
                    let name = self.expect_ident()?;
                    let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                    e = Spanned::new(
                        span,
                        ExprKind::PrivateMember {
                            obj: Box::new(e),
                            name,
                            optional: false,
                        },
                    );
                    continue;
                }
                let (name, name_span) = self.expect_prop_name()?;
                let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                e = Spanned::new(
                    span,
                    ExprKind::Member {
                        obj: Box::new(e),
                        prop: Box::new(Spanned::new(name_span, ExprKind::Ident(name))),
                        computed: false,
                        optional: false,
                    },
                );
            } else if self.at_punct(Punct::LBracket) {
                self.bump();
                let prop = self.parse_expr()?;
                self.expect_punct(Punct::RBracket)?;
                let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                e = Spanned::new(
                    span,
                    ExprKind::Member {
                        obj: Box::new(e),
                        prop: Box::new(prop),
                        computed: true,
                        optional: false,
                    },
                );
            } else if self.at_punct(Punct::QuestionDot) {
                self.bump(); // ?.
                if allow_call && self.at_punct(Punct::LParen) {
                    // a?.(args)
                    self.bump();
                    let args = self.parse_arg_list()?;
                    let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                    e = Spanned::new(
                        span,
                        ExprKind::Call {
                            callee: Box::new(e),
                            args,
                            optional: true,
                        },
                    );
                } else if self.at_punct(Punct::LBracket) {
                    // a?.[b]
                    self.bump();
                    let prop = self.parse_expr()?;
                    self.expect_punct(Punct::RBracket)?;
                    let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                    e = Spanned::new(
                        span,
                        ExprKind::Member {
                            obj: Box::new(e),
                            prop: Box::new(prop),
                            computed: true,
                            optional: true,
                        },
                    );
                } else {
                    // a?.#x（phase 9：私有访问的可选链）。
                    if self.at_punct(Punct::Hash) {
                        if self.in_class == 0 {
                            return self.err("private name # outside of class");
                        }
                        self.bump();
                        let name = self.expect_ident()?;
                        let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                        e = Spanned::new(
                            span,
                            ExprKind::PrivateMember {
                                obj: Box::new(e),
                                name,
                                optional: true,
                            },
                        );
                        continue;
                    }
                    // a?.b（new callee 里遇到 ?. 后跟 `(` 会在这里报"缺属性名"，符合预期）
                    let (name, name_span) = self.expect_prop_name()?;
                    let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                    e = Spanned::new(
                        span,
                        ExprKind::Member {
                            obj: Box::new(e),
                            prop: Box::new(Spanned::new(name_span, ExprKind::Ident(name))),
                            computed: false,
                            optional: true,
                        },
                    );
                }
            } else if allow_call && matches!(self.peek().kind, TokenKind::Template(_)) {
                return self.err("tagged templates are not yet supported (TODO)");
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let t = self.peek().clone();
        let (sl, sc) = (t.line, t.col);
        let kind = match t.kind {
            TokenKind::Number(n) => {
                self.bump();
                // phase 8：legacy 八进制标记（严格作用域下解析期报错）。
                self.flag_octal(&n.raw);
                ExprKind::Literal(Literal::Number(n.value))
            }
            TokenKind::Str(s) => {
                self.bump();
                ExprKind::Literal(Literal::String(s))
            }
            TokenKind::Template(s) => {
                self.bump();
                // Phase 14：`${}` 插值在解析期脱糖为字符串拼接；
                // 无插值时仍为字面量（此时做转义烹制）。
                let kind = self.parse_template_literal(&s)?;
                return Ok(self.spanned(sl, sc, kind));
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                ExprKind::Literal(Literal::Bool(true))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                ExprKind::Literal(Literal::Bool(false))
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.bump();
                ExprKind::Literal(Literal::Null)
            }
            // `undefined` 当作普通标识符：解释器在全局作用域绑定了它，
            // 且允许用户（不推荐地）遮蔽。
            TokenKind::Keyword(Keyword::Undefined) => {
                self.bump();
                ExprKind::Ident("undefined".to_string())
            }
            TokenKind::Keyword(Keyword::This) => {
                self.bump();
                ExprKind::This
            }
            // `super`（phase 9）：`super(...)` / `super.x`。合法性由解释器判定。
            TokenKind::Keyword(Keyword::Super) => {
                self.bump();
                ExprKind::Super
            }
            // 类表达式（phase 9）。
            TokenKind::Keyword(Keyword::Class) => {
                let c = self.parse_class(false)?;
                ExprKind::Class(Box::new(c))
            }
            TokenKind::Ident(name) => {
                self.bump();
                ExprKind::Ident(name)
            }
            // `async function` 表达式（phase 7）：必须在 Async 当标识符之前判定。
            TokenKind::Keyword(Keyword::Async)
                if matches!(self.peek_at(1).kind, TokenKind::Keyword(Keyword::Function))
                    && self.peek().line == self.peek_at(1).line =>
            {
                self.bump(); // async
                let f = self.parse_function(false, true)?;
                // Phase 15：async 生成器已支持。
                ExprKind::Function(Box::new(f))
            }
            // async / await / yield 在子集里当普通标识符用（非严格模式）。
            // 注意：async 函数内 `await` 走 parse_unary 的 Await 分支，
            // 到这里的 Await 一定不在 async 上下文中。
            TokenKind::Keyword(Keyword::Async)
            | TokenKind::Keyword(Keyword::Await)
            | TokenKind::Keyword(Keyword::Yield) => {
                let name = t.lexeme.clone();
                self.bump();
                ExprKind::Ident(name)
            }
            TokenKind::Punct(Punct::LParen) => {
                self.bump();
                // 括号内恢复 `in`（`for ((a in b);;)` 合法）。
                let e = self.with_allow_in(true, |p| p.parse_expr())?;
                self.expect_punct(Punct::RParen)?;
                return Ok(e);
            }
            TokenKind::Punct(Punct::LBracket) => return self.parse_array(),
            TokenKind::Punct(Punct::LBrace) => return self.parse_object(),
            TokenKind::Keyword(Keyword::Function) => {
                let f = self.parse_function(false, false)?;
                ExprKind::Function(Box::new(f))
            }
            // 正则字面量（phase 7）：词法已按前 token 启发式切好。
            TokenKind::Regex { pattern, flags } => {
                self.bump();
                ExprKind::Literal(Literal::Regex { pattern, flags })
            }
            TokenKind::Punct(Punct::Slash) | TokenKind::Punct(Punct::SlashAssign) => {
                return self.err(
                    "unexpected '/' here (a regex literal was not recognized; the lexer heuristic may have misclassified it)",
                );
            }
            _ => return self.err(format!("unexpected token '{}'", t.lexeme)),
        };
        Ok(self.spanned(sl, sc, kind))
    }

    /// Phase 14：模板字面量脱糖。无 `${}` → 烹制后的字符串字面量；
    /// 有插值 → `"" + part1 + part2 ...`（左结合；表达式片段先解析，
    /// 空字符串起手保证一律按字符串拼接）。
    fn parse_template_literal(&mut self, raw: &str) -> Result<ExprKind, ParseError> {
        let parts =
            split_template(raw).map_err(|e| ParseError::new(0, 0, e))?;
        let has_expr = parts.iter().any(|p| matches!(p, TemplatePart::Expr(_)));
        if !has_expr {
            let mut s = String::new();
            for p in parts {
                if let TemplatePart::Str(t) = p {
                    s.push_str(&t);
                }
            }
            return Ok(ExprKind::Literal(Literal::Template(s)));
        }
        let span = Span::new(0, 0, 0, 0);
        let mut acc = ExprKind::Literal(Literal::String(String::new()));
        for p in parts {
            let e = match p {
                TemplatePart::Str(t) => Spanned::new(
                    span,
                    ExprKind::Literal(Literal::String(t)),
                ),
                TemplatePart::Expr(src) => self.parse_template_expr(&src)?,
            };
            let prev = Spanned::new(span, acc);
            acc = ExprKind::Binary {
                op: BinaryOp::Add,
                left: Box::new(prev),
                right: Box::new(e),
            };
        }
        Ok(acc)
    }

    /// 解析模板插值里的表达式源码（子 Parser；span 为近似值）。
    fn parse_template_expr(&mut self, src: &str) -> Result<Expr, ParseError> {
        let tokens = crate::lexer::lex(src)
            .map_err(|e| ParseError::new(0, 0, format!("template expr: {e:?}")))?;
        let mut p = Parser::new(tokens);
        p.parse_expr()
    }

    fn parse_array(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        self.expect_punct(Punct::LBracket)?;
        let mut elems = Vec::new();
        while !self.at_punct(Punct::RBracket) {
            if self.at_eof() {
                return self.err("unterminated array literal");
            }
            if self.eat_punct(Punct::Comma) {
                elems.push(ArrayElem::Hole);
                continue;
            }
            if self.at_punct(Punct::Ellipsis) {
                // phase 9：`[...iter]` 展开（数组/字符串/生成器）。
                self.bump();
                elems.push(ArrayElem::Spread(self.parse_assignment()?));
            } else {
                elems.push(ArrayElem::Expr(self.parse_assignment()?));
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(self.spanned(sl, sc, ExprKind::Array(elems)))
    }

    fn parse_object(&mut self) -> Result<Expr, ParseError> {
        let (sl, sc) = self.cur_pos();
        self.expect_punct(Punct::LBrace)?;
        let mut props = Vec::new();
        while !self.at_punct(Punct::RBrace) {
            if self.at_eof() {
                return self.err("unterminated object literal");
            }
            if self.at_punct(Punct::Ellipsis) {
                return self.err("spread in object literals is not yet supported (TODO)");
            }
            props.push(self.parse_prop()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(self.spanned(sl, sc, ExprKind::Object(props)))
    }

    /// 解析对象 key，返回 (key, 标识符名[如有])。
    fn parse_prop_key(&mut self) -> Result<(PropKey, Option<String>), ParseError> {
        if self.at_punct(Punct::LBracket) {
            self.bump();
            let e = self.parse_assignment()?;
            self.expect_punct(Punct::RBracket)?;
            return Ok((PropKey::Computed(e), None));
        }
        let t = self.peek().clone();
        let (key, name) = match t.kind {
            TokenKind::Ident(n) => (PropKey::Ident(n.clone()), Some(n)),
            TokenKind::Str(s) => (PropKey::String(s), None),
            TokenKind::Number(n) => {
                // phase 8：对象字面量键里的 legacy 八进制同样标记。
                self.flag_octal(&n.raw);
                (PropKey::Number(n.value), None)
            }
            TokenKind::Keyword(k) => {
                let s = k.as_str().to_string();
                (PropKey::Ident(s.clone()), Some(s))
            }
            _ => return self.err(format!("unexpected token '{}' in object literal", t.lexeme)),
        };
        self.bump();
        Ok((key, name))
    }

    fn parse_prop(&mut self) -> Result<Prop, ParseError> {
        // phase 9：生成器方法 `*gen() {}` —— `*` 在 key 之前。
        let is_gen_star = self.at_punct(Punct::Star);
        if is_gen_star {
            self.bump(); // *
        }
        let (key, name) = self.parse_prop_key()?;
        // getter/setter：`get x() {}` / `set x(v) {}`（`{get: 1}` 走普通分支）。
        // 生成器方法不能是 getter/setter。
        let is_accessor = !is_gen_star
            && matches!(&name, Some(n) if n == "get" || n == "set")
            && self.peek_is_prop_key_start()
            && self.peek_at_is(1, Punct::LParen);
        if is_accessor {
            let is_get = name.unwrap() == "get";
            let (inner_key, _) = self.parse_prop_key()?;
            let mut pat_params = Vec::new();
            let params = self.parse_params(&mut pat_params)?;
            // phase 8：getter/setter 同样是独立严格作用域。
            // phase 9：getter/setter 不是生成器，重置 yield 上下文。
            let (mut body, eff_strict) = self.with_generator_reset(|p| p.parse_function_body(false))?;
            self.prepend_pat_params(&pat_params, &mut body)?;
            if eff_strict {
                self.check_dup_params(&params)?;
                self.check_dup_pat_params(&pat_params)?;
            }
            let f = FunctionNode {
                id: None,
                params,
                body,
                is_generator: false,
                is_async: false,
                strict: eff_strict,
            };
            let value = if is_get {
                PropValue::Getter(Box::new(f))
            } else {
                PropValue::Setter(Box::new(f))
            };
            return Ok(Prop {
                key: inner_key,
                value,
            });
        }
        // {a: 1}
        if self.eat_punct(Punct::Colon) {
            let v = self.parse_assignment()?;
            return Ok(Prop {
                key,
                value: PropValue::Init(v),
            });
        }
        // 方法：{f() {}} / {*gen() {}}（`*` 已在函数开头处理）。
        let is_gen = is_gen_star || self.eat_punct(Punct::Star);
        if self.at_punct(Punct::LParen) {
            let mut pat_params = Vec::new();
            let params = self.parse_params(&mut pat_params)?;
            // phase 8：对象方法同样是独立严格作用域。
            // phase 9：`*gen()` 方法体内 `yield` 为关键字。
            let (mut body, eff_strict) = if is_gen {
                self.with_generator(|p| p.parse_function_body(false))?
            } else {
                self.with_generator_reset(|p| p.parse_function_body(false))?
            };
            self.prepend_pat_params(&pat_params, &mut body)?;
            if eff_strict {
                self.check_dup_params(&params)?;
                self.check_dup_pat_params(&pat_params)?;
            }
            let f = FunctionNode {
                id: None,
                params,
                body,
                is_generator: is_gen,
                is_async: false,
                strict: eff_strict,
            };
            return Ok(Prop {
                key,
                value: PropValue::Method(Box::new(f)),
            });
        }
        // 简写：{a}
        if let Some(n) = name {
            return Ok(Prop {
                key,
                value: PropValue::Shorthand(n),
            });
        }
        self.err("expected ':' or '(' after property name")
    }

    // ---------- 类（phase 9） ----------

    /// `class Name extends Super { ... }` / `class { ... }`。
    fn parse_class(&mut self, is_decl: bool) -> Result<ClassNode, ParseError> {
        self.expect_keyword(Keyword::Class)?;
        let id = if matches!(self.peek().kind, TokenKind::Ident(_)) {
            Some(self.expect_ident()?)
        } else if is_decl {
            return self.err("class declaration requires a name");
        } else {
            None
        };
        let super_class = if self.at_keyword(Keyword::Extends) {
            self.bump();
            Some(Box::new(self.parse_assignment()?))
        } else {
            None
        };
        self.expect_punct(Punct::LBrace)?;
        let mut body = Vec::new();
        let mut seen_ctor = false;
        while !self.at_punct(Punct::RBrace) {
            if self.at_eof() {
                return self.err("unterminated class body");
            }
            if self.eat_punct(Punct::Semi) {
                continue;
            }
            let elem = self.with_class(|p| p.parse_class_element())?;
            if matches!(
                &elem,
                ClassElem::Method {
                    kind: MethodKind::Constructor,
                    is_static: false,
                    ..
                }
            ) {
                if seen_ctor {
                    return self.err("duplicate constructor in class body");
                }
                seen_ctor = true;
            }
            body.push(elem);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(ClassNode {
            id,
            super_class,
            body,
        })
    }

    /// 解析类键：`#x` 私有；否则公开键（标识符/关键字/字符串/数字/计算键）。
    fn parse_class_key(&mut self) -> Result<ClassKey, ParseError> {
        if self.eat_punct(Punct::Hash) {
            let name = self.expect_ident()?;
            return Ok(ClassKey::Private(name));
        }
        let (key, _) = self.parse_prop_key()?;
        Ok(ClassKey::Public(key))
    }

    /// `peek_at(n)` 是否为类键起始（`#` 亦可，`get #x()` 用）。
    fn peek_at_is_key_start(&self, n: usize) -> bool {
        matches!(
            &self.peek_at(n).kind,
            TokenKind::Ident(_)
                | TokenKind::Keyword(_)
                | TokenKind::Str(_)
                | TokenKind::Number(_)
                | TokenKind::Punct(Punct::LBracket)
                | TokenKind::Punct(Punct::Hash)
        )
    }

    /// 解析一个类体成员（调用方已保证 in_class > 0）。
    fn parse_class_element(&mut self) -> Result<ClassElem, ParseError> {
        // `static { ... }` 静态块。
        if self.at_keyword(Keyword::Static) && self.peek_at_is(1, Punct::LBrace) {
            self.bump();
            self.expect_punct(Punct::LBrace)?;
            let stmts = self.parse_stmts_until_rbrace()?;
            return Ok(ClassElem::StaticBlock(stmts));
        }
        // `static` 修饰符：后面是 `(`/`=`/`;`/`}` 时它是成员名而非修饰符。
        let is_static = if self.at_keyword(Keyword::Static)
            && !self.peek_at_is(1, Punct::LParen)
            && !matches!(
                self.peek_at(1).kind,
                TokenKind::Punct(Punct::Assign | Punct::Semi | Punct::RBrace)
            ) {
            self.bump();
            true
        } else {
            false
        };
        // `async` 修饰符：后面是 `(`/`=`/`;`/`}`/`,` 时它是成员名而非修饰符；
        // 与 key 之间不允许换行（沿用 async function 的规则）。
        let is_async = if self.at_keyword(Keyword::Async)
            && !self.peek_at_is(1, Punct::LParen)
            && !matches!(
                self.peek_at(1).kind,
                TokenKind::Punct(Punct::Assign | Punct::Semi | Punct::RBrace | Punct::Comma)
            )
            && self.peek().line == self.peek_at(1).line
        {
            self.bump();
            true
        } else {
            false
        };
        // `*` 生成器。
        let is_gen = self.eat_punct(Punct::Star);
        // Phase 15：async 生成器已支持（`async function*` / `async *method()`）。
        // getter/setter：`get x()` / `set x(v)`（无 async/* 修饰，
        // key 为 get/set 且后跟属性键起始再跟 `(`）。
        // 注意：`get`/`set` 在词法层是 Keyword::Get/Keyword::Set，不是 Ident。
        let peek_kind = &self.peek().kind;
        let is_get_or_set_kw = matches!(
            peek_kind,
            TokenKind::Ident(n) if n == "get" || n == "set"
        ) || matches!(
            peek_kind,
            TokenKind::Keyword(Keyword::Get) | TokenKind::Keyword(Keyword::Set)
        );
        let is_get_set = !is_async
            && !is_gen
            && is_get_or_set_kw
            && self.peek_at_is_key_start(1)
            && self.peek_at_is(2, Punct::LParen);
        if is_get_set {
            let is_get = matches!(peek_kind, TokenKind::Ident(n) if n == "get")
                || matches!(peek_kind, TokenKind::Keyword(Keyword::Get));
            self.bump(); // get/set
            let key = self.parse_class_key()?;
            let mut pat_params = Vec::new();
            let params = self.parse_params(&mut pat_params)?;
            // getter/setter 不是生成器。
            let (mut body, eff_strict) =
                self.with_generator_reset(|p| p.parse_function_body(false))?;
            self.prepend_pat_params(&pat_params, &mut body)?;
            if eff_strict {
                self.check_dup_params(&params)?;
                self.check_dup_pat_params(&pat_params)?;
            }
            let func = FunctionNode {
                id: None,
                params,
                body,
                is_generator: false,
                is_async: false,
                strict: eff_strict,
            };
            let kind = if is_get {
                MethodKind::Getter
            } else {
                MethodKind::Setter
            };
            return Ok(ClassElem::Method {
                key,
                func,
                kind,
                is_static,
            });
        }
        let key = self.parse_class_key()?;
        // 方法：`key(...)`。
        if self.at_punct(Punct::LParen) {
            // `#constructor` 非法；`prototype` 不能做字段（方法可以）。
            if matches!(&key, ClassKey::Private(n) if n == "constructor") {
                return self.err("private method cannot be named #constructor");
            }
            let mut pat_params = Vec::new();
            let params = self.parse_params(&mut pat_params)?;
            let (mut body, eff_strict) = if is_gen {
                self.with_generator(|p| p.parse_function_body(is_async))?
            } else if is_async {
                self.with_generator_reset(|p| p.parse_function_body(true))?
            } else {
                self.with_generator_reset(|p| p.parse_function_body(false))?
            };
            self.prepend_pat_params(&pat_params, &mut body)?;
            if eff_strict {
                self.check_dup_params(&params)?;
                self.check_dup_pat_params(&pat_params)?;
            }
            // constructor 判定：非静态、非生成器/async、公开、名为 constructor。
            let is_ctor = !is_static
                && !is_gen
                && !is_async
                && matches!(&key, ClassKey::Public(PropKey::Ident(n)) if n == "constructor");
            let kind = if is_ctor {
                MethodKind::Constructor
            } else {
                MethodKind::Method
            };
            let func = FunctionNode {
                id: None,
                params,
                body,
                is_generator: is_gen,
                is_async,
                strict: eff_strict,
            };
            return Ok(ClassElem::Method {
                key,
                func,
                kind,
                is_static,
            });
        }
        // 字段：`key = init;` / `key;`。
        if is_gen || is_async {
            return self.err("class field cannot be a generator or async");
        }
        if matches!(&key, ClassKey::Public(PropKey::Ident(n)) if n == "prototype") {
            return self.err("class field cannot be named 'prototype'");
        }
        if matches!(&key, ClassKey::Private(n) if n == "constructor") {
            return self.err("private field cannot be named #constructor");
        }
        let init = if self.eat_punct(Punct::Assign) {
            Some(self.parse_assignment()?)
        } else {
            None
        };
        self.expect_semi()?;
        Ok(ClassElem::Field {
            key,
            init,
            is_static,
        })
    }
}

/// 赋值目标是否合法（标识符 / 成员表达式 / 私有成员）。
fn is_assign_target(e: &Expr) -> bool {
    matches!(
        &e.node,
        ExprKind::Ident(_) | ExprKind::Member { .. } | ExprKind::PrivateMember { .. }
    )
}

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 用已切好的 token 解析出 AST。
pub fn parse_program(tokens: &[Token]) -> Result<Program, ParseError> {
    Parser::new(tokens.to_vec()).parse_program()
}

/// 一步到位：源码 → AST（词法错误会转为 ParseError）。
pub fn parse_source(src: &str) -> Result<Program, ParseError> {
    let tokens = crate::lexer::lex(src).map_err(ParseError::from)?;
    parse_program(&tokens)
}

// ---------------------------------------------------------------------------
// Phase 15：解构（解析期脱糖）。
//
// 策略：模式只活在 parser 内部（`Pat`），解析后立即脱糖为普通声明 /
// IIFE，AST、解释器、VM 零改动：
// - 声明 `var {a} = o` → 展开为同一 `VarDecl` 下的多个 declarator
//   （`var $t = o, a = $t.a`），提升/TDZ 语义天然正确；
// - 赋值 `({a} = o)` → IIFE 箭头（`(($p) => { var $t = $p; a = $t.a; return $p; })(o)`），
//   RHS 在外层求值一次，`yield`/`await` 不受影响；
// - 形参 / catch / for-of：形参改写为临时名，脱糖语句前置到函数体。
//
// 已知偏差（文档化）：
// - 无限迭代器的数组解构（`var [a] = infiniteGen()`）会 hang（用了 spread 全消费）；
// - 对象 rest 对 Proxy 只触发值拷贝，不走 set trap（与 `Object.assign` 一致）；
// - `yield*` 暂只支持语句位置（async 生成器）。
// ---------------------------------------------------------------------------

/// 解构模式（parser 内部表示，不进 AST）。
#[derive(Debug, Clone)]
enum Pat {
    /// 绑定：`a`
    Bind(String),
    /// 成员赋值目标：`a.b` / `a[i]`（仅赋值形式）。
    Target(Expr),
    /// 对象：[(键, 模式)] + rest（rest 是完整模式）。
    Object(Vec<(PatKey, Pat)>, Option<Box<Pat>>),
    /// 数组：[元素]（None = 空位）+ rest（rest 是完整模式）。
    Array(Vec<Option<Pat>>, Option<Box<Pat>>),
    /// 默认值：`pat = expr`。
    Default(Box<Pat>, Expr),
}

#[derive(Debug, Clone)]
enum PatKey {
    Name(String),
    Computed(Expr),
}

/// 形参中的模式：临时形参名 + 模式 + 默认值。
#[derive(Debug, Clone)]
struct PatParam {
    temp: String,
    pat: Pat,
    default: Option<Expr>,
}

impl Parser {
    /// 分配解构临时变量名（`__yousj$d<N>`，全局唯一）。
    fn fresh_dtmp(&mut self) -> String {
        let n = self.destructure_tmp;
        self.destructure_tmp += 1;
        format!("__yousj$d{n}")
    }

    // ---- 脱糖用的 AST 小件 ----

    fn d_sp(&self) -> Span {
        Span::point(0, 0)
    }

    fn d_ident(&self, name: &str) -> Expr {
        Spanned::new(self.d_sp(), ExprKind::Ident(name.to_string()))
    }

    fn d_str(&self, s: &str) -> Expr {
        Spanned::new(
            self.d_sp(),
            ExprKind::Literal(Literal::String(s.to_string())),
        )
    }

    fn d_num(&self, n: f64) -> Expr {
        Spanned::new(self.d_sp(), ExprKind::Literal(Literal::Number(n)))
    }

    fn d_undefined(&self) -> Expr {
        self.d_ident("undefined")
    }

    fn d_member(&self, obj: Expr, prop: Expr, computed: bool) -> Expr {
        Spanned::new(
            self.d_sp(),
            ExprKind::Member {
                obj: Box::new(obj),
                prop: Box::new(prop),
                computed,
                optional: false,
            },
        )
    }

    fn d_call(&self, callee: Expr, args: Vec<Expr>) -> Expr {
        Spanned::new(
            self.d_sp(),
            ExprKind::Call {
                callee: Box::new(callee),
                args,
                optional: false,
            },
        )
    }

    fn d_binary(&self, op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Spanned::new(
            self.d_sp(),
            ExprKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
        )
    }

    fn d_assign_expr(&self, left: Expr, right: Expr) -> Expr {
        Spanned::new(
            self.d_sp(),
            ExprKind::Assign {
                op: AssignOp::Assign,
                left: Box::new(left),
                right: Box::new(right),
            },
        )
    }

    fn d_var_decl(&self, kind: VarKind, decls: Vec<VarDeclarator>) -> Stmt {
        Spanned::new(self.d_sp(), StmtKind::VarDecl { kind, decls })
    }

    fn d_expr_stmt(&self, e: Expr) -> Stmt {
        Spanned::new(self.d_sp(), StmtKind::Expr(e))
    }

    // ---- 模式解析 ----

    /// 解析解构模式。`allow_member` 为 true 时允许成员目标（赋值形式）。
    fn parse_pat(&mut self, allow_member: bool) -> Result<Pat, ParseError> {
        if self.at_punct(Punct::LBrace) {
            self.parse_object_pat(allow_member)
        } else if self.at_punct(Punct::LBracket) {
            self.parse_array_pat(allow_member)
        } else {
            self.err("expected destructuring pattern")
        }
    }

    /// 解析模式元素（嵌套模式 / 目标），并处理 `= default`。
    fn parse_pat_element(&mut self, allow_member: bool) -> Result<Pat, ParseError> {
        let mut pat = if self.at_punct(Punct::LBrace) || self.at_punct(Punct::LBracket) {
            self.parse_pat(allow_member)?
        } else {
            self.parse_pat_target(allow_member)?
        };
        if self.eat_punct(Punct::Assign) {
            let def = self.parse_assignment()?;
            pat = Pat::Default(Box::new(pat), def);
        }
        Ok(pat)
    }

    /// 解析单个目标：标识符或成员链（`a.b[i]`）。
    fn parse_pat_target(&mut self, allow_member: bool) -> Result<Pat, ParseError> {
        let t = self.peek().clone();
        // 上下文关键字（await/async/yield）在非严格位置可作标识符。
        let name = match &t.kind {
            TokenKind::Ident(n) => n.clone(),
            TokenKind::Keyword(k)
                if matches!(
                    k.as_str(),
                    "await" | "async" | "yield" | "let" | "static" | "get" | "set"
                ) =>
            {
                k.as_str().to_string()
            }
            _ => {
                return self.err(format!(
                    "expected identifier in destructuring pattern, found '{}'",
                    t.lexeme
                ))
            }
        };
        self.bump();
        let (sl, sc) = (t.line, t.col);
        let mut expr = self.spanned(sl, sc, ExprKind::Ident(name.clone()));
        let mut is_member = false;
        loop {
            if self.at_punct(Punct::Dot) {
                if !allow_member {
                    return self.err("invalid destructuring target");
                }
                self.bump();
                let (pn, _) = self.expect_prop_name()?;
                let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                expr = Spanned::new(
                    span,
                    ExprKind::Member {
                        obj: Box::new(expr),
                        prop: Box::new(Spanned::new(span, ExprKind::Ident(pn))),
                        computed: false,
                        optional: false,
                    },
                );
                is_member = true;
            } else if self.at_punct(Punct::LBracket) {
                if !allow_member {
                    return self.err("invalid destructuring target");
                }
                self.bump();
                let prop = self.parse_expr()?;
                self.expect_punct(Punct::RBracket)?;
                let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
                expr = Spanned::new(
                    span,
                    ExprKind::Member {
                        obj: Box::new(expr),
                        prop: Box::new(prop),
                        computed: true,
                        optional: false,
                    },
                );
                is_member = true;
            } else {
                break;
            }
        }
        if is_member {
            Ok(Pat::Target(expr))
        } else {
            Ok(Pat::Bind(name))
        }
    }

    fn parse_object_pat(&mut self, allow_member: bool) -> Result<Pat, ParseError> {
        self.expect_punct(Punct::LBrace)?;
        let mut elems = Vec::new();
        let mut rest: Option<Box<Pat>> = None;
        loop {
            if self.eat_punct(Punct::RBrace) {
                break;
            }
            if self.eat_punct(Punct::Ellipsis) {
                // rest 元素是完整模式（`...[x]` / `...{a}` / `...a.b`）。
                let r = self.parse_pat_element(allow_member)?;
                rest = Some(Box::new(r));
                self.expect_punct(Punct::RBrace)?;
                break;
            }
            // --- 键 ---
            let (key, is_shorthand_able) = if self.at_punct(Punct::LBracket) {
                self.bump();
                let e = self.parse_assignment()?;
                self.expect_punct(Punct::RBracket)?;
                (PatKey::Computed(e), false)
            } else {
                let t = self.peek().clone();
                let (name, is_ident_like) = match &t.kind {
                    TokenKind::Ident(n) => (n.clone(), true),
                    TokenKind::Str(s) => (s.clone(), false),
                    TokenKind::Number(n) => (n.value.to_string(), false),
                    TokenKind::Keyword(k) => {
                        let s = k.as_str().to_string();
                        // 上下文关键字可作简写属性名。
                        let ident_like = matches!(
                            s.as_str(),
                            "await" | "async" | "yield" | "let" | "static" | "get" | "set"
                        );
                        (s, ident_like)
                    }
                    _ => {
                        return self.err(format!(
                            "unexpected token '{}' in object pattern",
                            t.lexeme
                        ))
                    }
                };
                self.bump();
                (PatKey::Name(name), is_ident_like)
            };
            // --- 值 ---
            let pat = if self.eat_punct(Punct::Colon) {
                self.parse_pat_element(allow_member)?
            } else {
                let name = match &key {
                    PatKey::Name(n) if is_shorthand_able => n.clone(),
                    _ => return self.err("invalid shorthand property in object pattern"),
                };
                let mut p = Pat::Bind(name);
                if self.eat_punct(Punct::Assign) {
                    let def = self.parse_assignment()?;
                    p = Pat::Default(Box::new(p), def);
                }
                p
            };
            elems.push((key, pat));
            if !self.eat_punct(Punct::Comma) {
                self.expect_punct(Punct::RBrace)?;
                break;
            }
        }
        Ok(Pat::Object(elems, rest))
    }

    fn parse_array_pat(&mut self, allow_member: bool) -> Result<Pat, ParseError> {
        self.expect_punct(Punct::LBracket)?;
        let mut elems = Vec::new();
        let mut rest: Option<Box<Pat>> = None;
        loop {
            if self.eat_punct(Punct::RBracket) {
                break;
            }
            if self.eat_punct(Punct::Ellipsis) {
                // rest 元素是完整模式（`...[x]` / `...{a}`）。
                let r = self.parse_pat_element(allow_member)?;
                rest = Some(Box::new(r));
                self.expect_punct(Punct::RBracket)?;
                break;
            }
            if self.eat_punct(Punct::Comma) {
                elems.push(None); // 空位
                continue;
            }
            let pat = self.parse_pat_element(allow_member)?;
            elems.push(Some(pat));
            if !self.eat_punct(Punct::Comma) {
                self.expect_punct(Punct::RBracket)?;
                break;
            }
        }
        Ok(Pat::Array(elems, rest))
    }

    // ---- 脱糖发射 ----

    /// 声明形式：`kind <pat> = <src>` → 追加 declarator。
    /// `src` 只求值一次（内部按需绑临时变量）。
    fn emit_pat_decl(
        &mut self,
        pat: &Pat,
        src: Expr,
        kind: VarKind,
        decls: &mut Vec<VarDeclarator>,
    ) -> Result<(), ParseError> {
        match pat {
            Pat::Bind(name) => {
                decls.push(VarDeclarator {
                    id: name.clone(),
                    init: Some(src),
                });
                Ok(())
            }
            Pat::Target(_) => self.err("invalid binding target in declaration"),
            Pat::Default(inner, def) => {
                let t = self.fresh_dtmp();
                decls.push(VarDeclarator {
                    id: t.clone(),
                    init: Some(src),
                });
                let test =
                    self.d_binary(BinaryOp::StrictEq, self.d_ident(&t), self.d_undefined());
                let cond = Spanned::new(
                    self.d_sp(),
                    ExprKind::Conditional {
                        test: Box::new(test),
                        cons: Box::new(def.clone()),
                        alt: Box::new(self.d_ident(&t)),
                    },
                );
                self.emit_pat_decl(inner, cond, kind, decls)
            }
            Pat::Object(elems, rest) => {
                let t = self.fresh_dtmp();
                decls.push(VarDeclarator {
                    id: t.clone(),
                    init: Some(src),
                });
                // 空模式 `var {} = x`：显式 null 检查（非空靠属性访问自然抛 TypeError）。
                if elems.is_empty() && rest.is_none() {
                    let chk = self.fresh_dtmp();
                    let access = self.d_member(self.d_ident(&t), self.d_str("__yousj$dchk"), true);
                    decls.push(VarDeclarator {
                        id: chk,
                        init: Some(access),
                    });
                }
                let mut excluded: Vec<Expr> = Vec::new();
                for (key, p) in elems {
                    let (access, excl) = match key {
                        PatKey::Name(n) => (
                            self.d_member(self.d_ident(&t), self.d_str(n), true),
                            self.d_str(n),
                        ),
                        PatKey::Computed(e) => {
                            let kt = self.fresh_dtmp();
                            decls.push(VarDeclarator {
                                id: kt.clone(),
                                init: Some(e.clone()),
                            });
                            let acc =
                                self.d_member(self.d_ident(&t), self.d_ident(&kt), true);
                            let excl = self.d_binary(
                                BinaryOp::Add,
                                self.d_str(""),
                                self.d_ident(&kt),
                            );
                            (acc, excl)
                        }
                    };
                    excluded.push(excl);
                    self.emit_pat_decl(p, access, kind, decls)?;
                }
                if let Some(r) = rest {
                    let rest_init = self.build_obj_rest(&t, excluded);
                    // rest 是完整模式（`...[x]` / `...{a}` / `...a`）。
                    self.emit_pat_decl(r, rest_init, kind, decls)?;
                }
                Ok(())
            }
            Pat::Array(elems, rest) => {
                let t = self.fresh_dtmp();
                decls.push(VarDeclarator {
                    id: t.clone(),
                    init: Some(src),
                });
                // 无 rest：有界拉取（不多消费迭代器）；有 rest：全量 drain（rest 本来就要全部）。
                let arr = self.fresh_dtmp();
                let arr_init: Expr = match rest {
                    None => self.build_bounded_take(&t, elems.len()),
                    Some(_) => Spanned::new(
                        self.d_sp(),
                        ExprKind::Array(vec![ArrayElem::Spread(self.d_ident(&t))]),
                    ),
                };
                decls.push(VarDeclarator {
                    id: arr.clone(),
                    init: Some(arr_init),
                });
                let mut idx = 0usize;
                for el in elems {
                    if let Some(p) = el {
                        let access =
                            self.d_member(self.d_ident(&arr), self.d_num(idx as f64), true);
                        self.emit_pat_decl(p, access, kind, decls)?;
                    }
                    idx += 1;
                }
                if let Some(r) = rest {
                    let slice_fn =
                        self.d_member(self.d_ident(&arr), self.d_ident("slice"), false);
                    let rest_init = self.d_call(slice_fn, vec![self.d_num(idx as f64)]);
                    self.emit_pat_decl(r, rest_init, kind, decls)?;
                }
                Ok(())
            }
        }
    }

    /// 赋值形式：`pat` 对 `src` 的赋值 → 追加语句（`var` 临时量 + 赋值表达式）。
    fn emit_pat_assign(
        &mut self,
        pat: &Pat,
        src: Expr,
        stmts: &mut Vec<Stmt>,
    ) -> Result<(), ParseError> {
        // 临时量声明小件。
        let tmp_decl = |p: &mut Self, id: String, init: Expr, stmts: &mut Vec<Stmt>| {
            stmts.push(p.d_var_decl(
                VarKind::Var,
                vec![VarDeclarator { id, init: Some(init) }],
            ));
        };
        match pat {
            Pat::Bind(name) => {
                stmts.push(self.d_expr_stmt(self.d_assign_expr(self.d_ident(name), src)));
                Ok(())
            }
            Pat::Target(e) => {
                stmts.push(self.d_expr_stmt(self.d_assign_expr(e.clone(), src)));
                Ok(())
            }
            Pat::Default(inner, def) => {
                let t = self.fresh_dtmp();
                tmp_decl(self, t.clone(), src, stmts);
                let test =
                    self.d_binary(BinaryOp::StrictEq, self.d_ident(&t), self.d_undefined());
                let cond = Spanned::new(
                    self.d_sp(),
                    ExprKind::Conditional {
                        test: Box::new(test),
                        cons: Box::new(def.clone()),
                        alt: Box::new(self.d_ident(&t)),
                    },
                );
                self.emit_pat_assign(inner, cond, stmts)
            }
            Pat::Object(elems, rest) => {
                let t = self.fresh_dtmp();
                tmp_decl(self, t.clone(), src, stmts);
                if elems.is_empty() && rest.is_none() {
                    let chk = self.fresh_dtmp();
                    let access = self.d_member(self.d_ident(&t), self.d_str("__yousj$dchk"), true);
                    tmp_decl(self, chk, access, stmts);
                }
                let mut excluded: Vec<Expr> = Vec::new();
                for (key, p) in elems {
                    let (access, excl) = match key {
                        PatKey::Name(n) => (
                            self.d_member(self.d_ident(&t), self.d_str(n), true),
                            self.d_str(n),
                        ),
                        PatKey::Computed(e) => {
                            let kt = self.fresh_dtmp();
                            tmp_decl(self, kt.clone(), e.clone(), stmts);
                            let acc =
                                self.d_member(self.d_ident(&t), self.d_ident(&kt), true);
                            let excl = self.d_binary(
                                BinaryOp::Add,
                                self.d_str(""),
                                self.d_ident(&kt),
                            );
                            (acc, excl)
                        }
                    };
                    excluded.push(excl);
                    self.emit_pat_assign(p, access, stmts)?;
                }
                if let Some(r) = rest {
                    let rest_init = self.build_obj_rest(&t, excluded);
                    self.emit_pat_assign(r, rest_init, stmts)?;
                }
                Ok(())
            }
            Pat::Array(elems, rest) => {
                let t = self.fresh_dtmp();
                tmp_decl(self, t.clone(), src, stmts);
                let arr = self.fresh_dtmp();
                let arr_init: Expr = match rest {
                    None => self.build_bounded_take(&t, elems.len()),
                    Some(_) => Spanned::new(
                        self.d_sp(),
                        ExprKind::Array(vec![ArrayElem::Spread(self.d_ident(&t))]),
                    ),
                };
                tmp_decl(self, arr.clone(), arr_init, stmts);
                let mut idx = 0usize;
                for el in elems {
                    if let Some(p) = el {
                        let access =
                            self.d_member(self.d_ident(&arr), self.d_num(idx as f64), true);
                        self.emit_pat_assign(p, access, stmts)?;
                    }
                    idx += 1;
                }
                if let Some(r) = rest {
                    let slice_fn =
                        self.d_member(self.d_ident(&arr), self.d_ident("slice"), false);
                    let rest_init = self.d_call(slice_fn, vec![self.d_num(idx as f64)]);
                    self.emit_pat_assign(r, rest_init, stmts)?;
                }
                Ok(())
            }
        }
    }

    /// 对象 rest 的内联实现：
    /// `(($o) => { var $r = {}; var $ks = Object.keys($o); for (...) { var $k = $ks[$i]; if (cond) $r[$k] = $o[$k]; } return $r; })($t)`。
    /// 自身可枚举键（getter 会被求值），排除 `excluded`。
    fn build_obj_rest(&mut self, obj_tmp: &str, excluded: Vec<Expr>) -> Expr {
        let sp = self.d_sp();
        let o = self.fresh_dtmp();
        let r = self.fresh_dtmp();
        let ks = self.fresh_dtmp();
        let i = self.fresh_dtmp();
        let k = self.fresh_dtmp();
        let mut body: Vec<Stmt> = Vec::new();
        body.push(self.d_var_decl(
            VarKind::Var,
            vec![VarDeclarator {
                id: r.clone(),
                init: Some(Spanned::new(sp, ExprKind::Object(vec![]))),
            }],
        ));
        let keys_call = self.d_call(
            self.d_member(self.d_ident("Object"), self.d_ident("keys"), false),
            vec![self.d_ident(&o)],
        );
        body.push(self.d_var_decl(
            VarKind::Var,
            vec![VarDeclarator {
                id: ks.clone(),
                init: Some(keys_call),
            }],
        ));
        // for (var $i = 0; $i < $ks.length; $i++) { var $k = $ks[$i]; if (cond) $r[$k] = $o[$k]; }
        let mut loop_body: Vec<Stmt> = Vec::new();
        loop_body.push(self.d_var_decl(
            VarKind::Var,
            vec![VarDeclarator {
                id: k.clone(),
                init: Some(self.d_member(self.d_ident(&ks), self.d_ident(&i), true)),
            }],
        ));
        let mut cond: Option<Expr> = None;
        for ex in excluded {
            let neq = self.d_binary(BinaryOp::StrictNe, self.d_ident(&k), ex);
            cond = Some(match cond {
                None => neq,
                Some(c) => Spanned::new(
                    sp,
                    ExprKind::Logical {
                        op: LogicalOp::And,
                        left: Box::new(c),
                        right: Box::new(neq),
                    },
                ),
            });
        }
        let copy = self.d_assign_expr(
            self.d_member(self.d_ident(&r), self.d_ident(&k), true),
            self.d_member(self.d_ident(&o), self.d_ident(&k), true),
        );
        let copy_stmt: Stmt = match cond {
            Some(c) => Spanned::new(
                sp,
                StmtKind::If {
                    test: c,
                    cons: Box::new(self.d_expr_stmt(copy)),
                    alt: None,
                },
            ),
            None => self.d_expr_stmt(copy),
        };
        loop_body.push(copy_stmt);
        body.push(Spanned::new(
            sp,
            StmtKind::For {
                init: Some(ForInit::VarDecl {
                    kind: VarKind::Var,
                    decls: vec![VarDeclarator {
                        id: i.clone(),
                        init: Some(self.d_num(0.0)),
                    }],
                }),
                test: Some(self.d_binary(
                    BinaryOp::Lt,
                    self.d_ident(&i),
                    self.d_member(self.d_ident(&ks), self.d_ident("length"), false),
                )),
                update: Some(Spanned::new(
                    sp,
                    ExprKind::Update {
                        op: UpdateOp::Inc,
                        arg: Box::new(self.d_ident(&i)),
                        prefix: false,
                    },
                )),
                body: Box::new(Spanned::new(sp, StmtKind::Block(loop_body))),
            },
        ));
        body.push(Spanned::new(
            sp,
            StmtKind::Return(Some(self.d_ident(&r))),
        ));
        let arrow = Spanned::new(
            sp,
            ExprKind::ArrowFunction(Box::new(ArrowFunction {
                params: vec![Param {
                    name: o.clone(),
                    default: None,
                }],
                body: ArrowBody::Block(body),
                is_async: false,
                strict: self.strict,
            })),
        );
        self.d_call(arrow, vec![self.d_ident(obj_tmp)])
    }

    /// 有界迭代拉取：`(($s, $n) => { ... })($t, k)`，返回前 k 个值的数组。
    /// 数组/字符串用 slice；生成器用 `.next()` 逐个拉（不多消费）。
    /// null/不可迭代 → TypeError。
    /// 有界迭代拉取：`take($t, k)` 返回前 k 个值的数组。
    /// 数组/字符串用 slice；生成器用 `.next()` 逐个拉（不多消费）。
    /// null/不可迭代 → TypeError。
    fn build_bounded_take(&mut self, src_tmp: &str, n: usize) -> Expr {
        let sp = self.d_sp();
        // take = ($s, $n) => {
        //   if ($s instanceof Array) return $s.slice(0, $n);
        //   if (typeof $s === "string") return [...$s.slice(0, $n)];
        //   var $o = [];
        //   for (var $i = 0; $i < $n; $i++) {
        //     var $r = $s.next();
        //     if ($r.done) break;
        //     $o.push($r.value);
        //   }
        //   return $o;
        // };
        // take($t, n)
        let s_name = self.fresh_dtmp();
        let nn_name = self.fresh_dtmp();
        let o_name = self.fresh_dtmp();
        let i_name = self.fresh_dtmp();
        let r_name = self.fresh_dtmp();
        let slice_call = |me: &mut Self| {
            me.d_call(
                me.d_member(me.d_ident(&s_name), me.d_ident("slice"), false),
                vec![me.d_num(0.0), me.d_ident(&nn_name)],
            )
        };
        let mut body: Vec<Stmt> = Vec::new();
        body.push(Spanned::new(
            sp,
            StmtKind::If {
                test: self.d_binary(
                    BinaryOp::Instanceof,
                    self.d_ident(&s_name),
                    self.d_ident("Array"),
                ),
                cons: Box::new(Spanned::new(
                    sp,
                    StmtKind::Return(Some(slice_call(self))),
                )),
                alt: None,
            },
        ));
        let type_of_s = Spanned::new(
            sp,
            ExprKind::Unary {
                op: UnaryOp::Typeof,
                arg: Box::new(self.d_ident(&s_name)),
            },
        );
        body.push(Spanned::new(
            sp,
            StmtKind::If {
                test: self.d_binary(BinaryOp::StrictEq, type_of_s, self.d_str("string")),
                cons: Box::new(Spanned::new(
                    sp,
                    StmtKind::Return(Some(Spanned::new(
                        sp,
                        ExprKind::Array(vec![ArrayElem::Spread(slice_call(self))]),
                    ))),
                )),
                alt: None,
            },
        ));
        body.push(self.d_var_decl(
            VarKind::Var,
            vec![VarDeclarator {
                id: o_name.clone(),
                init: Some(Spanned::new(sp, ExprKind::Array(vec![]))),
            }],
        ));
        let mut loop_body: Vec<Stmt> = Vec::new();
        loop_body.push(self.d_var_decl(
            VarKind::Var,
            vec![VarDeclarator {
                id: r_name.clone(),
                init: Some(self.d_call(
                    self.d_member(self.d_ident(&s_name), self.d_ident("next"), false),
                    vec![],
                )),
            }],
        ));
        loop_body.push(Spanned::new(
            sp,
            StmtKind::If {
                test: self.d_member(self.d_ident(&r_name), self.d_ident("done"), false),
                cons: Box::new(Spanned::new(sp, StmtKind::Break)),
                alt: None,
            },
        ));
        loop_body.push(self.d_expr_stmt(self.d_call(
            self.d_member(self.d_ident(&o_name), self.d_ident("push"), false),
            vec![self.d_member(self.d_ident(&r_name), self.d_ident("value"), false)],
        )));
        body.push(Spanned::new(
            sp,
            StmtKind::For {
                init: Some(ForInit::VarDecl {
                    kind: VarKind::Var,
                    decls: vec![VarDeclarator {
                        id: i_name.clone(),
                        init: Some(self.d_num(0.0)),
                    }],
                }),
                test: Some(self.d_binary(
                    BinaryOp::Lt,
                    self.d_ident(&i_name),
                    self.d_ident(&nn_name),
                )),
                update: Some(Spanned::new(
                    sp,
                    ExprKind::Update {
                        op: UpdateOp::Inc,
                        arg: Box::new(self.d_ident(&i_name)),
                        prefix: false,
                    },
                )),
                body: Box::new(Spanned::new(sp, StmtKind::Block(loop_body))),
            },
        ));
        body.push(Spanned::new(
            sp,
            StmtKind::Return(Some(self.d_ident(&o_name))),
        ));
        let arrow = Spanned::new(
            sp,
            ExprKind::ArrowFunction(Box::new(ArrowFunction {
                params: vec![
                    Param {
                        name: s_name.clone(),
                        default: None,
                    },
                    Param {
                        name: nn_name.clone(),
                        default: None,
                    },
                ],
                body: ArrowBody::Block(body),
                is_async: false,
                strict: self.strict,
            })),
        );
        self.d_call(arrow, vec![self.d_ident(src_tmp), self.d_num(n as f64)])
    }


    /// 赋值形式脱糖为 IIFE：
    /// `(($p) => { <stmts>; return $p; })(rhs)`。
    fn build_pat_assign(&mut self, pat: &Pat, rhs: Expr) -> Result<Expr, ParseError> {
        let param = self.fresh_dtmp();
        let mut stmts: Vec<Stmt> = Vec::new();
        self.emit_pat_assign(pat, self.d_ident(&param), &mut stmts)?;
        stmts.push(Spanned::new(
            self.d_sp(),
            StmtKind::Return(Some(self.d_ident(&param))),
        ));
        let arrow = Spanned::new(
            self.d_sp(),
            ExprKind::ArrowFunction(Box::new(ArrowFunction {
                params: vec![Param {
                    name: param,
                    default: None,
                }],
                body: ArrowBody::Block(stmts),
                is_async: false,
                strict: self.strict,
            })),
        );
        Ok(self.d_call(arrow, vec![rhs]))
    }

    /// 回溯试探解构赋值：`[pat] = rhs` / `{pat} = rhs`。
    /// Ok(None) = 不是解构赋值（调用方恢复 token 位置）。
    fn try_parse_destructure_assign(&mut self) -> Result<Option<Expr>, ParseError> {
        if !self.at_punct(Punct::LBracket) && !self.at_punct(Punct::LBrace) {
            return Ok(None);
        }
        let m = self.mark();
        let (sl, sc) = self.cur_pos();
        let pat = match self.parse_pat(true) {
            Ok(p) => p,
            Err(_) => {
                self.restore(m);
                return Ok(None);
            }
        };
        if !self.at_punct(Punct::Assign) {
            self.restore(m);
            return Ok(None);
        }
        self.bump(); // =
        let rhs = self.parse_assignment()?;
        let span = Span::new(sl, sc, self.prev_end_line, self.prev_end_col);
        let e = self.build_pat_assign(&pat, rhs)?;
        Ok(Some(Spanned::new(span, e.node)))
    }

    /// 收集模式绑定的标识符名（严格模式重复检查用）。
    fn pat_bound_names(&self, pat: &Pat, out: &mut Vec<String>) {
        match pat {
            Pat::Bind(n) => out.push(n.clone()),
            Pat::Target(_) => {}
            Pat::Object(elems, rest) => {
                for (_, p) in elems {
                    self.pat_bound_names(p, out);
                }
                if let Some(r) = rest {
                    self.pat_bound_names(r, out);
                }
            }
            Pat::Array(elems, rest) => {
                for el in elems {
                    if let Some(p) = el {
                        self.pat_bound_names(p, out);
                    }
                }
                if let Some(r) = rest {
                    self.pat_bound_names(r, out);
                }
            }
            Pat::Default(inner, _) => self.pat_bound_names(inner, out),
        }
    }

    /// 严格模式：解构形参的绑定名不得重复。
    fn check_dup_pat_params(&self, pat_params: &[PatParam]) -> Result<(), ParseError> {
        let mut names: Vec<String> = Vec::new();
        for pp in pat_params {
            self.pat_bound_names(&pp.pat, &mut names);
        }
        for i in 0..names.len() {
            for j in (i + 1)..names.len() {
                if names[i] == names[j] {
                    return self.err(format!(
                        "duplicate parameter name '{}' in strict mode",
                        names[i]
                    ));
                }
            }
        }
        Ok(())
    }

    /// 形参解构脱糖前置到函数体（`var` 语义）。
    fn prepend_pat_params(
        &mut self,
        pat_params: &[PatParam],
        body: &mut Vec<Stmt>,
    ) -> Result<(), ParseError> {
        if pat_params.is_empty() {
            return Ok(());
        }
        let mut decls = Vec::new();
        for pp in pat_params {
            let src: Expr = match &pp.default {
                Some(d) => {
                    let test =
                        self.d_binary(BinaryOp::StrictEq, self.d_ident(&pp.temp), self.d_undefined());
                    Spanned::new(
                        self.d_sp(),
                        ExprKind::Conditional {
                            test: Box::new(test),
                            cons: Box::new(d.clone()),
                            alt: Box::new(self.d_ident(&pp.temp)),
                        },
                    )
                }
                None => self.d_ident(&pp.temp),
            };
            self.emit_pat_decl(&pp.pat, src, VarKind::Var, &mut decls)?;
        }
        let stmt = self.d_var_decl(VarKind::Var, decls);
        // 插到指令序言（`"use strict"` 等）之后。
        let pos = body
            .iter()
            .position(|s| {
                !matches!(&s.node, StmtKind::Expr(e) if matches!(&e.node, ExprKind::Literal(Literal::String(_))))
            })
            .unwrap_or(body.len());
        body.insert(pos, stmt);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(src: &str) -> Program {
        parse_source(src).unwrap_or_else(|e| panic!("parse failed for {:?}: {}", src, e))
    }

    fn one_stmt(src: &str) -> Stmt {
        let p = prog(src);
        assert_eq!(p.body.len(), 1, "expected 1 stmt in {:?}", src);
        p.body.into_iter().next().unwrap()
    }

    fn one_expr(src: &str) -> Expr {
        match one_stmt(src).node {
            StmtKind::Expr(e) => e,
            other => panic!("expected expr stmt, got {:?}", other),
        }
    }

    fn num(e: &Expr) -> f64 {
        match &e.node {
            ExprKind::Literal(Literal::Number(v)) => *v,
            other => panic!("expected number literal, got {:?}", other),
        }
    }

    fn ident_name(e: &Expr) -> &str {
        match &e.node {
            ExprKind::Ident(n) => n,
            other => panic!("expected identifier, got {:?}", other),
        }
    }

    #[test]
    fn var_let_const_decl() {
        match one_stmt("var x = 1, y = 'hi', z;").node {
            StmtKind::VarDecl { kind, decls } => {
                assert_eq!(kind, VarKind::Var);
                assert_eq!(decls.len(), 3);
                assert_eq!(decls[0].id, "x");
                assert_eq!(num(decls[0].init.as_ref().unwrap()), 1.0);
                assert_eq!(decls[1].id, "y");
                assert!(decls[2].init.is_none());
            }
            other => panic!("got {:?}", other),
        }
        match one_stmt("const PI = 3.14;").node {
            StmtKind::VarDecl { kind, .. } => assert_eq!(kind, VarKind::Const),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn precedence_mul_over_add() {
        // 1 + 2 * 3 必须解析为 1 + (2 * 3)
        let e = one_expr("1 + 2 * 3;");
        match &e.node {
            ExprKind::Binary {
                op: BinaryOp::Add,
                left,
                right,
            } => {
                assert_eq!(num(left), 1.0);
                match &right.node {
                    ExprKind::Binary {
                        op: BinaryOp::Mul,
                        left: l2,
                        right: r2,
                    } => {
                        assert_eq!(num(l2), 2.0);
                        assert_eq!(num(r2), 3.0);
                    }
                    other => panic!("expected 2*3, got {:?}", other),
                }
            }
            other => panic!("expected 1+(2*3), got {:?}", other),
        }
    }

    #[test]
    fn precedence_and_over_eq() {
        // a === b && c !== d 必须解析为 (a===b) && (c!==d)
        let e = one_expr("a === b && c !== d;");
        match &e.node {
            ExprKind::Logical {
                op: LogicalOp::And,
                left,
                right,
            } => {
                assert!(matches!(
                    &left.node,
                    ExprKind::Binary {
                        op: BinaryOp::StrictEq,
                        ..
                    }
                ));
                assert!(matches!(
                    &right.node,
                    ExprKind::Binary {
                        op: BinaryOp::StrictNe,
                        ..
                    }
                ));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn precedence_pow_right_assoc() {
        // 2 ** 3 ** 2 必须解析为 2 ** (3 ** 2)
        let e = one_expr("2 ** 3 ** 2;");
        match &e.node {
            ExprKind::Binary {
                op: BinaryOp::Pow,
                left,
                right,
            } => {
                assert_eq!(num(left), 2.0);
                assert!(matches!(
                    &right.node,
                    ExprKind::Binary {
                        op: BinaryOp::Pow,
                        ..
                    }
                ));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn ternary() {
        let e = one_expr("x ? y : z;");
        match &e.node {
            ExprKind::Conditional { test, cons, alt } => {
                assert_eq!(ident_name(test), "x");
                assert_eq!(ident_name(cons), "y");
                assert_eq!(ident_name(alt), "z");
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn call_member_chain() {
        // a.b(c).d
        let e = one_expr("a.b(c).d;");
        match &e.node {
            ExprKind::Member {
                obj,
                prop,
                computed: false,
                optional: false,
            } => {
                assert_eq!(ident_name(prop), "d");
                match &obj.node {
                    ExprKind::Call {
                        callee,
                        args,
                        optional: false,
                    } => {
                        assert_eq!(args.len(), 1);
                        assert_eq!(ident_name(&args[0]), "c");
                        match &callee.node {
                            ExprKind::Member {
                                obj: o2,
                                prop: p2,
                                computed: false,
                                optional: false,
                            } => {
                                assert_eq!(ident_name(o2), "a");
                                assert_eq!(ident_name(p2), "b");
                            }
                            other => panic!("expected a.b, got {:?}", other),
                        }
                    }
                    other => panic!("expected call, got {:?}", other),
                }
            }
            other => panic!("expected member, got {:?}", other),
        }
    }

    #[test]
    fn arrows() {
        // (a, b) => a + b
        let e = one_expr("(a, b) => a + b;");
        match &e.node {
            ExprKind::ArrowFunction(f) => {
                assert_eq!(f.params.len(), 2);
                assert_eq!(f.params[0].name, "a");
                assert!(matches!(f.body, ArrowBody::Expr(_)));
            }
            other => panic!("got {:?}", other),
        }
        // x => x * 2
        let e2 = one_expr("x => x * 2;");
        match &e2.node {
            ExprKind::ArrowFunction(f) => {
                assert_eq!(f.params.len(), 1);
                assert_eq!(f.params[0].name, "x");
            }
            other => panic!("got {:?}", other),
        }
        // () => { return 1; }
        let e3 = one_expr("() => { return 1; };");
        match &e3.node {
            ExprKind::ArrowFunction(f) => {
                assert!(f.params.is_empty());
                assert!(matches!(f.body, ArrowBody::Block(_)));
            }
            other => panic!("got {:?}", other),
        }
        // 分组表达式不能被误判为箭头：(a + b) * c
        let e4 = one_expr("(a + b) * c;");
        assert!(matches!(
            &e4.node,
            ExprKind::Binary {
                op: BinaryOp::Mul,
                ..
            }
        ));
    }

    #[test]
    fn function_decl() {
        match one_stmt("function add(a, b = 2) { return a + b; }").node {
            StmtKind::FunctionDecl(f) => {
                assert_eq!(f.id.as_deref(), Some("add"));
                assert_eq!(f.params.len(), 2);
                assert_eq!(f.params[1].name, "b");
                assert!(f.params[1].default.is_some());
                assert_eq!(f.body.len(), 1);
                assert!(matches!(f.body[0].node, StmtKind::Return(_)));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn if_else() {
        match one_stmt("if (x) { y(); } else if (z) { w(); } else { v(); }").node {
            StmtKind::If { test, cons, alt } => {
                assert_eq!(ident_name(&test), "x");
                assert!(matches!(cons.node, StmtKind::Block(_)));
                // else if 嵌套
                match alt.unwrap().node {
                    StmtKind::If { .. } => {}
                    other => panic!("expected nested if, got {:?}", other),
                }
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn classic_for() {
        match one_stmt("for (var i = 0; i < 10; i++) { sum(i); }").node {
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                assert!(matches!(init, Some(ForInit::VarDecl { .. })));
                assert!(test.is_some());
                assert!(matches!(
                    update.as_ref().unwrap().node,
                    ExprKind::Update { prefix: false, .. }
                ));
                assert!(matches!(body.node, StmtKind::Block(_)));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn for_in_of() {
        match one_stmt("for (k in obj) { print(k); }").node {
            StmtKind::ForInOf {
                is_of: false,
                left,
                right,
                ..
            } => {
                assert!(matches!(left, ForLeft::Expr(_)));
                assert_eq!(ident_name(&right), "obj");
            }
            other => panic!("got {:?}", other),
        }
        match one_stmt("for (const v of arr) v++;").node {
            StmtKind::ForInOf {
                is_of: true,
                left: ForLeft::VarDecl { kind, name },
                ..
            } => {
                assert_eq!(kind, VarKind::Const);
                assert_eq!(name, "v");
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn while_and_do_while() {
        match one_stmt("while (x > 0) x--;").node {
            StmtKind::While { test, body } => {
                assert!(matches!(&test.node, ExprKind::Binary { .. }));
                assert!(matches!(&body.node, StmtKind::Expr(_)));
            }
            other => panic!("got {:?}", other),
        }
        match one_stmt("do { x++; } while (x < 5);").node {
            StmtKind::DoWhile { body, test } => {
                assert!(matches!(&body.node, StmtKind::Block(_)));
                assert!(matches!(&test.node, ExprKind::Binary { .. }));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn object_and_array() {
        match one_stmt("var o = {a: 1, b, 'c-d': 3, m() { return 1; }, get x() { return 2; }};").node
        {
            StmtKind::VarDecl { decls, .. } => {
                match &decls[0].init.as_ref().unwrap().node {
                    ExprKind::Object(props) => {
                        assert_eq!(props.len(), 5);
                        assert!(matches!(props[0].value, PropValue::Init(_)));
                        assert!(matches!(props[1].value, PropValue::Shorthand(_)));
                        assert!(matches!(props[3].value, PropValue::Method(_)));
                        assert!(matches!(props[4].value, PropValue::Getter(_)));
                    }
                    other => panic!("got {:?}", other),
                }
            }
            other => panic!("got {:?}", other),
        }
        let e2 = one_expr("[1, , 3];");
        match &e2.node {
            ExprKind::Array(elems) => {
                assert_eq!(elems.len(), 3);
                assert!(matches!(elems[1], ArrayElem::Hole));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn return_break_continue_throw() {
        let p = prog("function f() { return 1; while (x) { break; continue; } throw new Error('e'); }");
        match &p.body[0].node {
            StmtKind::FunctionDecl(f) => {
                assert!(matches!(f.body[0].node, StmtKind::Return(Some(_))));
                match &f.body[1].node {
                    StmtKind::While { body, .. } => match &body.node {
                        StmtKind::Block(stmts) => {
                            assert!(matches!(stmts[0].node, StmtKind::Break));
                            assert!(matches!(stmts[1].node, StmtKind::Continue));
                        }
                        other => panic!("got {:?}", other),
                    },
                    other => panic!("got {:?}", other),
                }
                assert!(matches!(f.body[2].node, StmtKind::Throw(_)));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn try_catch_finally_and_switch() {
        match one_stmt("try { x(); } catch (e) { y(e); } finally { z(); }").node {
            StmtKind::Try {
                handler,
                finalizer,
                ..
            } => {
                let h = handler.unwrap();
                assert_eq!(h.param.as_deref(), Some("e"));
                assert!(finalizer.is_some());
            }
            other => panic!("got {:?}", other),
        }
        match one_stmt("switch (x) { case 1: a(); break; default: b(); }").node {
            StmtKind::Switch { disc, cases } => {
                assert_eq!(ident_name(&disc), "x");
                assert_eq!(cases.len(), 2);
                assert!(cases[0].test.is_some());
                assert!(cases[1].test.is_none()); // default
                assert_eq!(cases[0].body.len(), 2); // a(); break;
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn new_unary_update() {
        match one_stmt("var d = new Date(2024, 0, 1);").node {
            StmtKind::VarDecl { decls, .. } => match &decls[0].init.as_ref().unwrap().node {
                ExprKind::New { callee, args } => {
                    assert_eq!(ident_name(callee), "Date");
                    assert_eq!(args.len(), 3);
                }
                other => panic!("got {:?}", other),
            },
            other => panic!("got {:?}", other),
        }
        assert!(matches!(
            one_expr("typeof x;").node,
            ExprKind::Unary {
                op: UnaryOp::Typeof,
                ..
            }
        ));
        assert!(matches!(
            one_expr("x++;").node,
            ExprKind::Update {
                op: UpdateOp::Inc,
                prefix: false,
                ..
            }
        ));
        assert!(matches!(
            one_expr("--y;").node,
            ExprKind::Update {
                op: UpdateOp::Dec,
                prefix: true,
                ..
            }
        ));
        assert!(matches!(
            one_expr("delete a.b;").node,
            ExprKind::Unary {
                op: UnaryOp::Delete,
                ..
            }
        ));
    }

    #[test]
    fn asi_and_restricted_productions() {
        // 换行自动补分号
        let p = prog("var x = 1\nvar y = 2\n");
        assert_eq!(p.body.len(), 2);
        // return 后换行 → return;（x 留给下一条语句）
        let p2 = prog("function f() {\n  return\n  x\n}");
        match &p2.body[0].node {
            StmtKind::FunctionDecl(f) => {
                assert!(matches!(f.body[0].node, StmtKind::Return(None)));
                assert_eq!(f.body.len(), 2);
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn template_and_sequence() {
        // Phase 14：`${}` 插值脱糖为字符串拼接（`"" + "hello " + name`）。
        match one_stmt("var s = `hello ${name}`;").node {
            StmtKind::VarDecl { decls, .. } => {
                assert!(matches!(
                    &decls[0].init.as_ref().unwrap().node,
                    ExprKind::Binary {
                        op: BinaryOp::Add,
                        ..
                    }
                ));
            }
            other => panic!("got {:?}", other),
        }
        // 无插值时仍为模板字面量（烹制转义）。
        match one_expr("`a\\nb`").node {
            ExprKind::Literal(Literal::Template(s)) => {
                assert_eq!(s, "a\nb");
            }
            other => panic!("got {:?}", other),
        }
        let e2 = one_expr("a = 1, b = 2;");
        assert!(matches!(&e2.node, ExprKind::Sequence(v) if v.len() == 2));
    }

    #[test]
    fn errors_have_line_col() {
        // 未闭合括号：第 1 行报错
        let e = parse_source("if (x { }").unwrap_err();
        assert_eq!(e.line, 1);
        assert!(e.message.contains("expected"));
        // 非法赋值目标
        let e2 = parse_source("1 = 2;").unwrap_err();
        assert!(e2.message.contains("invalid assignment target"));
        // 非法 token
        assert!(parse_source("var 123 = 1;").is_err());
        // 未闭合块：第 2 行
        let e3 = parse_source("function f() {\n  var x = 1;\n").unwrap_err();
        assert_eq!(e3.line, 3);
    }

    #[test]
    fn compound_assign_and_logical() {
        let e = one_expr("x += 1;");
        assert!(matches!(
            &e.node,
            ExprKind::Assign {
                op: AssignOp::AddAssign,
                ..
            }
        ));
        let e2 = one_expr("a ?? b || c && d;");
        // ?? 优先级最低：a ?? (b || (c && d))
        // 注：规范里 ?? 与 && / || 混用不加括号是语法错误；子集里按优先级接受。
        match &e2.node {
            ExprKind::Logical {
                op: LogicalOp::Nullish,
                left,
                right,
            } => {
                assert_eq!(ident_name(left), "a");
                assert!(matches!(
                    &right.node,
                    ExprKind::Logical {
                        op: LogicalOp::Or,
                        ..
                    }
                ));
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn optional_chaining() {
        let e = one_expr("a?.b?.(c);");
        match &e.node {
            ExprKind::Call {
                optional: true, ..
            } => {}
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn realistic_snippet() {
        // 模拟页面脚本常见写法：一把梭，能解析不报错就算过。
        let src = r#"
const app = document.getElementById('app');
let count = 0;
function render(items) {
  let html = '';
  for (const item of items) {
    html += '<li>' + item.name + '</li>';
  }
  app.innerHTML = html;
}
fetch('/api/data').then(res => res.json()).then(data => {
  render(data.items ?? []);
}).catch(e => console.error('oops', e));
try { init(); } catch (err) { console.log(err.message); }
switch (count) { case 0: break; default: count = 0; }
"#;
        let p = prog(src);
        assert_eq!(p.body.len(), 6);
        // 第一句：const app = document.getElementById('app')
        match &p.body[0].node {
            StmtKind::VarDecl { kind, decls } => {
                assert_eq!(*kind, VarKind::Const);
                match &decls[0].init.as_ref().unwrap().node {
                    ExprKind::Call { callee, args, .. } => {
                        assert_eq!(args.len(), 1);
                        assert!(matches!(&callee.node, ExprKind::Member { .. }));
                    }
                    other => panic!("got {:?}", other),
                }
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn spans_look_sane() {
        let e = one_expr("1 + 2;");
        assert_eq!(e.span.start_line, 1);
        assert_eq!(e.span.start_col, 1);
        assert_eq!(e.span.end_line, 1);
        assert_eq!(e.span.end_col, 6); // "1 + 2" 占 5 列，结束指向第 6 列
        let p = prog("var x = 1;\nvar y = 2;");
        assert_eq!(p.body[1].span.start_line, 2);
    }
}
