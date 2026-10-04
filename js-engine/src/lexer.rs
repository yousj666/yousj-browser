//! yousj-js · Phase 1：手写 JavaScript 词法分析器。
//!
//! - 零第三方依赖（不用 logos / nom，保持和 HTML 引擎一样的"自己写"传统）。
//! - ASCII 子集：标识符暂不支持完整 Unicode（见 TODO），行终止符处理 `\n` / `\r\n` / `\r`。
//! - 每个 token 记录行号 / 列号（1-based），供报错和后续 parser 使用。
//! - Phase 1 刻意不做的事（见各处 TODO）：正则字面量（需要 parser 上下文）、
//!   模板字符串拆分（先整体切分为单个 token）、完整 Unicode 标识符、
//!   严格模式 legacy 八进制转义。

use std::fmt;

// ---------------------------------------------------------------------------
// 关键字
// ---------------------------------------------------------------------------

/// JS 关键字（phase 1 覆盖 ES2024 常用集合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keyword {
    Var, Let, Const, Function, Return, If, Else, For, While, Do,
    Break, Continue, New, Delete, Typeof, Void, Instanceof, In, Of,
    True, False, Null, Undefined, This, Super, Class, Extends,
    Import, Export, Default, Try, Catch, Finally, Throw, Switch, Case,
    Async, Await, Yield, Static, Get, Set,
}

impl Keyword {
    pub fn from_str(s: &str) -> Option<Keyword> {
        Some(match s {
            "var" => Keyword::Var,
            "let" => Keyword::Let,
            "const" => Keyword::Const,
            "function" => Keyword::Function,
            "return" => Keyword::Return,
            "if" => Keyword::If,
            "else" => Keyword::Else,
            "for" => Keyword::For,
            "while" => Keyword::While,
            "do" => Keyword::Do,
            "break" => Keyword::Break,
            "continue" => Keyword::Continue,
            "new" => Keyword::New,
            "delete" => Keyword::Delete,
            "typeof" => Keyword::Typeof,
            "void" => Keyword::Void,
            "instanceof" => Keyword::Instanceof,
            "in" => Keyword::In,
            "of" => Keyword::Of,
            "true" => Keyword::True,
            "false" => Keyword::False,
            "null" => Keyword::Null,
            "undefined" => Keyword::Undefined,
            "this" => Keyword::This,
            "super" => Keyword::Super,
            "class" => Keyword::Class,
            "extends" => Keyword::Extends,
            "import" => Keyword::Import,
            "export" => Keyword::Export,
            "default" => Keyword::Default,
            "try" => Keyword::Try,
            "catch" => Keyword::Catch,
            "finally" => Keyword::Finally,
            "throw" => Keyword::Throw,
            "switch" => Keyword::Switch,
            "case" => Keyword::Case,
            "async" => Keyword::Async,
            "await" => Keyword::Await,
            "yield" => Keyword::Yield,
            "static" => Keyword::Static,
            "get" => Keyword::Get,
            "set" => Keyword::Set,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Keyword::Var => "var",
            Keyword::Let => "let",
            Keyword::Const => "const",
            Keyword::Function => "function",
            Keyword::Return => "return",
            Keyword::If => "if",
            Keyword::Else => "else",
            Keyword::For => "for",
            Keyword::While => "while",
            Keyword::Do => "do",
            Keyword::Break => "break",
            Keyword::Continue => "continue",
            Keyword::New => "new",
            Keyword::Delete => "delete",
            Keyword::Typeof => "typeof",
            Keyword::Void => "void",
            Keyword::Instanceof => "instanceof",
            Keyword::In => "in",
            Keyword::Of => "of",
            Keyword::True => "true",
            Keyword::False => "false",
            Keyword::Null => "null",
            Keyword::Undefined => "undefined",
            Keyword::This => "this",
            Keyword::Super => "super",
            Keyword::Class => "class",
            Keyword::Extends => "extends",
            Keyword::Import => "import",
            Keyword::Export => "export",
            Keyword::Default => "default",
            Keyword::Try => "try",
            Keyword::Catch => "catch",
            Keyword::Finally => "finally",
            Keyword::Throw => "throw",
            Keyword::Switch => "switch",
            Keyword::Case => "case",
            Keyword::Async => "async",
            Keyword::Await => "await",
            Keyword::Yield => "yield",
            Keyword::Static => "static",
            Keyword::Get => "get",
            Keyword::Set => "set",
        }
    }
}

// ---------------------------------------------------------------------------
// 标点 / 运算符
// ---------------------------------------------------------------------------

/// JS 全套标点与运算符（按 longest-match / maximal munch 切分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Punct {
    // 赋值与比较
    Assign, Eq, StrictEq, Ne, StrictNe, Arrow,
    // 关系与位移
    Lt, Le, Shl, ShlAssign,
    Gt, Ge, Shr, ShrAssign, UShr, UShrAssign,
    // 算术
    Plus, PlusPlus, PlusAssign,
    Minus, MinusMinus, MinusAssign,
    Star, StarAssign, Pow, PowAssign,
    Slash, SlashAssign, Percent, PercentAssign,
    // 位运算与逻辑
    Amp, AmpAmp, AmpAssign, AmpAmpAssign,
    Pipe, PipePipe, PipeAssign, PipePipeAssign,
    Caret, CaretAssign, Bang, Tilde,
    // 问号族
    Question, QuestionQuestion, QuestionQuestionAssign, QuestionDot,
    // 分隔符
    Colon, Semi, Comma, Dot, Ellipsis,
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    /// `#`：私有字段/方法名 `#x`（phase 9）。注意 hashbang `#!` 在
    /// read_punct 之前已被跳过，这里只处理单个 `#`。
    Hash,
}

