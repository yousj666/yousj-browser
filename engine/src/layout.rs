//! Minimal layout / box model for the Yousj engine.
//!
//! Purpose: give AI callers *coordinates* for elements (so a mouse can
//! move/click/drag), without a full CSS engine. This is deliberately a
//! "minimum usable" implementation, not a browser-grade layout:
//!
//! Supported:
//! - Block layout: block elements stack vertically; nested blocks get
//!   the parent content width minus margins.
//! - Inline flow: text + inline elements flow left-to-right, wrap at the
//!   content edge; inline-block elements (button/input/select/textarea/
//!   img) get intrinsic sizes.
//! - `style` attributes, px only: display, width, height, font-size,
//!   margin / margin-* , padding / padding-* (1-4 value shorthands).
//! - Tag defaults: block vs inline vs inline-block; body 8px margin;
//!   p / headings / lists / blockquote / figure get 16px vertical margin.
//! - Text measurement is ESTIMATED (no font metrics): CJK chars 1.0em,
//!   other chars 0.5em; line height 1.2em.
//!
//! NOT supported (reported honestly, not silently faked):
//! - % / em / rem / vw / auto sizes (treated as auto, except bare "0").
//! - float, position (absolute/fixed/relative offsets), flex, grid,
//!   table layout (table parts fall back to block), borders, box-sizing,
//!   line-breaking inside a single inline element's box (a long inline
//!   element wraps as a whole to the next line; its box is the first
//!   fragment only), real font metrics, images without width/height
//!   attributes (fallback 200x150).
//! - Margins do NOT collapse (each block's top+bottom margins both apply).
//!
//! Coordinate contract with `yousj_interactive`: both walkers count
//! EVERY element node per tag in document order (skipping the same
//! head/script/style/noscript/template subtrees), so `(tag, tag_index)`
//! joins a layout box to its interactive-list entry. A layout pass
//! first assigns tag indices in a separate pre-pass, so box emission
//! (which skips nested inline descendants) cannot shift indices.

use crate::dom::Node;
use crate::Doc;
use std::collections::HashMap;

const MAX_BOXES: usize = 5000;
const MAX_DEPTH: usize = 200;

#[derive(Clone, Copy, PartialEq)]
enum Display {
    Block,
    Inline,
    InlineBlock,
    None,
}

#[derive(Clone)]
struct Style {
    display: Display,
    width: Option<f32>,
    height: Option<f32>,
    // top, right, bottom, left
    margin: [f32; 4],
    padding: [f32; 4],
    font_size: Option<f32>,
}

fn parse_px(s: &str) -> Option<f32> {
    let t = s.trim();
    if t == "0" {
        return Some(0.0);
    }
    t.strip_suffix("px")
        .and_then(|v| v.trim().parse::<f32>().ok())
}

fn parse_sides(style: &str, name: &str) -> [f32; 4] {
    // Returns [top, right, bottom, left]; shorthand first, longhands win.
    let mut vals = [0.0f32; 4];
    let get = |key: &str| -> Option<&str> {
        style.split(';').find_map(|decl| {
            let (k, v) = decl.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(key) {
                Some(v.trim())
            } else {
                None
            }
        })
    };
    if let Some(v) = get(name) {
        let parts: Vec<f32> = v
            .split_whitespace()
            .filter_map(parse_px)
            .collect();
        match parts.len() {
            1 => vals = [parts[0]; 4],
            2 => vals = [parts[0], parts[1], parts[0], parts[1]],
            3 => vals = [parts[0], parts[1], parts[2], parts[1]],
            4 => vals = [parts[0], parts[1], parts[2], parts[3]],
            _ => {}
        }
    }
    for (i, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        let key = format!("{}-{}", name, side);
        if let Some(v) = get(&key) {
            if let Some(px) = parse_px(v) {
                vals[i] = px;
            }
        }
    }
    vals
}

