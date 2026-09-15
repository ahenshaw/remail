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
use elegance::{Accent, BadgeTone, Button, ButtonSize, TextInput, Theme, Toast, Toasts, glyphs};

use crate::config::{AccountId, Config};
use crate::html::Prepared;
use crate::html::native::TextureCache;
use crate::mail::{
    Command, ConnectionState, Draft, Engine, Envelope, Event, Flags, MessageBody, MessageKey,
    RowKey, SearchScope, SpecialUse, Store,
};
use crate::secrets;
use crate::ui::accounts::{AccountsAction, AccountsDialog, FolderAction, FolderEdit};
use crate::ui::compose::{ComposeAction, ComposeState};
use crate::ui::images::RemoteImages;
use crate::ui::sidebar::{AccountView, SidebarInput};
use crate::ui::{Action, message_list, reader, sidebar};

/// How many messages either side of the viewport to warm the cache with.
/// How many cached envelopes a saved search is answered from while the server
/// is being asked. The whole cache, in practice: a query is cheap to run over
/// envelopes already in memory, and stopping short would hide older results
/// that the server is about to return anyway.
const CACHED_SEARCH_LIMIT: u32 = 50_000;

const PREFETCH_MARGIN: usize = 6;

/// Hover text for the search box. The language is only useful if it is
/// discoverable from the box it applies to; see `mail::query`.
/// The `remail-cli` invocation that runs the search currently on screen.
///
/// The bridge between the two surfaces: a query built by hand, with the scope
/// selector and the spam toggle doing what they do, comes back out as
/// something that can be scripted or handed to an agent. What it must not be
/// is approximately right — a command that quietly searches somewhere else is
/// worse than no command at all, so the mailbox is always named (the tool
/// defaults to the account's, not the one on screen) and the query always
/// goes after `--` (the query language negates with a leading `-`, which is
/// otherwise indistinguishable from an option).
///
/// `--server` follows what produced what is on screen: results that came back
/// from an IMAP SEARCH, or the filter over the cache that typing does. The
/// tool makes the same distinction and defaults the same way.
fn search_command(
    query: &str,
    mailbox: &str,
    account: Option<AccountId>,
    scope: SearchScope,
    spam_and_trash: bool,
    server: bool,
) -> String {
    let mut out = String::from("remail-cli search");

    if let Some(account) = account {
        out.push_str(&format!(" --account {account}"));
    }
    out.push_str(&format!(" --mailbox {}", shell_quote(mailbox)));
    // How far a search reaches is a question for the server. The filter over
    // the cache reads the one mailbox it was given, here and in the tool
    // alike, so carrying the scope across would describe a reach the command
    // does not have.
    if server {
        out.push_str(" --server");
        match scope {
            // The tool's own default, so saying it adds nothing.
            SearchScope::Folder => {}
            SearchScope::Subtree => out.push_str(" --scope subtree"),
            SearchScope::All => out.push_str(" --scope all"),
        }
        // Only ever meant anything at the widest scope, which is the only
        // place the interface offers it.
        if spam_and_trash && scope == SearchScope::All {
            out.push_str(" --spam-and-trash");
        }
    }

    out.push_str(" -- ");
    out.push_str(&shell_quote(query));
    out
}

/// Wraps a word so a shell hands it over exactly as it is.
///
/// Single quotes, in which a shell interprets nothing at all — the escape for
/// a single quote inside them is to leave, quote it, and go back in. Mail
/// addresses and subjects contain everything eventually.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// What the query box's context menu offers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchMenu {
    Save,
    Copy,
}

/// Height of every control in the search bar.
///
/// Sits between the two controls that will not be told: `ButtonSize::Medium`
/// comes to 29pt and the query box at its full height to 30.12, neither of
/// them adjustable any nearer. The scope selector is held to this number
/// exactly, through the one lever it has — see `search_controls` — rather
/// than left to arrive near it on its own, which is what it was doing when
/// it kept turning up short.
///
/// A point of slack between the other two, centred, is not a thing the eye
/// can find. A selector sized by whichever font is drawing its label is.
const SEARCH_BAR_HEIGHT: f32 = 30.0;

const SEARCH_SYNTAX: &str = "\
Plain words search subject, sender and recipient.

  subject:invoice from:jane     both must match
  subject:\"quarterly report\"    quote a phrase
  from:jane OR from:paul        either
  -from:noreply                 exclude
  (a OR b) subject:c            group

  to: cc: bcc: body: text:      other fields
  is:unread is:starred is:read  state
  has:attachment                approximate on the server, exact locally
  since:7d before:2026-01-01    dates, or 2w / 3m / 1y
  larger:2m smaller:200k        size

