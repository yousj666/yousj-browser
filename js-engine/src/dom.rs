//! yousj-js · Phase 4：DOM 宿主接口。
//!
//! 设计：
//! - `DomHost` trait：宿主（phase 5 接真实 HTML 引擎，
//!   本阶段用 `MockDom`）实现的 DOM 操作。全部 `&self`（内部用
//!   `RefCell` 做可变），以便 `Rc<dyn DomHost>` 共享。
//! - `DomNode`：轻量句柄 `{ node_id, host }`，解释器侧以
//!   `Value::DomNode` 持有；方法调用经解释器分发回宿主。
//! - `NullDom`：未绑定宿主时的空实现（本阶段 `bind_dom` 未调用时
//!   `document` 即为未定义，走不到这里；保留作嵌入方默认）。
//! - `MockDom`：测试用内存 DOM，支持 `#id` / 标签名选择器。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// 宿主 DOM 接口。phase 5 用 feature 门控对接真实 HTML 引擎时，
/// 只需再实现一遍这个 trait，解释器侧零改动。
pub trait DomHost {
    /// 文档根节点 id（`document` 自身查询用）。
    fn document_id(&self) -> u64;
    fn tag_name(&self, id: u64) -> String;
    fn get_attribute(&self, id: u64, name: &str) -> Option<String>;
    fn set_attribute(&self, id: u64, name: &str, value: &str);
    fn text_content(&self, id: u64) -> String;
    fn set_text_content(&self, id: u64, text: &str);
    /// 简化版：只读（phase 5 再做完整序列化）。
    fn inner_html(&self, id: u64) -> String;
    fn get_element_by_id(&self, id: &str) -> Option<u64>;
    /// 子集选择器：`#id` 或标签名（如 `div`）。
    fn query_selector_all(&self, selector: &str) -> Vec<u64>;
    fn create_element(&self, tag: &str) -> u64;

    // ---- Phase 6 新增（都有默认实现；NullDom 无需改动） ----

    /// 子节点 id 列表（选择器引擎 / 遍历用）。
    fn children(&self, id: u64) -> Vec<u64> {
        let _ = id;
        Vec::new()
    }

    /// 单数版：默认取 `query_selector_all` 的第一个。
    fn query_selector(&self, selector: &str) -> Option<u64> {
        self.query_selector_all(selector).into_iter().next()
    }

    /// 挂载 / 摘除。arena 只追加不压缩，下标天然稳定，
    /// 无需 tombstone：摘除只是从父节点的 children 里拿掉。
    fn append_child(&self, parent: u64, child: u64) {
        let _ = (parent, child);
    }
    fn insert_before(&self, parent: u64, child: u64, before: u64) {
        let _ = (parent, child, before);
    }
    fn remove_child(&self, parent: u64, child: u64) {
        let _ = (parent, child);
    }

    /// `innerHTML` 写：解析片段后替换全部子节点。
    fn set_inner_html(&self, id: u64, html: &str) {
        let _ = (id, html);
    }
    /// `outerHTML` 读：节点自身的序列化。
    fn outer_html(&self, id: u64) -> String {
        let _ = id;
        String::new()
    }

    /// `classList` 简化版（操作 class 属性的空白分隔 token）。
    fn class_add(&self, id: u64, class: &str) {
        let _ = (id, class);
    }
    fn class_remove(&self, id: u64, class: &str) {
        let _ = (id, class);
    }
    fn class_contains(&self, id: u64, class: &str) -> bool {
        let _ = (id, class);
        false
    }

    /// 事件监听：宿主只存 token（`u64`，0 表无效），解释器另存
    /// token → JS 回调的映射；`click()` 时解释器把回调推进任务队列。
    fn add_event_listener(&self, id: u64, event: &str) -> u64 {
        let _ = (id, event);
        0
    }
    fn event_listeners(&self, id: u64, event: &str) -> Vec<u64> {
        let _ = (id, event);
        Vec::new()
    }
}

/// 挂载成环检查：`parent == child`，或 parent 落在 child 的子树内。
/// 解释器在 `appendChild` / `insertBefore` 前调用，避免遍历死循环；
/// 命中时调用方静默跳过（不抛错）。
pub fn would_create_cycle(host: &dyn DomHost, parent: u64, child: u64) -> bool {
    if parent == child {
        return true;
    }
    let mut stack = vec![child];
    while let Some(id) = stack.pop() {
        if id == parent {
            return true;
        }
        stack.extend(host.children(id));
    }
    false
}

