//! Yousj browser engine — HTML tokenizer (hand-written, zero dependencies).
//!
//! Low-level helpers (ASCII case-insensitive compare, void-element lookup,
//! character-entity decoding) are provided by the C layer (c/yousj_str.c)
//! and called here over FFI.

use std::os::raw::{c_char, c_int};

extern "C" {
    fn yousj_tag_eq(a: *const c_char, a_len: usize, b: *const c_char) -> c_int;
    fn yousj_is_void_tag(name: *const c_char, len: usize) -> c_int;
    fn yousj_decode_entity(
        name: *const c_char,
        name_len: usize,
        out: *mut c_char,
        out_cap: usize,
    ) -> usize;
}

/// Case-insensitive tag-name compare, via the C layer.
pub(crate) fn c_tag_eq(a: &[u8], b: &str) -> bool {
    let cstr = std::ffi::CString::new(b).unwrap();
    unsafe { yousj_tag_eq(a.as_ptr() as *const c_char, a.len(), cstr.as_ptr()) != 0 }
}

pub(crate) fn c_tag_eq_str(a: &str, b: &str) -> bool {
    c_tag_eq(a.as_bytes(), b)
}

/// Void-element check, via the C layer.
pub(crate) fn c_is_void(name: &[u8]) -> bool {
    unsafe { yousj_is_void_tag(name.as_ptr() as *const c_char, name.len()) != 0 }
}

/// Decode one entity body (without '&'/';'), appending UTF-8 to `out`.
/// Returns false when the entity is unknown.
fn c_decode_entity(name: &[u8], out: &mut Vec<u8>) -> bool {
    let mut buf = [0 as c_char; 8];
    let n = unsafe {
        yousj_decode_entity(
            name.as_ptr() as *const c_char,
            name.len(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    if n == 0 {
        return false;
    }
    out.extend_from_slice(unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, n)
    });
    true
}

/// Decode character entities (e.g. `&amp;`) in a raw byte span.
/// Used for both text content and attribute values.
fn decode_entities(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'&' {
            let ns = i + 1;
            let mut j = ns;
            while j < input.len()
                && input[j] != b';'
                && j - ns < 12
                && (input[j].is_ascii_alphanumeric() || input[j] == b'#')
            {
                j += 1;
            }
            if j < input.len() && input[j] == b';' {
                if !c_decode_entity(&input[ns..j], &mut out) {
                    out.push(b'&');
                    out.extend_from_slice(&input[ns..j]);
                    out.push(b';');
                }
                i = j + 1; // consume ';'
            } else {
                // Not an entity: emit the '&' literally.
                out.push(b'&');
                i += 1;
            }
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug)]
pub enum Token {
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Text(String),
    #[allow(dead_code)]
    Comment(String),
    Doctype,
    Eof,
}

pub struct Tokenizer<'a> {
    input: &'a [u8],
    pos: usize,
    pending: Option<Token>,
}

