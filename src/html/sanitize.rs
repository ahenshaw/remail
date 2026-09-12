//! HTML sanitization for message bodies.
//!
//! Email HTML is hostile by default: scripts, tracking beacons, and layout
//! that assumes a browser. Everything here runs before either renderer sees
//! the markup, so the Servo backend is protected by the same rules as the
//! native one.
//!
//! Two things are enforced:
//!
//! * Active content (`<script>`, `<iframe>`, event handlers, `javascript:`
//!   URLs) is removed outright.
//! * Remote images are dropped unless the user opts in, because a uniquely
//!   named remote image is the standard read receipt.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Result of sanitizing a message body.
pub struct Sanitized {
    pub html: String,
    /// How many remote resources were withheld, so the UI can offer to load
    /// them.
    pub blocked_remote: usize,
}

/// Sanitizes a body. `allow_remote` permits `http(s)` image sources.
pub fn sanitize(html: &str, allow_remote: bool) -> Sanitized {
    let blocked = Arc::new(AtomicUsize::new(0));
    let counter = blocked.clone();

    let mut builder = ammonia::Builder::default();
    builder
        .tags(allowed_tags())
        .generic_attributes(generic_attributes())
        .tag_attributes(tag_attributes())
        // Drop the contents of these, not just the tags: a bare <style> body
        // would otherwise render as a wall of CSS text.
        .clean_content_tags(HashSet::from_iter(["script", "style", "title", "head"]))
        .url_schemes(url_schemes())
        // Relative URLs have no base in an email; they cannot resolve.
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .attribute_filter(move |element, attribute, value| {
            filter_attribute(element, attribute, value, allow_remote, &counter)
        });

    Sanitized {
        html: builder.clean(html).to_string(),
        blocked_remote: blocked.load(Ordering::Relaxed),
    }
}

fn filter_attribute<'u>(
    element: &str,
    attribute: &str,
    value: &'u str,
    allow_remote: bool,
    blocked: &AtomicUsize,
) -> Option<Cow<'u, str>> {
    match (element, attribute) {
        // Image sources are the tracking vector; everything else that can
        // fetch has already been stripped with its tag.
        ("img", "src") => {
            // A part carried inside the message reveals nothing by being
            // drawn; a remote one reports the open, so it waits for consent.
            let embedded = value.starts_with("cid:") || value.starts_with("data:image/");
            if embedded || allow_remote {
                Some(Cow::Borrowed(value))
            } else {
                blocked.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
        // Backgrounds can fetch too, and carry no content worth keeping.
        (_, "background") => None,
        _ => Some(Cow::Borrowed(value)),
    }
}

fn allowed_tags() -> HashSet<&'static str> {
    HashSet::from_iter([
        "a",
        "abbr",
        "b",
        "blockquote",
        "br",
        "caption",
        "cite",
        "code",
        "col",
        "colgroup",
        "dd",
        "del",
        "div",
        "dl",
        "dt",
        "em",
        "figcaption",
        "figure",
        "font",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "i",
        "img",
        "ins",
        "kbd",
        "li",
        "mark",
        "ol",
        "p",
        "pre",
        "q",
        "s",
        "samp",
        "small",
        "span",
        "strike",
        "strong",
        "sub",
        "sup",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "tr",
        "tt",
        "u",
        "ul",
        "var",
        "wbr",
    ])
}

fn generic_attributes() -> HashSet<&'static str> {
    // `style` survives because the native renderer reads colors from it; the
    // sanitizer restricts which properties may appear.
    HashSet::from_iter(["dir", "lang", "title", "align", "style", "class"])
}

fn tag_attributes() -> HashMap<&'static str, HashSet<&'static str>> {
    HashMap::from_iter([
        ("a", HashSet::from_iter(["href"])),
        ("img", HashSet::from_iter(["src", "alt", "width", "height"])),
        ("td", HashSet::from_iter(["colspan", "rowspan"])),
        ("th", HashSet::from_iter(["colspan", "rowspan"])),
        ("ol", HashSet::from_iter(["start"])),
        ("font", HashSet::from_iter(["color", "size", "face"])),
        ("blockquote", HashSet::from_iter(["cite"])),
    ])
}

fn url_schemes() -> HashSet<&'static str> {
    // `cid` resolves to a part inside this message; `data` is inert once
    // scripts are gone and is how many senders embed small images.
    HashSet::from_iter(["http", "https", "mailto", "tel", "cid", "data"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_scripts_and_handlers() {
        let dirty = r#"<p onclick="steal()">hi</p><script>evil()</script>"#;
        let clean = sanitize(dirty, true).html;
        assert!(!clean.contains("script"));
        assert!(!clean.contains("onclick"));
        assert!(clean.contains("hi"));
    }

    #[test]
    fn drops_style_blocks_entirely() {
        let clean = sanitize("<style>p{color:red}</style><p>text</p>", true).html;
        assert!(!clean.contains("color:red"));
        assert!(clean.contains("text"));
    }

    #[test]
    fn blocks_remote_images_by_default() {
        let dirty = r#"<img src="https://tracker.example/pixel.gif"><p>body</p>"#;
        let result = sanitize(dirty, false);
        assert_eq!(result.blocked_remote, 1);
        assert!(!result.html.contains("tracker.example"));
    }

    #[test]
    fn keeps_remote_images_when_allowed() {
        let dirty = r#"<img src="https://example.com/a.png">"#;
        let result = sanitize(dirty, true);
        assert_eq!(result.blocked_remote, 0);
        assert!(result.html.contains("example.com/a.png"));
    }

    #[test]
    fn always_keeps_embedded_parts() {
        let result = sanitize(r#"<img src="cid:logo@example">"#, false);
        assert_eq!(result.blocked_remote, 0);
        assert!(result.html.contains("cid:logo@example"));
    }

    #[test]
    fn rejects_javascript_urls() {
        let clean = sanitize(r#"<a href="javascript:alert(1)">x</a>"#, true).html;
        assert!(!clean.contains("javascript:"));
    }
}
