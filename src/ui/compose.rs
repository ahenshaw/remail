//! The compose window.
//!
//! A draft lives in application state, not in this module, so closing and
//! reopening the window (or switching messages behind it) never loses typing.

use egui::{Context, RichText, Ui};
use elegance::{Accent, Button, ButtonSize, Card, TextArea, TextInput, Theme, glyphs};

use super::format_size;
use crate::config::AccountConfig;
use crate::mail::Draft;

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

pub struct ComposeState {
    pub draft: Draft,
    pub open: bool,
    /// Set while the send is in flight, so the button cannot be double-fired.
    pub sending: bool,
    pub show_cc: bool,
}

impl ComposeState {
    pub fn new(draft: Draft) -> Self {
        let show_cc = !draft.cc.trim().is_empty() || !draft.bcc.trim().is_empty();
        Self { draft, open: true, sending: false, show_cc }
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
            action = body(ui, state, account, theme);
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
) -> Option<ComposeAction> {
    let mut action = None;

    ui.horizontal(|ui| {
        ui.label(theme.faint_text("From"));
        match account {
            Some(account) => {
                let from = if account.display_name.trim().is_empty() {
                    account.email.clone()
                } else {
                    format!("{} <{}>", account.display_name, account.email)
                };
                ui.label(theme.body_text(from));
            }
            None => {
                ui.label(
                    RichText::new("no account selected").color(ui.visuals().error_fg_color),
                );
            }
        }
    });
    ui.add_space(6.0);

    ui.add(TextInput::new(&mut state.draft.to).label("To").hint("name@example.com"));

    if state.show_cc {
        ui.add(TextInput::new(&mut state.draft.cc).label("Cc"));
        ui.add(TextInput::new(&mut state.draft.bcc).label("Bcc"));
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
