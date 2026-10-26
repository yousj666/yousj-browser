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
//!   char*  yousj_interactive(const Doc* doc); // JSON array of interactive
//!          // elements (Python AI-browsing API)
//!   char*  yousj_layout(const Doc* doc, int viewport_width); // JSON
//!          // layout boxes (minimal box model, see layout.rs)
//!   char*  yousj_scripts(const Doc* doc); // JSON array of <script>
//!          // bodies (for button activation: page scripts + click must
//!          // run in a single run_js call, because listeners do not
//!          // persist across calls)
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
mod layout;
mod parser;
mod tokenizer;

use std::ffi::CString;
use std::os::raw::c_char;

pub struct Doc {
    pub(crate) arena: Vec<dom::Node>,
    pub(crate) root: usize,
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

/// Append `s` as a JSON string literal (with quotes) to `out`.
/// Same escaping rules as the nested helper inside `yousj_forms`;
/// kept at file level so other FFI exports can share it.
fn json_escape(s: &str, out: &mut String) {
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

/// True when the subtree of `idx` contains a form control (input /
/// select / textarea / button). Used for label "wraps_control": a label
/// that wraps a control activates it on click, like a `for` label.
fn subtree_has_control(d: &Doc, idx: usize) -> bool {
    for &c in &d.arena[idx].children {
        let cn = &d.arena[c];
        if let Some(t) = cn.tag.as_deref() {
            if matches!(t, "input" | "select" | "textarea" | "button") {
                return true;
            }
        }
        if subtree_has_control(d, c) {
            return true;
        }
    }
    false
}

/// Serialize all interactive elements as JSON (Python AI-browsing API).
///
/// Returns a malloc'd JSON array (free with `yousj_free_str`), in
/// document order (pre-order DFS). Subtrees of head/script/style/
/// noscript/template are skipped (not rendered, not clickable).
///
/// Each item:
/// `{"tag","kind","type","text","href","name","html_id","value","disabled",
///   "onclick","role","tag_index","form_index","checked","options",
///   "label_for","wraps_control"}`
/// - `tag`: lowercased element tag.
/// - `kind`: "link" (a[href]) | "button" | "submit" | "input" |
///   "toggle" (checkbox/radio) | "select" | "textarea" | "label" |
///   "clickable" (onclick / interactive role on another tag).
/// - `type`: input/button type (lowercased; input defaults to "text",
///   button defaults to "submit"); "" for other tags.
/// - `text`: visible text, whitespace-collapsed, truncated to 120 chars.
/// - `tag_index`: index among same-tag elements in document order, so
///   Python can reach the element from JS as
///   `document.querySelectorAll(tag)[tag_index]` (or by id when present).
///   Counts ALL elements of that tag, not just interactive ones.
/// - `form_index`: index of the enclosing <form> in document order
///   (same order as `yousj_forms`), or null when outside any form.
/// - `checked`: bool for checkbox/radio, else null.
/// - `options`: [{"value","selected"}] for select, else null.
/// - `label_for`: the `for` attribute of a label, else "".
/// - `wraps_control`: true when a label wraps a form control in its
///   subtree (activates it on click, like a `for` label).
/// - `disabled`/`onclick`: presence of the attribute (no value needed).
///
/// Notes: this reports the live DOM (including JS mutations, since the
/// arena is shared). Pixel coordinates are NOT here — they come from
/// `yousj_layout`, joined by `(tag, tag_index)` (both walkers count
/// every element per tag in document order, so indices align).
#[no_mangle]
pub extern "C" fn yousj_interactive(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };

    fn is_skipped(tag: &str) -> bool {
        matches!(tag, "head" | "script" | "style" | "noscript" | "template")
    }

    fn kind_of(tag: &str, typ: &str, has_onclick: bool, role: &str) -> Option<&'static str> {
        match tag {
            "a" => Some("link"),
            "button" => Some(if typ == "submit" { "submit" } else { "button" }),
            "input" => Some(match typ {
                "submit" => "submit",
                "button" | "reset" => "button",
                "checkbox" | "radio" => "toggle",
                _ => "input",
            }),
            "select" => Some("select"),
            "textarea" => Some("textarea"),
            "label" => Some("label"),
            _ => {
                if has_onclick {
                    Some("clickable")
                } else if matches!(
                    role,
                    "button" | "link" | "checkbox" | "radio" | "switch"
                        | "tab" | "menuitem" | "menuitemcheckbox"
                        | "menuitemradio" | "option" | "treeitem"
                ) {
                    Some("clickable")
                } else {
                    None
                }
            }
        }
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

    fn push_option(
        d: &Doc,
        o: usize,
        out: &mut String,
        first: &mut bool,
    ) {
        let on = &d.arena[o];
        let mut t = String::new();
        text_of(d, o, &mut t);
        let t = t.trim();
        let value = on.attr("value").unwrap_or(t);
        if !*first {
            out.push(',');
        }
        *first = false;
        out.push_str("{\"value\":");
        json_escape(value, out);
        out.push_str(",\"selected\":");
        out.push_str(if on.attr("selected").is_some() {
            "true"
        } else {
            "false"
        });
        out.push('}');
    }

    fn walk(
        d: &Doc,
        idx: usize,
        counts: &mut std::collections::HashMap<String, usize>,
        form_stack: &mut Vec<usize>,
        form_count: &mut usize,
        out: &mut String,
        first: &mut bool,
    ) {
        let (tag_opt, children) = {
            let n = &d.arena[idx];
            (n.tag.clone(), n.children.clone())
        };
        let tag = match tag_opt {
            None => {
                // Document / text node: descend (text nodes have no
                // element children, so this is a no-op for them).
                for c in children {
                    walk(d, c, counts, form_stack, form_count, out, first);
                }
                return;
            }
            Some(t) => t,
        };
        if is_skipped(&tag) {
            return;
        }
        // Count ALL elements of this tag (querySelectorAll indexing
        // counts non-interactive ones too).
        let tag_index = *counts.entry(tag.clone()).or_insert(0);
        counts.insert(tag.clone(), tag_index + 1);

        let mut pushed_form = false;
        if tag == "form" {
            form_stack.push(*form_count);
            *form_count += 1;
            pushed_form = true;
        }
        let form_index = form_stack.last().copied();

        let n = &d.arena[idx];
        let typ = if tag == "button" {
            n.attr("type").unwrap_or("submit").to_ascii_lowercase()
        } else if tag == "input" {
            n.attr("type").unwrap_or("text").to_ascii_lowercase()
        } else {
            n.attr("type").unwrap_or("").to_ascii_lowercase()
        };
        let has_onclick = n.attr("onclick").is_some();
        let role = n.attr("role").unwrap_or("").to_ascii_lowercase();
        let interactive = if tag == "a" {
            n.attr("href").is_some()
        } else if tag == "input" && typ == "hidden" {
            false
        } else {
            kind_of(&tag, &typ, has_onclick, &role).is_some()
        };
        if interactive {
            let kind = kind_of(&tag, &typ, has_onclick, &role).unwrap();
            if !*first {
                out.push(',');
            }
            *first = false;
            let mut t = String::new();
            text_of(d, idx, &mut t);
            let text: String = collapse_ws(&t).chars().take(120).collect();
            out.push_str("{\"tag\":");
            json_escape(&tag, out);
            out.push_str(",\"kind\":");
            json_escape(kind, out);
            out.push_str(",\"type\":");
            json_escape(&typ, out);
            out.push_str(",\"text\":");
            json_escape(&text, out);
            out.push_str(",\"href\":");
            json_escape(n.attr("href").unwrap_or(""), out);
            out.push_str(",\"name\":");
            json_escape(n.attr("name").unwrap_or(""), out);
            out.push_str(",\"html_id\":");
            json_escape(n.attr("id").unwrap_or(""), out);
            out.push_str(",\"value\":");
            json_escape(n.attr("value").unwrap_or(""), out);
            out.push_str(",\"disabled\":");
            out.push_str(if n.attr("disabled").is_some() {
                "true"
            } else {
                "false"
            });
            out.push_str(",\"onclick\":");
            out.push_str(if has_onclick { "true" } else { "false" });
            out.push_str(",\"role\":");
            json_escape(n.attr("role").unwrap_or(""), out);
            out.push_str(",\"tag_index\":");
            out.push_str(&tag_index.to_string());
            out.push_str(",\"form_index\":");
            match form_index {
                Some(f) => out.push_str(&f.to_string()),
                None => out.push_str("null"),
            }
            if tag == "input" && (typ == "checkbox" || typ == "radio") {
                out.push_str(",\"checked\":");
                out.push_str(if n.attr("checked").is_some() {
                    "true"
                } else {
                    "false"
                });
            } else {
                out.push_str(",\"checked\":null");
            }
            if tag == "select" {
                out.push_str(",\"options\":[");
                let mut ofirst = true;
                for &c in &children {
                    let cn = &d.arena[c];
                    if cn.tag.as_deref() == Some("option") {
                        push_option(d, c, out, &mut ofirst);
                    } else if cn.tag.as_deref() == Some("optgroup") {
                        for &g in &cn.children {
                            if d.arena[g].tag.as_deref() == Some("option") {
                                push_option(d, g, out, &mut ofirst);
                            }
                        }
                    }
                }
                out.push(']');
            } else {
                out.push_str(",\"options\":null");
            }
            out.push_str(",\"label_for\":");
            json_escape(n.attr("for").unwrap_or(""), out);
            out.push_str(",\"wraps_control\":");
            out.push_str(if tag == "label" && subtree_has_control(d, idx) {
                "true"
            } else {
                "false"
            });
            out.push('}');
        }
        for c in children {
            walk(d, c, counts, form_stack, form_count, out, first);
        }
        if pushed_form {
            form_stack.pop();
        }
    }

    let mut out = String::from("[");
    let mut first = true;
    let mut counts = std::collections::HashMap::new();
    let mut form_stack = Vec::new();
    let mut form_count = 0usize;
    walk(
        d,
        d.root,
        &mut counts,
        &mut form_stack,
        &mut form_count,
        &mut out,
        &mut first,
    );
    out.push(']');
    to_c_string(out)
}

/// Minimal layout / box model (see layout.rs).
///
/// Returns a malloc'd JSON object (free with `yousj_free_str`):
/// `{"viewport_width":W,"truncated":bool,"boxes":[...]}` where each box
/// is `{"tag","text","x","y","width","height","tag_index","html_id",
/// "href","leaf"}`. Coordinates are CSS pixels relative to the document
/// origin (top-left), estimated (no real font metrics): CJK 1.0em,
/// other chars 0.5em, line height 1.2em. `viewport_width` <= 0 falls
/// back to 1280. `tag_index` aligns with `yousj_interactive` (both
/// count every element per tag in document order), so Python joins
/// boxes to interactive-list entries by `(tag, tag_index)`. `leaf` is
/// true when no descendant element got its own box (screenshot labels
/// are drawn only for leaves).
#[no_mangle]
pub extern "C" fn yousj_layout(doc: *const Doc, viewport_width: i32) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };
    to_c_string(layout::layout_json(d, viewport_width))
}

