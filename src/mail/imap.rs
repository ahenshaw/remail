//! IMAP transport: connection setup, authentication, and the small set of
//! operations the client performs.
//!
//! Sync strategy: the client tracks `UIDVALIDITY` and the highest UID it has
//! seen per mailbox. A sync fetches only headers for UIDs above that mark, then
//! re-reads flags for the cached window so reads and stars made elsewhere show
//! up. Bodies are never fetched during sync; they are pulled on demand and
//! cached.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_imap::extensions::idle::IdleResponse;
use async_imap::types::{Fetch, Flag, Name, NameAttribute};
use async_imap::{Client, Session};
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::model::{Envelope, Flags, MailboxInfo, SpecialUse};
use super::parse;
use crate::auth::Credential;
use crate::config::{AccountConfig, Encryption};

/// Header fields worth fetching for the message list. Requesting a subset
/// rather than the whole header block keeps the initial sync small.
const ENVELOPE_HEADERS: &str =
    "BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID IN-REPLY-TO CONTENT-TYPE)]";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// IMAP requires servers to accept at least 29 minutes of IDLE; renew earlier.
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

type Stream = TlsStream<TcpStream>;

pub struct ImapConnection {
    session: Session<Stream>,
    capabilities: HashSet<String>,
    selected: Option<String>,
}

/// The parts of a `SELECT` response the client acts on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Selected {
    pub uid_validity: u32,
    pub uid_next: u32,
}

/// What ended an IDLE wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    /// The server reported activity; the caller should resync.
    Changed,
    /// The wait elapsed without news; the caller should renew IDLE.
    TimedOut,
}

impl ImapConnection {
    /// Opens a connection, negotiates TLS and authenticates.
    pub async fn connect(account: &AccountConfig, credential: &Credential) -> Result<Self> {
        let addr = (account.imap_host.as_str(), account.imap_port);
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .with_context(|| format!("connecting to {}:{}", account.imap_host, account.imap_port))?
            .with_context(|| {
                format!("connecting to {}:{}", account.imap_host, account.imap_port)
            })?;
        // Mail is latency-bound on small commands, not throughput-bound.
        tcp.set_nodelay(true).ok();

        let stream = match account.imap_encryption {
            Encryption::Tls => tls_wrap(tcp, &account.imap_host).await?,
            Encryption::StartTls => {
                // Greet in the clear, upgrade, then continue on the same socket.
                let mut plain = Client::new(tcp);
                read_greeting(&mut plain).await?;
                plain
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .context("server refused STARTTLS")?;
                tls_wrap(plain.into_inner(), &account.imap_host).await?
            }
        };

        let mut client = Client::new(stream);
        // Consume the greeting the server sends before any command. After a
        // STARTTLS upgrade the greeting was already read on the plain socket.
        if account.imap_encryption == Encryption::Tls {
            read_greeting(&mut client).await?;
        }

        let session = match credential {
            Credential::Password(password) => client
                .login(&account.username, password)
                .await
                .map_err(|(e, _)| anyhow!("IMAP login failed: {e}"))?,
            Credential::Bearer(token) => {
                let auth = XOAuth2 { user: account.username.clone(), token: token.clone() };
                client
                    .authenticate("XOAUTH2", auth)
                    .await
                    .map_err(|(e, _)| anyhow!("IMAP XOAUTH2 authentication failed: {e}"))?
            }
        };

        let mut conn = Self { session, capabilities: HashSet::new(), selected: None };
        conn.load_capabilities().await;
        Ok(conn)
    }

    async fn load_capabilities(&mut self) {
        if let Ok(caps) = self.session.capabilities().await {
            self.capabilities =
                caps.iter().map(|c| format!("{c:?}").to_ascii_uppercase()).collect();
        }
    }

    fn has_capability(&self, name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        self.capabilities.iter().any(|c| c.contains(&upper))
    }

    pub fn supports_idle(&self) -> bool {
        self.has_capability("IDLE")
    }

