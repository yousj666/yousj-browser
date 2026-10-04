//! Yousj browser engine — DOM (arena-allocated tree).

#[derive(Debug, Clone)]
pub struct Node {
    /// Element tag name (lowercased). `None` means this is a text node.
    pub tag: Option<String>,
    /// Text content (only for text nodes).
    pub text: String,
    pub attrs: Vec<(String, String)>,
    /// Indices into the arena.
    pub children: Vec<usize>,
}

impl Node {
    pub fn elem(tag: &str) -> Self {
        Node {
            tag: Some(tag.to_string()),
            text: String::new(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    pub fn text(t: &str) -> Self {
        Node {
            tag: None,
            text: t.to_string(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
