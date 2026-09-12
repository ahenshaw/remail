//! Conversion from raw RFC 5322 bytes into the client's own message types.
//!
//! Parsing is cheap enough (`mail-parser` is zero-copy where it can be) that
//! the client re-parses from the cached raw message instead of storing a
//! decoded form, which keeps the cache format trivially forward-compatible.

use mail_parser::{Address as MpAddress, Message, MessageParser, MimeHeaders, PartType};

use super::model::{Addr, Attachment, Envelope, Flags, InlinePart, MessageBody};

/// Parses a raw message into a body, decoding parts and collecting the
/// attachments and `cid:` resources the reader needs.
pub fn parse_body(raw: &[u8]) -> MessageBody {
    let Some(msg) = MessageParser::default().parse(raw) else {
        // Undecodable message: show it as plain text rather than nothing.
        return MessageBody {
            text: Some(String::from_utf8_lossy(raw).into_owned()),
            raw_size: raw.len(),
            ..Default::default()
        };
    };

    let html = msg.body_html(0).map(|c| c.into_owned());
    // Deliberately not `body_text`: for an HTML-only message that synthesises
    // a text version whose converter leaks `<style>` contents into what the
    // user sees. Take a real `text/plain` part or nothing, and let the HTML
    // pipeline (which strips style blocks correctly) handle the rest.
    let text = msg.text_bodies().find_map(|part| match &part.body {
        PartType::Text(text) => Some(text.as_ref().to_owned()),
        _ => None,
    });

    let mut attachments = Vec::new();
    let mut inline = Vec::new();
    for part in msg.attachments() {
        let mime = mime_of(part);
        let data = part.contents().to_vec();
        if data.is_empty() {
            continue;
        }
        // A part with a Content-ID is referenced from the HTML body by
        // `cid:`; it belongs in the document, not the attachment bar.
        match part.content_id() {
            Some(cid) if !cid.is_empty() => inline.push(InlinePart {
                content_id: cid.trim_matches(['<', '>']).to_string(),
                data,
            }),
            _ => attachments.push(Attachment {
                filename: part
                    .attachment_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| default_name(&mime)),
                mime,
                data,
            }),
        }
    }

    // Some senders mark inline images as regular parts and reference them by
    // filename; expose those to the renderer as well.
    for part in msg.parts.iter() {
        if let PartType::InlineBinary(bin) = &part.body {
            if let Some(cid) = part.content_id().filter(|c| !c.is_empty()) {
                let cid = cid.trim_matches(['<', '>']).to_string();
                if !inline.iter().any(|i| i.content_id == cid) {
                    inline.push(InlinePart { content_id: cid, data: bin.to_vec() });
                }
            }
        }
    }

    let headers = msg
        .headers_raw()
        .map(|(name, value)| (name.to_string(), value.trim().to_string()))
        .collect();

    MessageBody { html, text, attachments, inline, headers, raw_size: raw.len() }
}

/// Parses the header block of a message into an envelope. Used when the server
/// gives us headers rather than a structured `ENVELOPE` response.
pub fn parse_envelope(uid: u32, raw: &[u8]) -> Envelope {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return Envelope { uid, subject: "(unparseable message)".into(), ..Default::default() };
    };
    envelope_from_message(uid, &msg)
}

fn envelope_from_message(uid: u32, msg: &Message<'_>) -> Envelope {
    Envelope {
        uid,
        // Filled in by whoever knows which mailbox this came from.
        mailbox: String::new(),
        subject: msg.subject().unwrap_or_default().to_string(),
        from: addrs(msg.from()),
        to: addrs(msg.to()),
        cc: addrs(msg.cc()),
        date: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        flags: Flags::default(),
        size: 0,
        message_id: msg.message_id().unwrap_or_default().to_string(),
        in_reply_to: msg
            .in_reply_to()
            .as_text_list()
            .and_then(|l| l.first().map(|s| s.to_string()))
            .unwrap_or_default(),
        has_attachments: msg.attachment_count() > 0,
        preview: String::new(),
    }
}

fn addrs(address: Option<&MpAddress<'_>>) -> Vec<Addr> {
    let Some(address) = address else { return Vec::new() };
    address
        .iter()
        .filter_map(|a| {
            let email = a.address()?.to_string();
            Some(Addr { name: a.name().unwrap_or_default().to_string(), email })
        })
        .collect()
}

fn mime_of(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype().to_ascii_lowercase(), sub.to_ascii_lowercase()),
            None => ct.ctype().to_ascii_lowercase(),
        },
        None => "application/octet-stream".to_string(),
    }
}

fn default_name(mime: &str) -> String {
    let ext = mime.rsplit('/').next().unwrap_or("bin");
    format!("attachment.{ext}")
}

/// Splits a comma-separated recipient list into addresses, tolerating the
/// `Name <addr>` form and stray whitespace.
pub fn parse_address_list(input: &str) -> Vec<Addr> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angle = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '<' if !in_quotes => {
                in_angle = true;
                current.push(ch);
            }
            '>' if !in_quotes => {
                in_angle = false;
                current.push(ch);
            }
            ',' | ';' if !in_quotes && !in_angle => {
                push_addr(&mut out, &current);
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    push_addr(&mut out, &current);
    out
}

fn push_addr(out: &mut Vec<Addr>, raw: &str) {
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    if let Some(open) = raw.rfind('<') {
        let email = raw[open + 1..].trim_end_matches('>').trim();
        let name = raw[..open].trim().trim_matches('"').trim();
        if !email.is_empty() {
            out.push(Addr { name: name.to_string(), email: email.to_string() });
        }
    } else {
        out.push(Addr { name: String::new(), email: raw.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_recipient_lists() {
        let list = parse_address_list("Ada <ada@example.com>, bob@example.org");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Ada");
        assert_eq!(list[0].email, "ada@example.com");
        assert_eq!(list[1].email, "bob@example.org");
    }

    #[test]
    fn keeps_commas_inside_quoted_display_names() {
        let list = parse_address_list("\"Doe, Jane\" <jane@example.com>");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].email, "jane@example.com");
    }

    #[test]
    fn ignores_text_synthesised_from_html() {
        // No text/plain part: the style block must not reach the preview.
        let raw = b"From: a@example.com\r\n\
                    Subject: HTML only\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <html><body><style>div.p { display: none !important; }</style>\
                    <p>Real content</p></body></html>\r\n";
        let body = parse_body(raw);
        assert!(body.text.is_none(), "synthesised text must not be used");
        let preview = body.preview();
        assert!(!preview.contains("display"), "preview leaked CSS: {preview}");
        assert!(preview.contains("Real content"), "preview lost content: {preview}");
    }

    #[test]
    fn parses_a_simple_message() {
        let raw = b"From: Ada <ada@example.com>\r\n\
                    To: bob@example.org\r\n\
                    Subject: Hello\r\n\
                    \r\n\
                    Hi there\r\n";
        let env = parse_envelope(7, raw);
        assert_eq!(env.uid, 7);
        assert_eq!(env.subject, "Hello");
        assert_eq!(env.from[0].email, "ada@example.com");

        let body = parse_body(raw);
        assert!(body.text.unwrap().contains("Hi there"));
    }
}
