//! The "Move to folder" dialog.
//!
//! A flat, filtered list rather than a tree. An account with sixty folders
//! is common, and picking one of sixty by expanding a tree is slower than
//! typing three letters of its name — so the filter is the primary control
//! and the list follows it.

use egui::Context;
use elegance::{Accent, Button, Modal, TextInput, Theme, glyphs};

use crate::config::AccountId;
use crate::mail::{MailboxInfo, RowKey, SpecialUse};

/// Open dialog state.
pub struct MoveDialog {
    pub account: AccountId,
    /// The rows being moved, captured when the dialog opened so that
    /// clicking elsewhere cannot change what is about to move.
    pub rows: Vec<RowKey>,
    pub filter: String,
    /// Index into the filtered list, moved with the arrow keys.
    pub selected: usize,
}

impl MoveDialog {
    pub fn new(account: AccountId, rows: Vec<RowKey>) -> Self {
        Self { account, rows, filter: String::new(), selected: 0 }
    }

    /// The mailboxes the rows currently live in, which are not destinations.
    fn sources(&self) -> Vec<&str> {
        let mut sources: Vec<&str> = self.rows.iter().map(|row| row.mailbox.as_str()).collect();
        sources.sort_unstable();
        sources.dedup();
        sources
    }
}

/// A folder the dialog is offering.
struct Candidate<'a> {
    mailbox: &'a MailboxInfo,
    /// Where the filter matched, for highlighting. Empty when not filtering.
    matched: bool,
}

/// Folders that can be moved into, in the order to show them.
///
/// Free of the dialog so the filtering rules can be tested without a UI.
pub fn destinations<'a>(
    mailboxes: &'a [MailboxInfo],
    sources: &[&str],
    filter: &str,
) -> Vec<&'a MailboxInfo> {
    let needle = filter.trim().to_ascii_lowercase();

    mailboxes
        .iter()
        // A container that cannot hold messages is not somewhere to put them.
        .filter(|mailbox| mailbox.selectable)
        // Moving a message to where it already is does nothing, and the
        // server may well refuse.
        .filter(|mailbox| !sources.contains(&mailbox.name.as_str()))
        // Gmail's All Mail is a view, not a folder: moving into it is how
        // messages get archived, which is what the Archive button is for.
        .filter(|mailbox| mailbox.special != SpecialUse::All)
        .filter(|mailbox| needle.is_empty() || mailbox.name.to_ascii_lowercase().contains(&needle))
        .collect()
}

/// Draws the dialog. Returns the chosen destination, and whether it closed.
pub fn show(
    ctx: &Context,
    dialog: &mut MoveDialog,
    mailboxes: &[MailboxInfo],
    theme: &Theme,
) -> (Option<String>, bool) {
    let mut chosen = None;
    let mut open = true;
    let mut cancelled = false;

    let sources = dialog.sources();
    let candidates: Vec<&MailboxInfo> = destinations(mailboxes, &sources, &dialog.filter);
    dialog.selected = dialog.selected.min(candidates.len().saturating_sub(1));

    // Read before the modal draws, so the list keys are not also delivered to
    // the filter field, which has focus.
    let (up, down, enter) = ctx.input(|input| {
        (
            input.key_pressed(egui::Key::ArrowUp),
            input.key_pressed(egui::Key::ArrowDown),
            input.key_pressed(egui::Key::Enter),
        )
    });
    if down && !candidates.is_empty() {
        dialog.selected = (dialog.selected + 1).min(candidates.len() - 1);
    }
    if up {
        dialog.selected = dialog.selected.saturating_sub(1);
    }
    if enter && let Some(mailbox) = candidates.get(dialog.selected) {
        chosen = Some(mailbox.name.clone());
    }

    let count = dialog.rows.len();
    let heading =
        if count == 1 { "Move message".to_string() } else { format!("Move {count} messages") };

    Modal::new("move-to", &mut open)
        .heading(heading)
        .header_icon(glyphs::FOLDER.to_string())
        .max_width(460.0)
        .show(ctx, |ui| {
            ui.add(TextInput::new(&mut dialog.filter).label("Folder").hint("Type to filter"));
            ui.add_space(8.0);

            if candidates.is_empty() {
                ui.label(theme.muted_text(if dialog.filter.trim().is_empty() {
                    "This account has nowhere else to put them."
                } else {
                    "No folder matches."
                }));
            } else {
                let row_height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
                egui::ScrollArea::vertical().max_height(row_height * 9.0).show(ui, |ui| {
                    for (index, mailbox) in candidates.iter().enumerate() {
                        let row = folder_row(
                            ui,
                            Candidate { mailbox, matched: index == dialog.selected },
                            theme,
                            row_height,
                        );
                        if row {
                            chosen = Some(mailbox.name.clone());
                        }
                    }
                });
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let target = candidates.get(dialog.selected);
                if ui
                    .add(Button::new("Move").accent(Accent::Blue).enabled(target.is_some()))
                    .clicked()
                    && let Some(mailbox) = target
                {
                    chosen = Some(mailbox.name.clone());
                }
                if ui.add(Button::new("Cancel").outline()).clicked() {
                    cancelled = true;
                }
            });
        });

    (chosen, cancelled || !open)
}