    pub fn selected_mailbox(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Lists every mailbox, resolving special-use roles.
    pub async fn list_mailboxes(&mut self) -> Result<Vec<MailboxInfo>> {
        let names: Vec<Name> = self
            .session
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await
            .context("listing mailboxes")?;

        // A server that labels its own folders is authoritative. Guessing on
        // top of that promotes any user folder that happens to be called
        // "Sent Mail" into a second Sent folder.
        let advertises_special_use =
            names.iter().any(|name| name.attributes().iter().any(is_special_use_attribute));

        let mut boxes: Vec<MailboxInfo> = names.iter().map(mailbox_from_name).collect();
        classify(&mut boxes, advertises_special_use);
        boxes.sort_by(|a, b| {
            a.special.rank().cmp(&b.special.rank()).then_with(|| a.name.cmp(&b.name))
        });
        Ok(boxes)
    }

    /// Selects a mailbox for read-write access, unless it is already selected.
    pub async fn select(&mut self, mailbox: &str) -> Result<Selected> {
        let info = self
            .session
            .select(mailbox)
            .await
            .with_context(|| format!("selecting mailbox {mailbox}"))?;
        self.selected = Some(mailbox.to_string());
        // `SELECT` also reports UNSEEN, but that is the sequence number of
        // the first unseen message, not a count of them. Counts come from
        // `STATUS`; see `unread_count`.
        Ok(Selected {
            uid_validity: info.uid_validity.unwrap_or(0),
            uid_next: info.uid_next.unwrap_or(0),
        })
    }

    /// Re-selects the mailbox so `EXISTS` and `UIDNEXT` are refreshed. Cheaper
    /// than reconnecting and works on servers without `STATUS` on the selected
    /// mailbox.
    pub async fn reselect(&mut self, mailbox: &str) -> Result<Selected> {
        self.selected = None;
        self.select(mailbox).await
    }

    /// How many unread messages a mailbox holds.
    ///
    /// `STATUS` is the only command that answers this directly. RFC 3501 says
    /// a server should not be asked about the mailbox that is currently
    /// selected, so that one is counted with `SEARCH UNSEEN` instead.
    pub async fn unread_count(&mut self, mailbox: &str) -> Result<u32> {
        if self.selected.as_deref() == Some(mailbox) {
            return Ok(self.session.search("UNSEEN").await?.len() as u32);
        }

        let status = self
            .session
            .status(mailbox, "(UNSEEN)")
            .await
            .with_context(|| format!("reading the status of {mailbox}"))?;
        Ok(status.unseen.unwrap_or(0))
    }

    /// Fetches envelopes for a UID range such as `"1000:*"`.
    pub async fn fetch_envelopes(&mut self, range: &str) -> Result<Vec<Envelope>> {
        // Gmail's labels say where a message actually lives, which the
        // mailbox cannot when the search ran against All Mail.
        let labels = if self.supports_gmail_labels() { " X-GM-LABELS" } else { "" };
        let query = format!("(UID FLAGS RFC822.SIZE{labels} {ENVELOPE_HEADERS})");

        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(range, query)
            .await?
            .try_collect()
            .await
            .with_context(|| format!("fetching envelopes for {range}"))?;

        let mailbox = self.selected.clone().unwrap_or_default();
        Ok(fetches.iter().filter_map(|fetch| envelope_from_fetch(fetch, &mailbox)).collect())
    }

    /// Whether the server implements Gmail's IMAP extensions.
    pub fn supports_gmail_labels(&self) -> bool {
        self.has_capability("X-GM-EXT-1")
    }

    /// Fetches only UIDs and flags, used to reconcile reads and stars made in
    /// another client.
    pub async fn fetch_flags(&mut self, range: &str) -> Result<Vec<(u32, Flags)>> {
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(range, "(UID FLAGS)")
            .await?
            .try_collect()
            .await
            .with_context(|| format!("fetching flags for {range}"))?;

        Ok(fetches.iter().filter_map(|f| Some((f.uid?, flags_from_fetch(f)))).collect())
    }

    /// Fetches one complete message. `BODY.PEEK[]` so reading does not
    /// implicitly mark it `\Seen`; that is the client's decision to make.
    pub async fn fetch_raw(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .await?
            .try_collect()
            .await
            .with_context(|| format!("fetching message {uid}"))?;

        Ok(fetches.first().and_then(|f| f.body().map(<[u8]>::to_vec)))
    }

    /// Adds or removes one flag across a set of UIDs.
    pub async fn store_flag(&mut self, uids: &[u32], flag_bit: u16, add: bool) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let verb = if add { "+FLAGS.SILENT" } else { "-FLAGS.SILENT" };
        let query = format!("{verb} ({})", Flags::imap_name(flag_bit));
        let stream = self.session.uid_store(uid_set(uids), query).await?;
        // The response stream must be drained before the next command.
        let _: Vec<Fetch> = stream.try_collect().await?;
        Ok(())
    }

