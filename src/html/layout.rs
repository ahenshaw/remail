//! Lowers a parsed HTML tree into a flat list of blocks the renderer can walk
//! without recursion.
//!
//! The model is deliberately narrower than CSS: emails use a handful of
//! constructs (paragraphs, headings, lists, quotes, images, and tables used
//! both for data and for layout), and matching those well beats approximating
//! a full box model badly.

use super::dom::{Element, Node};

/// Character styling carried down the tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub monospace: bool,
    /// Multiplier on the base body size.
    pub scale: f32,
    /// Explicit `color:`/`<font color>`, as sRGB.
    pub color: Option<[u8; 3]>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            monospace: false,
            scale: 1.0,
            color: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Inline {
    Text {
        text: String,
        style: Style,
        link: Option<String>,
    },
    Image {
        src: String,
        alt: String,
        width: Option<u32>,
        height: Option<u32>,
    },
    /// An explicit `<br>`.
    Break,
}

#[derive(Debug, Clone)]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
        quote_depth: u8,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    ListItem {
        depth: u8,
        marker: String,
        inlines: Vec<Inline>,
        quote_depth: u8,
    },
    /// Preformatted text, rendered without reflowing.
    Pre {
        text: String,
        quote_depth: u8,
    },
    Rule,
    /// Cells are themselves inline runs; nested tables are flattened into the
    /// cell that contains them.
    Table {
        rows: Vec<Row>,
        quote_depth: u8,
    },
}

#[derive(Debug, Clone)]
pub struct Row {
    pub header: bool,
    pub cells: Vec<Vec<Inline>>,
}

/// The whole message body, ready to draw.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

impl Document {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Lowers parsed nodes into blocks.
pub fn lower(nodes: &[Node]) -> Document {
    let mut builder = Builder::default();
    builder.walk_children(nodes, Style::default(), None);
    builder.flush();
    Document { blocks: builder.blocks }
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    pending: Vec<Inline>,
    quote_depth: u8,
    /// Open lists: `(is_ordered, next_number)`.
    lists: Vec<(bool, u32)>,
    /// Set when the pending run belongs to a list item rather than a paragraph.
    list_item: Option<(u8, String)>,
}

impl Builder {
    /// Ends the current inline run and emits it as a block.
    fn flush(&mut self) {
        if self.pending.iter().all(inline_is_blank) {
            self.pending.clear();
            self.list_item = None;
            return;
        }
        let inlines = std::mem::take(&mut self.pending);
        let quote_depth = self.quote_depth;
        match self.list_item.take() {
            Some((depth, marker)) => {
                self.blocks.push(Block::ListItem { depth, marker, inlines, quote_depth })
            }
            None => self.blocks.push(Block::Paragraph { inlines, quote_depth }),
        }
    }

    fn walk_children(&mut self, nodes: &[Node], style: Style, link: Option<&str>) {
        for node in nodes {
            self.walk(node, style, link);
        }
    }

    fn walk(&mut self, node: &Node, style: Style, link: Option<&str>) {
        match node {
            Node::Text(text) => self.push_text(text, style, link),
            Node::Element(element) => self.walk_element(element, style, link),
        }
    }

    fn push_text(&mut self, text: &str, style: Style, link: Option<&str>) {
        let collapsed = collapse_whitespace(text);
        if collapsed.is_empty() {
            return;
        }
        // Merge into the previous run when nothing about it changed; this
        // keeps galley counts low on heavily nested marketing HTML.
        if let Some(Inline::Text { text: previous, style: prev_style, link: prev_link }) =
            self.pending.last_mut()
            && *prev_style == style
            && prev_link.as_deref() == link
        {
            previous.push_str(&collapsed);
            return;
        }
        self.pending.push(Inline::Text { text: collapsed, style, link: link.map(str::to_string) });
    }