impl Punct {
    pub fn as_str(&self) -> &'static str {
        match self {
            Punct::Assign => "=",
            Punct::Eq => "==",
            Punct::StrictEq => "===",
            Punct::Ne => "!=",
            Punct::StrictNe => "!==",
            Punct::Arrow => "=>",
            Punct::Lt => "<",
            Punct::Le => "<=",
            Punct::Shl => "<<",
            Punct::ShlAssign => "<<=",
            Punct::Gt => ">",
            Punct::Ge => ">=",
            Punct::Shr => ">>",
            Punct::ShrAssign => ">>=",
            Punct::UShr => ">>>",
            Punct::UShrAssign => ">>>=",
            Punct::Plus => "+",
            Punct::PlusPlus => "++",
            Punct::PlusAssign => "+=",
            Punct::Minus => "-",
            Punct::MinusMinus => "--",
            Punct::MinusAssign => "-=",
            Punct::Star => "*",
            Punct::StarAssign => "*=",
            Punct::Pow => "**",
            Punct::PowAssign => "**=",
            Punct::Slash => "/",
            Punct::SlashAssign => "/=",
            Punct::Percent => "%",
            Punct::PercentAssign => "%=",
            Punct::Amp => "&",
            Punct::AmpAmp => "&&",
            Punct::AmpAssign => "&=",
            Punct::AmpAmpAssign => "&&=",
            Punct::Pipe => "|",
            Punct::PipePipe => "||",
            Punct::PipeAssign => "|=",
            Punct::PipePipeAssign => "||=",
            Punct::Caret => "^",
            Punct::CaretAssign => "^=",
            Punct::Bang => "!",
            Punct::Tilde => "~",
            Punct::Question => "?",
            Punct::QuestionQuestion => "??",
            Punct::QuestionQuestionAssign => "??=",
            Punct::QuestionDot => "?.",
            Punct::Colon => ":",
            Punct::Semi => ";",
            Punct::Comma => ",",
            Punct::Dot => ".",
            Punct::Ellipsis => "...",
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::LBrace => "{",
            Punct::RBrace => "}",
            Punct::LBracket => "[",
            Punct::RBracket => "]",
            Punct::Hash => "#",
        }
    }
}

// ---------------------------------------------------------------------------
// Token
// ---------------------------------------------------------------------------

/// 数字字面量。`value` 为解析后的 f64；`bigint` 标记 `123n` 形式。
#[derive(Debug, Clone, PartialEq)]
pub struct NumberLit {
    pub raw: String,
    pub value: f64,
    pub bigint: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Keyword(Keyword),
    Number(NumberLit),
    /// 解码后的字符串值（转义已处理）。
    Str(String),
    /// 模板字符串：phase 1 按整体切分，存反引号之间的原始文本。
    /// TODO(phase 2): 拆分为 TemplateHead / TemplateMiddle / TemplateTail。
    Template(String),
    Punct(Punct),
    /// 正则字面量（phase 7）：`TokenKind::Regex { pattern, flags }`。
    /// `/` 是除法还是正则开头，由前一个 token 启发式判定
    /// （`read_punct` 里处理，见 `regex_allowed_after`）。
    Regex { pattern: String, flags: String },
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// 1-based 行号。
    pub line: usize,
    /// 1-based 列号（按字符计）。
    pub col: usize,
    /// 源码原文。
    pub lexeme: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // phase 8：词法错误按规范也是 SyntaxError。
        write!(f, "SyntaxError at {}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for LexError {}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

fn is_ident_start(c: char) -> bool {
    // TODO: 完整 Unicode ID_Start（目前 ASCII 子集：字母 / _ / $）。
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    // TODO: 完整 Unicode ID_Continue。
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
    /// 上一个产出的 token 种类（正则/除法启发式用）。
    prev_kind: Option<TokenKind>,
}

impl Lexer {
    pub fn new(input: &str) -> Self {
        let mut lx = Lexer {
            chars: input.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
            prev_kind: None,
        };
        // Hashbang：`#!/usr/bin/env node` 开头的一行直接跳过。
        if lx.chars.get(0) == Some(&'#') && lx.chars.get(1) == Some(&'!') {
            while !matches!(lx.peek(), None | Some('\n') | Some('\r')) {
                lx.bump();
            }
        }
        lx
    }

    fn at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).copied()
    }

    fn peek(&self) -> Option<char> {
        self.at(0)
    }

    fn peek2(&self) -> Option<char> {
        self.at(1)
    }

    fn peek3(&self) -> Option<char> {
        self.at(2)
    }

    fn peek4(&self) -> Option<char> {
        self.at(3)
    }