fn default_display(tag: &str) -> Display {
    match tag {
        "head" | "script" | "style" | "noscript" | "template" | "meta"
        | "link" | "title" | "base" => Display::None,
        "html" | "body" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5"
        | "h6" | "form" | "ul" | "ol" | "li" | "dl" | "dt" | "dd" | "table"
        | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "section"
        | "article" | "header" | "footer" | "nav" | "main" | "aside"
        | "figure" | "figcaption" | "blockquote" | "pre" | "hr" | "fieldset"
        | "address" | "details" | "summary" => Display::Block,
        "button" | "input" | "select" | "textarea" | "img" | "video"
        | "audio" | "canvas" | "iframe" => Display::InlineBlock,
        _ => Display::Inline,
    }
}

fn default_margins(tag: &str) -> [f32; 4] {
    match tag {
        "body" => [8.0, 8.0, 8.0, 8.0],
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
        | "blockquote" | "figure" => [16.0, 0.0, 16.0, 0.0],
        _ => [0.0; 4],
    }
}

fn style_of(n: &Node) -> Style {
    let tag = n.tag.as_deref().unwrap_or("");
    let style = n.attr("style").unwrap_or("");
    let mut display = default_display(tag);
    if n.attr("hidden").is_some() {
        display = Display::None;
    }
    for decl in style.split(';') {
        if let Some((k, v)) = decl.split_once(':') {
            if k.trim().eq_ignore_ascii_case("display") {
                display = match v.trim().to_ascii_lowercase().as_str() {
                    "block" => Display::Block,
                    "inline" => Display::Inline,
                    "inline-block" | "inline-flex" | "inline-grid" => {
                        Display::InlineBlock
                    }
                    "none" => Display::None,
                    // flex/grid/table/...: fall back to block (documented).
                    _ => Display::Block,
                };
            }
        }
    }
    let get = |key: &str| -> Option<&str> {
        style.split(';').find_map(|decl| {
            let (k, v) = decl.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(key) {
                Some(v.trim())
            } else {
                None
            }
        })
    };
    Style {
        display,
        width: get("width").and_then(parse_px),
        height: get("height").and_then(parse_px),
        margin: {
            let mut m = default_margins(tag);
            let parsed = parse_sides(style, "margin");
            // parse_sides returns zeros when absent; only override when
            // the property is actually present.
            if style.to_ascii_lowercase().contains("margin") {
                m = parsed;
            }
            m
        },
        padding: parse_sides(style, "padding"),
        font_size: get("font-size").and_then(parse_px),
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{1100}'..='\u{11ff}'
        | '\u{2e80}'..='\u{9fff}'
        | '\u{ac00}'..='\u{d7af}'
        | '\u{f900}'..='\u{faff}'
        | '\u{ff00}'..='\u{ff60}')
}

fn char_w(c: char, font: f32) -> f32 {
    if is_cjk(c) {
        font
    } else {
        font * 0.5
    }
}

fn text_width(text: &str, font: f32) -> f32 {
    text.chars().map(|c| char_w(c, font)).sum()
}

fn node_text(d: &Doc, idx: usize, out: &mut String) {
    let n = &d.arena[idx];
    if let Some(t) = n.tag.as_deref() {
        // CSS/JS source is not visible text; keep it out of box labels.
        if t == "script" || t == "style" {
            return;
        }
    }
    if n.tag.is_none() {
        out.push_str(&n.text);
    }
    for &c in &n.children {
        node_text(d, c, out);
    }
}

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

fn is_skipped(tag: &str) -> bool {
    matches!(tag, "head" | "script" | "style" | "noscript" | "template")
}

struct BoxOut {
    tag: String,
    text: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    tag_index: usize,
    html_id: String,
    href: String,
    /// True when no descendant element got its own box (a "leaf" box:
    /// its text is its own, not a container's concatenation). Screenshot
    /// labels are drawn only for leaf boxes, so container labels (html /
    /// body / div, whose text is all descendant text) don't overlap
    /// their children's labels.
    leaf: bool,
}

struct Layouter<'a> {
    d: &'a Doc,
    counts: HashMap<String, usize>,
    tag_index_of: HashMap<usize, usize>,
    boxes: Vec<BoxOut>,
    truncated: bool,
}

