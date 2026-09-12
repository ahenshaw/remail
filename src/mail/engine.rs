//! The mail engine: an async supervisor plus one worker task per account.
//!
//! The UI never blocks on the network. It sends [`Command`]s and drains
//! [`Event`]s once per frame. Cached results are emitted synchronously from
//! the supervisor so opening a folder paints from SQLite immediately, and the
//! server reconciliation arrives a moment later as further events.
//!
//! Each account runs two connections: one for commands, and (when the server
//! offers `IDLE`) one parked in IDLE that pokes the command task whenever the
//! mailbox changes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{Notify, mpsc};

use super::imap::{ImapConnection, IdleOutcome};
use super::model::{Draft, Envelope, Flags, MailboxInfo, MessageBody, SpecialUse};
use super::store::{MailboxState, Store};
use super::{imap, parse, smtp};
use crate::auth::{Credential, TokenStore, oauth};
use crate::config::{AccountConfig, AccountId, AuthMethod, Config};
use crate::secrets::{self, SecretKind};

/// Cap on how many messages one flag-reconciliation pass examines. Keeps the
/// per-sync cost bounded on mailboxes with a hundred thousand messages.
const FLAG_WINDOW: usize = 2_000;

/// Body cache budget. Beyond this the least recently read messages are evicted.
const BODY_CACHE_BYTES: i64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum Command {
    /// Bring an account online and list its mailboxes.
    Connect(AccountId),
    /// Show a mailbox: emits cached contents, then syncs.
    OpenMailbox { account: AccountId, mailbox: String },
    Sync { account: AccountId, mailbox: String },
    /// Load one message body, from cache when possible.
    FetchBody { account: AccountId, mailbox: String, uid: u32 },
    /// Warm the cache for messages the user is likely to open next.
    Prefetch { account: AccountId, mailbox: String, uids: Vec<u32> },
    SetFlag { account: AccountId, mailbox: String, uids: Vec<u32>, bit: u16, add: bool },
    Move { account: AccountId, mailbox: String, uids: Vec<u32>, destination: String },
    /// Move to Trash, or expunge outright if already there.
    Delete { account: AccountId, mailbox: String, uids: Vec<u32> },
    Send { draft: Draft },
    Search { account: AccountId, mailbox: String, query: String },
    /// Run the interactive OAuth flow for an account.
    SignIn(AccountId),
    SignOut(AccountId),
    Shutdown,
}

/// Connection state, surfaced in the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online,
    Failed,
}

#[derive(Debug, Clone)]
pub enum Event {
    Status { account: AccountId, text: String },
    Error { account: AccountId, text: String },
    Connection { account: AccountId, state: ConnectionState },
    Mailboxes { account: AccountId, mailboxes: Vec<MailboxInfo> },
    /// Message counts for one mailbox, refreshed on every `SELECT`.
    MailboxStats { account: AccountId, mailbox: String, exists: u32, unseen: u32 },
    /// Contents of a mailbox as of this moment. `from_cache` distinguishes the
    /// instant paint from the server-confirmed refresh.
    Listing {
        account: AccountId,
        mailbox: String,
        envelopes: Vec<Envelope>,
        from_cache: bool,
    },
    /// Envelopes added or updated since the last listing.
    Envelopes { account: AccountId, mailbox: String, envelopes: Vec<Envelope> },
    /// Messages that no longer exist on the server.
    Vanished { account: AccountId, mailbox: String, uids: Vec<u32> },
    /// A move or delete did not happen. The UI hides such rows optimistically,
    /// so it needs to be told to put them back.
    RemovalFailed { account: AccountId, mailbox: String, uids: Vec<u32> },
    FlagsChanged { account: AccountId, mailbox: String, changes: Vec<(u32, Flags)> },
    Body { account: AccountId, mailbox: String, uid: u32, body: Arc<MessageBody> },
    /// A preview line became available for a row already on screen.
    Preview { account: AccountId, mailbox: String, uid: u32, preview: String, has_attachments: bool },
    SearchResults { account: AccountId, mailbox: String, envelopes: Vec<Envelope> },
    Sent,
    /// The account is configured for OAuth but has never been authorized, so
    /// the UI should offer sign-in rather than a generic retry.
    NeedsSignIn { account: AccountId },
    SignedIn { account: AccountId },
}