    /// Moves messages to another mailbox, falling back to copy + delete when
    /// the server lacks RFC 6851 `MOVE`.
    pub async fn move_messages(&mut self, uids: &[u32], destination: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let set = uid_set(uids);
        if self.has_capability("MOVE") {
            self.session
                .uid_mv(&set, destination)
                .await
                .with_context(|| format!("moving messages to {destination}"))?;
            return Ok(());
        }

        self.session
            .uid_copy(&set, destination)
            .await
            .with_context(|| format!("copying messages to {destination}"))?;
        self.store_flag(uids, Flags::DELETED, true).await?;
        self.expunge(uids).await
    }

    /// Permanently removes messages. Uses `UID EXPUNGE` where available so
    /// unrelated messages already marked `\Deleted` are left alone.
    pub async fn expunge(&mut self, uids: &[u32]) -> Result<()> {
        if self.has_capability("UIDPLUS") && !uids.is_empty() {
            let stream = self.session.uid_expunge(uid_set(uids)).await?;
            let _: Vec<u32> = stream.try_collect().await?;
        } else {
            let stream = self.session.expunge().await?;
            let _: Vec<u32> = stream.try_collect().await?;
        }
        Ok(())
    }

    /// Marks every message in the selected mailbox as read.
    ///
    /// Uses a sequence set rather than UIDs: `1:*` covers the mailbox without
    /// first having to learn what is in it.
    pub async fn mark_all_seen(&mut self, mailbox: &str) -> Result<()> {
        if self.selected.as_deref() != Some(mailbox) {
            self.select(mailbox).await?;
        }
        let stream = self
            .session
            .store("1:*", format!("+FLAGS.SILENT ({})", Flags::imap_name(Flags::SEEN)))
            .await
            .with_context(|| format!("marking {mailbox} read"))?;
        // The response must be drained before the next command.
        let _: Vec<Fetch> = stream.try_collect().await?;
        Ok(())
    }

    pub async fn create_mailbox(&mut self, name: &str) -> Result<()> {
        self.session.create(name).await.with_context(|| format!("creating {name}"))?;
        // Servers vary on whether a new mailbox is subscribed; do it so the
        // folder shows up in clients that list subscriptions.
        let _ = self.session.subscribe(name).await;
        Ok(())
    }

    pub async fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<()> {
        // A selected mailbox cannot be renamed on some servers.
        if self.selected.is_some() {
            let _ = self.session.close().await;
            self.selected = None;
        }
        self.session.rename(from, to).await.with_context(|| format!("renaming {from} to {to}"))?;
        let _ = self.session.unsubscribe(from).await;
        let _ = self.session.subscribe(to).await;
        Ok(())
    }

    pub async fn delete_mailbox(&mut self, name: &str) -> Result<()> {
        if self.selected.as_deref() == Some(name) {
            let _ = self.session.close().await;
            self.selected = None;
        }
        self.session.delete(name).await.with_context(|| format!("deleting {name}"))?;
        let _ = self.session.unsubscribe(name).await;
        Ok(())
    }

    /// Joins a parent path and a new child name with the server's delimiter.
    pub fn child_path(parent: &str, delimiter: Option<&str>, name: &str) -> String {
        let delimiter = delimiter.filter(|d| !d.is_empty()).unwrap_or("/");
        if parent.is_empty() { name.to_string() } else { format!("{parent}{delimiter}{name}") }
    }

