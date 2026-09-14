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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{Notify, mpsc};

use super::imap::{IdleOutcome, ImapConnection};
use super::model::{Draft, Envelope, Flags, MailboxInfo, MessageBody, SearchScope, SpecialUse};
use super::query::Query;
use super::store::{MailboxState, Store};
use super::{imap, parse, smtp};
use crate::auth::{Credential, TokenStore, oauth};
use crate::config::{AccountConfig, AccountId, AuthMethod, Config};
use crate::secrets::{self, SecretKind};

/// How much a sighting of an address counts towards completing recipients.
///
/// Someone the user chose to write to is worth far more than an address that
/// merely appeared in a header, or every newsletter would outrank the people
/// they correspond with.
mod contact_weight {
    /// A recipient of a message the user just sent.
    pub const SENT_TO: i64 = 40;
    /// A recipient of something already in the Sent or Drafts mailbox.
    pub const ADDRESSED: i64 = 8;
    /// Anyone appearing on a message that arrived.
    pub const SEEN: i64 = 1;
}

/// Most results a search returns, across every mailbox it covers.
const SEARCH_LIMIT: usize = 500;
/// How many messages a search fetches per round trip. Smaller than the limit
/// so that abandoning one is noticed part way through a mailbox rather than
/// only between mailboxes.
const SEARCH_BATCH: usize = 100;

/// Cap on how many messages one flag-reconciliation pass examines. Keeps the
/// per-sync cost bounded on mailboxes with a hundred thousand messages.
const FLAG_WINDOW: usize = 2_000;

