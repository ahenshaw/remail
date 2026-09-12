//! The application: state, the event pump, and the window layout.
//!
//! One rule keeps this tractable: the UI never touches the network and never
//! blocks. It sends [`Command`]s to the mail engine, drains [`Event`]s once per
//! frame, and renders whatever state those events left behind.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::Context;
use elegance::{
    Accent, Button, ButtonSize, Theme, TextInput, Toast, Toasts, BadgeTone, glyphs,
};

use crate::config::{AccountId, Config};
use crate::html::Prepared;
use crate::html::native::TextureCache;
use crate::mail::{
    Command, ConnectionState, Draft, Engine, Envelope, Event, Flags, MessageBody, MessageKey,
    RowKey, SearchScope, SpecialUse, Store,
};
use crate::secrets;
use crate::ui::accounts::{AccountsAction, AccountsDialog};
use crate::ui::compose::{ComposeAction, ComposeState};
use crate::ui::images::RemoteImages;
use crate::ui::sidebar::{AccountView, SidebarInput};
use crate::ui::{Action, message_list, reader, sidebar};

/// How many messages either side of the viewport to warm the cache with.
const PREFETCH_MARGIN: usize = 6;

/// The message currently open in the reader.
struct OpenMessage {
    key: MessageKey,
    envelope: Envelope,
    body: Option<Arc<MessageBody>>,
    /// Sanitized and lowered body, absent for a plain-text-only message.
    prepared: Option<Prepared>,
    /// Per-message override of the remote-content policy.
    allow_remote: bool,
    show_source: bool,
    /// When the body arrived, for the delayed mark-as-read.
    opened_at: Instant,
    marked_read: bool,
}

pub struct RemailApp {
    config: Arc<RwLock<Config>>,
    store: Arc<Store>,
    engine: Engine,

    accounts: HashMap<AccountId, AccountView>,
    /// The mailbox on screen.
    open_mailbox: Option<(AccountId, String)>,
    envelopes: Vec<Envelope>,

    cursor: Option<RowKey>,
    selection: BTreeSet<RowKey>,
    /// Anchor for shift-click range selection.
    anchor: Option<RowKey>,
    scroll_to_cursor: bool,

    search: String,
    search_scope: SearchScope,
    /// Results of a server-side search, replacing the mailbox listing.
    search_results: Option<Vec<Envelope>>,

    open_message: Option<OpenMessage>,
    textures: TextureCache,
    remote_images: RemoteImages,
    /// UIDs already sent for prefetch, so the engine is not asked twice.
    prefetched: BTreeSet<u32>,
    /// Rows hidden before the server confirmed, kept so a failed move or
    /// delete can put them back.
    pending_removal: Vec<Envelope>,

    compose: Option<ComposeState>,
    accounts_dialog: Option<AccountsDialog>,
    settings_open: bool,

    status: String,
    /// Queued notifications, drained once per frame.
    pending_toasts: Vec<PendingToast>,
    theme: Theme,
    /// Theme currently installed, so it is only reinstalled on change.
    installed_theme: Option<crate::config::ThemeChoice>,
    /// Senders trusted to load remote content, refreshed when settings open.
    trusted_senders: u32,
    keyring_available: bool,

    #[cfg(feature = "servo")]
    servo: Option<crate::html::servo::ServoView>,
    #[cfg(feature = "servo")]
    servo_failed: bool,
}

impl RemailApp {
    pub fn new(ctx: &Context, config: Config, store: Store) -> Result<Self> {
        let config = Arc::new(RwLock::new(config));
        let store = Arc::new(store);

        let repaint_ctx = ctx.clone();
        let engine = Engine::start(config.clone(), store.clone(), move || {
            repaint_ctx.request_repaint();
        })?;

        let remote_ctx = ctx.clone();
        let remote_images =
            RemoteImages::new(engine.runtime(), move || remote_ctx.request_repaint());

        let theme = config.read().unwrap().ui.theme.theme();

        let mut app = Self {
            config,
            store,
            engine,
            accounts: HashMap::new(),
            open_mailbox: None,
            envelopes: Vec::new(),
            cursor: None,
            selection: BTreeSet::new(),
            anchor: None,
            scroll_to_cursor: false,
            search: String::new(),
            search_scope: SearchScope::default(),
            search_results: None,
            open_message: None,
            textures: TextureCache::new(),
            remote_images,
            prefetched: BTreeSet::new(),
            pending_removal: Vec::new(),
            compose: None,
            accounts_dialog: None,
            settings_open: false,
            status: String::new(),
            pending_toasts: Vec::new(),
            theme,
            installed_theme: None,
            trusted_senders: 0,
            keyring_available: secrets::available(),
            #[cfg(feature = "servo")]
            servo: None,
            #[cfg(feature = "servo")]
            servo_failed: false,
        };

        app.restore_cached_view();
        app.connect_all();
        Ok(app)
    }

    /// Paints from the cache before any network activity, so the window opens
    /// with content rather than an empty frame.
    fn restore_cached_view(&mut self) {
        let accounts: Vec<(AccountId, String)> = {
            let config = self.config.read().unwrap();
            config
                .accounts
                .iter()
                .filter(|a| a.enabled)
                .map(|a| (a.id, a.default_mailbox.clone()))
                .collect()
        };

        for (id, _) in &accounts {
            let mailboxes = self.store.load_mailboxes(*id).unwrap_or_default();
            let view = self.accounts.entry(*id).or_default();
            view.mailboxes = mailboxes;
        }

        if let Some((id, mailbox)) = accounts.first() {
            self.open_mailbox = Some((*id, mailbox.clone()));
            let limit = self.config.read().unwrap().ui.initial_sync_count;
            self.envelopes = self.store.load_envelopes(*id, mailbox, limit).unwrap_or_default();
        }
    }

    fn connect_all(&mut self) {
        let ids: Vec<AccountId> = {
            let config = self.config.read().unwrap();
            config.accounts.iter().filter(|a| a.enabled).map(|a| a.id).collect()
        };
        for id in ids {
            self.engine.send(Command::Connect(id));
        }
        if let Some((account, mailbox)) = self.open_mailbox.clone() {
            self.engine.send(Command::OpenMailbox { account, mailbox });
        }
    }

    // -- events ------------------------------------------------------------