/// DOM 节点句柄：解释器以 `Value::DomNode` 持有它。
#[derive(Clone)]
pub struct DomNode {
    pub node_id: u64,
    pub host: Rc<dyn DomHost>,
}

impl DomNode {
    pub fn new(node_id: u64, host: Rc<dyn DomHost>) -> Self {
        DomNode { node_id, host }
    }

    /// 同一宿主 + 同一节点 → 同一（`===` 用）。
    pub fn same_node(&self, other: &DomNode) -> bool {
        self.node_id == other.node_id && Rc::ptr_eq(&self.host, &other.host)
    }

    pub fn tag_name(&self) -> String {
        self.host.tag_name(self.node_id)
    }

    /// `[object HTMLDivElement]` 风格（`to_js_string` 用）。
    pub fn js_class_string(&self) -> String {
        format!("[object HTML{}Element]", self.tag_name().to_uppercase())
    }

    /// Debug 用的一行描述。
    pub fn describe(&self) -> String {
        format!("#{} <{}>", self.node_id, self.tag_name())
    }
}

impl std::fmt::Debug for DomNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DomNode({})", self.describe())
    }
}

// ---------------------------------------------------------------------------
// NullDom：空实现
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct NullDom;

impl DomHost for NullDom {
    fn document_id(&self) -> u64 {
        0
    }
    fn tag_name(&self, _id: u64) -> String {
        String::new()
    }
    fn get_attribute(&self, _id: u64, _name: &str) -> Option<String> {
        None
    }
    fn set_attribute(&self, _id: u64, _name: &str, _value: &str) {}
    fn text_content(&self, _id: u64) -> String {
        String::new()
    }
    fn set_text_content(&self, _id: u64, _text: &str) {}
    fn inner_html(&self, _id: u64) -> String {
        String::new()
    }
    fn get_element_by_id(&self, _id: &str) -> Option<u64> {
        None
    }
    fn query_selector_all(&self, _selector: &str) -> Vec<u64> {
        Vec::new()
    }
    fn create_element(&self, _tag: &str) -> u64 {
        0
    }
}

// ---------------------------------------------------------------------------
// MockDom：测试用内存 DOM
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MockNode {
    tag: String,
    attrs: HashMap<String, String>,
    text: String,
    children: Vec<u64>,
}

#[derive(Debug, Default)]
pub struct MockDom {
    nodes: RefCell<HashMap<u64, MockNode>>,
    next_id: Cell<u64>,
    by_id: RefCell<HashMap<String, u64>>,
    document: u64,
    listeners: RefCell<HashMap<(u64, String), Vec<u64>>>,
    next_listener: Cell<u64>,
}

impl MockDom {
    pub fn new() -> Self {
        let dom = MockDom {
            nodes: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            by_id: RefCell::new(HashMap::new()),
            document: 0,
            listeners: RefCell::new(HashMap::new()),
            next_listener: Cell::new(0),
        };
        dom.nodes.borrow_mut().insert(
            0,
            MockNode {
                tag: "#document".to_string(),
                ..Default::default()
            },
        );
        dom
    }