/// Handle held by the UI.
pub struct Engine {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<Event>,
    /// Kept alive for as long as the engine: dropping it stops all workers.
    _runtime: tokio::runtime::Runtime,
}

impl Engine {
    /// Starts the runtime and supervisor.
    ///
    /// `repaint` is called whenever an event is queued, so the UI wakes up
    /// without polling.
    pub fn start(
        config: Arc<RwLock<Config>>,
        store: Arc<Store>,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("remail-net")
            .build()
            .context("starting the network runtime")?;

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let supervisor = Supervisor {
            config,
            store,
            events: EventSink { tx: event_tx, repaint: Arc::new(repaint) },
            tokens: Arc::new(TokenStore::new()),
            workers: HashMap::new(),
            commands: command_tx.clone(),
        };
        runtime.spawn(supervisor.run(command_rx));

        Ok(Self { commands: command_tx, events: event_rx, _runtime: runtime })
    }

    /// Handle to the network runtime, so UI-side helpers (remote image
    /// fetching) can share it rather than starting a second one.
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self._runtime.handle().clone()
    }

    pub fn send(&self, command: Command) {
        // A closed channel means the runtime is shutting down; the UI has
        // nothing useful to do about it.
        let _ = self.commands.send(command);
    }

    /// Drains queued events. Called once per frame.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }
}

/// Clonable event channel that also nudges the UI to repaint.
#[derive(Clone)]
struct EventSink {
    tx: mpsc::UnboundedSender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    fn emit(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            (self.repaint)();
        }
    }

    fn status(&self, account: AccountId, text: impl Into<String>) {
        self.emit(Event::Status { account, text: text.into() });
    }

    fn error(&self, account: AccountId, error: &anyhow::Error) {
        // Include the cause chain: "connecting to imap.example.com: dns error"
        // is far more actionable than either half alone.
        let text = error.chain().map(|c| c.to_string()).collect::<Vec<_>>().join(": ");
        tracing::warn!(account, "{text}");
        self.emit(Event::Error { account, text });
    }
}

struct Supervisor {
    config: Arc<RwLock<Config>>,
    store: Arc<Store>,
    events: EventSink,
    tokens: Arc<TokenStore>,
    workers: HashMap<AccountId, mpsc::UnboundedSender<Command>>,
    /// Loopback so account workers can queue follow-up commands.
    commands: mpsc::UnboundedSender<Command>,
}