    fn pump_events(&mut self) {
        for event in self.engine.poll() {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            // With several accounts configured, an unattributed status line
            // is ambiguous; name the account it came from.
            Event::Status { account, text } => {
                let config = self.config.read().unwrap();
                self.status = if config.accounts.len() > 1 {
                    match config.account(account) {
                        Some(account) => format!("{}: {text}", account.title()),
                        None => text,
                    }
                } else {
                    text
                };
            }

            Event::Error { account, text } => {
                self.status = text.clone();
                if self.accounts.get(&account).is_some_and(|v| v.needs_sign_in) {
                    return;
                }
                let title = match self.config.read().unwrap().account(account) {
                    Some(config) => format!("{} \u{2014} error", config.title()),
                    None => "Mail error".to_string(),
                };
                self.toast(&title, Some(text), BadgeTone::Danger);
            }

            Event::Connection { account, state } => {
                self.accounts.entry(account).or_default().state = state;
            }

            Event::Mailboxes { account, mailboxes } => {
                self.accounts.entry(account).or_default().mailboxes = mailboxes;
            }

            Event::Listing { account, mailbox, envelopes, from_cache } => {
                if !self.is_open(account, &mailbox) {
                    return;
                }
                // A cached listing may arrive after a server one if the user
                // reopens a mailbox quickly; never let it overwrite fresher
                // data with a stale snapshot.
                if from_cache && !self.envelopes.is_empty() {
                    return;
                }
                self.envelopes = envelopes;
                self.sort_envelopes();
            }

            Event::MailboxStats { account, mailbox, exists, unseen } => {
                if let Some(view) = self.accounts.get_mut(&account) {
                    if let Some(info) = view.mailboxes.iter_mut().find(|m| m.name == mailbox) {
                        info.exists = exists;
                        info.unseen = unseen;
                    }
                }
            }

            Event::Envelopes { account, mailbox, envelopes } => {
                if !self.is_open(account, &mailbox) {
                    return;
                }
                for envelope in envelopes {
                    match self.envelopes.iter_mut().find(|e| e.uid == envelope.uid) {
                        Some(existing) => *existing = envelope,
                        None => self.envelopes.push(envelope),
                    }
                }
                self.sort_envelopes();
            }

            Event::Vanished { account, mailbox, uids } => {
                if !self.is_current_account(account) {
                    return;
                }
                let rows = row_keys(&mailbox, &uids);
                // Usually already gone: this confirms an optimistic removal.
                // It also covers messages deleted from another client.
                take_rows(&mut self.pending_removal, &rows);
                take_rows(&mut self.envelopes, &rows);
                if let Some(results) = &mut self.search_results {
                    take_rows(results, &rows);
                }
                self.selection.retain(|key| !rows.contains(key));
                if self.cursor.as_ref().is_some_and(|key| rows.contains(key)) {
                    self.cursor = None;
                    self.open_message = None;
                }
            }

            Event::RemovalFailed { account, mailbox, uids } => {
                if !self.is_current_account(account) {
                    return;
                }
                self.restore_rows(&row_keys(&mailbox, &uids));
                self.toast(
                    "Could not remove messages",
                    Some("They have been put back.".into()),
                    BadgeTone::Warning,
                );
            }

            Event::FlagsChanged { account, mailbox, changes } => {
                if !self.is_current_account(account) {
                    return;
                }
                for (uid, flags) in changes {
                    let key = RowKey::new(mailbox.clone(), uid);
                    for envelope in self.envelopes.iter_mut().filter(|e| e.key() == key) {
                        envelope.flags = flags;
                    }
                    if let Some(results) = &mut self.search_results {
                        for envelope in results.iter_mut().filter(|e| e.key() == key) {
                            envelope.flags = flags;
                        }
                    }
                    if let Some(open) = &mut self.open_message {
                        if open.key.row() == key {
                            open.envelope.flags = flags;
                        }
                    }
                }
            }

            Event::Preview { account, mailbox, uid, preview, has_attachments } => {
                if !self.is_current_account(account) {
                    return;
                }
                let key = RowKey::new(mailbox, uid);
                for envelope in self.envelopes.iter_mut().filter(|e| e.key() == key) {
                    envelope.preview = preview.clone();
                    envelope.has_attachments = has_attachments;
                }
                if let Some(results) = &mut self.search_results {
                    for envelope in results.iter_mut().filter(|e| e.key() == key) {
                        envelope.preview = preview.clone();
                        envelope.has_attachments = has_attachments;
                    }
                }
            }

            Event::Body { account, mailbox, uid, body } => {
                let Some(open) = &mut self.open_message else { return };
                if open.key.account != account || open.key.mailbox != mailbox || open.key.uid != uid
                {
                    return;
                }
                let allow_remote = open.allow_remote;
                open.prepared = Prepared::from_parts(
                    body.html.as_deref(),
                    body.text.as_deref(),
                    allow_remote,
                );
                open.body = Some(body);
                open.opened_at = Instant::now();
                self.textures.clear();
            }

            Event::SearchResults { account, mailbox, envelopes } => {
                if !self.is_open(account, &mailbox) {
                    return;
                }
                let count = envelopes.len();
                self.search_results = Some(envelopes);
                self.status = format!("{count} matching messages");
            }

            Event::Sent => {
                if let Some(compose) = &mut self.compose {
                    compose.sending = false;
                }
                self.compose = None;
                self.toast("Message sent", None, BadgeTone::Ok);
            }

            Event::SignedIn { account } => {
                self.toast("Signed in", None, BadgeTone::Ok);
                let view = self.accounts.entry(account).or_default();
                view.state = ConnectionState::Connecting;
                view.needs_sign_in = false;
            }

            Event::NeedsSignIn { account } => {
                self.accounts.entry(account).or_default().needs_sign_in = true;
            }
        }
    }

    fn is_open(&self, account: AccountId, mailbox: &str) -> bool {
        self.open_mailbox.as_ref().is_some_and(|(a, m)| *a == account && m == mailbox)
    }

    /// Whether an event belongs to the account on screen. Used for events
    /// that carry their own mailbox and so may legitimately concern a folder
    /// other than the open one, as cross-folder search results do.
    fn is_current_account(&self, account: AccountId) -> bool {
        self.open_mailbox.as_ref().is_some_and(|(a, _)| *a == account)
    }

