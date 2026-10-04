//! yousj-js integration — feature-gated DOM wiring (phase 5).
//!
//! Whole file is `#![cfg(feature = "js")]`: public builds (no feature, no
//! js-engine/ directory) never compile this.
//!
//! Build with the helper (NOT plain `cargo build --features js`):
//!
//!   ./build-js.sh
//!
//! which compiles ../js-engine to an rlib and passes
//! `--extern yousj_js=<...>/libyousj_js.rlib` via RUSTFLAGS. Without that
//! flag, `use yousj_js::...` below fails with "unresolved import" — that
//! error means you skipped the script.
//!
//! Design: `JsDoc` wraps `&mut Doc` in a `RefCell` (the `DomHost` trait
//! takes `&self`) and implements `yousj_js::dom::DomHost` with the arena
//! index as the node id (`u64`). The interpreter mutates the live DOM
//! through it, so JS like `document.getElementById('t').textContent = 'hi'`
//! is immediately visible to the HTML engine (and to Python via a fresh
//! `dom_tree()` / `text()` call).

#![cfg(feature = "js")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::rc::Rc;

use yousj_js::dom::DomHost;
use yousj_js::fetch::{FetchHost, FetchResponse};
use yousj_js::{selector, BreakpointHost, DebugPause, FlowError, Interpreter};

use crate::dom::Node;
use crate::Doc;

/// `DomHost` over the live HTML arena. `RefCell` gives the `&self` trait
/// methods interior mutability over the borrowed `Doc`.
pub struct JsDoc<'a> {
    doc: RefCell<&'a mut Doc>,
    /// 事件监听 token 存储（token → 宿主分配；回调存在解释器侧）。
    listeners: RefCell<HashMap<(u64, String), Vec<u64>>>,
    next_listener: Cell<u64>,
}

impl<'a> JsDoc<'a> {
    pub fn new(doc: &'a mut Doc) -> Self {
        JsDoc {
            doc: RefCell::new(doc),
            listeners: RefCell::new(HashMap::new()),
            next_listener: Cell::new(0),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Doc) -> R) -> R {
        let mut b = self.doc.borrow_mut();
        f(&mut **b)
    }

    /// Bounds-checked arena access; invalid ids behave like missing nodes.
    fn with_node<R>(&self, id: u64, default: R, f: impl FnOnce(&Node) -> R) -> R {
        self.with(|d| {
            d.arena
                .get(id as usize)
                .map(f)
                .unwrap_or(default)
        })
    }

    fn walk_ids(&self, out: &mut Vec<u64>) {
        self.with(|d| {
            fn rec(arena: &[Node], idx: usize, out: &mut Vec<u64>) {
                out.push(idx as u64);
                for &c in &arena[idx].children {
                    rec(arena, c, out);
                }
            }
            rec(&d.arena, d.root, out);
        });
    }

    /// 把 child 从它当前所在的父节点摘下（append/insert 前的移动语义）。
    /// arena 只追加不压缩，下标天然稳定，无需 tombstone。
    fn detach(&self, child: usize) {
        self.with(|d| {
            for n in d.arena.iter_mut() {
                n.children.retain(|&c| c != child);
            }
        });
    }

    /// `anc` 是否为 `id` 的祖先（含自身相等）——挂载成环检查用。
    fn is_ancestor(&self, anc: usize, id: usize) -> bool {
        let doc = self.doc.borrow();
        let mut stack = vec![anc];
        while let Some(i) = stack.pop() {
            if i == id {
                return true;
            }
            if let Some(n) = doc.arena.get(i) {
                stack.extend(n.children.iter().copied());
            }
        }
        false
    }

    /// 解析 HTML 片段并嫁接到 live arena，返回重映射后的顶层节点 id。
    /// 顶层 = 解析产物中隐含 `<html>` 的子节点（与浏览器 innerHTML 语义近似）。
    fn graft_fragment(&self, html: &str) -> Vec<u64> {
        let (farena, _froot) = crate::parser::parse(html);
        let tops: Vec<usize> = farena
            .iter()
            .position(|n| n.tag.as_deref() == Some("html"))
            .map(|i| farena[i].children.clone())
            .unwrap_or_else(|| farena[0].children.clone());
        self.with(|d| {
            let mut map: HashMap<usize, usize> = HashMap::new();
            tops.into_iter()
                .map(|t| graft_node(d, &farena, t, &mut map) as u64)
                .collect()
        })
    }

