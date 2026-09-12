//! A small HTML parser and block-lowering pass for the native renderer.
//!
//! This runs on output from [`super::sanitize`], which is already well-formed
//! and tag-restricted, so the parser does not need to handle the full mess of
//! real-world HTML: no implied tags, no `<table>` foster parenting, no
//! character-encoding guessing. What it does need is to be fast on large
//! marketing mail and to never panic on input a sender controls.

/// A parsed node.
#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Elements that never have children or a closing tag.
const VOID: &[&str] = &["br", "hr", "img", "wbr", "col", "area", "base", "input", "meta", "link"];

/// Parses a document fragment into a node list.
pub fn parse(html: &str) -> Vec<Node> {
    let bytes = html.as_bytes();
    let mut position = 0usize;
    let mut roots: Vec<Node> = Vec::new();
    let mut stack: Vec<Element> = Vec::new();

    while position < bytes.len() {
        match bytes[position] {
            b'<' if html[position..].starts_with("<!--") => {
                position = html[position..]
                    .find("-->")
                    .map(|i| position + i + 3)
                    .unwrap_or(bytes.len());
            }
            b'<' if html[position..].starts_with("<!") => {
                position = html[position..].find('>').map(|i| position + i + 1).unwrap_or(bytes.len());
            }
            b'<' if html[position..].starts_with("</") => {
                let end = html[position..].find('>').map(|i| position + i).unwrap_or(bytes.len());
                let name = html[position + 2..end].trim().to_ascii_lowercase();
                close_element(&mut stack, &mut roots, &name);
                position = (end + 1).min(bytes.len());
            }
            b'<' if position + 1 < bytes.len() && is_name_start(bytes[position + 1]) => {
                let (element, self_closing, next) = parse_start_tag(html, position);
                position = next;
                let void = self_closing || VOID.contains(&element.name.as_str());
                if void {
                    push_node(&mut stack, &mut roots, Node::Element(element));
                } else {
                    stack.push(element);
                }
            }
            _ => {
                // Run of text up to the next tag.
                let end = html[position..].find('<').map(|i| position + i).unwrap_or(bytes.len());
                let text = decode_entities(&html[position..end]);
                if !text.is_empty() {
                    push_node(&mut stack, &mut roots, Node::Text(text));
                }
                position = end;
            }
        }
    }

    // Unclosed elements at EOF still hold content worth showing.
    while let Some(element) = stack.pop() {
        push_node(&mut stack, &mut roots, Node::Element(element));
    }
    roots
}

fn push_node(stack: &mut Vec<Element>, roots: &mut Vec<Node>, node: Node) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => roots.push(node),
    }
}

/// Closes the nearest matching open element. A stray end tag with no match is
/// ignored rather than unwinding the whole stack.
fn close_element(stack: &mut Vec<Element>, roots: &mut Vec<Node>, name: &str) {
    let Some(index) = stack.iter().rposition(|e| e.name == name) else {
        return;
    };
    while stack.len() > index {
        let element = stack.pop().expect("index is in range");
        push_node(stack, roots, Node::Element(element));
    }
}

fn parse_start_tag(html: &str, start: usize) -> (Element, bool, usize) {
    let bytes = html.as_bytes();
    let mut position = start + 1;

    let name_start = position;
    while position < bytes.len() && is_name_char(bytes[position]) {
        position += 1;
    }
    let name = html[name_start..position].to_ascii_lowercase();

    let mut attrs = Vec::new();
    let mut self_closing = false;

    loop {
        while position < bytes.len() && bytes[position].is_ascii_whitespace() {
            position += 1;
        }
        if position >= bytes.len() {
            break;
        }
        match bytes[position] {
            b'>' => {
                position += 1;
                break;
            }
            b'/' => {
                self_closing = true;
                position += 1;
            }
            _ => {
                let key_start = position;
                while position < bytes.len()
                    && !bytes[position].is_ascii_whitespace()
                    && bytes[position] != b'='
                    && bytes[position] != b'>'
                {
                    position += 1;
                }
                let key = html[key_start..position].to_ascii_lowercase();

                while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                    position += 1;
                }
                let value = if position < bytes.len() && bytes[position] == b'=' {
                    position += 1;
                    while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                        position += 1;
                    }
                    read_attr_value(html, &mut position)
                } else {
                    String::new()
                };
                if !key.is_empty() {
                    attrs.push((key, value));
                }
            }
        }
    }

    (Element { name, attrs, children: Vec::new() }, self_closing, position)
}

fn read_attr_value(html: &str, position: &mut usize) -> String {
    let bytes = html.as_bytes();
    if *position >= bytes.len() {
        return String::new();
    }
    let quote = bytes[*position];
    if quote == b'"' || quote == b'\'' {
        *position += 1;
        let start = *position;
        while *position < bytes.len() && bytes[*position] != quote {
            *position += 1;
        }
        let value = decode_entities(&html[start..*position]);
        *position = (*position + 1).min(bytes.len());
        value
    } else {
        let start = *position;
        while *position < bytes.len()
            && !bytes[*position].is_ascii_whitespace()
            && bytes[*position] != b'>'
        {
            *position += 1;
        }
        decode_entities(&html[start..*position])
    }
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b':' || b == b'_'
}