    fn walk_element(&mut self, element: &Element, style: Style, link: Option<&str>) {
        // Hidden content is hidden on purpose: preheader text, responsive
        // alternates, Outlook-only blocks. Rendering it is just noise.
        if is_hidden(element) {
            return;
        }
        let style = apply_element_style(element, style);

        match element.name.as_str() {
            "br" => self.pending.push(Inline::Break),
            "hr" => {
                self.flush();
                self.blocks.push(Block::Rule);
            }
            "img" => {
                let Some(src) = element.attr("src").filter(|s| !s.is_empty()) else {
                    return;
                };
                self.pending.push(Inline::Image {
                    src: src.to_string(),
                    alt: element.attr("alt").unwrap_or_default().to_string(),
                    width: element.attr("width").and_then(parse_dimension),
                    height: element.attr("height").and_then(parse_dimension),
                });
            }

            "p" | "div" | "figure" | "figcaption" | "dl" | "dd" | "dt" | "caption" => {
                self.flush();
                self.walk_children(&element.children, style, link);
                self.flush();
            }

            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                let level = element.name.as_bytes()[1] - b'0';
                self.walk_children(&element.children, style, link);
                let inlines = std::mem::take(&mut self.pending);
                if !inlines.iter().all(inline_is_blank) {
                    self.blocks.push(Block::Heading { level, inlines });
                }
                self.list_item = None;
            }

            "ul" | "ol" => {
                self.flush();
                let ordered = element.name == "ol";
                let start = element.attr("start").and_then(|s| s.parse().ok()).unwrap_or(1);
                self.lists.push((ordered, start));
                self.walk_children(&element.children, style, link);
                self.flush();
                self.lists.pop();
            }

            "li" => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1) as u8;
                let marker = match self.lists.last_mut() {
                    Some((true, counter)) => {
                        let marker = format!("{counter}.");
                        *counter += 1;
                        marker
                    }
                    // Alternate the bullet by depth, as browsers do.
                    _ => crate::ui::icons::BULLETS[depth as usize % 3].to_string(),
                };
                self.list_item = Some((depth, marker));
                self.walk_children(&element.children, style, link);
                self.flush();
            }

            "blockquote" | "q" => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_add(1);
                self.walk_children(&element.children, style, link);
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }

            "pre" => {
                self.flush();
                let text = text_content(&element.children);
                if !text.trim().is_empty() {
                    self.blocks.push(Block::Pre { text, quote_depth: self.quote_depth });
                }
            }

            "table" => {
                self.flush();
                let rows = collect_rows(element, style, link);
                if !rows.is_empty() {
                    self.blocks.push(Block::Table { rows, quote_depth: self.quote_depth });
                }
            }
            // Rows and cells outside a table: flow them instead of dropping.
            "tr" => {
                self.flush();
                self.walk_children(&element.children, style, link);
                self.flush();
            }

            "a" => {
                let href = element.attr("href").filter(|h| !h.is_empty());
                self.walk_children(&element.children, style, href.or(link));
            }

            // Everything else contributes styling only.
            _ => self.walk_children(&element.children, style, link),
        }
    }
}

/// Builds table rows, flattening anything that is not a cell into the row it
/// appears in.
fn collect_rows(table: &Element, style: Style, link: Option<&str>) -> Vec<Row> {
    let mut rows = Vec::new();
    collect_rows_into(table, style, link, &mut rows);
    rows
}

fn collect_rows_into(element: &Element, style: Style, link: Option<&str>, rows: &mut Vec<Row>) {
    for child in &element.children {
        let Node::Element(child) = child else { continue };
        match child.name.as_str() {
            "thead" | "tbody" | "tfoot" => collect_rows_into(child, style, link, rows),
            "tr" => {
                let mut cells = Vec::new();
                let mut header = false;
                for cell in &child.children {
                    let Node::Element(cell) = cell else { continue };
                    if cell.name != "td" && cell.name != "th" {
                        continue;
                    }
                    header |= cell.name == "th";

                    let mut builder = Builder::default();
                    let cell_style = apply_element_style(cell, style);
                    builder.walk_children(&cell.children, cell_style, link);
                    builder.flush();
                    cells.push(flatten_blocks(builder.blocks));
                }
                if !cells.is_empty() {
                    rows.push(Row { header, cells });
                }
            }
            _ => {}
        }
    }
}

/// Collapses the blocks inside a table cell into one inline run, inserting
/// breaks where the block boundaries were.
fn flatten_blocks(blocks: Vec<Block>) -> Vec<Inline> {
    let mut out = Vec::new();
    for block in blocks {
        if !out.is_empty() {
            out.push(Inline::Break);
        }
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                out.extend(inlines)
            }
            Block::ListItem { marker, inlines, .. } => {
                out.push(Inline::Text {
                    text: format!("{marker} "),
                    style: Style::default(),
                    link: None,
                });
                out.extend(inlines);
            }
            Block::Pre { text, .. } => out.push(Inline::Text {
                text,
                style: Style { monospace: true, ..Style::default() },
                link: None,
            }),
            Block::Rule => {}
            // A nested table inside a cell is layout scaffolding; take its text.
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in row.cells {
                        out.extend(cell);
                    }
                    out.push(Inline::Break);
                }
            }
        }
    }
    out
}

