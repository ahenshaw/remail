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

                        if mailbox_row(
                            ui,
                            mailbox,
                            depth,
                            selected,
                            &font,
                            row_height,
                            &input.theme.palette,
                        ) {
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
    let text_font = FontId::new(font.size, font.family.clone());

    // Drawn rather than set: neither small triangle has a glyph in the
    // bundled fonts, and a disclosure arrow that renders as a box is worse
    // than no arrow at all.
    disclosure_arrow(
        painter,
        pos2(rect.left() + 9.0, rect.center().y),
        font.size * 0.30,
        view.expanded,
        visuals.weak_text_color(),
    );
    painter.circle_filled(
        pos2(rect.left() + 18.0, rect.center().y),
        3.0,
        connection_color(view.state),
    );

    let left = rect.left() + 26.0;
    // Laid out first so the row can centre it, rather than centring an
    // estimate of where the text will land.
    let galley = painter.layout_no_wrap(
        name.to_string(),
        text_font,
        visuals.strong_text_color(),
    );
    painter.galley(
        pos2(left, rect.center().y - galley.size().y * 0.5),
        galley,
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
    palette: &elegance::Palette,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }

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
    let indent = rect.left() + 6.0 + 9.0 * depth as f32;

    // Lay the label out first so the icon can be aligned to the text that is
    // actually there, rather than to an estimate of it.
    let text_font = FontId::new(font.size, font.family.clone());
    let icon_size = font.size * 0.96;
    let text_left = indent + icon_size + font.size * 0.38;

    // Reserve the badge before wrapping, so a long name shortens rather than
    // running under the count.
    let mut right = rect.right() - 4.0;
    let badge = unread.then(|| {
        painter.layout_no_wrap(
            mailbox.unseen.to_string(),
            FontId::new(font.size * 0.82, font.family.clone()),
            accent,
        )
    });
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

    // Stood on the text baseline, the way a capital letter is. Centring the
    // icon on the row does not work: a line box holds a descender's worth of
    // space below the baseline, so its centre sits well under the letters.
    let baseline = top + metrics.baseline;
    super::icons::draw_mailbox(
        painter,
        Rect::from_min_size(
            pos2(indent, baseline - icon_size),
            Vec2::splat(icon_size),
        ),
        mailbox.special,
        icon_color(mailbox.special, palette),
    );

    if let Some(badge) = badge {
        let y = rect.center().y - badge.size().y * 0.5;
        painter.galley(pos2(right + 6.0, y), badge, accent);
    }

    // Only offer the full path when the name is actually cut off, or when the
    // leaf alone is ambiguous because the folder is nested.
    if shortened || depth > 0 {
        return response.on_hover_text(&mailbox.name).clicked();
    }
    response.clicked()
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