impl Supervisor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        while let Some(command) = rx.recv().await {
            match command {
                Command::Shutdown => break,

                // Served from cache on this task so folder switches are
                // instant even when the account worker is mid-fetch.
                Command::OpenMailbox { account, ref mailbox } => {
                    let limit = self.config.read().unwrap().ui.initial_sync_count.max(200);
                    match self.store.load_envelopes(account, mailbox, limit) {
                        Ok(envelopes) => self.events.emit(Event::Listing {
                            account,
                            mailbox: mailbox.clone(),
                            envelopes,
                            from_cache: true,
                        }),
                        Err(e) => self.events.error(account, &e),
                    }
                    self.dispatch(account, command);
                }

                Command::SignIn(account) => {
                    self.sign_in(account);
                }

                Command::SignOut(account) => {
                    self.tokens.forget(account);
                    secrets::delete_all(account);
                    self.workers.remove(&account);
                    self.events.emit(Event::Connection {
                        account,
                        state: ConnectionState::Offline,
                    });
                }

                other => {
                    if let Some(account) = other.account() {
                        self.dispatch(account, other);
                    }
                }
            }
        }
        tracing::info!("mail engine stopped");
    }

    /// Routes a command to an account worker, starting one if needed.
    fn dispatch(&mut self, account: AccountId, command: Command) {
        let tx = self.worker(account);
        if tx.send(command).is_err() {
            // The worker died; drop it so the next command respawns it.
            self.workers.remove(&account);
        }
    }

    fn worker(&mut self, account: AccountId) -> mpsc::UnboundedSender<Command> {
        if let Some(tx) = self.workers.get(&account) {
            if !tx.is_closed() {
                return tx.clone();
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let worker = AccountWorker {
            account,
            config: self.config.clone(),
            store: self.store.clone(),
            events: self.events.clone(),
            tokens: self.tokens.clone(),
            connection: None,
            supervisor: self.commands.clone(),
            idle_cancel: Arc::new(Notify::new()),
            idle_running: false,
        };
        tokio::spawn(worker.run(rx));
        self.workers.insert(account, tx.clone());
        tx
    }

    /// Runs the interactive OAuth flow and, on success, connects.
    fn sign_in(&self, account: AccountId) {
        let Some(config) = self.config.read().unwrap().account(account).cloned() else {
            return;
        };
        let events = self.events.clone();
        let tokens = self.tokens.clone();
        let commands = self.commands.clone();

        tokio::spawn(async move {
            events.status(account, "Waiting for browser sign-in\u{2026}");
            let creds = crate::auth::client_credentials(&config);
            match oauth::authorize(&creds, &config.email).await {
                Ok(token_set) => {
                    if let Err(e) = tokens.remember(account, token_set) {
                        events.error(account, &e);
                        return;
                    }
                    events.emit(Event::SignedIn { account });
                    events.status(account, "Signed in");
                    let _ = commands.send(Command::Connect(account));
                }
                Err(e) => events.error(account, &e),
            }
        });
    }
}

impl Command {
    /// The account a command targets, if any.
    fn account(&self) -> Option<AccountId> {
        Some(match self {
            Command::Connect(a) | Command::SignIn(a) | Command::SignOut(a) => *a,
            Command::OpenMailbox { account, .. }
            | Command::Sync { account, .. }
            | Command::FetchBody { account, .. }
            | Command::Prefetch { account, .. }
            | Command::SetFlag { account, .. }
            | Command::Move { account, .. }
            | Command::Delete { account, .. }
            | Command::Search { account, .. } => *account,
            Command::Send { draft } => draft.account,
            Command::Shutdown => return None,
        })
    }
}

struct AccountWorker {
    account: AccountId,
    config: Arc<RwLock<Config>>,
    store: Arc<Store>,
    events: EventSink,
    tokens: Arc<TokenStore>,
    connection: Option<ImapConnection>,
    supervisor: mpsc::UnboundedSender<Command>,
    idle_cancel: Arc<Notify>,
    idle_running: bool,
}

impl AccountWorker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let poll_interval = {
            let config = self.config.read().unwrap();
            Duration::from_secs(config.ui.poll_interval_secs.max(30))
        };
        let mut poll = tokio::time::interval(poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        poll.tick().await; // The first tick is immediate; skip it.

        loop {
            tokio::select! {
                command = rx.recv() => {
                    let Some(command) = command else { break };
                    if let Err(e) = self.handle(command).await {
                        self.events.error(self.account, &e);
                        // Any failed command may have left the connection in an
                        // unknown state; drop it so the next one reconnects.
                        self.drop_connection();
                    }
                }
                _ = poll.tick() => {
                    // Only poll when IDLE is not covering this mailbox.
                    if !self.idle_running {
                        if let Some(mailbox) = self.current_mailbox() {
                            if let Err(e) = self.sync(&mailbox).await {
                                self.events.error(self.account, &e);
                                self.drop_connection();
                            }
                        }
                    }
                }
            }
        }

        self.idle_cancel.notify_waiters();
        if let Some(connection) = self.connection.take() {
            connection.logout().await;
        }
    }

    fn account_config(&self) -> Result<AccountConfig> {
        self.config
            .read()
            .unwrap()
            .account(self.account)
            .cloned()
            .ok_or_else(|| anyhow!("account {} is no longer configured", self.account))
    }

    fn current_mailbox(&self) -> Option<String> {
        self.connection.as_ref()?.selected_mailbox().map(str::to_string)
    }

    fn drop_connection(&mut self) {
        if self.connection.take().is_some() {
            self.events.emit(Event::Connection {
                account: self.account,
                state: ConnectionState::Failed,
            });
        }
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Connect(_) => {
                self.connect().await?;
                self.refresh_mailboxes().await?;
            }
            Command::OpenMailbox { mailbox, .. } | Command::Sync { mailbox, .. } => {
                self.sync(&mailbox).await?;
                self.ensure_idle(&mailbox).await;
            }
            Command::FetchBody { mailbox, uid, .. } => {
                self.load_body(&mailbox, uid, true).await?;
            }
            Command::Prefetch { mailbox, uids, .. } => {
                for uid in uids {
                    if self.store.has_body(self.account, &mailbox, uid) {
                        continue;
                    }
                    // Prefetch failures are not worth reporting; the user has
                    // not asked for these messages yet.
                    if self.load_body(&mailbox, uid, false).await.is_err() {
                        break;
                    }
                }
                let _ = self.store.prune_bodies(BODY_CACHE_BYTES);
            }
            Command::SetFlag { mailbox, uids, bit, add, .. } => {
                self.set_flag(&mailbox, &uids, bit, add).await?;
            }
            Command::Move { mailbox, uids, destination, .. } => {
                if let Err(e) = self.move_messages(&mailbox, &uids, &destination).await {
                    self.report_removal_failed(&mailbox, uids);
                    return Err(e);
                }
            }
            Command::Delete { mailbox, uids, .. } => {
                if let Err(e) = self.delete(&mailbox, &uids).await {
                    self.report_removal_failed(&mailbox, uids);
                    return Err(e);
                }
            }
            Command::Search { mailbox, query, .. } => {
                self.search(&mailbox, &query).await?;
            }
            Command::Send { draft } => {
                self.send(draft).await?;
            }
            // Handled by the supervisor, never routed to a worker.
            Command::SignIn(_)
            | Command::SignOut(_)
            | Command::Shutdown => {}
        }
        Ok(())
    }

    /// Returns a live connection, dialling one if necessary.
    async fn connect(&mut self) -> Result<&mut ImapConnection> {
        if self.connection.is_some() {
            return Ok(self.connection.as_mut().unwrap());
        }

        let account = self.account_config()?;
        self.events
            .emit(Event::Connection { account: self.account, state: ConnectionState::Connecting });
        self.events.status(self.account, format!("Connecting to {}\u{2026}", account.imap_host));

        let credential = self.credential(&account).await?;
        let connection = ImapConnection::connect(&account, &credential).await?;

        self.events
            .emit(Event::Connection { account: self.account, state: ConnectionState::Online });
        self.events.status(self.account, "Connected");
        self.connection = Some(connection);
        Ok(self.connection.as_mut().unwrap())
    }

    async fn credential(&self, account: &AccountConfig) -> Result<Credential> {
        // Never authorized: that is a prompt, not a failure to retry.
        if account.auth == AuthMethod::OAuth2 && !self.tokens.is_signed_in(account.id) {
            self.events.emit(Event::NeedsSignIn { account: account.id });
            bail!("{} needs to be signed in with Google", account.email);
        }

        match self.tokens.credential(account).await {
            Ok(credential) => Ok(credential),
            Err(e) if account.auth == AuthMethod::OAuth2 => {
                // A stored token that no longer works also needs the browser.
                self.events.emit(Event::NeedsSignIn { account: account.id });
                Err(e).context("authorization expired; sign in again from Accounts")
            }
            Err(e) => Err(e),
        }
    }

    async fn refresh_mailboxes(&mut self) -> Result<()> {
        let connection = self.connect().await?;
        let mailboxes = connection.list_mailboxes().await?;
        self.store.save_mailboxes(self.account, &mailboxes)?;
        self.events.emit(Event::Mailboxes { account: self.account, mailboxes });
        Ok(())
    }

    /// Reconciles one mailbox with the server: new messages, flag changes and
    /// deletions.
    async fn sync(&mut self, mailbox: &str) -> Result<()> {
        let initial_count = self.config.read().unwrap().ui.initial_sync_count.max(50);
        let account = self.account;
        let connection = self.connect().await?;

        let selected = connection.select(mailbox).await?;
        self.events.emit(Event::MailboxStats {
            account,
            mailbox: mailbox.to_string(),
            exists: selected.exists,
            unseen: selected.unseen,
        });
        let mut state = self.store.mailbox_state(account, mailbox)?;

        // A changed UIDVALIDITY means every cached UID is meaningless.
        if state.uid_validity != 0 && state.uid_validity != selected.uid_validity {
            tracing::info!(account, mailbox, "UIDVALIDITY changed; dropping cache");
            self.store.clear_mailbox(account, mailbox)?;
            state = MailboxState::default();
        }

        let first_sync = state.highest_uid == 0;
        let range = if first_sync {
            imap::recent_range(selected.uid_next, initial_count)
        } else {
            format!("{}:*", state.highest_uid + 1)
        };

        let connection = self.connection.as_mut().expect("connected above");
        let fetched = connection.fetch_envelopes(&range).await?;

        // `n:*` always returns the last message even when nothing is new.
        let fresh: Vec<Envelope> =
            fetched.into_iter().filter(|e| first_sync || e.uid > state.highest_uid).collect();

        let highest = fresh.iter().map(|e| e.uid).max().unwrap_or(state.highest_uid);
        if !fresh.is_empty() {
            self.store.save_envelopes(account, mailbox, &fresh)?;
            self.events.emit(Event::Envelopes {
                account,
                mailbox: mailbox.to_string(),
                envelopes: fresh.clone(),
            });
        }

        self.store.set_mailbox_state(
            account,
            mailbox,
            MailboxState {
                uid_validity: selected.uid_validity,
                highest_uid: highest.max(state.highest_uid),
            },
        )?;

        if first_sync {
            // Paint the full cached listing now that it exists.
            let limit = initial_count;
            let envelopes = self.store.load_envelopes(account, mailbox, limit)?;
            self.events.emit(Event::Listing {
                account,
                mailbox: mailbox.to_string(),
                envelopes,
                from_cache: false,
            });
        }

        self.reconcile_flags(mailbox).await?;
        self.events.status(account, "Up to date");
        Ok(())
    }

    /// Compares cached flags with the server's, and notices deletions.
    async fn reconcile_flags(&mut self, mailbox: &str) -> Result<()> {
        let account = self.account;
        let cached = self.store.load_envelopes(account, mailbox, FLAG_WINDOW as u32)?;
        if cached.is_empty() {
            return Ok(());
        }

        let uids: Vec<u32> = cached.iter().map(|e| e.uid).collect();
        let low = uids.iter().copied().min().unwrap_or(1);
        let connection = self.connection.as_mut().context("not connected")?;
        let server = connection.fetch_flags(&format!("{low}:*")).await?;

        let server_map: HashMap<u32, Flags> = server.into_iter().collect();

        let mut changes = Vec::new();
        let mut vanished = Vec::new();
        for envelope in &cached {
            match server_map.get(&envelope.uid) {
                Some(flags) if *flags != envelope.flags => {
                    changes.push((envelope.uid, *flags));
                }
                Some(_) => {}
                None => vanished.push(envelope.uid),
            }
        }

        for (uid, flags) in &changes {
            self.store.set_flags(account, mailbox, &[*uid], *flags)?;
        }
        if !changes.is_empty() {
            self.events.emit(Event::FlagsChanged {
                account,
                mailbox: mailbox.to_string(),
                changes,
            });
        }

        if !vanished.is_empty() {
            self.store.delete_envelopes(account, mailbox, &vanished)?;
            self.events.emit(Event::Vanished {
                account,
                mailbox: mailbox.to_string(),
                uids: vanished,
            });
        }
        Ok(())
    }

    /// Loads a body from the cache, falling back to the server.
    async fn load_body(&mut self, mailbox: &str, uid: u32, announce: bool) -> Result<()> {
        let account = self.account;

        if let Some(raw) = self.store.load_raw(account, mailbox, uid)? {
            let body = parse::parse_body(&raw);
            // The cached preview was produced by whatever renderer was current
            // when the body was first fetched. Refresh it so improvements
            // reach messages already in the cache.
            let preview = body.preview();
            let has_attachments = !body.attachments.is_empty();
            self.store.set_preview(account, mailbox, uid, &preview, has_attachments)?;
            self.events.emit(Event::Preview {
                account,
                mailbox: mailbox.to_string(),
                uid,
                preview,
                has_attachments,
            });

            if announce {
                self.events.emit(Event::Body {
                    account,
                    mailbox: mailbox.to_string(),
                    uid,
                    body: Arc::new(body),
                });
            }
            return Ok(());
        }

        let connection = self.connect().await?;
        if connection.selected_mailbox() != Some(mailbox) {
            connection.select(mailbox).await?;
        }
        let connection = self.connection.as_mut().expect("connected above");
        let Some(raw) = connection.fetch_raw(uid).await? else {
            bail!("message {uid} is no longer on the server");
        };

        self.store.save_raw(account, mailbox, uid, &raw)?;
        let body = parse::parse_body(&raw);
        let preview = body.preview();
        let has_attachments = !body.attachments.is_empty();
        self.store.set_preview(account, mailbox, uid, &preview, has_attachments)?;
        self.events.emit(Event::Preview {
            account,
            mailbox: mailbox.to_string(),
            uid,
            preview,
            has_attachments,
        });

        if announce {
            self.events.emit(Event::Body {
                account,
                mailbox: mailbox.to_string(),
                uid,
                body: Arc::new(body),
            });
        }
        Ok(())
    }

    async fn set_flag(
        &mut self,
        mailbox: &str,
        uids: &[u32],
        bit: u16,
        add: bool,
    ) -> Result<()> {
        let account = self.account;
        let connection = self.connect().await?;
        if connection.selected_mailbox() != Some(mailbox) {
            connection.select(mailbox).await?;
        }
        let connection = self.connection.as_mut().expect("connected above");
        connection.store_flag(uids, bit, add).await?;

        // Reflect the change locally; the next reconcile confirms it.
        let cached = self.store.load_envelopes(account, mailbox, FLAG_WINDOW as u32)?;
        let mut changes = Vec::new();
        for envelope in cached.iter().filter(|e| uids.contains(&e.uid)) {
            let mut flags = envelope.flags;
            flags.set(bit, add);
            self.store.set_flags(account, mailbox, &[envelope.uid], flags)?;
            changes.push((envelope.uid, flags));
        }
        if !changes.is_empty() {
            self.events.emit(Event::FlagsChanged {
                account,
                mailbox: mailbox.to_string(),
                changes,
            });
        }
        Ok(())
    }

    fn report_removal_failed(&self, mailbox: &str, uids: Vec<u32>) {
        self.events.emit(Event::RemovalFailed {
            account: self.account,
            mailbox: mailbox.to_string(),
            uids,
        });
    }

    async fn move_messages(
        &mut self,
        mailbox: &str,
        uids: &[u32],
        destination: &str,
    ) -> Result<()> {
        let account = self.account;
        let connection = self.connect().await?;
        if connection.selected_mailbox() != Some(mailbox) {
            connection.select(mailbox).await?;
        }
        let connection = self.connection.as_mut().expect("connected above");
        connection.move_messages(uids, destination).await?;

        self.store.delete_envelopes(account, mailbox, uids)?;
        self.events.emit(Event::Vanished {
            account,
            mailbox: mailbox.to_string(),
            uids: uids.to_vec(),
        });
        self.events.status(account, format!("Moved {} to {destination}", plural(uids.len())));
        Ok(())
    }

    /// Moves to Trash, or expunges when already in Trash or Junk.
    async fn delete(&mut self, mailbox: &str, uids: &[u32]) -> Result<()> {
        let trash = self
            .store
            .load_mailboxes(self.account)?
            .into_iter()
            .find(|m| m.special == SpecialUse::Trash)
            .map(|m| m.name);

        match trash {
            Some(trash) if trash != mailbox => {
                self.move_messages(mailbox, uids, &trash).await
            }
            _ => {
                let account = self.account;
                let connection = self.connect().await?;
                if connection.selected_mailbox() != Some(mailbox) {
                    connection.select(mailbox).await?;
                }
                let connection = self.connection.as_mut().expect("connected above");
                connection.store_flag(uids, Flags::DELETED, true).await?;
                connection.expunge(uids).await?;

                self.store.delete_envelopes(account, mailbox, uids)?;
                self.events.emit(Event::Vanished {
                    account,
                    mailbox: mailbox.to_string(),
                    uids: uids.to_vec(),
                });
                self.events.status(account, format!("Deleted {}", plural(uids.len())));
                Ok(())
            }
        }
    }

    async fn search(&mut self, mailbox: &str, query: &str) -> Result<()> {
        let account = self.account;
        let connection = self.connect().await?;
        if connection.selected_mailbox() != Some(mailbox) {
            connection.select(mailbox).await?;
        }
        let connection = self.connection.as_mut().expect("connected above");

        let uids = connection.search(&imap::text_search(query)?).await?;
        // Bound the result set; a bare term can match tens of thousands.
        let uids: Vec<u32> = uids.into_iter().take(500).collect();
        if uids.is_empty() {
            self.events.emit(Event::SearchResults {
                account,
                mailbox: mailbox.to_string(),
                envelopes: Vec::new(),
            });
            return Ok(());
        }

        let envelopes = connection.fetch_envelopes(&imap::uid_set(&uids)).await?;
        self.store.save_envelopes(account, mailbox, &envelopes)?;

        let mut envelopes = envelopes;
        envelopes.sort_by(|a, b| b.date.cmp(&a.date));
        self.events.emit(Event::SearchResults {
            account,
            mailbox: mailbox.to_string(),
            envelopes,
        });
        Ok(())
    }

    async fn send(&mut self, draft: Draft) -> Result<()> {
        let account = self.account_config()?;
        self.events.status(self.account, "Sending\u{2026}");

        let credential = self.credential(&account).await?;
        let sent = smtp::send(&account, &credential, &draft).await?;
        self.events.emit(Event::Sent);
        self.events.status(self.account, "Message sent");

        // File a copy in Sent. Gmail does this server-side, so skip it there
        // to avoid a duplicate.
        if !account.imap_host.contains("gmail.com") {
            if let Some(sent_box) = self
                .store
                .load_mailboxes(self.account)?
                .into_iter()
                .find(|m| m.special == SpecialUse::Sent)
                .map(|m| m.name)
            {
                let connection = self.connect().await?;
                if let Err(e) = connection.append(&sent_box, &sent.raw, &["\\Seen"]).await {
                    // The message did go out; a filing failure is not fatal.
                    tracing::warn!("could not file sent message: {e}");
                    self.events.status(self.account, "Sent, but could not file a copy");
                }
            }
        }
        Ok(())
    }

    /// Parks a second connection in IDLE so new mail arrives without polling.
    async fn ensure_idle(&mut self, mailbox: &str) {
        if self.idle_running {
            return;
        }
        let Ok(account) = self.account_config() else { return };
        if !account.use_idle {
            return;
        }
        if !self.connection.as_ref().is_some_and(ImapConnection::supports_idle) {
            return;
        }

        let credential = match self.tokens.credential(&account).await {
            Ok(c) => c,
            Err(_) => return,
        };

        let cancel = self.idle_cancel.clone();
        let events = self.events.clone();
        let supervisor = self.supervisor.clone();
        let mailbox = mailbox.to_string();
        let account_id = self.account;
        self.idle_running = true;

        tokio::spawn(async move {
            let mut connection = match ImapConnection::connect(&account, &credential).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(account_id, "IDLE connection failed: {e}");
                    return;
                }
            };
            if connection.select(&mailbox).await.is_err() {
                return;
            }
            tracing::debug!(account_id, mailbox, "IDLE started");

            loop {
                let (returned, outcome) = match connection.idle(&cancel).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::debug!(account_id, "IDLE ended: {e}");
                        return;
                    }
                };
                connection = returned;

                if outcome == IdleOutcome::Changed {
                    events.status(account_id, "New activity");
                    if supervisor
                        .send(Command::Sync { account: account_id, mailbox: mailbox.clone() })
                        .is_err()
                    {
                        return;
                    }
                }
                // Re-select to pick up the new state before idling again.
                if connection.reselect(&mailbox).await.is_err() {
                    return;
                }
            }
        });
    }
}

fn plural(count: usize) -> String {
    if count == 1 { "1 message".to_string() } else { format!("{count} messages") }
}

/// Stores a password for an account, used by the account editor.
pub fn save_password(account: AccountId, password: &str) -> Result<()> {
    secrets::set(SecretKind::Password, account, password)
}