impl<'a> Tokenizer<'a> {
    pub fn new(html: &'a str) -> Self {
        Tokenizer {
            input: html.as_bytes(),
            pos: 0,
            pending: None,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> u8 {
        if self.eof() {
            0
        } else {
            self.input[self.pos]
        }
    }

    fn starts_with(&self, s: &[u8]) -> bool {
        self.input.len() - self.pos >= s.len()
            && &self.input[self.pos..self.pos + s.len()] == s
    }

    fn skip_ws(&mut self) {
        while !self.eof() && matches!(self.peek(), b' ' | b'\t' | b'\n' | b'\r' | 0x0C) {
            self.pos += 1;
        }
    }

    fn read_name(&mut self) -> Vec<u8> {
        let start = self.pos;
        while !self.eof() {
            let c = self.peek();
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.') {
                self.pos += 1;
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_vec()
    }

    pub fn next_token(&mut self) -> Token {
        if let Some(t) = self.pending.take() {
            return t;
        }
        if self.eof() {
            return Token::Eof;
        }
        if self.peek() == b'<' {
            if self.starts_with(b"<!--") {
                return self.read_comment();
            }
            if self.starts_with(b"</") {
                self.pos += 2;
                self.skip_ws();
                let name = self.read_name();
                while !self.eof() && self.peek() != b'>' {
                    self.pos += 1;
                }
                if !self.eof() {
                    self.pos += 1;
                }
                return Token::EndTag {
                    name: String::from_utf8_lossy(&name).into_owned(),
                };
            }
            if self.starts_with(b"<!") {
                while !self.eof() && self.peek() != b'>' {
                    self.pos += 1;
                }
                if !self.eof() {
                    self.pos += 1;
                }
                return Token::Doctype;
            }
            if self.starts_with(b"<?") {
                while !self.eof() && self.peek() != b'>' {
                    self.pos += 1;
                }
                if !self.eof() {
                    self.pos += 1;
                }
                return Token::Comment(String::new());
            }
            return self.read_start_tag();
        }
        self.read_text()
    }

    fn read_comment(&mut self) -> Token {
        self.pos += 4; // consume "<!--"
        let start = self.pos;
        while !self.eof() && !self.starts_with(b"-->") {
            self.pos += 1;
        }
        let s = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        self.pos = (self.pos + 3).min(self.input.len());
        Token::Comment(s)
    }

    fn read_start_tag(&mut self) -> Token {
        self.pos += 1; // consume '<'
        self.skip_ws();
        let name_bytes = self.read_name();
        let name = String::from_utf8_lossy(&name_bytes).to_ascii_lowercase();
        let mut attrs = Vec::new();
        let mut self_closing = false;

        loop {
            self.skip_ws();
            if self.eof() {
                break;
            }
            let c = self.peek();
            if c == b'>' {
                self.pos += 1;
                break;
            }
            if c == b'/' {
                self.pos += 1;
                self.skip_ws();
                if !self.eof() && self.peek() == b'>' {
                    self.pos += 1;
                }
                self_closing = true;
                break;
            }
            let aname = self.read_name();
            if aname.is_empty() {
                self.pos += 1; // guarantee progress on junk
                continue;
            }
            let aname = String::from_utf8_lossy(&aname).into_owned();
            self.skip_ws();
            let mut val = String::new();
            if !self.eof() && self.peek() == b'=' {
                self.pos += 1;
                self.skip_ws();
                if !self.eof() {
                    let q = self.peek();
                    if q == b'"' || q == b'\'' {
                        self.pos += 1;
                        let start = self.pos;
                        while !self.eof() && self.peek() != q {
                            self.pos += 1;
                        }
                        val = decode_entities(&self.input[start..self.pos]);
                        if !self.eof() {
                            self.pos += 1;
                        }
                    } else {
                        let start = self.pos;
                        // NOTE: '/' is legal inside unquoted values (e.g.
                        // src=/s.js); only whitespace and '>' terminate.
                        while !self.eof()
                            && !matches!(
                                self.peek(),
                                b' ' | b'\t' | b'\n' | b'\r' | b'>'
                            )
                        {
                            self.pos += 1;
                        }
                        val = decode_entities(&self.input[start..self.pos]);
                    }
                }
            }
            attrs.push((aname, val));
        }

        // Raw-text elements: script/style content is not tokenized as markup.
        if c_tag_eq(name.as_bytes(), "script") || c_tag_eq(name.as_bytes(), "style") {
            let raw = self.read_raw_text(&name);
            self.pending = Some(Token::Text(raw));
        }

        Token::StartTag {
            name,
            attrs,
            self_closing,
        }
    }

    /// Consume raw text until the matching end tag; the end tag itself
    /// is consumed too.
    fn read_raw_text(&mut self, tag: &str) -> String {
        let start = self.pos;
        loop {
            if self.eof() {
                break;
            }
            if self.peek() == b'<' && self.starts_with(b"</") {
                let mut i = self.pos + 2;
                while i < self.input.len() && self.input[i].is_ascii_whitespace() {
                    i += 1;
                }
                let ns = i;
                while i < self.input.len() && self.input[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                if c_tag_eq(&self.input[ns..i], tag) {
                    let text =
                        String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
                    self.pos = i;
                    while !self.eof() && self.peek() != b'>' {
                        self.pos += 1;
                    }
                    if !self.eof() {
                        self.pos += 1;
                    }
                    return text;
                }
            }
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.input[start..self.pos]).into_owned()
    }

    fn read_text(&mut self) -> Token {
        let start = self.pos;
        while !self.eof() && self.peek() != b'<' {
            self.pos += 1;
        }
        Token::Text(decode_entities(&self.input[start..self.pos]))
    }
}