impl<'a> Layouter<'a> {
    fn new(d: &'a Doc) -> Self {
        Layouter {
            d,
            counts: HashMap::new(),
            tag_index_of: HashMap::new(),
            boxes: Vec::new(),
            truncated: false,
        }
    }

    /// Pre-pass: assign tag_index to every element node in document
    /// order, skipping the same subtrees as yousj_interactive.
    fn count_pass(&mut self, idx: usize) {
        let (tag_opt, children) = {
            let n = &self.d.arena[idx];
            (n.tag.clone(), n.children.clone())
        };
        match tag_opt {
            None => {
                for c in children {
                    self.count_pass(c);
                }
            }
            Some(tag) => {
                if is_skipped(&tag) {
                    return;
                }
                let ti = *self.counts.entry(tag.clone()).or_insert(0);
                self.counts.insert(tag.clone(), ti + 1);
                self.tag_index_of.insert(idx, ti);
                for c in children {
                    self.count_pass(c);
                }
            }
        }
    }

    fn emit(&mut self, idx: usize, x: f32, y: f32, w: f32, h: f32,
            leaf: bool) {
        if self.boxes.len() >= MAX_BOXES {
            self.truncated = true;
            return;
        }
        let n = &self.d.arena[idx];
        let tag = n.tag.clone().unwrap_or_default();
        let mut t = String::new();
        node_text(self.d, idx, &mut t);
        let text: String = t
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        self.boxes.push(BoxOut {
            tag,
            text,
            x: x.round(),
            y: y.round(),
            width: w.max(0.0).round(),
            height: h.max(0.0).round(),
            tag_index: *self.tag_index_of.get(&idx).unwrap_or(&0),
            html_id: n.attr("id").unwrap_or("").to_string(),
            href: n.attr("href").unwrap_or("").to_string(),
            leaf,
        });
    }

    /// Intrinsic size for inline-block elements (content box, no padding).
    fn intrinsic(&self, idx: usize, font: f32) -> (f32, f32) {
        let n = &self.d.arena[idx];
        let tag = n.tag.as_deref().unwrap_or("");
        match tag {
            "button" => {
                let mut t = String::new();
                node_text(self.d, idx, &mut t);
                let tw = text_width(t.trim(), font);
                (tw + 16.0, (font * 1.2).max(24.0))
            }
            "input" => {
                let typ = n
                    .attr("type")
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                match typ.as_str() {
                    "checkbox" | "radio" => (16.0, 16.0),
                    "submit" | "button" | "reset" => {
                        let v = n.attr("value").unwrap_or("");
                        (text_width(v, font) + 16.0, 24.0)
                    }
                    _ => {
                        let w = n
                            .attr("size")
                            .and_then(|s| s.parse::<f32>().ok())
                            .map(|cols| cols * font * 0.5 + 8.0)
                            .or_else(|| {
                                n.attr("width").and_then(parse_px)
                            })
                            .unwrap_or(160.0);
                        (w, 24.0)
                    }
                }
            }
            "select" => {
                let mut best: f32 = 0.0;
                for &c in &n.children {
                    let cn = &self.d.arena[c];
                    let mut opts = Vec::new();
                    if cn.tag.as_deref() == Some("option") {
                        opts.push(c);
                    } else if cn.tag.as_deref() == Some("optgroup") {
                        for &g in &cn.children {
                            if self.d.arena[g].tag.as_deref() == Some("option")
                            {
                                opts.push(g);
                            }
                        }
                    }
                    for o in opts {
                        let mut t = String::new();
                        node_text(self.d, o, &mut t);
                        best = best.max(text_width(t.trim(), font));
                    }
                }
                (best + 30.0, 24.0)
            }
            "textarea" => {
                let cols = n
                    .attr("cols")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(20.0);
                let rows = n
                    .attr("rows")
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(2.0);
                (cols * font * 0.5 + 8.0, rows * font * 1.2 + 8.0)
            }
            "img" => {
                let w = n.attr("width").and_then(parse_px).unwrap_or(200.0);
                let h = n.attr("height").and_then(parse_px).unwrap_or(150.0);
                (w, h)
            }
            _ => (0.0, font * 1.2),
        }
    }

