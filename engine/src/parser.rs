//! Yousj browser engine — tree builder (stack-based, pragmatic).

use crate::dom::Node;
use crate::tokenizer::{c_is_void, c_tag_eq_str, Token, Tokenizer};

/// Parse HTML into an arena of nodes. Returns (arena, root_index).
pub fn parse(html: &str) -> (Vec<Node>, usize) {
    let mut tok = Tokenizer::new(html);
    let mut arena: Vec<Node> = Vec::new();
    arena.push(Node::elem("#document"));
    let mut stack: Vec<usize> = vec![0];
    let mut html_open = false;
    let mut implied_html: Option<usize> = None;

    // Lazily create the implied <html> element. Returns its index when created.
    let mut ensure_html = |arena: &mut Vec<Node>,
                           stack: &mut Vec<usize>|
     -> Option<usize> {
        if !html_open {
            let h = arena.len();
            arena.push(Node::elem("html"));
            arena[0].children.push(h);
            stack.push(h);
            html_open = true;
            Some(h)
        } else {
            None
        }
    };

    loop {
        let token = tok.next_token();
        match token {
            Token::Eof => break,
            Token::Doctype | Token::Comment(_) => {}
            Token::Text(s) => {
                if s.is_empty() {
                    continue;
                }
                if let Some(h) = ensure_html(&mut arena, &mut stack) {
                    implied_html = Some(h);
                }
                let parent = *stack.last().unwrap();
                // Merge into a trailing text node to avoid fragmentation.
                if let Some(&last) = arena[parent].children.last() {
                    if arena[last].tag.is_none() {
                        arena[last].text.push_str(&s);
                        continue;
                    }
                }
                let idx = arena.len();
                arena.push(Node::text(&s));
                arena[parent].children.push(idx);
            }
            Token::StartTag {
                name,
                attrs,
                self_closing,
            } => {
                if let Some(h) = ensure_html(&mut arena, &mut stack) {
                    implied_html = Some(h);
                }
                // A real <html> tag reuses the implied one instead of nesting
                // a second <html> inside it.
                if c_tag_eq_str(&name, "html") {
                    if let Some(h) = implied_html {
                        if *stack.last().unwrap() == h
                            && arena[h].children.is_empty()
                        {
                            for (k, v) in attrs {
                                if !arena[h]
                                    .attrs
                                    .iter()
                                    .any(|(ek, _)| ek.eq_ignore_ascii_case(&k))
                                {
                                    arena[h].attrs.push((k, v));
                                }
                            }
                            continue;
                        }
                    }
                }
                // Auto-close an open <p> when a new <p> starts.
                if c_tag_eq_str(&name, "p") {
                    if let Some(&t) = stack.last() {
                        if arena[t].tag.as_deref() == Some("p") {
                            stack.pop();
                        }
                    }
                }
                let idx = arena.len();
                let mut node = Node::elem(&name);
                node.attrs = attrs;
                arena.push(node);
                let parent = *stack.last().unwrap();
                arena[parent].children.push(idx);
                if !c_is_void(name.as_bytes()) && !self_closing {
                    stack.push(idx);
                }
            }
            Token::EndTag { name } => {
                let mut found: Option<usize> = None;
                for (i, &idx) in stack.iter().enumerate().rev() {
                    if idx == 0 {
                        break; // never pop #document
                    }
                    if let Some(ref t) = arena[idx].tag {
                        if c_tag_eq_str(t, &name) {
                            found = Some(i);
                            break;
                        }
                    }
                }
                if let Some(i) = found {
                    stack.truncate(i);
                }
            }
        }
    }

    (arena, 0)
}