    /// Newest first, with a stable tiebreak so rows never jitter between
    /// frames when several messages share a timestamp.
    fn sort_envelopes(&mut self) {
        self.envelopes.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.uid.cmp(&a.uid)));
        self.envelopes.dedup_by_key(|e| e.uid);
    }

    // -- actions -----------------------------------------------------------

    fn apply(&mut self, action: Action) {
        match action {
            Action::OpenMailbox { account, mailbox } => self.open_mailbox(account, mailbox),
            Action::Connect(account) => self.engine.send(Command::Connect(account)),
            Action::SignIn(account) => self.engine.send(Command::SignIn(account)),

            Action::Focus(key) => {
                self.cursor = Some(key.clone());
                self.anchor = Some(key.clone());
                self.selection.clear();
                self.selection.insert(key);
                self.open_current();
            }
            Action::ToggleSelected(key) => {
                if !self.selection.remove(&key) {
                    self.selection.insert(key.clone());
                }
                self.cursor = Some(key.clone());
                self.anchor = Some(key);
            }
            Action::SelectRange(key) => self.select_range_to(&key),

            Action::ToggleStar(key) => {
                let starred = self
                    .visible()
                    .iter()
                    .find(|e| e.key() == key)
                    .is_some_and(|e| e.flags.has(Flags::FLAGGED));
                self.set_flag(&[key], Flags::FLAGGED, !starred);
            }
            Action::ToggleRead => {
                let targets = self.targets();
                if targets.is_empty() {
                    return;
                }
                let any_unread = self
                    .visible()
                    .iter()
                    .filter(|e| targets.contains(&e.key()))
                    .any(|e| e.flags.is_unread());
                self.set_flag(&targets, Flags::SEEN, any_unread);
            }

            Action::Archive => self.archive(),
            Action::Delete => self.delete(),
            Action::Reply { all } => self.start_reply(all),
            Action::Forward => self.start_forward(),
            Action::Compose => self.start_compose(),

            Action::Refresh => {
                if let Some((account, mailbox)) = self.open_mailbox.clone() {
                    self.engine.send(Command::Sync { account, mailbox });
                }
            }

            Action::SearchServer(query) => {
                if let Some((account, mailbox)) = self.open_mailbox.clone() {
                    let scope = self.search_scope;
                    self.status = format!(
                        "Searching {} for \u{201c}{query}\u{201d}\u{2026}",
                        scope.label().to_lowercase()
                    );
                    self.engine.send(Command::Search { account, mailbox, query, scope });
                }
            }
            Action::ClearSearch => {
                self.search.clear();
                self.search_results = None;
            }

            Action::LoadRemoteImages => {
                if let Some(open) = &self.open_message {
                    let key = remote_key(&open.envelope, &open.key.mailbox);
                    if let Err(e) = self.store.allow_remote_message(open.key.account, &key) {
                        tracing::warn!("could not remember image permission: {e}");
                    }
                }
                self.show_remote_images();
            }

            Action::AllowRemoteSender => {
                if let Some(open) = &self.open_message {
                    let sender =
                        open.envelope.from.first().map(|a| a.email.clone()).unwrap_or_default();
                    match self.store.allow_remote_sender(open.key.account, &sender) {
                        Ok(()) => {
                            self.status = format!("Loading remote content from {sender}")
                        }
                        Err(e) => tracing::warn!("could not remember sender: {e}"),
                    }
                }
                self.show_remote_images();
            }
            Action::OpenUrl(url) => self.open_url(&url),
            Action::SaveAttachment(index) => self.save_attachment(index),
        }
    }

    fn open_mailbox(&mut self, account: AccountId, mailbox: String) {
        if self.is_open(account, &mailbox) {
            return;
        }
        self.open_mailbox = Some((account, mailbox.clone()));
        self.envelopes.clear();
        self.selection.clear();
        self.cursor = None;
        self.anchor = None;
        self.open_message = None;
        self.search_results = None;
        self.prefetched.clear();
        self.pending_removal.clear();
        self.textures.clear();
        self.engine.send(Command::OpenMailbox { account, mailbox });
    }

    /// Loads the body for the cursor row into the reader.
    fn open_current(&mut self) {
        let Some(row) = self.cursor.clone() else { return };
        let Some((account, _)) = self.open_mailbox.clone() else { return };
        let Some(envelope) = self.visible().iter().find(|e| e.key() == row).cloned() else {
            return;
        };

        // Search results span folders, so the row says where it lives.
        let mailbox = envelope.mailbox.clone();
        let uid = envelope.uid;
        let key = MessageKey { account, mailbox: mailbox.clone(), uid };
        if self.open_message.as_ref().is_some_and(|open| open.key == key) {
            return;
        }

        // Remote content is allowed when the setting says always, or when
        // this message or its sender was trusted on a previous visit.
        let sender = envelope.from.first().map(|a| a.email.clone()).unwrap_or_default();
        let allow_remote = self.config.read().unwrap().ui.load_remote_content
            || self
                .store
                .remote_allowed(account, &remote_key(&envelope, &mailbox), &sender)
                .unwrap_or(false);

        self.open_message = Some(OpenMessage {
            key,
            envelope,
            body: None,
            prepared: None,
            allow_remote,
            show_source: false,
            opened_at: Instant::now(),
            marked_read: false,
        });
        self.textures.clear();
        self.remote_images.clear();
        self.engine.send(Command::FetchBody { account, mailbox, uid });
    }

    fn select_range_to(&mut self, key: &RowKey) {
        let visible = self.visible();
        let Some(end) = visible.iter().position(|e| &e.key() == key) else { return };
        let start = self
            .anchor
            .as_ref()
            .and_then(|anchor| visible.iter().position(|e| &e.key() == anchor))
            .unwrap_or(end);
        let (low, high) = if start <= end { (start, end) } else { (end, start) };

        self.selection = visible[low..=high].iter().map(Envelope::key).collect();
        self.cursor = Some(key.clone());
    }

    /// The messages an action applies to: the multi-selection if there is one,
    /// otherwise the cursor row.
    fn targets(&self) -> Vec<RowKey> {
        if self.selection.is_empty() {
            self.cursor.clone().into_iter().collect()
        } else {
            self.selection.iter().cloned().collect()
        }
    }

    /// Groups rows by the mailbox they live in, since every server operation
    /// works against one selected mailbox at a time.
    fn by_mailbox(rows: &[RowKey]) -> Vec<(String, Vec<u32>)> {
        let mut grouped: Vec<(String, Vec<u32>)> = Vec::new();
        for row in rows {
            match grouped.iter_mut().find(|(mailbox, _)| *mailbox == row.mailbox) {
                Some((_, uids)) => uids.push(row.uid),
                None => grouped.push((row.mailbox.clone(), vec![row.uid])),
            }
        }
        grouped
    }

    fn set_flag(&mut self, rows: &[RowKey], bit: u16, add: bool) {
        let Some((account, _)) = self.open_mailbox.clone() else { return };
        if rows.is_empty() {
            return;
        }

        // Update locally first; the engine confirms or corrects it.
        for envelope in self.envelopes.iter_mut().filter(|e| rows.contains(&e.key())) {
            envelope.flags.set(bit, add);
        }
        if let Some(results) = &mut self.search_results {
            for envelope in results.iter_mut().filter(|e| rows.contains(&e.key())) {
                envelope.flags.set(bit, add);
            }
        }
        if let Some(open) = &mut self.open_message {
            if rows.contains(&open.key.row()) {
                open.envelope.flags.set(bit, add);
            }
        }

        // One command per mailbox: a server operates on the selected one.
        for (mailbox, uids) in Self::by_mailbox(rows) {
            self.engine.send(Command::SetFlag { account, mailbox, uids, bit, add });
        }
    }

    fn archive(&mut self) {
        let Some((account, mailbox)) = self.open_mailbox.clone() else { return };
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }

        let destination = self
            .accounts
            .get(&account)
            .and_then(|view| {
                view.mailboxes
                    .iter()
                    .find(|m| m.special == SpecialUse::Archive)
                    .or_else(|| view.mailboxes.iter().find(|m| m.special == SpecialUse::All))
            })
            .map(|m| m.name.clone());

        let Some(destination) = destination else {
            self.status = "No archive mailbox on this account".into();
            return;
        };

        // Rows already in the archive have nowhere to go.
        let movable: Vec<RowKey> =
            targets.into_iter().filter(|row| row.mailbox != destination).collect();
        if movable.is_empty() {
            self.status = "Already archived".into();
            return;
        }

        self.remove_rows(&movable);
        for (mailbox, uids) in Self::by_mailbox(&movable) {
            self.engine.send(Command::Move {
                account,
                mailbox,
                uids,
                destination: destination.clone(),
            });
        }
        let _ = mailbox;
    }

    fn delete(&mut self) {
        let Some((account, _)) = self.open_mailbox.clone() else { return };
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        self.remove_rows(&targets);
        for (mailbox, uids) in Self::by_mailbox(&targets) {
            self.engine.send(Command::Delete { account, mailbox, uids });
        }
    }

    /// Hides rows immediately and lands the cursor on whatever takes their
    /// place.
    ///
    /// The server round trip takes long enough to see, so waiting for
    /// confirmation would move the cursor now and shift the list a moment
    /// later. The rows are kept in `pending_removal` until the server either
    /// confirms (`Vanished`) or refuses (`RemovalFailed`).
    fn remove_rows(&mut self, rows: &[RowKey]) {
        // Where the first removed row sits today: the cursor should land on
        // whichever message slides up into that position.
        let landing = landing_index(&self.visible(), rows);

        self.pending_removal.extend(take_rows(&mut self.envelopes, rows));
        if let Some(results) = &mut self.search_results {
            take_rows(results, rows);
        }

        self.selection.clear();
        self.open_message = None;

        let visible = self.visible();
        let next = visible.get(landing).or_else(|| visible.last()).map(Envelope::key);
        self.cursor = next.clone();
        self.anchor = next.clone();
        self.scroll_to_cursor = true;
        if next.is_some() {
            self.open_current();
        }
    }

    /// Puts back rows the server refused to remove.
    fn restore_rows(&mut self, rows: &[RowKey]) {
        let restored = take_rows(&mut self.pending_removal, rows);
        if restored.is_empty() {
            return;
        }
        // Sorting by date puts each row back where it belongs, so the exact
        // index it held before does not need to be tracked.
        self.envelopes.extend(restored.iter().cloned());
        self.sort_envelopes();

        // A search view is a separate list; it needs the rows back too, or
        // they stay missing until the search is re-run.
        if let Some(results) = &mut self.search_results {
            results.extend(restored);
            results.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.uid.cmp(&a.uid)));
            results.dedup_by_key(|e| e.uid);
        }
    }

    fn start_compose(&mut self) {
        let Some(account) = self.current_account() else {
            self.status = "Add an account first".into();
            return;
        };
        self.compose = Some(ComposeState::new(Draft { account, ..Default::default() }));
    }

    fn start_reply(&mut self, all: bool) {
        let Some(open) = &self.open_message else { return };
        let Some(body) = &open.body else {
            self.status = "Still loading that message".into();
            return;
        };
        let self_address = self
            .config
            .read()
            .unwrap()
            .account(open.key.account)
            .map(|a| a.email.clone())
            .unwrap_or_default();

        let draft = crate::mail::smtp::reply_draft(
            open.key.account,
            &open.envelope,
            body,
            all,
            &self_address,
        );
        self.compose = Some(ComposeState::new(draft));

        // Replying implies having read it.
        let row = open.key.row();
        self.set_flag(&[row], Flags::ANSWERED, true);
    }

    fn start_forward(&mut self) {
        let Some(open) = &self.open_message else { return };
        let Some(body) = &open.body else {
            self.status = "Still loading that message".into();
            return;
        };
        let draft = crate::mail::smtp::forward_draft(open.key.account, &open.envelope, body);
        self.compose = Some(ComposeState::new(draft));
    }

    fn current_account(&self) -> Option<AccountId> {
        self.open_mailbox
            .as_ref()
            .map(|(account, _)| *account)
            .or_else(|| self.config.read().unwrap().accounts.first().map(|a| a.id))
    }

    /// Re-prepares the open message with remote content permitted.
    fn show_remote_images(&mut self) {
        let Some(open) = &mut self.open_message else { return };
        open.allow_remote = true;
        if let Some(body) = &open.body {
            open.prepared =
                Prepared::from_parts(body.html.as_deref(), body.text.as_deref(), true);
        }
        self.textures.clear();
        #[cfg(feature = "servo")]
        if let Some(servo) = &mut self.servo {
            // Force a reload so the engine picks up the new markup.
            servo.load(u64::MAX, "");
        }
    }

    /// Re-prepares the open message with remote content blocked again.
    fn hide_remote_images(&mut self) {
        let Some(open) = &mut self.open_message else { return };
        if !open.allow_remote {
            return;
        }
        open.allow_remote = false;
        if let Some(body) = &open.body {
            open.prepared =
                Prepared::from_parts(body.html.as_deref(), body.text.as_deref(), false);
        }
        self.textures.clear();
        self.remote_images.clear();
        #[cfg(feature = "servo")]
        if let Some(servo) = &mut self.servo {
            servo.load(u64::MAX, "");
        }
    }

    fn open_url(&mut self, url: &str) {
        // Only hand the system browser schemes a mail reader should follow.
        let allowed = ["http://", "https://", "mailto:", "tel:"];
        if !allowed.iter().any(|prefix| url.starts_with(prefix)) {
            self.status = format!("Refused to open unsupported link: {url}");
            return;
        }
        if let Err(e) = open::that_detached(url) {
            self.status = format!("Could not open link: {e}");
        }
    }

    fn save_attachment(&mut self, index: usize) {
        let Some(open) = &self.open_message else { return };
        let Some(body) = &open.body else { return };
        let Some(attachment) = body.attachments.get(index) else { return };

        let directory = directories::UserDirs::new()
            .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let path = unique_path(&directory, &sanitize_filename(&attachment.filename));

        match std::fs::write(&path, &attachment.data) {
            Ok(()) => {
                self.status = format!("Saved to {}", path.display());
                let description = path.display().to_string();
                self.toast("Attachment saved", Some(description), BadgeTone::Ok);
            }
            Err(e) => self.status = format!("Could not save attachment: {e}"),
        }
    }

    /// Which envelopes the list shows: search results if a search is active,
    /// otherwise the mailbox filtered by the query box.
    fn visible(&self) -> Vec<Envelope> {
        if let Some(results) = &self.search_results {
            return results.clone();
        }
        let needle = self.search.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.envelopes.clone();
        }
        self.envelopes.iter().filter(|e| e.matches(&needle)).cloned().collect()
    }

    /// Queues a notification. Events are handled before the frame has a
    /// `Context` to draw into, so toasts are buffered rather than shown here.
    fn toast(&mut self, title: &str, description: Option<String>, tone: BadgeTone) {
        self.pending_toasts.push(PendingToast {
            title: title.to_string(),
            description,
            tone,
        });
    }
}