Enter searches the server; typing filters what is already loaded.
Right-click for the remail-cli command that runs this search.";

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
    /// What each search folder is holding, keyed by the account and the
    /// folder's reserved name. A map rather than one slot: saved searches are
    /// folders too, and several can be full at once — opening one does not
    /// empty the rest.
    search_results: HashMap<(AccountId, String), Vec<Envelope>>,
    /// The folder the search now running will fill when it answers. Held
    /// because a search is started from the query box, which knows nothing
    /// about which folder asked for it.
    search_target: Option<(AccountId, String)>,
    /// Where the search was started from, to go back to when it is cleared.
    search_origin: Option<(AccountId, String)>,
    /// A server-side search is in flight. Drives the spinner in the search
    /// bar and dims the rows the search is about to replace, which until it
    /// returns are the local filter's answer rather than the one asked for.
    searching: bool,
    /// Which search the screen is waiting for. Raised whenever one is started
    /// or abandoned, so the results of an earlier one can be told apart from
    /// the results of this one: a search cannot be stopped once it is
    /// running, and a whole-account search runs long enough to be given up
    /// on, cleared, and replaced before it answers.
    search_generation: u64,

    open_message: Option<OpenMessage>,
    textures: TextureCache,
    remote_images: RemoteImages,
    /// Rows already sent for prefetch, so the engine is not asked twice.
    /// Keyed by row rather than by UID: in the search folder the rows come
    /// from several mailboxes, where a UID on its own names more than one.
    prefetched: BTreeSet<RowKey>,
    /// Rows hidden before the server confirmed, kept so a failed move or
    /// delete can put them back.
    pending_removal: Vec<Envelope>,

    compose: Option<ComposeState>,
    accounts_dialog: Option<AccountsDialog>,
    /// A folder create, rename or delete awaiting confirmation.
    folder_edit: Option<FolderEdit>,
    move_to: Option<crate::ui::move_to::MoveDialog>,
    /// Folders a drag is holding open. Never written to the config.
    spring: crate::ui::sidebar::SpringLoad,
    settings_open: bool,

    status: String,
    /// Queued notifications, drained once per frame.
    pending_toasts: Vec<PendingToast>,
    theme: Theme,
    /// Theme currently installed, so it is only reinstalled on change.
    installed_theme: Option<crate::config::ThemeChoice>,
    /// Senders trusted to load remote content, refreshed when settings open.
    trusted_senders: u32,
    /// When the panel layout last changed, for debouncing the config write.
    layout_dirty_since: Option<Instant>,
    /// System fonts, scanned once and installed into egui on first use.
    fonts: crate::ui::fonts::FontLibrary,
    font_picker: crate::ui::accounts::FontPicker,
    keyring_available: bool,
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
            search_results: HashMap::new(),
            search_target: None,
            search_origin: None,
            searching: false,
            search_generation: 0,
            open_message: None,
            textures: TextureCache::new(),
            remote_images,
            prefetched: BTreeSet::new(),
            pending_removal: Vec::new(),
            compose: None,
            accounts_dialog: None,
            folder_edit: None,
            move_to: None,
            spring: crate::ui::sidebar::SpringLoad::default(),
            settings_open: false,
            status: String::new(),
            pending_toasts: Vec::new(),
            theme,
            installed_theme: None,
            trusted_senders: 0,
            layout_dirty_since: None,
            fonts: crate::ui::fonts::FontLibrary::load(),
            font_picker: crate::ui::accounts::FontPicker::default(),
            keyring_available: secrets::available(),
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
                // Whatever failed, a search that was in flight is not coming
                // back; the spinner has to stop whether or not this is why.
                self.searching = false;
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
                // Folders that were renamed or deleted elsewhere would
                // otherwise accumulate in the config forever.
                let live: std::collections::HashSet<&str> =
                    mailboxes.iter().map(|m| m.name.as_str()).collect();
                let pruned = {
                    let mut config = self.config.write().unwrap();
                    match config.account_mut(account) {
                        Some(config) => {
                            let before = config.collapsed_folders.len();
                            config.collapsed_folders.retain(|name| live.contains(name.as_str()));
                            config.collapsed_folders.len() != before
                        }
                        None => false,
                    }
                };
                if pruned {
                    self.save_config();
                }

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

            Event::MailboxStats { account, mailbox, unseen } => {
                if let Some(view) = self.accounts.get_mut(&account)
                    && let Some(info) = view.mailboxes.iter_mut().find(|m| m.name == mailbox)
                {
                    info.unseen = unseen;
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
                for results in self.search_results.values_mut() {
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
                    for results in self.search_results.values_mut() {
                        for envelope in results.iter_mut().filter(|e| e.key() == key) {
                            envelope.flags = flags;
                        }
                    }
                    if let Some(open) = &mut self.open_message
                        && open.key.row() == key
                    {
                        open.envelope.flags = flags;
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
                for results in self.search_results.values_mut() {
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
                open.prepared =
                    Prepared::from_parts(body.html.as_deref(), body.text.as_deref(), allow_remote);
                open.body = Some(body);
                open.opened_at = Instant::now();
                self.textures.clear();
            }

            Event::SearchResults { account, mailbox, envelopes, generation } => {
                // A search that has been superseded or cleared still runs to
                // completion and still answers. Its results would otherwise
                // replace the listing the user is looking at now, minutes
                // after they stopped asking for them.
                //
                // Only the generation decides that. Where the reader happens
                // to be does not: the results have a folder of their own to
                // go into, and a search is not withdrawn by reading something
                // while it runs.
                if generation != self.search_generation {
                    return;
                }

                // Whether to go there, though, is exactly that question.
                // Still where the search was started, or already in the
                // folder: take them to it, which is what they asked for.
                // Gone somewhere else in the meantime: leave them, and let
                // the folder in the sidebar say the results are waiting.
                let expecting = self.is_open(account, &mailbox) || self.in_search_folder();

                // Into the folder that asked. A saved search fills its own;
                // the query box fills the unsaved one.
                let target = self
                    .search_target
                    .take()
                    .unwrap_or((account, crate::mail::model::SEARCH_MAILBOX.to_string()));

                let count = envelopes.len();
                self.search_results.insert(target.clone(), envelopes);
                self.searching = false;
                self.status = format!("{count} matching messages");

                if expecting {
                    self.open_mailbox(target.0, target.1);
                }
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
                let Some((account, mailbox)) = self.open_mailbox.clone() else { return };
                // A search folder has nothing to sync; what refreshing it
                // means is asking the question again.
                if let Some(saved) = self.saved_search(account, &mailbox) {
                    self.search_results.remove(&(account, mailbox.clone()));
                    self.fill_saved_search(account, &mailbox);
                    let _ = saved;
                    return;
                }
                if crate::mail::model::is_search_mailbox(&mailbox) {
                    return;
                }
                self.engine.send(Command::Sync { account, mailbox });
            }

            Action::SearchServer(query) => {
                if let Some((account, mailbox)) = self.open_mailbox.clone() {
                    let scope = self.search_scope;
                    let include_spam_and_trash =
                        self.config.read().unwrap().ui.search_spam_and_trash;
                    self.status = format!(
                        "Searching {} for \u{201c}{query}\u{201d}\u{2026}",
                        scope.label().to_lowercase()
                    );
                    self.searching = true;
                    self.search_generation += 1;
                    // Where to go back to when the results are dismissed.
                    // Not the search folder itself, or clearing would leave
                    // nowhere to return to.
                    if !self.in_search_folder() {
                        self.search_origin = self.open_mailbox.clone();
                    }
                    self.engine.send(Command::Search {
                        account,
                        mailbox,
                        query,
                        scope,
                        generation: self.search_generation,
                        include_spam_and_trash,
                    });
                }
            }
            Action::SaveSearch => {
                let query = self.search.trim().to_string();
                let Some((account, _)) = self.open_mailbox.clone() else { return };
                if query.is_empty() {
                    self.status = "Nothing to save".into();
                    return;
                }
                // Named by the user rather than derived from the query: a
                // query worth keeping is longer than the column it would be
                // drawn in. The query is on the row's tooltip.
                self.folder_edit = Some(FolderEdit::SaveSearch {
                    account,
                    query,
                    scope: self.search_scope,
                    include_spam_and_trash: self.config.read().unwrap().ui.search_spam_and_trash,
                    name: String::new(),
                });
            }

            Action::RenameSearch { account, mailbox } => {
                let Some(name) = crate::mail::model::saved_search_name(&mailbox) else { return };
                self.folder_edit = Some(FolderEdit::RenameSearch {
                    account,
                    mailbox: mailbox.clone(),
                    name: name.to_string(),
                });
            }

            Action::ForgetSearch { account, mailbox } => {
                let Some(name) = crate::mail::model::saved_search_name(&mailbox) else { return };
                {
                    let mut config = self.config.write().unwrap();
                    let Some(account) = config.account_mut(account) else { return };
                    account.saved_searches.retain(|saved| saved.name != name);
                }
                self.save_config();
                self.search_results.remove(&(account, mailbox.clone()));
                // Standing in a folder that no longer exists is no place to
                // be, so leave for whatever the account opens with.
                if self.is_open(account, &mailbox) {
                    let home = self.default_mailbox(account);
                    self.open_mailbox = None;
                    self.open_mailbox(account, home);
                }
            }

            Action::ClearSearch => {
                let was_searching = self.searching;
                self.search.clear();
                self.searching = false;
                // Dismissing the results takes the folder with them, so go
                // back to whatever was being read before the search.
                if let Some((account, mailbox)) = self.search_origin.take()
                    && self.in_search_folder()
                {
                    self.open_mailbox = None; // so `open_mailbox` does not no-op
                    self.open_mailbox(account, mailbox);
                }
                // Only the unsaved results are dismissed. A saved search is
                // a folder the user made, not something the query box can
                // throw away by being emptied.
                self.search_results.retain(|(_, mailbox), _| {
                    crate::mail::model::saved_search_name(mailbox).is_some()
                });
                self.search_target = None;
                // Whatever is still running out there is now answering a
                // question that has been withdrawn. Telling the engine lets
                // it stop rather than finish and be ignored, which matters
                // because it holds the account while it runs.
                self.search_generation += 1;
                if was_searching && let Some((account, _)) = self.open_mailbox.clone() {
                    self.engine.send(Command::CancelSearch {
                        account,
                        generation: self.search_generation,
                    });
                }
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
                        Ok(()) => self.status = format!("Loading remote content from {sender}"),
                        Err(e) => tracing::warn!("could not remember sender: {e}"),
                    }
                }
                self.show_remote_images();
            }
            Action::OpenUrl(url) => self.open_url(&url),
            Action::SaveAttachment(index) => self.save_attachment(index),
            Action::OpenInBrowser => self.open_message_in_browser(),

            Action::MarkFolderRead { account, mailbox } => {
                self.engine.send(Command::MarkAllRead { account, mailbox });
            }
            Action::NewSubfolder { account, parent } => {
                let view = self.accounts.get(&account);
                let delimiter = view
                    .and_then(|view| {
                        // The parent's own delimiter, or any mailbox's: a
                        // server uses one separator throughout.
                        view.mailboxes
                            .iter()
                            .find(|m| m.name == parent)
                            .or_else(|| view.mailboxes.iter().find(|m| m.delimiter.is_some()))
                    })
                    .and_then(|mailbox| mailbox.delimiter.clone());
                self.folder_edit =
                    Some(FolderEdit::New { account, parent, delimiter, name: String::new() });
            }
            Action::RenameFolder { account, mailbox } => {
                let name = mailbox.rsplit(['/', '.']).next().unwrap_or(&mailbox).to_string();
                self.folder_edit = Some(FolderEdit::Rename { account, mailbox, name });
            }
            Action::DeleteFolder { account, mailbox } => {
                self.folder_edit = Some(FolderEdit::Delete { account, mailbox });
            }
            Action::ToggleFolder { account, mailbox } => {
                {
                    let mut config = self.config.write().unwrap();
                    let Some(account) = config.account_mut(account) else { return };
                    let closed = &mut account.collapsed_folders;
                    match closed.iter().position(|name| *name == mailbox) {
                        Some(at) => drop(closed.remove(at)),
                        None => closed.push(mailbox),
                    }
                }
                // A click, not a drag, so there is nothing to debounce.
                self.save_config();
            }
            Action::DropOnFolder { account, mailbox, rows } => {
                self.move_rows(account, rows, mailbox);
            }
            Action::MoveTo => {
                let Some((account, _)) = self.open_mailbox.clone() else { return };
                let rows = self.targets();
                if rows.is_empty() {
                    self.status = "Nothing selected to move".into();
                    return;
                }
                self.move_to = Some(crate::ui::move_to::MoveDialog::new(account, rows));
            }
            Action::ToggleAccount(account) => {
                {
                    let mut config = self.config.write().unwrap();
                    let Some(account) = config.account_mut(account) else { return };
                    account.sidebar_expanded = !account.sidebar_expanded;
                }
                self.save_config();
            }
        }
    }

    fn open_mailbox(&mut self, account: AccountId, mailbox: String) {
        if self.is_open(account, &mailbox) {
            return;
        }
        // Results outlive being navigated away from: that is what makes them
        // a folder rather than a mode. They go when they are dismissed, or
        // when a later search replaces them.
        let to_search = crate::mail::model::is_search_mailbox(&mailbox);

        self.open_mailbox = Some((account, mailbox.clone()));
        if !to_search {
            // The search folder is not drawn from these, and keeping them
            // means the folder they belong to is still there to come back to.
            self.envelopes.clear();
        }
        self.selection.clear();
        self.cursor = None;
        self.anchor = None;
        self.open_message = None;
        self.prefetched.clear();
        self.pending_removal.clear();
        self.textures.clear();
        if !to_search {
            self.engine.send(Command::OpenMailbox { account, mailbox });
            return;
        }
        self.fill_saved_search(account, &mailbox);
    }

    /// Fills a saved search: from the cache at once, from the server after.
    ///
    /// The same shape a folder opens with, for the same reason — something to
    /// read immediately, made right a moment later. The cache holds envelopes
    /// from every mailbox that has been synced, and the query that picks the
    /// results out of it is the query the server will be given, so the two
    /// answers are the same question asked twice. The cached one is narrower:
    /// it can only find what has been cached.
    ///
    /// A folder that already has results keeps them. They came from a server
    /// search this session, and clicking between folders is not a reason to
    /// ask again — `Refresh` is.
    fn fill_saved_search(&mut self, account: AccountId, mailbox: &str) {
        let Some(name) = crate::mail::model::saved_search_name(mailbox) else { return };
        let Some(saved) = self
            .config
            .read()
            .unwrap()
            .accounts
            .iter()
            .find(|a| a.id == account)
            .and_then(|a| a.saved_searches.iter().find(|s| s.name == name))
            .cloned()
        else {
            return;
        };

        let key = (account, mailbox.to_string());
        if self.search_results.contains_key(&key) {
            return;
        }

        if let Ok(query) = crate::mail::Query::parse(&saved.query) {
            let cached: Vec<Envelope> = self
                .store
                .load_account_envelopes(account, CACHED_SEARCH_LIMIT)
                .unwrap_or_default()
                .into_iter()
                .filter(|envelope| query.matches(envelope))
                .collect();
            self.search_results.insert(key, cached);
        }

        self.run_saved_search(account, mailbox, &saved);
    }

    /// Asks the server for a saved search, so what the cache could not know
    /// about arrives too.
    fn run_saved_search(
        &mut self,
        account: AccountId,
        mailbox: &str,
        saved: &crate::config::SavedSearch,
    ) {
        self.searching = true;
        self.search_generation += 1;
        self.search_target = Some((account, mailbox.to_string()));
        self.status = format!("Searching for \u{201c}{}\u{201d}\u{2026}", saved.query);
        self.engine.send(Command::Search {
            account,
            mailbox: self.default_mailbox(account),
            query: saved.query.clone(),
            scope: saved.scope,
            include_spam_and_trash: saved.include_spam_and_trash,
            generation: self.search_generation,
        });
    }

    /// Keeps a search under a name, and goes to the folder it now has.
    fn save_search(&mut self, account: AccountId, search: crate::config::SavedSearch) {
        let mailbox = crate::mail::model::saved_search_mailbox(&search.name);
        {
            let mut config = self.config.write().unwrap();
            let Some(config) = config.account_mut(account) else { return };
            // One name, one search: saving over a name is how a saved search
            // is edited.
            config.saved_searches.retain(|saved| saved.name != search.name);
            config.saved_searches.push(search);
        }
        self.save_config();

        // The results already on screen belong to it now, rather than being
        // run again to be told the same thing.
        if let Some(found) =
            self.search_results.remove(&(account, crate::mail::model::SEARCH_MAILBOX.to_string()))
        {
            self.search_results.insert((account, mailbox.clone()), found);
        }
        self.open_mailbox = None;
        self.open_mailbox(account, mailbox);
        self.search.clear();
    }

    /// The saved search a folder stands for, if it is one.
    fn saved_search(
        &self,
        account: AccountId,
        mailbox: &str,
    ) -> Option<crate::config::SavedSearch> {
        let name = crate::mail::model::saved_search_name(mailbox)?;
        self.config
            .read()
            .unwrap()
            .accounts
            .iter()
            .find(|a| a.id == account)?
            .saved_searches
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    /// The mailbox an account opens with.
    fn default_mailbox(&self, account: AccountId) -> String {
        self.config
            .read()
            .unwrap()
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.default_mailbox.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "INBOX".to_string())
    }

    /// The search folder on screen, if what is on screen is one.
    fn open_search_key(&self) -> Option<(AccountId, String)> {
        let (account, mailbox) = self.open_mailbox.clone()?;
        crate::mail::model::is_search_mailbox(&mailbox).then_some((account, mailbox))
    }

    /// Whether a search folder is what is on screen.
    fn in_search_folder(&self) -> bool {
        self.open_search_key().is_some()
    }

    /// The search folders to draw, with how many unread each is holding.
    ///
    /// Built every frame rather than kept: the unsaved one exists only while
    /// it has something in it, and the saved ones are whatever the
    /// configuration currently says.
    fn search_folders(&self) -> Vec<(AccountId, crate::mail::MailboxInfo)> {
        use crate::mail::MailboxInfo;
        use crate::mail::model::SEARCH_MAILBOX;

        let mut out = Vec::new();
        let unread = |key: &(AccountId, String)| -> u32 {
            self.search_results
                .get(key)
                .map(|rows| rows.iter().filter(|e| e.flags.is_unread()).count() as u32)
                .unwrap_or(0)
        };

        for account in self.config.read().unwrap().accounts.iter().filter(|a| a.enabled) {
            let key = (account.id, SEARCH_MAILBOX.to_string());
            if self.search_results.contains_key(&key) {
                let mut folder = MailboxInfo::search_results();
                folder.unseen = unread(&key);
                out.push((account.id, folder));
            }
            for saved in &account.saved_searches {
                let mut folder = MailboxInfo::saved_search(&saved.name);
                folder.unseen = unread(&(account.id, folder.name.clone()));
                out.push((account.id, folder));
            }
        }
        out
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
        self.engine.send(Command::FetchBody { account, mailbox, uid, served: false });
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
        for results in self.search_results.values_mut() {
            for envelope in results.iter_mut().filter(|e| rows.contains(&e.key())) {
                envelope.flags.set(bit, add);
            }
        }
        if let Some(open) = &mut self.open_message
            && rows.contains(&open.key.row())
        {
            open.envelope.flags.set(bit, add);
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

    /// Moves rows to a folder the user picked.
    fn move_rows(&mut self, account: AccountId, rows: Vec<RowKey>, destination: String) {
        let movable: Vec<RowKey> =
            rows.into_iter().filter(|row| row.mailbox != destination).collect();
        if movable.is_empty() {
            return;
        }

        let moved = movable.len();
        self.remove_rows(&movable);
        for (mailbox, uids) in Self::by_mailbox(&movable) {
            self.engine.send(Command::Move {
                account,
                mailbox,
                uids,
                destination: destination.clone(),
            });
        }

        // Present tense: the rows have gone from the list, but the server
        // has not answered yet. The engine reports the outcome, and puts the
        // rows back if the move failed.
        let shown = crate::mail::model::display_folder(&destination);
        self.status = if moved == 1 {
            format!("Moving to {shown}\u{2026}")
        } else {
            format!("Moving {moved} messages to {shown}\u{2026}")
        };
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
        for results in self.search_results.values_mut() {
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

        // A search folder is a separate list; it needs the rows back too,
        // or they stay missing until the search is run again. Every folder
        // holding them, since more than one can.
        for results in self.search_results.values_mut() {
            results.extend(restored.iter().cloned());
            results.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.uid.cmp(&a.uid)));
            // By row, not by UID: these come from several mailboxes, where a
            // UID on its own names more than one message.
            results.dedup_by_key(|e| e.key());
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
        let Some(account) = self.config.read().unwrap().account(open.key.account).cloned() else {
            return;
        };

        let draft = crate::mail::smtp::reply_draft(&account, &open.envelope, body, all);
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
        // Forward from whichever address the message reached, so a thread
        // stays on one identity.
        let from = self
            .config
            .read()
            .unwrap()
            .account(open.key.account)
            .map(|account| {
                let identities = account.identities();
                open.envelope
                    .to
                    .iter()
                    .chain(open.envelope.cc.iter())
                    .find_map(|address| {
                        identities
                            .iter()
                            .find(|identity| identity.email.eq_ignore_ascii_case(&address.email))
                    })
                    .map(|identity| identity.email.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let draft = crate::mail::smtp::forward_draft(open.key.account, from, &open.envelope, body);
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
            open.prepared = Prepared::from_parts(body.html.as_deref(), body.text.as_deref(), true);
        }
        self.textures.clear();
    }

    /// Re-prepares the open message with remote content blocked again.
    fn hide_remote_images(&mut self) {
        let Some(open) = &mut self.open_message else { return };
        if !open.allow_remote {
            return;
        }
        open.allow_remote = false;
        if let Some(body) = &open.body {
            open.prepared = Prepared::from_parts(body.html.as_deref(), body.text.as_deref(), false);
        }
        self.textures.clear();
        self.remote_images.clear();
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

    /// Writes the open message out as a standalone document and hands it to
    /// the system, which opens it in a browser where the print dialog lives.
    /// Hands the open message to whatever opens HTML, which on every desktop
    /// is a browser.
    ///
    /// For the message the reader cannot do justice to: one built as a page,
    /// where a sanitized block model is a poor likeness of what was sent. The
    /// document is the same one printing uses — self-contained, `cid:` parts
    /// inlined, remote content still blocked if this message is blocking it —
    /// so nothing is disclosed by opening it that the reader had not already
    /// disclosed.
    fn open_message_in_browser(&mut self) {
        let Some(open) = &self.open_message else { return };
        let (Some(body), Some(prepared)) = (&open.body, &open.prepared) else {
            self.status = "Still loading that message".into();
            return;
        };

        let document = crate::html::print::document(&open.envelope, body, prepared);
        match self.write_print_file(&document) {
            Ok(path) => match open::that_detached(&path) {
                Ok(()) => self.status = "Opened in your browser".into(),
                Err(e) => self.status = format!("Could not open a browser: {e}"),
            },
            Err(e) => self.status = format!("Could not prepare the message: {e}"),
        }
    }

    /// Writes a print document somewhere only this user can read it.
    ///
    /// The file holds the full text of a message, so it goes in the
    /// application's own data directory rather than a world-readable temp
    /// directory, and is owner-only. Earlier files are swept as we go: the
    /// browser still needs this one after the call returns, so it cannot be
    /// deleted immediately.
    fn write_print_file(&self, document: &str) -> Result<std::path::PathBuf> {
        use std::io::Write as _;

        let directory = crate::config::data_dir()?.join("print");
        std::fs::create_dir_all(&directory)?;
        restrict(&directory, 0o700);
        sweep_old_print_files(&directory);

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = directory.join(format!("message-{stamp}.html"));

        let mut file = std::fs::File::create(&path)?;
        restrict(&path, 0o600);
        file.write_all(document.as_bytes())?;
        file.sync_all()?;
        Ok(path)
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
        // In the search folder the results are the listing; the query box
        // narrows them further, the same as it narrows any other folder.
        let rows: &[Envelope] = match self.open_search_key() {
            Some(key) => self.search_results.get(&key).map_or(&[][..], Vec::as_slice),
            None => &self.envelopes,
        };

        let typed = self.search.trim();
        if typed.is_empty() {
            return rows.to_vec();
        }

        // A query half-typed is a query that does not parse — `subject:` on
        // the way to `subject:invoice`. Falling back to a plain substring
        // match keeps the list from emptying under the cursor; the error is
        // only worth reporting once Enter asks the server.
        match crate::mail::Query::parse(typed) {
            Ok(query) => rows.iter().filter(|e| query.matches(e)).cloned().collect(),
            Err(_) => {
                let needle = typed.to_ascii_lowercase();
                rows.iter().filter(|e| e.matches(&needle)).cloned().collect()
            }
        }
    }

    /// Queues a notification. Events are handled before the frame has a
    /// `Context` to draw into, so toasts are buffered rather than shown here.
    fn toast(&mut self, title: &str, description: Option<String>, tone: BadgeTone) {
        self.pending_toasts.push(PendingToast { title: title.to_string(), description, tone });
    }
}

/// A notification waiting for the next frame.
struct PendingToast {
    title: String,
    description: Option<String>,
    tone: BadgeTone,
}

/// Tightens permissions where the platform has them.
fn restrict(path: &std::path::Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// Deletes print files left over from previous messages.
///
/// They are not removed as soon as the browser is launched because it has not
/// necessarily read the file by then, so each print cleans up after the last.
fn sweep_old_print_files(directory: &std::path::Path) {
    const KEEP: Duration = Duration::from_secs(60 * 60);

    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|at| at.elapsed().unwrap_or_default() > KEEP)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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

// -- frame ----------------------------------------------------------------

impl eframe::App for RemailApp {
    /// Non-drawing work. Runs before `ui`, and also while the window is
    /// hidden, so background mail activity keeps being processed.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.pump_events();
        self.sync_theme(ctx);
        self.handle_shortcuts(ctx);
        self.tick_mark_read();
        self.flush_layout();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        let mut action = None;

        // The folder list recedes onto the app surface; the messages and
        // reading panes share the card colour, so they read as one sheet of
        // paper split by the panel's separator line.
        // Before anything is laid out: every widget that is not one of the
        // three panes draws in whatever Proportional is when it is asked.
        self.fonts.use_for_interface(&ctx, &self.config.read().unwrap().ui.interface_font.clone());

        let palette = &self.theme.palette;
        // The folder pane can be given its own polarity. Everything it draws
        // is taken from the theme it is handed, so this is the whole of the
        // switch — bar the few text colours it reads off the style instead,
        // which are overridden on its own `Ui` below.
        let folders_theme = self
            .config
            .read()
            .unwrap()
            .ui
            .dark_folders
            .then(|| crate::config::dark_pane(&self.theme))
            .flatten()
            .unwrap_or_else(|| self.theme.clone());
        let folders_palette = &folders_theme.palette;
        let folders_fill = folders_palette.depth_tint(folders_palette.bg, 0.025);
        let messages_fill = palette.card;
        let reading_fill = palette.card;
        let surface =
            |fill: egui::Color32, margin: i8| egui::Frame::new().fill(fill).inner_margin(margin);

        let (folders_width, messages_width) = {
            let config = self.config.read().unwrap();
            (config.ui.folders_width, config.ui.messages_width)
        };

        // Resolve each pane's font once per frame. After the first use of a
        // family this is a map lookup; the expensive install happens inside.
        let (folders_font, messages_font, reading_font) = {
            let ui_config = self.config.read().unwrap().ui.clone();
            let base = ui_config.font_size;
            (
                egui::FontId::new(
                    ui_config.folders.size(base),
                    self.fonts.resolve(&ctx, &ui_config.folders.font),
                ),
                egui::FontId::new(
                    ui_config.messages.size(base),
                    self.fonts.resolve(&ctx, &ui_config.messages.font),
                ),
                egui::FontId::new(
                    ui_config.reading.size(base),
                    self.fonts.resolve(&ctx, &ui_config.reading.font),
                ),
            )
        };

        egui::Panel::top("toolbar").show(ui, |ui| {
            action = action.take().or(self.toolbar(ui));
        });

        // The default panel frame is sized for a toolbar. This one holds a
        // single line of small text, so it gets its own margin; the fill is
        // the style's, so only the height changes.
        let status_frame = egui::Frame::side_top_panel(&ui.style().clone())
            .inner_margin(egui::Margin::symmetric(8, 4));
        egui::Panel::bottom("status").frame(status_frame).show(ui, |ui| {
            self.status_bar(ui);
        });

        let searches = self.search_folders();
        egui::Panel::left("sidebar")
            .default_size(folders_width)
            // Narrow enough to become a strip of icons and initials.
            .size_range(56.0..=460.0)
            .frame(surface(folders_fill, 2))
            .show(ui, |ui| {
                // The three the sidebar takes from the style rather than
                // from the theme it was handed. Set on this pane's own `Ui`,
                // so nothing outside it changes.
                let visuals = ui.visuals_mut();
                visuals.override_text_color = Some(folders_palette.text);
                visuals.weak_text_color = Some(folders_palette.text_muted);
                visuals.widgets.active.fg_stroke.color = folders_palette.text;

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
                        searches: &searches,
                        font: folders_font.clone(),
                        theme: &folders_theme,
                        spring: &mut self.spring,
                    },
                );
                action = action.take().or(found);
            });

        egui::Panel::left("messages")
            .default_size(messages_width)
            .size_range(180.0..=760.0)
            .frame(surface(messages_fill, 0))
            .show(ui, |ui| {
                action =
                    action.take().or(self.message_list(ui, messages_font.clone(), messages_fill));
            });

        egui::CentralPanel::default().frame(surface(reading_fill, 8)).show(ui, |ui| {
            action = action.take().or(self.reader(ui, reading_font.clone()));
        });

        self.remember_panel_sizes(&ctx);
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
    /// Records the panel widths the user has dragged to.
    ///
    /// egui owns the live size, so it is read back rather than tracked. Disk
    /// writes are debounced: dragging a splitter changes the width on every
    /// frame of the drag, and the config is not worth rewriting sixty times a
    /// second.
    fn remember_panel_sizes(&mut self, ctx: &Context) {
        let width_of = |name: &str| {
            egui::containers::panel::PanelState::load(ctx, egui::Id::new(name))
                .map(|state| state.size().x)
        };
        let (Some(folders), Some(messages)) = (width_of("sidebar"), width_of("messages")) else {
            return;
        };

        let mut config = self.config.write().unwrap();
        // Sub-point differences are rounding, not intent.
        let changed = (config.ui.folders_width - folders).abs() > 0.5
            || (config.ui.messages_width - messages).abs() > 0.5;
        if changed {
            config.ui.folders_width = folders;
            config.ui.messages_width = messages;
            drop(config);
            self.layout_dirty_since = Some(Instant::now());
        }
    }

    /// Writes a debounced layout change once the drag has settled.
    fn flush_layout(&mut self) {
        const SETTLE: Duration = Duration::from_millis(800);
        if self.layout_dirty_since.is_some_and(|at| at.elapsed() >= SETTLE) {
            self.layout_dirty_since = None;
            self.save_config();
        }
    }

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
            if let Some(font) = style.text_styles.get_mut(&egui::TextStyle::Body)
                && (font.size - font_size).abs() > f32::EPSILON
            {
                font.size = font_size;
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
                .add(Button::new(glyphs::REFRESH.to_string()).size(ButtonSize::Small).outline())
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
                    Button::new(format!("{} Move", glyphs::FOLDER))
                        .size(ButtonSize::Small)
                        .outline()
                        .enabled(has_target),
                )
                .on_hover_text("Move to a folder (M)")
                .clicked()
            {
                action = Some(Action::MoveTo);
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

            // Only simple widgets go in a right-to-left layout. `Select` and
            // `TextInput` lay out their own internals left-to-right, so they
            // overlap rather than stack when the parent runs the other way.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        Button::new(glyphs::SETTINGS.to_string()).size(ButtonSize::Small).outline(),
                    )
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

                // The rest of the bar fills what is left, laid out normally.
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    action = action.take().or(self.search_bar(ui));
                });
            });
        });
        ui.add_space(4.0);
        action
    }

    /// Scope selector, query box and a button to drop server-side results.
    fn search_bar(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        // One row of a known height, so `Align::Center` has a centreline to
        // work from. Given more room than they need — and the toolbar is
        // taller than any one of them — these widgets do not agree on what
        // to do with it: a `Button` centres itself in whatever it is handed,
        // while `Select` and `TextInput` start at the top. The further apart
        // the toolbar's height and theirs, the further they drift.
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), SEARCH_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| self.search_controls(ui),
        )
        .inner
    }

    /// The controls themselves, laid out by [`Self::search_bar`].
    fn search_controls(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;

        // The scope selector is an egui `ComboBox` underneath, and a
        // ComboBox is never shorter than `interact_size.y`. Saying the
        // number outright is the only height in this bar that does not come
        // out of a font: left to itself the selector is as tall as its own
        // label, which moves with the face that ends up drawing it.
        ui.spacing_mut().interact_size.y = SEARCH_BAR_HEIGHT;

        // Scope applies to the server-side search that Enter runs; the
        // as-you-type filter always works on what is already loaded.
        let mut scope = self.search_scope;
        ui.add(
            elegance::Select::new("search-scope", &mut scope)
                .options(SearchScope::all().map(|s| (s, s.label())))
                .width(132.0),
        )
        .on_hover_text("How far Enter searches");
        if scope != self.search_scope {
            self.search_scope = scope;
            // A narrower or wider scope invalidates what is on screen.
            if self.in_search_folder() && !self.search.trim().is_empty() {
                action = Some(Action::SearchServer(self.search.trim().to_string()));
            }
        }

        // Spam and Trash only mean anything when the search covers the whole
        // account; at a narrower scope the option would be inert.
        if self.search_scope == SearchScope::All {
            let mut include = self.config.read().unwrap().ui.search_spam_and_trash;
            // Filled when it is on, outlined when off: the state has to be
            // readable without hovering for a tooltip.
            let mut button = Button::new(glyphs::TRASH.to_string()).size(ButtonSize::Medium);
            button = if include { button.accent(Accent::Blue) } else { button.outline() };
            let toggle = ui.add(button).on_hover_text(if include {
                "Including Spam and Trash \u{2014} click to exclude them"
            } else {
                "Excluding Spam and Trash \u{2014} click to include them"
            });
            if toggle.clicked() {
                include = !include;
                self.config.write().unwrap().ui.search_spam_and_trash = include;
                self.save_config();
                if self.in_search_folder() && !self.search.trim().is_empty() {
                    action = Some(Action::SearchServer(self.search.trim().to_string()));
                }
            }
        }

        // Anything to clear: a query being typed, results on screen, or both.
        // A filter that has only run locally is still something the user has
        // to undo, and the only way to undo it was to select the text and
        // delete it — the button appeared once the search had reached the
        // server and not before.
        let clearable = !self.search.is_empty() || self.in_search_folder();
        // Reserved whether or not the button is there, so the field does not
        // jump a button's width narrower on the first keystroke and back on
        // the last. Take the space that is actually left rather than a fixed
        // width, which is what overflowed into the scope selector before.
        let clear_width = 34.0;
        let width = (ui.available_width() - clear_width - 8.0).clamp(90.0, 320.0);

        let search = ui
            .add(
                TextInput::new(&mut self.search)
                    .hint(match self.search_scope {
                        SearchScope::Folder => "Search this folder",
                        SearchScope::Subtree => "Search with subfolders",
                        SearchScope::All => "Search all folders",
                    })
                    .desired_width(width),
            )
            .on_hover_text(SEARCH_SYNTAX);
        // Right-click rather than another control: the bar is four things
        // wide already, and nothing else here has a context menu to compete
        // with — neither egui's text field nor elegance's registers one.
        let menu = elegance::ContextMenu::new("search-menu").show(&search, |ui| {
            let typed = !self.search.trim().is_empty();
            let mut chosen = None;
            if ui.add_enabled(typed, elegance::MenuItem::new("Save this search\u{2026}")).clicked()
            {
                chosen = Some(SearchMenu::Save);
            }
            if ui.add_enabled(typed, elegance::MenuItem::new("Copy as command")).clicked() {
                chosen = Some(SearchMenu::Copy);
            }
            chosen
        });
        if menu == Some(Some(SearchMenu::Save)) {
            action = Some(Action::SaveSearch);
        }
        if menu == Some(Some(SearchMenu::Copy)) {
            let query = self.search.trim().to_string();
            let (account, mailbox) = self.open_mailbox.clone().unwrap_or_default();
            // Named only when there is more than one to choose between; the
            // tool takes the first enabled account otherwise, which is this
            // one.
            let enabled = self.config.read().unwrap().accounts.iter().filter(|a| a.enabled).count();
            let command = search_command(
                &query,
                &mailbox,
                (enabled > 1).then_some(account),
                self.search_scope,
                self.config.read().unwrap().ui.search_spam_and_trash,
                self.in_search_folder(),
            );
            ui.ctx().copy_text(command);
            // Named rather than quoted: the command is ninety characters of
            // shell and the status bar is one line beside the message counts.
            // It is on the clipboard, which is where it was asked for.
            self.status = "Copied the command for this search".into();
        }

        // Escape clears from inside the field. The global shortcut cannot:
        // it stands down whenever a text field holds the keyboard, which is
        // exactly when there is a search to abandon. Focus goes back to the
        // list, since the point of the key is to leave the field.
        if search.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            search.surrender_focus();
            action = Some(Action::ClearSearch);
        }

        // Enter escalates from the local filter to a server search, which
        // reaches messages that are not cached locally.
        if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let query = self.search.trim().to_string();
            if !query.is_empty() {
                // Checked here so a typo is answered immediately instead of
                // after a round trip to every mailbox in scope.
                match crate::mail::Query::parse(&query) {
                    Ok(_) => action = Some(Action::SearchServer(query)),
                    Err(e) => self.status = format!("Search: {e}"),
                }
            }
        }

        // The spinner stands in the clear button's place rather than beside
        // it: the slot's width is reserved either way, so the swap costs no
        // layout, and this is where the eye already is after pressing Enter.
        if self.searching {
            ui.add(elegance::Spinner::new().size(SEARCH_BAR_HEIGHT * 0.6).accent(Accent::Blue))
                .on_hover_text(&self.status);
        } else if clearable
            && ui
                .add(Button::new(glyphs::X.to_string()).size(ButtonSize::Medium).outline())
                .on_hover_text(if self.in_search_folder() {
                    "Clear search results (Esc)"
                } else {
                    "Clear search (Esc)"
                })
                .clicked()
        {
            action = Some(Action::ClearSearch);
        }

        action
    }

    fn message_list(
        &mut self,
        ui: &mut egui::Ui,
        font: egui::FontId,
        surface: egui::Color32,
    ) -> Option<Action> {
        let visible = self.visible();
        let compact = self.config.read().unwrap().ui.compact_list;
        let account = self.open_mailbox.as_ref().map(|(account, _)| *account).unwrap_or_default();

        let empty_message = if self.open_mailbox.is_none() {
            "Select a mailbox"
        } else if self.searching {
            "Searching\u{2026}"
        } else if self.in_search_folder() {
            "No messages matched"
        } else if !self.search.trim().is_empty() {
            // `body:` and `text:` reach the whole message on the server and
            // only the cached preview here, so an empty list is more likely
            // to mean "not cached" than "not there".
            match crate::mail::Query::parse(self.search.trim()) {
                Ok(query) if query.needs_the_server() => {
                    "Nothing in the cache matched \u{2014} press Enter to search the server"
                }
                _ => "No cached messages matched",
            }
        } else {
            "No messages"
        };

        // Sent and Drafts show who a message went to rather than who it came
        // from, which in those folders is always the account itself.
        let outgoing: std::collections::BTreeSet<String> = self
            .accounts
            .get(&account)
            .map(|view| {
                view.mailboxes
                    .iter()
                    .filter(|mailbox| {
                        matches!(mailbox.special, SpecialUse::Sent | SpecialUse::Drafts)
                    })
                    .map(|mailbox| mailbox.name.clone())
                    .collect()
            })
            .unwrap_or_default();

        let scroll_to_cursor = std::mem::take(&mut self.scroll_to_cursor);
        let output = message_list::show(
            ui,
            message_list::ListInput {
                account,
                envelopes: &visible,
                cursor: self.cursor.clone(),
                selection: &self.selection,
                compact,
                // Always in the search folder: the whole point of a result
                // is that it came from somewhere you were not looking.
                show_folder: self.in_search_folder(),
                outgoing: &outgoing,
                theme: &self.theme,
                font,
                surface,
                scroll_to_cursor,
                empty_message,
            },
        );

        self.prefetch(&visible, output.visible);
        // The focus change comes first, so a menu choice on an unselected row
        // acts on that row.
        if let Some(action) = output.action {
            self.apply(action);
        }
        output.pending
    }

    /// Asks the engine to cache bodies around the viewport, so scrolling then
    /// clicking rarely waits on the network.
    fn prefetch(&mut self, visible: &[Envelope], range: std::ops::Range<usize>) {
        let Some((account, open)) = self.open_mailbox.clone() else { return };
        if visible.is_empty() {
            return;
        }

        let start = range.start.saturating_sub(PREFETCH_MARGIN);
        let end = (range.end + PREFETCH_MARGIN).min(visible.len());

        // Grouped by the mailbox each row actually lives in, not by the one
        // being looked at. In the search folder those differ — the rows came
        // from wherever the search found them — and the folder itself is on
        // no server to fetch from.
        let mut wanted: HashMap<String, Vec<u32>> = HashMap::new();
        for envelope in &visible[start..end] {
            if !self.prefetched.insert(envelope.key()) {
                continue;
            }
            let mailbox =
                if envelope.mailbox.is_empty() { open.clone() } else { envelope.mailbox.clone() };
            wanted.entry(mailbox).or_default().push(envelope.uid);
        }

        for (mailbox, uids) in wanted {
            if mailbox == crate::mail::model::SEARCH_MAILBOX {
                continue;
            }
            self.engine.send(Command::Prefetch { account, mailbox, uids });
        }
    }

    fn reader(&mut self, ui: &mut egui::Ui, font: egui::FontId) -> Option<Action> {
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
                    font: font.clone(),
                    show_source: &mut false,
                    loading: false,
                    theme: &self.theme,
                },
            );
        };

        reader::show(
            ui,
            reader::ReaderInput {
                envelope: Some(&open.envelope),
                body: open.body.as_deref(),
                prepared: open.prepared.as_ref(),
                textures: &mut self.textures,
                remote: &mut self.remote_images,
                allow_remote: open.allow_remote,
                font,
                show_source: &mut open.show_source,
                loading: open.body.is_none(),
                theme: &self.theme,
            },
        )
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
            } else if input.key_pressed(Key::M) {
                action = Some(Nav::Act(Action::MoveTo));
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
        // `interact_size.y` is the theme's button height, and a horizontal
        // row is at least that tall whether or not anything in it can be
        // clicked. Nothing here can, so the row is sized by its text.
        ui.spacing_mut().interact_size.y = 0.0;

        ui.horizontal(|ui| {
            // Count what is actually on screen: while a search is showing,
            // the mailbox's own total is not what the list is displaying.
            let showing = self.visible();
            let in_search = self.in_search_folder();
            let rows = &showing;
            let unread = showing.iter().filter(|e| e.flags.is_unread()).count();
            let noun = if in_search { "results" } else { "messages" };
            let counts = if unread > 0 {
                format!("{} {noun}, {unread} unread", rows.len())
            } else {
                format!("{} {noun}", rows.len())
            };
            ui.label(self.theme.faint_text(counts));

            if !self.selection.is_empty() {
                ui.separator();
                ui.label(self.theme.muted_text(format!("{} selected", self.selection.len())));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.status.is_empty() {
                    // Truncated, not left to run: this is a right-aligned
                    // label on a row that does not wrap, so a status longer
                    // than the space left would be drawn over the counts.
                    // The whole of it is on the tooltip.
                    ui.add(egui::Label::new(self.theme.muted_text(&self.status)).truncate())
                        .on_hover_text(&self.status);
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &Context) {
        // Compose.
        if let Some(compose) = &mut self.compose {
            let account_id = compose.draft.account;
            let config = self.config.read().unwrap().clone();
            let account = config.account(account_id);

            // The store is the address book: everything the account has seen.
            let store = self.store.clone();
            let lookup = move |fragment: &str| {
                store.suggest_contacts(account_id, fragment, 8).unwrap_or_default()
            };

            match crate::ui::compose::show(ctx, compose, account, &self.theme, &lookup) {
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
                        Toast::new("Draft discarded").tone(BadgeTone::Warning).show(ctx);
                    }
                }
                Some(ComposeAction::AttachFile) => self.attach_file(),
                Some(ComposeAction::RemoveAttachment(index)) => {
                    // The index names a row drawn this frame; guard against a
                    // draft that changed underneath rather than panicking.
                    let attachments = &mut compose.draft.attachments;
                    if index < attachments.len() {
                        attachments.remove(index);
                    }
                }
                None => {}
            }
        }

        // Accounts.
        if let Some(dialog) = &mut self.accounts_dialog {
            let config = self.config.read().unwrap().clone();
            let found = crate::ui::accounts::show(
                ctx,
                dialog,
                &config,
                self.keyring_available,
                &self.theme,
            );

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

        // Folder create / rename / delete.
        if let Some(edit) = &mut self.folder_edit {
            let (done, open) = crate::ui::accounts::folder_dialog(ctx, edit, &self.theme);
            if let Some(action) = done {
                match action {
                    FolderAction::Create { account, name } => {
                        self.engine.send(Command::CreateMailbox { account, name });
                    }
                    FolderAction::SaveSearch { account, search } => {
                        self.save_search(account, search);
                    }
                    FolderAction::RenameSearch { account, mailbox, to } => {
                        let Some(from) = crate::mail::model::saved_search_name(&mailbox) else {
                            return;
                        };
                        {
                            let mut config = self.config.write().unwrap();
                            let Some(account) = config.account_mut(account) else { return };
                            if let Some(saved) =
                                account.saved_searches.iter_mut().find(|s| s.name == from)
                            {
                                saved.name = to.clone();
                            }
                        }
                        self.save_config();

                        // The folder is named after the search, so renaming
                        // one moves the other.
                        let renamed = crate::mail::model::saved_search_mailbox(&to);
                        if let Some(found) = self.search_results.remove(&(account, mailbox.clone()))
                        {
                            self.search_results.insert((account, renamed.clone()), found);
                        }
                        if self.is_open(account, &mailbox) {
                            self.open_mailbox = Some((account, renamed));
                        }
                    }
                    FolderAction::Rename { account, from, to } => {
                        // The open mailbox is about to change name under us.
                        if self
                            .open_mailbox
                            .as_ref()
                            .is_some_and(|(a, m)| *a == account && *m == from)
                        {
                            self.open_mailbox = None;
                            self.envelopes.clear();
                            self.open_message = None;
                        }
                        self.engine.send(Command::RenameMailbox { account, from, to });
                    }
                    FolderAction::Delete { account, mailbox } => {
                        if self
                            .open_mailbox
                            .as_ref()
                            .is_some_and(|(a, m)| *a == account && *m == mailbox)
                        {
                            self.open_mailbox = None;
                            self.envelopes.clear();
                            self.open_message = None;
                        }
                        self.engine.send(Command::DeleteMailbox { account, mailbox });
                    }
                }
                self.folder_edit = None;
            } else if !open {
                self.folder_edit = None;
            }
        }

        // Move to folder.
        if let Some(dialog) = &mut self.move_to {
            let mailboxes = self
                .accounts
                .get(&dialog.account)
                .map(|view| view.mailboxes.clone())
                .unwrap_or_default();
            let (chosen, closed) = crate::ui::move_to::show(ctx, dialog, &mailboxes, &self.theme);

            if let Some(destination) = chosen {
                let account = dialog.account;
                let rows = std::mem::take(&mut dialog.rows);
                self.move_rows(account, rows, destination);
                self.move_to = None;
            } else if closed {
                self.move_to = None;
            }
        }

        // Settings.
        if self.settings_open {
            let mut open = self.settings_open;
            let changed = {
                let mut config = self.config.write().unwrap();
                crate::ui::accounts::settings(
                    ctx,
                    &mut open,
                    &mut config,
                    crate::ui::accounts::SettingsInput {
                        trusted_senders: self.trusted_senders,
                        families: self.fonts.families(),
                        picker: &mut self.font_picker,
                        theme: &self.theme,
                    },
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
                        Some(existing) => *existing = *account,
                        None => config.accounts.push(*account),
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
        config.accounts.iter().filter_map(|a| self.store.remote_sender_count(a.id).ok()).sum()
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

#[cfg(test)]
mod tests {
    /// The command has to run the search that is on screen, not one like it.
    /// A command that quietly looks somewhere else is worse than none.
    #[test]
    fn the_copied_command_names_everything_that_is_not_a_default() {
        let plain = search_command("from:dupr", "INBOX", None, SearchScope::Folder, false, false);
        assert_eq!(plain, "remail-cli search --mailbox 'INBOX' -- 'from:dupr'");

        // The mailbox is always named: the tool defaults to the account's,
        // which is not necessarily the one being looked at.
        let elsewhere =
            search_command("x", "[Gmail]/All Mail", None, SearchScope::Folder, false, false);
        assert!(elsewhere.contains("--mailbox '[Gmail]/All Mail'"), "{elsewhere}");

        let wide = search_command("x", "INBOX", Some(2), SearchScope::All, true, true);
        assert_eq!(
            wide,
            "remail-cli search --account 2 --mailbox 'INBOX' --server --scope all \
             --spam-and-trash -- 'x'"
        );
    }

    #[test]
    fn the_copied_command_leaves_out_what_the_tool_already_does() {
        let folder = search_command("x", "INBOX", None, SearchScope::Folder, false, false);
        assert!(!folder.contains("--scope"), "the default scope was spelled out: {folder}");
        assert!(!folder.contains("--server"), "a cache search asked for the server: {folder}");
        assert!(!folder.contains("--account"), "the only account was named: {folder}");

        // Spam and Trash only mean anything at the widest scope, which is the
        // only place the interface offers the choice.
        let narrow = search_command("x", "INBOX", None, SearchScope::All, true, false);
        assert!(!narrow.contains("--spam-and-trash"), "{narrow}");

        // Reach is the server's business. A cache search reads the mailbox
        // it is given, so a scope on it would promise something else.
        let cached = search_command("x", "INBOX", None, SearchScope::All, true, false);
        assert!(!cached.contains("--scope"), "a cache search claimed a reach it has not: {cached}");
        let asked = search_command("x", "INBOX", None, SearchScope::All, true, true);
        assert!(asked.contains("--scope all"), "{asked}");
    }

    /// The query language negates with a leading `-`, and a subject can
    /// contain anything at all. Both have to survive the shell.
    #[test]
    fn the_copied_command_survives_being_run() {
        let negated = search_command("-is:read", "INBOX", None, SearchScope::Folder, false, false);
        assert!(negated.ends_with("-- '-is:read'"), "{negated}");

        let quoted = search_command(
            "subject:\"it's here\"",
            "INBOX",
            None,
            SearchScope::Folder,
            false,
            false,
        );
        assert!(quoted.ends_with(r#"-- 'subject:"it'\''s here"'"#), "{quoted}");
    }

    #[test]
    fn shell_quoting_closes_and_reopens_around_a_quote() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    /// An application with nothing configured, for exercising the state the
    /// interface keeps rather than anything it draws or fetches.
    /// One account, nothing it can connect to. Enough for the state the
    /// interface keeps, which is what these exercise.
    fn app() -> RemailApp {
        let ctx = Context::default();
        let store = crate::mail::Store::open_memory().expect("in-memory store");

        let mut config = Config::default();
        config.accounts.push(crate::config::AccountConfig::imap(1, "me@example.com"));

        let mut app = RemailApp::new(&ctx, config, store).expect("app");
        app.open_mailbox = Some((1, "INBOX".to_string()));
        app
    }

    /// Results as the engine reports them: carrying the mailbox the search
    /// was started from, which is what the reader checks it is still in.
    fn results_from(mailbox: &str, generation: u64) -> Event {
        Event::SearchResults {
            account: 1,
            mailbox: mailbox.to_string(),
            // A result the query that found it would also match, since the
            // box it was typed into goes on narrowing the folder.
            envelopes: vec![Envelope {
                uid: 7,
                mailbox: "[Gmail]/All Mail".to_string(),
                subject: "Welcome to DUPR".to_string(),
                from: vec![crate::mail::Addr {
                    name: "DUPR".to_string(),
                    email: "noreply@mydupr.com".to_string(),
                }],
                ..Default::default()
            }],
            generation,
        }
    }

    fn results(generation: u64) -> Event {
        results_from("INBOX", generation)
    }

    fn search_mailbox() -> String {
        crate::mail::model::SEARCH_MAILBOX.to_string()
    }

    /// Results are a place, not a mode. Arriving puts the reader in that
    /// place; they are not folded into whatever folder was already open.
    #[test]
    fn results_open_a_folder_of_their_own() {
        let mut app = app();
        assert_eq!(app.open_mailbox, Some((1, "INBOX".to_string())));

        app.apply(Action::SearchServer("from:dupr".into()));
        // Still in the inbox while the search is out: there is nothing in
        // the results folder yet to look at.
        assert_eq!(app.open_mailbox, Some((1, "INBOX".to_string())));

        app.handle_event(results(app.search_generation));
        assert_eq!(app.open_mailbox, Some((1, search_mailbox())), "the results were not opened");
        assert_eq!(app.visible().len(), 1);
    }

    /// The point of it being a folder: leaving does not destroy it.
    #[test]
    fn results_survive_opening_another_folder() {
        let mut app = app();
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));

        app.apply(Action::OpenMailbox { account: 1, mailbox: "Archery".into() });
        assert_eq!(app.open_mailbox, Some((1, "Archery".to_string())));
        assert!(!app.search_results.is_empty(), "the results went with the folder change");

        // And the folder is still there to go back to.
        app.apply(Action::OpenMailbox { account: 1, mailbox: search_mailbox() });
        assert_eq!(app.visible().len(), 1, "the results did not come back");
    }

    fn saved(app: &mut RemailApp, name: &str, query: &str) -> String {
        {
            let mut config = app.config.write().unwrap();
            let account = config.account_mut(1).expect("the test account");
            account.saved_searches.push(crate::config::SavedSearch {
                name: name.to_string(),
                query: query.to_string(),
                ..Default::default()
            });
        }
        crate::mail::model::saved_search_mailbox(name)
    }

    /// A saved search is a folder like the unsaved one, and both are full at
    /// once: opening one does not empty the other.
    #[test]
    fn a_saved_search_and_the_unsaved_results_are_both_kept() {
        let mut app = app();
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));
        assert_eq!(app.visible().len(), 1);

        let folder = saved(&mut app, "Pickleball", "subject:pickleball");
        app.apply(Action::OpenMailbox { account: 1, mailbox: folder });
        // Nothing cached to find, and no account to ask, so it is empty —
        // but the unsaved results are still where they were.
        assert!(app.visible().is_empty());

        app.apply(Action::OpenMailbox {
            account: 1,
            mailbox: crate::mail::model::SEARCH_MAILBOX.to_string(),
        });
        assert_eq!(app.visible().len(), 1, "the unsaved results were emptied by the saved one");
    }

    /// Saving asks for a name, carrying the query that is being saved.
    #[test]
    fn saving_asks_for_a_name_and_keeps_the_query() {
        let mut app = app();
        app.search = "from:dupr".into();
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));

        app.apply(Action::SaveSearch);
        let Some(crate::ui::accounts::FolderEdit::SaveSearch { query, scope, .. }) =
            &app.folder_edit
        else {
            panic!("saving did not ask for a name");
        };
        assert_eq!(query, "from:dupr", "the query being saved is not the one that was run");
        assert_eq!(*scope, app.search_scope, "the scope it was run at was not kept with it");
    }

    /// Naming it takes the results already on screen with it, rather than
    /// running the same search again to be told the same thing.
    #[test]
    fn saving_carries_the_results_into_the_new_folder() {
        let mut app = app();
        app.search = "from:dupr".into();
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));
        assert_eq!(app.visible().len(), 1);

        app.save_search(
            1,
            crate::config::SavedSearch {
                name: "Pickleball".into(),
                query: "from:dupr".into(),
                ..Default::default()
            },
        );

        let folder = crate::mail::model::saved_search_mailbox("Pickleball");
        assert_eq!(app.open_mailbox, Some((1, folder)), "saving did not open what it made");
        assert_eq!(app.visible().len(), 1, "the results were not carried into the folder");
        assert!(app.search.is_empty(), "the query box still holds what is now a folder");
    }

    /// A saved search is answered from the cache the moment it is opened, so
    /// there is something to read while the server is being asked. The rows
    /// come from whatever mailboxes hold them, which is what makes it a
    /// search rather than a folder.
    #[test]
    fn opening_a_saved_search_fills_it_from_the_cache() {
        let mut app = app();
        let dupr = |uid: u32, mailbox: &str, subject: &str, email: &str| Envelope {
            uid,
            mailbox: mailbox.to_string(),
            subject: subject.to_string(),
            from: vec![crate::mail::Addr { name: String::new(), email: email.to_string() }],
            date: uid as i64,
            ..Default::default()
        };

        app.store
            .save_envelopes(
                1,
                "INBOX",
                &[dupr(1, "INBOX", "Welcome to DUPR", "noreply@mydupr.com")],
            )
            .unwrap();
        app.store
            .save_envelopes(
                1,
                "Archery",
                &[
                    dupr(2, "Archery", "Shoes for pickleball", "info@pb.dupr.com"),
                    dupr(3, "Archery", "Nothing to do with it", "someone@example.com"),
                ],
            )
            .unwrap();

        let folder = saved(&mut app, "DUPR", "from:dupr");
        app.apply(Action::OpenMailbox { account: 1, mailbox: folder });

        let found = app.visible();
        assert_eq!(found.len(), 2, "the cache was not searched: {found:?}");
        assert!(
            found.iter().any(|e| e.mailbox == "INBOX")
                && found.iter().any(|e| e.mailbox == "Archery"),
            "the results came from one mailbox rather than from wherever they are"
        );
        assert!(
            found.iter().all(|e| e.from[0].email.contains("dupr")),
            "the query did not decide what came back"
        );

        // And it asks the server as well, rather than settling for what
        // happens to be cached.
        assert!(app.searching, "nothing was asked of the server");
    }

    /// Clearing the query box dismisses the unsaved results. A saved folder
    /// is not something the box can throw away by being emptied.
    #[test]
    fn clearing_does_not_forget_a_saved_search() {
        let mut app = app();
        let folder = saved(&mut app, "Pickleball", "subject:pickleball");
        app.search_results.insert(
            (1, folder.clone()),
            vec![Envelope { uid: 3, mailbox: "INBOX".into(), ..Default::default() }],
        );
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));

        app.apply(Action::ClearSearch);
        assert!(
            app.search_results.contains_key(&(1, folder)),
            "clearing the box emptied a folder it was not typed into"
        );
    }

    /// Forgetting one takes its results and leaves the folder, which is no
    /// longer anywhere to be.
    #[test]
    fn forgetting_a_saved_search_leaves_its_folder() {
        let mut app = app();
        let folder = saved(&mut app, "Pickleball", "subject:pickleball");
        app.search_results.insert((1, folder.clone()), vec![Envelope::default()]);
        app.apply(Action::OpenMailbox { account: 1, mailbox: folder.clone() });

        app.apply(Action::ForgetSearch { account: 1, mailbox: folder.clone() });
        assert!(!app.search_results.contains_key(&(1, folder)), "its results outlived it");
        assert_eq!(app.open_mailbox, Some((1, "INBOX".to_string())), "left standing in nowhere");

        let config = app.config.read().unwrap();
        assert!(config.account(1).unwrap().saved_searches.is_empty(), "it is still saved");
    }

    /// The sidebar draws the unsaved results only while there are some, and
    /// every saved search whether or not it has been opened.
    #[test]
    fn the_sidebar_lists_the_searches_there_are() {
        let mut app = app();
        assert!(app.search_folders().is_empty(), "a folder with nothing in it was drawn");

        saved(&mut app, "Pickleball", "subject:pickleball");
        let names: Vec<String> =
            app.search_folders().iter().map(|(_, m)| m.display_name().to_string()).collect();
        assert_eq!(names, vec!["Pickleball"], "a saved search is drawn before it is opened");

        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));
        let names: Vec<String> =
            app.search_folders().iter().map(|(_, m)| m.display_name().to_string()).collect();
        assert_eq!(
            names,
            vec!["Search results", "Pickleball"],
            "the unsaved results come first, being the newest thing asked for"
        );
    }

    /// Reading something while a search runs is not withdrawing it. The
    /// results used to be thrown away for it — the folder they were meant to
    /// replace was no longer open, so nothing took them.
    #[test]
    fn results_are_kept_when_the_reader_has_moved_on() {
        let mut app = app();
        app.apply(Action::SearchServer("from:dupr".into()));

        // Off to read something else while it runs.
        app.apply(Action::OpenMailbox { account: 1, mailbox: "Archery".into() });
        app.handle_event(results_from("INBOX", app.search_generation));

        assert!(!app.search_results.is_empty(), "the results were dropped for having moved");
        assert!(!app.searching, "the search never finished");

        // Left where they chose to be, rather than taken somewhere.
        assert_eq!(app.open_mailbox, Some((1, "Archery".to_string())), "moved unasked");

        // And the folder is there when they want it.
        app.apply(Action::OpenMailbox { account: 1, mailbox: search_mailbox() });
        assert_eq!(app.visible().len(), 1);
    }

    /// Already in the folder from an earlier search, a new one lands there
    /// without having to be opened again.
    #[test]
    fn a_second_search_lands_in_the_folder_already_open() {
        let mut app = app();
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));
        assert_eq!(app.open_mailbox, Some((1, search_mailbox())));

        app.apply(Action::SearchServer("subject:pickleball".into()));
        app.handle_event(results_from("INBOX", app.search_generation));
        assert_eq!(app.open_mailbox, Some((1, search_mailbox())), "left the folder it was in");
    }

    /// Dismissing them takes the folder away, so it has to put the reader
    /// back where the search was started from.
    #[test]
    fn clearing_the_search_goes_back_where_it_started() {
        let mut app = app();
        app.apply(Action::OpenMailbox { account: 1, mailbox: "Keowee".into() });

        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results_from("Keowee", app.search_generation));
        assert_eq!(app.open_mailbox, Some((1, search_mailbox())));

        app.apply(Action::ClearSearch);
        assert_eq!(app.open_mailbox, Some((1, "Keowee".to_string())), "left nowhere to be");
        assert!(app.search_results.is_empty(), "the folder outlived being dismissed");
    }

    /// Dismissed from somewhere else, there is nothing to go back from.
    #[test]
    fn clearing_from_another_folder_stays_put() {
        let mut app = app();
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));
        app.apply(Action::OpenMailbox { account: 1, mailbox: "Archery".into() });

        app.apply(Action::ClearSearch);
        assert_eq!(app.open_mailbox, Some((1, "Archery".to_string())), "moved unasked");
        assert!(app.search_results.is_empty());
    }

    /// A query typed while the results are open narrows them, rather than
    /// reaching past them into the folder underneath.
    #[test]
    fn typing_in_the_search_folder_narrows_the_results() {
        let mut app = app();
        app.envelopes =
            vec![Envelope { uid: 99, subject: "In the inbox".into(), ..Default::default() }];
        app.apply(Action::SearchServer("from:dupr".into()));
        app.handle_event(results(app.search_generation));

        app.search = "subject:nothing-matches-this".into();
        assert!(app.visible().is_empty(), "the filter found the folder underneath");
    }

    /// A search cannot be stopped once it is running. Having given up on one,
    /// the results must not arrive later and replace what is on screen.
    #[test]
    fn results_from_an_abandoned_search_are_ignored() {
        let mut app = app();

        app.apply(Action::SearchServer("pickleball".into()));
        let abandoned = app.search_generation;
        assert!(app.searching, "the search did not start");

        // Given up on: the box is cleared and the spinner stops.
        app.apply(Action::ClearSearch);
        assert!(!app.searching);

        app.handle_event(results(abandoned));
        assert!(
            app.search_results.is_empty(),
            "a search the user cleared came back and filled the list anyway"
        );
        assert!(!app.searching, "and restarted the spinner");
    }

    /// The same, for a search replaced by a newer one rather than cleared.
    /// The older answer usually arrives second, having had further to go.
    #[test]
    fn results_from_a_superseded_search_are_ignored() {
        let mut app = app();

        app.apply(Action::SearchServer("pickleball".into()));
        let first = app.search_generation;
        app.apply(Action::SearchServer("from:dupr".into()));
        let second = app.search_generation;
        assert_ne!(first, second, "the second search reused the first one's identity");

        app.handle_event(results(first));
        assert!(app.search_results.is_empty(), "the abandoned search answered for the current one");
        assert!(app.searching, "and stopped the spinner while the current one was still out");

        app.handle_event(results(second));
        assert_eq!(app.visible().len(), 1, "the current search's own results were dropped");
        assert!(!app.searching, "the spinner ran on past the results");
    }

    /// The scope selector, the query box and the buttons beside them are one
    /// row of controls and have to read as one: the same height, on one
    /// centreline. Nothing in their construction enforces it — each is sized
    /// by its own padding and typography — so it is asserted here, where a
    /// change to any of the three sizes will fail rather than quietly go
    /// crooked.
    #[test]
    fn the_search_bar_controls_are_one_height_on_one_centreline() {
        use crate::mail::SearchScope;

        let theme = crate::config::ThemeChoice::Outlook.theme();
        // A toolbar far taller than the controls, which is the case that
        // pulled them apart: a Button centres itself in whatever room it is
        // given, while Select and TextInput start at the top.
        let rects = crate::ui::raster::measure(&theme, egui::vec2(560.0, 96.0), |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), SEARCH_BAR_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().interact_size.y = SEARCH_BAR_HEIGHT;
                        let mut scope = SearchScope::All;
                        let select = ui.add(
                            elegance::Select::new("scope", &mut scope)
                                .options(SearchScope::all().map(|s| (s, s.label())))
                                .width(132.0),
                        );
                        let toggle = ui.add(
                            Button::new(glyphs::TRASH.to_string())
                                .size(ButtonSize::Medium)
                                .outline(),
                        );
                        let mut text = "pickleball".to_string();
                        let input =
                            ui.add(elegance::TextInput::new(&mut text).desired_width(180.0));
                        let clear = ui.add(
                            Button::new(glyphs::X.to_string()).size(ButtonSize::Medium).outline(),
                        );
                        // The spinner stands in the clear button's place
                        // while a search is out. It is deliberately smaller
                        // than the controls — it has no frame to match — but
                        // it sits on their line.
                        let spinner = ui.add(
                            elegance::Spinner::new()
                                .size(SEARCH_BAR_HEIGHT * 0.6)
                                .accent(Accent::Blue),
                        );
                        [
                            ("scope", select.rect),
                            ("spam toggle", toggle.rect),
                            ("query box", input.rect),
                            ("clear", clear.rect),
                            ("spinner", spinner.rect),
                        ]
                    },
                )
                .inner
            })
            .inner
        });

        for (name, rect) in rects {
            if name == "spinner" {
                continue; // Frameless, so its own size rather than the bar's.
            }
            assert!(
                (rect.height() - SEARCH_BAR_HEIGHT).abs() <= 1.5,
                "{name} is {:.2} tall, the bar is {SEARCH_BAR_HEIGHT}",
                rect.height()
            );
        }

        // The selector is the one that is actually told, so it is held to
        // the number rather than to the tolerance the other two need.
        let scope = rects[0].1;
        assert!(
            (scope.height() - SEARCH_BAR_HEIGHT).abs() < 0.01,
            "the scope selector is {:.2}, not {SEARCH_BAR_HEIGHT}: interact_size is what \
             holds it there, and nothing else in its construction will",
            scope.height()
        );

        let centres: Vec<f32> = rects.iter().map(|(_, rect)| rect.center().y).collect();
        let highest = centres.iter().copied().fold(f32::MAX, f32::min);
        let lowest = centres.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            lowest - highest <= 1.0,
            "centres are {:.2} apart: {:?}",
            lowest - highest,
            rects.map(|(name, rect)| (name, rect.center().y))
        );
    }

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
        let rows =
            vec![RowKey::new("INBOX", 1), RowKey::new("Archive", 5), RowKey::new("INBOX", 2)];
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
