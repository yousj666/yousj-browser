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
//!   char*  yousj_forms(const Doc* doc); // JSON array of forms (Python form API)
//!   void   yousj_free_str(char* s);
//!
//! With `--features js` (see build-js.sh) one more is available:
//!
//!   char*  yousj_run_js(Doc* doc, const char* js, size_t len);
//!          // runs JS against the live DOM; returns malloc'd JSON:
//!          // {"console": [...], "error": null | "msg"}

mod dom;
#[cfg(feature = "js")]
mod js;
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

/// Serialize all <form> elements as JSON (Python form API).
///
/// Returns a malloc'd JSON array (free with `yousj_free_str`):
/// `[{"action": "...", "method": "get", "fields": [
///    {"tag": "input", "name": "q", "type": "text", "value": "", "checked": false},
///    {"tag": "select", "name": "city", "options":
///      [{"value": "fz", "selected": true}, ...]},
///    {"tag": "textarea", "name": "msg", "value": "hi"},
///    {"tag": "button", "name": "go", "type": "submit", "value": "Go"}]}]`
#[no_mangle]
pub extern "C" fn yousj_forms(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };

    fn jstr(s: &str, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32))
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }

    fn text_of(d: &Doc, idx: usize, out: &mut String) {
        let n = &d.arena[idx];
        if n.tag.is_none() {
            out.push_str(&n.text);
        }
        for &c in &n.children {
            text_of(d, c, out);
        }
    }

    fn field_json(d: &Doc, idx: usize, out: &mut String) {
        let n = &d.arena[idx];
        let tag = n.tag.as_deref().unwrap_or("");
        out.push_str("{\"tag\":");
        jstr(tag, out);
        out.push_str(",\"name\":");
        jstr(n.attr("name").unwrap_or(""), out);
        match tag {
            "input" => {
                let typ = n.attr("type").unwrap_or("text").to_ascii_lowercase();
                out.push_str(",\"type\":");
                jstr(&typ, out);
                out.push_str(",\"value\":");
                jstr(n.attr("value").unwrap_or(""), out);
                out.push_str(",\"checked\":");
                out.push_str(if n.attr("checked").is_some() {
                    "true"
                } else {
                    "false"
                });
            }
            "select" => {
                out.push_str(",\"options\":[");
                let mut first = true;
                for &c in &n.children {
                    // also look one level deeper for optgroup > option
                    let mut opts = Vec::new();
                    let cn = &d.arena[c];
                    if cn.tag.as_deref() == Some("option") {
                        opts.push(c);
                    } else if cn.tag.as_deref() == Some("optgroup") {
                        for &g in &cn.children {
                            if d.arena[g].tag.as_deref() == Some("option") {
                                opts.push(g);
                            }
                        }
                    }
                    for o in opts {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        let on = &d.arena[o];
                        let mut t = String::new();
                        text_of(d, o, &mut t);
                        let t = t.trim();
                        let value = on.attr("value").unwrap_or(t);
                        out.push_str("{\"value\":");
                        jstr(value, out);
                        out.push_str(",\"selected\":");
                        out.push_str(if on.attr("selected").is_some() {
                            "true"
                        } else {
                            "false"
                        });
                        out.push('}');
                    }
                }
                out.push(']');
            }
            "textarea" => {
                let mut t = String::new();
                text_of(d, idx, &mut t);
                out.push_str(",\"value\":");
                jstr(&t, out);
            }
            "button" => {
                out.push_str(",\"type\":");
                jstr(n.attr("type").unwrap_or("submit"), out);
                out.push_str(",\"value\":");
                jstr(n.attr("value").unwrap_or(""), out);
            }
            _ => {}
        }
        out.push('}');
    }

    fn walk_fields(d: &Doc, idx: usize, out: &mut String, first: &mut bool) {
        {
            let n = &d.arena[idx];
            match n.tag.as_deref() {
                Some("input") | Some("select") | Some("textarea")
                | Some("button") => {
                    if !*first {
                        out.push(',');
                    }
                    *first = false;
                    field_json(d, idx, out);
                }
                _ => {}
            }
        }
        let children = d.arena[idx].children.clone();
        for c in children {
            walk_fields(d, c, out, first);
        }
    }

    fn walk_forms(d: &Doc, idx: usize, out: &mut String, first: &mut bool) {
        let is_form;
        {
            let n = &d.arena[idx];
            is_form = n.tag.as_deref() == Some("form");
        }
        if is_form {
            let n = &d.arena[idx];
            if !*first {
                out.push(',');
            }
            *first = false;
            out.push_str("{\"action\":");
            jstr(n.attr("action").unwrap_or(""), out);
            out.push_str(",\"method\":");
            jstr(&n.attr("method").unwrap_or("get").to_ascii_lowercase(), out);
            out.push_str(",\"fields\":[");
            let mut ffirst = true;
            walk_fields(d, idx, out, &mut ffirst);
            out.push_str("]}");
        }
        let children = d.arena[idx].children.clone();
        for c in children {
            walk_forms(d, c, out, first);
        }
    }

    let mut out = String::from("[");
    let mut first = true;
    walk_forms(d, d.root, &mut out, &mut first);
    out.push(']');
    to_c_string(out)
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

