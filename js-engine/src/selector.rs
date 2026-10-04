//! yousj-js · Phase 6：CSS 子集选择器解析与匹配。
//!
//! 支持：`tag`、`#id`、`.class`、复合 `div.x` / `div#id.cls`、后代组合
//! （空格，可多层，如 `#a .b span`）。
//!
//! 非法选择器 → `parse_selector` 返回 `None`，调用方按浏览器行为
//! 返回空结果集，而不抛错。

use std::collections::HashSet;

use crate::dom::DomHost;

/// 单个复合选择器，如 `div#main.box.big`。
#[derive(Debug, Clone, Default)]
pub struct Compound {
    /// 小写标签名；`None` = 通配（`*` 或未写）。
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

/// 后代组合链（从左到右：祖先 → 后代）。
#[derive(Debug, Clone, Default)]
pub struct Selector {
    pub parts: Vec<Compound>,
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// 解析单个复合 token（不含空格），如 `div` / `#a` / `.x` / `div.x#y.z`。
fn parse_compound(token: &str) -> Option<Compound> {
    let b = token.as_bytes();
    let mut i = 0;
    let mut c = Compound::default();
    // 可选的标签名或通配符 `*`
    if i < b.len() && b[i] == b'*' {
        i += 1;
    } else if i < b.len() && is_name_start(b[i]) {
        let start = i;
        while i < b.len() && is_name_char(b[i]) {
            i += 1;
        }
        c.tag = Some(token[start..i].to_ascii_lowercase());
    }
    let mut seen_any = i > 0; // 消费了标签名或 `*`
    while i < b.len() {
        match b[i] {
            b'#' | b'.' => {
                let is_id = b[i] == b'#';
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'#' && b[i] != b'.' {
                    i += 1;
                }
                if start == i {
                    return None; // `#` / `.` 后面为空
                }
                let name = &token[start..i];
                if is_id {
                    if c.id.is_some() {
                        return None; // 重复 id，非 CSS 合法写法
                    }
                    c.id = Some(name.to_string());
                } else {
                    c.classes.push(name.to_string());
                }
                seen_any = true;
            }
            _ => return None, // 属性选择器 / 伪类等不在子集内 → 非法
        }
    }
    if !seen_any {
        return None;
    }
    Some(c)
}

/// 解析完整选择器；非法 → `None`（调用方返回空结果，不抛错）。
pub fn parse_selector(s: &str) -> Option<Selector> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(tokens.len());
    for t in tokens {
        parts.push(parse_compound(t)?);
    }
    Some(Selector { parts })
}

impl Compound {
    /// `tag` 为宿主返回的标签名（大小写不敏感比）；
    /// `id` / `class_attr` 取自对应属性（大小写敏感）。
    pub fn matches(
        &self,
        tag: &str,
        id: Option<&str>,
        class_attr: Option<&str>,
    ) -> bool {
        if let Some(t) = &self.tag {
            if tag.to_ascii_lowercase() != *t {
                return false;
            }
        }
        if let Some(want) = &self.id {
            if id != Some(want.as_str()) {
                return false;
            }
        }
        if !self.classes.is_empty() {
            let have: Vec<&str> =
                class_attr.unwrap_or("").split_whitespace().collect();
            if !self
                .classes
                .iter()
                .all(|w| have.iter().any(|h| h == w))
            {
                return false;
            }
        }
        true
    }
}

fn node_matches(host: &dyn DomHost, id: u64, c: &Compound) -> bool {
    c.matches(
        &host.tag_name(id),
        host.get_attribute(id, "id").as_deref(),
        host.get_attribute(id, "class").as_deref(),
    )
}

fn walk_descendants(host: &dyn DomHost, id: u64, out: &mut Vec<u64>) {
    for c in host.children(id) {
        out.push(c);
        walk_descendants(host, c, out);
    }
}

/// 在 `root` 子树中按文档序查找匹配选择器的节点 id。
/// 非法选择器返回空 Vec（浏览器行为）。
pub fn select(host: &dyn DomHost, root: u64, selector: &str) -> Vec<u64> {
    let sel = match parse_selector(selector) {
        Some(s) => s,
        None => return Vec::new(),
    };
    // 逐层收窄 frontier：每层的候选是上一层节点的后代中匹配本 part 者。
    // 同一节点只保留一次（`div div` 嵌套时不重复）。
    let mut frontier = vec![root];
    for (i, part) in sel.parts.iter().enumerate() {
        let mut next = Vec::new();
        let mut seen = HashSet::new();
        for &id in &frontier {
            let mut pool = Vec::new();
            if i == 0 {
                pool.push(id); // 第一层：root 自身也可匹配
            }
            walk_descendants(host, id, &mut pool);
            for n in pool {
                if seen.insert(n) && node_matches(host, n, part) {
                    next.push(n);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    frontier
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::MockDom;
    use std::rc::Rc;

    fn sample() -> Rc<MockDom> {
        let dom = Rc::new(MockDom::new());
        let body = dom.add_element(0, "body", &[], "");
        let wrap = dom.add_element(body, "div", &[("id", "a")], "");
        dom.add_element(wrap, "span", &[("class", "b")], "x");
        dom.add_element(wrap, "span", &[("class", "b c")], "y");
        dom.add_element(body, "div", &[("class", "x")], "");
        dom
    }

    fn sel(dom: &MockDom, s: &str) -> Vec<u64> {
        select(dom, dom.document_id(), s)
    }

    #[test]
    fn tag_id_class_basics() {
        let dom = sample();
        assert_eq!(sel(&dom, "span").len(), 2);
        assert_eq!(sel(&dom, "#a").len(), 1);
        assert_eq!(sel(&dom, ".b").len(), 2);
        assert_eq!(sel(&dom, ".c").len(), 1);
        assert_eq!(sel(&dom, "zzz").len(), 0);
    }

    #[test]
    fn compound_selector() {
        let dom = sample();
        assert_eq!(sel(&dom, "div.x").len(), 1);
        assert_eq!(sel(&dom, "span.b").len(), 2);
        assert_eq!(sel(&dom, "span.c").len(), 1);
        assert_eq!(sel(&dom, "span#nope").len(), 0);
    }

    #[test]
    fn descendant_combinator() {
        let dom = sample();
        assert_eq!(sel(&dom, "#a .b").len(), 2);
        assert_eq!(sel(&dom, "div span").len(), 2);
        assert_eq!(sel(&dom, "body div.x").len(), 1);
        // 不在 #a 下的 .x 不应被命中
        assert_eq!(sel(&dom, "#a .x").len(), 0);
    }

    #[test]
    fn no_duplicates_on_nested() {
        let dom = Rc::new(MockDom::new());
        let outer = dom.add_element(0, "div", &[], "");
        let inner = dom.add_element(outer, "div", &[], "");
        dom.add_element(inner, "span", &[], "");
        // span 有两个 div 祖先，只应出现一次
        assert_eq!(select(&*dom, dom.document_id(), "div span").len(), 1);
    }

    #[test]
    fn invalid_selectors_return_empty() {
        let dom = sample();
        for bad in ["", "   ", "div[", "#", ".", "div#", ">", "div > span", "a:hover"] {
            assert!(sel(&dom, bad).is_empty(), "should be empty: {:?}", bad);
        }
    }

    #[test]
    fn parse_ok_and_bad() {
        assert!(parse_selector("div.x#y .z").is_some());
        assert!(parse_selector("*").is_some());
        assert!(parse_selector("DIV.X").unwrap().parts[0].tag.as_deref() == Some("div"));
        assert!(parse_selector("div##a").is_none());
        assert!(parse_selector("").is_none());
    }
}