    /// Appends a message to a mailbox, used to file sent mail.
    pub async fn append(&mut self, mailbox: &str, raw: &[u8], flags: &[&str]) -> Result<()> {
        let flags = (!flags.is_empty()).then(|| format!("({})", flags.join(" ")));
        self.session
            .append(mailbox, flags.as_deref(), None, raw)
            .await
            .with_context(|| format!("appending to {mailbox}"))?;
        Ok(())
    }

    /// Runs a server-side search, returning matching UIDs.
    pub async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        let set = self.session.uid_search(query).await.context("searching")?;
        let mut uids: Vec<u32> = set.into_iter().collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(uids)
    }

    /// Waits for server-side activity on the selected mailbox.
    ///
    /// Consumes and returns the connection because IDLE takes ownership of the
    /// session for its duration.
    pub async fn idle(mut self, cancel: &tokio::sync::Notify) -> Result<(Self, IdleOutcome)> {
        let mut handle = self.session.idle();
        handle.init().await.context("entering IDLE")?;

        // The wait future borrows the handle, so it has to be dropped before
        // IDLE can be ended; keep it confined to this block.
        let result = {
            let (wait, stop) = handle.wait_with_timeout(IDLE_TIMEOUT);
            tokio::pin!(wait);
            let result = tokio::select! {
                result = &mut wait => Some(result),
                // Dropping the stop source below ends the wait cleanly.
                _ = cancel.notified() => None,
            };
            drop(stop);
            result
        };

        let session = handle.done().await.map_err(|e| anyhow!("leaving IDLE: {e}"))?;
        self.session = session;

        let outcome = match result {
            Some(Ok(IdleResponse::Timeout)) => IdleOutcome::TimedOut,
            Some(Ok(_)) | None => IdleOutcome::Changed,
            // Surface the failure so the caller reconnects rather than
            // spinning on a dead socket.
            Some(Err(e)) => return Err(anyhow!("IDLE failed: {e}")),
        };
        Ok((self, outcome))
    }

    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }
}

/// Authenticator for the `XOAUTH2` SASL mechanism.
///
/// `async-imap` base64-encodes whatever `process` returns, so this yields the
/// raw SASL string. On failure Gmail sends a second challenge carrying a JSON
/// error and expects an empty response before it will report `NO`.
struct XOAuth2 {
    user: String,
    token: String,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = Vec<u8>;

    fn process(&mut self, challenge: &[u8]) -> Self::Response {
        if challenge.is_empty() && !self.token.is_empty() {
            let response =
                format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token).into_bytes();
            // Only answer the first challenge with credentials.
            self.token.clear();
            response
        } else {
            Vec::new()
        }
    }
}

/// Reads the server's untagged greeting. A connection that closes or errors
/// here is dead, and saying so beats failing later inside `LOGIN`.
async fn read_greeting<T>(client: &mut Client<T>) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    match client.read_response().await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => bail!("server closed the connection before sending a greeting"),
        Err(e) => Err(anyhow!("reading IMAP greeting: {e}")),
    }
}

async fn tls_wrap(tcp: TcpStream, host: &str) -> Result<Stream> {
    let connector = TlsConnector::from(tls_config());
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
        .with_context(|| format!("invalid TLS server name {host}"))?;
    connector.connect(server_name, tcp).await.with_context(|| format!("TLS handshake with {host}"))
}

/// One shared client config: building it parses the full root store, which is
/// far too expensive to redo per connection.
fn tls_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            Arc::new(
                rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth(),
            )
        })
        .clone()
}

fn mailbox_from_name(name: &Name) -> MailboxInfo {
    let mut special = SpecialUse::Normal;
    let mut selectable = true;
    for attr in name.attributes() {
        match attr {
            NameAttribute::NoSelect => selectable = false,
            NameAttribute::All => special = SpecialUse::All,
            NameAttribute::Archive => special = SpecialUse::Archive,
            NameAttribute::Drafts => special = SpecialUse::Drafts,
            NameAttribute::Junk => special = SpecialUse::Junk,
            NameAttribute::Sent => special = SpecialUse::Sent,
            NameAttribute::Trash => special = SpecialUse::Trash,
            _ => {}
        }
    }
    if name.name().eq_ignore_ascii_case("INBOX") {
        special = SpecialUse::Inbox;
    }

    MailboxInfo {
        name: name.name().to_string(),
        delimiter: name.delimiter().map(str::to_string),
        special,
        selectable,
        unseen: 0,
    }
}

