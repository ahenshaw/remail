//! Left pane: accounts and their mailboxes.
//!
//! Rows are painted directly rather than built from widgets, so a folder name
//! shortens with an ellipsis when the pane is narrow instead of wrapping or
//! pushing the unread count off the edge.

use std::collections::{HashMap, HashSet};

use egui::{Color32, FontId, Rect, RichText, Sense, Ui, Vec2, pos2};
use elegance::{Accent, Button, ButtonSize, Theme};

use super::{Action, paint_truncated};
use crate::config::{AccountId, Config};
use crate::mail::{ConnectionState, MailboxInfo, SpecialUse};

/// Per-account view state the sidebar needs.
pub struct AccountView {
    pub mailboxes: Vec<MailboxInfo>,
    pub state: ConnectionState,
    pub expanded: bool,
    /// Set when the engine reports the account has no usable authorization.
    pub needs_sign_in: bool,
    /// Folders whose children are hidden.
    pub collapsed: HashSet<String>,
}

impl Default for AccountView {
    fn default() -> Self {
        Self {
            mailboxes: Vec::new(),
            state: ConnectionState::Offline,
            expanded: true,
            needs_sign_in: false,
            collapsed: HashSet::new(),
        }
    }
}

pub struct SidebarInput<'a> {
    pub config: &'a Config,
    pub accounts: &'a mut HashMap<AccountId, AccountView>,
    pub selected: Option<(AccountId, &'a str)>,
    pub font: FontId,
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

    let font = input.font.clone();
    let size = font.size;
    // Rows are sized from the text, so tightening the font tightens the list.
    let row_height = (size * 1.5).round();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;

            for account in input.config.accounts.iter().filter(|a| a.enabled) {
                let view = input.accounts.entry(account.id).or_default();

                match account_header(ui, account.short_name(), view, &font, row_height) {
                    Some(AccountOutcome::Toggle) => view.expanded = !view.expanded,
                    Some(AccountOutcome::NewFolder) => {
                        action = Some(Action::NewSubfolder {
                            account: account.id,
                            // No parent: a folder at the top of the account.
                            parent: String::new(),
                        });
                    }
                    None => {}
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
                        // Hidden if anything above it is collapsed.
                        let under_collapsed = mailbox
                            .ancestors()
                            .iter()
                            .any(|path| view.collapsed.contains(*path));
                        if under_collapsed {
                            continue;
                        }

                        let selected = input
                            .selected
                            .is_some_and(|(a, m)| a == account.id && m == mailbox.name);
                        let depth = mailbox.display_depth(|path| shown.contains(path));
                        let has_children = view
                            .mailboxes
                            .iter()
                            .any(|other| other.ancestors().contains(&mailbox.name.as_str()));

                        let row = mailbox_row(
                            ui,
                            RowInput {
                                mailbox,
                                depth,
                                selected,
                                has_children,
                                collapsed: view.collapsed.contains(&mailbox.name),
                                font: &font,
                                row_height,
                                palette: &input.theme.palette,
                            },
                        );
                        if let Some(found) = row {
                            action = Some(found.into_action(account.id, &mailbox.name));
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

/// What the account line was asked to do.
enum AccountOutcome {
    Toggle,
    NewFolder,
}

/// Draws the account line.
fn account_header(
    ui: &mut Ui,
    name: &str,
    view: &AccountView,
    font: &FontId,
    row_height: f32,
) -> Option<AccountOutcome> {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return None;
    }

    let visuals = ui.visuals();
    let painter = ui.painter();
    let text_font = FontId::new(font.size, font.family.clone());

    // Drawn rather than set: neither small triangle has a glyph in the
    // bundled fonts, and a disclosure arrow that renders as a box is worse
    // than no arrow at all.
    let metrics = TextMetrics::measure(painter, &text_font);
    let top = rect.center().y - metrics.line_height * 0.5;
    // Everything on this row hangs off the text's line, not the row's middle.
    let mark_centre = metrics.centre_for(top, font.size * 0.72);

    disclosure_arrow(
        painter,
        pos2(rect.left() + 9.0, mark_centre),
        font.size * 0.30,
        view.expanded,
        visuals.weak_text_color(),
    );
    painter.circle_filled(
        pos2(rect.left() + 18.0, mark_centre),
        3.0,
        connection_color(view.state),
    );

    let left = rect.left() + 26.0;
    let galley = painter.layout_no_wrap(
        name.to_string(),
        text_font,
        visuals.strong_text_color(),
    );
    painter.galley(pos2(left, top), galley, visuals.strong_text_color());

    let menu = elegance::ContextMenu::new(("account-menu", name)).show(&response, |ui| {
        ui.add(elegance::MenuItem::new("New folder\u{2026}")).clicked()
    });
    if menu.unwrap_or(false) {
        return Some(AccountOutcome::NewFolder);
    }

    response
        .on_hover_text(state_label(view.state))
        .clicked()
        .then_some(AccountOutcome::Toggle)
}

/// What a mailbox row was asked to do.
enum RowOutcome {
    Open,
    Toggle,
    MarkRead,
    NewChild,
    Rename,
    Delete,
}

impl RowOutcome {
    fn into_action(self, account: AccountId, mailbox: &str) -> Action {
        let mailbox = mailbox.to_string();
        match self {
            RowOutcome::Open => Action::OpenMailbox { account, mailbox },
            RowOutcome::Toggle => Action::ToggleFolder { account, mailbox },
            RowOutcome::MarkRead => Action::MarkFolderRead { account, mailbox },
            RowOutcome::NewChild => Action::NewSubfolder { account, parent: mailbox },
            RowOutcome::Rename => Action::RenameFolder { account, mailbox },
            RowOutcome::Delete => Action::DeleteFolder { account, mailbox },
        }
    }
}

struct RowInput<'a> {
    mailbox: &'a MailboxInfo,
    depth: usize,
    selected: bool,
    has_children: bool,
    collapsed: bool,
    font: &'a FontId,
    row_height: f32,
    palette: &'a elegance::Palette,
}

/// Draws one mailbox line.
fn mailbox_row(ui: &mut Ui, input: RowInput<'_>) -> Option<RowOutcome> {
    let RowInput { mailbox, depth, selected, has_children, collapsed, font, row_height, palette } =
        input;

    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return None;
    }

    let mut outcome = None;
    let visuals = ui.visuals();
    let painter = ui.painter();

    let background = if selected {
        super::accent_tint(palette, 0.80)
    } else if response.hovered() {
        super::accent_tint(palette, 0.92)
    } else {
        Color32::TRANSPARENT
    };
    if background != Color32::TRANSPARENT {
        painter.rect_filled(rect.shrink2(Vec2::new(2.0, 0.0)), 3.0, background);
    }

    let unread = mailbox.unseen > 0;
    let accent = palette.blue;
    let color = if selected || unread {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };

    // Room for a disclosure arrow at every depth, so names line up whether or
    // not a folder has children.
    let arrow_width = font.size * 0.8;
    let indent = rect.left() + 4.0 + 9.0 * depth as f32;
    let icon_left = indent + arrow_width;

    let text_font = FontId::new(font.size, font.family.clone());
    let icon_size = font.size * 0.96;
    let text_left = icon_left + icon_size + font.size * 0.38;

    let mut right = rect.right() - 4.0;
    let badge_font = FontId::new(font.size * 0.82, font.family.clone());
    let badge = unread
        .then(|| painter.layout_no_wrap(mailbox.unseen.to_string(), badge_font.clone(), accent));
    if let Some(badge) = &badge {
        right -= badge.size().x + 6.0;
    }

    let metrics = TextMetrics::measure(painter, &text_font);
    let top = rect.center().y - metrics.line_height * 0.5;
    let shortened = paint_truncated(
        painter,
        pos2(text_left, top),
        right - text_left,
        mailbox.display_name(),
        text_font,
        color,
    );

    let baseline = top + metrics.baseline;
    super::icons::draw_mailbox(
        painter,
        Rect::from_min_size(pos2(icon_left, baseline - icon_size), Vec2::splat(icon_size)),
        mailbox.special,
        icon_color(mailbox.special, palette),
    );

    if has_children {
        disclosure_arrow(
            painter,
            pos2(indent + arrow_width * 0.5, metrics.centre_for(top, icon_size)),
            font.size * 0.26,
            !collapsed,
            visuals.weak_text_color(),
        );
    }

    if let Some(badge) = badge {
        // Smaller text, so its line box differs: line the two baselines up
        // rather than their tops, which would leave the count riding high.
        let badge_metrics = TextMetrics::measure(painter, &badge_font);
        let badge_top = top + metrics.baseline - badge_metrics.baseline;
        painter.galley(pos2(right + 6.0, badge_top), badge, accent);
    }

    // A click on the arrow folds the subtree; anywhere else opens the folder.
    if response.clicked() {
        let on_arrow = response
            .interact_pointer_pos()
            .is_some_and(|at| at.x < indent + arrow_width);
        outcome = Some(if has_children && on_arrow {
            RowOutcome::Toggle
        } else {
            RowOutcome::Open
        });
    }

    let menu = elegance::ContextMenu::new(("folder-menu", &mailbox.name)).show(
        &response,
        |ui| {
            let mut chosen = None;
            if ui
                .add(
                    elegance::MenuItem::new("Mark all as read")
                        .enabled(mailbox.unseen > 0),
                )
                .clicked()
            {
                chosen = Some(RowOutcome::MarkRead);
            }
            ui.separator();
            if ui.add(elegance::MenuItem::new("New subfolder\u{2026}")).clicked() {
                chosen = Some(RowOutcome::NewChild);
            }
            if ui.add(elegance::MenuItem::new("Rename\u{2026}")).clicked() {
                chosen = Some(RowOutcome::Rename);
            }
            if ui
                .add(
                    elegance::MenuItem::new("Delete folder\u{2026}")
                        .danger()
                        // A well-known mailbox is part of how the account
                        // works; removing it is not an ordinary edit.
                        .enabled(mailbox.special == SpecialUse::Normal),
                )
                .clicked()
            {
                chosen = Some(RowOutcome::Delete);
            }
            chosen
        },
    );
    if let Some(chosen) = menu.flatten() {
        outcome = Some(chosen);
    }

    // The full path is the only way to tell apart two folders whose leaf
    // names match, which is common once a pane is narrow enough to truncate.
    if shortened || depth > 0 {
        response.on_hover_text(&mailbox.name);
    }
    outcome
}

/// Where the ink sits inside a line of text.
///
/// The nominal font size says nothing about this: a line box reserves room
/// for ascenders and descenders, so its centre is not where the letters look
/// centred, and its top is not where they start.
struct TextMetrics {
    line_height: f32,
    /// Baseline, measured down from the top of the line box.
    baseline: f32,
}

impl TextMetrics {
    /// The y that a mark of `height` should be centred on so it sits on the
    /// baseline, like a capital letter.
    ///
    /// Not the row's centre: a line box reserves a descender's worth of space
    /// below the baseline, so its middle is well under the letters.
    fn centre_for(&self, top: f32, height: f32) -> f32 {
        top + self.baseline - height * 0.5
    }

    fn measure(painter: &egui::Painter, font: &FontId) -> Self {
        let galley =
            painter.layout_no_wrap("X".to_string(), font.clone(), Color32::PLACEHOLDER);
        let baseline = galley
            .rows
            .first()
            .and_then(|row| row.row.glyphs.first().map(|glyph| row.pos.y + glyph.pos.y))
            // A font with no glyph for "X" is not worth a special case; the
            // ascender of a typical face is close enough to keep going.
            .unwrap_or(font.size * 0.8);
        Self { line_height: galley.size().y, baseline }
    }
}

/// The colour of a mailbox's icon.
///
/// Ordinary folders are manila, as a paper folder is. The well-known ones take
/// a colour that says what they are at a glance. Accent colours come from the
/// palette so they track the theme; manila and magenta are not in it and are
/// given a light and a dark variant, since a single tan cannot carry on both
/// a white and a near-black background.
fn icon_color(special: SpecialUse, palette: &elegance::Palette) -> Color32 {
    let pick = |light: Color32, dark: Color32| if palette.is_dark { dark } else { light };

    match special {
        SpecialUse::Inbox => palette.blue,
        SpecialUse::Sent => palette.green,
        SpecialUse::Junk => palette.red,
        SpecialUse::Drafts => pick(
            Color32::from_rgb(0xb5, 0x2d, 0x8f),
            Color32::from_rgb(0xe2, 0x7d, 0xc6),
        ),
        // Deliberately not coloured: deleted mail should not draw the eye.
        SpecialUse::Trash => palette.text_faint,
        SpecialUse::Normal | SpecialUse::Archive | SpecialUse::All => pick(
            Color32::from_rgb(0xc4, 0x92, 0x3d),
            Color32::from_rgb(0xdc, 0xb9, 0x77),
        ),
    }
}

/// A filled triangle pointing down when expanded, right when collapsed.
fn disclosure_arrow(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    expanded: bool,
    color: Color32,
) {
    let points = if expanded {
        vec![
            pos2(center.x - radius, center.y - radius * 0.6),
            pos2(center.x + radius, center.y - radius * 0.6),
            pos2(center.x, center.y + radius * 0.8),
        ]
    } else {
        vec![
            pos2(center.x - radius * 0.6, center.y - radius),
            pos2(center.x - radius * 0.6, center.y + radius),
            pos2(center.x + radius * 0.8, center.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));
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