/// Whether an element is hidden by its own attributes. Only inline styles are
/// consulted: there is no stylesheet, so a `class` tells us nothing.
fn is_hidden(element: &Element) -> bool {
    if element.attr("hidden").is_some() {
        return true;
    }
    let Some(declarations) = element.attr("style") else { return false };
    declarations.split(';').any(|declaration| {
        let Some((property, value)) = declaration.split_once(':') else { return false };
        let value = value.trim().to_ascii_lowercase();
        // `!important` and stray spacing are both common here.
        let value = value.split('!').next().unwrap_or("").trim();
        match property.trim().to_ascii_lowercase().as_str() {
            "display" => value == "none",
            "visibility" => value == "hidden" || value == "collapse",
            _ => false,
        }
    })
}

fn apply_element_style(element: &Element, mut style: Style) -> Style {
    match element.name.as_str() {
        "b" | "strong" | "th" => style.bold = true,
        "i" | "em" | "cite" | "var" | "dfn" => style.italic = true,
        "u" | "ins" => style.underline = true,
        "s" | "strike" | "del" => style.strike = true,
        "code" | "kbd" | "samp" | "tt" => style.monospace = true,
        "small" | "sub" | "sup" => style.scale *= 0.82,
        "mark" => style.bold = true,
        _ => {}
    }

    if let Some(color) = element.attr("color").and_then(parse_color) {
        style.color = Some(color);
    }
    if let Some(declarations) = element.attr("style") {
        apply_inline_css(declarations, &mut style);
    }
    style
}

/// Reads the handful of CSS properties worth honouring from a `style`
/// attribute. Anything else is ignored rather than half-applied.
fn apply_inline_css(declarations: &str, style: &mut Style) {
    for declaration in declarations.split(';') {
        let Some((property, value)) = declaration.split_once(':') else { continue };
        let property = property.trim().to_ascii_lowercase();
        let value = value.trim();
        match property.as_str() {
            "color" => {
                if let Some(color) = parse_color(value) {
                    style.color = Some(color);
                }
            }
            "font-weight" => {
                style.bold = matches!(value, "bold" | "bolder")
                    || value.parse::<u32>().is_ok_and(|w| w >= 600);
            }
            "font-style" => style.italic = value.starts_with("italic") || value == "oblique",
            "text-decoration" | "text-decoration-line" => {
                style.underline |= value.contains("underline");
                style.strike |= value.contains("line-through");
            }
            "font-family" => {
                style.monospace = value.to_ascii_lowercase().contains("monospace")
                    || value.to_ascii_lowercase().contains("courier");
            }
            _ => {}
        }
    }
}

/// Parses `#rgb`, `#rrggbb`, `rgb(r,g,b)` and the CSS basic color keywords.
pub fn parse_color(value: &str) -> Option<[u8; 3]> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        // Hex digits are ASCII by definition. The guard also keeps the
        // byte-indexed slicing below on character boundaries, which a sender
        // could otherwise break with something like `#\u{e9}1`.
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            3 => {
                let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
                Some([digit(0)?, digit(1)?, digit(2)?])
            }
            6 | 8 => {
                let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                Some([pair(0)?, pair(2)?, pair(4)?])
            }
            _ => None,
        };
    }
    if let Some(args) = value.strip_prefix("rgb(").or(value.strip_prefix("rgba(")) {
        let args = args.trim_end_matches(')');
        let mut parts = args.split(',').map(|p| p.trim().parse::<f32>().ok());
        let r = parts.next()??;
        let g = parts.next()??;
        let b = parts.next()??;
        return Some([r as u8, g as u8, b as u8]);
    }

    Some(match value.to_ascii_lowercase().as_str() {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "red" => [255, 0, 0],
        "green" => [0, 128, 0],
        "lime" => [0, 255, 0],
        "blue" => [0, 0, 255],
        "yellow" => [255, 255, 0],
        "cyan" | "aqua" => [0, 255, 255],
        "magenta" | "fuchsia" => [255, 0, 255],
        "gray" | "grey" => [128, 128, 128],
        "silver" => [192, 192, 192],
        "maroon" => [128, 0, 0],
        "olive" => [128, 128, 0],
        "navy" => [0, 0, 128],
        "purple" => [128, 0, 128],
        "teal" => [0, 128, 128],
        "orange" => [255, 165, 0],
        _ => return None,
    })
}