fn is_special_use_attribute(attribute: &NameAttribute<'_>) -> bool {
    matches!(
        attribute,
        NameAttribute::All
            | NameAttribute::Archive
            | NameAttribute::Drafts
            | NameAttribute::Junk
            | NameAttribute::Sent
            | NameAttribute::Trash
    )
}

/// Fills in special-use roles the server did not provide.
///
/// The name-based fallback only runs when the server advertises no RFC 6154
/// attributes anywhere. Otherwise its silence about a folder is meaningful:
/// the folder is not special, whatever it happens to be called.
fn classify(mailboxes: &mut [MailboxInfo], advertises_special_use: bool) {
    if advertises_special_use {
        return;
    }
    for mailbox in mailboxes.iter_mut().filter(|m| m.special == SpecialUse::Normal) {
        mailbox.special = guess_special_use(&mailbox.name);
    }
}

/// Fallback for servers that do not advertise RFC 6154 attributes.
fn guess_special_use(name: &str) -> SpecialUse {
    let leaf = name.rsplit(['/', '.']).next().unwrap_or(name).to_ascii_lowercase();
    match leaf.as_str() {
        "sent" | "sent mail" | "sent items" | "sent messages" => SpecialUse::Sent,
        "draft" | "drafts" => SpecialUse::Drafts,
        "trash" | "deleted" | "deleted items" | "bin" => SpecialUse::Trash,
        "spam" | "junk" | "junk e-mail" | "bulk mail" => SpecialUse::Junk,
        "archive" | "archives" | "all mail" => SpecialUse::Archive,
        _ => SpecialUse::Normal,
    }
}

fn envelope_from_fetch(fetch: &Fetch, mailbox: &str) -> Option<Envelope> {
    let uid = fetch.uid?;
    let header = fetch.header().unwrap_or(b"");
    let mut envelope = parse::parse_envelope(uid, header);
    envelope.mailbox = mailbox.to_string();
    envelope.flags = flags_from_fetch(fetch);
    envelope.size = fetch.size.unwrap_or(0);
    // The header block alone cannot show attachments; a multipart container is
    // the best signal available until the body is fetched.
    envelope.has_attachments = header_suggests_attachments(header);
    if let Some(labels) = fetch.gmail_labels() {
        envelope.folder_hint = gmail_folder(labels.iter().map(|l| l.as_ref()));
    }
    if envelope.date == 0
        && let Some(internal) = fetch.internal_date()
    {
        envelope.date = internal.timestamp();
    }
    Some(envelope)
}

/// Picks the label to show for a Gmail message.
///
/// Gmail exposes labels rather than folders, and a message usually carries
/// several: system markers such as `\\Important` or `\\Starred` that say
/// nothing about where it lives, plus the user's own. A user label is the
/// most informative, so it wins; otherwise the system label that does denote
/// a place is used.
fn gmail_folder<'a>(labels: impl Iterator<Item = &'a str>) -> String {
    let mut system: Option<String> = None;

    for label in labels {
        let label = label.trim().trim_matches('"');
        if label.is_empty() {
            continue;
        }

        // System labels arrive backslash-prefixed, and the escaping survives
        // parsing unevenly: the same label can reach us as `\Inbox` or
        // `\\Inbox`. Strip whatever is there rather than a fixed count.
        let marker = label.trim_start_matches('\\');
        if marker.len() == label.len() {
            // A user label. Gmail nests with '/', and the leaf is enough.
            return marker.rsplit('/').next().unwrap_or(marker).to_string();
        }
        // Markers that are states, not places.
        if matches!(marker, "Important" | "Starred" | "Unread" | "Muted") {
            continue;
        }
        if system.is_none() {
            system = Some(match marker {
                "Inbox" => "Inbox".to_string(),
                "Sent" => "Sent".to_string(),
                "Draft" | "Drafts" => "Drafts".to_string(),
                "Trash" => "Trash".to_string(),
                "Junk" | "Spam" => "Spam".to_string(),
                "All" | "AllMail" => "All Mail".to_string(),
                other => other.to_string(),
            });
        }
    }
    system.unwrap_or_default()
}

