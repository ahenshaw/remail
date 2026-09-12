//! Left pane: accounts and their mailboxes.

use std::collections::{HashMap, HashSet};

use egui::{Color32, RichText, Ui};
use elegance::{Accent, Button, ButtonSize};

use super::Action;
use crate::config::{AccountId, Config};
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
}

pub fn show(ui: &mut Ui, input: SidebarInput<'_>) -> Option<Action> {
    let mut action = None;

    if input.config.accounts.is_empty() {
        ui.add_space(24.0);
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

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for account in input.config.accounts.iter().filter(|a| a.enabled) {
            let view = input.accounts.entry(account.id).or_default();

            ui.horizontal(|ui| {
                let arrow = if view.expanded { "\u{25be}" } else { "\u{25b8}" };
                if ui.selectable_label(false, arrow).clicked() {
                    view.expanded = !view.expanded;
                }
                ui.label(connection_dot(view.state)).on_hover_text(state_label(view.state));
                ui.label(RichText::new(account.title()).strong());
            });

            // Signing in is the only thing that helps an unauthorized
            // account, so never offer a Connect that is certain to fail.
            if view.needs_sign_in {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
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
                    ui.add_space(20.0);
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
                    let is_selected = input
                        .selected
                        .is_some_and(|(a, m)| a == account.id && m == mailbox.name);

                    ui.horizontal(|ui| {
                        let depth = mailbox.display_depth(|path| shown.contains(path));
                        ui.add_space(8.0 + 10.0 * depth as f32);
                        let label = format!("{} {}", mailbox.special.icon(), mailbox.leaf());
                        let mut text = RichText::new(label);
                        if mailbox.unseen > 0 {
                            text = text.strong();
                        }

                        if ui.selectable_label(is_selected, text).clicked() {
                            action = Some(Action::OpenMailbox {
                                account: account.id,
                                mailbox: mailbox.name.clone(),
                            });
                        }

                        if mailbox.unseen > 0 {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(mailbox.unseen.to_string())
                                            .small()
                                            .color(ui.visuals().selection.bg_fill),
                                    );
                                },
                            );
                        }
                    });
                }

                if view.mailboxes.is_empty() && view.state == ConnectionState::Online {
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            RichText::new("no mailboxes")
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
                }
            }
            ui.add_space(6.0);
        }
    });

    action
}

fn connection_dot(state: ConnectionState) -> RichText {
    let (glyph, color) = match state {
        ConnectionState::Online => ("\u{25cf}", Color32::from_rgb(90, 190, 110)),
        ConnectionState::Connecting => ("\u{25cf}", Color32::from_rgb(220, 180, 70)),
        ConnectionState::Failed => ("\u{25cf}", Color32::from_rgb(220, 100, 90)),
        ConnectionState::Offline => ("\u{25cb}", Color32::GRAY),
    };
    RichText::new(glyph).color(color).small()
}

fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Online => "Connected",
        ConnectionState::Connecting => "Connecting",
        ConnectionState::Failed => "Connection failed",
        ConnectionState::Offline => "Offline",
    }
}