/// HTML dimension attributes may be `"120"`, `"120px"` or `"50%"`. Only
/// absolute values are usable without a containing block.
fn parse_dimension(value: &str) -> Option<u32> {
    let value = value.trim().trim_end_matches("px").trim();
    if value.ends_with('%') {
        return None;
    }
    value.parse().ok()
}

/// Concatenates descendant text without collapsing whitespace, for `<pre>`.
fn text_content(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Element(element) => {
                if element.name == "br" {
                    out.push('\n');
                } else {
                    out.push_str(&text_content(&element.children));
                }
            }
        }
    }
    out
}

/// Collapses runs of whitespace the way HTML does, keeping non-breaking
/// spaces intact.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_space = false;
    for ch in input.chars() {
        // Invisible formatting characters carry no meaning for this renderer
        // and have no glyph, so they would draw as boxes. Marketing mail uses
        // them by the hundred to pad preheaders.
        if is_invisible(ch) {
            continue;
        }
        if ch.is_whitespace() && ch != '\u{a0}' {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        // Leading whitespace in a run still separates from the previous run.
        if in_space && out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(ch);
    }
    if in_space && !out.is_empty() {
        out.push(' ');
    }
    out
}

/// Zero-width and formatting characters, which have no glyph to draw.
fn is_invisible(ch: char) -> bool {
    matches!(ch,
        '\u{034f}'            // combining grapheme joiner
        | '\u{200b}'..='\u{200f}' // zero-width spaces, joiners, bidi marks
        | '\u{2028}'..='\u{202e}' // line/paragraph separators, bidi overrides
        | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
        | '\u{feff}'              // byte order mark
    )
}

fn inline_is_blank(inline: &Inline) -> bool {
    match inline {
        Inline::Text { text, .. } => text.trim().is_empty(),
        Inline::Image { .. } => false,
        Inline::Break => true,
    }
}