/// A notification waiting for the next frame.
struct PendingToast {
    title: String,
    description: Option<String>,
    tone: BadgeTone,
}

/// Builds row keys for a set of UIDs that all live in one mailbox.
fn row_keys(mailbox: &str, uids: &[u32]) -> Vec<RowKey> {
    uids.iter().map(|uid| RowKey::new(mailbox, *uid)).collect()
}

/// Identifies a message for remembering remote-content permission.
///
/// `Message-ID` is preferred because it survives the message being moved
/// between folders and a `UIDVALIDITY` reset, both of which change the UID.
/// The mailbox and UID are only a fallback for messages that carry no id.
fn remote_key(envelope: &Envelope, mailbox: &str) -> String {
    if envelope.message_id.trim().is_empty() {
        format!("{mailbox}#{}", envelope.uid)
    } else {
        envelope.message_id.trim().to_string()
    }
}

/// Splits the rows matching `uids` out of `list` and returns them, preserving
/// the order of both the kept and the removed rows.
fn take_rows(list: &mut Vec<Envelope>, rows: &[RowKey]) -> Vec<Envelope> {
    let mut taken = Vec::new();
    list.retain(|envelope| {
        let removing = rows.contains(&envelope.key());
        if removing {
            taken.push(envelope.clone());
        }
        !removing
    });
    taken
}