/// One folder in the list. Returns whether it was chosen.
fn folder_row(ui: &mut egui::Ui, candidate: Candidate<'_>, theme: &Theme, height: f32) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }

    let palette = &theme.palette;
    if candidate.matched {
        ui.painter().rect_filled(rect, 4.0, super::accent_tint(palette, 0.78));
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, palette.depth_tint(palette.card, 0.04));
    }

    let font = egui::TextStyle::Body.resolve(ui.style());
    let colour = if candidate.matched { palette.text } else { palette.text_muted };
    // Centred on the capitals rather than on the line box: the two coincide
    // for the bundled face and for almost nothing else. See `TextMetrics`.
    let metrics = super::TextMetrics::measure(ui.painter(), &font);
    let galley = ui.painter().layout_no_wrap(
        crate::mail::model::display_folder(&candidate.mailbox.name).to_string(),
        font,
        colour,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 10.0, metrics.top_for_centred_caps(rect.center().y)),
        galley,
        colour,
    );

    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn mailbox(name: &str, special: SpecialUse, selectable: bool) -> MailboxInfo {
        MailboxInfo {
            name: name.to_string(),
            delimiter: Some("/".into()),
            special,
            selectable,
            unseen: 0,
        }
    }

    fn account() -> Vec<MailboxInfo> {
        vec![
            mailbox("INBOX", SpecialUse::Inbox, true),
            mailbox("[Gmail]", SpecialUse::Normal, false),
            mailbox("[Gmail]/All Mail", SpecialUse::All, true),
            mailbox("[Gmail]/Trash", SpecialUse::Trash, true),
            mailbox("Work", SpecialUse::Normal, true),
            mailbox("Work/Reports", SpecialUse::Normal, true),
        ]
    }

    fn names(mailboxes: &[&MailboxInfo]) -> Vec<String> {
        mailboxes.iter().map(|m| m.name.clone()).collect()
    }

    /// Keeps the mailbox list alive while the borrowed result is inspected.
    fn found(sources: &[&str], filter: &str) -> Vec<String> {
        let account = account();
        names(&destinations(&account, sources, filter))
    }

    #[test]
    fn offers_every_folder_that_can_hold_messages() {
        assert_eq!(found(&[], ""), ["INBOX", "[Gmail]/Trash", "Work", "Work/Reports"]);
    }

    #[test]
    fn a_container_that_holds_no_messages_is_not_a_destination() {
        assert!(!found(&[], "gmail").contains(&"[Gmail]".to_string()));
    }

    #[test]
    fn all_mail_is_archiving_rather_than_a_folder() {
        assert!(!found(&[], "").contains(&"[Gmail]/All Mail".to_string()));
    }

    #[test]
    fn the_folder_a_message_is_already_in_is_not_offered() {
        assert_eq!(found(&["Work"], ""), ["INBOX", "[Gmail]/Trash", "Work/Reports"]);
    }

    #[test]
    fn every_source_is_excluded_when_the_rows_span_mailboxes() {
        // Search results come from more than one folder at a time.
        assert_eq!(found(&["INBOX", "Work"], ""), ["[Gmail]/Trash", "Work/Reports"]);
    }

    #[test]
    fn the_filter_matches_anywhere_in_the_path() {
        assert_eq!(found(&[], "rep"), ["Work/Reports"]);
        assert_eq!(found(&[], "work"), ["Work", "Work/Reports"]);
        // Case does not matter on either side.
        assert_eq!(found(&[], "INBOX"), ["INBOX"]);
        assert!(found(&[], "nothing").is_empty());
    }
}

#[cfg(test)]
mod render_tests {
    use super::tests::mailbox;
    use super::*;

    /// As `render_sidebar_rows`, for the folder picker.
    #[test]
    #[ignore = "writes a file; run it when you want to look at something"]
    fn render_move_dialog() {
        let out = std::env::var("REMAIL_RENDER").unwrap_or_else(|_| "/tmp/move.png".into());
        let theme = Theme::slate();
        let mailboxes = vec![
            mailbox("INBOX", SpecialUse::Inbox, true),
            mailbox("Work", SpecialUse::Normal, true),
            mailbox("Work/Reports", SpecialUse::Normal, true),
            mailbox("Engineering", SpecialUse::Normal, true),
            mailbox("[Gmail]/Trash", SpecialUse::Trash, true),
        ];

        crate::ui::raster::render(&out, 330.0, 300.0, 5.0, move |ui| {
            let mut dialog =
                MoveDialog::new(1, vec![crate::mail::RowKey { mailbox: "INBOX".into(), uid: 1 }]);
            dialog.selected = 1;
            egui::CentralPanel::default().show(ui, |ui| {
                let candidates = destinations(&mailboxes, &["INBOX"], "");
                let row_height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
                for (index, mailbox) in candidates.iter().enumerate() {
                    folder_row(
                        ui,
                        Candidate { mailbox, matched: index == dialog.selected },
                        &theme,
                        row_height,
                    );
                }
            });
        });
    }
}