/// Body cache budget. Beyond this the least recently read messages are evicted.
const BODY_CACHE_BYTES: i64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum Command {
    /// Bring an account online and list its mailboxes.
    Connect(AccountId),
    /// Count unread messages in every mailbox.
    CountUnread(AccountId),
    /// Mark everything in a mailbox as read.
    MarkAllRead {
        account: AccountId,
        mailbox: String,
    },
    CreateMailbox {
        account: AccountId,
        name: String,
    },
    RenameMailbox {
        account: AccountId,
        from: String,
        to: String,
    },
    DeleteMailbox {
        account: AccountId,
        mailbox: String,
    },
    /// Show a mailbox: emits cached contents, then syncs.
    OpenMailbox {
        account: AccountId,
        mailbox: String,
    },
    Sync {
        account: AccountId,
        mailbox: String,
    },
    /// Load one message body, from cache when possible.
    FetchBody {
        account: AccountId,
        mailbox: String,
        uid: u32,
        /// Set by the supervisor when it has already answered from the cache.
        /// The worker then only refreshes the stored preview, rather than
        /// sending the body a second time and making the reader lay it out
        /// again for no change.
        served: bool,
    },
    /// Warm the cache for messages the user is likely to open next.
    Prefetch {
        account: AccountId,
        mailbox: String,
        uids: Vec<u32>,
    },
    SetFlag {
        account: AccountId,
        mailbox: String,
        uids: Vec<u32>,
        bit: u16,
        add: bool,
    },
    Move {
        account: AccountId,
        mailbox: String,
        uids: Vec<u32>,
        destination: String,
    },
    /// Move to Trash, or expunge outright if already there.
    Delete {
        account: AccountId,
        mailbox: String,
        uids: Vec<u32>,
    },
    Send {
        draft: Draft,
    },
    Search {
        account: AccountId,
        mailbox: String,
        query: String,
        scope: SearchScope,
        /// Whether Spam and Trash are included in a whole-account search.
        include_spam_and_trash: bool,
        /// Which search this is. Handed back with the results so the caller
        /// can tell them from the results of a search it has since moved on
        /// from — there is no way to stop one that is already running, and a
        /// whole-account search takes long enough to be abandoned.
        generation: u64,
    },
    /// Abandon whatever search is running. Carries the generation that
    /// supersedes it, which is what a running search compares itself against.
    CancelSearch {
        account: AccountId,
        generation: u64,
    },
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
    Status {
        account: AccountId,
        text: String,
    },
    Error {
        account: AccountId,
        text: String,
    },
    Connection {
        account: AccountId,
        state: ConnectionState,
    },
    Mailboxes {
        account: AccountId,
        mailboxes: Vec<MailboxInfo>,
    },
    /// How many unread messages a mailbox holds.
    MailboxStats {
        account: AccountId,
        mailbox: String,
        unseen: u32,
    },
    /// Contents of a mailbox as of this moment. `from_cache` distinguishes the
    /// instant paint from the server-confirmed refresh.
    Listing {
        account: AccountId,
        mailbox: String,
        envelopes: Vec<Envelope>,
        from_cache: bool,
    },
    /// Envelopes added or updated since the last listing.
    Envelopes {
        account: AccountId,
        mailbox: String,
        envelopes: Vec<Envelope>,
    },
    /// Messages that no longer exist on the server.
    Vanished {
        account: AccountId,
        mailbox: String,
        uids: Vec<u32>,
    },
    /// A move or delete did not happen. The UI hides such rows optimistically,
    /// so it needs to be told to put them back.
    RemovalFailed {
        account: AccountId,
        mailbox: String,
        uids: Vec<u32>,
    },
    FlagsChanged {
        account: AccountId,
        mailbox: String,
        changes: Vec<(u32, Flags)>,
    },
    Body {
        account: AccountId,
        mailbox: String,
        uid: u32,
        body: Arc<MessageBody>,
    },
    /// A preview line became available for a row already on screen.
    Preview {
        account: AccountId,
        mailbox: String,
        uid: u32,
        preview: String,
        has_attachments: bool,
    },
    SearchResults {
        account: AccountId,
        mailbox: String,
        envelopes: Vec<Envelope>,
        /// The `generation` of the [`Command::Search`] these answer.
        generation: u64,
    },
    Sent,
    /// The account is configured for OAuth but has never been authorized, so
    /// the UI should offer sign-in rather than a generic retry.
    NeedsSignIn {
        account: AccountId,
    },
    SignedIn {
        account: AccountId,
    },
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
            search_generation: Arc::new(AtomicU64::new(0)),
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

    /// Sends a command and blocks until it has been answered.
    ///
    /// The interface never needs this: it sends, returns to drawing, and
    /// picks the answer up from [`Engine::poll`] on a later frame. Anything
    /// without frames — a command-line tool, a test — has nowhere to go in
    /// the meantime, so it waits here instead.
    ///
    /// `answer` is called with each event as it arrives and returns `Some`
    /// for the one being waited on. Which event that is depends on the
    /// command, and sometimes on its contents: a search is answered by the
    /// results carrying its own generation, a body by the one carrying its
    /// own UID.
    pub fn send_and_wait<T>(
        &mut self,
        command: Command,
        timeout: Duration,
        answer: impl FnMut(&Event) -> Option<T>,
    ) -> Result<T> {
        self.send(command);
        self.wait_for(timeout, answer)
    }

    /// Blocks until an event answers `answer`, or the wait runs out.
    ///
    /// An [`Event::Error`] that `answer` does not claim ends the wait as a
    /// failure, rather than being passed over to sit out the timeout. A
    /// caller that expects an error — testing one, or waiting through a
    /// failure that does not concern it — claims it and carries on.
    ///
    /// Events arriving before the one wanted are consumed. This takes from
    /// the same queue [`Engine::poll`] does, so a caller that uses both will
    /// find that whatever this passed over has already been drained.
    pub fn wait_for<T>(
        &mut self,
        timeout: Duration,
        mut answer: impl FnMut(&Event) -> Option<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;

        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("nothing answered within {timeout:?}");
            }

            // The runtime is owned here, and this is not one of its threads.
            let received = self
                ._runtime
                .block_on(async { tokio::time::timeout(left, self.events.recv()).await });

            match received {
                Err(_) => bail!("nothing answered within {timeout:?}"),
                Ok(None) => bail!("the mail engine stopped"),
                Ok(Some(event)) => {
                    if let Some(answer) = answer(&event) {
                        return Ok(answer);
                    }
                    if let Event::Error { text, .. } = event {
                        bail!(text);
                    }
                }
            }
        }
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
    /// The search the interface is waiting for. One counter for every
    /// account, because the interface has one search box. Set here, where it
    /// can be set while a worker is busy, and read by the search itself.
    search_generation: Arc<AtomicU64>,
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

                // The same, for a body that is already cached. The account
                // worker takes its commands one at a time, and one of them is
                // a prefetch run that may be part way through fetching a
                // screenful of messages off the server. A body request queued
                // behind that waited out every one of them, even though the
                // message asked for was sitting in the cache: the reading pane
                // stayed empty for as long as the fetching took, rather than
                // for the millisecond the parse costs.
                Command::FetchBody { account, mailbox, uid, .. } => {
                    let cached = match self.store.load_raw(account, &mailbox, uid) {
                        Ok(cached) => cached,
                        Err(e) => {
                            self.events.error(account, &e);
                            None
                        }
                    };
                    let served = cached.is_some();
                    if let Some(raw) = cached {
                        self.events.emit(Event::Body {
                            account,
                            mailbox: mailbox.clone(),
                            uid,
                            body: Arc::new(parse::parse_body(&raw)),
                        });
                    }
                    // Dispatched either way: with the body served the worker
                    // only refreshes the stored preview, and without it there
                    // is a message to go and get.
                    self.dispatch(account, Command::FetchBody { account, mailbox, uid, served });
                }

                // Recorded here rather than in the worker, which is busy:
                // that is the whole point. A search already running compares
                // itself against this and gives up when it no longer matches.
                Command::Search { account, generation, .. } => {
                    self.search_generation.store(generation, Ordering::Relaxed);
                    self.dispatch(account, command);
                }

                // Nothing to dispatch: withdrawing the question is the whole
                // of the work, and the search that was answering it is inside
                // the worker already.
                Command::CancelSearch { generation, .. } => {
                    self.search_generation.store(generation, Ordering::Relaxed);
                }

                Command::SignIn(account) => {
                    self.sign_in(account);
                }

                Command::SignOut(account) => {
                    self.tokens.forget(account);
                    secrets::delete_all(account);
                    self.workers.remove(&account);
                    self.events
                        .emit(Event::Connection { account, state: ConnectionState::Offline });
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
        if let Some(tx) = self.workers.get(&account)
            && !tx.is_closed()
        {
            return tx.clone();
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let worker = AccountWorker {
            account,
            search_generation: self.search_generation.clone(),
            config: self.config.clone(),
            store: self.store.clone(),
            events: self.events.clone(),
            tokens: self.tokens.clone(),
            connection: None,
            supervisor: self.commands.clone(),
            idle_cancel: Arc::new(Notify::new()),
            idle_running: false,
            prefetch: VecDeque::new(),
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
            Command::Connect(a)
            | Command::CountUnread(a)
            | Command::SignIn(a)
            | Command::SignOut(a) => *a,
            Command::MarkAllRead { account, .. }
            | Command::CreateMailbox { account, .. }
            | Command::RenameMailbox { account, .. }
            | Command::DeleteMailbox { account, .. } => *account,
            Command::OpenMailbox { account, .. }
            | Command::Sync { account, .. }
            | Command::FetchBody { account, .. }
            | Command::Prefetch { account, .. }
            | Command::SetFlag { account, .. }
            | Command::Move { account, .. }
            | Command::Delete { account, .. }
            | Command::Search { account, .. }
            | Command::CancelSearch { account, .. } => *account,
            Command::Send { draft } => draft.account,
            Command::Shutdown => return None,
        })
    }
}

