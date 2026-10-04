//! yousj-js · Phase 7：手写极简正则引擎（零依赖）。
//!
//! - 支持：字面量 `/ab+c/` 与 `new RegExp("ab+c", "gi")`；`.` `*` `+` `?`
//!   `{n,m}` `[]` `()`（捕获与 `(?:)`）`^$` `|` `\d\w\s\D\W\S`
//!   `\b\B` `\n\t\r\f\v\0` `\xHH` `\uHHHH`；flag `g`/`i`/`m`。
//! - 不支持（遇到直接报编译错误）：命名组、lookahead/lookbehind、
//!   反向引用 `\1`、`\p{...}`。
//! - 回溯按"树形多结果"实现（`match_ends` 返回所有可能的结束位置，
//!   保持优先级顺序），并带步数上限（默认 10 万步）防 ReDoS hang；
//!   熔断后按"无匹配"处理（`test` → false，`exec` → null）。
//! - `String.prototype.replace` 的替换串不支持 `$1` 等引用（按字面量处理）。

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

/// 回溯步数上限（单次 `search` 内 `match_ends` 调用次数）。
pub const MAX_REGEX_STEPS: u64 = 100_000;

/// 正则 flag（`g`/`i`/`m`；重复或未知 flag 视为编译错误）。
#[derive(Debug, Clone, Default)]
pub struct RegexFlags {
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
}

impl RegexFlags {
    pub fn as_string(&self) -> String {
        let mut s = String::new();
        if self.global {
            s.push('g');
        }
        if self.ignore_case {
            s.push('i');
        }
        if self.multiline {
            s.push('m');
        }
        s
    }
}