    /// class 属性读写（大小写不敏感定位属性）。
    fn class_tokens(&self, id: u64) -> Vec<String> {
        self.with_node(id, Vec::new(), |n| {
            n.attr("class")
                .unwrap_or("")
                .split_whitespace()
                .map(|s| s.to_string())
                .collect()
        })
    }

    fn set_class_tokens(&self, id: u64, toks: &[String]) {
        self.with(|d| {
            let Some(n) = d.arena.get_mut(id as usize) else {
                return;
            };
            if n.tag.is_none() {
                return;
            }
            let v = toks.join(" ");
            match n.attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("class")) {
                Some((_, old)) => *old = v,
                None => n.attrs.push(("class".to_string(), v)),
            }
        });
    }
}

/// 把片段 arena 的一棵子树拷贝进 live arena（下标重映射），返回新下标。
fn graft_node(
    d: &mut Doc,
    farena: &[Node],
    old: usize,
    map: &mut HashMap<usize, usize>,
) -> usize {
    if let Some(&n) = map.get(&old) {
        return n;
    }
    let new_idx = d.arena.len();
    map.insert(old, new_idx);
    let mut node = farena[old].clone();
    node.children = Vec::new(); // 先占位，子节点逐个拷贝
    d.arena.push(node);
    let kids: Vec<usize> = farena[old]
        .children
        .iter()
        .map(|&c| graft_node(d, farena, c, map))
        .collect();
    d.arena[new_idx].children = kids;
    new_idx
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

fn serialize_node(arena: &[Node], idx: usize, out: &mut String) {
    let n = &arena[idx];
    match n.tag.as_deref() {
        None => out.push_str(&escape_text(&n.text)),
        Some(tag) => {
            out.push('<');
            out.push_str(tag);
            for (k, v) in &n.attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(&escape_attr(v));
                out.push('"');
            }
            if crate::tokenizer::c_is_void(tag.as_bytes()) {
                // void elements: no closing tag, HTML style.
                out.push('>');
            } else {
                out.push('>');
                for &c in &n.children {
                    serialize_node(arena, c, out);
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }
}

impl<'a> DomHost for JsDoc<'a> {
    fn document_id(&self) -> u64 {
        self.with(|d| d.root as u64)
    }

    fn tag_name(&self, id: u64) -> String {
        self.with_node(id, String::new(), |n| {
            n.tag.clone().unwrap_or_else(|| "#text".to_string())
        })
    }

    fn get_attribute(&self, id: u64, name: &str) -> Option<String> {
        self.with_node(id, None, |n| {
            n.attr(name).map(|s| s.to_string())
        })
    }

    fn set_attribute(&self, id: u64, name: &str, value: &str) {
        self.with(|d| {
            let Some(n) = d.arena.get_mut(id as usize) else {
                return;
            };
            if n.tag.is_none() {
                return; // text nodes don't take attributes
            }
            if let Some((_, v)) = n
                .attrs
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
            {
                *v = value.to_string();
            } else {
                n.attrs.push((name.to_string(), value.to_string()));
            }
        });
    }

    fn text_content(&self, id: u64) -> String {
        self.with(|d| match d.arena.get(id as usize) {
            Some(_) => d.node_text(id as usize).trim_end().to_string(),
            None => String::new(),
        })
    }

    fn set_text_content(&self, id: u64, text: &str) {
        self.with(|d| {
            let text_idx = d.arena.len();
            d.arena.push(Node::text(text));
            if let Some(n) = d.arena.get_mut(id as usize) {
                if n.tag.is_some() {
                    n.children = vec![text_idx];
                }
            } else {
                // invalid id: drop the orphan text node again
                d.arena.pop();
            }
        });
    }

    fn inner_html(&self, id: u64) -> String {
        self.with(|d| {
            let Some(n) = d.arena.get(id as usize) else {
                return String::new();
            };
            let mut out = String::new();
            for &c in n.children.clone().iter() {
                serialize_node(&d.arena, c, &mut out);
            }
            out
        })
    }

    fn get_element_by_id(&self, id: &str) -> Option<u64> {
        let mut ids = Vec::new();
        self.walk_ids(&mut ids);
        self.with(|d| {
            ids.into_iter().find(|&i| {
                d.arena[i as usize].tag.is_some()
                    && d.arena[i as usize].attr("id") == Some(id)
            })
        })
    }

    fn query_selector_all(&self, selector: &str) -> Vec<u64> {
        // Phase 6：统一走 js-engine 的共享选择器引擎
        //（tag/#id/.class/复合/后代组合；非法选择器返回空）。
        selector::select(self, self.document_id(), selector)
    }

    fn create_element(&self, tag: &str) -> u64 {
        // Detached, like real DOM createElement (caller appends it).
        self.with(|d| {
            let idx = d.arena.len();
            d.arena.push(Node::elem(&tag.to_ascii_lowercase()));
            idx as u64
        })
    }

    // ---- Phase 6：树操作 / classList / innerHTML 写 / 事件 ----

    fn children(&self, id: u64) -> Vec<u64> {
        self.with_node(id, Vec::new(), |n| {
            n.children.iter().map(|&c| c as u64).collect()
        })
    }

    fn append_child(&self, parent: u64, child: u64) {
        let (p, c) = (parent as usize, child as usize);
        let ok = self.with(|d| {
            matches!(d.arena.get(p), Some(n) if n.tag.is_some())
                && d.arena.get(c).is_some()
        });
        if !ok || self.is_ancestor(c, p) {
            return; // 非法 id / 文本节点当父 / 成环：静默跳过
        }
        self.detach(c);
        self.with(|d| {
            d.arena[p].children.push(c);
        });
    }

    fn insert_before(&self, parent: u64, child: u64, before: u64) {
        let (p, c, b) = (parent as usize, child as usize, before as usize);
        let ok = self.with(|d| {
            matches!(d.arena.get(p), Some(n) if n.tag.is_some())
                && d.arena.get(c).is_some()
        });
        if !ok || self.is_ancestor(c, p) {
            return;
        }
        self.detach(c);
        self.with(|d| {
            let kids = &mut d.arena[p].children;
            match kids.iter().position(|&x| x == b) {
                Some(pos) => kids.insert(pos, c),
                None => kids.push(c), // before 不是其子节点时退化为 append
            }
        });
    }

    fn remove_child(&self, parent: u64, child: u64) {
        let (p, c) = (parent as usize, child as usize);
        self.with(|d| {
            if let Some(n) = d.arena.get_mut(p) {
                n.children.retain(|&x| x != c);
            }
        });
    }

    fn set_inner_html(&self, id: u64, html: &str) {
        if self.with_node(id, false, |n| n.tag.is_some()) {
            let tops = self.graft_fragment(html);
            self.with(|d| {
                if let Some(n) = d.arena.get_mut(id as usize) {
                    n.children = tops.into_iter().map(|t| t as usize).collect();
                }
            });
        }
    }

    fn outer_html(&self, id: u64) -> String {
        self.with(|d| {
            if d.arena.get(id as usize).is_none() {
                return String::new();
            }
            let mut out = String::new();
            serialize_node(&d.arena, id as usize, &mut out);
            out
        })
    }

    fn class_add(&self, id: u64, class: &str) {
        let class = class.trim();
        if class.is_empty() || class.chars().any(char::is_whitespace) {
            return;
        }
        let mut toks = self.class_tokens(id);
        if !toks.iter().any(|t| t == class) {
            toks.push(class.to_string());
            self.set_class_tokens(id, &toks);
        }
    }

    fn class_remove(&self, id: u64, class: &str) {
        let mut toks = self.class_tokens(id);
        let before = toks.len();
        toks.retain(|t| t != class);
        if toks.len() != before {
            self.set_class_tokens(id, &toks);
        }
    }

    fn class_contains(&self, id: u64, class: &str) -> bool {
        self.class_tokens(id).iter().any(|t| t == class)
    }

    fn add_event_listener(&self, id: u64, event: &str) -> u64 {
        let tok = self.next_listener.get() + 1;
        self.next_listener.set(tok);
        self.listeners
            .borrow_mut()
            .entry((id, event.to_string()))
            .or_default()
            .push(tok);
        tok
    }

    fn event_listeners(&self, id: u64, event: &str) -> Vec<u64> {
        self.listeners
            .borrow()
            .get(&(id, event.to_string()))
            .cloned()
            .unwrap_or_default()
    }
}

fn flow_err_msg(e: FlowError) -> String {
    match e {
        FlowError::Runtime(r) => r.to_string(),
        FlowError::Thrown(v) => format!("uncaught exception: {}", v.to_js_string()),
    }
}

/// Run JS source against a live `Doc`. Returns `(console_lines, error)`.
///
/// The `DomHost` trait object requires `'static`; the borrow is extended via
/// a raw pointer. This is sound here: the `Doc` is owned by the C/Python
/// caller, which keeps it alive across `yousj_run_js`, and the interpreter
/// (the only holder of the `'static` reference) is dropped before return —
/// the reference never escapes this function.
pub fn run_on_doc(doc: &mut Doc, src: &str) -> (Vec<String>, Option<String>) {
    let doc: &'static mut Doc = unsafe { &mut *(doc as *mut Doc) };
    let host: Rc<dyn DomHost> = Rc::new(JsDoc::new(doc));
    let prog = match yousj_js::parse_source(src) {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Some(e.to_string())),
    };
    let mut ip = Interpreter::new();
    ip.bind_dom(host);
    match ip.run(&prog) {
        Ok(_) => (ip.take_console(), None),
        Err(e) => {
            let console = ip.take_console();
            (console, Some(flow_err_msg(e)))
        }
    }
}

/// `fetch` 回调的 C ABI 类型：输入 URL（`*const c_char`），返回 JSON
/// （`*mut c_char`，`{"status":200,"body":"..."}` 或 `{"error":"..."}`）。
/// 返回的指针在回调返回后由 Rust 立即拷贝，不长期持有。
pub type FetchCallback = unsafe extern "C" fn(*const c_char) -> *mut c_char;

/// `FetchHost` 实现：经 C 回调调 python 层的 `net.get`（同步）。
/// js-engine 保持零依赖，网络由宿主提供。
pub struct CbFetchHost {
    callback: FetchCallback,
}

impl CbFetchHost {
    pub fn new(callback: FetchCallback) -> Self {
        CbFetchHost { callback }
    }
}

impl FetchHost for CbFetchHost {
    fn fetch(&self, url: &str) -> Result<FetchResponse, String> {
        let url_c = CString::new(url).map_err(|e| e.to_string())?;
        let resp_ptr = unsafe { (self.callback)(url_c.as_ptr()) };
        if resp_ptr.is_null() {
            return Err("fetch callback returned null".to_string());
        }
        let json_str = unsafe { CStr::from_ptr(resp_ptr).to_string_lossy().into_owned() };
        parse_fetch_json(&json_str)
    }
}

/// 解析 fetch 回调的 JSON：`{"status":200,"body":"..."}` 或 `{"error":"..."}`。
/// 极简手写解析（只认这两种形状），零依赖。
fn parse_fetch_json(s: &str) -> Result<FetchResponse, String> {
    // 先找 "error"。
    if let Some(body) = extract_json_string(s, "error") {
        return Err(body);
    }
    let status_str = extract_json_number(s, "status").unwrap_or("200".to_string());
    let status: u16 = status_str.parse().map_err(|_| "bad status in fetch JSON".to_string())?;
    let body = extract_json_string(s, "body").unwrap_or_default();
    Ok(FetchResponse::new(status, body))
}

/// 从 `{"key": "value", ...}` 中提取字符串值（处理 JSON 转义）。
fn extract_json_string(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let mut pos = s.find(&needle)?;
    pos += needle.len();
    // 跳过空白和冒号。
    let bytes = s.as_bytes();
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b':' || bytes[pos] == b'\t') {
        pos += 1;
    }
    if pos >= bytes.len() || bytes[pos] != b'"' {
        return None;
    }
    pos += 1;
    let mut out = String::new();
    let chars: Vec<char> = s[pos..].chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            return Some(out);
        }
        if c == '\\' && i + 1 < chars.len() {
            let e = chars[i + 1];
            match e {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' if i + 5 < chars.len() => {
                    let hex: String = chars[i + 2..i + 6].iter().collect();
                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = std::char::from_u32(cp) {
                            out.push(ch);
                        }
                    }
                    i += 4;
                }
                _ => out.push(e),
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    None
}

