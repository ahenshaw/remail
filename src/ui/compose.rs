//! The compose window.
//!
//! A draft lives in application state, not in this module, so closing and
//! reopening the window (or switching messages behind it) never loses typing.

use egui::{Context, RichText, Ui};
use elegance::{Accent, Button, ButtonSize, Card, Select, TextArea, TextInput, Theme, glyphs};

use super::format_size;
use crate::config::AccountConfig;
use crate::mail::{Addr, Draft};

/// What the compose window is asking the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeAction {
    Send,
    Close,
    /// Open a file picker and append the result to the draft.
    AttachFile,
    /// Drop the attachment at this index.
    RemoveAttachment(usize),
}

/// Which recipient field completion is happening in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    To,
    Cc,
    Bcc,
}

/// An in-progress recipient completion.
pub struct Suggest {
    pub field: Field,
    pub matches: Vec<Addr>,
    pub selected: usize,
}

pub struct ComposeState {
    pub draft: Draft,
    pub open: bool,
    /// Set while the send is in flight, so the button cannot be double-fired.
    pub sending: bool,
    pub show_cc: bool,
    /// Recipient completion, when a field is being typed into.
    pub suggest: Option<Suggest>,
}

impl ComposeState {
    pub fn new(draft: Draft) -> Self {
        let show_cc = !draft.cc.trim().is_empty() || !draft.bcc.trim().is_empty();
        Self { draft, open: true, sending: false, show_cc, suggest: None }
    }

    /// A draft with content the user would not want to lose.
    pub fn has_content(&self) -> bool {
        !self.draft.is_empty() || !self.draft.attachments.is_empty()
    }
}

/// Draws the compose window. Returns an action when the user asks for one.
pub fn show(
    ctx: &Context,
    state: &mut ComposeState,
    account: Option<&AccountConfig>,
    theme: &Theme,
    lookup: &dyn Fn(&str) -> Vec<Addr>,
) -> Option<ComposeAction> {
    let mut action = None;
    let mut open = state.open;

    let title = if state.draft.subject.trim().is_empty() {
        "New message".to_string()
    } else {
        state.draft.subject.clone()
    };

    egui::Window::new(title)
        .id(egui::Id::new("compose-window"))
        .open(&mut open)
        .default_size([680.0, 520.0])
        .min_width(420.0)
        .min_height(320.0)
        .collapsible(false)
        .show(ctx, |ui| {
            action = body(ui, state, account, theme, lookup);
        });

    // The window's own close button.
    if !open && state.open {
        state.open = false;
        action = Some(ComposeAction::Close);
    }
    action
}