    /// Single-line measure of an inline element's content width
    /// (descendant text + inline-block descendants + padding).
    /// No internal line breaking: long content measures wide.
    fn measure_inline(&self, idx: usize, font: f32) -> f32 {
        let n = &self.d.arena[idx];
        let st = style_of(n);
        let pad = st.padding[1] + st.padding[3];
        let mut w = pad;
        for &c in &n.children {
            let cn = &self.d.arena[c];
            match cn.tag.as_deref() {
                None => {
                    w += text_width(&cn.text, font);
                }
                Some(t) => {
                    let cst = style_of(cn);
                    match cst.display {
                        Display::None => {}
                        Display::InlineBlock => {
                            let (iw, _) = self.intrinsic(c, font);
                            w += iw + cst.padding[1] + cst.padding[3];
                        }
                        Display::Block => {
                            // Invalid nesting (block inside inline):
                            // measure its text as inline flow.
                            w += self.measure_inline(c, font);
                        }
                        Display::Inline => {
                            w += self.measure_inline(c, font);
                        }
                    }
                    let _ = t;
                }
            }
        }
        w
    }

    /// Lay out the children of a block container. Returns content height.
    fn layout_flow(
        &mut self,
        container: usize,
        cx: f32,
        mut cy: f32,
        cwidth: f32,
        font: f32,
        depth: usize,
    ) -> f32 {
        if depth > MAX_DEPTH {
            self.truncated = true;
            return 0.0;
        }
        let start_y = cy;
        let children = self.d.arena[container].children.clone();
        // Pending inline run: (kind, idx-or-text, font)
        enum Item {
            Text(String, f32),
            Elem(usize, f32),
        }
        let mut run: Vec<Item> = Vec::new();
        let flush = |lay: &mut Self, run: &mut Vec<Item>, cy: &mut f32| {
            if run.is_empty() {
                return;
            }
            let right = cx + cwidth;
            let mut lx = cx;
            let mut ly = *cy;
            let mut lh: f32 = 0.0;
            for item in run.drain(..) {
                match item {
                    Item::Text(t, f) => {
                        let line_h = f * 1.2;
                        for ch in t.chars() {
                            if ch == ' ' && lx <= cx {
                                continue; // no leading space on fresh line
                            }
                            let cw = char_w(ch, f);
                            if lx + cw > right && lx > cx {
                                ly += lh.max(line_h);
                                lx = cx;
                                lh = line_h;
                            }
                            lh = lh.max(line_h);
                            lx += cw;
                        }
                    }
                    Item::Elem(eidx, f) => {
                        let en = &lay.d.arena[eidx];
                        let est = style_of(en);
                        let (w, h) = match est.display {
                            Display::InlineBlock => {
                                let (iw, ih) = lay.intrinsic(eidx, f);
                                (
                                    iw + est.padding[1] + est.padding[3],
                                    ih + est.padding[0] + est.padding[2],
                                )
                            }
                            _ => {
                                let mw = lay.measure_inline(eidx, f);
                                (mw, f * 1.2 + est.padding[0] + est.padding[2])
                            }
                        };
                        if lx + w > right && lx > cx {
                            ly += lh.max(f * 1.2);
                            lx = cx;
                            lh = f * 1.2;
                        }
                        lh = lh.max(h);
                        // Inline / inline-block boxes never contain
                        // descendant boxes (measure doesn't emit), so
                        // they are always leaves.
                        lay.emit(eidx, lx, ly, w, h, true);
                        lx += w;
                    }
                }
            }
            *cy = ly + lh;
        };
        for c in children {
            let cn = &self.d.arena[c];
            match cn.tag.as_deref() {
                None => {
                    if !cn.text.trim().is_empty() {
                        run.push(Item::Text(cn.text.clone(), font));
                    }
                }
                Some(t) => {
                    if is_skipped(t) {
                        continue;
                    }
                    let st = style_of(cn);
                    match st.display {
                        Display::None => {}
                        Display::Block => {
                            flush(self, &mut run, &mut cy);
                            let h = self.layout_block(c, cx, cy, cwidth, font, depth + 1);
                            cy += h;
                        }
                        _ => {
                            run.push(Item::Elem(c, font));
                        }
                    }
                }
            }
        }
        flush(self, &mut run, &mut cy);
        cy - start_y
    }

