//! HTML handling for message bodies: sanitize, parse, lay out, render.
//!
//! Two renderers sit behind the same prepared document:
//!
//! * [`native`] draws the block model directly with egui. It is the default
//!   because it starts instantly, costs nothing when idle, and inherits the
//!   application's theme and text selection.
//! * [`servo`] (behind the `servo` feature) hands the sanitized markup to a
//!   real web engine for messages that genuinely need CSS layout.
//!
//! Both consume output from [`sanitize`], so the security properties do not
//! depend on which renderer is selected.

pub mod dom;
pub mod layout;
pub mod native;
pub mod print;
pub mod sanitize;

#[cfg(feature = "servo")]
pub mod servo;

pub use layout::{Block, Document, Inline, Style};

/// A message body that has been cleaned and lowered, ready for either renderer.
pub struct Prepared {
    pub document: Document,
    /// The sanitized markup, kept for the Servo backend and "view source".
    pub html: String,
    /// Remote resources withheld from the document.
    pub blocked_remote: usize,
}

impl Prepared {
    /// Builds a renderable document from whichever body part a message
    /// carried, preferring HTML and falling back to plain text.
    ///
    /// Returns `None` only when the message has no body at all.
    pub fn from_parts(
        html: Option<&str>,
        text: Option<&str>,
        allow_remote: bool,
    ) -> Option<Self> {
        // An HTML part that sanitizes down to nothing (all markup, no
        // content) is worse than the plain-text alternative, so fall through.
        if let Some(html) = html.filter(|h| !h.trim().is_empty()) {
            if let Some(prepared) = prepare_guarded(html, allow_remote) {
                if !prepared.document.is_empty() {
                    return Some(prepared);
                }
            }
        }
        match text.filter(|t| !t.trim().is_empty()) {
            Some(text) => Some(Prepared {
                document: prepare_text(text),
                html: String::new(),
                blocked_remote: 0,
            }),
            None => None,
        }
    }
}

/// [`prepare`], but a panic somewhere in the HTML pipeline degrades to the
/// plain-text part instead of taking down the application.
///
/// Message bodies are attacker-controlled input from the open internet. The
/// parser is written not to panic and is tested for it, but "this one message
/// is unreadable" is a far better failure than "the client dies whenever you
/// select that row".
fn prepare_guarded(html: &str, allow_remote: bool) -> Option<Prepared> {
    match std::panic::catch_unwind(|| prepare(html, allow_remote)) {
        Ok(prepared) => Some(prepared),
        Err(_) => {
            tracing::error!("HTML rendering panicked; falling back to plain text");
            None
        }
    }
}

/// Sanitizes and lowers a message body.
pub fn prepare(html: &str, allow_remote: bool) -> Prepared {
    let cleaned = sanitize::sanitize(html, allow_remote);
    let document = layout::lower(&dom::parse(&cleaned.html));
    Prepared { document, html: cleaned.html, blocked_remote: cleaned.blocked_remote }
}

/// Lowers plain text into the same block model, so the reader has one drawing
/// path regardless of which body part a message carried.
///
/// Quoted lines (`>`) become quote blocks, which is what makes plain-text
/// reply chains readable.
pub fn prepare_text(text: &str) -> Document {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<Inline> = Vec::new();
    let mut current_depth = 0u8;

    let flush = |blocks: &mut Vec<Block>, paragraph: &mut Vec<Inline>, depth: u8| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph {
                inlines: std::mem::take(paragraph),
                quote_depth: depth,
            });
        }
    };

    for line in text.lines() {
        let (depth, content) = strip_quote_markers(line);

        if content.trim().is_empty() {
            flush(&mut blocks, &mut paragraph, current_depth);
            current_depth = depth;
            continue;
        }
        if depth != current_depth {
            flush(&mut blocks, &mut paragraph, current_depth);
            current_depth = depth;
        }
        if !paragraph.is_empty() {
            paragraph.push(Inline::Break);
        }
        paragraph.push(Inline::Text {
            text: content.to_string(),
            style: Style::default(),
            link: detect_url(content),
        });
    }
    flush(&mut blocks, &mut paragraph, current_depth);

    Document { blocks }
}

/// Counts and removes leading `>` quote markers.
fn strip_quote_markers(line: &str) -> (u8, &str) {
    let mut depth = 0u8;
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        match trimmed.strip_prefix('>') {
            Some(after) => {
                depth = depth.saturating_add(1);
                rest = after;
            }
            None => return (depth, if depth > 0 { trimmed } else { rest }),
        }
    }
}

/// Treats a line that is nothing but a URL as a link. Anything subtler
/// belongs to the HTML path, where the sender said what they meant.
fn detect_url(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let looks_like_url = trimmed.starts_with("http://") || trimmed.starts_with("https://");
    (looks_like_url && !trimmed.contains(char::is_whitespace))
        .then(|| trimmed.to_string())
}

/// Extracts readable text from HTML, for previews and reply quoting.
pub fn strip_tags(html: &str) -> String {
    let cleaned = sanitize::sanitize(html, false);
    let document = layout::lower(&dom::parse(&cleaned.html));
    layout::to_text(&document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup_to_readable_text() {
        let text = strip_tags("<p>Hello <b>there</b></p><p>Second</p>");
        assert!(text.contains("Hello there"));
        assert!(text.contains("Second"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn reads_quote_depth_from_plain_text() {
        let doc = prepare_text("top\n\n> one\n>> two");
        let depths: Vec<u8> = doc
            .blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph { quote_depth, .. } => *quote_depth,
                _ => 0,
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn links_bare_urls_in_plain_text() {
        let doc = prepare_text("https://example.com/x");
        let Block::Paragraph { inlines, .. } = &doc.blocks[0] else { panic!() };
        let Inline::Text { link, .. } = &inlines[0] else { panic!() };
        assert_eq!(link.as_deref(), Some("https://example.com/x"));
    }

    /// Renders every message in the local cache through the *unguarded*
    /// pipeline, so a panic fails the test rather than being swallowed by
    /// [`prepare_guarded`]. Real mail is the best corpus available.
    #[test]
    #[ignore = "reads the local message cache"]
    fn renders_every_cached_message() {
        let Ok(path) = crate::config::data_dir().map(|d| d.join("cache.sqlite")) else {
            return;
        };
        if !path.exists() {
            return;
        }
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("opening the cache");

        let mut statement = conn
            .prepare("SELECT mailbox, uid, raw FROM body")
            .expect("querying bodies");
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .expect("reading bodies");

        let mut rendered = 0;
        for row in rows {
            let (mailbox, uid, raw) = row.expect("decoding a cached body");
            let body = crate::mail::parse::parse_body(&raw);
            if let Some(html) = &body.html {
                // Direct call: no panic guard, so failures are visible here.
                let prepared = prepare(html, true);
                assert!(
                    !prepared.html.is_empty() || html.trim().is_empty(),
                    "{mailbox}:{uid} sanitized to nothing"
                );
            }
            if let Some(text) = &body.text {
                let _ = prepare_text(text);
            }
            rendered += 1;
        }
        println!("rendered {rendered} cached messages without panicking");
    }

    #[test]
    fn prepare_reports_blocked_resources() {
        let prepared = prepare(r#"<img src="https://t.example/p.gif">text"#, false);
        assert_eq!(prepared.blocked_remote, 1);
        assert!(!prepared.document.is_empty());
    }
}