/// Serialize <script> elements as JSON (for button activation).
///
/// Returns a malloc'd JSON array (free with `yousj_free_str`):
/// `[{"src": "...", "code": "..."}]` in document order. External
/// scripts (``src``) have empty ``code`` (not fetched here); inline
/// scripts carry their raw body text (newlines preserved, so the code
/// stays runnable).
///
/// Why this exists: yousj-js event listeners live only for one
/// ``run_js`` call — they do not persist across calls (DOM mutations
/// do). A button click dispatched in a separate call can therefore
/// never fire a handler registered by an earlier call. Button
/// activation must run the page's inline scripts and the click
/// expression in a SINGLE ``run_js`` call; this export supplies the
/// scripts for that combined call. Scripts inside <template> are
/// skipped (inert by spec); scripts in <head> are included.
#[no_mangle]
pub extern "C" fn yousj_scripts(doc: *const Doc) -> *mut c_char {
    if doc.is_null() {
        return std::ptr::null_mut();
    }
    let d = unsafe { &*doc };

    fn text_of(d: &Doc, idx: usize, out: &mut String) {
        let n = &d.arena[idx];
        if n.tag.is_none() {
            out.push_str(&n.text);
        }
        for &c in &n.children {
            text_of(d, c, out);
        }
    }

    fn walk(d: &Doc, idx: usize, out: &mut String, first: &mut bool) {
        let (tag_opt, children) = {
            let n = &d.arena[idx];
            (n.tag.clone(), n.children.clone())
        };
        let tag = match tag_opt {
            None => {
                for c in children {
                    walk(d, c, out, first);
                }
                return;
            }
            Some(t) => t,
        };
        if tag == "template" {
            return;
        }
        if tag == "script" {
            if !*first {
                out.push(',');
            }
            *first = false;
            let n = &d.arena[idx];
            let mut code = String::new();
            text_of(d, idx, &mut code);
            out.push_str("{\"src\":");
            json_escape(n.attr("src").unwrap_or(""), out);
            out.push_str(",\"code\":");
            json_escape(&code, out);
            out.push('}');
        }
        for c in children {
            walk(d, c, out, first);
        }
    }

    let mut out = String::from("[");
    let mut first = true;
    walk(d, d.root, &mut out, &mut first);
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
