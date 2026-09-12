//! Builds a printable document for a message.
//!
//! Printing is handed to the system's default handler for HTML, which on
//! every desktop is a browser. That is not a cop-out: browsers already have
//! print preview, page setup, printer selection and PDF export, all of which
//! this client would otherwise have to reimplement badly, and the message is
//! already HTML by the time it reaches here.
//!
//! The file written out must be self-contained, because the browser opens it
//! with no access to the message's MIME parts: `cid:` images are inlined as
//! `data:` URLs. Remote images are *not* fetched — the document is built from
//! the same sanitized body the reader is showing, so a message whose remote
//! content is blocked stays blocked on paper.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::Prepared;
use crate::mail::{Envelope, MessageBody};

/// Builds a standalone HTML document for one message.
pub fn document(envelope: &Envelope, body: &MessageBody, prepared: &Prepared) -> String {
    let mut out = String::with_capacity(prepared.html.len() + 4096);

    out.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    out.push_str("<title>");
    out.push_str(&escape(subject(envelope)));
    out.push_str("</title>");
    out.push_str(STYLE);
    out.push_str("</head><body>");

    out.push_str("<header class=\"m\">");
    out.push_str("<h1>");
    out.push_str(&escape(subject(envelope)));
    out.push_str("</h1><dl>");
    field(&mut out, "From", &addresses(&envelope.from));
    field(&mut out, "To", &addresses(&envelope.to));
    field(&mut out, "Cc", &addresses(&envelope.cc));
    field(&mut out, "Date", &crate::ui::format_date_long(envelope.date));
    if !body.attachments.is_empty() {
        let names: Vec<String> = body
            .attachments
            .iter()
            .map(|a| format!("{} ({})", a.filename, crate::ui::format_size(a.data.len())))
            .collect();
        field(&mut out, "Attachments", &names.join(", "));
    }
    out.push_str("</dl></header><article>");

    if prepared.html.is_empty() {
        // A plain-text message. `pre` keeps the sender's own line breaks,
        // which is the whole of their formatting.
        out.push_str("<pre class=\"plain\">");
        out.push_str(&escape(body.text.as_deref().unwrap_or_default()));
        out.push_str("</pre>");
    } else {
        out.push_str(&inline_images(&prepared.html, body));
    }

    out.push_str("</article></body></html>");
    out
}

