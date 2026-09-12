//! SQLite cache of mailboxes, envelopes and raw message bodies.
//!
//! The cache is what makes the client feel instant: switching folders reads
//! from here synchronously and paints immediately, while the IMAP worker
//! reconciles with the server in the background.
//!
//! Connections run in WAL mode, so the UI's reader connection and the mail
//! engine's writer connection never block each other.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::model::{Addr, Envelope, Flags, MailboxInfo, SpecialUse};
use crate::config::AccountId;

pub struct Store {
    conn: Mutex<Connection>,
}

/// Server-side identity of a mailbox's UID space, used to decide whether the
/// cached rows can be trusted or must be discarded.
#[derive(Debug, Clone, Copy, Default)]
pub struct MailboxState {
    pub uid_validity: u32,
    pub highest_uid: u32,
}

impl Store {
    /// Opens (creating if needed) the cache database and applies the schema.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Keep a generous page cache; envelope scans are the hot path.
        conn.pragma_update(None, "cache_size", -32_000i64)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { conn: Mutex::new(conn) };
        store.migrate()?;
        Ok(store)
    }

    /// An in-memory cache, used when no data directory is available.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn: Mutex::new(conn) };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS mailbox (
                account      INTEGER NOT NULL,
                name         TEXT    NOT NULL,
                delimiter    TEXT,
                special      INTEGER NOT NULL DEFAULT 7,
                selectable   INTEGER NOT NULL DEFAULT 1,
                exists_count INTEGER NOT NULL DEFAULT 0,
                unseen       INTEGER NOT NULL DEFAULT 0,
                uid_validity INTEGER NOT NULL DEFAULT 0,
                highest_uid  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (account, name)
            );

            CREATE TABLE IF NOT EXISTS envelope (
                account     INTEGER NOT NULL,
                mailbox     TEXT    NOT NULL,
                uid         INTEGER NOT NULL,
                subject     TEXT    NOT NULL DEFAULT '',
                from_addrs  TEXT    NOT NULL DEFAULT '[]',
                to_addrs    TEXT    NOT NULL DEFAULT '[]',
                cc_addrs    TEXT    NOT NULL DEFAULT '[]',
                date        INTEGER NOT NULL DEFAULT 0,
                flags       INTEGER NOT NULL DEFAULT 0,
                size        INTEGER NOT NULL DEFAULT 0,
                message_id  TEXT    NOT NULL DEFAULT '',
                in_reply_to TEXT    NOT NULL DEFAULT '',
                has_attach  INTEGER NOT NULL DEFAULT 0,
                preview     TEXT    NOT NULL DEFAULT '',
                PRIMARY KEY (account, mailbox, uid)
            );

            CREATE INDEX IF NOT EXISTS envelope_by_date
                ON envelope (account, mailbox, date DESC);

            CREATE TABLE IF NOT EXISTS body (
                account  INTEGER NOT NULL,
                mailbox  TEXT    NOT NULL,
                uid      INTEGER NOT NULL,
                fetched  INTEGER NOT NULL,
                raw      BLOB    NOT NULL,
                PRIMARY KEY (account, mailbox, uid)
            );

            CREATE INDEX IF NOT EXISTS body_by_age ON body (fetched);

            -- Addresses seen in mail, for completing recipients.
            CREATE TABLE IF NOT EXISTS contact (
                account   INTEGER NOT NULL,
                email     TEXT    NOT NULL,
                name      TEXT    NOT NULL DEFAULT '',
                weight    INTEGER NOT NULL DEFAULT 0,
                last_seen INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (account, email)
            );

            CREATE INDEX IF NOT EXISTS contact_rank
                ON contact (account, weight DESC, last_seen DESC);

            -- Remote-content permissions the user has granted. `kind` is 0
            -- for a single message and 1 for a sender address.
            CREATE TABLE IF NOT EXISTS remote_allowed (
                account INTEGER NOT NULL,
                kind    INTEGER NOT NULL,
                value   TEXT    NOT NULL,
                added   INTEGER NOT NULL,
                PRIMARY KEY (account, kind, value)
            );
            "#,
        )?;
        Ok(())
    }

    // -- mailboxes ---------------------------------------------------------

    /// Replaces the mailbox list for an account, preserving the cached UID
    /// state of mailboxes that still exist.
    pub fn save_mailboxes(&self, account: AccountId, boxes: &[MailboxInfo]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
            let mut keep = tx.prepare("SELECT name FROM mailbox WHERE account = ?1")?;
            let existing: Vec<String> = keep
                .query_map(params![account], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(keep);
            for gone in existing.iter().filter(|n| !names.contains(&n.as_str())) {
                tx.execute(
                    "DELETE FROM mailbox WHERE account = ?1 AND name = ?2",
                    params![account, gone],
                )?;
            }

            let mut up = tx.prepare(
                "INSERT INTO mailbox (account, name, delimiter, special, selectable)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account, name) DO UPDATE SET
                     delimiter  = excluded.delimiter,
                     special    = excluded.special,
                     selectable = excluded.selectable",
            )?;
            for b in boxes {
                // Unread counts are not written here: they come from STATUS
                // after listing, and a stale one is worse than none.
                up.execute(params![
                    account,
                    b.name,
                    b.delimiter,
                    special_to_i64(b.special),
                    b.selectable as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_mailboxes(&self, account: AccountId) -> Result<Vec<MailboxInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, delimiter, special, selectable, unseen
             FROM mailbox WHERE account = ?1",
        )?;
        let rows = stmt.query_map(params![account], |r| {
            Ok(MailboxInfo {
                name: r.get(0)?,
                delimiter: r.get(1)?,
                special: special_from_i64(r.get(2)?),
                selectable: r.get::<_, i64>(3)? != 0,
                unseen: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn mailbox_state(&self, account: AccountId, mailbox: &str) -> Result<MailboxState> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT uid_validity, highest_uid FROM mailbox WHERE account = ?1 AND name = ?2",
                params![account, mailbox],
                |r| Ok(MailboxState { uid_validity: r.get(0)?, highest_uid: r.get(1)? }),
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    pub fn set_mailbox_state(
        &self,
        account: AccountId,
        mailbox: &str,
        state: MailboxState,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mailbox (account, name, uid_validity, highest_uid)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account, name) DO UPDATE SET
                 uid_validity = excluded.uid_validity,
                 highest_uid  = max(highest_uid, excluded.highest_uid)",
            params![account, mailbox, state.uid_validity, state.highest_uid],
        )?;
        Ok(())
    }

    /// Drops every cached row for a mailbox. Used when `UIDVALIDITY` changes,
    /// which invalidates all UIDs the client holds.
    pub fn clear_mailbox(&self, account: AccountId, mailbox: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM envelope WHERE account = ?1 AND mailbox = ?2",
            params![account, mailbox],
        )?;
        conn.execute(
            "DELETE FROM body WHERE account = ?1 AND mailbox = ?2",
            params![account, mailbox],
        )?;
        conn.execute(
            "UPDATE mailbox SET highest_uid = 0 WHERE account = ?1 AND name = ?2",
            params![account, mailbox],
        )?;
        Ok(())
    }

    // -- envelopes ---------------------------------------------------------

    pub fn save_envelopes(
        &self,
        account: AccountId,
        mailbox: &str,
        envelopes: &[Envelope],
    ) -> Result<()> {
        if envelopes.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO envelope
                   (account, mailbox, uid, subject, from_addrs, to_addrs, cc_addrs,
                    date, flags, size, message_id, in_reply_to, has_attach, preview)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                 ON CONFLICT(account, mailbox, uid) DO UPDATE SET
                     subject     = excluded.subject,
                     from_addrs  = excluded.from_addrs,
                     to_addrs    = excluded.to_addrs,
                     cc_addrs    = excluded.cc_addrs,
                     date        = excluded.date,
                     flags       = excluded.flags,
                     size        = excluded.size,
                     message_id  = excluded.message_id,
                     in_reply_to = excluded.in_reply_to,
                     has_attach  = excluded.has_attach,
                     -- A refreshed envelope has no preview; keep the cached one.
                     preview     = CASE WHEN excluded.preview = '' THEN preview
                                        ELSE excluded.preview END",
            )?;
            for e in envelopes {
                stmt.execute(params![
                    account,
                    mailbox,
                    e.uid,
                    e.subject,
                    serde_json::to_string(&e.from)?,
                    serde_json::to_string(&e.to)?,
                    serde_json::to_string(&e.cc)?,
                    e.date,
                    e.flags.0,
                    e.size,
                    e.message_id,
                    e.in_reply_to,
                    e.has_attachments as i64,
                    e.preview,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Newest `limit` envelopes for a mailbox, most recent first.
    pub fn load_envelopes(
        &self,
        account: AccountId,
        mailbox: &str,
        limit: u32,
    ) -> Result<Vec<Envelope>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT uid, subject, from_addrs, to_addrs, cc_addrs, date, flags, size,
                    message_id, in_reply_to, has_attach, preview
             FROM envelope WHERE account = ?1 AND mailbox = ?2
             ORDER BY date DESC, uid DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account, mailbox, limit], |r| {
            Ok(Envelope {
                uid: r.get(0)?,
                mailbox: mailbox.to_string(),
                // Display-only and derived from a live fetch, so not cached.
                folder_hint: String::new(),
                subject: r.get(1)?,
                from: parse_addrs(r.get::<_, String>(2)?),
                to: parse_addrs(r.get::<_, String>(3)?),
                cc: parse_addrs(r.get::<_, String>(4)?),
                date: r.get(5)?,
                flags: Flags(r.get(6)?),
                size: r.get(7)?,
                message_id: r.get(8)?,
                in_reply_to: r.get(9)?,
                has_attachments: r.get::<_, i64>(10)? != 0,
                preview: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_envelopes(&self, account: AccountId, mailbox: &str, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut del_env = tx
                .prepare("DELETE FROM envelope WHERE account = ?1 AND mailbox = ?2 AND uid = ?3")?;
            let mut del_body =
                tx.prepare("DELETE FROM body WHERE account = ?1 AND mailbox = ?2 AND uid = ?3")?;
            for uid in uids {
                del_env.execute(params![account, mailbox, uid])?;
                del_body.execute(params![account, mailbox, uid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_flags(
        &self,
        account: AccountId,
        mailbox: &str,
        uids: &[u32],
        flags: Flags,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE envelope SET flags = ?4 WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            )?;
            for uid in uids {
                stmt.execute(params![account, mailbox, uid, flags.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_preview(
        &self,
        account: AccountId,
        mailbox: &str,
        uid: u32,
        preview: &str,
        has_attach: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE envelope SET preview = ?4, has_attach = ?5
             WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            params![account, mailbox, uid, preview, has_attach as i64],
        )?;
        Ok(())
    }

    // -- contacts ----------------------------------------------------------

    /// Records addresses seen in mail.
    ///
    /// `weight` says how much this sighting counts for: an address the user
    /// actually sent to is worth far more than one that merely appeared in a
    /// `From:` header, or a mailing list would outrank the people they write
    /// to. The display name is only overwritten when a better one turns up,
    /// since many senders put an address in the name slot.
    pub fn record_contacts(
        &self,
        account: AccountId,
        addresses: &[Addr],
        weight: i64,
    ) -> Result<()> {
        if addresses.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO contact (account, email, name, weight, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account, email) DO UPDATE SET
                     weight    = weight + excluded.weight,
                     last_seen = max(last_seen, excluded.last_seen),
                     name      = CASE
                                     WHEN excluded.name <> '' AND
                                          (name = '' OR name = email)
                                     THEN excluded.name ELSE name
                                 END",
            )?;
            let now = now_secs();
            for address in addresses {
                let email = address.email.trim().to_ascii_lowercase();
                // Anything without an @ is not an address we can complete to.
                if email.is_empty() || !email.contains('@') {
                    continue;
                }
                stmt.execute(params![account, email, address.name.trim(), weight, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// How many addresses are on file, used to tell a fresh account from one
    /// whose mail was cached before contacts were being recorded.
    pub fn contact_count(&self, account: AccountId) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT count(*) FROM contact WHERE account = ?1",
            params![account],
            |r| r.get(0),
        )?)
    }

    /// Addresses matching a typed fragment, best first.
    pub fn suggest_contacts(
        &self,
        account: AccountId,
        needle: &str,
        limit: u32,
    ) -> Result<Vec<Addr>> {
        let needle = needle.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!("%{}%", needle.replace('%', "\\%").replace('_', "\\_"));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, email FROM contact
             WHERE account = ?1 AND (email LIKE ?2 ESCAPE '\\' OR lower(name) LIKE ?2 ESCAPE '\\')
             ORDER BY
                 -- A match at the start is what the typist meant.
                 CASE WHEN email LIKE ?3 ESCAPE '\\' OR lower(name) LIKE ?3 ESCAPE '\\'
                      THEN 0 ELSE 1 END,
                 weight DESC, last_seen DESC, email
             LIMIT ?4",
        )?;
        let prefix = format!("{}%", needle.replace('%', "\\%").replace('_', "\\_"));
        let rows = stmt.query_map(params![account, pattern, prefix, limit], |r| {
            Ok(Addr { name: r.get(0)?, email: r.get(1)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // -- remote content permissions ----------------------------------------

    /// Records that one message may load remote content.
    pub fn allow_remote_message(&self, account: AccountId, key: &str) -> Result<()> {
        self.allow_remote(account, KIND_MESSAGE, key)
    }

    /// Records that every message from an address may load remote content.
    pub fn allow_remote_sender(&self, account: AccountId, address: &str) -> Result<()> {
        self.allow_remote(account, KIND_SENDER, &address.trim().to_ascii_lowercase())
    }

    fn allow_remote(&self, account: AccountId, kind: i64, value: &str) -> Result<()> {
        if value.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO remote_allowed (account, kind, value, added) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account, kind, value) DO UPDATE SET added = excluded.added",
            params![account, kind, value, now_secs()],
        )?;
        Ok(())
    }

    /// Whether this message may load remote content, either because it was
    /// allowed individually or because its sender is trusted.
    pub fn remote_allowed(
        &self,
        account: AccountId,
        message_key: &str,
        sender: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let found = conn
            .query_row(
                "SELECT 1 FROM remote_allowed
                 WHERE account = ?1
                   AND ((kind = ?2 AND value = ?3) OR (kind = ?4 AND value = ?5))
                 LIMIT 1",
                params![
                    account,
                    KIND_MESSAGE,
                    message_key,
                    KIND_SENDER,
                    sender.trim().to_ascii_lowercase()
                ],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// How many senders are trusted, for the settings summary.
    pub fn remote_sender_count(&self, account: AccountId) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT count(*) FROM remote_allowed WHERE account = ?1 AND kind = ?2",
            params![account, KIND_SENDER],
            |r| r.get(0),
        )?)
    }

    /// Revokes every remote-content permission for an account.
    pub fn forget_remote_permissions(&self, account: AccountId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM remote_allowed WHERE account = ?1", params![account])?;
        Ok(())
    }

    // -- bodies ------------------------------------------------------------

    pub fn save_raw(&self, account: AccountId, mailbox: &str, uid: u32, raw: &[u8]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO body (account, mailbox, uid, fetched, raw) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(account, mailbox, uid) DO UPDATE SET
                 fetched = excluded.fetched, raw = excluded.raw",
            params![account, mailbox, uid, now_secs(), raw],
        )?;
        Ok(())
    }

    pub fn load_raw(&self, account: AccountId, mailbox: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT raw FROM body WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
                params![account, mailbox, uid],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        if row.is_some() {
            // Touch so the eviction pass keeps recently read messages.
            let _ = conn.execute(
                "UPDATE body SET fetched = ?4 WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
                params![account, mailbox, uid, now_secs()],
            );
        }
        Ok(row)
    }

    pub fn has_body(&self, account: AccountId, mailbox: &str, uid: u32) -> bool {
        let Ok(conn) = self.conn.lock() else { return false };
        conn.query_row(
            "SELECT 1 FROM body WHERE account = ?1 AND mailbox = ?2 AND uid = ?3",
            params![account, mailbox, uid],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    /// Evicts least-recently-used bodies until the cache fits in `max_bytes`.
    pub fn prune_bodies(&self, max_bytes: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let total: i64 =
            conn.query_row("SELECT coalesce(sum(length(raw)), 0) FROM body", [], |r| r.get(0))?;
        if total <= max_bytes {
            return Ok(());
        }
        // Delete oldest-touched rows until under budget. Done in one statement
        // so a large cache does not require many round trips.
        conn.execute(
            "DELETE FROM body WHERE rowid IN (
                 SELECT rowid FROM (
                     SELECT rowid,
                            sum(length(raw)) OVER (ORDER BY fetched DESC) AS running
                     FROM body
                 ) WHERE running > ?1
             )",
            params![max_bytes],
        )?;
        Ok(())
    }

    /// Removes every trace of an account.
    pub fn forget_account(&self, account: AccountId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for table in ["envelope", "body", "mailbox", "remote_allowed", "contact"] {
            conn.execute(&format!("DELETE FROM {table} WHERE account = ?1"), params![account])?;
        }
        Ok(())
    }
}

/// Discriminators for `remote_allowed.kind`.
const KIND_MESSAGE: i64 = 0;
const KIND_SENDER: i64 = 1;

fn parse_addrs(json: String) -> Vec<Addr> {
    serde_json::from_str(&json).unwrap_or_default()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn special_to_i64(s: SpecialUse) -> i64 {
    match s {
        SpecialUse::Inbox => 0,
        SpecialUse::Sent => 1,
        SpecialUse::Drafts => 2,
        SpecialUse::Trash => 3,
        SpecialUse::Junk => 4,
        SpecialUse::Archive => 5,
        SpecialUse::All => 6,
        SpecialUse::Normal => 7,
    }
}

fn special_from_i64(v: i64) -> SpecialUse {
    match v {
        0 => SpecialUse::Inbox,
        1 => SpecialUse::Sent,
        2 => SpecialUse::Drafts,
        3 => SpecialUse::Trash,
        4 => SpecialUse::Junk,
        5 => SpecialUse::Archive,
        6 => SpecialUse::All,
        _ => SpecialUse::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_memory().expect("in-memory store")
    }

    #[test]
    fn remembers_a_single_message() {
        let store = store();
        assert!(!store.remote_allowed(1, "msg-1", "a@example.com").unwrap());

        store.allow_remote_message(1, "msg-1").unwrap();
        assert!(store.remote_allowed(1, "msg-1", "a@example.com").unwrap());
        // Allowing one message says nothing about the rest of the sender.
        assert!(!store.remote_allowed(1, "msg-2", "a@example.com").unwrap());
    }

    #[test]
    fn remembers_a_sender_for_every_message() {
        let store = store();
        store.allow_remote_sender(1, "Alerts@Example.com").unwrap();
        // Addresses are matched case-insensitively.
        assert!(store.remote_allowed(1, "msg-9", "alerts@example.com").unwrap());
        assert!(store.remote_allowed(1, "msg-8", "ALERTS@EXAMPLE.COM").unwrap());
        assert!(!store.remote_allowed(1, "msg-9", "other@example.com").unwrap());
    }

    #[test]
    fn keeps_permissions_per_account() {
        let store = store();
        store.allow_remote_sender(1, "a@example.com").unwrap();
        assert!(!store.remote_allowed(2, "m", "a@example.com").unwrap());
    }

    #[test]
    fn counts_and_revokes_sender_permissions() {
        let store = store();
        store.allow_remote_sender(1, "a@example.com").unwrap();
        store.allow_remote_sender(1, "b@example.com").unwrap();
        store.allow_remote_message(1, "msg-1").unwrap();
        assert_eq!(store.remote_sender_count(1).unwrap(), 2);

        store.forget_remote_permissions(1).unwrap();
        assert_eq!(store.remote_sender_count(1).unwrap(), 0);
        assert!(!store.remote_allowed(1, "msg-1", "a@example.com").unwrap());
    }

    #[test]
    fn ignores_empty_keys() {
        let store = store();
        store.allow_remote_message(1, "").unwrap();
        store.allow_remote_sender(1, "  ").unwrap();
        assert!(!store.remote_allowed(1, "", "").unwrap());
    }

    #[test]
    fn granting_twice_is_harmless() {
        let store = store();
        store.allow_remote_sender(1, "a@example.com").unwrap();
        store.allow_remote_sender(1, "a@example.com").unwrap();
        assert_eq!(store.remote_sender_count(1).unwrap(), 1);
    }
}

#[cfg(test)]
mod contact_tests {
    use super::*;

    fn addr(name: &str, email: &str) -> Addr {
        Addr { name: name.into(), email: email.into() }
    }

    #[test]
    fn suggests_what_was_recorded() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("Ada Lovelace", "ada@example.com")], 1).unwrap();

        let found = store.suggest_contacts(1, "ada", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].email, "ada@example.com");
        assert_eq!(found[0].name, "Ada Lovelace");

        // Names are searched too, not just addresses.
        assert_eq!(store.suggest_contacts(1, "lovelace", 10).unwrap().len(), 1);
        assert!(store.suggest_contacts(1, "babbage", 10).unwrap().is_empty());
    }

    #[test]
    fn matches_case_insensitively_and_stores_one_row_per_address() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("Ada", "Ada@Example.com")], 1).unwrap();
        store.record_contacts(1, &[addr("Ada", "ada@example.COM")], 1).unwrap();

        let found = store.suggest_contacts(1, "ADA", 10).unwrap();
        assert_eq!(found.len(), 1, "the same address was stored twice");
    }

    #[test]
    fn ranks_heavier_and_prefix_matches_first() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("", "someone@example.com")], 1).unwrap();
        store.record_contacts(1, &[addr("", "sam@example.com")], 20).unwrap();

        let found = store.suggest_contacts(1, "sam", 10).unwrap();
        assert_eq!(found[0].email, "sam@example.com", "the heavier match lost");

        // "one" appears mid-address in one and not at all in the other.
        let found = store.suggest_contacts(1, "one", 10).unwrap();
        assert_eq!(found[0].email, "someone@example.com");
    }

    #[test]
    fn a_better_name_replaces_a_placeholder_one() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("", "ada@example.com")], 1).unwrap();
        store.record_contacts(1, &[addr("Ada Lovelace", "ada@example.com")], 1).unwrap();
        assert_eq!(store.suggest_contacts(1, "ada", 10).unwrap()[0].name, "Ada Lovelace");

        // But a real name is not replaced by a later empty one.
        store.record_contacts(1, &[addr("", "ada@example.com")], 1).unwrap();
        assert_eq!(store.suggest_contacts(1, "ada", 10).unwrap()[0].name, "Ada Lovelace");
    }

    #[test]
    fn ignores_entries_that_are_not_addresses() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("Nobody", "not-an-address")], 1).unwrap();
        store.record_contacts(1, &[addr("", "")], 1).unwrap();
        assert!(store.suggest_contacts(1, "nobody", 10).unwrap().is_empty());
    }

    #[test]
    fn wildcards_in_the_query_are_literal() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("", "ada@example.com")], 1).unwrap();
        // "%" would match everything if it reached LIKE unescaped.
        assert!(store.suggest_contacts(1, "%", 10).unwrap().is_empty());
        assert!(store.suggest_contacts(1, "a_a@", 10).unwrap().is_empty());
    }

    #[test]
    fn keeps_contacts_per_account() {
        let store = Store::open_memory().unwrap();
        store.record_contacts(1, &[addr("", "ada@example.com")], 1).unwrap();
        assert!(store.suggest_contacts(2, "ada", 10).unwrap().is_empty());
    }
}
