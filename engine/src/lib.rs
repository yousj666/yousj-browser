//! Yousj browser engine — C ABI.
//!
//! The engine is built as a cdylib. Python (or any other host) drives it
//! through these functions:
//!
//!   Doc*   yousj_parse(const char* html, size_t len);
//!   void   yousj_free_doc(Doc* doc);
//!   char*  yousj_title(const Doc* doc);   // malloc'd, free with yousj_free_str
//!   char*  yousj_text(const Doc* doc);
//!   char*  yousj_links(const Doc* doc);   // newline-separated hrefs
//!   char*  yousj_anchors(const Doc* doc); // lines of "href\tanchor text"
//!   char*  yousj_dom_tree(const Doc* doc); // indented DOM tree (Elements panel)
//!   void   yousj_free_str(char* s);

mod dom;
mod parser;
mod tokenizer;

use std::ffi::CString;
use std::os::raw::c_char;

pub struct Doc {
    arena: Vec<dom::Node>,
    root: usize,
}

impl Doc {
    fn walk_text(&self, idx: usize, out: &mut String) {
        let n = &self.arena[idx];
        match n.tag.as_deref() {
            Some("script") | Some("style") | Some("noscript") => return,
            Some(_) => {}
            None => {
                out.push_str(&n.text);
                out.push(' ');
                return;
            }
        }
        for &c in &n.children {
            self.walk_text(c, out);
        }
    }

    fn node_text(&self, idx: usize) -> String {
        let mut s = String::new();
        self.walk_text(idx, &mut s);
        s
    }

    fn find_first(&self, idx: usize, tag: &str) -> Option<usize> {
        let n = &self.arena[idx];
        if n.tag.as_deref() == Some(tag) {
            return Some(idx);
        }
        for &c in &n.children {
            if let Some(f) = self.find_first(c, tag) {
                return Some(f);
            }
        }
        None
    }

    /// Dump an indented DOM tree for the Elements panel. `budget` caps the
    /// node count so giant pages don't explode the output.
    fn dump_tree(&self, idx: usize, depth: usize, out: &mut String, budget: &mut usize) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let n = &self.arena[idx];
        let indent = "  ".repeat(depth.min(24));
        match n.tag.as_deref() {
            None => {
                let t = collapse_ws(&n.text);
                if !t.is_empty() {
                    let short: String = t.chars().take(60).collect();
                    out.push_str(&format!("{}#text \"{}\"\n", indent, short));
                }
            }
            Some(tag) => {
                let mut line = format!("{}{}", indent, tag);
                for key in ["id", "class", "href", "src", "name", "type",
                            "lang", "alt", "title"] {
                    if let Some(v) = n.attr(key) {
                        let short: String = v.chars().take(48).collect();
                        line.push_str(&format!(" {}=\"{}\"", key, short));
                    }
                }
                out.push_str(&line);
                out.push('\n');
                for &c in &n.children {
                    self.dump_tree(c, depth + 1, out, budget);
                }
            }
        }
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn to_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn yousj_parse(html: *const c_char, len: usize) -> *mut Doc {
    if html.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(html as *const u8, len) };
    let s = String::from_utf8_lossy(bytes);
    let (arena, root) = parser::parse(&s);
    Box::into_raw(Box::new(Doc { arena, root }))
}

#[no_mangle]
pub extern "C" fn yousj_free_doc(p: *mut Doc) {
    if !p.is_null() {
        unsafe {
            let _ = Box::from_raw(p);
        }
    }
}

#[no_mangle]
pub extern "C" fn yousj_free_str(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            let _ = CString::from_raw(p);
        }
    }
}

#[no_mangle]
pub extern "C" fn yousj_title(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };
    let t = d
        .find_first(d.root, "title")
        .map(|i| collapse_ws(&d.node_text(i)))
        .unwrap_or_default();
    to_c_string(t)
}

#[no_mangle]
pub extern "C" fn yousj_text(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };
    let start = d.find_first(d.root, "body").unwrap_or(d.root);
    let mut s = String::new();
    d.walk_text(start, &mut s);
    to_c_string(collapse_ws(&s))
}

#[no_mangle]
pub extern "C" fn yousj_links(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };
    fn rec(d: &Doc, idx: usize, out: &mut Vec<String>) {
        let n = &d.arena[idx];
        if n.tag.as_deref() == Some("a") {
            if let Some(h) = n.attr("href") {
                out.push(h.to_string());
            }
        }
        for &c in &n.children {
            rec(d, c, out);
        }
    }
    let mut out = Vec::new();
    rec(d, d.root, &mut out);
    to_c_string(out.join("\n"))
}

/// One line per anchor: "href\tanchor text". Used by search result parsing.
#[no_mangle]
pub extern "C" fn yousj_anchors(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };
    fn rec(d: &Doc, idx: usize, out: &mut Vec<String>) {
        let n = &d.arena[idx];
        if n.tag.as_deref() == Some("a") {
            if let Some(h) = n.attr("href") {
                let t = collapse_ws(&d.node_text(idx));
                let short: String = t.chars().take(120).collect();
                out.push(format!("{}\t{}", h, short));
            }
        }
        for &c in &n.children {
            rec(d, c, out);
        }
    }
    let mut out = Vec::new();
    rec(d, d.root, &mut out);
    to_c_string(out.join("\n"))
}

/// Indented DOM tree text for the Elements panel (F12).
#[no_mangle]
pub extern "C" fn yousj_dom_tree(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };
    let mut s = String::new();
    let mut budget = 3000usize;
    d.dump_tree(d.root, 0, &mut s, &mut budget);
    if budget == 0 {
        s.push_str("... (truncated: DOM too large)\n");
    }
    to_c_string(s)
}