/// Run JavaScript against the live DOM (only with `--features js`).
///
/// Takes `*mut Doc` because scripts may mutate the DOM. Returns a malloc'd
/// JSON string (free with `yousj_free_str`):
/// `{"console": ["..."], "error": null}` or `"error": "<message>"`.
#[cfg(feature = "js")]
#[no_mangle]
pub extern "C" fn yousj_run_js(
    doc: *mut Doc,
    js: *const c_char,
    len: usize,
) -> *mut c_char {
    if doc.is_null() || js.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(js as *const u8, len) };
    let src = String::from_utf8_lossy(bytes);
    let d = unsafe { &mut *doc };
    let (console, error) = js::run_on_doc(d, &src);

    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    }

    let mut out = String::from("{\"console\": [");
    for (i, line) in console.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(&esc(line));
        out.push('"');
    }
    out.push_str("], \"error\": ");
    match error {
        None => out.push_str("null"),
        Some(e) => {
            out.push('"');
            out.push_str(&esc(&e));
            out.push('"');
        }
    }
    out.push('}');
    to_c_string(out)
}

/// Run JavaScript with `fetch()` wired to a host callback (only with
/// `--features js`).
///
/// `fetch_cb`: `*mut c_char fetch_cb(*const c_char url)` — receives the URL,
/// returns JSON `{"status":200,"body":"..."}` or `{"error":"..."}` (copied
/// immediately; caller retains ownership). Python wires this to `net.get`.
/// Output format is identical to `yousj_run_js`.
#[cfg(feature = "js")]
#[no_mangle]
pub extern "C" fn yousj_run_js_with_fetch(
    doc: *mut Doc,
    js: *const c_char,
    len: usize,
    fetch_cb: js::FetchCallback,
) -> *mut c_char {
    if doc.is_null() || js.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(js as *const u8, len) };
    let src = String::from_utf8_lossy(bytes);
    let d = unsafe { &mut *doc };
    let (console, error) = js::run_on_doc_with_fetch(d, &src, fetch_cb);
    to_c_string(console_error_json(&console, &error))
}

/// Shared JSON encoding for `yousj_run_js` / `yousj_run_js_with_fetch`.
#[cfg(feature = "js")]
fn console_error_json(console: &[String], error: &Option<String>) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    }

    let mut out = String::from("{\"console\": [");
    for (i, line) in console.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(&esc(line));
        out.push('"');
    }
    out.push_str("], \"error\": ");
    match error {
        None => out.push_str("null"),
        Some(e) => {
            out.push('"');
            out.push_str(&esc(e));
            out.push('"');
        }
    }
    out.push('}');
    out
}

/// Run JavaScript with the phase-8 debugger attached.
///
/// `bp` / `bp_len`: JSON array of 1-based breakpoint line numbers,
/// e.g. `"[1, 5]"` (may be null → no breakpoints).
/// Returns malloc'd JSON:
/// `{"console": [...], "error": null | "msg", "debug_hits": [...]}`.
#[cfg(feature = "js")]
#[no_mangle]
pub extern "C" fn yousj_run_js_debug(
    doc: *mut Doc,
    js: *const c_char,
    len: usize,
    bp: *const c_char,
    bp_len: usize,
) -> *mut c_char {
    if doc.is_null() || js.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(js as *const u8, len) };
    let src = String::from_utf8_lossy(bytes);
    let breakpoints = if bp.is_null() {
        Vec::new()
    } else {
        let b = unsafe { std::slice::from_raw_parts(bp as *const u8, bp_len) };
        parse_breakpoints(&String::from_utf8_lossy(b))
    };
    let d = unsafe { &mut *doc };
    let (console, error, pauses) = js::run_on_doc_with_debug(d, &src, &breakpoints);
    let mut out = console_error_json(&console, &error);
    out.pop(); // 去掉末尾 '}'，追加 debug_hits
    out.push_str(", \"debug_hits\": ");
    out.push_str(&js::debug_pauses_json(&pauses));
    out.push('}');
    to_c_string(out)
}

/// phase 8：极简解析 `"[1, 5]"` 形式的断点行号数组（非法输入 → 空列表）。
#[cfg(feature = "js")]
fn parse_breakpoints(s: &str) -> Vec<usize> {
    s.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|t| t.trim().parse::<usize>().ok())
        .collect()
}