fn header_suggests_attachments(header: &[u8]) -> bool {
    let text = String::from_utf8_lossy(header).to_ascii_lowercase();
    text.contains("multipart/mixed")
}

fn flags_from_fetch(fetch: &Fetch) -> Flags {
    let mut flags = Flags::default();
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => flags.set(Flags::SEEN, true),
            Flag::Answered => flags.set(Flags::ANSWERED, true),
            Flag::Flagged => flags.set(Flags::FLAGGED, true),
            Flag::Deleted => flags.set(Flags::DELETED, true),
            Flag::Draft => flags.set(Flags::DRAFT, true),
            Flag::Recent => flags.set(Flags::RECENT, true),
            _ => {}
        }
    }
    flags
}

/// Collapses a UID list into IMAP sequence-set notation (`3,5:8,11`), which
/// keeps commands short enough to avoid literal continuations.
pub fn uid_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut out = String::new();
    let mut start = sorted[0];
    let mut prev = sorted[0];
    for &uid in &sorted[1..] {
        if uid == prev + 1 {
            prev = uid;
            continue;
        }
        append_range(&mut out, start, prev);
        start = uid;
        prev = uid;
    }
    append_range(&mut out, start, prev);
    out
}

fn append_range(out: &mut String, start: u32, end: u32) {
    if !out.is_empty() {
        out.push(',');
    }
    if start == end {
        out.push_str(&start.to_string());
    } else {
        out.push_str(&format!("{start}:{end}"));
    }
}

/// The UID range covering the newest `count` messages, given `uid_next`.
///
/// UIDs are sparse, so this is an upper bound on how far back to look rather
/// than an exact count; the server returns whatever exists in the window.
pub fn recent_range(uid_next: u32, count: u32) -> String {
    let start = uid_next.saturating_sub(count).max(1);
    format!("{start}:*")
}

/// Escapes a string for use as an IMAP quoted argument in `SEARCH`.
pub fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""))
}