/// 从 `{"key": 123, ...}` 中提取数字值。
fn extract_json_number(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let mut pos = s.find(&needle)?;
    pos += needle.len();
    let bytes = s.as_bytes();
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b':' || bytes[pos] == b'\t') {
        pos += 1;
    }
    let start = pos;
    while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'-') {
        pos += 1;
    }
    if pos > start {
        Some(s[start..pos].to_string())
    } else {
        None
    }
}

/// 带 fetch 的 `run_on_doc`：绑定 `CbFetchHost`，其余同 `run_on_doc`。
pub fn run_on_doc_with_fetch(
    doc: &mut Doc,
    src: &str,
    fetch_cb: FetchCallback,
) -> (Vec<String>, Option<String>) {
    let doc: &'static mut Doc = unsafe { &mut *(doc as *mut Doc) };
    let host: Rc<dyn DomHost> = Rc::new(JsDoc::new(doc));
    let prog = match yousj_js::parse_source(src) {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Some(e.to_string())),
    };
    let mut ip = Interpreter::new();
    ip.bind_dom(host);
    ip.bind_fetch(Rc::new(CbFetchHost::new(fetch_cb)));
    match ip.run(&prog) {
        Ok(_) => (ip.take_console(), None),
        Err(e) => {
            let console = ip.take_console();
            (console, Some(flow_err_msg(e)))
        }
    }
}