    fn alloc(&self, tag: &str) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.nodes.borrow_mut().insert(
            id,
            MockNode {
                tag: tag.to_string(),
                ..Default::default()
            },
        );
        id
    }

    /// 建一个元素并挂到 parent 下；返回节点 id。测试脚手架用。
    pub fn add_element(
        &self,
        parent: u64,
        tag: &str,
        attrs: &[(&str, &str)],
        text: &str,
    ) -> u64 {
        let id = self.alloc(tag);
        {
            let mut nodes = self.nodes.borrow_mut();
            let n = nodes.get_mut(&id).unwrap();
            for (k, v) in attrs {
                n.attrs.insert(k.to_string(), v.to_string());
            }
            n.text = text.to_string();
            if let Some(p) = nodes.get_mut(&parent) {
                p.children.push(id);
            }
        }
        if let Some((_, id_attr)) = attrs.iter().find(|(k, _)| *k == "id") {
            self.by_id
                .borrow_mut()
                .insert(id_attr.to_string(), id);
        }
        id
    }

    /// 把 child 从它当前所在的父节点摘下（挂载前的 detach 语义）。
    fn detach(&self, child: u64) {
        let mut nodes = self.nodes.borrow_mut();
        for n in nodes.values_mut() {
            n.children.retain(|&c| c != child);
        }
    }

    /// 摘除子树时同步清理 by_id（真 DOM 里 detached 节点不可被查到）。
    fn drop_ids_in_subtree(&self, id: u64) {
        let mut stack = vec![id];
        let nodes = self.nodes.borrow();
        let mut by_id = self.by_id.borrow_mut();
        while let Some(i) = stack.pop() {
            if let Some(n) = nodes.get(&i) {
                if let Some(v) = n.attrs.get("id") {
                    if by_id.get(v) == Some(&i) {
                        by_id.remove(v);
                    }
                }
                stack.extend(n.children.iter().copied());
            }
        }
    }

    /// class 属性的空白分隔 token 列表。
    fn class_tokens(&self, id: u64) -> Vec<String> {
        self.get_attribute(id, "class")
            .unwrap_or_default()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect()
    }

    fn set_class_tokens(&self, id: u64, toks: &[String]) {
        if let Some(n) = self.nodes.borrow_mut().get_mut(&id) {
            n.attrs
                .insert("class".to_string(), toks.join(" "));
        }
    }

    /// 极简 HTML 片段解析（标签 / 属性 / 文本 / 注释），挂到 `parent` 下。
    /// 属性只支持引号包裹（`k="v"` / `k='v'`）；残缺标签直接丢弃剩余输入。
    fn parse_fragment(&self, parent: u64, html: &str) {
        let b = html.as_bytes();
        let mut i = 0;
        let mut stack: Vec<u64> = vec![parent];
        while i < b.len() {
            if b[i] == b'<' {
                if html[i..].starts_with("<!--") {
                    match html[i..].find("-->") {
                        Some(e) => i += e + 3,
                        None => break,
                    }
                    continue;
                }
                // 找配对的 '>'（跳过引号内的 '>'）
                let mut k = i + 1;
                let mut quote: Option<u8> = None;
                while k < b.len() {
                    match quote {
                        Some(q) => {
                            if b[k] == q {
                                quote = None;
                            }
                        }
                        None => {
                            if b[k] == b'"' || b[k] == b'\'' {
                                quote = Some(b[k]);
                            } else if b[k] == b'>' {
                                break;
                            }
                        }
                    }
                    k += 1;
                }
                if k >= b.len() {
                    break;
                }
                let mut inner = html[i + 1..k].trim();
                i = k + 1;
                if let Some(rest) = inner.strip_prefix('/') {
                    // 闭合标签：弹栈到匹配者（宽容模式）
                    let want = rest.trim().to_ascii_lowercase();
                    while stack.len() > 1 {
                        let top = *stack.last().unwrap();
                        let t = self.tag_name(top).to_ascii_lowercase();
                        stack.pop();
                        if t == want {
                            break;
                        }
                    }
                    continue;
                }
                let self_close = inner.ends_with('/');
                if self_close {
                    inner = inner[..inner.len() - 1].trim();
                }
                let (tag, attrs) = split_tag(inner);
                if tag.is_empty() {
                    continue;
                }
                let tag = tag.to_ascii_lowercase();
                let id = self.alloc(&tag);
                for (ak, av) in attrs {
                    self.set_attribute(id, &ak, &av); // 同步 by_id
                }
                let p = *stack.last().unwrap();
                self.nodes
                    .borrow_mut()
                    .get_mut(&p)
                    .unwrap()
                    .children
                    .push(id);
                if !self_close && !is_void_tag(&tag) {
                    stack.push(id);
                }
            } else {
                let start = i;
                while i < b.len() && b[i] != b'<' {
                    i += 1;
                }
                let text = &html[start..i];
                if !text.trim().is_empty() {
                    let id = self.alloc("#text");
                    let p = *stack.last().unwrap();
                    let mut nodes = self.nodes.borrow_mut();
                    nodes.get_mut(&id).unwrap().text = text.to_string();
                    nodes.get_mut(&p).unwrap().children.push(id);
                }
            }
        }
    }
}

impl DomHost for MockDom {
    fn document_id(&self) -> u64 {
        self.document
    }

    fn tag_name(&self, id: u64) -> String {
        self.nodes
            .borrow()
            .get(&id)
            .map(|n| n.tag.clone())
            .unwrap_or_default()
    }

