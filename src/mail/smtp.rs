//! Outgoing mail: builds an RFC 5322 message from a draft and hands it to an
//! SMTP relay.
//!
//! The formatted bytes are returned alongside the send so the caller can
//! `APPEND` the same message to the Sent mailbox, which is how the message
//! shows up in other clients.

use anyhow::{Context, Result, bail};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::model::Draft;
use super::parse::parse_address_list;
use crate::auth::Credential;
use crate::config::{AccountConfig, Encryption};

/// A message that has been sent, with the bytes as they went on the wire.
pub struct Sent {
    pub raw: Vec<u8>,
}

/// Builds and sends a draft.
pub async fn send(
    account: &AccountConfig,
    credential: &Credential,
    draft: &Draft,
) -> Result<Sent> {
    let message = build(account, draft)?;
    let raw = message.formatted();

    let transport = transport(account, credential)?;
    transport.send(message).await.context("sending message")?;

    Ok(Sent { raw })
}

fn transport(
    account: &AccountConfig,
    credential: &Credential,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let builder = match account.smtp_encryption {
        Encryption::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host),
        Encryption::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.smtp_host)
        }
    }
    .with_context(|| format!("configuring SMTP for {}", account.smtp_host))?;

    let (secret, mechanisms) = match credential {
        Credential::Password(password) => {
            (password.clone(), vec![Mechanism::Plain, Mechanism::Login])
        }
        Credential::Bearer(token) => (token.clone(), vec![Mechanism::Xoauth2]),
    };

    Ok(builder
        .port(account.smtp_port)
        .credentials(Credentials::new(account.username.clone(), secret))
        .authentication(mechanisms)
        .timeout(Some(std::time::Duration::from_secs(60)))
        .build())
}

/// Assembles the MIME message. Plain text alone when there are no
/// attachments, `multipart/mixed` otherwise.
pub fn build(account: &AccountConfig, draft: &Draft) -> Result<Message> {
    let identity = account.identity_for(&draft.from);
    let from = mailbox(&identity.display_name, &identity.email)
        .with_context(|| format!("invalid sender address {}", identity.email))?;

    let mut builder = Message::builder().from(from);

    let to = mailboxes(&draft.to)?;
    if to.is_empty() {
        bail!("a message needs at least one recipient");
    }
    for addr in to {
        builder = builder.to(addr);
    }
    for addr in mailboxes(&draft.cc)? {
        builder = builder.cc(addr);
    }
    for addr in mailboxes(&draft.bcc)? {
        builder = builder.bcc(addr);
    }

    builder = builder.subject(draft.subject.clone());

    // Threading headers, so replies land in the right conversation.
    if let Some(parent) = &draft.in_reply_to {
        builder = builder.in_reply_to(parent.clone());
        let mut references = draft.references.clone();
        if !references.iter().any(|r| r == parent) {
            references.push(parent.clone());
        }
        builder = builder.references(references.join(" "));
    }

    let text = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(draft.body.clone());

    if draft.attachments.is_empty() {
        return builder.singlepart(text).context("building message");
    }

    let mut multipart = MultiPart::mixed().singlepart(text);
    for path in &draft.attachments {
        let data = std::fs::read(path)
            .with_context(|| format!("reading attachment {}", path.display()))?;
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        let content_type = ContentType::parse(guess_mime(&filename))
            .unwrap_or(ContentType::TEXT_PLAIN);
        multipart = multipart.singlepart(Attachment::new(filename).body(data, content_type));
    }

    builder.multipart(multipart).context("building message")
}

fn mailbox(name: &str, email: &str) -> Result<Mailbox> {
    let text = if name.trim().is_empty() {
        email.trim().to_string()
    } else {
        format!("{} <{}>", name.trim(), email.trim())
    };
    text.parse::<Mailbox>().with_context(|| format!("invalid address {text}"))
}

fn mailboxes(list: &str) -> Result<Vec<Mailbox>> {
    parse_address_list(list)
        .into_iter()
        .map(|a| mailbox(&a.name, &a.email))
        .collect()
}

