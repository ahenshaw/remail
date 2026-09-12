//! Left pane: accounts and their mailboxes.
//!
//! Rows are painted directly rather than built from widgets, so a folder name
//! shortens with an ellipsis when the pane is narrow instead of wrapping or
//! pushing the unread count off the edge.

use std::collections::{HashMap, HashSet};

use egui::{Align2, Color32, FontId, RichText, Sense, Ui, Vec2, pos2};
use elegance::{Accent, Button, ButtonSize, Theme};

use super::{Action, paint_truncated};
use crate::config::{AccountId, Config, PaneStyle};
use crate::mail::{ConnectionState, MailboxInfo};

/// Per-account view state the sidebar needs.
pub struct AccountView {
    pub mailboxes: Vec<MailboxInfo>,
    pub state: ConnectionState,
    pub expanded: bool,
    /// Set when the engine reports the account has no usable authorization.
    pub needs_sign_in: bool,
}

impl Default for AccountView {
    fn default() -> Self {
        Self {
            mailboxes: Vec::new(),
            state: ConnectionState::Offline,
            expanded: true,
            needs_sign_in: false,
        }
    }
}

pub struct SidebarInput<'a> {
    pub config: &'a Config,
    pub accounts: &'a mut HashMap<AccountId, AccountView>,
    pub selected: Option<(AccountId, &'a str)>,
    pub style: PaneStyle,
    pub base_size: f32,
    pub theme: &'a Theme,
}

pub fn show(ui: &mut Ui, input: SidebarInput<'_>) -> Option<Action> {
    let mut action = None;

    if input.config.accounts.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No accounts yet").strong());
            ui.add_space(6.0);
            ui.label(
                RichText::new("Add one from Accounts in the toolbar.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
        return None;
    }

    let font = super::pane_font(input.style, input.base_size);
    let size = font.size;
    // Rows are sized from the text, so tightening the font tightens the list.
    let row_height = (size * 1.5).round();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;

            for account in input.config.accounts.iter().filter(|a| a.enabled) {
                let view = input.accounts.entry(account.id).or_default();

                if account_header(ui, account.short_name(), view, &font, row_height) {
                    view.expanded = !view.expanded;
                }

                if view.needs_sign_in {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        if ui
                            .add(
                                Button::new("Sign in with Google")
                                    .size(ButtonSize::Small)
                                    .accent(Accent::Blue),
                            )
                            .clicked()
                        {
                            action = Some(Action::SignIn(account.id));
                        }
                    });
                } else if matches!(
                    view.state,
                    ConnectionState::Offline | ConnectionState::Failed
                ) {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        if ui
                            .add(Button::new("Connect").size(ButtonSize::Small).outline())
                            .clicked()
                        {
                            action = Some(Action::Connect(account.id));
                        }
                    });
                }

                if view.expanded {
                    // Indent against the mailboxes actually on screen, so the
                    // children of a hidden container are not left dangling.
                    let shown: HashSet<&str> = view
                        .mailboxes
                        .iter()
                        .filter(|m| m.selectable)
                        .map(|m| m.name.as_str())
                        .collect();

                    for mailbox in view.mailboxes.iter().filter(|m| m.selectable) {
                        let selected = input
                            .selected
                            .is_some_and(|(a, m)| a == account.id && m == mailbox.name);
                        let depth = mailbox.display_depth(|path| shown.contains(path));

                        if mailbox_row(ui, mailbox, depth, selected, &font, row_height) {
                            action = Some(Action::OpenMailbox {
                                account: account.id,
                                mailbox: mailbox.name.clone(),
                            });
                        }
                    }

                    if view.mailboxes.is_empty() && view.state == ConnectionState::Online {
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            ui.label(input.theme.faint_text("no mailboxes"));
                        });
                    }
                }
                ui.add_space(4.0);
            }
        });

    action
}

/// Draws the account line. Returns true when the user toggled it.
fn account_header(
    ui: &mut Ui,
    name: &str,
    view: &AccountView,
    font: &FontId,
    row_height: f32,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }

    let visuals = ui.visuals();
    let painter = ui.painter();
    let baseline = rect.top() + (row_height - font.size) * 0.5 - 1.0;

    painter.text(
        pos2(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        if view.expanded { "\u{25be}" } else { "\u{25b8}" },
        FontId::proportional(font.size * 0.8),
        visuals.weak_text_color(),
    );
    painter.circle_filled(
        pos2(rect.left() + 18.0, rect.center().y),
        3.0,
        connection_color(view.state),
    );

    let left = rect.left() + 26.0;
    let _ = paint_truncated(
        painter,
        pos2(left, baseline),
        rect.right() - left - 4.0,
        name,
        FontId::new(font.size, font.family.clone()),
        visuals.strong_text_color(),
    );

    response.on_hover_text(state_label(view.state)).clicked()
}

/// Draws one mailbox line. Returns true when it was clicked.
fn mailbox_row(
    ui: &mut Ui,
    mailbox: &MailboxInfo,
    depth: usize,
    selected: bool,
    font: &FontId,
    row_height: f32,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }

    let visuals = ui.visuals();
    let painter = ui.painter();

    let background = if selected {
        visuals.selection.bg_fill.gamma_multiply(0.65)
    } else if response.hovered() {
        visuals.widgets.hovered.bg_fill.gamma_multiply(0.5)
    } else {
        Color32::TRANSPARENT
    };
    if background != Color32::TRANSPARENT {
        painter.rect_filled(rect.shrink2(Vec2::new(2.0, 0.0)), 3.0, background);
    }

    let unread = mailbox.unseen > 0;
    let color = if selected || unread {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };
    let baseline = rect.top() + (row_height - font.size) * 0.5 - 1.0;

    // Indentation is deliberately small: the pane may be very narrow.
    let icon_left = rect.left() + 6.0 + 9.0 * depth as f32;
    painter.text(
        pos2(icon_left, rect.center().y),
        Align2::LEFT_CENTER,
        mailbox.special.icon(),
        FontId::proportional(font.size * 0.85),
        visuals.weak_text_color(),
    );

    // Reserve room for the unread badge before laying out the name.
    let mut right = rect.right() - 4.0;
    if unread {
        let badge = mailbox.unseen.to_string();
        let galley = painter.layout_no_wrap(
            badge,
            FontId::new(font.size * 0.82, font.family.clone()),
            visuals.selection.bg_fill,
        );
        let width = galley.size().x;
        painter.galley(
            pos2(right - width, baseline + font.size * 0.08),
            galley,
            visuals.selection.bg_fill,
        );
        right -= width + 6.0;
    }

    let text_left = icon_left + font.size * 1.15;
    let shortened = paint_truncated(
        painter,
        pos2(text_left, baseline),
        right - text_left,
        mailbox.leaf(),
        FontId::new(font.size, font.family.clone()),
        color,
    );

    // Only offer the full path when the name is actually cut off, or when the
    // leaf alone is ambiguous because the folder is nested.
    if shortened || depth > 0 {
        return response.on_hover_text(&mailbox.name).clicked();
    }
    response.clicked()
}

fn connection_color(state: ConnectionState) -> Color32 {
    match state {
        ConnectionState::Online => Color32::from_rgb(90, 190, 110),
        ConnectionState::Connecting => Color32::from_rgb(220, 180, 70),
        ConnectionState::Failed => Color32::from_rgb(220, 100, 90),
        ConnectionState::Offline => Color32::GRAY,
    }
}

fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Online => "Connected",
        ConnectionState::Connecting => "Connecting",
        ConnectionState::Failed => "Connection failed",
        ConnectionState::Offline => "Offline",
    }
}