    fn get_attribute(&self, id: u64, name: &str) -> Option<String> {
        self.nodes
            .borrow()
            .get(&id)
            .and_then(|n| n.attrs.get(name).cloned())
    }

    fn set_attribute(&self, id: u64, name: &str, value: &str) {
        if let Some(n) = self.nodes.borrow_mut().get_mut(&id) {
            n.attrs.insert(name.to_string(), value.to_string());
            if name == "id" {
                self.by_id.borrow_mut().insert(value.to_string(), id);
            }
        }
    }

    fn text_content(&self, id: u64) -> String {
        // 递归：与真实引擎的 node_text 语义一致（含 #text 子节点）。
        fn rec(nodes: &HashMap<u64, MockNode>, id: u64, out: &mut String) {
            if let Some(n) = nodes.get(&id) {
                out.push_str(&n.text);
                for c in &n.children {
                    rec(nodes, *c, out);
                }
            }
        }
        let nodes = self.nodes.borrow();
        let mut out = String::new();
        rec(&nodes, id, &mut out);
        out
    }

    fn set_text_content(&self, id: u64, text: &str) {
        if let Some(n) = self.nodes.borrow_mut().get_mut(&id) {
            n.text = text.to_string();
        }
    }

    fn inner_html(&self, id: u64) -> String {
        let nodes = self.nodes.borrow();
        let node = match nodes.get(&id) {
            Some(n) => n,
            None => return String::new(),
        };
        let mut out = String::new();
        for c in &node.children {
            serialize_mock_node(&nodes, *c, &mut out);
        }
        out
    }

    fn outer_html(&self, id: u64) -> String {
        let nodes = self.nodes.borrow();
        if !nodes.contains_key(&id) {
            return String::new();
        }
        let mut out = String::new();
        serialize_mock_node(&nodes, id, &mut out);
        out
    }

    fn get_element_by_id(&self, id: &str) -> Option<u64> {
        self.by_id.borrow().get(id).copied()
    }

    fn query_selector_all(&self, selector: &str) -> Vec<u64> {
        // Phase 6：统一走共享选择器引擎（tag/#id/.class/复合/后代组合）。
        crate::selector::select(self, self.document, selector)
    }

    fn create_element(&self, tag: &str) -> u64 {
        self.alloc(tag)
    }

    // ---- Phase 6：树操作 / classList / 事件 ----

    fn children(&self, id: u64) -> Vec<u64> {
        self.nodes
            .borrow()
            .get(&id)
            .map(|n| n.children.clone())
            .unwrap_or_default()
    }

    fn append_child(&self, parent: u64, child: u64) {
        {
            let nodes = self.nodes.borrow();
            if !nodes.contains_key(&parent) || !nodes.contains_key(&child) {
                return;
            }
        }
        self.detach(child);
        self.nodes
            .borrow_mut()
            .get_mut(&parent)
            .unwrap()
            .children
            .push(child);
    }

    fn insert_before(&self, parent: u64, child: u64, before: u64) {
        {
            let nodes = self.nodes.borrow();
            if !nodes.contains_key(&parent) || !nodes.contains_key(&child) {
                return;
            }
        }
        self.detach(child);
        let mut nodes = self.nodes.borrow_mut();
        let kids = &mut nodes.get_mut(&parent).unwrap().children;
        match kids.iter().position(|&x| x == before) {
            Some(pos) => kids.insert(pos, child),
            None => kids.push(child), // before 不是其子节点时退化为 append
        }
    }

    fn remove_child(&self, parent: u64, child: u64) {
        if let Some(n) = self.nodes.borrow_mut().get_mut(&parent) {
            n.children.retain(|&c| c != child);
        }
        self.drop_ids_in_subtree(child);
    }