struct AccountWorker {
    account: AccountId,
    /// The search the interface is waiting for, written by the supervisor.
    /// A running search reads it to find out whether it is still the answer
    /// anyone wants.
    search_generation: Arc<AtomicU64>,
    config: Arc<RwLock<Config>>,
    store: Arc<Store>,
    events: EventSink,
    tokens: Arc<TokenStore>,
    connection: Option<ImapConnection>,
    supervisor: mpsc::UnboundedSender<Command>,
    idle_cancel: Arc<Notify>,
    idle_running: bool,
    /// Messages to warm the cache with, one per turn of the loop and only
    /// when nothing the user asked for is waiting. Held as a queue rather
    /// than fetched where the command arrives, because a command is handled
    /// to completion: a run of these used to hold the worker for as long as
    /// it took to fetch every one of them.
    prefetch: VecDeque<(String, u32)>,
}

/// Adds prefetch requests to the queue, holding it to
/// [`PREFETCH_QUEUE_LIMIT`].
///
/// What overflows is dropped from the front, the oldest request being the one
/// the reader has most likely scrolled past. Nothing is lost by it: a message
/// that is never prefetched is fetched the moment it is opened.
fn enqueue_prefetch(
    queue: &mut VecDeque<(String, u32)>,
    mailbox: &str,
    uids: impl Iterator<Item = u32>,
) {
    queue.extend(uids.map(|uid| (mailbox.to_string(), uid)));
    let excess = queue.len().saturating_sub(PREFETCH_QUEUE_LIMIT);
    queue.drain(..excess);
}