/// The row index the cursor should land on once `uids` are gone: the position
/// of the first one removed, which is where the next message slides up to.
fn landing_index(view: &[Envelope], rows: &[RowKey]) -> usize {
    view.iter().position(|e| rows.contains(&e.key())).unwrap_or(0)
}

/// Strips path separators so a sender cannot choose where a file lands.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    if trimmed.is_empty() { "attachment".to_string() } else { trimmed.to_string() }
}

/// Adds ` (2)`, ` (3)`… before the extension rather than overwriting.
fn unique_path(directory: &std::path::Path, filename: &str) -> std::path::PathBuf {
    let candidate = directory.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let path = std::path::Path::new(filename);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let extension = path.extension().map(|e| format!(".{}", e.to_string_lossy()));

    for index in 2..1000 {
        let name = format!("{stem} ({index}){}", extension.as_deref().unwrap_or(""));
        let candidate = directory.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(uids: &[u32]) -> Vec<Envelope> {
        in_mailbox("INBOX", uids)
    }

    fn in_mailbox(mailbox: &str, uids: &[u32]) -> Vec<Envelope> {
        uids.iter()
            .map(|uid| Envelope {
                uid: *uid,
                mailbox: mailbox.to_string(),
                // Descending dates, matching the newest-first display order.
                date: 1_000 - *uid as i64,
                ..Default::default()
            })
            .collect()
    }

    fn uids(list: &[Envelope]) -> Vec<u32> {
        list.iter().map(|e| e.uid).collect()
    }

    fn keys(mailbox: &str, uids: &[u32]) -> Vec<RowKey> {
        uids.iter().map(|uid| RowKey::new(mailbox, *uid)).collect()
    }

    #[test]
    fn rows_in_different_mailboxes_are_distinct() {
        // The reason rows are not keyed by UID alone: a cross-folder search
        // can put the same UID from two mailboxes in one list.
        let mut rows = in_mailbox("INBOX", &[7]);
        rows.extend(in_mailbox("Archive", &[7]));

        let taken = take_rows(&mut rows, &keys("INBOX", &[7]));
        assert_eq!(taken.len(), 1, "removed more than the requested row");
        assert_eq!(taken[0].mailbox, "INBOX");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mailbox, "Archive", "removed the wrong folder's row");
    }

    #[test]
    fn groups_rows_by_the_mailbox_they_live_in() {
        let rows = vec![
            RowKey::new("INBOX", 1),
            RowKey::new("Archive", 5),
            RowKey::new("INBOX", 2),
        ];
        let grouped = RemailApp::by_mailbox(&rows);
        assert_eq!(grouped.len(), 2);
        // Order follows first appearance, so the visible folder goes first.
        assert_eq!(grouped[0], ("INBOX".to_string(), vec![1, 2]));
        assert_eq!(grouped[1], ("Archive".to_string(), vec![5]));
    }

    #[test]
    fn grouping_an_empty_selection_yields_nothing() {
        assert!(RemailApp::by_mailbox(&[]).is_empty());
    }

    #[test]
    fn prefers_the_message_id_as_a_remote_key() {
        let mut envelope = Envelope { uid: 7, ..Default::default() };
        envelope.message_id = "  <abc@example.com>  ".into();
        assert_eq!(remote_key(&envelope, "INBOX"), "<abc@example.com>");
    }

    #[test]
    fn falls_back_to_mailbox_and_uid_without_a_message_id() {
        let envelope = Envelope { uid: 7, ..Default::default() };
        assert_eq!(remote_key(&envelope, "INBOX"), "INBOX#7");
    }

    #[test]
    fn takes_rows_out_in_one_pass() {
        let mut rows = list(&[1, 2, 3, 4, 5]);
        let taken = take_rows(&mut rows, &keys("INBOX", &[2, 4]));
        assert_eq!(uids(&rows), vec![1, 3, 5]);
        assert_eq!(uids(&taken), vec![2, 4]);
    }

    #[test]
    fn taking_nothing_leaves_the_list_alone() {
        let mut rows = list(&[1, 2, 3]);
        assert!(take_rows(&mut rows, &keys("INBOX", &[99])).is_empty());
        assert_eq!(uids(&rows), vec![1, 2, 3]);
    }

    #[test]
    fn cursor_lands_where_the_first_removed_row_was() {
        let rows = list(&[1, 2, 3, 4, 5]);
        // Deleting the third row: the cursor should land on index 2, which
        // after removal holds what was row 4.
        assert_eq!(landing_index(&rows, &keys("INBOX", &[3])), 2);
        // A multi-selection lands on the topmost removed row.
        assert_eq!(landing_index(&rows, &keys("INBOX", &[4, 2])), 1);
        // Deleting the first row lands at the top.
        assert_eq!(landing_index(&rows, &keys("INBOX", &[1])), 0);
    }

    #[test]
    fn landing_index_survives_rows_that_are_already_gone() {
        let rows = list(&[1, 2, 3]);
        assert_eq!(landing_index(&rows, &keys("INBOX", &[42])), 0);
        assert_eq!(landing_index(&[], &keys("INBOX", &[1])), 0);
    }

    #[test]
    fn restoring_puts_rows_back_in_date_order() {
        // What `restore_rows` does: take the rows back, append, re-sort.
        let mut rows = list(&[1, 2, 3, 4]);
        let mut pending = take_rows(&mut rows, &keys("INBOX", &[2, 3]));
        assert_eq!(uids(&rows), vec![1, 4]);

        let restored = take_rows(&mut pending, &keys("INBOX", &[2, 3]));
        rows.extend(restored);
        rows.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.uid.cmp(&a.uid)));

        assert_eq!(uids(&rows), vec![1, 2, 3, 4], "rows did not return to place");
        assert!(pending.is_empty());
    }

    #[test]
    fn strips_paths_from_attachment_names() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("   "), "attachment");
        assert_eq!(sanitize_filename(".bashrc"), "bashrc");
    }
}

// -- frame ----------------------------------------------------------------

impl eframe::App for RemailApp {
    /// Non-drawing work. Runs before `ui`, and also while the window is
    /// hidden, so background mail activity keeps being processed.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.pump_events();
        self.sync_theme(ctx);
        self.handle_shortcuts(ctx);
        self.tick_mark_read();