    /// Lay out one block element. Returns margin_top + height +
    /// margin_bottom (the amount the parent cursor advances).
    fn layout_block(
        &mut self,
        idx: usize,
        container_x: f32,
        cursor_y: f32,
        container_w: f32,
        parent_font: f32,
        depth: usize,
    ) -> f32 {
        let n = &self.d.arena[idx];
        let st = style_of(n);
        let font = st.font_size.unwrap_or(parent_font);
        let m = st.margin;
        let p = st.padding;
        let x = container_x + m[3];
        let mut w = match st.width {
            Some(v) => v,
            None => container_w - m[3] - m[1],
        };
        if w < 0.0 {
            w = 0.0;
        }
        let y = cursor_y + m[0];
        let cx = x + p[3];
        let cw = (w - p[3] - p[1]).max(0.0);
        let before_boxes = self.boxes.len();
        let content_h = self.layout_flow(idx, cx, y + p[0], cw, font, depth);
        let leaf = self.boxes.len() == before_boxes;
        let h = match st.height {
            Some(v) => v,
            None => content_h + p[0] + p[2],
        };
        self.emit(idx, x, y, w, h, leaf);
        m[0] + h + m[2]
    }
}

/// Build the layout JSON for a document.
///
/// `viewport_width` <= 0 falls back to 1280. Output:
/// `{"viewport_width":W,"truncated":bool,"boxes":[{...}]}`
/// with boxes in layout order (roughly document order for blocks).
/// Each box carries `leaf` (true when no descendant element got its
/// own box — screenshot labels are drawn only for leaves).
pub fn layout_json(d: &Doc, viewport_width: i32) -> String {
    let vw = if viewport_width > 0 {
        viewport_width as f32
    } else {
        1280.0
    };
    let mut lay = Layouter::new(d);
    lay.count_pass(d.root);
    // Root: html is a block child of the virtual viewport container.
    let mut total = 0.0f32;
    let root_children = d.arena[d.root].children.clone();
    for c in root_children {
        let cn = &d.arena[c];
        if let Some(t) = cn.tag.as_deref() {
            if is_skipped(t) {
                continue;
            }
            // html/body/etc: treat top-level elements as blocks.
            total += lay.layout_block(c, 0.0, total, vw, 16.0, 0);
        }
    }
    let mut out = String::new();
    out.push_str("{\"viewport_width\":");
    out.push_str(&format!("{}", vw as i32));
    out.push_str(",\"truncated\":");
    out.push_str(if lay.truncated { "true" } else { "false" });
    out.push_str(",\"boxes\":[");
    for (i, b) in lay.boxes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"tag\":");
        json_escape(&b.tag, &mut out);
        out.push_str(",\"text\":");
        json_escape(&b.text, &mut out);
        out.push_str(",\"x\":");
        out.push_str(&format!("{}", b.x as i32));
        out.push_str(",\"y\":");
        out.push_str(&format!("{}", b.y as i32));
        out.push_str(",\"width\":");
        out.push_str(&format!("{}", b.width as i32));
        out.push_str(",\"height\":");
        out.push_str(&format!("{}", b.height as i32));
        out.push_str(",\"tag_index\":");
        out.push_str(&b.tag_index.to_string());
        out.push_str(",\"html_id\":");
        json_escape(&b.html_id, &mut out);
        out.push_str(",\"href\":");
        json_escape(&b.href, &mut out);
        out.push_str(",\"leaf\":");
        out.push_str(if b.leaf { "true" } else { "false" });
        out.push('}');
    }
    out.push_str("]}");
    out
}