    fn set_inner_html(&self, id: u64, html: &str) {
        if !self.nodes.borrow().contains_key(&id) {
            return;
        }
        {
            let mut nodes = self.nodes.borrow_mut();
            let n = nodes.get_mut(&id).unwrap();
            n.text.clear(); // 直接文本也要清（add_element 把文本存在 n.text 里）
            n.children.clear();
        }
        self.parse_fragment(id, html);
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

/// 切分 `<tag k="v" ...>` 内部：标签名 + 属性表（引号外空白切分）。
fn split_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let b = inner.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        let mut quote: Option<u8> = None;
        while i < b.len() {
            match quote {
                Some(q) => {
                    if b[i] == q {
                        quote = None;
                    }
                }
                None => {
                    if b[i] == b'"' || b[i] == b'\'' {
                        quote = Some(b[i]);
                    } else if b[i].is_ascii_whitespace() {
                        break;
                    }
                }
            }
            i += 1;
        }
        toks.push(inner[start..i].to_string());
    }
    if toks.is_empty() {
        return (String::new(), Vec::new());
    }
    let tag = toks.remove(0);
    let mut attrs = Vec::new();
    for t in toks {
        match t.find('=') {
            Some(e) => {
                let k = t[..e].to_string();
                let mut v = t[e + 1..].to_string();
                if v.len() >= 2
                    && ((v.starts_with('"') && v.ends_with('"'))
                        || (v.starts_with('\'') && v.ends_with('\'')))
                {
                    v = v[1..v.len() - 1].to_string();
                }
                attrs.push((k, v));
            }
            None => attrs.push((t, String::new())),
        }
    }
    (tag, attrs)
}

/// void 元素（无闭合标签，不入栈嵌套）。子集表。
fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

/// MockDom 节点序列化（`#text` 节点直接输出文本）。
fn serialize_mock_node(
    nodes: &HashMap<u64, MockNode>,
    id: u64,
    out: &mut String,
) {
    let n = match nodes.get(&id) {
        Some(n) => n,
        None => return,
    };
    if n.tag == "#text" {
        out.push_str(&n.text);
        return;
    }
    out.push('<');
    out.push_str(&n.tag);
    for (k, v) in &n.attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        out.push_str(v);
        out.push('"');
    }
    out.push('>');
    // 直接文本（add_element 存法）+ 子节点
    out.push_str(&n.text);
    for c in &n.children {
        serialize_mock_node(nodes, *c, out);
    }
    out.push_str("</");
    out.push_str(&n.tag);
    out.push('>');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dom() -> MockDom {
        let dom = MockDom::new();
        let body = dom.add_element(0, "body", &[], "");
        dom.add_element(body, "div", &[("id", "t")], "hello");
        dom.add_element(body, "p", &[("class", "x")], "world");
        dom
    }

    #[test]
    fn mock_get_element_by_id() {
        let dom = sample_dom();
        let id = dom.get_element_by_id("t").expect("should find #t");
        assert_eq!(dom.tag_name(id), "div");
        assert_eq!(dom.text_content(id), "hello");
    }

    #[test]
    fn mock_set_attribute_roundtrip() {
        let dom = sample_dom();
        let id = dom.get_element_by_id("t").unwrap();
        dom.set_attribute(id, "data-v", "42");
        assert_eq!(dom.get_attribute(id, "data-v").as_deref(), Some("42"));
        // id 属性写入后可被 getElementById 查到
        dom.set_attribute(id, "id", "t2");
        assert_eq!(dom.get_element_by_id("t2"), Some(id));
    }

    #[test]
    fn mock_query_selector_all() {
        let dom = sample_dom();
        assert_eq!(dom.query_selector_all("div").len(), 1);
        assert_eq!(dom.query_selector_all("p").len(), 1);
        assert_eq!(dom.query_selector_all("span").len(), 0);
        assert_eq!(dom.query_selector_all("#t").len(), 1);
    }

    #[test]
    fn mock_text_content_rw() {
        let dom = sample_dom();
        let id = dom.get_element_by_id("t").unwrap();
        dom.set_text_content(id, "changed");
        assert_eq!(dom.text_content(id), "changed");
    }

    #[test]
    fn null_dom_is_empty() {
        let dom = NullDom;
        assert_eq!(dom.get_element_by_id("x"), None);
        assert!(dom.query_selector_all("div").is_empty());
        assert_eq!(dom.text_content(99), "");
    }

    #[test]
    fn dom_node_identity() {
        let host: Rc<dyn DomHost> = Rc::new(sample_dom());
        let a = DomNode::new(1, host.clone());
        let b = DomNode::new(1, host.clone());
        let c = DomNode::new(2, host.clone());
        assert!(a.same_node(&b));
        assert!(!a.same_node(&c));
        assert!(a.js_class_string().contains("BODY")); // node 1 = body
        assert!(c.js_class_string().contains("DIV")); // node 2 = div#t
    }
}