/// Builds a `SEARCH` command for a free-text query across the usual fields.
pub fn text_search(query: &str) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("empty search");
    }
    Ok(format!("OR OR SUBJECT {q} FROM {q} TO {q}", q = quote(query)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_uid_runs() {
        assert_eq!(uid_set(&[3, 4, 5, 9, 11, 12]), "3:5,9,11:12");
        assert_eq!(uid_set(&[7]), "7");
        assert_eq!(uid_set(&[]), "");
    }

    #[test]
    fn sorts_and_dedups_uids() {
        assert_eq!(uid_set(&[5, 3, 4, 4]), "3:5");
    }

    #[test]
    fn windows_the_recent_range() {
        assert_eq!(recent_range(1000, 200), "800:*");
        // Never asks for UID 0, which is not a valid UID.
        assert_eq!(recent_range(50, 500), "1:*");
    }

    #[test]
    fn quotes_search_terms() {
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert!(text_search("report").unwrap().contains("SUBJECT \"report\""));
        assert!(text_search("  ").is_err());
    }

    /// Renders the sidebar tree for the mailboxes in the local cache, using
    /// the real classification and indentation rules. Lets folder-layout
    /// changes be checked without launching the UI.
    ///
    /// The cache stores folder names, not the server's `SPECIAL-USE`
    /// attributes, so roles cannot be reconstructed here: the well-known
    /// folders print as ordinary ones. What this does show faithfully is the
    /// nesting and which folders stay ordinary.
    #[test]
    #[ignore = "reads the local message cache"]
    fn print_sidebar_tree() {
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
        let mut statement =
            conn.prepare("SELECT name, delimiter, selectable FROM mailbox").unwrap();
        let mut boxes: Vec<MailboxInfo> = statement
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let inbox = name.eq_ignore_ascii_case("INBOX");
                Ok(MailboxInfo {
                    name,
                    delimiter: row.get(1)?,
                    // Roles come from the server; start from what attributes
                    // alone would give, which is Inbox and nothing else.
                    special: if inbox { SpecialUse::Inbox } else { SpecialUse::Normal },
                    selectable: row.get::<_, i64>(2)? != 0,
                    unseen: 0,
                })
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();

        // Gmail advertises SPECIAL-USE, so the name fallback stays off.
        classify(&mut boxes, true);
        boxes.sort_by(|a, b| {
            a.special.rank().cmp(&b.special.rank()).then_with(|| a.name.cmp(&b.name))
        });

        let shown: std::collections::HashSet<&str> =
            boxes.iter().filter(|m| m.selectable).map(|m| m.name.as_str()).collect();
        println!("--- sidebar ---");
        for mailbox in boxes.iter().filter(|m| m.selectable) {
            let depth = mailbox.display_depth(|path| shown.contains(path));
            println!(
                "{}{:<9} {}",
                "    ".repeat(depth),
                format!("[{:?}]", mailbox.special),
                mailbox.display_name()
            );
        }
    }

    /// Fetches a few envelopes from the account's All Mail and prints where
    /// each one reports living. Verifies the `X-GM-LABELS` path, which no
    /// offline test can reach.
    #[tokio::test]
    #[ignore = "requires a signed-in account"]
    async fn prints_gmail_folder_hints() {
        let config = crate::config::Config::load().expect("config");
        let Some(account) = config.accounts.iter().find(|a| a.enabled) else {
            println!("no account configured");
            return;
        };
        let tokens = crate::auth::TokenStore::new();
        let Ok(credential) = tokens.credential(account).await else {
            println!("account is not signed in");
            return;
        };

        let mut connection = ImapConnection::connect(account, &credential).await.expect("connect");
        println!("X-GM-EXT-1 supported: {}", connection.supports_gmail_labels());

        let mailboxes = connection.list_mailboxes().await.expect("list");
        let all = mailboxes
            .iter()
            .find(|m| m.special == SpecialUse::All)
            .map(|m| m.name.clone())
            .expect("an All Mail mailbox");

        let selected = connection.select(&all).await.expect("select");
        let range = recent_range(selected.uid_next, 400);
        let envelopes = connection.fetch_envelopes(&range).await.expect("fetch");

        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for envelope in &envelopes {
            *counts.entry(envelope.folder_label().to_string()).or_default() += 1;
        }
        println!("{} messages, folders shown:", envelopes.len());
        for (folder, count) in counts {
            let shown = if folder.is_empty() { "(none)" } else { &folder };
            println!("  {shown:<22}{count}");
        }
        connection.logout().await;
    }

    /// Runs a whole-account search the way the engine does and reports where
    /// the hits are, so a folder being missed shows up as a count rather than
    /// as a user noticing later.
    #[tokio::test]
    #[ignore = "requires a signed-in account"]
    async fn reports_where_a_search_finds_things() {
        let term = std::env::var("REMAIL_SEARCH").unwrap_or_else(|_| "brill".to_string());

        let config = crate::config::Config::load().expect("config");
        let Some(account) = config.accounts.iter().find(|a| a.enabled) else { return };
        let tokens = crate::auth::TokenStore::new();
        let Ok(credential) = tokens.credential(account).await else {
            println!("account is not signed in");
            return;
        };
        let mut connection = ImapConnection::connect(account, &credential).await.expect("connect");

        let mailboxes = connection.list_mailboxes().await.expect("list");
        let criteria = text_search(&term).expect("criteria");

        // Every selectable mailbox, so the engine's choice can be compared
        // against the ground truth.
        println!("searching every mailbox for {term:?}:");
        let mut total = 0;
        for mailbox in mailboxes.iter().filter(|m| m.selectable) {
            if connection.select(&mailbox.name).await.is_err() {
                continue;
            }
            let hits = connection.search(&criteria).await.unwrap_or_default();
            if !hits.is_empty() {
                println!("  {:<24}{}", mailbox.name, hits.len());
                total += hits.len();
            }
        }
        println!("  {:<24}{total}", "TOTAL");
        connection.logout().await;
    }

    /// Exercises TCP, TLS, the greeting and `LOGIN` against a real server.
    /// Ignored by default because it needs the network; run with
    /// `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn reports_bad_gmail_credentials_cleanly() {
        let account = AccountConfig::gmail(0, "nobody@example.com");
        let credential = Credential::Password("not-a-real-password".into());

        let text = match ImapConnection::connect(&account, &credential).await {
            Ok(_) => panic!("bogus credentials must not authenticate"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("login failed"), "expected an authentication failure, got: {text}");
    }

    fn mailbox(name: &str, special: SpecialUse) -> MailboxInfo {
        MailboxInfo {
            name: name.to_string(),
            delimiter: Some("/".to_string()),
            special,
            selectable: true,
            unseen: 0,
        }
    }

    #[test]
    fn keeps_server_labelling_authoritative() {
        // Gmail's shape: it labels its own Sent folder, and a user label
        // merely called "Sent Mail" must stay an ordinary folder.
        let mut boxes = vec![
            mailbox("[Gmail]/Sent Mail", SpecialUse::Sent),
            mailbox("Maverick/Sent Mail", SpecialUse::Normal),
            mailbox("Maverick/Important", SpecialUse::Normal),
        ];
        classify(&mut boxes, true);
        assert_eq!(boxes[0].special, SpecialUse::Sent);
        assert_eq!(boxes[1].special, SpecialUse::Normal);
        assert_eq!(boxes[2].special, SpecialUse::Normal);
    }

    #[test]
    fn guesses_only_when_the_server_says_nothing() {
        let mut boxes =
            vec![mailbox("Sent", SpecialUse::Normal), mailbox("Trash", SpecialUse::Normal)];
        classify(&mut boxes, false);
        assert_eq!(boxes[0].special, SpecialUse::Sent);
        assert_eq!(boxes[1].special, SpecialUse::Trash);
    }

    #[test]
    fn builds_child_paths_with_the_server_delimiter() {
        assert_eq!(ImapConnection::child_path("Work", Some("/"), "Reports"), "Work/Reports");
        assert_eq!(ImapConnection::child_path("INBOX", Some("."), "Sub"), "INBOX.Sub");
        // A top-level folder has no parent to join to.
        assert_eq!(ImapConnection::child_path("", Some("/"), "Work"), "Work");
        // Servers that report no delimiter still have to be given something.
        assert_eq!(ImapConnection::child_path("Work", None, "Sub"), "Work/Sub");
    }

    #[test]
    fn prefers_a_user_label_over_a_system_one() {
        let labels = ["\\Inbox", "\\Important", "Receipts"];
        assert_eq!(gmail_folder(labels.into_iter()), "Receipts");
    }

    #[test]
    fn falls_back_to_a_system_label_that_denotes_a_place() {
        assert_eq!(gmail_folder(["\\Important", "\\Sent"].into_iter()), "Sent");
        assert_eq!(gmail_folder(["\\Inbox"].into_iter()), "Inbox");
    }

    #[test]
    fn reads_system_labels_however_they_are_escaped() {
        // Both forms reach us depending on how the response was parsed.
        assert_eq!(gmail_folder(["\\Inbox"].into_iter()), "Inbox");
        assert_eq!(gmail_folder(["\\\\Inbox"].into_iter()), "Inbox");
        assert_eq!(gmail_folder(["\\\\Important", "\\\\Sent"].into_iter()), "Sent");
    }

    #[test]
    fn ignores_labels_that_are_states_rather_than_places() {
        assert_eq!(gmail_folder(["\\Starred", "\\Important"].into_iter()), "");
        assert_eq!(gmail_folder(["\\\\Starred"].into_iter()), "");
        assert_eq!(gmail_folder([].into_iter()), "");
    }

    #[test]
    fn takes_the_leaf_of_a_nested_user_label() {
        assert_eq!(gmail_folder(["Maverick/HR"].into_iter()), "HR");
    }

    #[test]
    fn maps_folder_names_to_roles() {
        assert_eq!(guess_special_use("[Gmail]/Sent Mail"), SpecialUse::Sent);
        assert_eq!(guess_special_use("Trash"), SpecialUse::Trash);
        assert_eq!(guess_special_use("Work/Reports"), SpecialUse::Normal);
    }
}