fn body(
    ui: &mut Ui,
    state: &mut ComposeState,
    account: Option<&AccountConfig>,
    theme: &Theme,
    lookup: &dyn Fn(&str) -> Vec<Addr>,
) -> Option<ComposeAction> {
    let mut action = None;

    ui.horizontal(|ui| {
        ui.label(theme.faint_text("From"));
        match account {
            Some(account) => {
                let identities = account.identities();
                if identities.len() > 1 {
                    // Empty means the primary address, so resolve it to a
                    // real one before offering the choice.
                    let mut chosen = account.identity_for(&state.draft.from).email;
                    ui.add(
                        Select::new("compose-from", &mut chosen)
                            .options(
                                identities
                                    .iter()
                                    .map(|identity| (identity.email.clone(), identity.label())),
                            )
                            .width(320.0),
                    );
                    state.draft.from = chosen;
                } else {
                    ui.label(theme.body_text(account.identity_for("").label()));
                }
            }
            None => {
                ui.label(
                    RichText::new("no account selected").color(ui.visuals().error_fg_color),
                );
            }
        }
    });
    ui.add_space(6.0);

    // Keys the dropdown reacts to are taken before the fields are drawn, so
    // an arrow key moves the selection instead of the caret.
    let keys = take_completion_keys(ui, state.suggest.is_some());

    recipient_field(ui, state, Field::To, "To", Some("name@example.com"), lookup, &keys);

    if state.show_cc {
        recipient_field(ui, state, Field::Cc, "Cc", None, lookup, &keys);
        recipient_field(ui, state, Field::Bcc, "Bcc", None, lookup, &keys);
    } else if ui
        .add(Button::new("Add Cc / Bcc").size(ButtonSize::Small).outline())
        .clicked()
    {
        state.show_cc = true;
    }

    ui.add(TextInput::new(&mut state.draft.subject).label("Subject"));
    ui.add_space(8.0);

    if !state.draft.attachments.is_empty() {
        Card::new().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let mut remove = None;
                for (index, path) in state.draft.attachments.iter().enumerate() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let size = std::fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0);
                    let label = format!("{name}  {}  {}", format_size(size), glyphs::X);
                    if ui.add(Button::new(label).size(ButtonSize::Small).outline()).clicked() {
                        remove = Some(index);
                    }
                }
                if let Some(index) = remove {
                    action = Some(ComposeAction::RemoveAttachment(index));
                }
            });
        });
        ui.add_space(6.0);
    }

    // Size the editor to the space left after the action row, in whole lines.
    let line_height = ui.text_style_height(&egui::TextStyle::Body);
    let rows = (((ui.available_height() - 52.0) / line_height) as usize).max(6);
    ui.add(
        TextArea::new(&mut state.draft.body)
            .rows(rows)
            .hint("Write your message\u{2026}"),
    );

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let ready = account.is_some() && !state.draft.to.trim().is_empty() && !state.sending;
        if ui
            .add(
                Button::new(format!("{} Send", glyphs::UPLOAD))
                    .accent(Accent::Blue)
                    .enabled(ready)
                    .loading(state.sending),
            )
            .clicked()
        {
            action = Some(ComposeAction::Send);
        }
        if ui
            .add(Button::new(format!("{} Attach", glyphs::PLUS)).outline())
            .clicked()
        {
            action = Some(ComposeAction::AttachFile);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(Button::new("Discard").outline().accent(Accent::Red)).clicked() {
                action = Some(ComposeAction::Close);
            }
            ui.label(theme.faint_text("Ctrl+Enter to send"));
        });
    });

    // Ctrl+Enter sends from anywhere in the window, including the body editor.
    if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter))
        && account.is_some()
        && !state.draft.to.trim().is_empty()
        && !state.sending
    {
        action = Some(ComposeAction::Send);
    }

    action
}

/// Which completion keys were pressed this frame.
#[derive(Default)]
struct CompletionKeys {
    up: bool,
    down: bool,
    accept: bool,
    dismiss: bool,
}

/// Consumes the keys the dropdown uses, so the text field never sees them.
fn take_completion_keys(ui: &Ui, active: bool) -> CompletionKeys {
    if !active {
        return CompletionKeys::default();
    }
    ui.input_mut(|input| CompletionKeys {
        up: input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
        down: input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
        // Tab as well as Enter: both mean "take the highlighted one".
        accept: input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            || input.consume_key(egui::Modifiers::NONE, egui::Key::Tab),
        dismiss: input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
    })
}

/// A recipient field with completion from previously seen addresses.
fn recipient_field(
    ui: &mut Ui,
    state: &mut ComposeState,
    field: Field,
    label: &str,
    hint: Option<&str>,
    lookup: &dyn Fn(&str) -> Vec<Addr>,
    keys: &CompletionKeys,
) {
    let text = match field {
        Field::To => &mut state.draft.to,
        Field::Cc => &mut state.draft.cc,
        Field::Bcc => &mut state.draft.bcc,
    };

    // A pinned id, not egui's auto-id: the Cc and Bcc fields appear and
    // disappear, which would otherwise shift the id and lose the caret.
    let salt = egui::Id::new(("compose-recipient", label));
    let mut input = TextInput::new(text).label(label).id_salt(salt);
    if let Some(hint) = hint {
        input = input.hint(hint);
    }
    let response = ui.add(input);
    // The response is the editor's own, so this is the id its caret is
    // stored under. Recomputing it from the salt would not match: elegance
    // derives it inside a child `ui`, whose id differs from this one's.
    let edit_id = response.id;

    // Look up only when the fragment changes, not on every frame.
    if response.changed() {
        let value = match field {
            Field::To => state.draft.to.clone(),
            Field::Cc => state.draft.cc.clone(),
            Field::Bcc => state.draft.bcc.clone(),
        };
        let (_, fragment) = current_token(&value);
        state.suggest = (fragment.len() >= 2)
            .then(|| {
                let matches = lookup(fragment);
                (!matches.is_empty()).then(|| Suggest { field, matches, selected: 0 })
            })
            .flatten();
    }

    let Some(suggest) = &mut state.suggest else { return };
    if suggest.field != field {
        return;
    }
    if keys.dismiss {
        state.suggest = None;
        return;
    }
    if keys.down {
        suggest.selected = (suggest.selected + 1) % suggest.matches.len();
    }
    if keys.up {
        suggest.selected =
            (suggest.selected + suggest.matches.len() - 1) % suggest.matches.len();
    }

    let mut chosen = keys.accept.then(|| suggest.matches[suggest.selected].clone());
    let highlighted = suggest.selected;
    let matches = suggest.matches.clone();

    // An Area rather than inline content: a dropdown that pushed the rest of
    // the form down as you typed would be unusable.
    egui::Area::new(ui.id().with(("recipients", label)))
        .order(egui::Order::Foreground)
        .fixed_pos(response.rect.left_bottom() + egui::vec2(0.0, 2.0))
        .show(ui.ctx(), |ui| {
            ui.set_max_width(response.rect.width().max(220.0));
            Card::new().padding(4.0).show(ui, |ui| {
                for (index, address) in matches.iter().enumerate() {
                    let label = if address.name.is_empty() {
                        address.email.clone()
                    } else {
                        format!("{}  \u{2014}  {}", address.name, address.email)
                    };
                    if ui.selectable_label(index == highlighted, label).clicked() {
                        chosen = Some(address.clone());
                    }
                }
            });
        });

    if let Some(address) = chosen {
        let text = match field {
            Field::To => &mut state.draft.to,
            Field::Cc => &mut state.draft.cc,
            Field::Bcc => &mut state.draft.bcc,
        };
        *text = complete(text, &address);
        let length = text.chars().count();
        state.suggest = None;

        // Rewriting the field behind egui's back leaves its caret at the old
        // offset, so the next character typed lands in the middle of the
        // address that was just completed. Put the caret at the end.
        move_caret_to_end(ui, edit_id, length);
    }
}