/// Longest character reference this decoder recognises, counted in `char`s:
/// `&#x1F600;` is nine, and the named entities below are shorter.
const ENTITY_MAX_CHARS: usize = 12;

/// Expands the character references that actually appear in mail.
pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];

        // An entity is short; anything longer is a literal ampersand.
        //
        // The search window is counted in characters, not bytes: mail is full
        // of invisible padding (soft hyphens, zero-width joiners) and slicing
        // at a fixed byte offset lands inside one of them.
        let semi = rest
            .char_indices()
            .take(ENTITY_MAX_CHARS)
            .find(|(_, ch)| *ch == ';')
            .map(|(index, _)| index);

        let Some(semi) = semi else {
            out.push('&');
            // Safe: `rest` starts with the ASCII '&' matched above.
            rest = &rest[1..];
            continue;
        };

        let body = &rest[1..semi];
        let replacement = if let Some(hex) = body.strip_prefix("#x").or(body.strip_prefix("#X")) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = body.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            named_entity(body)
        };

        match replacement {
            Some(ch) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        // A non-breaking space, kept distinct so whitespace collapsing does
        // not eat the deliberate spacing senders rely on.
        "nbsp" => '\u{a0}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "bull" => '\u{2022}',
        "middot" => '\u{b7}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "euro" => '\u{20ac}',
        "pound" => '\u{a3}',
        "yen" => '\u{a5}',
        "cent" => '\u{a2}',
        "deg" => '\u{b0}',
        "plusmn" => '\u{b1}',
        "times" => '\u{d7}',
        "divide" => '\u{f7}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "shy" => '\u{ad}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(nodes: &[Node], index: usize) -> &Element {
        match &nodes[index] {
            Node::Element(e) => e,
            other => panic!("expected an element, found {other:?}"),
        }
    }

    #[test]
    fn parses_nested_elements() {
        let nodes = parse("<p>hello <b>world</b></p>");
        assert_eq!(nodes.len(), 1);
        let p = element(&nodes, 0);
        assert_eq!(p.name, "p");
        assert_eq!(p.children.len(), 2);
        assert_eq!(element(&p.children, 1).name, "b");
    }

    #[test]
    fn treats_void_elements_as_childless() {
        let nodes = parse("<p>a<br>b</p>");
        let p = element(&nodes, 0);
        assert_eq!(p.children.len(), 3);
        assert_eq!(element(&p.children, 1).name, "br");
    }

    #[test]
    fn reads_quoted_and_bare_attributes() {
        let nodes = parse(r#"<img src="a b.png" width=32 alt='x'>"#);
        let img = element(&nodes, 0);
        assert_eq!(img.attr("src"), Some("a b.png"));
        assert_eq!(img.attr("width"), Some("32"));
        assert_eq!(img.attr("alt"), Some("x"));
    }

    #[test]
    fn ignores_unmatched_end_tags() {
        let nodes = parse("<p>text</span></p>");
        assert_eq!(nodes.len(), 1);
        assert_eq!(element(&nodes, 0).name, "p");
    }

    #[test]
    fn recovers_from_unclosed_elements() {
        let nodes = parse("<div><p>text");
        let div = element(&nodes, 0);
        assert_eq!(div.name, "div");
        assert_eq!(element(&div.children, 0).name, "p");
    }

    #[test]
    fn decodes_character_references() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&mdash;"), "\u{2014}");
        // A bare ampersand is not an entity and must survive.
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("&notanentity;"), "&notanentity;");
    }

    #[test]
    fn decodes_entities_next_to_invisible_padding() {
        // The preheader padding that crashed the reader: `&nbsp;` runs mixed
        // with soft hyphens, combining joiners and zero-width non-joiners.
        let padding = "&nbsp;   \u{ad}\u{34f} \u{200c} ".repeat(20);
        let decoded = decode_entities(&padding);
        assert!(decoded.contains('\u{a0}'));
        assert!(!decoded.contains("&nbsp;"));
    }

    #[test]
    fn never_splits_a_multibyte_character() {
        // A bare ampersand followed by multi-byte text must not panic,
        // whatever the byte offsets happen to be.
        for pad in 0..24 {
            let input = format!("&{}", "\u{ad}".repeat(pad));
            let decoded = decode_entities(&input);
            assert!(decoded.starts_with('&'));
        }
        for pad in 0..24 {
            let input = format!("{}&amp;", "\u{2014}".repeat(pad));
            assert!(decode_entities(&input).ends_with('&'));
        }
    }

    #[test]
    fn ignores_an_overlong_run_after_an_ampersand() {
        // No `;` within the window: a literal ampersand, not an entity.
        let input = "&thisisfartoolongtobeanentity;";
        assert_eq!(decode_entities(input), input);
    }

    #[test]
    fn parses_a_document_full_of_padding_without_panicking() {
        let body = format!(
            "<p>{}</p><p>real text</p>",
            "&nbsp;\u{ad}\u{200c}\u{34f} ".repeat(50)
        );
        let nodes = parse(&body);
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn skips_comments_and_doctypes() {
        let nodes = parse("<!doctype html><!-- hi --><p>x</p>");
        assert_eq!(nodes.len(), 1);
        assert_eq!(element(&nodes, 0).name, "p");
    }
}