/// Replaces `cid:` image references with the part they point at.
///
/// Anything still unresolved is left alone: the browser will draw a broken
/// image, which is a truer report than silently dropping content.
fn inline_images(html: &str, body: &MessageBody) -> String {
    if body.inline.is_empty() || !html.contains("cid:") {
        return html.to_string();
    }

    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(at) = rest.find("cid:") {
        out.push_str(&rest[..at]);
        rest = &rest[at + 4..];

        // The reference runs to the quote that closes the attribute.
        let end = rest.find(['"', '\'']).unwrap_or(rest.len());
        let reference = rest[..end].trim_matches(['<', '>']);

        match body.inline.iter().find(|part| part.content_id == reference) {
            Some(part) => {
                let mime = if part.mime.is_empty() {
                    "application/octet-stream"
                } else {
                    &part.mime
                };
                out.push_str("data:");
                out.push_str(mime);
                out.push_str(";base64,");
                out.push_str(&STANDARD.encode(&part.data));
            }
            None => {
                out.push_str("cid:");
                out.push_str(&rest[..end]);
            }
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn field(out: &mut String, label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    out.push_str("<dt>");
    out.push_str(label);
    out.push_str("</dt><dd>");
    out.push_str(&escape(value));
    out.push_str("</dd>");
}

fn subject(envelope: &Envelope) -> &str {
    if envelope.subject.trim().is_empty() {
        "(no subject)"
    } else {
        &envelope.subject
    }
}

fn addresses(list: &[crate::mail::Addr]) -> String {
    list.iter().map(|a| a.full()).collect::<Vec<_>>().join(", ")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Paper, not screen: black on white, margins left to the print dialog, and
/// nothing that depends on the application's theme.
const STYLE: &str = "<style>\
body{margin:0;padding:24px;background:#fff;color:#000;\
font:12pt/1.45 Georgia,'Times New Roman',serif;}\
header.m{border-bottom:1pt solid #999;padding-bottom:10px;margin-bottom:16px;}\
h1{font-size:16pt;margin:0 0 10px;}\
dl{margin:0;font:10pt/1.5 Arial,Helvetica,sans-serif;}\
dt{float:left;clear:left;width:92px;color:#555;}\
dd{margin:0 0 0 100px;}\
article{font-size:11pt;}\
article img{max-width:100%;height:auto;}\
article table{max-width:100%;border-collapse:collapse;}\
pre.plain{white-space:pre-wrap;font:11pt/1.45 Georgia,serif;}\
a{color:#000;text-decoration:underline;}\
blockquote{margin:0 0 0 8px;padding-left:12px;border-left:2pt solid #bbb;color:#333;}\
@media print{body{padding:0;}a[href^=http]::after{content:' <' attr(href) '>';\
font-size:9pt;color:#444;word-break:break-all;}}\
</style>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Addr;
    use crate::mail::model::{Attachment, InlinePart};

    fn message() -> (Envelope, MessageBody) {
        let envelope = Envelope {
            subject: "Quarterly <review>".into(),
            from: vec![Addr { name: "Ada".into(), email: "ada@example.com".into() }],
            to: vec![Addr { name: String::new(), email: "bob@example.org".into() }],
            date: 1_700_000_000,
            ..Default::default()
        };
        let body = MessageBody::default();
        (envelope, body)
    }

    fn prepared(html: &str) -> Prepared {
        Prepared { document: Default::default(), html: html.to_string(), blocked_remote: 0 }
    }

    #[test]
    fn writes_a_standalone_document() {
        let (envelope, body) = message();
        let out = document(&envelope, &body, &prepared("<p>Hello</p>"));
        assert!(out.starts_with("<!doctype html>"));
        assert!(out.contains("<p>Hello</p>"));
        assert!(out.contains("ada@example.com"));
        assert!(out.contains("bob@example.org"));
    }

    #[test]
    fn escapes_headers_but_not_the_sanitized_body() {
        let (envelope, body) = message();
        let out = document(&envelope, &body, &prepared("<p>body</p>"));
        // The subject is text and must not become markup.
        assert!(out.contains("Quarterly &lt;review&gt;"));
        // The body has already been sanitized, so it stays as markup.
        assert!(out.contains("<p>body</p>"));
    }

    #[test]
    fn omits_headers_that_are_empty() {
        let (envelope, body) = message();
        let out = document(&envelope, &body, &prepared("<p>x</p>"));
        assert!(!out.contains("<dt>Cc</dt>"), "empty Cc was rendered");
        assert!(out.contains("<dt>To</dt>"));
    }

    #[test]
    fn renders_a_plain_text_message_preformatted() {
        let (envelope, mut body) = message();
        body.text = Some("line one\n  indented".into());
        let out = document(&envelope, &body, &prepared(""));
        assert!(out.contains("<pre class=\"plain\">line one\n  indented</pre>"));
    }

    #[test]
    fn inlines_referenced_parts() {
        let (envelope, mut body) = message();
        body.inline = vec![InlinePart {
            content_id: "logo@example".into(),
            mime: "image/png".into(),
            data: b"hi".to_vec(),
        }];
        let html = r#"<img src="cid:logo@example"><p>after</p>"#;
        let out = document(&envelope, &body, &prepared(html));
        assert!(out.contains("data:image/png;base64,aGk="));
        assert!(!out.contains("cid:logo@example"));
        assert!(out.contains("<p>after</p>"), "content after the image was lost");
    }

    #[test]
    fn leaves_unresolvable_references_alone() {
        let (envelope, body) = message();
        let html = r#"<img src="cid:missing@example">tail"#;
        let out = document(&envelope, &body, &prepared(html));
        assert!(out.contains("cid:missing@example"));
        assert!(out.contains("tail"));
    }

    /// Builds print documents from the messages in the local cache and
    /// checks each is self-contained. Real mail exercises inlining and
    /// escaping in ways the synthetic cases above do not.
    #[test]
    #[ignore = "reads the local message cache"]
    fn builds_documents_for_cached_messages() {
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
        .unwrap();
        let mut statement = conn.prepare("SELECT uid, raw FROM body").unwrap();
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?)))
            .unwrap();

        let (mut built, mut with_images, mut remaining_cid) = (0, 0, 0);
        for row in rows {
            let (uid, raw) = row.unwrap();
            let body = crate::mail::parse::parse_body(&raw);
            let envelope = crate::mail::parse::parse_envelope(uid, &raw);
            let Some(prepared) =
                Prepared::from_parts(body.html.as_deref(), body.text.as_deref(), false)
            else {
                continue;
            };

            let out = document(&envelope, &body, &prepared);
            assert!(out.starts_with("<!doctype html>"), "uid {uid} is not a document");
            assert!(out.ends_with("</html>"), "uid {uid} is truncated");

            if !body.inline.is_empty() {
                with_images += 1;
            }
            if out.contains("cid:") {
                remaining_cid += 1;
            }
            built += 1;
        }
        println!(
            "built {built} documents; {with_images} had inline parts, \
             {remaining_cid} still reference cid:"
        );
    }

    #[test]
    fn lists_attachments() {
        let (envelope, mut body) = message();
        body.attachments = vec![Attachment {
            filename: "report.pdf".into(),
            mime: "application/pdf".into(),
            data: vec![0; 2048],
        }];
        let out = document(&envelope, &body, &prepared("<p>x</p>"));
        assert!(out.contains("report.pdf (2.0 KB)"));
    }
}
