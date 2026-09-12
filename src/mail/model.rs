//! Types shared between the IMAP worker, the SQLite cache and the UI.
//!
//! Everything here is plain data and `Send`, so it can cross the channel
//! between the async mail engine and the egui thread without further work.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::AccountId;

/// IMAP message flags packed into a word. Only the flags the client acts on
/// are modelled; unknown keywords are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flags(pub u16);

impl Flags {
    pub const SEEN: u16 = 1 << 0;
    pub const ANSWERED: u16 = 1 << 1;
    pub const FLAGGED: u16 = 1 << 2;
    pub const DELETED: u16 = 1 << 3;
    pub const DRAFT: u16 = 1 << 4;
    pub const RECENT: u16 = 1 << 5;

    pub fn has(self, bit: u16) -> bool {
        self.0 & bit != 0
    }

    pub fn set(&mut self, bit: u16, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    pub fn is_unread(self) -> bool {
        !self.has(Self::SEEN)
    }

    /// The IMAP wire name for a single flag bit.
    pub fn imap_name(bit: u16) -> &'static str {
        match bit {
            Self::SEEN => "\\Seen",
            Self::ANSWERED => "\\Answered",
            Self::FLAGGED => "\\Flagged",
            Self::DELETED => "\\Deleted",
            Self::DRAFT => "\\Draft",
            _ => "\\Recent",
        }
    }
}

/// A mail address with its optional display name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addr {
    pub name: String,
    pub email: String,
}

impl Addr {
    /// Short form for list columns: the display name when there is one.
    pub fn short(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }

    /// `Name <addr@example.com>`, or just the address.
    pub fn full(&self) -> String {
        if self.name.is_empty() {
            self.email.clone()
        } else {
            format!("{} <{}>", self.name, self.email)
        }
    }
}

/// Everything the message list needs to draw a row, without the body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Envelope {
    pub uid: u32,
    pub subject: String,
    pub from: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    /// Unix seconds. Zero when the server supplied no usable date.
    pub date: i64,
    pub flags: Flags,
    pub size: u32,
    pub message_id: String,
    pub in_reply_to: String,
    pub has_attachments: bool,
    /// First line or so of the body, filled in once the body is cached.
    pub preview: String,
}

impl Envelope {
    /// Case-insensitive match across the fields a user expects to search.
    pub fn matches(&self, needle_lower: &str) -> bool {
        if needle_lower.is_empty() {
            return true;
        }
        let hit = |s: &str| s.to_ascii_lowercase().contains(needle_lower);
        hit(&self.subject)
            || hit(&self.preview)
            || self.from.iter().any(|a| hit(&a.name) || hit(&a.email))
            || self.to.iter().any(|a| hit(&a.name) || hit(&a.email))
    }
}

/// Well-known mailbox roles, resolved from RFC 6154 `SPECIAL-USE` attributes
/// with a name-based fallback for servers that do not advertise them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialUse {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    /// Gmail's virtual "All Mail".
    All,
    #[default]
    Normal,
}