/// Plain-text projection of a document, used for previews and reply quoting.
pub fn to_text(document: &Document) -> String {
    let mut out = String::new();
    for block in &document.blocks {
        match block {
            Block::Paragraph { inlines, .. } => push_inlines(&mut out, inlines),
            Block::Heading { inlines, .. } => push_inlines(&mut out, inlines),
            Block::ListItem { marker, inlines, depth, .. } => {
                out.push_str(&"  ".repeat(*depth as usize));
                out.push_str(marker);
                out.push(' ');
                push_inlines(&mut out, inlines);
            }
            Block::Pre { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::Rule => out.push_str("----\n"),
            Block::Table { rows, .. } => {
                for row in rows {
                    let cells: Vec<String> = row
                        .cells
                        .iter()
                        .map(|cell| {
                            let mut buffer = String::new();
                            push_inlines(&mut buffer, cell);
                            buffer.trim().to_string()
                        })
                        .collect();
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
            }
        }
        out.push('\n');
    }
    out
}

fn push_inlines(out: &mut String, inlines: &[Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Image { alt, .. } if !alt.is_empty() => {
                out.push('[');
                out.push_str(alt);
                out.push(']');
            }
            Inline::Image { .. } => {}
            Inline::Break => out.push('\n'),
        }
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse;
    use super::*;

    fn document(html: &str) -> Document {
        lower(&parse(html))
    }

    #[test]
    fn splits_paragraphs() {
        let doc = document("<p>one</p><p>two</p>");
        assert_eq!(doc.blocks.len(), 2);
        assert!(matches!(doc.blocks[0], Block::Paragraph { .. }));
    }

    #[test]
    fn carries_styling_down_the_tree() {
        let doc = document("<p><b>bold <i>both</i></b></p>");
        let Block::Paragraph { inlines, .. } = &doc.blocks[0] else { panic!() };
        let Inline::Text { style, .. } = &inlines[0] else { panic!() };
        assert!(style.bold && !style.italic);
        let Inline::Text { style, .. } = &inlines[1] else { panic!() };
        assert!(style.bold && style.italic);
    }

    #[test]
    fn numbers_ordered_lists() {
        let doc = document("<ol start=3><li>a</li><li>b</li></ol>");
        let Block::ListItem { marker, .. } = &doc.blocks[0] else { panic!() };
        assert_eq!(marker, "3.");
        let Block::ListItem { marker, .. } = &doc.blocks[1] else { panic!() };
        assert_eq!(marker, "4.");
    }

    #[test]
    fn tracks_quote_nesting() {
        let doc = document("<blockquote><p>a</p><blockquote><p>b</p></blockquote></blockquote>");
        let Block::Paragraph { quote_depth, .. } = &doc.blocks[0] else { panic!() };
        assert_eq!(*quote_depth, 1);
        let Block::Paragraph { quote_depth, .. } = &doc.blocks[1] else { panic!() };
        assert_eq!(*quote_depth, 2);
    }

    #[test]
    fn builds_tables_with_headers() {
        let doc = document("<table><tr><th>H</th></tr><tr><td>c</td></tr></table>");
        let Block::Table { rows, .. } = &doc.blocks[0] else { panic!() };
        assert_eq!(rows.len(), 2);
        assert!(rows[0].header);
        assert!(!rows[1].header);
    }

    #[test]
    fn keeps_link_targets() {
        let doc = document(r#"<p><a href="https://example.com">click</a></p>"#);
        let Block::Paragraph { inlines, .. } = &doc.blocks[0] else { panic!() };
        let Inline::Text { link, .. } = &inlines[0] else { panic!() };
        assert_eq!(link.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn preserves_whitespace_in_pre() {
        let doc = document("<pre>a\n  b</pre>");
        let Block::Pre { text, .. } = &doc.blocks[0] else { panic!() };
        assert_eq!(text, "a\n  b");
    }

    #[test]
    fn collapses_runs_of_whitespace() {
        assert_eq!(collapse_whitespace("  a   b \n c "), " a b c ");
        assert_eq!(collapse_whitespace("a\u{a0}b"), "a\u{a0}b");
    }

    #[test]
    fn drops_characters_that_have_no_glyph() {
        // Preheader padding: joiners and zero-width spaces would draw as
        // boxes, since the bundled fonts have no glyph for them.
        assert_eq!(collapse_whitespace("a\u{200c}\u{034f}b"), "ab");
        assert_eq!(collapse_whitespace("\u{feff}text"), "text");
        // A soft hyphen does have a glyph and is left alone.
        assert!(collapse_whitespace("a\u{ad}b").contains('\u{ad}'));
    }

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#fff"), Some([255, 255, 255]));
        assert_eq!(parse_color("#336699"), Some([0x33, 0x66, 0x99]));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some([1, 2, 3]));
        assert_eq!(parse_color("red"), Some([255, 0, 0]));
        assert_eq!(parse_color("not-a-color"), None);
    }

    #[test]
    fn rejects_non_ascii_colors_without_panicking() {
        // Byte lengths that would land mid-character in the hex arms.
        assert_eq!(parse_color("#\u{e9}1"), None);
        assert_eq!(parse_color("#\u{e9}\u{e9}\u{e9}"), None);
        assert_eq!(parse_color("#\u{2014}"), None);
        assert_eq!(parse_color("rgb(\u{e9})"), None);
    }

    #[test]
    fn reads_inline_css() {
        let doc = document(r#"<p style="color:#ff0000;font-weight:700">x</p>"#);
        let Block::Paragraph { inlines, .. } = &doc.blocks[0] else { panic!() };
        let Inline::Text { style, .. } = &inlines[0] else { panic!() };
        assert_eq!(style.color, Some([255, 0, 0]));
        assert!(style.bold);
    }

    #[test]
    fn skips_hidden_preheaders() {
        let doc = document(
            r#"<div style="font-size:1px; display: none !important;">preheader</div><p>body</p>"#,
        );
        let text = to_text(&doc);
        assert!(!text.contains("preheader"), "hidden text rendered: {text}");
        assert!(text.contains("body"));

        assert!(!to_text(&document(r#"<p style="visibility:hidden">x</p>"#)).contains('x'));
        assert!(!to_text(&document(r#"<p hidden>x</p>"#)).contains('x'));
        // Visible content with an unrelated display value must survive.
        assert!(to_text(&document(r#"<p style="display:block">x</p>"#)).contains('x'));
    }

    #[test]
    fn ignores_percentage_dimensions() {
        let doc = document(r#"<img src="cid:a" width="100%" height="40">"#);
        let Block::Paragraph { inlines, .. } = &doc.blocks[0] else { panic!() };
        let Inline::Image { width, height, .. } = &inlines[0] else { panic!() };
        assert_eq!(*width, None);
        assert_eq!(*height, Some(40));
    }
}
