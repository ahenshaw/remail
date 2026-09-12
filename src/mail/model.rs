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
    ///
    /// A display name holding a comma, or any other character RFC 5322 gives
    /// a meaning to, is quoted. Unquoted it would read as the end of one
    /// address and the start of another: "Doe, Jane" becomes two recipients,
    /// neither of which exists.
    pub fn full(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            return self.email.clone();
        }
        if name.contains(|c| "(),:;<>@[]\\\"".contains(c)) {
            let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\" <{}>", self.email)
        } else {
            format!("{name} <{}>", self.email)
        }
    }
}

/// Identifies a row in the message list.
///
/// A UID is only unique within its mailbox, so any view that can span
/// mailboxes — a cross-folder search — needs the mailbox as part of the
/// identity. Using it everywhere keeps one code path.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowKey {
    pub mailbox: String,
    pub uid: u32,
}

impl RowKey {
    pub fn new(mailbox: impl Into<String>, uid: u32) -> Self {
        Self { mailbox: mailbox.into(), uid }
    }
}

/// Everything the message list needs to draw a row, without the body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Envelope {
    pub uid: u32,
    /// Mailbox this message lives in. Set on every envelope so search
    /// results spanning folders stay unambiguous, and used as the target of
    /// every server operation on the row.
    #[serde(default)]
    pub mailbox: String,
    /// Where to *tell the user* the message lives, when that differs from
    /// `mailbox`. Gmail searches run against All Mail, which is a union of
    /// every label rather than a place, so the labels are shown instead.
    #[serde(default, skip)]
    pub folder_hint: String,
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
    /// The folder to display for this row.
    pub fn folder_label(&self) -> &str {
        if !self.folder_hint.is_empty() {
            return display_folder(&self.folder_hint);
        }
        let leaf = match self.mailbox.rsplit(['/', '.']).next() {
            Some(leaf) if !leaf.is_empty() => leaf,
            _ => &self.mailbox,
        };
        display_folder(leaf)
    }

    pub fn key(&self) -> RowKey {
        RowKey::new(self.mailbox.clone(), self.uid)
    }

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

/// Presentable form of a folder name.
///
/// `INBOX` is a protocol keyword that IMAP requires be spelled that way on the
/// wire; shouting it in the sidebar is an implementation detail leaking out.
pub fn display_folder(name: &str) -> &str {
    if name.eq_ignore_ascii_case("INBOX") { "Inbox" } else { name }
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
    /// Unread messages, from `STATUS`. Zero until first counted.
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

    /// The leaf as it should be shown.
    pub fn display_name(&self) -> &str {
        display_folder(self.leaf())
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
    /// Needed to build a `data:` URL when the message is written out for
    /// printing, where `cid:` references have nothing to resolve against.
    pub mime: String,
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

impl MessageKey {
    pub fn row(&self) -> RowKey {
        RowKey::new(self.mailbox.clone(), self.uid)
    }
}

/// How widely a search reaches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchScope {
    /// The open mailbox only.
    #[default]
    Folder,
    /// The open mailbox and everything nested under it.
    Subtree,
    /// Every selectable mailbox in the account.
    All,
}

impl SearchScope {
    pub fn label(self) -> &'static str {
        match self {
            SearchScope::Folder => "This folder",
            SearchScope::Subtree => "With subfolders",
            SearchScope::All => "All folders",
        }
    }

    pub fn all() -> [SearchScope; 3] {
        [SearchScope::Folder, SearchScope::Subtree, SearchScope::All]
    }
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
    fn quotes_display_names_that_would_otherwise_split() {
        let plain = Addr { name: "Ada Lovelace".into(), email: "ada@example.com".into() };
        assert_eq!(plain.full(), "Ada Lovelace <ada@example.com>");

        let comma = Addr { name: "Doe, Jane".into(), email: "jane@example.com".into() };
        assert_eq!(comma.full(), "\"Doe, Jane\" <jane@example.com>");

        // Quotes and backslashes inside the name are escaped, not dropped.
        let tricky = Addr { name: "A \"B\" C".into(), email: "x@example.com".into() };
        assert_eq!(tricky.full(), "\"A \\\"B\\\" C\" <x@example.com>");

        let nameless = Addr { name: "  ".into(), email: "x@example.com".into() };
        assert_eq!(nameless.full(), "x@example.com");
    }

    #[test]
    fn spells_inbox_as_a_word() {
        assert_eq!(display_folder("INBOX"), "Inbox");
        assert_eq!(display_folder("inbox"), "Inbox");
        // Only the mailbox of that exact name; a user folder is left alone.
        assert_eq!(display_folder("Inbox archive"), "Inbox archive");
        assert_eq!(display_folder("Sent"), "Sent");

        let inbox = MailboxInfo { name: "INBOX".into(), ..Default::default() };
        assert_eq!(inbox.display_name(), "Inbox");
        let envelope = Envelope { mailbox: "INBOX".into(), ..Default::default() };
        assert_eq!(envelope.folder_label(), "Inbox");
    }

    #[test]
    fn shows_the_mailbox_leaf_when_there_is_no_hint() {
        let envelope = Envelope { mailbox: "Maverick/HR".into(), ..Default::default() };
        assert_eq!(envelope.folder_label(), "HR");
    }

    #[test]
    fn a_hint_wins_over_the_mailbox() {
        let envelope = Envelope {
            mailbox: "[Gmail]/All Mail".into(),
            folder_hint: "Receipts".into(),
            ..Default::default()
        };
        assert_eq!(envelope.folder_label(), "Receipts");
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