impl SpecialUse {
    pub fn icon(self) -> &'static str {
        match self {
            SpecialUse::Inbox => "\u{1F4E5}",
            SpecialUse::Sent => "\u{1F4E4}",
            SpecialUse::Drafts => "\u{1F4DD}",
            SpecialUse::Trash => "\u{1F5D1}",
            SpecialUse::Junk => "\u{26A0}",
            SpecialUse::Archive => "\u{1F4E6}",
            SpecialUse::All => "\u{1F5C2}",
            SpecialUse::Normal => "\u{1F4C1}",
        }
    }

    /// Sort rank so the common mailboxes stay at the top of the sidebar.
    pub fn rank(self) -> u8 {
        match self {
            SpecialUse::Inbox => 0,
            SpecialUse::Drafts => 1,
            SpecialUse::Sent => 2,
            SpecialUse::Archive => 3,
            SpecialUse::All => 4,
            SpecialUse::Junk => 5,
            SpecialUse::Trash => 6,
            SpecialUse::Normal => 7,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailboxInfo {
    /// Full IMAP path, e.g. `[Gmail]/Sent Mail` or `Work/Reports`.
    pub name: String,
    pub delimiter: Option<String>,
    pub special: SpecialUse,
    /// `\Noselect` mailboxes exist only as parents of other mailboxes.
    pub selectable: bool,
    pub exists: u32,
    pub unseen: u32,
}

impl MailboxInfo {
    /// Last path component, for display under its parent.
    pub fn leaf(&self) -> &str {
        match self.delimiter.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => self.name.rsplit(d).next().unwrap_or(&self.name),
            None => &self.name,
        }
    }

    /// Paths of this mailbox's ancestors, outermost first. `Maverick/HR`
    /// yields `["Maverick"]`.
    pub fn ancestors(&self) -> Vec<&str> {
        let Some(delimiter) = self.delimiter.as_deref().filter(|d| !d.is_empty()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut offset = 0;
        while let Some(index) = self.name[offset..].find(delimiter) {
            offset += index;
            if offset > 0 {
                out.push(&self.name[..offset]);
            }
            offset += delimiter.len();
        }
        out
    }

    /// Indentation level for display, counting only ancestors that are
    /// themselves shown.
    ///
    /// Gmail nests its special folders under a `\Noselect` `[Gmail]`
    /// container that never appears in the sidebar. Indenting its children
    /// under it would leave them hanging below nothing.
    pub fn display_depth(&self, is_shown: impl Fn(&str) -> bool) -> usize {
        self.ancestors().into_iter().filter(|path| is_shown(path)).count()
    }
}

/// A non-inline part the user can save.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// A `cid:` referenced part, usually an image embedded in the HTML body.
#[derive(Debug, Clone)]
pub struct InlinePart {
    pub content_id: String,
    pub data: Vec<u8>,
}

/// A fully parsed message body.
#[derive(Debug, Clone, Default)]
pub struct MessageBody {
    pub html: Option<String>,
    pub text: Option<String>,
    pub attachments: Vec<Attachment>,
    pub inline: Vec<InlinePart>,
    /// Headers kept for the "show source" view, in wire order.
    pub headers: Vec<(String, String)>,
    pub raw_size: usize,
}

impl MessageBody {
    /// A short plain-text summary for the list row.
    pub fn preview(&self) -> String {
        let source = match (&self.text, &self.html) {
            (Some(t), _) if !t.trim().is_empty() => t.clone(),
            (_, Some(h)) => crate::html::strip_tags(h),
            _ => String::new(),
        };
        let mut out = String::with_capacity(160);
        for word in source.split_whitespace() {
            if out.len() + word.len() + 1 > 160 {
                break;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
        out
    }
}

/// A message being composed.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub account: AccountId,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    /// `Message-ID` this is a reply to, for correct threading.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<PathBuf>,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.to.trim().is_empty()
            && self.subject.trim().is_empty()
            && self.body.trim().is_empty()
    }
}

/// Identifies a message across accounts and mailboxes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageKey {
    pub account: AccountId,
    pub mailbox: String,
    pub uid: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(name: &str) -> MailboxInfo {
        MailboxInfo {
            name: name.to_string(),
            delimiter: Some("/".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn lists_ancestor_paths_outermost_first() {
        assert_eq!(mailbox("Maverick/HR").ancestors(), vec!["Maverick"]);
        assert_eq!(mailbox("a/b/c").ancestors(), vec!["a", "a/b"]);
        assert!(mailbox("INBOX").ancestors().is_empty());
    }

    #[test]
    fn ignores_a_missing_delimiter() {
        let flat = MailboxInfo { name: "a/b".into(), delimiter: None, ..Default::default() };
        assert!(flat.ancestors().is_empty());
        assert_eq!(flat.leaf(), "a/b");
    }

    #[test]
    fn indents_against_visible_ancestors_only() {
        // `[Gmail]` is \Noselect and never shown, so its children sit at the
        // top level rather than under an invisible parent.
        let shown = ["INBOX", "Maverick", "Maverick/HR", "[Gmail]/Important"];
        let is_shown = |path: &str| shown.contains(&path);

        assert_eq!(mailbox("[Gmail]/Important").display_depth(is_shown), 0);
        assert_eq!(mailbox("Maverick/HR").display_depth(is_shown), 1);
        assert_eq!(mailbox("Maverick").display_depth(is_shown), 0);
    }

    #[test]
    fn takes_the_leaf_after_the_delimiter() {
        assert_eq!(mailbox("[Gmail]/Sent Mail").leaf(), "Sent Mail");
        assert_eq!(mailbox("Maverick/Sent Mail").leaf(), "Sent Mail");
        assert_eq!(mailbox("INBOX").leaf(), "INBOX");
    }
}