/// phase 8：带调试器运行 JS。`breakpoints` 为行号断点列表；
/// 返回 `(console, error, pauses)`，pauses 为暂停快照（JSON 由 C ABI 层编码）。
pub fn run_on_doc_with_debug(
    doc: &mut Doc,
    src: &str,
    breakpoints: &[usize],
) -> (Vec<String>, Option<String>, Vec<DebugPause>) {
    let doc: &'static mut Doc = unsafe { &mut *(doc as *mut Doc) };
    let host: Rc<dyn DomHost> = Rc::new(JsDoc::new(doc));
    let prog = match yousj_js::parse_source(src) {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Some(e.to_string()), Vec::new()),
    };
    let mut ip = Interpreter::new();
    ip.bind_dom(host);
    let bp_host = Rc::new(std::cell::RefCell::new(BreakpointHost::new(
        breakpoints.to_vec(),
    )));
    ip.set_debug_host(bp_host);
    let (console, error) = match ip.run(&prog) {
        Ok(_) => (ip.take_console(), None),
        Err(e) => {
            let console = ip.take_console();
            (console, Some(flow_err_msg(e)))
        }
    };
    (console, error, ip.take_debug_pauses())
}

/// phase 8：把暂停快照编码为 JSON（`debug_hits` 数组元素）。
pub fn debug_pauses_json(pauses: &[DebugPause]) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    }
    let mut out = String::from("[");
    for (i, p) in pauses.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "{{\"line\": {}, \"col\": {}, \"stack\": [",
            p.line, p.col
        ));
        for (j, f) in p.stack.iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!(
                "{{\"name\": \"{}\", \"line\": {}, \"col\": {}}}",
                esc(&f.name),
                f.line,
                f.col
            ));
        }
        out.push_str("], \"vars\": {");
        // 排序保证输出稳定（HashMap 迭代顺序不定）。
        let mut vars: Vec<(&String, &String)> = p.vars.iter().collect();
        vars.sort_by(|a, b| a.0.cmp(b.0));
        for (k, (name, val)) in vars.iter().enumerate() {
            if k > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("\"{}\": \"{}\"", esc(name), esc(val)));
        }
        out.push_str("}}");
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn doc_of(html: &str) -> Doc {
        let (arena, root) = parser::parse(html);
        Doc { arena, root }
    }

    fn host_of(html: &str) -> JsDoc<'static> {
        // Leak the Doc so the borrow lives for 'static; harmless in tests
        // (the test process exits), never do this in production code.
        let doc: &'static mut Doc = Box::leak(Box::new(doc_of(html)));
        JsDoc::new(doc)
    }

    #[test]
    fn get_element_by_id_finds_node() {
        let h = host_of(r#"<html><body><div id="t">hi</div></body></html>"#);
        let id = h.get_element_by_id("t").expect("should find #t");
        assert_eq!(h.tag_name(id), "div");
        assert_eq!(h.get_attribute(id, "id").as_deref(), Some("t"));
        assert!(h.get_element_by_id("nope").is_none());
    }

    #[test]
    fn query_selector_all_tag_class_id() {
        let h = host_of(
            r#"<html><body><p class="x">a</p><p class="x y">b</p><span>c</span></body></html>"#,
        );
        assert_eq!(h.query_selector_all("p").len(), 2);
        assert_eq!(h.query_selector_all("span").len(), 1);
        assert_eq!(h.query_selector_all("div").len(), 0);
        assert_eq!(h.query_selector_all(".x").len(), 2);
        assert_eq!(h.query_selector_all(".y").len(), 1);
        assert_eq!(h.query_selector_all(".zzz").len(), 0);
    }

    #[test]
    fn text_content_rw() {
        let h = host_of(r#"<html><body><div id="t">a<b>bold</b>c</div></body></html>"#);
        let id = h.get_element_by_id("t").unwrap();
        assert!(h.text_content(id).contains("bold"));
        h.set_text_content(id, "replaced");
        assert_eq!(h.text_content(id), "replaced");
        // children collapsed to a single text node
        assert_eq!(h.query_selector_all("b").len(), 0);
    }

    #[test]
    fn set_attribute_roundtrip() {
        let h = host_of(r#"<html><body><div id="t"></div></body></html>"#);
        let id = h.get_element_by_id("t").unwrap();
        h.set_attribute(id, "data-n", "7");
        assert_eq!(h.get_attribute(id, "data-n").as_deref(), Some("7"));
        assert_eq!(h.get_attribute(id, "DATA-N").as_deref(), Some("7")); // case-insensitive
        h.set_attribute(id, "id", "t2");
        assert!(h.get_element_by_id("t").is_none());
        assert_eq!(h.get_element_by_id("t2"), Some(id));
    }

    #[test]
    fn inner_html_serializes() {
        let h = host_of(
            r#"<html><body><div id="t"><span class="s">x</span><br></div></body></html>"#,
        );
        let id = h.get_element_by_id("t").unwrap();
        let html = h.inner_html(id);
        assert!(html.contains(r#"<span class="s">x</span>"#), "got: {}", html);
        assert!(html.contains("<br>"), "got: {}", html);
        assert!(!html.contains("</br>"), "got: {}", html);
    }

    #[test]
    fn create_element_is_detached() {
        let h = host_of(r#"<html><body></body></html>"#);
        let id = h.create_element("SECTION");
        assert_eq!(h.tag_name(id), "section"); // lowercased
        assert_eq!(h.query_selector_all("section").len(), 0); // not in tree
    }

    #[test]
    fn invalid_ids_are_safe() {
        let h = host_of(r#"<html><body></body></html>"#);
        assert_eq!(h.tag_name(99999), "");
        assert_eq!(h.get_attribute(99999, "x"), None);
        assert_eq!(h.text_content(99999), "");
        assert_eq!(h.inner_html(99999), "");
        h.set_attribute(99999, "x", "y"); // no panic
        h.set_text_content(99999, "z"); // no panic
    }

    // ---- Phase 6 ----

    #[test]
    fn compound_and_descendant_selectors() {
        let h = host_of(
            r#"<html><body><div id="a"><span class="b">x</span><span class="b c">y</span></div><div class="x"></div></body></html>"#,
        );
        assert_eq!(h.query_selector_all("div.x").len(), 1);
        assert_eq!(h.query_selector_all("span.b").len(), 2);
        assert_eq!(h.query_selector_all("#a .b").len(), 2);
        assert_eq!(h.query_selector_all("#a .c").len(), 1);
        assert_eq!(h.query_selector_all("body div span").len(), 2);
        assert_eq!(h.query_selector_all("#a .missing").len(), 0);
        assert_eq!(h.query_selector_all("div[").len(), 0); // 非法 → 空
        assert_eq!(h.query_selector("#a .c").map(|id| h.tag_name(id)).as_deref(), Some("span"));
        assert_eq!(h.query_selector(".missing"), None);
    }

    #[test]
    fn append_remove_child_real_arena() {
        let h = host_of(r#"<html><body><div id="t"></div></body></html>"#);
        let t = h.get_element_by_id("t").unwrap();
        let el = h.create_element("p");
        h.append_child(t, el);
        assert_eq!(h.query_selector_all("p").len(), 1);
        // arena 下标稳定：旧 id 仍然有效
        assert_eq!(h.tag_name(t), "div");
        h.remove_child(t, el);
        assert_eq!(h.query_selector_all("p").len(), 0);
        assert_eq!(h.get_element_by_id("t"), Some(t)); // t 不受影响
    }

    #[test]
    fn insert_before_real() {
        let h = host_of(
            r#"<html><body><div id="t"><b>1</b><i>2</i></div></body></html>"#,
        );
        let t = h.get_element_by_id("t").unwrap();
        let n = h.create_element("u");
        let second = h.query_selector_all("i")[0];
        h.insert_before(t, n, second);
        let tags: Vec<String> =
            h.children(t).iter().map(|&c| h.tag_name(c)).collect();
        assert_eq!(tags, vec!["b", "u", "i"]);
    }

    #[test]
    fn set_inner_html_grafts_fragment() {
        let h = host_of(r#"<html><body><div id="t">old</div></body></html>"#);
        let t = h.get_element_by_id("t").unwrap();
        h.set_inner_html(t, r#"<b class="z">hi <i>x</i></b><br>"#);
        assert_eq!(h.text_content(t).replace(|c: char| c.is_whitespace(), ""), "hix");
        assert_eq!(h.query_selector_all("b.z").len(), 1);
        assert_eq!(h.query_selector_all("b i").len(), 1);
        assert!(h.inner_html(t).contains("<br>"));
        assert!(!h.inner_html(t).contains("old"));
    }

    #[test]
    fn outer_html_real() {
        let h = host_of(r#"<html><body><div id="t" class="a">hi</div></body></html>"#);
        let t = h.get_element_by_id("t").unwrap();
        let html = h.outer_html(t);
        assert!(html.starts_with("<div"), "got: {}", html);
        assert!(html.contains("hi"), "got: {}", html);
        assert!(html.ends_with("</div>"), "got: {}", html);
    }

    #[test]
    fn class_list_real() {
        let h = host_of(r#"<html><body><div id="t"></div></body></html>"#);
        let t = h.get_element_by_id("t").unwrap();
        h.class_add(t, "a");
        h.class_add(t, "b");
        h.class_add(t, "a"); // 重复添加无效
        assert!(h.class_contains(t, "a"));
        assert_eq!(h.get_attribute(t, "class").as_deref(), Some("a b"));
        h.class_remove(t, "a");
        assert!(!h.class_contains(t, "a"));
        assert!(h.class_contains(t, "b"));
        assert_eq!(h.query_selector_all(".b").len(), 1);
    }

    #[test]
    fn event_listener_tokens() {
        let h = host_of(r#"<html><body><div id="t"></div></body></html>"#);
        let t = h.get_element_by_id("t").unwrap();
        let t1 = h.add_event_listener(t, "click");
        let t2 = h.add_event_listener(t, "click");
        assert!(t1 != 0 && t2 != 0 && t1 != t2);
        assert_eq!(h.event_listeners(t, "click"), vec![t1, t2]);
        assert!(h.event_listeners(t, "mouseover").is_empty());
    }

    #[test]
    fn run_js_uses_new_dom_apis() {
        // 端到端：真实 DOM 上跑 JS，用 phase 6 的 API。
        let mut doc = doc_of(
            r#"<html><body><div id="t"></div><ul id="list"></ul></body></html>"#,
        );
        let (console, err) = run_on_doc(
            &mut doc,
            r#"
            const t = document.getElementById('t');
            t.innerHTML = '<b class="z">hi</b>';
            t.classList.add('on');
            const li = document.createElement('li');
            li.textContent = 'item';
            document.getElementById('list').appendChild(li);
            let n = 0;
            t.addEventListener('click', () => { n++; });
            t.click();
            window.setTimeout(() => t.setAttribute('data-done', '1'), 0);
            console.log('wired:' + document.querySelectorAll('#t .z').length);
            "#,
        );
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(console, vec!["wired:1"]);
        // 宿主侧验证：JS 的修改真实落到了 arena 上
        let mut html = String::new();
        serialize_node(&doc.arena, doc.root, &mut html);
        assert!(html.contains(r#"<b class="z">hi</b>"#), "got: {}", html);
        assert!(html.contains("<li>item</li>"), "got: {}", html);
        assert!(html.contains(r#"data-done="1""#), "got: {}", html);
    }
}
