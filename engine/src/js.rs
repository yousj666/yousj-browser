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

use std::cell::RefCell;
use std::rc::Rc;

use yousj_js::dom::DomHost;
use yousj_js::{FlowError, Interpreter};

use crate::dom::Node;
use crate::Doc;

/// `DomHost` over the live HTML arena. `RefCell` gives the `&self` trait
/// methods interior mutability over the borrowed `Doc`.
pub struct JsDoc<'a> {
    doc: RefCell<&'a mut Doc>,
}

impl<'a> JsDoc<'a> {
    pub fn new(doc: &'a mut Doc) -> Self {
        JsDoc {
            doc: RefCell::new(doc),
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
        let s = selector.trim();
        if s.is_empty() {
            return Vec::new();
        }
        if let Some(id) = s.strip_prefix('#') {
            // #id — ids can't contain whitespace; extra tokens are TODO.
            let id = id.split_whitespace().next().unwrap_or("");
            return self.get_element_by_id(id).into_iter().collect();
        }
        let mut ids = Vec::new();
        self.walk_ids(&mut ids);
        if let Some(class) = s.strip_prefix('.') {
            // .class — single class token (compound selectors are TODO).
            let class = class.split_whitespace().next().unwrap_or("");
            return self.with(|d| {
                ids.into_iter()
                    .filter(|&i| {
                        let n = &d.arena[i as usize];
                        n.tag.is_some()
                            && n.attr("class")
                                .map(|c| c.split_whitespace().any(|w| w == class))
                                .unwrap_or(false)
                    })
                    .collect()
            });
        }
        // tag name (tags are lowercased by the parser).
        let want = s.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        self.with(|d| {
            ids.into_iter()
                .filter(|&i| {
                    d.arena[i as usize].tag.as_deref() == Some(want.as_str())
                })
                .collect()
        })
    }

    fn create_element(&self, tag: &str) -> u64 {
        // Detached, like real DOM createElement (caller appends it).
        self.with(|d| {
            let idx = d.arena.len();
            d.arena.push(Node::elem(&tag.to_ascii_lowercase()));
            idx as u64
        })
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
}