        #[cfg(feature = "servo")]
        self.update_servo(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut action = None;

        // Three surfaces, deepest to shallowest: folders, messages, reading.
        // In a light theme `card` is lighter than `bg`, so the reading column
        // is the brightest thing on screen and the folder list recedes.
        let palette = &self.theme.palette;
        let folders_fill = palette.depth_tint(palette.bg, 0.07);
        let messages_fill = palette.bg;
        let reading_fill = palette.card;
        let surface = |fill: egui::Color32, margin: i8| {
            egui::Frame::new().fill(fill).inner_margin(margin)
        };

        egui::Panel::top("toolbar").show(ui, |ui| {
            action = action.take().or(self.toolbar(ui));
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            self.status_bar(ui);
        });

        egui::Panel::left("sidebar")
            .default_size(200.0)
            // Narrow enough to become a strip of icons and initials.
            .size_range(56.0..=460.0)
            .frame(surface(folders_fill, 2))
            .show(ui, |ui| {
                let config = self.config.read().unwrap().clone();
                let selected = self
                    .open_mailbox
                    .as_ref()
                    .map(|(account, mailbox)| (*account, mailbox.as_str()));
                let found = sidebar::show(
                    ui,
                    SidebarInput {
                        config: &config,
                        accounts: &mut self.accounts,
                        selected,
                        style: config.ui.folders,
                        base_size: config.ui.font_size,
                        theme: &self.theme,
                    },
                );
                action = action.take().or(found);
            });

        egui::Panel::left("messages")
            .default_size(380.0)
            .size_range(180.0..=760.0)
            .frame(surface(messages_fill, 0))
            .show(ui, |ui| {
                action = action.take().or(self.message_list(ui));
            });

        egui::CentralPanel::default()
            .frame(surface(reading_fill, 8))
            .show(ui, |ui| {
                action = action.take().or(self.reader(ui));
            });

        self.dialogs(&ctx);
        self.toasts_frame(&ctx);

        if let Some(action) = action {
            self.apply(action);
        }
    }

    fn on_exit(&mut self) {
        self.engine.send(Command::Shutdown);
        if let Err(e) = self.config.read().unwrap().save() {
            tracing::warn!("could not save configuration: {e}");
        }
    }
}