/// How many messages may be queued for prefetching before the oldest are
/// dropped. Scrolling fast enough to overrun this asks for more than the
/// cache can usefully hold anyway, and anything dropped is still fetched the
/// moment it is opened.
const PREFETCH_QUEUE_LIMIT: usize = 500;

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
                // Ordered, not random: a command the user is waiting on is
                // always taken before another speculative fetch. The prefetch
                // branch is ready whenever the queue is not empty, so it runs
                // only on a turn where nothing else was.
                biased;

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
                    if !self.idle_running
                        && let Some(mailbox) = self.current_mailbox()
                        && let Err(e) = self.sync(&mailbox).await
                    {
                        self.events.error(self.account, &e);
                        self.drop_connection();
                    }
                }
                // One message, then back to the top to look for commands
                // again. The guard is what keeps this branch from spinning
                // when there is nothing queued.
                () = std::future::ready(()), if !self.prefetch.is_empty() => {
                    self.prefetch_one().await;
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
            self.events
                .emit(Event::Connection { account: self.account, state: ConnectionState::Failed });
        }
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Connect(_) => {
                self.connect().await?;
                self.refresh_mailboxes().await?;
            }
            Command::CountUnread(_) => {
                let mailboxes: Vec<String> = self
                    .store
                    .load_mailboxes(self.account)?
                    .into_iter()
                    .filter(|mailbox| mailbox.selectable)
                    .map(|mailbox| mailbox.name)
                    .collect();
                self.report_unread(&mailboxes).await;
            }
            Command::OpenMailbox { mailbox, .. } | Command::Sync { mailbox, .. } => {
                self.sync(&mailbox).await?;
                self.ensure_idle(&mailbox).await;
            }
            Command::FetchBody { mailbox, uid, served, .. } => {
                self.load_body(&mailbox, uid, !served).await?;
            }
            Command::Prefetch { mailbox, uids, .. } => {
                let wanted = uids
                    .into_iter()
                    .filter(|uid| !self.store.has_body(self.account, &mailbox, *uid));
                enqueue_prefetch(&mut self.prefetch, &mailbox, wanted);
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
            Command::MarkAllRead { mailbox, .. } => {
                let connection = self.connect().await?;
                connection.mark_all_seen(&mailbox).await?;
                self.events.status(self.account, format!("Marked {mailbox} read"));
                // Re-read the flags so rows on screen stop showing unread.
                self.sync(&mailbox).await?;
            }
            Command::CreateMailbox { name, .. } => {
                let connection = self.connect().await?;
                connection.create_mailbox(&name).await?;
                self.events.status(self.account, format!("Created {name}"));
                self.refresh_mailboxes().await?;
            }
            Command::RenameMailbox { from, to, .. } => {
                let connection = self.connect().await?;
                connection.rename_mailbox(&from, &to).await?;
                // The old name's cached rows belong to a mailbox that no
                // longer exists; the new name syncs from scratch.
                self.store.clear_mailbox(self.account, &from)?;
                self.events.status(self.account, format!("Renamed to {to}"));
                self.refresh_mailboxes().await?;
            }
            Command::DeleteMailbox { mailbox, .. } => {
                let connection = self.connect().await?;
                connection.delete_mailbox(&mailbox).await?;
                self.store.clear_mailbox(self.account, &mailbox)?;
                self.events.status(self.account, format!("Deleted {mailbox}"));
                self.refresh_mailboxes().await?;
            }
            Command::Search {
                mailbox, query, scope, include_spam_and_trash, generation, ..
            } => {
                self.search(&mailbox, &query, scope, include_spam_and_trash, generation).await?;
            }
            Command::Send { draft } => {
                self.send(draft).await?;
            }
            // Handled by the supervisor, never routed to a worker.
            Command::CancelSearch { .. }
            | Command::SignIn(_)
            | Command::SignOut(_)
            | Command::Shutdown => {}
        }
        Ok(())
    }

    /// Returns a live connection, dialling one if necessary.
    async fn connect(&mut self) -> Result<&mut ImapConnection> {
        if self.connection.is_none() {
            let account = self.account_config()?;
            self.events.emit(Event::Connection {
                account: self.account,
                state: ConnectionState::Connecting,
            });
            self.events
                .status(self.account, format!("Connecting to {}\u{2026}", account.imap_host));

            let credential = self.credential(&account).await?;
            let connection = ImapConnection::connect(&account, &credential).await?;

            self.events
                .emit(Event::Connection { account: self.account, state: ConnectionState::Online });
            self.events.status(self.account, "Connected");
            self.connection = Some(connection);
        }

        Ok(self.connection.as_mut().expect("just dialled"))
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
        self.backfill_contacts();

        // Counting is one round trip per mailbox, which on a large account
        // is slower than the first sync the user is waiting for. Queue it
        // behind whatever is already pending rather than ahead of it.
        let _ = self.supervisor.send(Command::CountUnread(self.account));
        Ok(())
    }

    /// Counts unread messages and reports each mailbox as it is measured.
    ///
    /// One round trip per mailbox, so the counts arrive progressively rather
    /// than the sidebar waiting for all of them. A mailbox that cannot be
    /// counted is skipped: a missing badge is a better outcome than failing
    /// the command that asked for it.
    async fn report_unread(&mut self, mailboxes: &[String]) {
        for mailbox in mailboxes {
            let Ok(connection) = self.connect().await else { return };
            match connection.unread_count(mailbox).await {
                Ok(unseen) => self.events.emit(Event::MailboxStats {
                    account: self.account,
                    mailbox: mailbox.clone(),
                    unseen,
                }),
                Err(e) => tracing::debug!("could not count {mailbox}: {e}"),
            }
        }
    }

    /// Re-fetches envelopes that were cached without their headers.
    ///
    /// Nothing else would: a sync asks only for UIDs above the high-water
    /// mark, so a row stored from a FETCH that carried no header section
    /// keeps its placeholder subject for the life of the cache. These are
    /// asked for by UID, so the cost is proportional to the damage — nothing
    /// at all in the ordinary case, where there is none.
    async fn repair_envelopes(&mut self, mailbox: &str) -> Result<()> {
        const MAX_REPAIRS: u32 = 200;

        let account = self.account;
        let uids = self.store.unparseable_uids(account, mailbox, MAX_REPAIRS)?;
        if uids.is_empty() {
            return Ok(());
        }

        tracing::info!(account, mailbox, count = uids.len(), "re-fetching headerless envelopes");
        let set = imap::uid_set(&uids);

        let connection = self.connection.as_mut().expect("connected by the caller");
        let repaired: Vec<Envelope> = connection
            .fetch_envelopes(&set)
            .await?
            .into_iter()
            // The server may no longer have them, and a second failure must
            // not rewrite the row with the same placeholder.
            .filter(|e| e.subject != parse::UNPARSEABLE_SUBJECT)
            .collect();
        if repaired.is_empty() {
            return Ok(());
        }

        self.store.save_envelopes(account, mailbox, &repaired)?;
        self.record_contacts(self.is_outgoing(mailbox), &repaired);
        self.events.emit(Event::Envelopes {
            account,
            mailbox: mailbox.to_string(),
            envelopes: repaired,
        });
        Ok(())
    }

    /// Reconciles one mailbox with the server: new messages, flag changes and
    /// deletions.
    async fn sync(&mut self, mailbox: &str) -> Result<()> {
        let initial_count = self.config.read().unwrap().ui.initial_sync_count.max(50);
        let account = self.account;
        let connection = self.connect().await?;

        let selected = connection.select(mailbox).await?;
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
            self.record_contacts(self.is_outgoing(mailbox), &fresh);
            self.events.emit(Event::Envelopes {
                account,
                mailbox: mailbox.to_string(),
                envelopes: fresh.clone(),
            });
        }

        self.repair_envelopes(mailbox).await?;

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
        self.report_unread(&[mailbox.to_string()]).await;
        self.events.status(account, "Up to date");
        Ok(())
    }

    /// Files the addresses on a batch of envelopes for recipient completion.
    ///
    /// `outgoing` marks a mailbox whose recipients the user chose — Sent or
    /// Drafts — where the `To:` line is worth much more than a `From:`.
    fn record_contacts(&self, outgoing: bool, envelopes: &[Envelope]) {
        let mut addresses = Vec::new();
        let mut recipients = Vec::new();
        for envelope in envelopes {
            addresses.extend(envelope.from.iter().cloned());
            recipients.extend(envelope.to.iter().cloned());
            recipients.extend(envelope.cc.iter().cloned());
        }

        let (weight, recipient_weight) = if outgoing {
            (contact_weight::SEEN, contact_weight::ADDRESSED)
        } else {
            (contact_weight::SEEN, contact_weight::SEEN)
        };
        let _ = self.store.record_contacts(self.account, &addresses, weight);
        let _ = self.store.record_contacts(self.account, &recipients, recipient_weight);
    }

    /// Whether a mailbox holds mail the user sent rather than received.
    fn is_outgoing(&self, mailbox: &str) -> bool {
        self.store
            .load_mailboxes(self.account)
            .unwrap_or_default()
            .into_iter()
            .find(|candidate| candidate.name == mailbox)
            .is_some_and(|candidate| {
                matches!(candidate.special, SpecialUse::Sent | SpecialUse::Drafts)
            })
    }

    /// Mines addresses out of mail that was cached before contacts were being
    /// recorded, so completion works from the first compose rather than only
    /// for mail that arrives from now on.
    fn backfill_contacts(&self) {
        if self.store.contact_count(self.account).unwrap_or(0) > 0 {
            return;
        }
        let mailboxes = self.store.load_mailboxes(self.account).unwrap_or_default();
        let mut mined = 0usize;
        for mailbox in mailboxes.iter().filter(|mailbox| mailbox.selectable) {
            let envelopes =
                self.store.load_envelopes(self.account, &mailbox.name, 20_000).unwrap_or_default();
            let outgoing = matches!(mailbox.special, SpecialUse::Sent | SpecialUse::Drafts);
            mined += envelopes.len();
            self.record_contacts(outgoing, &envelopes);
        }
        if mined > 0 {
            tracing::info!("built contacts from {mined} cached messages");
        }
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

    /// Warms the cache with one queued message.
    ///
    /// Failures are not reported: the user has not asked for these. A failure
    /// does stop the run, because it usually means the connection is gone and
    /// the rest of the queue would fail the same way; what is left is dropped
    /// rather than retried, since every one of them is fetched on demand the
    /// moment it is opened.
    async fn prefetch_one(&mut self) {
        let Some((mailbox, uid)) = self.prefetch.pop_front() else { return };

        if self.store.has_body(self.account, &mailbox, uid) {
            return;
        }
        if self.load_body(&mailbox, uid, false).await.is_err() {
            // Same reasoning as a failed command: the connection may have
            // been left in an unknown state, and the rest of the queue would
            // fail behind it. Dropped so the next real command reconnects
            // rather than inheriting it.
            self.drop_connection();
            self.prefetch.clear();
        }
        if self.prefetch.is_empty() {
            let _ = self.store.prune_bodies(BODY_CACHE_BYTES);
        }
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

    async fn set_flag(&mut self, mailbox: &str, uids: &[u32], bit: u16, add: bool) -> Result<()> {
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
        self.report_unread(&[mailbox.to_string(), destination.to_string()]).await;
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
            Some(trash) if trash != mailbox => self.move_messages(mailbox, uids, &trash).await,
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

    /// Runs a search over one mailbox, its subtree, or the whole account.
    ///
    /// Base IMAP has no cross-folder search, so anything wider than one
    /// mailbox means selecting and searching each in turn. Gmail is the
    /// exception worth special-casing: its `\All` mailbox already contains
    /// every message, so a whole-account search is one round trip there
    /// instead of thirty.
    async fn search(
        &mut self,
        mailbox: &str,
        query: &str,
        scope: SearchScope,
        include_spam_and_trash: bool,
        generation: u64,
    ) -> Result<()> {
        let account = self.account;
        let criteria = Query::parse(query)?.to_imap()?;
        let targets = self.search_targets(mailbox, scope, include_spam_and_trash)?;

        let mut results: Vec<Envelope> = Vec::new();
        let mut searched = 0usize;

        for target in &targets {
            if self.search_superseded(generation) {
                return Ok(());
            }
            self.events.status(
                account,
                format!("Searching {target} ({}/{})", searched + 1, targets.len()),
            );

            let connection = self.connect().await?;
            if connection.selected_mailbox() != Some(target.as_str()) {
                // A mailbox can disappear between listing and searching.
                if connection.select(target).await.is_err() {
                    continue;
                }
            }
            let connection = self.connection.as_mut().expect("connected above");

            let uids = connection.search(&criteria).await?;
            searched += 1;
            if uids.is_empty() {
                continue;
            }

            // Bound per mailbox as well as overall; a bare term can match
            // tens of thousands in a single folder.
            let uids: Vec<u32> = uids.into_iter().take(SEARCH_LIMIT).collect();

            // Fetched in batches rather than in one call, so that giving up
            // is answered somewhere other than the end. The search that
            // matters most here is the whole-account one on Gmail, and that
            // visits exactly one mailbox: checking between mailboxes would
            // never once get to look.
            for batch in uids.chunks(SEARCH_BATCH) {
                if self.search_superseded(generation) {
                    return Ok(());
                }
                let connection = self.connection.as_mut().expect("connected above");
                let envelopes = connection.fetch_envelopes(&imap::uid_set(batch)).await?;
                // Kept even though the results are not sent: the envelopes
                // are as good in the cache as any other, and the work is
                // already done.
                self.store.save_envelopes(account, target, &envelopes)?;
                results.extend(envelopes);
            }

            if results.len() >= SEARCH_LIMIT {
                break;
            }
        }

        if self.search_superseded(generation) {
            return Ok(());
        }

        results.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.uid.cmp(&a.uid)));
        results.truncate(SEARCH_LIMIT);

        self.events.emit(Event::SearchResults {
            account,
            mailbox: mailbox.to_string(),
            envelopes: results,
            generation,
        });
        Ok(())
    }

    /// Whether the search being run has been given up on: replaced by a
    /// newer one, or cleared. Checked rather than signalled, because the
    /// worker is inside this command and cannot take another.
    fn search_superseded(&self, generation: u64) -> bool {
        let current = self.search_generation.load(Ordering::Relaxed);
        if current != generation {
            tracing::debug!(
                account = self.account,
                generation,
                current,
                "abandoning a superseded search"
            );
            return true;
        }
        false
    }

    /// The mailboxes a search should cover, in the order to visit them.
    fn search_targets(
        &self,
        mailbox: &str,
        scope: SearchScope,
        include_spam_and_trash: bool,
    ) -> Result<Vec<String>> {
        if scope == SearchScope::Folder {
            return Ok(vec![mailbox.to_string()]);
        }
        Ok(search_targets(
            &self.store.load_mailboxes(self.account)?,
            mailbox,
            scope,
            include_spam_and_trash,
        ))
    }

    async fn send(&mut self, draft: Draft) -> Result<()> {
        let account = self.account_config()?;
        self.events.status(self.account, "Sending\u{2026}");

        let credential = self.credential(&account).await?;
        let sent = smtp::send(&account, &credential, &draft).await?;

        // The strongest signal there is about who the user writes to.
        let recipients: Vec<_> = [&draft.to, &draft.cc, &draft.bcc]
            .iter()
            .flat_map(|list| super::parse::parse_address_list(list))
            .collect();
        let _ = self.store.record_contacts(self.account, &recipients, contact_weight::SENT_TO);

        self.events.emit(Event::Sent);
        self.events.status(self.account, "Message sent");

        // File a copy in Sent. Gmail does this server-side, so skip it there
        // to avoid a duplicate.
        if !account.imap_host.contains("gmail.com")
            && let Some(sent_box) = self
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

/// The mailboxes a search should cover, in the order to visit them.
///
/// An `\All` mailbox stands in for the whole account, which turns a
/// thirty-round-trip search into one. It is not quite everything, though:
/// RFC 6154 permits `\All` to omit `\Trash` and `\Junk`, and Gmail does
/// exactly that. Whether those two belong in the results is the user's call,
/// so it is asked for rather than assumed — but when they are wanted, they
/// have to be named explicitly, or a message in the bin is simply not found.
fn search_targets(
    mailboxes: &[MailboxInfo],
    mailbox: &str,
    scope: SearchScope,
    include_spam_and_trash: bool,
) -> Vec<String> {
    let selectable = |candidate: &&MailboxInfo| candidate.selectable;
    let is_spam_or_trash =
        |candidate: &MailboxInfo| matches!(candidate.special, SpecialUse::Trash | SpecialUse::Junk);

    if scope == SearchScope::All
        && let Some(all) = mailboxes
            .iter()
            .filter(selectable)
            .find(|candidate| candidate.special == SpecialUse::All)
    {
        let mut targets = vec![all.name.clone()];
        if include_spam_and_trash {
            targets.extend(
                mailboxes
                    .iter()
                    .filter(selectable)
                    .filter(|candidate| is_spam_or_trash(candidate))
                    .map(|candidate| candidate.name.clone()),
            );
        }
        targets.dedup();
        return targets;
    }

    let mut targets: Vec<String> = mailboxes
        .iter()
        .filter(selectable)
        .filter(|candidate| match scope {
            // Without an All mailbox every folder is visited, so the two are
            // dropped here instead of skipped there.
            SearchScope::All => include_spam_and_trash || !is_spam_or_trash(candidate),
            SearchScope::Subtree => candidate.name == mailbox || is_descendant(candidate, mailbox),
            SearchScope::Folder => candidate.name == mailbox,
        })
        .map(|candidate| candidate.name.clone())
        .collect();

    // Search the mailbox in view first: its results are the ones the user is
    // most likely waiting for.
    targets.sort_by_key(|name| (name != mailbox, name.clone()));
    if targets.is_empty() {
        targets.push(mailbox.to_string());
    }
    targets
}

/// Whether `candidate` sits underneath `parent` in the folder hierarchy.
fn is_descendant(candidate: &MailboxInfo, parent: &str) -> bool {
    let Some(delimiter) = candidate.delimiter.as_deref().filter(|d| !d.is_empty()) else {
        return false;
    };
    candidate.name.starts_with(&format!("{parent}{delimiter}"))
}

fn plural(count: usize) -> String {
    if count == 1 { "1 message".to_string() } else { format!("{count} messages") }
}

/// Stores a password for an account, used by the account editor.
pub fn save_password(account: AccountId, password: &str) -> Result<()> {
    secrets::set(SecretKind::Password, account, password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(search_generation: Arc<AtomicU64>) -> AccountWorker {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        AccountWorker {
            account: 1,
            search_generation,
            config: Arc::new(RwLock::new(Config::default())),
            store: Arc::new(Store::open_memory().unwrap()),
            events: EventSink { tx: event_tx, repaint: Arc::new(|| {}) },
            tokens: Arc::new(TokenStore::new()),
            connection: None,
            supervisor: command_tx,
            idle_cancel: Arc::new(Notify::new()),
            idle_running: false,
            prefetch: VecDeque::new(),
        }
    }

    /// A search reads the counter the supervisor writes, so it can be told to
    /// stop while it is running — which is the only time it could be, the
    /// worker being inside the command for the whole of it.
    #[test]
    fn a_running_search_notices_that_it_has_been_superseded() {
        let generation = Arc::new(AtomicU64::new(7));
        let worker = worker(generation.clone());

        assert!(!worker.search_superseded(7), "the search running is the one wanted");

        // A second search, or a cleared box: either way the supervisor moves
        // the counter on while this one is still going.
        generation.store(8, Ordering::Relaxed);
        assert!(worker.search_superseded(7), "the search carried on answering a withdrawn query");
        assert!(!worker.search_superseded(8), "the search that replaced it stopped as well");
    }

    #[test]
    fn the_prefetch_queue_keeps_the_newest_requests() {
        let mut queue = VecDeque::new();

        enqueue_prefetch(&mut queue, "INBOX", 1..=3);
        assert_eq!(
            queue.iter().map(|(_, uid)| *uid).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "requests are fetched in the order they were asked for"
        );

        // Well past the limit, in two goes, so the drop has to span them.
        let over = PREFETCH_QUEUE_LIMIT as u32 + 50;
        enqueue_prefetch(&mut queue, "INBOX", 10..10 + over);
        assert_eq!(queue.len(), PREFETCH_QUEUE_LIMIT, "the queue grew past its limit");

        let uids: Vec<u32> = queue.iter().map(|(_, uid)| *uid).collect();
        assert_eq!(
            uids.last(),
            Some(&(10 + over - 1)),
            "the newest request was dropped instead of the oldest"
        );
        assert!(!uids.contains(&1), "the oldest request survived the overflow");
    }

    #[test]
    fn the_prefetch_queue_carries_the_mailbox_each_request_came_from() {
        let mut queue = VecDeque::new();
        enqueue_prefetch(&mut queue, "INBOX", 1..=2);
        enqueue_prefetch(&mut queue, "[Gmail]/Sent Mail", 7..=7);

        assert_eq!(queue.back().unwrap().0, "[Gmail]/Sent Mail");
        assert_eq!(queue.front().unwrap().0, "INBOX");
    }

    /// A cached body is answered by the supervisor, so it does not queue
    /// behind whatever the account worker is part way through.
    ///
    /// What is checkable from out here is that it is answered *once*. The
    /// worker is still sent the command, because it refreshes the stored
    /// preview, and it would otherwise read the same cache and announce the
    /// same body a second time — making the reader lay out the message twice
    /// for no change. That the first answer is also the faster one is a
    /// property of a busy worker, which a test with no account cannot make
    /// busy; it is the reason for the arrangement rather than a claim this
    /// proves.
    #[test]
    fn a_cached_body_is_announced_once() {
        let store = Arc::new(Store::open_memory().unwrap());
        let raw = b"From: Lance <lance@example.net>\r\n\
                    Subject: Re: The Harbor Point Interactive Map\r\n\
                    \r\n\
                    The map is up to date now.\r\n";
        store.save_raw(1, "INBOX", 41349, raw).unwrap();

        let config = Arc::new(RwLock::new(Config::default()));
        let mut engine = Engine::start(config, store, || {}).unwrap();
        engine.send(Command::FetchBody {
            account: 1,
            mailbox: "INBOX".into(),
            uid: 41349,
            served: false,
        });

        // Long enough for the worker to have had its turn as well.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        let mut bodies = Vec::new();
        while std::time::Instant::now() < deadline {
            bodies.extend(engine.poll().into_iter().filter_map(|event| match event {
                Event::Body { uid: 41349, body, .. } => Some(body),
                _ => None,
            }));
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(bodies.len(), 1, "the body was announced {} times", bodies.len());
        assert!(bodies[0].text.as_deref().unwrap_or_default().contains("up to date"));
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

    /// A Gmail account: an All Mail that omits Trash and Spam, plus labels.
    fn gmail() -> Vec<MailboxInfo> {
        vec![
            mailbox("INBOX", SpecialUse::Inbox),
            mailbox("[Gmail]/All Mail", SpecialUse::All),
            mailbox("[Gmail]/Trash", SpecialUse::Trash),
            mailbox("[Gmail]/Spam", SpecialUse::Junk),
            mailbox("Work", SpecialUse::Normal),
            mailbox("Work/Reports", SpecialUse::Normal),
        ]
    }

    #[test]
    fn searching_everything_covers_the_bin_and_the_spam() {
        // All Mail alone would miss them: RFC 6154 lets it, and Gmail does.
        let targets = search_targets(&gmail(), "INBOX", SearchScope::All, true);
        assert_eq!(targets[0], "[Gmail]/All Mail", "the cheap path was not used");
        assert!(targets.contains(&"[Gmail]/Trash".to_string()), "Trash was skipped");
        assert!(targets.contains(&"[Gmail]/Spam".to_string()), "Spam was skipped");
        // Still cheap: three round trips, not one per folder.
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn without_an_all_mailbox_every_folder_is_searched() {
        let plain = vec![
            mailbox("INBOX", SpecialUse::Inbox),
            mailbox("Archive", SpecialUse::Archive),
            mailbox("Trash", SpecialUse::Trash),
        ];
        let targets = search_targets(&plain, "INBOX", SearchScope::All, true);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0], "INBOX", "the open folder should be searched first");
    }

    #[test]
    fn the_bin_and_the_spam_can_be_left_out() {
        let targets = search_targets(&gmail(), "INBOX", SearchScope::All, false);
        assert_eq!(targets, vec!["[Gmail]/All Mail"]);

        // And on a server with no All mailbox, where they would otherwise be
        // visited along with everything else.
        let plain = vec![
            mailbox("INBOX", SpecialUse::Inbox),
            mailbox("Archive", SpecialUse::Archive),
            mailbox("Trash", SpecialUse::Trash),
            mailbox("Spam", SpecialUse::Junk),
        ];
        let targets = search_targets(&plain, "INBOX", SearchScope::All, false);
        assert_eq!(targets, vec!["INBOX", "Archive"]);
    }

    #[test]
    fn the_setting_does_not_touch_a_narrower_scope() {
        // Searching inside Trash itself must work whatever the option says.
        let with = search_targets(&gmail(), "[Gmail]/Trash", SearchScope::Subtree, true);
        let without = search_targets(&gmail(), "[Gmail]/Trash", SearchScope::Subtree, false);
        assert_eq!(with, vec!["[Gmail]/Trash"]);
        assert_eq!(without, vec!["[Gmail]/Trash"]);
    }

    #[test]
    fn a_subtree_covers_the_folder_and_its_children() {
        let targets = search_targets(&gmail(), "Work", SearchScope::Subtree, true);
        assert_eq!(targets, vec!["Work", "Work/Reports"]);
    }

    #[test]
    fn a_subtree_of_a_leaf_is_just_that_leaf() {
        let targets = search_targets(&gmail(), "Work/Reports", SearchScope::Subtree, true);
        assert_eq!(targets, vec!["Work/Reports"]);
    }

    #[test]
    fn unselectable_containers_are_never_searched() {
        let mut boxes = gmail();
        boxes.push(MailboxInfo {
            name: "[Gmail]".into(),
            delimiter: Some("/".into()),
            special: SpecialUse::Normal,
            selectable: false,
            unseen: 0,
        });
        let targets = search_targets(&boxes, "INBOX", SearchScope::All, true);
        assert!(!targets.contains(&"[Gmail]".to_string()));
    }

    #[test]
    fn a_mailbox_the_account_no_longer_lists_is_still_searched() {
        // Better to ask the server than to return nothing at all.
        let targets = search_targets(&[], "Somewhere", SearchScope::Subtree, true);
        assert_eq!(targets, vec!["Somewhere"]);
    }
}