pub fn parse_flags(flags: &str) -> Result<RegexFlags, String> {
    let mut f = RegexFlags::default();
    for c in flags.chars() {
        match c {
            'g' if !f.global => f.global = true,
            'i' if !f.ignore_case => f.ignore_case = true,
            'm' if !f.multiline => f.multiline = true,
            _ => return Err(format!("invalid regex flag {:?}", c)),
        }
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// 模式 AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Perl {
    Digit,
    Word,
    Space,
}

fn perl_matches(p: Perl, ch: char) -> bool {
    match p {
        Perl::Digit => ch.is_ascii_digit(),
        Perl::Word => ch.is_ascii_alphanumeric() || ch == '_',
        Perl::Space => matches!(ch, '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' '),
    }
}

#[derive(Debug, Clone, Default)]
struct CharClass {
    negated: bool,
    chars: Vec<char>,
    ranges: Vec<(char, char)>,
    perls: Vec<(Perl, bool)>, // (类, 是否取反)，如 \D
}

impl CharClass {
    fn matches(&self, ch: char, ignore_case: bool) -> bool {
        let eq = |a: char, b: char| {
            a == b || (ignore_case && lower1(a) == lower1(b))
        };
        let mut hit = self.chars.iter().any(|c| eq(*c, ch));
        hit |= self.ranges.iter().any(|(lo, hi)| {
            if ignore_case {
                lower1(ch) >= lower1(*lo) && lower1(ch) <= lower1(*hi)
            } else {
                ch >= *lo && ch <= *hi
            }
        });
        hit |= self
            .perls
            .iter()
            .any(|(p, neg)| perl_matches(*p, ch) != *neg);
        if self.negated {
            !hit
        } else {
            hit
        }
    }
}

fn lower1(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Literal(char),
    Dot,
    Class(CharClass),
    Start,
    End,
    WordBoundary(bool), // true = \b，false = \B
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Quant {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    Group {
        idx: usize,
        node: Box<Node>,
    },
}

// ---------------------------------------------------------------------------
// 模式解析
// ---------------------------------------------------------------------------

struct PatParser {
    chars: Vec<char>,
    pos: usize,
    group_count: usize,
}

impl PatParser {
    fn new(pattern: &str) -> Self {
        PatParser {
            chars: pattern.chars().collect(),
            pos: 0,
            group_count: 0,
        }
    }

    fn err(&self, msg: impl Into<String>) -> String {
        format!("invalid regex: {}", msg.into())
    }

    fn peek(&self) -> char {
        self.chars.get(self.pos).copied().unwrap_or('\0')
    }

    fn peek2(&self) -> char {
        self.chars.get(self.pos + 1).copied().unwrap_or('\0')
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn bump(&mut self) -> char {
        let c = self.peek();
        if !self.at_end() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == c {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(format!("expected {:?}", c)))
        }
    }

    fn parse_digits(&mut self) -> Option<usize> {
        let start = self.pos;
        let mut n = 0usize;
        while self.peek().is_ascii_digit() {
            n = n * 10 + (self.bump() as usize - '0' as usize);
        }
        if self.pos == start {
            None
        } else {
            Some(n)
        }
    }

    fn parse(&mut self) -> Result<Node, String> {
        let n = self.parse_alt()?;
        if !self.at_end() {
            return Err(self.err(format!(
                "unexpected character {:?}",
                self.peek()
            )));
        }
        Ok(n)
    }

    fn parse_alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_seq()?];
        while self.eat('|') {
            branches.push(self.parse_seq()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_seq(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while !self.at_end() && self.peek() != '|' && self.peek() != ')' {
            items.push(self.parse_quant()?);
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Node::Seq(items)),
        }
    }

    fn parse_quant(&mut self) -> Result<Node, String> {
        let atom = self.parse_atom()?;
        let (min, max) = match self.peek() {
            '*' => {
                self.bump();
                (0, None)
            }
            '+' => {
                self.bump();
                (1, None)
            }
            '?' => {
                self.bump();
                (0, Some(1))
            }
            '{' => match self.try_brace() {
                Some(q) => q,
                // 不是合法量词：`{` 按字面量处理（Annex B 宽松语义）。
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        let greedy = !self.eat('?');
        Ok(Node::Quant {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    /// 尝试解析 `{n}` / `{n,}` / `{n,m}`；失败时回退 pos 并返回 None。
    fn try_brace(&mut self) -> Option<(usize, Option<usize>)> {
        let save = self.pos;
        self.bump(); // {
        let n = self.parse_digits()?;
        let (min, max) = if self.eat(',') {
            if self.peek() == '}' {
                (n, None)
            } else {
                let m = self.parse_digits()?;
                if m < n {
                    self.pos = save;
                    return None;
                }
                (n, Some(m))
            }
        } else {
            (n, Some(n))
        };
        if !self.eat('}') {
            self.pos = save;
            return None;
        }
        Some((min, max))
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.peek() {
            '(' => self.parse_group(),
            '[' => self.parse_class(),
            '.' => {
                self.bump();
                Ok(Node::Dot)
            }
            '^' => {
                self.bump();
                Ok(Node::Start)
            }
            '$' => {
                self.bump();
                Ok(Node::End)
            }
            '\\' => self.parse_escape_atom(),
            c => {
                self.bump();
                Ok(Node::Literal(c))
            }
        }
    }

    fn parse_group(&mut self) -> Result<Node, String> {
        self.bump(); // (
        if self.eat('?') {
            match self.peek() {
                ':' => {
                    self.bump();
                    let n = self.parse_alt()?;
                    self.expect(')')?;
                    Ok(n)
                }
                '=' | '!' => Err(self.err("lookahead is not supported")),
                '<' => Err(self.err("named capture groups are not supported")),
                _ => Err(self.err("invalid group")),
            }
        } else {
            let idx = self.group_count;
            self.group_count += 1;
            let n = self.parse_alt()?;
            self.expect(')')?;
            Ok(Node::Group {
                idx,
                node: Box::new(n),
            })
        }
    }

    /// 解析 `\` 转义（类外部）。
    fn parse_escape_atom(&mut self) -> Result<Node, String> {
        self.bump(); // \
        if self.at_end() {
            return Err(self.err("trailing backslash"));
        }
        let c = self.bump();
        match c {
            'd' => Ok(class_node(false, vec![], vec![], vec![(Perl::Digit, false)])),
            'w' => Ok(class_node(false, vec![], vec![], vec![(Perl::Word, false)])),
            's' => Ok(class_node(false, vec![], vec![], vec![(Perl::Space, false)])),
            'D' => Ok(class_node(false, vec![], vec![], vec![(Perl::Digit, true)])),
            'W' => Ok(class_node(false, vec![], vec![], vec![(Perl::Word, true)])),
            'S' => Ok(class_node(false, vec![], vec![], vec![(Perl::Space, true)])),
            'n' => Ok(Node::Literal('\n')),
            't' => Ok(Node::Literal('\t')),
            'r' => Ok(Node::Literal('\r')),
            'f' => Ok(Node::Literal('\x0C')),
            'v' => Ok(Node::Literal('\x0B')),
            '0' => Ok(Node::Literal('\0')),
            'b' => Ok(Node::WordBoundary(true)),
            'B' => Ok(Node::WordBoundary(false)),
            'x' => {
                let h = self.parse_hex(2)?;
                Ok(Node::Literal(char::from_u32(h).unwrap_or('\u{FFFD}')))
            }
            'u' => {
                let h = self.parse_hex(4)?;
                Ok(Node::Literal(char::from_u32(h).unwrap_or('\u{FFFD}')))
            }
            '1'..='9' => Err(self.err("backreferences are not supported")),
            c if c.is_ascii_alphanumeric() => {
                Err(self.err(format!("unsupported escape \\{}", c)))
            }
            // 标点转义按字面量（`\.` `\/` 等，Annex B）。
            c => Ok(Node::Literal(c)),
        }
    }

    fn parse_hex(&mut self, n: usize) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..n {
            let c = self.peek();
            let d = c.to_digit(16).ok_or_else(|| {
                self.err(format!("expected hex digit, found {:?}", c))
            })?;
            v = v * 16 + d;
            self.bump();
        }
        Ok(v)
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        self.bump(); // [
        let negated = self.eat('^');
        let mut cc = CharClass {
            negated,
            ..CharClass::default()
        };
        // `[]` / `[^]` 的首个 `]` 按字面量。
        if self.peek() == ']' {
            cc.chars.push(']');
            self.bump();
        }
        loop {
            if self.at_end() {
                return Err(self.err("unterminated character class"));
            }
            if self.eat(']') {
                break;
            }
            let lo = self.parse_class_atom()?;
            // 范围 `a-z`（`-` 在末尾或后跟 `]` 时按字面量）。
            if lo.is_char() && self.peek() == '-' && self.peek2() != ']' {
                self.bump(); // -
                let hi = self.parse_class_atom()?;
                match (lo, hi) {
                    (ClassAtom::Char(a), ClassAtom::Char(b)) => {
                        if b < a {
                            return Err(self.err("range out of order in character class"));
                        }
                        cc.ranges.push((a, b));
                    }
                    (ClassAtom::Char(a), h) => {
                        cc.chars.push(a);
                        cc.chars.push('-');
                        cc.add_atom(h);
                    }
                    (l, _) => {
                        cc.add_atom(l);
                        cc.chars.push('-');
                    }
                }
            } else {
                cc.add_atom(lo);
            }
        }
        Ok(Node::Class(cc))
    }

    /// 类内的单个元素：字符或 perl 类。
    fn parse_class_atom(&mut self) -> Result<ClassAtom, String> {
        if self.eat('\\') {
            if self.at_end() {
                return Err(self.err("trailing backslash in character class"));
            }
            let c = self.bump();
            match c {
                'd' => Ok(ClassAtom::Perl(Perl::Digit, false)),
                'w' => Ok(ClassAtom::Perl(Perl::Word, false)),
                's' => Ok(ClassAtom::Perl(Perl::Space, false)),
                'D' => Ok(ClassAtom::Perl(Perl::Digit, true)),
                'W' => Ok(ClassAtom::Perl(Perl::Word, true)),
                'S' => Ok(ClassAtom::Perl(Perl::Space, true)),
                'n' => Ok(ClassAtom::Char('\n')),
                't' => Ok(ClassAtom::Char('\t')),
                'r' => Ok(ClassAtom::Char('\r')),
                'f' => Ok(ClassAtom::Char('\x0C')),
                'v' => Ok(ClassAtom::Char('\x0B')),
                '0' => Ok(ClassAtom::Char('\0')),
                // 类内 `\b` = 退格（0x08），不是单词边界。
                'b' => Ok(ClassAtom::Char('\x08')),
                'x' => {
                    let h = self.parse_hex(2)?;
                    Ok(ClassAtom::Char(char::from_u32(h).unwrap_or('\u{FFFD}')))
                }
                'u' => {
                    let h = self.parse_hex(4)?;
                    Ok(ClassAtom::Char(char::from_u32(h).unwrap_or('\u{FFFD}')))
                }
                c if c.is_ascii_alphanumeric() => {
                    Err(self.err(format!("unsupported escape \\{}", c)))
                }
                c => Ok(ClassAtom::Char(c)),
            }
        } else {
            Ok(ClassAtom::Char(self.bump()))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ClassAtom {
    Char(char),
    Perl(Perl, bool),
}

impl ClassAtom {
    fn is_char(&self) -> bool {
        matches!(self, ClassAtom::Char(_))
    }
}

impl CharClass {
    fn add_atom(&mut self, a: ClassAtom) {
        match a {
            ClassAtom::Char(c) => self.chars.push(c),
            ClassAtom::Perl(p, n) => self.perls.push((p, n)),
        }
    }
}

fn class_node(
    negated: bool,
    chars: Vec<char>,
    ranges: Vec<(char, char)>,
    perls: Vec<(Perl, bool)>,
) -> Node {
    Node::Class(CharClass {
        negated,
        chars,
        ranges,
        perls,
    })
}

// ---------------------------------------------------------------------------
// 编译产物与匹配器
// ---------------------------------------------------------------------------

/// 一次匹配的结果（字符下标）。
#[derive(Debug, Clone)]
pub struct RegexMatch {
    pub index: usize,
    pub text: String,
    pub groups: Vec<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct CompiledRegex {
    pub source: String,
    pub flags: RegexFlags,
    group_count: usize,
    root: Node,
}

impl CompiledRegex {
    pub fn compile(pattern: &str, flags: &str) -> Result<Self, String> {
        let flags = parse_flags(flags)?;
        let mut p = PatParser::new(pattern);
        let root = p.parse()?;
        Ok(CompiledRegex {
            source: pattern.to_string(),
            flags,
            group_count: p.group_count,
            root,
        })
    }

    pub fn group_count(&self) -> usize {
        self.group_count
    }

    /// 从字符下标 `from` 开始找第一个匹配（`from` 越界 → None）。
    pub fn search(&self, text: &str, from: usize) -> Option<RegexMatch> {
        let chars: Vec<char> = text.chars().collect();
        if from > chars.len() {
            return None;
        }
        let mut m = Matcher {
            chars,
            flags: &self.flags,
            steps: 0,
        };
        let empty_caps = vec![(None, None); self.group_count];
        // `^` 锚定（非 multiline）：只试 from 位置。
        let anchored = matches!(self.root, Node::Seq(ref v) if matches!(v.first(), Some(Node::Start)))
            || matches!(self.root, Node::Start);
        let starts: Vec<usize> = if anchored && !self.flags.multiline {
            vec![from]
        } else {
            (from..=m.chars.len()).collect()
        };
        for start in starts {
            let ends = m.match_ends(&self.root, start, &empty_caps);
            if m.steps > MAX_REGEX_STEPS {
                return None; // 熔断：按无匹配处理
            }
            if let Some((end, caps)) = ends.into_iter().next() {
                return Some(self.build_match(&m.chars, start, end, &caps));
            }
        }
        None
    }

    /// 是否匹配（`test` 语义：从 0 开始找）。
    pub fn test(&self, text: &str) -> bool {
        self.search(text, 0).is_some()
    }

    fn build_match(
        &self,
        chars: &[char],
        start: usize,
        end: usize,
        caps: &[(Option<usize>, Option<usize>)],
    ) -> RegexMatch {
        let text: String = chars[start..end].iter().collect();
        let groups = caps
            .iter()
            .map(|(s, e)| match (s, e) {
                (Some(a), Some(b)) => Some(chars[*a..*b].iter().collect()),
                _ => None,
            })
            .collect();
        RegexMatch {
            index: start,
            text,
            groups,
        }
    }
}

/// 捕获组快照：每组 (起始, 结束) 字符下标。
type Caps = Vec<(Option<usize>, Option<usize>)>;

struct Matcher<'a> {
    chars: Vec<char>,
    flags: &'a RegexFlags,
    steps: u64,
}

fn dedupe_ends(v: &mut Vec<(usize, Caps)>) {
    let mut seen = HashSet::new();
    v.retain(|(e, _)| seen.insert(*e));
}

impl<'a> Matcher<'a> {
    fn char_at(&self, pos: usize) -> Option<char> {
        self.chars.get(pos).copied()
    }

    fn chars_eq(&self, a: char, b: char) -> bool {
        a == b || (self.flags.ignore_case && lower1(a) == lower1(b))
    }

    /// 返回 node 在 pos 处匹配的所有可能 (结束位置, 捕获快照)，
    /// 按优先级排序（贪婪优先）。
    fn match_ends(
        &mut self,
        node: &Node,
        pos: usize,
        caps: &Caps,
    ) -> Vec<(usize, Caps)> {
        self.steps += 1;
        if self.steps > MAX_REGEX_STEPS {
            return Vec::new();
        }
        match node {
            Node::Empty => vec![(pos, caps.clone())],
            Node::Literal(c) => match self.char_at(pos) {
                Some(ch) if self.chars_eq(ch, *c) => vec![(pos + 1, caps.clone())],
                _ => vec![],
            },
            Node::Dot => match self.char_at(pos) {
                Some(ch) if ch != '\n' && ch != '\r' => vec![(pos + 1, caps.clone())],
                _ => vec![],
            },
            Node::Class(cc) => match self.char_at(pos) {
                Some(ch) if cc.matches(ch, self.flags.ignore_case) => {
                    vec![(pos + 1, caps.clone())]
                }
                _ => vec![],
            },
            Node::Start => {
                let ok = pos == 0
                    || (self.flags.multiline
                        && pos > 0
                        && self.char_at(pos - 1) == Some('\n'));
                if ok {
                    vec![(pos, caps.clone())]
                } else {
                    vec![]
                }
            }
            Node::End => {
                let ok = pos == self.chars.len()
                    || (self.flags.multiline && self.char_at(pos) == Some('\n'));
                if ok {
                    vec![(pos, caps.clone())]
                } else {
                    vec![]
                }
            }
            Node::WordBoundary(positive) => {
                let word = |p: usize| {
                    self.char_at(p)
                        .map_or(false, |c| c.is_ascii_alphanumeric() || c == '_')
                };
                let l = pos > 0 && word(pos - 1);
                let r = word(pos);
                if (l != r) == *positive {
                    vec![(pos, caps.clone())]
                } else {
                    vec![]
                }
            }
            Node::Seq(items) => {
                let mut cur = vec![(pos, caps.clone())];
                for it in items {
                    let mut nxt = Vec::new();
                    for (p, c) in &cur {
                        nxt.extend(self.match_ends(it, *p, c));
                    }
                    dedupe_ends(&mut nxt);
                    cur = nxt;
                    if cur.is_empty() {
                        break;
                    }
                }
                cur
            }
            Node::Alt(branches) => {
                let mut out = Vec::new();
                for b in branches {
                    out.extend(self.match_ends(b, pos, caps));
                }
                dedupe_ends(&mut out);
                out
            }
            Node::Quant {
                node,
                min,
                max,
                greedy,
            } => self.match_quant(node, *min, *max, *greedy, pos, caps),
            Node::Group { idx, node } => {
                let mut out = Vec::new();
                for (end, mut c) in self.match_ends(node, pos, caps) {
                    c[*idx] = (Some(pos), Some(end));
                    out.push((end, c));
                }
                out
            }
        }
    }

    fn match_quant(
        &mut self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        pos: usize,
        caps: &Caps,
    ) -> Vec<(usize, Caps)> {
        // reps[i] = 恰好匹配 i 次后的 (结束, 捕获) 集合。
        let mut reps: Vec<Vec<(usize, Caps)>> = vec![vec![(pos, caps.clone())]];
        let hard_max = max.unwrap_or(usize::MAX);
        loop {
            let i = reps.len();
            if i > hard_max {
                break;
            }
            let mut cur = Vec::new();
            let mut progressed = false;
            // borrow 分开：先 clone 出 prev，避免与 self 的可变借用冲突。
            let prev = reps[i - 1].clone();
            for (p, c) in &prev {
                for (e, c2) in self.match_ends(node, *p, c) {
                    if e != *p {
                        progressed = true;
                    }
                    cur.push((e, c2));
                }
                if self.steps > MAX_REGEX_STEPS {
                    return Vec::new();
                }
            }
            dedupe_ends(&mut cur);
            if cur.is_empty() {
                break;
            }
            reps.push(cur);
            // 空匹配无法推进更多；无界时用剩余长度兜底。
            if !progressed {
                break;
            }
            if max.is_none() && i >= self.chars.len().saturating_sub(pos) + 1 {
                break;
            }
        }
        let range: Vec<usize> = if greedy {
            (min..reps.len()).rev().collect()
        } else {
            (min..reps.len()).collect()
        };
        for i in range {
            if !reps[i].is_empty() {
                return reps[i].clone();
            }
        }
        Vec::new()
    }
}

/// 解释器持有的正则值：编译产物 + `lastIndex`（`/g` 的 test/exec 用）。
#[derive(Debug)]
pub struct JsRegExp {
    pub compiled: CompiledRegex,
    pub last_index: usize,
}

/// 正则引用（解释器与用户代码共享所有权；`lastIndex` 可变）。
pub type RegExpRef = Rc<RefCell<JsRegExp>>;

impl JsRegExp {
    pub fn new(compiled: CompiledRegex) -> RegExpRef {
        Rc::new(RefCell::new(JsRegExp {
            compiled,
            last_index: 0,
        }))
    }

    /// `/source/flags` 展示形。
    pub fn display(&self) -> String {
        format!("/{}/{}", self.compiled.source, self.compiled.flags.as_string())
    }
}

#[cfg(test)]
mod tests {    use super::*;

    fn t(pattern: &str, text: &str) -> bool {
        CompiledRegex::compile(pattern, "").unwrap().test(text)
    }

    #[test]
    fn basics() {
        assert!(t("ab+c", "xxabbbbc"));
        assert!(!t("ab+c", "xxac"));
        assert!(t("^ab", "abc"));
        assert!(!t("^ab", "xabc"));
        assert!(t("ab$", "xxab"));
        assert!(!t("ab$", "abx"));
        assert!(t("a.c", "abc"));
        assert!(!t("a.c", "ac"));
        assert!(t("a|b", "b"));
        assert!(t("(ab)+", "ababab"));
        assert!(t("a?", "b"));
        assert!(t("\\d+", "abc123"));
        assert!(!t("^\\d+$", "12a"));
        assert!(t("[a-z]+", "hello"));
        assert!(t("[^0-9]+", "abc"));
        assert!(!t("[^0-9]+", "123"));
        assert!(t("\\w+@\\w+", "a@b"));
        assert!(t("a{2,3}", "aa"));
        assert!(t("a{2,3}", "aaa"));
        assert!(!t("a{2,3}", "a"));
        assert!(t("a{2,}", "aaaa"));
    }

    #[test]
    fn flags() {
        let re = CompiledRegex::compile("abc", "i").unwrap();
        assert!(re.test("ABC"));
        let re = CompiledRegex::compile("^b", "m").unwrap();
        assert!(re.test("a\nb"));
        assert!(CompiledRegex::compile("a", "x").is_err());
        assert!(CompiledRegex::compile("a", "gg").is_err());
    }

    #[test]
    fn groups() {
        let re = CompiledRegex::compile("(\\d+)-(\\d+)", "").unwrap();
        let m = re.search("a12-34b", 0).unwrap();
        assert_eq!(m.text, "12-34");
        assert_eq!(m.index, 1);
        assert_eq!(m.groups, vec![Some("12".into()), Some("34".into())]);
        let re = CompiledRegex::compile("(?:ab)(c)", "").unwrap();
        let m = re.search("abc", 0).unwrap();
        assert_eq!(m.groups.len(), 1);
        assert_eq!(m.groups[0], Some("c".into()));
    }

    #[test]
    fn no_redos_hang() {
        // 经典灾难回溯：必须在步数上限内熔断返回 false，不能 hang。
        let re = CompiledRegex::compile("(a+)+$", "").unwrap();
        let text = "a".repeat(30) + "!";
        assert!(!re.test(&text));
        let re = CompiledRegex::compile("(a|aa)+$", "").unwrap();
        assert!(!re.test(&("a".repeat(30) + "!")));
    }

    #[test]
    fn escapes_and_classes() {
        assert!(t("\\.", "a.b"));
        assert!(!t("\\.", "ab"));
        assert!(t("\\bfoo\\b", "a foo b"));
        assert!(!t("\\bfoo\\b", "afoob"));
        assert!(t("[\\d]+", "123"));
        assert!(t("\\x41+", "AAB"));
        // `{` 非法量词按字面量。
        assert!(t("a{1", "a{1"));
    }

    #[test]
    fn unsupported_is_error() {
        assert!(CompiledRegex::compile("(?=a)", "").is_err());
        assert!(CompiledRegex::compile("(a)\\1", "").is_err());
        assert!(CompiledRegex::compile("(?<n>a)", "").is_err());
    }
}