impl RemailApp {
    /// Installs the theme when the setting changes, and keeps the body font
    /// size in step with it.
    fn sync_theme(&mut self, ctx: &Context) {
        let (choice, font_size) = {
            let config = self.config.read().unwrap();
            (config.ui.theme, config.ui.font_size)
        };
        if self.installed_theme != Some(choice) {
            self.theme = choice.theme();
            self.theme.clone().install(ctx);
            self.installed_theme = Some(choice);
        }

        // Elegance sets its own text styles; scale the body style to the
        // configured size without disturbing the rest of the scale.
        ctx.all_styles_mut(|style| {
            if let Some(font) = style.text_styles.get_mut(&egui::TextStyle::Body) {
                if (font.size - font_size).abs() > f32::EPSILON {
                    font.size = font_size;
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            if ui
                .add(
                    Button::new(format!("{} Compose", glyphs::PENCIL))
                        .accent(Accent::Blue)
                        .size(ButtonSize::Small),
                )
                .clicked()
            {
                action = Some(Action::Compose);
            }
            if ui
                .add(
                    Button::new(glyphs::REFRESH.to_string())
                        .size(ButtonSize::Small)
                        .outline(),
                )
                .on_hover_text("Sync this mailbox (F5)")
                .clicked()
            {
                action = Some(Action::Refresh);
            }

            ui.separator();

            let has_target = self.cursor.is_some() || !self.selection.is_empty();
            if ui
                .add(
                    Button::new(format!("{} Archive", glyphs::FOLDER))
                        .size(ButtonSize::Small)
                        .outline()
                        .enabled(has_target),
                )
                .on_hover_text("Archive (E)")
                .clicked()
            {
                action = Some(Action::Archive);
            }
            if ui
                .add(
                    Button::new(format!("{} Delete", glyphs::TRASH))
                        .size(ButtonSize::Small)
                        .outline()
                        .accent(Accent::Red)
                        .enabled(has_target),
                )
                .on_hover_text("Delete (Del)")
                .clicked()
            {
                action = Some(Action::Delete);
            }
            if ui
                .add(
                    Button::new(format!("{} Read", glyphs::EYE))
                        .size(ButtonSize::Small)
                        .outline()
                        .enabled(has_target),
                )
                .on_hover_text("Toggle read (U)")
                .clicked()
            {
                action = Some(Action::ToggleRead);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(Button::new(glyphs::SETTINGS.to_string()).size(ButtonSize::Small).outline())
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.settings_open = true;
                    self.trusted_senders = self.count_trusted_senders();
                }
                if ui
                    .add(Button::new(glyphs::KEY.to_string()).size(ButtonSize::Small).outline())
                    .on_hover_text("Accounts")
                    .clicked()
                {
                    self.accounts_dialog = Some(AccountsDialog::default());
                }

                ui.add_space(8.0);

                // Scope applies to the server-side search that Enter runs;
                // the as-you-type filter always works on what is loaded.
                let mut scope = self.search_scope;
                ui.add(
                    elegance::Select::new("search-scope", &mut scope)
                        .options(SearchScope::all().map(|s| (s, s.label())))
                        .width(130.0),
                )
                .on_hover_text("How far Enter searches");
                if scope != self.search_scope {
                    self.search_scope = scope;
                    // A narrower or wider scope invalidates what is on screen.
                    if self.search_results.is_some() && !self.search.trim().is_empty() {
                        action = Some(Action::SearchServer(self.search.trim().to_string()));
                    }
                }

                let search = ui.add(
                    TextInput::new(&mut self.search)
                        .hint(match self.search_scope {
                            SearchScope::Folder => "Search this folder",
                            SearchScope::Subtree => "Search with subfolders",
                            SearchScope::All => "Search all folders",
                        })
                        .compact(true)
                        .desired_width(210.0),
                );
                // Enter escalates from the local filter to a server search,
                // which reaches messages that are not cached locally.
                if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let query = self.search.trim().to_string();
                    if !query.is_empty() {
                        action = Some(Action::SearchServer(query));
                    }
                }
                if self.search_results.is_some()
                    && ui
                        .add(Button::new(glyphs::X.to_string()).size(ButtonSize::Small).outline())
                        .on_hover_text("Clear search results")
                        .clicked()
                {
                    action = Some(Action::ClearSearch);
                }
            });
        });
        ui.add_space(4.0);
        action
    }

    fn message_list(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let visible = self.visible();
        let (compact, base_size, style) = {
            let config = self.config.read().unwrap();
            (config.ui.compact_list, config.ui.font_size, config.ui.messages)
        };
        let font = crate::ui::pane_font(style, base_size);

        let empty_message = if self.open_mailbox.is_none() {
            "Select a mailbox"
        } else if self.search_results.is_some() {
            "No messages matched"
        } else if !self.search.trim().is_empty() {
            "No cached messages matched"
        } else {
            "No messages"
        };

        let scroll_to_cursor = std::mem::take(&mut self.scroll_to_cursor);
        let output = message_list::show(
            ui,
            message_list::ListInput {
                envelopes: &visible,
                cursor: self.cursor.clone(),
                selection: &self.selection,
                compact,
                show_folder: self.search_results.is_some(),
                base_size: font.size,
                family: font.family.clone(),
                scroll_to_cursor,
                empty_message,
            },
        );

        self.prefetch(&visible, output.visible);
        output.action
    }

    /// Asks the engine to cache bodies around the viewport, so scrolling then
    /// clicking rarely waits on the network.
    fn prefetch(&mut self, visible: &[Envelope], range: std::ops::Range<usize>) {
        let Some((account, mailbox)) = self.open_mailbox.clone() else { return };
        if visible.is_empty() {
            return;
        }

        let start = range.start.saturating_sub(PREFETCH_MARGIN);
        let end = (range.end + PREFETCH_MARGIN).min(visible.len());
        let uids: Vec<u32> = visible[start..end]
            .iter()
            .map(|e| e.uid)
            .filter(|uid| self.prefetched.insert(*uid))
            .collect();

        if !uids.is_empty() {
            self.engine.send(Command::Prefetch { account, mailbox, uids });
        }
    }

    fn reader(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let (base_size, style) = {
            let config = self.config.read().unwrap();
            (config.ui.font_size, config.ui.reading)
        };
        // Resolved before the mutable borrow of `open_message` below.
        let servo_active = self.servo_active();

        // Split the borrow: the reader needs the open message plus caches.
        let Some(open) = &mut self.open_message else {
            return reader::show(
                ui,
                reader::ReaderInput {
                    envelope: None,
                    body: None,
                    prepared: None,
                    textures: &mut self.textures,
                    remote: &mut self.remote_images,
                    allow_remote: false,
                    base_size,
                    style,
                    show_source: &mut false,
                    loading: false,
                    theme: &self.theme,
                    servo_drawing: false,
                },
            );
        };

        let servo_drawing = servo_active && !open.show_source && open.prepared.is_some();

        let action = reader::show(
            ui,
            reader::ReaderInput {
                envelope: Some(&open.envelope),
                body: open.body.as_deref(),
                prepared: open.prepared.as_ref(),
                textures: &mut self.textures,
                remote: &mut self.remote_images,
                allow_remote: open.allow_remote,
                base_size,
                style,
                show_source: &mut open.show_source,
                loading: open.body.is_none(),
                theme: &self.theme,
                servo_drawing,
            },
        );

        #[cfg(feature = "servo")]
        if servo_drawing {
            if let Some(servo) = &mut self.servo {
                // Match the offscreen surface to the space left under the
                // header, in physical pixels, so text is laid out at the
                // width it will actually be shown at.
                let scale = ui.ctx().pixels_per_point();
                let available = ui.available_size();
                servo.resize((
                    (available.x * scale).max(1.0) as u32,
                    (available.y * scale).max(1.0) as u32,
                ));
                servo.show(ui);
            }
        }

        action
    }

    /// Whether the Servo backend is selected and running.
    fn servo_active(&self) -> bool {
        #[cfg(feature = "servo")]
        {
            self.config.read().unwrap().ui.html_backend == crate::config::HtmlBackend::Servo
                && self.servo.is_some()
        }
        #[cfg(not(feature = "servo"))]
        {
            false
        }
    }

    #[cfg(feature = "servo")]
    fn update_servo(&mut self, ctx: &Context) {
        let wanted = self.config.read().unwrap().ui.html_backend
            == crate::config::HtmlBackend::Servo;
        if !wanted || self.servo_failed {
            return;
        }

        if self.servo.is_none() {
            // Starting Servo is expensive, so it happens on first need rather
            // than at launch.
            match crate::html::servo::ServoView::new(ctx, (1024, 768)) {
                Ok(view) => self.servo = Some(view),
                Err(e) => {
                    tracing::error!("could not start Servo: {e}");
                    self.status = format!("Servo unavailable: {e}");
                    self.servo_failed = true;
                    self.config.write().unwrap().ui.html_backend =
                        crate::config::HtmlBackend::Native;
                    return;
                }
            }
        }

        let Some(servo) = &mut self.servo else { return };
        let dark = self.theme.palette.is_dark;

        if let Some(open) = &self.open_message {
            if let Some(prepared) = &open.prepared {
                let key = message_hash(&open.key, open.allow_remote);
                servo.load(key, &crate::html::servo::document(&prepared.html, dark));
            }
        }
        servo.update(ctx);

        // Keep frames coming until the document settles, otherwise a page
        // that finishes loading after the last input would never repaint.
        if !servo.is_ready() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Marks the open message `\Seen` once it has been on screen long enough
    /// that the user plausibly read it.
    fn tick_mark_read(&mut self) {
        let delay = self.config.read().unwrap().ui.mark_read_after_secs;
        if delay <= 0.0 {
            return;
        }

        let Some(open) = &self.open_message else { return };
        if open.marked_read || open.body.is_none() || !open.envelope.flags.is_unread() {
            return;
        }
        if open.opened_at.elapsed().as_secs_f32() < delay {
            return;
        }

        let row = open.key.row();
        if let Some(open) = &mut self.open_message {
            open.marked_read = true;
        }
        self.set_flag(&[row], Flags::SEEN, true);
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        // Typing in a text field must not trigger single-letter shortcuts.
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        let mut action = None;
        ctx.input(|input| {
            use egui::Key;
            let shift = input.modifiers.shift;

            if input.key_pressed(Key::J) || input.key_pressed(Key::ArrowDown) {
                action = Some(Nav::Next);
            } else if input.key_pressed(Key::K) || input.key_pressed(Key::ArrowUp) {
                action = Some(Nav::Previous);
            } else if input.key_pressed(Key::R) {
                action = Some(Nav::Act(Action::Reply { all: shift }));
            } else if input.key_pressed(Key::F) {
                action = Some(Nav::Act(Action::Forward));
            } else if input.key_pressed(Key::C) {
                action = Some(Nav::Act(Action::Compose));
            } else if input.key_pressed(Key::E) {
                action = Some(Nav::Act(Action::Archive));
            } else if input.key_pressed(Key::U) {
                action = Some(Nav::Act(Action::ToggleRead));
            } else if input.key_pressed(Key::S) {
                if let Some(key) = self.cursor.clone() {
                    action = Some(Nav::Act(Action::ToggleStar(key)));
                }
            } else if input.key_pressed(Key::Delete) || input.key_pressed(Key::Backspace) {
                action = Some(Nav::Act(Action::Delete));
            } else if input.key_pressed(Key::F5) {
                action = Some(Nav::Act(Action::Refresh));
            } else if input.key_pressed(Key::Escape) {
                action = Some(Nav::Act(Action::ClearSearch));
            }
        });

        match action {
            Some(Nav::Next) => self.move_cursor(1),
            Some(Nav::Previous) => self.move_cursor(-1),
            Some(Nav::Act(action)) => self.apply(action),
            None => {}
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = self
            .cursor
            .as_ref()
            .and_then(|key| visible.iter().position(|e| &e.key() == key))
            .map(|index| index as isize);

        let next = match current {
            Some(index) => (index + delta).clamp(0, visible.len() as isize - 1),
            None if delta > 0 => 0,
            None => visible.len() as isize - 1,
        } as usize;

        let key = visible[next].key();
        self.cursor = Some(key.clone());
        self.anchor = Some(key.clone());
        self.selection.clear();
        self.selection.insert(key);
        self.scroll_to_cursor = true;
        self.open_current();
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let total = self.envelopes.len();
            let unread = self.envelopes.iter().filter(|e| e.flags.is_unread()).count();
            let counts = if unread > 0 {
                format!("{total} messages, {unread} unread")
            } else {
                format!("{total} messages")
            };
            ui.label(self.theme.faint_text(counts));

            if !self.selection.is_empty() {
                ui.separator();
                ui.label(self.theme.muted_text(format!("{} selected", self.selection.len())));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.status.is_empty() {
                    ui.label(self.theme.muted_text(&self.status));
                }
            });
        });
        ui.add_space(2.0);
    }

    fn dialogs(&mut self, ctx: &Context) {
        // Compose.
        if let Some(compose) = &mut self.compose {
            let account_id = compose.draft.account;
            let config = self.config.read().unwrap().clone();
            let account = config.account(account_id);

            match crate::ui::compose::show(ctx, compose, account, &self.theme) {
                Some(ComposeAction::Send) => {
                    compose.sending = true;
                    let draft = compose.draft.clone();
                    self.engine.send(Command::Send { draft });
                }
                Some(ComposeAction::Close) => {
                    let discard = !compose.has_content();
                    compose.open = false;
                    if discard {
                        self.compose = None;
                    } else {
                        // Keep the draft in memory; closing the window should
                        // not silently destroy typing.
                        self.compose = None;
                        Toast::new("Draft discarded")
                            .tone(BadgeTone::Warning)
                            .show(ctx);
                    }
                }
                Some(ComposeAction::AttachFile) => self.attach_file(),
                Some(ComposeAction::RemoveAttachment(index)) => {
                    if index < compose.draft.attachments.len() {
                        compose.draft.attachments.remove(index);
                    }
                }
                None => {}
            }
        }

        // Accounts.
        if let Some(dialog) = &mut self.accounts_dialog {
            let config = self.config.read().unwrap().clone();
            let found =
                crate::ui::accounts::show(ctx, dialog, &config, self.keyring_available, &self.theme);

            // Apply the side effects the editor staged, now that the account
            // itself has been written to the config.
            let staged = dialog.pending_password.take();
            let sign_in = dialog.pending_sign_in.take();
            let closed = !dialog.open;

            if let Some(action) = found {
                self.apply_accounts_action(action);
            }
            if let Some((account, password)) = staged {
                match crate::mail::engine::save_password(account, &password) {
                    Ok(()) => self.engine.send(Command::Connect(account)),
                    Err(e) => self.status = format!("Could not save password: {e}"),
                }
            }
            if let Some(account) = sign_in {
                self.engine.send(Command::SignIn(account));
            }
            if closed {
                self.accounts_dialog = None;
            }
        }

        // Settings.
        if self.settings_open {
            let mut open = self.settings_open;
            let servo_available = cfg!(feature = "servo");
            let changed = {
                let mut config = self.config.write().unwrap();
                crate::ui::accounts::settings(
                    ctx,
                    &mut open,
                    &mut config,
                    servo_available,
                    self.trusted_senders,
                    &self.theme,
                )
            };
            self.settings_open = open;
            if let Some(action) = changed {
                self.apply_accounts_action(action);
            }
        }
    }

    fn apply_accounts_action(&mut self, action: AccountsAction) {
        match action {
            AccountsAction::Save(account) => {
                let id = account.id;
                {
                    let mut config = self.config.write().unwrap();
                    match config.account_mut(id) {
                        Some(existing) => *existing = account,
                        None => config.accounts.push(account),
                    }
                }
                self.save_config();
                self.engine.send(Command::Connect(id));
            }
            AccountsAction::SignIn(account) => self.engine.send(Command::SignIn(account)),
            AccountsAction::SignOut(account) => self.engine.send(Command::SignOut(account)),
            AccountsAction::Remove(account) => {
                self.engine.send(Command::SignOut(account));
                self.config.write().unwrap().accounts.retain(|a| a.id != account);
                self.save_config();
                let _ = self.store.forget_account(account);
                self.accounts.remove(&account);
                if self.open_mailbox.as_ref().is_some_and(|(a, _)| *a == account) {
                    self.open_mailbox = None;
                    self.envelopes.clear();
                    self.open_message = None;
                }
            }
            AccountsAction::SettingsChanged => self.save_config(),
            AccountsAction::ForgetRemoteSenders => {
                let accounts: Vec<AccountId> = {
                    let config = self.config.read().unwrap();
                    config.accounts.iter().map(|a| a.id).collect()
                };
                for account in accounts {
                    if let Err(e) = self.store.forget_remote_permissions(account) {
                        self.status = format!("Could not revoke permissions: {e}");
                    }
                }
                self.trusted_senders = 0;
                // Revoking should be visible at once, including in whatever
                // is open, rather than only from the next message onwards.
                self.hide_remote_images();
                self.save_config();
                self.toast("Remote content permissions cleared", None, BadgeTone::Ok);
            }
        }
    }

    fn count_trusted_senders(&self) -> u32 {
        let config = self.config.read().unwrap();
        config
            .accounts
            .iter()
            .filter_map(|a| self.store.remote_sender_count(a.id).ok())
            .sum()
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.read().unwrap().save() {
            self.status = format!("Could not save settings: {e}");
        }
    }

    /// Adds files to the open draft. Uses a native picker when one is
    /// available and otherwise says so, rather than failing silently.
    fn attach_file(&mut self) {
        let Some(compose) = &mut self.compose else { return };
        match pick_files() {
            Some(paths) if !paths.is_empty() => compose.draft.attachments.extend(paths),
            Some(_) => {}
            None => {
                self.status =
                    "No file picker available; install zenity or kdialog to attach files".into();
            }
        }
    }

    fn toasts_frame(&mut self, ctx: &Context) {
        for pending in self.pending_toasts.drain(..) {
            let mut toast = Toast::new(pending.title).tone(pending.tone);
            if let Some(description) = pending.description {
                toast = toast.description(description);
            }
            if pending.tone == BadgeTone::Danger {
                toast = toast.duration(Duration::from_secs(8));
            }
            toast.show(ctx);
        }

        Toasts::new()
            .anchor(egui::Align2::RIGHT_BOTTOM)
            .offset([12.0, 12.0])
            .max_visible(4)
            .render(ctx);
    }
}