    /// 前进一步；`\r\n` 算一个换行。
    fn bump(&mut self) -> Option<char> {
        let c = self.at(0)?;
        self.pos += 1;
        if c == '\r' {
            if self.peek() == Some('\n') {
                self.pos += 1;
            }
            self.line += 1;
            self.col = 1;
        } else if c == '\n' {
            // TODO: U+2028 / U+2029 也算行终止符（目前只处理 ASCII 换行）。
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, LexError> {
        Err(LexError {
            line: self.line,
            col: self.col,
            message: msg.into(),
        })
    }

    fn token(&self, kind: TokenKind, line: usize, col: usize, start: usize) -> Token {
        Token {
            kind,
            line,
            col,
            lexeme: self.chars[start..self.pos].iter().collect(),
        }
    }

    /// 跳过空白、行注释、块注释。块注释未闭合则报错。
    fn skip_ws_and_comments(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\x0B') | Some('\x0C') | Some('\n') | Some('\r') => {
                    self.bump();
                }
                // Phase 14：Unicode 空白（Zs 类 + U+2028/2029 + BOM）。
                Some(c)
                    if c == '\u{00A0}'
                        || c == '\u{2028}'
                        || c == '\u{2029}'
                        || c == '\u{FEFF}'
                        || matches!(
                            c,
                            '\u{1680}'
                                | '\u{2000}'
                                | '\u{2001}'
                                | '\u{2002}'
                                | '\u{2003}'
                                | '\u{2004}'
                                | '\u{2005}'
                                | '\u{2006}'
                                | '\u{2007}'
                                | '\u{2008}'
                                | '\u{2009}'
                                | '\u{200A}'
                                | '\u{202F}'
                                | '\u{205F}'
                                | '\u{3000}'
                        ) =>
                {
                    self.bump();
                }
                Some('/') if self.peek2() == Some('/') => {
                    while !matches!(self.peek(), None | Some('\n') | Some('\r')) {
                        self.bump();
                    }
                }
                Some('/') if self.peek2() == Some('*') => {
                    let (el, ec) = (self.line, self.col);
                    self.bump();
                    self.bump();
                    loop {
                        match self.bump() {
                            None => {
                                return Err(LexError {
                                    line: el,
                                    col: ec,
                                    message: "unterminated block comment".into(),
                                });
                            }
                            Some('*') if self.peek() == Some('/') => {
                                self.bump();
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                // Annex B: `<!--` 视为行注释。
                // TODO: `-->` 的完整规则（`a --> b` 应为 `a-- > b`），phase 1 暂按普通标点切分。
                Some('<')
                    if self.peek2() == Some('!')
                        && self.peek3() == Some('-')
                        && self.peek4() == Some('-') =>
                {
                    while !matches!(self.peek(), None | Some('\n') | Some('\r')) {
                        self.bump();
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    pub fn next_token(&mut self) -> Result<Token, LexError> {
        let t = self.next_token_inner()?;
        self.prev_kind = Some(t.kind.clone());
        Ok(t)
    }

    fn next_token_inner(&mut self) -> Result<Token, LexError> {
        self.skip_ws_and_comments()?;
        let (sl, sc) = (self.line, self.col);
        let c = match self.peek() {
            None => {
                return Ok(Token {
                    kind: TokenKind::Eof,
                    line: sl,
                    col: sc,
                    lexeme: String::new(),
                });
            }
            Some(c) => c,
        };
        if is_ident_start(c) {
            return self.read_ident_or_keyword();
        }
        if c.is_ascii_digit() {
            return self.read_number();
        }
        if c == '.' && self.peek2().map(|d| d.is_ascii_digit()).unwrap_or(false) {
            return self.read_number();
        }
        match c {
            '\'' | '"' => self.read_string(c),
            '`' => self.read_template(),
            _ => self.read_punct(),
        }
    }

    fn read_ident_or_keyword(&mut self) -> Result<Token, LexError> {
        let (sl, sc, sp) = (self.line, self.col, self.pos);
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        let name: String = self.chars[sp..self.pos].iter().collect();
        let kind = match Keyword::from_str(&name) {
            Some(k) => TokenKind::Keyword(k),
            None => TokenKind::Ident(name),
        };
        Ok(self.token(kind, sl, sc, sp))
    }

    /// 读取 `radix` 进制整数（允许 `_` 分隔符，必须夹在数字之间）。
    /// 返回 (值, 数字个数)。
    fn read_int_digits(
        &mut self,
        radix: u32,
        what: &str,
        allow_empty: bool,
    ) -> Result<(f64, usize), LexError> {
        let mut value = 0.0f64;
        let mut count = 0usize;
        let mut prev_sep = true; // 分隔符前面必须是数字
        loop {
            match self.peek() {
                Some('_') => {
                    let next_ok = self
                        .peek2()
                        .map(|c| c.to_digit(radix).is_some())
                        .unwrap_or(false);
                    if prev_sep || !next_ok {
                        return self.err("numeric separator must be between digits");
                    }
                    self.bump();
                    prev_sep = true;
                }
                Some(c) => match c.to_digit(radix) {
                    Some(d) => {
                        value = value * radix as f64 + d as f64;
                        self.bump();
                        count += 1;
                        prev_sep = false;
                    }
                    None => break,
                },
                None => break,
            }
        }
        if count == 0 && !allow_empty {
            return self.err(format!("expected digits in {} literal", what));
        }
        Ok((value, count))
    }

    fn read_hex_digits(&mut self, n: usize, what: &str) -> Result<u32, LexError> {
        let mut value = 0u32;
        for _ in 0..n {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(d) => {
                    value = value * 16 + d;
                    self.bump();
                }
                None => return self.err(format!("bad {} escape", what)),
            }
        }
        Ok(value)
    }

    /// `\u{H+}`：1~6 个十六进制数字。
    fn read_hex_digits_braced(&mut self) -> Result<u32, LexError> {
        let mut value = 0u32;
        let mut count = 0usize;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
            if count == 6 {
                return self.err("unicode escape too long");
            }
            value = value * 16 + d;
            self.bump();
            count += 1;
        }
        if count == 0 {
            return self.err("bad unicode escape");
        }
        Ok(value)
    }

    fn finish_number(
        &self,
        sl: usize,
        sc: usize,
        sp: usize,
        value: f64,
        bigint: bool,
    ) -> Result<Token, LexError> {
        let raw: String = self.chars[sp..self.pos].iter().collect();
        Ok(self.token(
            TokenKind::Number(NumberLit { raw, value, bigint }),
            sl,
            sc,
            sp,
        ))
    }

    fn read_number(&mut self) -> Result<Token, LexError> {
        let (sl, sc, sp) = (self.line, self.col, self.pos);

        // 0x / 0o / 0b 前缀
        if self.peek() == Some('0') {
            match self.peek2() {
                Some('x') | Some('X') => {
                    self.bump();
                    self.bump();
                    let (v, _) = self.read_int_digits(16, "hexadecimal", false)?;
                    let bi = self.eat_bigint()?;
                    return self.finish_number(sl, sc, sp, v, bi);
                }
                Some('o') | Some('O') => {
                    self.bump();
                    self.bump();
                    let (v, _) = self.read_int_digits(8, "octal", false)?;
                    let bi = self.eat_bigint()?;
                    return self.finish_number(sl, sc, sp, v, bi);
                }
                Some('b') | Some('B') => {
                    self.bump();
                    self.bump();
                    let (v, _) = self.read_int_digits(2, "binary", false)?;
                    let bi = self.eat_bigint()?;
                    return self.finish_number(sl, sc, sp, v, bi);
                }
                _ => {}
            }
        }

        // 十进制：整数部分（前导 `.` 的情况如 `.5`，调用方已保证后面是数字）
        let mut value = 0.0;
        let mut is_float = false;
        if self.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            let (v, _) = self.read_int_digits(10, "decimal", false)?;
            value = v;
        }
        // 小数部分：`5.`、`5.5`、`.5` 都合法；`5..x` 里第二个点留给标点
        if self.peek() == Some('.') {
            self.bump();
            is_float = true;
            let (fv, fc) = self.read_int_digits(10, "fraction", true)?;
            if fc > 0 {
                value += fv / 10f64.powi(fc as i32);
            }
        }
        // 指数部分：`e` 后面必须跟数字（`[+-]` 可选），否则 `e` 留给标识符
        if matches!(self.peek(), Some('e') | Some('E')) {
            let exp_ok = match self.peek2() {
                Some(d) if d.is_ascii_digit() => true,
                Some('+') | Some('-') => {
                    self.peek3().map(|c| c.is_ascii_digit()).unwrap_or(false)
                }
                _ => false,
            };
            if exp_ok {
                self.bump(); // e
                let neg = if matches!(self.peek(), Some('+') | Some('-')) {
                    let n = self.peek() == Some('-');
                    self.bump();
                    n
                } else {
                    false
                };
                let (ev, _) = self.read_int_digits(10, "exponent", false)?;
                is_float = true;
                value *= 10f64.powf(if neg { -ev } else { ev });
            }
        }

        let bigint = self.eat_bigint()?;
        if bigint && is_float {
            return self.err("bigint literal cannot have a fraction or exponent");
        }
        self.finish_number(sl, sc, sp, value, bigint)
    }

    /// 消费 `123n` 的 `n` 后缀。
    fn eat_bigint(&mut self) -> Result<bool, LexError> {
        if self.peek() == Some('n') {
            // `n` 后面紧跟标识符字符是非法的（如 `123nx`），但 `in`/`of` 等关键字
            // 紧跟数字是合法的（`0in x`），所以这里不做检查，交给 parser。
            self.bump();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn read_string(&mut self, quote: char) -> Result<Token, LexError> {
        let (sl, sc, sp) = (self.line, self.col, self.pos);
        self.bump(); // 开引号
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return self.err("unterminated string literal"),
                Some('\n') | Some('\r') => {
                    return self.err("unterminated string literal");
                }
                Some('\\') => match self.bump() {
                    None => return self.err("unterminated string literal"),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('b') => out.push('\x08'),
                    Some('f') => out.push('\x0C'),
                    Some('v') => out.push('\x0B'),
                    Some('\\') => out.push('\\'),
                    Some('\'') => out.push('\''),
                    Some('"') => out.push('"'),
                    // TODO: legacy 八进制转义（`\01`）在严格模式是语法错误；
                    // phase 1 把 `\0` 一律当 NUL。
                    Some('0') => out.push('\0'),
                    Some('x') => {
                        let h = self.read_hex_digits(2, "hex")?;
                        out.push(char::from_u32(h).unwrap_or('\u{FFFD}'));
                    }
                    Some('u') => {
                        let h = if self.peek() == Some('{') {
                            self.bump();
                            let v = self.read_hex_digits_braced()?;
                            if self.peek() != Some('}') {
                                return self.err("expected '}' in unicode escape");
                            }
                            self.bump();
                            v
                        } else {
                            self.read_hex_digits(4, "unicode")?
                        };
                        out.push(char::from_u32(h).unwrap_or('\u{FFFD}'));
                    }
                    Some('\n') => {} // 行继续：换行直接丢掉
                    Some('\r') => {
                        if self.peek() == Some('\n') {
                            self.bump();
                        }
                    }
                    Some(c) => out.push(c), // `\a` -> `a` 之类未知转义保留原字符
                },
                Some(c) if c == quote => break,
                Some(c) => out.push(c),
            }
        }
        Ok(self.token(TokenKind::Str(out), sl, sc, sp))
    }

    /// 跳过引号字符串（模板扫描器内部用，不解码）。
    fn skip_quoted(&mut self, quote: char) -> Result<(), LexError> {
        // 调用时开引号已消费
        loop {
            match self.bump() {
                None => return self.err("unterminated string literal"),
                Some('\n') | Some('\r') => {
                    return self.err("unterminated string literal");
                }
                Some('\\') => {
                    self.bump();
                }
                Some(c) if c == quote => return Ok(()),
                _ => {}
            }
        }
    }

    /// 模板字符串 phase 1：整体切分为单个 token（正确处理 `${}` 嵌套、
    /// 嵌套模板、转义反引号；`${}` 里的引号字符串会被跳过以免干扰扫描）。
    fn read_template(&mut self) -> Result<Token, LexError> {
        let (sl, sc, sp) = (self.line, self.col, self.pos);
        self.bump(); // 开反引号
        let inner_start = self.pos;
        self.skip_template()?;
        let inner_end = self.pos - 1; // 收反引号之前
        let inner: String = self.chars[inner_start..inner_end].iter().collect();
        Ok(self.token(TokenKind::Template(inner), sl, sc, sp))
    }

    /// 消费模板剩余部分（调用时开反引号已消费），正确处理嵌套。
    fn skip_template(&mut self) -> Result<(), LexError> {
        // depth == 0：在模板文本区；depth > 0：在 `${...}` 表达式里
        let mut depth = 0usize;
        loop {
            match self.bump() {
                None => return self.err("unterminated template literal"),
                Some('\\') => {
                    self.bump(); // 转义：跳过下一个字符
                }
                Some('`') if depth == 0 => return Ok(()),
                Some('`') => self.skip_template()?, // `${}` 里的嵌套模板
                Some('\'') | Some('"') if depth > 0 => {
                    let q = self.chars[self.pos - 1];
                    self.skip_quoted(q)?; // `${}` 里的字符串，避免引号干扰
                }
                Some('$') if depth == 0 && self.peek() == Some('{') => {
                    self.bump();
                    depth = 1;
                }
                Some('{') if depth > 0 => depth += 1,
                Some('}') if depth > 0 => depth -= 1,
                _ => {}
            }
        }
    }

    /// 标点 / 运算符：longest-match（4 字符 -> 3 字符 -> 2 字符 -> 1 字符）。
    /// TODO(phase 2): 正则字面量 `/ab+c/g` 需要 parser 上下文才能区分除法，
    /// 目前 `/` 一律按除法切分。
    fn read_punct(&mut self) -> Result<Token, LexError> {
        let (sl, sc, sp) = (self.line, self.col, self.pos);
        let a = self.at(0);
        let b = self.at(1);
        let c = self.at(2);
        let d = self.at(3);

        // 正则字面量（phase 7）：`/` 开头且前一个 token 允许表达式开头时，
        // 尝试按正则扫描。`//`、`/*` 已被 skip_ws_and_comments 吃掉，
        // `/=` 恒为除法赋值。
        // 若扫描失败（未终结），回退为除法（parser 会报更合适的错误；
        // 且单个 `/` 本身就该是除号）。
        if a == Some('/') && b != Some('=') && self.regex_allowed_after() {
            let save = (
                self.pos,
                self.line,
                self.col,
                self.prev_kind.clone(),
            );
            match self.read_regex(sl, sc, sp) {
                Ok(t) => return Ok(t),
                Err(_) => {
                    let (pos, line, col, prev) = save;
                    self.pos = pos;
                    self.line = line;
                    self.col = col;
                    self.prev_kind = prev;
                }
            }
        }

        // 4 字符：>>>=
        if a == Some('>') && b == Some('>') && c == Some('>') && d == Some('=') {
            for _ in 0..4 {
                self.bump();
            }
            return Ok(self.token(TokenKind::Punct(Punct::UShrAssign), sl, sc, sp));
        }

        // 3 字符
        let k3 = match (a, b, c) {
            (Some('='), Some('='), Some('=')) => Some(Punct::StrictEq),
            (Some('!'), Some('='), Some('=')) => Some(Punct::StrictNe),
            (Some('>'), Some('>'), Some('>')) => Some(Punct::UShr),
            (Some('<'), Some('<'), Some('=')) => Some(Punct::ShlAssign),
            (Some('>'), Some('>'), Some('=')) => Some(Punct::ShrAssign),
            (Some('*'), Some('*'), Some('=')) => Some(Punct::PowAssign),
            (Some('.'), Some('.'), Some('.')) => Some(Punct::Ellipsis),
            (Some('&'), Some('&'), Some('=')) => Some(Punct::AmpAmpAssign),
            (Some('|'), Some('|'), Some('=')) => Some(Punct::PipePipeAssign),
            (Some('?'), Some('?'), Some('=')) => Some(Punct::QuestionQuestionAssign),
            _ => None,
        };
        if let Some(k) = k3 {
            for _ in 0..3 {
                self.bump();
            }
            return Ok(self.token(TokenKind::Punct(k), sl, sc, sp));
        }

        // 2 字符。注意 `?.` 后面跟数字时不是可选链（`a?.5:0` 是三元表达式）。
        let qd_ok = !matches!(c, Some(ch) if ch.is_ascii_digit());
        let k2 = match (a, b) {
            (Some('='), Some('>')) => Some(Punct::Arrow),
            (Some('='), Some('=')) => Some(Punct::Eq),
            (Some('!'), Some('=')) => Some(Punct::Ne),
            (Some('<'), Some('=')) => Some(Punct::Le),
            (Some('>'), Some('=')) => Some(Punct::Ge),
            (Some('<'), Some('<')) => Some(Punct::Shl),
            (Some('>'), Some('>')) => Some(Punct::Shr),
            (Some('*'), Some('*')) => Some(Punct::Pow),
            (Some('+'), Some('+')) => Some(Punct::PlusPlus),
            (Some('-'), Some('-')) => Some(Punct::MinusMinus),
            (Some('+'), Some('=')) => Some(Punct::PlusAssign),
            (Some('-'), Some('=')) => Some(Punct::MinusAssign),
            (Some('*'), Some('=')) => Some(Punct::StarAssign),
            (Some('/'), Some('=')) => Some(Punct::SlashAssign),
            (Some('%'), Some('=')) => Some(Punct::PercentAssign),
            (Some('&'), Some('=')) => Some(Punct::AmpAssign),
            (Some('|'), Some('=')) => Some(Punct::PipeAssign),
            (Some('^'), Some('=')) => Some(Punct::CaretAssign),
            (Some('&'), Some('&')) => Some(Punct::AmpAmp),
            (Some('|'), Some('|')) => Some(Punct::PipePipe),
            (Some('?'), Some('?')) => Some(Punct::QuestionQuestion),
            (Some('?'), Some('.')) if qd_ok => Some(Punct::QuestionDot),
            _ => None,
        };
        if let Some(k) = k2 {
            self.bump();
            self.bump();
            return Ok(self.token(TokenKind::Punct(k), sl, sc, sp));
        }

        // 1 字符
        let k1 = match a {
            Some('=') => Punct::Assign,
            Some('<') => Punct::Lt,
            Some('>') => Punct::Gt,
            Some('+') => Punct::Plus,
            Some('-') => Punct::Minus,
            Some('*') => Punct::Star,
            Some('/') => Punct::Slash,
            Some('%') => Punct::Percent,
            Some('&') => Punct::Amp,
            Some('|') => Punct::Pipe,
            Some('^') => Punct::Caret,
            Some('!') => Punct::Bang,
            Some('~') => Punct::Tilde,
            Some('?') => Punct::Question,
            Some(':') => Punct::Colon,
            Some(';') => Punct::Semi,
            Some(',') => Punct::Comma,
            Some('.') => Punct::Dot,
            Some('(') => Punct::LParen,
            Some(')') => Punct::RParen,
            Some('{') => Punct::LBrace,
            Some('}') => Punct::RBrace,
            Some('[') => Punct::LBracket,
            Some(']') => Punct::RBracket,
            Some('#') => Punct::Hash,
            _ => return self.err(format!("unexpected character {:?}", a)),
        };
        self.bump();
        Ok(self.token(TokenKind::Punct(k1), sl, sc, sp))
    }

    /// `/` 在当前位置是否可能开始一个正则字面量：看前一个 token。
    /// 表达式可开头的位置 → true；值/后缀运算符之后 → false（除法）。
    /// 已知偏差：`x = {} / 2` 会被误判为正则（V8 靠 parser 反馈区分，
    /// 单遍词法做不到；这种写法极罕见）。
    fn regex_allowed_after(&self) -> bool {
        let prev = match &self.prev_kind {
            None => return true, // 输入开头
            Some(k) => k,
        };
        match prev {
            // 分隔符 / 运算符 / 赋值之后：表达式可开头。
            TokenKind::Punct(p) => matches!(
                p,
                Punct::LParen
                    | Punct::Comma
                    | Punct::LBracket
                    | Punct::LBrace
                    | Punct::Semi
                    | Punct::Colon
                    | Punct::Question
                    | Punct::Arrow
                    | Punct::Ellipsis
                    | Punct::RBrace // 语句位置：`if (x) {} /re/`
                    | Punct::Assign
                    | Punct::PlusAssign
                    | Punct::MinusAssign
                    | Punct::StarAssign
                    | Punct::SlashAssign
                    | Punct::PercentAssign
                    | Punct::PowAssign
                    | Punct::ShlAssign
                    | Punct::ShrAssign
                    | Punct::UShrAssign
                    | Punct::AmpAssign
                    | Punct::PipeAssign
                    | Punct::CaretAssign
                    | Punct::AmpAmpAssign
                    | Punct::PipePipeAssign
                    | Punct::QuestionQuestionAssign
                    | Punct::Plus
                    | Punct::Minus
                    | Punct::Star
                    | Punct::Slash
                    | Punct::Percent
                    | Punct::Pow
                    | Punct::Amp
                    | Punct::Pipe
                    | Punct::Caret
                    | Punct::Shl
                    | Punct::Shr
                    | Punct::UShr
                    | Punct::Lt
                    | Punct::Le
                    | Punct::Gt
                    | Punct::Ge
                    | Punct::Eq
                    | Punct::Ne
                    | Punct::StrictEq
                    | Punct::StrictNe
                    | Punct::AmpAmp
                    | Punct::PipePipe
                    | Punct::QuestionQuestion
                    | Punct::QuestionDot
                    | Punct::Bang
                    | Punct::Tilde,
            ),
            // 这些关键字后可接表达式。
            TokenKind::Keyword(k) => matches!(
                k,
                Keyword::Return
                    | Keyword::Throw
                    | Keyword::Typeof
                    | Keyword::Void
                    | Keyword::Delete
                    | Keyword::New
                    | Keyword::In
                    | Keyword::Of
                    | Keyword::Instanceof
                    | Keyword::Case
                    | Keyword::Do
                    | Keyword::Else
                    | Keyword::Var
                    | Keyword::Let
                    | Keyword::Const,
            ),
            // 标识符 / 字面量 / `)` / `]` 之后是除法。
            _ => false,
        }
    }

    /// 扫描正则字面量 `/pattern/flags`（调用时已确认开头是 `/`）。
    fn read_regex(
        &mut self,
        sl: usize,
        sc: usize,
        sp: usize,
    ) -> Result<Token, LexError> {
        self.bump(); // /
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            match self.peek() {
                None => return self.err("unterminated regex literal"),
                Some('\n') | Some('\r') => {
                    return self.err("unterminated regex literal")
                }
                Some('\\') => {
                    pattern.push('\\');
                    self.bump();
                    match self.peek() {
                        None => return self.err("unterminated regex literal"),
                        Some(c) => {
                            pattern.push(c);
                            self.bump();
                        }
                    }
                }
                Some('[') => {
                    in_class = true;
                    pattern.push('[');
                    self.bump();
                }
                Some(']') => {
                    in_class = false;
                    pattern.push(']');
                    self.bump();
                }
                Some('/') if !in_class => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    pattern.push(c);
                    self.bump();
                }
            }
        }
        let mut flags = String::new();
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            flags.push(self.peek().unwrap());
            self.bump();
        }
        Ok(self.token(TokenKind::Regex { pattern, flags }, sl, sc, sp))
    }
}

/// 一次性切分整个输入（末尾带一个 Eof token）。
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let mut lx = Lexer::new(input);
    let mut out = Vec::new();
    loop {
        let t = lx.next_token()?;
        let done = matches!(t.kind, TokenKind::Eof);
        out.push(t);
        if done {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds_no_eof(src: &str) -> Vec<TokenKind> {
        lex(src)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Eof))
            .collect()
    }

    fn all_keywords() -> Vec<Keyword> {
        vec![
            Keyword::Var, Keyword::Let, Keyword::Const, Keyword::Function,
            Keyword::Return, Keyword::If, Keyword::Else, Keyword::For,
            Keyword::While, Keyword::Do, Keyword::Break, Keyword::Continue,
            Keyword::New, Keyword::Delete, Keyword::Typeof, Keyword::Void,
            Keyword::Instanceof, Keyword::In, Keyword::Of, Keyword::True,
            Keyword::False, Keyword::Null, Keyword::Undefined, Keyword::This,
            Keyword::Super, Keyword::Class, Keyword::Extends, Keyword::Import,
            Keyword::Export, Keyword::Default, Keyword::Try, Keyword::Catch,
            Keyword::Finally, Keyword::Throw, Keyword::Switch, Keyword::Case,
            Keyword::Async, Keyword::Await, Keyword::Yield, Keyword::Static,
            Keyword::Get, Keyword::Set,
        ]
    }

    #[test]
    fn keywords_vs_identifiers() {
        // 关键字必须精确匹配：`variable` 是标识符不是 `var` + `iable`
        let ks = kinds_no_eof("var variable let letter const");
        assert_eq!(
            ks,
            vec![
                TokenKind::Keyword(Keyword::Var),
                TokenKind::Ident("variable".into()),
                TokenKind::Keyword(Keyword::Let),
                TokenKind::Ident("letter".into()),
                TokenKind::Keyword(Keyword::Const),
            ]
        );
        // `$` / `_` 开头与大小写：`Var` 不是关键字
        let ks = kinds_no_eof("$x _y Var VAR");
        assert_eq!(
            ks,
            vec![
                TokenKind::Ident("$x".into()),
                TokenKind::Ident("_y".into()),
                TokenKind::Ident("Var".into()),
                TokenKind::Ident("VAR".into()),
            ]
        );
        // 全部 42 个关键字逐个 round-trip
        for kw in all_keywords() {
            let t = lex(kw.as_str()).unwrap();
            assert_eq!(t.len(), 2); // keyword + Eof
            assert_eq!(t[0].kind, TokenKind::Keyword(kw.clone()));
            assert_eq!(t[0].lexeme, kw.as_str());
        }
    }

    #[test]
    fn numbers_all_radixes() {
        let cases: &[(&str, f64)] = &[
            ("42", 42.0),
            ("3.14", 3.14),
            ("0.5", 0.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e3", 1000.0),
            ("1E-3", 0.001),
            ("2.5e+2", 250.0),
            ("0xFF", 255.0),
            ("0xff", 255.0),
            ("0xdeadBEEF", 3735928559.0),
            ("0o17", 15.0),
            ("0O777", 511.0),
            ("0b101", 5.0),
            ("0B1010", 10.0),
            ("1_000", 1000.0),
            ("0xF_F", 255.0),
            ("1e1_0", 10000000000.0),
        ];
        for (src, want) in cases {
            let t = lex(src).unwrap();
            match &t[0].kind {
                TokenKind::Number(n) => {
                    assert!(
                        (n.value - want).abs() < 1e-6,
                        "{} => {} (want {})",
                        src,
                        n.value,
                        want
                    );
                    assert_eq!(n.raw, *src);
                }
                other => panic!("{} lexed as {:?}", src, other),
            }
        }
        // BigInt 后缀
        let t = lex("123n").unwrap();
        match &t[0].kind {
            TokenKind::Number(n) => {
                assert!(n.bigint);
                assert_eq!(n.value, 123.0);
            }
            other => panic!("123n lexed as {:?}", other),
        }
        // 非法字面量
        assert!(lex("0x").is_err());
        assert!(lex("0b").is_err());
        assert!(lex("1__2").is_err());
        assert!(lex("0xG").is_err());
        assert!(lex("1.5n").is_err()); // bigint 不能带小数
    }

    #[test]
    fn string_escapes() {
        let cases: &[(&str, &str)] = &[
            ("'hello'", "hello"),
            ("\"world\"", "world"),
            ("'a\\nb'", "a\nb"),
            ("'a\\tb\\rc'", "a\tb\rc"),
            ("'\\x41'", "A"),
            ("'\\u0041'", "A"),
            ("'\\u{1F600}'", "\u{1F600}"),
            ("'don\\'t'", "don't"),
            ("\"say \\\"hi\\\"\"", "say \"hi\""),
            ("'\\\\'", "\\"),
            ("'\\0'", "\0"),
            ("'a\\\n b'", "a b"), // 行继续：换行被丢掉
            ("'\\q'", "q"),       // 未知转义保留原字符
        ];
        for (src, want) in cases {
            let t = lex(src).unwrap();
            match &t[0].kind {
                TokenKind::Str(s) => assert_eq!(s, want, "source: {}", src),
                other => panic!("{} lexed as {:?}", src, other),
            }
        }
        // 未闭合 / 裸换行
        assert!(lex("'oops").is_err());
        assert!(lex("\"oops").is_err());
        assert!(lex("'a\nb'").is_err());
        assert!(lex("'\\x4'").is_err()); // \x 后必须跟 2 位
        assert!(lex("'\\u12'").is_err());
    }

    #[test]
    fn template_strings() {
        // 整体切分为单个 token
        let t = lex("`hello ${name}, you are ${a + b}!`").unwrap();
        assert_eq!(t.len(), 2);
        match &t[0].kind {
            TokenKind::Template(raw) => {
                assert_eq!(raw, "hello ${name}, you are ${a + b}!");
            }
            other => panic!("template lexed as {:?}", other),
        }
        // 嵌套模板
        let t = lex("`a ${`b ${c}`} d`").unwrap();
        assert_eq!(t.len(), 2);
        // 转义反引号不算结束
        let t = lex("`a\\`b`").unwrap();
        assert_eq!(t.len(), 2);
        // `${}` 里的引号不干扰扫描
        let t = lex("`${\"`\"}`").unwrap();
        assert_eq!(t.len(), 2);
        // 未闭合
        assert!(lex("`oops").is_err());
        assert!(lex("`oops ${x").is_err());
    }

    #[test]
    fn comments_are_skipped() {
        let ks = kinds_no_eof("var /* block\ncomment */ x; // line comment\nlet y;");
        assert_eq!(
            ks,
            vec![
                TokenKind::Keyword(Keyword::Var),
                TokenKind::Ident("x".into()),
                TokenKind::Punct(Punct::Semi),
                TokenKind::Keyword(Keyword::Let),
                TokenKind::Ident("y".into()),
                TokenKind::Punct(Punct::Semi),
            ]
        );
        assert!(lex("/* never ends").is_err());
        // 注释不影响行号
        let t = lex("// c1\n/* c2 */\nvar x;").unwrap();
        assert_eq!((t[0].line, t[0].col), (3, 1));
    }

    #[test]
    fn operators_maximal_munch() {
        // 全部标点逐个 round-trip
        let all = vec![
            Punct::Assign, Punct::Eq, Punct::StrictEq, Punct::Ne, Punct::StrictNe,
            Punct::Arrow, Punct::Lt, Punct::Le, Punct::Shl, Punct::ShlAssign,
            Punct::Gt, Punct::Ge, Punct::Shr, Punct::ShrAssign, Punct::UShr,
            Punct::UShrAssign, Punct::Plus, Punct::PlusPlus, Punct::PlusAssign,
            Punct::Minus, Punct::MinusMinus, Punct::MinusAssign, Punct::Star,
            Punct::StarAssign, Punct::Pow, Punct::PowAssign, Punct::Slash,
            Punct::SlashAssign, Punct::Percent, Punct::PercentAssign,
            Punct::Amp, Punct::AmpAmp, Punct::AmpAssign, Punct::AmpAmpAssign,
            Punct::Pipe, Punct::PipePipe, Punct::PipeAssign,
            Punct::PipePipeAssign, Punct::Caret, Punct::CaretAssign,
            Punct::Bang, Punct::Tilde, Punct::Question, Punct::QuestionQuestion,
            Punct::QuestionQuestionAssign, Punct::QuestionDot, Punct::Colon,
            Punct::Semi, Punct::Comma, Punct::Dot, Punct::Ellipsis,
            Punct::LParen, Punct::RParen, Punct::LBrace, Punct::RBrace,
            Punct::LBracket, Punct::RBracket,
        ];
        assert_eq!(all.len(), 57);
        for p in &all {
            let t = lex(p.as_str()).unwrap();
            assert_eq!(t.len(), 2, "punct {:?}", p.as_str());
            assert_eq!(t[0].kind, TokenKind::Punct(p.clone()));
            assert_eq!(t[0].lexeme, p.as_str());
        }
        // 贪婪匹配：`a=b` 不能切成 `a` `==` ...
        assert_eq!(
            kinds_no_eof("a===b"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Punct(Punct::StrictEq),
                TokenKind::Ident("b".into()),
            ]
        );
        assert_eq!(
            kinds_no_eof("x >>>= 2"),
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::Punct(Punct::UShrAssign),
                TokenKind::Number(NumberLit {
                    raw: "2".into(),
                    value: 2.0,
                    bigint: false,
                }),
            ]
        );
        // `?.` 后跟数字不是可选链：`a?.5:0` 是三元表达式
        let ks = kinds_no_eof("a?.5:0");
        assert_eq!(
            ks,
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Punct(Punct::Question),
                TokenKind::Number(NumberLit {
                    raw: ".5".into(),
                    value: 0.5,
                    bigint: false,
                }),
                TokenKind::Punct(Punct::Colon),
                TokenKind::Number(NumberLit {
                    raw: "0".into(),
                    value: 0.0,
                    bigint: false,
                }),
            ]
        );
        // 正常可选链
        assert_eq!(
            kinds_no_eof("x?.y ?? z"),
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::Punct(Punct::QuestionDot),
                TokenKind::Ident("y".into()),
                TokenKind::Punct(Punct::QuestionQuestion),
                TokenKind::Ident("z".into()),
            ]
        );
        // 复合赋值
        assert_eq!(
            kinds_no_eof("a &&= b ||= c ??= d"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Punct(Punct::AmpAmpAssign),
                TokenKind::Ident("b".into()),
                TokenKind::Punct(Punct::PipePipeAssign),
                TokenKind::Ident("c".into()),
                TokenKind::Punct(Punct::QuestionQuestionAssign),
                TokenKind::Ident("d".into()),
            ]
        );
    }

    #[test]
    fn full_snippet() {
        // 题目要求的完整小段代码
        let t = lex("const add = (a, b) => a + b; // comment").unwrap();
        let ks: Vec<TokenKind> = t
            .iter()
            .map(|x| x.kind.clone())
            .filter(|k| !matches!(k, TokenKind::Eof))
            .collect();
        assert_eq!(
            ks,
            vec![
                TokenKind::Keyword(Keyword::Const),
                TokenKind::Ident("add".into()),
                TokenKind::Punct(Punct::Assign),
                TokenKind::Punct(Punct::LParen),
                TokenKind::Ident("a".into()),
                TokenKind::Punct(Punct::Comma),
                TokenKind::Ident("b".into()),
                TokenKind::Punct(Punct::RParen),
                TokenKind::Punct(Punct::Arrow),
                TokenKind::Ident("a".into()),
                TokenKind::Punct(Punct::Plus),
                TokenKind::Ident("b".into()),
                TokenKind::Punct(Punct::Semi),
            ]
        );
        // 位置：`add` 在第 1 行第 7 列；注释被跳过不影响
        assert_eq!((t[1].line, t[1].col), (1, 7));
        assert_eq!(t[1].lexeme, "add");
    }

    #[test]
    fn line_col_tracking() {
        let t = lex("var a;\n  let b;").unwrap();
        assert_eq!((t[0].line, t[0].col), (1, 1)); // var
        assert_eq!((t[1].line, t[1].col), (1, 5)); // a
        assert_eq!((t[3].line, t[3].col), (2, 3)); // let
        assert_eq!((t[4].line, t[4].col), (2, 7)); // b
        // \r\n 算一个换行
        let t = lex("a;\r\nb;").unwrap();
        assert_eq!((t[2].line, t[2].col), (2, 1));
        // Eof 位置在输入末尾
        let last = t.last().unwrap();
        assert!(matches!(last.kind, TokenKind::Eof));
    }

    #[test]
    fn misc_edge_cases() {
        // hashbang
        let t = lex("#!/usr/bin/env node\nvar x;").unwrap();
        assert!(matches!(t[0].kind, TokenKind::Keyword(Keyword::Var)));
        // Annex B 的 <!-- 行注释
        let t = lex("<!-- old school\nvar x;").unwrap();
        assert!(matches!(t[0].kind, TokenKind::Keyword(Keyword::Var)));
        // 关键字紧跟数字是合法的（`0in x`）
        let ks = kinds_no_eof("0in x");
        assert_eq!(
            ks,
            vec![
                TokenKind::Number(NumberLit {
                    raw: "0".into(),
                    value: 0.0,
                    bigint: false,
                }),
                TokenKind::Keyword(Keyword::In),
                TokenKind::Ident("x".into()),
            ]
        );
        // 非法字符
        assert!(lex("@").is_err());
        // 空输入：只有 Eof
        let t = lex("").unwrap();
        assert_eq!(t.len(), 1);
        assert!(matches!(t[0].kind, TokenKind::Eof));
    }
}