/// Minimal extension-to-type map. Anything unknown is sent as an opaque
/// binary part, which every mail client handles correctly.
fn guess_mime(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// Builds the quoted body and subject for a reply.
pub fn reply_draft(
    account: &AccountConfig,
    envelope: &super::model::Envelope,
    body: &super::model::MessageBody,
    reply_all: bool,
) -> Draft {
    let quoted_source = match (&body.text, &body.html) {
        (Some(t), _) if !t.trim().is_empty() => t.clone(),
        (_, Some(h)) => crate::html::strip_tags(h),
        _ => String::new(),
    };
    let attribution = {
        let who = envelope.from.first().map(|a| a.full()).unwrap_or_default();
        let when = crate::ui::format_date_long(envelope.date);
        format!("On {when}, {who} wrote:")
    };
    let quoted: String = quoted_source
        .lines()
        .map(|line| format!("> {line}\n"))
        .collect();

    // Reply from whichever of the account's addresses this was sent to, so a
    // message to an alias is answered by that alias rather than silently
    // switching identity.
    let identities = account.identities();
    let addressed = envelope
        .to
        .iter()
        .chain(envelope.cc.iter())
        .find_map(|address| {
            identities
                .iter()
                .find(|identity| identity.email.eq_ignore_ascii_case(&address.email))
        })
        .cloned()
        .unwrap_or_default();

    // Every address of ours is dropped from Cc, not just the one replying,
    // or replying to all would copy the account back to itself.
    let is_self = |address: &&crate::mail::Addr| {
        identities
            .iter()
            .any(|identity| identity.email.eq_ignore_ascii_case(&address.email))
    };

    let to = envelope.from.iter().map(|a| a.full()).collect::<Vec<_>>().join(", ");
    let cc = if reply_all {
        envelope
            .to
            .iter()
            .chain(envelope.cc.iter())
            .filter(|address| !is_self(address))
            .map(|a| a.full())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    };

    Draft {
        account: account.id,
        from: addressed.email,
        to,
        cc,
        bcc: String::new(),
        subject: prefixed_subject("Re: ", &envelope.subject),
        body: format!("\n\n{attribution}\n{quoted}"),
        in_reply_to: (!envelope.message_id.is_empty()).then(|| envelope.message_id.clone()),
        references: Vec::new(),
        attachments: Vec::new(),
    }
}

/// Builds a forward draft with the original body inlined.
pub fn forward_draft(
    account: crate::config::AccountId,
    from: String,
    envelope: &super::model::Envelope,
    body: &super::model::MessageBody,
) -> Draft {
    let original = match (&body.text, &body.html) {
        (Some(t), _) if !t.trim().is_empty() => t.clone(),
        (_, Some(h)) => crate::html::strip_tags(h),
        _ => String::new(),
    };
    let header = format!(
        "---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n",
        envelope.from.first().map(|a| a.full()).unwrap_or_default(),
        crate::ui::format_date_long(envelope.date),
        envelope.subject,
        envelope.to.iter().map(|a| a.full()).collect::<Vec<_>>().join(", "),
    );

    Draft {
        account,
        from,
        subject: prefixed_subject("Fwd: ", &envelope.subject),
        body: format!("\n\n{header}{original}"),
        ..Default::default()
    }
}

/// Adds a prefix unless an equivalent one is already there.
fn prefixed_subject(prefix: &str, subject: &str) -> String {
    let lower = subject.to_ascii_lowercase();
    let prefix_lower = prefix.trim().to_ascii_lowercase();
    if lower.starts_with(&prefix_lower) {
        subject.to_string()
    } else {
        format!("{prefix}{subject}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;

    fn account() -> AccountConfig {
        let mut a = AccountConfig::imap(0, "me@example.com");
        a.display_name = "Me".into();
        a
    }

    #[test]
    fn builds_a_plain_message() {
        let draft = Draft {
            to: "you@example.org".into(),
            subject: "Hi".into(),
            body: "Hello".into(),
            ..Default::default()
        };
        let raw = String::from_utf8(build(&account(), &draft).unwrap().formatted()).unwrap();
        assert!(raw.contains("me@example.com"), "no sender in:\n{raw}");
        assert!(raw.contains("To: you@example.org"));
        assert!(raw.contains("Subject: Hi"));
        assert!(raw.contains("Hello"));
    }

    #[test]
    fn rejects_a_message_with_no_recipients() {
        let draft = Draft { subject: "Hi".into(), ..Default::default() };
        assert!(build(&account(), &draft).is_err());
    }

    #[test]
    fn sends_as_the_address_the_draft_names() {
        let mut account = account();
        account.aliases = vec![crate::config::Identity {
            email: "sales@example.com".into(),
            display_name: "Sales".into(),
        }];

        let draft = Draft {
            from: "sales@example.com".into(),
            to: "you@example.org".into(),
            subject: "Hi".into(),
            ..Default::default()
        };
        let raw = String::from_utf8(build(&account, &draft).unwrap().formatted()).unwrap();
        assert!(raw.contains("sales@example.com"), "wrong sender in:\n{raw}");
        assert!(!raw.contains("From: \"Me\" <me@example.com>"));
    }

    #[test]
    fn an_unknown_from_falls_back_to_the_primary_address() {
        let draft = Draft {
            from: "stranger@example.com".into(),
            to: "you@example.org".into(),
            ..Default::default()
        };
        let raw = String::from_utf8(build(&account(), &draft).unwrap().formatted()).unwrap();
        assert!(raw.contains("me@example.com"));
        assert!(!raw.contains("stranger@example.com"));
    }

    #[test]
    fn replies_from_the_alias_that_was_written_to() {
        let mut account = account();
        account.aliases = vec![crate::config::Identity {
            email: "sales@example.com".into(),
            display_name: "Sales".into(),
        }];

        let envelope = crate::mail::Envelope {
            from: vec![crate::mail::Addr {
                name: "Ada".into(),
                email: "ada@example.org".into(),
            }],
            to: vec![crate::mail::Addr {
                name: String::new(),
                email: "Sales@Example.com".into(),
            }],
            ..Default::default()
        };
        let body = crate::mail::MessageBody::default();

        let draft = reply_draft(&account, &envelope, &body, true);
        assert_eq!(draft.from, "sales@example.com");
        // None of the account's own addresses are copied back in.
        assert!(!draft.cc.to_lowercase().contains("sales@example.com"));
        assert!(draft.to.contains("ada@example.org"));
    }

    #[test]
    fn a_reply_to_an_unknown_address_uses_the_primary_one() {
        let envelope = crate::mail::Envelope {
            from: vec![crate::mail::Addr {
                name: String::new(),
                email: "ada@example.org".into(),
            }],
            to: vec![crate::mail::Addr {
                name: String::new(),
                email: "list@example.net".into(),
            }],
            ..Default::default()
        };
        let draft =
            reply_draft(&account(), &envelope, &crate::mail::MessageBody::default(), false);
        // Empty means "the account's primary address".
        assert!(draft.from.is_empty());
    }

    #[test]
    fn does_not_double_the_reply_prefix() {
        assert_eq!(prefixed_subject("Re: ", "Re: report"), "Re: report");
        assert_eq!(prefixed_subject("Re: ", "report"), "Re: report");
    }
}