/// Keyboard navigation outcome, kept separate so cursor moves and actions do
/// not have to share one enum.
enum Nav {
    Next,
    Previous,
    Act(Action),
}

/// A stable key for "which document is Servo showing", including the remote
/// content decision, which changes the markup.
#[cfg(feature = "servo")]
fn message_hash(key: &MessageKey, allow_remote: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.account.hash(&mut hasher);
    key.mailbox.hash(&mut hasher);
    key.uid.hash(&mut hasher);
    allow_remote.hash(&mut hasher);
    hasher.finish()
}

/// Shells out to a desktop file picker. Avoids a GTK dependency for a feature
/// used once in a while; returns `None` when no picker exists.
fn pick_files() -> Option<Vec<std::path::PathBuf>> {
    use std::process::Command as Process;

    let attempts: [(&str, Vec<&str>); 2] = [
        ("zenity", vec!["--file-selection", "--multiple", "--separator=\n"]),
        ("kdialog", vec!["--getopenfilename", ".", "--multiple", "--separate-output"]),
    ];

    for (program, args) in attempts {
        let Ok(output) = Process::new(program).args(&args).output() else { continue };
        if !output.status.success() {
            // A non-zero status is usually the user cancelling.
            return Some(Vec::new());
        }
        return Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| std::path::PathBuf::from(line.trim()))
                .filter(|path| path.is_file())
                .collect(),
        );
    }
    None
}