/// Places the text caret after the last character of a field.
fn move_caret_to_end(ui: &Ui, edit_id: egui::Id, length: usize) {
    let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit_id) else { return };
    let end = egui::text::CCursor::new(length);
    state
        .cursor
        .set_char_range(Some(egui::text_selection::CCursorRange::one(end)));
    state.store(ui.ctx(), edit_id);
}

/// The address fragment being typed: everything after the last separator.
///
/// Returns the byte offset it starts at, so a completion can replace exactly
/// that and leave the addresses already entered alone.
fn current_token(text: &str) -> (usize, &str) {
    let start = text
        .rfind([',', ';'])
        .map(|at| at + 1)
        .unwrap_or(0);
    let fragment = &text[start..];
    let trimmed = fragment.trim_start();
    (start + (fragment.len() - trimmed.len()), trimmed)
}

/// Replaces the fragment being typed with a chosen address.
///
/// Leaves a trailing separator so the next address can be typed straight
/// away, which is the whole point of completing one.
fn complete(text: &str, address: &Addr) -> String {
    let (start, _) = current_token(text);
    format!("{}{}, ", &text[..start], address.full())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str, email: &str) -> Addr {
        Addr { name: name.into(), email: email.into() }
    }

    #[test]
    fn takes_the_fragment_after_the_last_separator() {
        assert_eq!(current_token("ada"), (0, "ada"));
        // The offset is where the fragment starts, past the separator and
        // any spaces after it.
        assert_eq!(current_token("bob@example.org, ad"), (17, "ad"));
        // Semicolons separate too.
        assert_eq!(current_token("bob@example.org;  ad"), (18, "ad"));
    }

    #[test]
    fn an_empty_tail_completes_nothing() {
        assert_eq!(current_token("bob@example.org, ").1, "");
        assert_eq!(current_token("").1, "");
    }

    #[test]
    fn completing_keeps_the_addresses_already_entered() {
        let chosen = addr("Ada Lovelace", "ada@example.com");
        assert_eq!(
            complete("bob@example.org, ad", &chosen),
            "bob@example.org, Ada Lovelace <ada@example.com>, "
        );
    }

    #[test]
    fn completing_the_first_address_replaces_the_whole_field() {
        let chosen = addr("", "ada@example.com");
        assert_eq!(complete("ad", &chosen), "ada@example.com, ");
    }

    #[test]
    fn a_display_name_with_a_comma_still_round_trips() {
        let chosen = addr("Doe, Jane", "jane@example.com");
        let completed = complete("ja", &chosen);
        let parsed = crate::mail::parse::parse_address_list(&completed);
        assert_eq!(parsed.len(), 1, "the name's comma split the address");
        assert_eq!(parsed[0].email, "jane@example.com");
    }
}
