//! Left pane: accounts and their mailboxes.
//!
//! Rows are painted directly rather than built from widgets, so a folder name
//! shortens with an ellipsis when the pane is narrow instead of wrapping or
//! pushing the unread count off the edge.

use std::collections::{HashMap, HashSet};

use egui::{Color32, FontId, Rect, RichText, Sense, Ui, Vec2, pos2};
use elegance::{Accent, Button, ButtonSize, Theme};

use super::{Action, TextMetrics, paint_truncated};
use crate::config::{AccountId, Config};
use crate::mail::{ConnectionState, MailboxInfo, SpecialUse};

/// Per-account view state the sidebar needs.
pub struct AccountView {
    pub mailboxes: Vec<MailboxInfo>,
    pub state: ConnectionState,
    /// Set when the engine reports the account has no usable authorization.
    pub needs_sign_in: bool,
}

impl Default for AccountView {
    fn default() -> Self {
        Self { mailboxes: Vec::new(), state: ConnectionState::Offline, needs_sign_in: false }
    }
}

pub struct SidebarInput<'a> {
    pub config: &'a Config,
    pub accounts: &'a mut HashMap<AccountId, AccountView>,
    pub selected: Option<(AccountId, &'a str)>,
    pub font: FontId,
    pub theme: &'a Theme,
    pub spring: &'a mut SpringLoad,
}

/// Folders held open by hovering over them during a drag.
///
/// Transient by construction: a drag is a way to reach a folder, not a
/// statement about how the sidebar should look. Nothing here reaches the
/// config, and everything it opened closes again when the drag ends — which
/// is why it is a separate set rather than a write to `collapsed_folders`.
#[derive(Default)]
pub struct SpringLoad {
    /// The folder the pointer is resting on, and the time it arrived.
    dwelling: Option<(AccountId, String, f64)>,
    /// What this drag has opened so far.
    opened: HashSet<(AccountId, String)>,
}

/// What a dwell is waiting for.
enum Dwell {
    /// Still waiting; repaint after this long even if nothing moves.
    Waiting(std::time::Duration),
    /// The folder just opened.
    Opened,
}

impl SpringLoad {
    /// How long the pointer must rest before a folder opens. Long enough not
    /// to fire while crossing a folder on the way somewhere else, short
    /// enough that waiting for it does not feel like being stuck.
    const DWELL: f64 = 0.45;

    fn is_open(&self, account: AccountId, mailbox: &str) -> bool {
        self.opened.iter().any(|(held, name)| *held == account && name == mailbox)
    }

    /// Records the pointer resting on a closed folder.
    fn dwell(&mut self, account: AccountId, mailbox: &str, now: f64) -> Dwell {
        match &self.dwelling {
            Some((held, name, since)) if *held == account && name == mailbox => {
                let left = Self::DWELL - (now - since);
                if left <= 0.0 {
                    self.opened.insert((account, mailbox.to_string()));
                    self.dwelling = None;
                    Dwell::Opened
                } else {
                    Dwell::Waiting(std::time::Duration::from_secs_f64(left))
                }
            }
            // A different folder, or none: the clock starts here.
            _ => {
                self.dwelling = Some((account, mailbox.to_string(), now));
                Dwell::Waiting(std::time::Duration::from_secs_f64(Self::DWELL))
            }
        }
    }

    /// The pointer is not resting on any closed folder.
    fn idle(&mut self) {
        self.dwelling = None;
    }

    /// The drag ended, however it ended. Everything it opened closes.
    fn end(&mut self) {
        self.dwelling = None;
        self.opened.clear();
    }
}

pub fn show(ui: &mut Ui, input: SidebarInput<'_>) -> Option<Action> {
    let SidebarInput { config, accounts, selected, font: pane_font, theme, spring } = input;
    let mut action = None;

    // The payload outlives the frame, so this is also how the sidebar knows
    // a drag is still in progress once the pointer has left the message list.
    let dragging = egui::DragAndDrop::payload::<super::DraggedMessages>(ui.ctx()).is_some();
    if !dragging {
        spring.end();
    }
    let now = ui.input(|i| i.time);
    // Set when the pointer is resting on some closed folder this frame.
    let mut dwelling_somewhere = false;

    if config.accounts.is_empty() {
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

    let font = pane_font.clone();
    let size = font.size;
    let row_height = row_height(size);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;

        for account in config.accounts.iter().filter(|a| a.enabled) {
            let view = accounts.entry(account.id).or_default();
            // Which folders are closed is remembered across restarts, so it
            // is read from the config rather than kept beside the mailboxes.
            // Folders a drag is holding open are not closed for as long as
            // it lasts, without the config hearing about it.
            let collapsed: HashSet<&str> = account
                .collapsed_folders
                .iter()
                .map(String::as_str)
                .filter(|name| !spring.is_open(account.id, name))
                .collect();

            let header =
                account_header(ui, account, view, account.sidebar_expanded, &font, row_height);
            match header {
                Some(AccountOutcome::Toggle) => {
                    action = Some(Action::ToggleAccount(account.id));
                }
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
            } else if matches!(view.state, ConnectionState::Offline | ConnectionState::Failed) {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    if ui.add(Button::new("Connect").size(ButtonSize::Small).outline()).clicked() {
                        action = Some(Action::Connect(account.id));
                    }
                });
            }

            if account.sidebar_expanded {
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
                    let under_collapsed =
                        mailbox.ancestors().iter().any(|path| collapsed.contains(path));
                    if under_collapsed {
                        continue;
                    }

                    let selected =
                        selected.is_some_and(|(a, m)| a == account.id && m == mailbox.name);
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
                            collapsed: collapsed.contains(mailbox.name.as_str()),
                            account: account.id,
                            font: &font,
                            row_height,
                            palette: &theme.palette,
                        },
                    );

                    // Hovering a closed folder with messages in hand opens
                    // it, so its children can be reached without breaking
                    // off the drag.
                    if row.carrying
                        && has_children
                        && account.collapsed_folders.contains(&mailbox.name)
                    {
                        dwelling_somewhere = true;
                        match spring.dwell(account.id, &mailbox.name, now) {
                            // Repaint even if the pointer never moves again,
                            // or a still hand would wait forever.
                            Dwell::Waiting(left) => ui.ctx().request_repaint_after(left),
                            Dwell::Opened => ui.ctx().request_repaint(),
                        }
                    }

                    if let Some(found) = row.outcome {
                        action = Some(found.into_action(account.id, &mailbox.name));
                    }
                }

                if view.mailboxes.is_empty() && view.state == ConnectionState::Online {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(theme.faint_text("no mailboxes"));
                    });
                }
            }
            ui.add_space(4.0);
        }
    });

    // Leaving a folder restarts the clock, so crossing several on the way
    // somewhere does not open the one that happened to be under the pointer
    // longest.
    if !dwelling_somewhere {
        spring.idle();
    }

    action
}

/// What the account line was asked to do.
enum AccountOutcome {
    Toggle,
    NewFolder,
}

/// Height of one folder row.
///
/// Sized from the text, so tightening the font tightens the list. The ratio
/// leaves room above and below the capitals rather than fitting them: a
/// folder list is read by running down it, and rows packed to the height of
/// their own letters give the eye nothing to travel between.
fn row_height(size: f32) -> f32 {
    (size * 1.8).round()
}

/// Draws the account line.
fn account_header(
    ui: &mut Ui,
    account: &crate::config::AccountConfig,
    view: &AccountView,
    expanded: bool,
    font: &FontId,
    row_height: f32,
) -> Option<AccountOutcome> {
    let name = account.short_name();
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
    let top = metrics.top_for_centred_caps(rect.center().y);
    // Everything on this row hangs off the text's line, not the row's middle.
    let mark_centre = metrics.caps_centre(top);

    disclosure_arrow(
        painter,
        pos2(rect.left() + 9.0, mark_centre),
        font.size * 0.30,
        expanded,
        visuals.weak_text_color(),
    );
    painter.circle_filled(pos2(rect.left() + 18.0, mark_centre), 3.0, connection_color(view.state));

    let left = rect.left() + 26.0;
    let galley = painter.layout_no_wrap(name.to_string(), text_font, visuals.strong_text_color());
    painter.galley(pos2(left, top), galley, visuals.strong_text_color());

    let menu = elegance::ContextMenu::new(("account-menu", name))
        .show(&response, |ui| ui.add(elegance::MenuItem::new("New folder\u{2026}")).clicked());
    if menu.unwrap_or(false) {
        return Some(AccountOutcome::NewFolder);
    }

    response.on_hover_text(state_label(view.state)).clicked().then_some(AccountOutcome::Toggle)
}

/// What a mailbox row was asked to do.
enum RowOutcome {
    Open,
    Toggle,
    /// Messages were dropped here.
    Drop(Vec<crate::mail::RowKey>),
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
            RowOutcome::Drop(rows) => Action::DropOnFolder { account, mailbox, rows },
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
    /// Which account this row belongs to, so a drag from another one is not
    /// offered a drop it cannot perform.
    account: AccountId,
    font: &'a FontId,
    row_height: f32,
    palette: &'a elegance::Palette,
}

/// Whether a folder will take what is being dragged.
///
/// A move is one IMAP session acting on one server, so messages from another
/// account are not droppable here at all — there is no IMAP command for it.
/// A folder that cannot hold messages, or that already holds all of them, is
/// left inert rather than lighting up and then refusing the drop.
fn accepts_drop(
    mailbox: &MailboxInfo,
    account: AccountId,
    payload: &super::DraggedMessages,
) -> bool {
    mailbox.selectable
        && payload.account == account
        && payload.rows.iter().any(|row| row.mailbox != mailbox.name)
}

/// What one mailbox line reported.
struct RowResult {
    outcome: Option<RowOutcome>,
    /// The pointer is over this row holding messages from this account.
    /// True even when the row will not take them, because a folder can be
    /// a route to a child that will.
    carrying: bool,
}

/// Draws one mailbox line.
fn mailbox_row(ui: &mut Ui, input: RowInput<'_>) -> RowResult {
    let RowInput {
        mailbox,
        depth,
        selected,
        has_children,
        collapsed,
        font,
        row_height,
        palette,
        account,
    } = input;

    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return RowResult { outcome: None, carrying: false };
    }

    let mut outcome = None;

    let carried = response.dnd_hover_payload::<super::DraggedMessages>();
    let carrying = carried.as_ref().is_some_and(|payload| payload.account == account);
    let hovering_drop = carried.is_some_and(|payload| accepts_drop(mailbox, account, &payload));
    if let Some(payload) = response.dnd_release_payload::<super::DraggedMessages>()
        && accepts_drop(mailbox, account, &payload)
    {
        outcome = Some(RowOutcome::Drop(payload.rows.clone()));
    }

    let visuals = ui.visuals();
    let painter = ui.painter();

    let background = if hovering_drop {
        super::accent_tint(palette, 0.55)
    } else if selected {
        super::accent_tint(palette, 0.80)
    } else if response.hovered() {
        super::accent_tint(palette, 0.92)
    } else {
        Color32::TRANSPARENT
    };
    if background != Color32::TRANSPARENT {
        painter.rect_filled(rect.shrink2(Vec2::new(2.0, 0.0)), 3.0, background);
    }
    // An outline as well as a fill: on a row that is also the selected one,
    // the fill alone would barely change.
    if hovering_drop {
        painter.rect_stroke(
            rect.shrink2(Vec2::new(2.0, 0.0)),
            3.0,
            egui::Stroke::new(1.0, palette.blue),
            egui::StrokeKind::Inside,
        );
    }

    let unread = mailbox.unseen > 0;
    let accent = palette.blue;
    let color = if selected || unread { visuals.strong_text_color() } else { visuals.text_color() };

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
    let top = metrics.top_for_centred_caps(rect.center().y);
    let shortened = paint_truncated(
        painter,
        pos2(text_left, top),
        right - text_left,
        mailbox.display_name(),
        text_font,
        color,
    );

    super::icons::draw_mailbox(
        painter,
        Rect::from_center_size(
            pos2(icon_left + icon_size * 0.5, metrics.caps_centre(top)),
            Vec2::splat(icon_size),
        ),
        mailbox.special,
        icon_color(mailbox.special, palette),
    );

    if has_children {
        disclosure_arrow(
            painter,
            pos2(indent + arrow_width * 0.5, metrics.caps_centre(top)),
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
        let on_arrow =
            response.interact_pointer_pos().is_some_and(|at| at.x < indent + arrow_width);
        outcome =
            Some(if has_children && on_arrow { RowOutcome::Toggle } else { RowOutcome::Open });
    }

    let menu = elegance::ContextMenu::new(("folder-menu", &mailbox.name)).show(&response, |ui| {
        let mut chosen = None;
        if ui.add(elegance::MenuItem::new("Mark all as read").enabled(mailbox.unseen > 0)).clicked()
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
    });
    if let Some(chosen) = menu.flatten() {
        outcome = Some(chosen);
    }

    // The full path is the only way to tell apart two folders whose leaf
    // names match, which is common once a pane is narrow enough to truncate.
    if shortened || depth > 0 {
        response.on_hover_text(&mailbox.name);
    }
    RowResult { outcome, carrying }
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
        SpecialUse::Drafts => {
            pick(Color32::from_rgb(0xb5, 0x2d, 0x8f), Color32::from_rgb(0xe2, 0x7d, 0xc6))
        }
        // Deliberately not coloured: deleted mail should not draw the eye.
        SpecialUse::Trash => palette.text_faint,
        SpecialUse::Normal | SpecialUse::Archive | SpecialUse::All => {
            pick(Color32::from_rgb(0xc4, 0x92, 0x3d), Color32::from_rgb(0xdc, 0xb9, 0x77))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{RowKey, SpecialUse};

    fn folder(name: &str, selectable: bool) -> MailboxInfo {
        MailboxInfo {
            name: name.to_string(),
            delimiter: Some("/".into()),
            special: SpecialUse::Normal,
            selectable,
            unseen: 0,
        }
    }

    fn dragged(account: AccountId, mailboxes: &[&str]) -> super::super::DraggedMessages {
        super::super::DraggedMessages {
            account,
            rows: mailboxes
                .iter()
                .enumerate()
                .map(|(index, mailbox)| RowKey {
                    mailbox: (*mailbox).to_string(),
                    uid: index as u32 + 1,
                })
                .collect(),
        }
    }

    /// Ubuntu-Light at 14 px, which is what egui bundles. Its baseline sits
    /// almost exactly at the middle of its capitals, which is why centring
    /// the line box happened to work for it.
    fn bundled_14() -> TextMetrics {
        TextMetrics { baseline: 13.0, cap_height: 10.0 }
    }

    /// Candara at 15.64 px, derived from a screenshot of the running
    /// application. Its line box is half as tall again as the bundled face's
    /// at the same size, and hangs far below the capitals — centring that box
    /// pressed the letters against the top of the row.
    fn candara_15_6() -> TextMetrics {
        TextMetrics { baseline: 13.5, cap_height: 10.0 }
    }

    /// Every font has to put the capitals in the middle of the row, not just
    /// the one that happens to be bundled.
    #[test]
    fn the_capitals_sit_in_the_middle_of_the_row() {
        for (name, metrics, size) in
            [("bundled", bundled_14(), 14.0_f32), ("Candara", candara_15_6(), 15.641932)]
        {
            let row_height = row_height(size);
            let centre = row_height * 0.5;
            let top = metrics.top_for_centred_caps(centre);

            let above = top + metrics.baseline - metrics.cap_height;
            let below = row_height - (top + metrics.baseline);
            assert!((above - below).abs() < 0.01, "{name}: {above} above, {below} below");
        }
    }

    #[test]
    fn an_icon_taller_than_the_capitals_is_still_centred_in_the_row() {
        for (name, metrics, size) in
            [("bundled", bundled_14(), 14.0_f32), ("Candara", candara_15_6(), 15.641932)]
        {
            let row_height = row_height(size);
            let centre = row_height * 0.5;
            let top = metrics.top_for_centred_caps(centre);
            let icon = size * 0.96;
            assert!(icon > metrics.cap_height, "{name}: the case worth testing");

            let mark = metrics.caps_centre(top);
            let above = mark - icon * 0.5;
            let below = row_height - (mark + icon * 0.5);
            assert!((above - below).abs() < 0.01, "{name}: {above} above, {below} below");
        }
    }

    #[test]
    fn the_icon_and_the_capitals_share_a_centre() {
        let metrics = candara_15_6();
        let centre = 11.5;
        let top = metrics.top_for_centred_caps(centre);
        assert!((metrics.caps_centre(top) - centre).abs() < 0.01);
    }

    #[test]
    fn a_folder_takes_messages_from_elsewhere_in_its_account() {
        assert!(accepts_drop(&folder("Work", true), 1, &dragged(1, &["INBOX"])));
    }

    #[test]
    fn another_accounts_messages_are_not_droppable() {
        // IMAP has no command for it: a move is one session on one server.
        assert!(!accepts_drop(&folder("Work", true), 1, &dragged(2, &["INBOX"])));
    }

    #[test]
    fn a_container_that_holds_no_messages_takes_none() {
        assert!(!accepts_drop(&folder("[Gmail]", false), 1, &dragged(1, &["INBOX"])));
    }

    #[test]
    fn a_folder_does_not_take_what_it_already_holds() {
        assert!(!accepts_drop(&folder("Work", true), 1, &dragged(1, &["Work"])));
    }

    fn opened(dwell: &Dwell) -> bool {
        matches!(dwell, Dwell::Opened)
    }

    #[test]
    fn a_folder_opens_once_the_pointer_has_rested_long_enough() {
        let mut spring = SpringLoad::default();
        assert!(!opened(&spring.dwell(1, "Work", 0.0)));
        assert!(!opened(&spring.dwell(1, "Work", SpringLoad::DWELL / 2.0)));
        assert!(!spring.is_open(1, "Work"));

        assert!(opened(&spring.dwell(1, "Work", SpringLoad::DWELL)));
        assert!(spring.is_open(1, "Work"));
    }

    #[test]
    fn crossing_a_folder_on_the_way_elsewhere_does_not_open_it() {
        let mut spring = SpringLoad::default();
        spring.dwell(1, "Work", 0.0);
        // The pointer moved on before the dwell elapsed.
        spring.dwell(1, "Archive", 0.1);
        spring.dwell(1, "Archive", 0.1 + SpringLoad::DWELL);

        assert!(!spring.is_open(1, "Work"));
        assert!(spring.is_open(1, "Archive"));
    }

    #[test]
    fn leaving_every_folder_restarts_the_clock() {
        let mut spring = SpringLoad::default();
        spring.dwell(1, "Work", 0.0);
        spring.idle();
        // Coming back starts over rather than resuming.
        spring.dwell(1, "Work", SpringLoad::DWELL);
        assert!(!spring.is_open(1, "Work"));
        assert!(opened(&spring.dwell(1, "Work", SpringLoad::DWELL * 2.0)));
    }

    #[test]
    fn the_same_name_in_another_account_is_a_different_folder() {
        let mut spring = SpringLoad::default();
        spring.dwell(1, "Work", 0.0);
        spring.dwell(2, "Work", 0.1);
        spring.dwell(2, "Work", 0.1 + SpringLoad::DWELL);

        assert!(!spring.is_open(1, "Work"));
        assert!(spring.is_open(2, "Work"));
    }

    #[test]
    fn everything_a_drag_opened_closes_when_it_ends() {
        // The whole point of keeping this out of the config: dragging past a
        // folder is not a decision to leave it open.
        let mut spring = SpringLoad::default();
        spring.dwell(1, "Work", 0.0);
        spring.dwell(1, "Work", SpringLoad::DWELL);
        assert!(spring.is_open(1, "Work"));

        spring.end();
        assert!(!spring.is_open(1, "Work"));
    }

    #[test]
    fn a_mixed_drag_is_taken_for_the_rows_that_can_move() {
        // Search results span folders; the ones already here simply stay.
        assert!(accepts_drop(&folder("Work", true), 1, &dragged(1, &["Work", "INBOX"])));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    /// Writes a picture of the folder rows, for looking at spacing and
    /// alignment without launching the application.
    ///
    ///     REMAIL_RENDER=/tmp/rows.png REMAIL_SIZE=15.6 [REMAIL_DARK=1] \
    ///         cargo test render_sidebar_rows -- --ignored
    #[test]
    #[ignore = "writes a file; run it when you want to look at something"]
    fn render_sidebar_rows() {
        let out = std::env::var("REMAIL_RENDER").unwrap_or_else(|_| "/tmp/rows.png".into());
        let size: f32 =
            std::env::var("REMAIL_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(14.0);
        // REMAIL_DARK=1 draws the pane as the dark-folders option does, for
        // looking at the two side by side.
        let theme = crate::config::ThemeChoice::Outlook.theme();
        let theme = match std::env::var("REMAIL_DARK").is_ok() {
            true => crate::config::dark_pane(&theme).expect("a light theme has a dark pane"),
            false => theme,
        };
        let painted = theme.clone();
        let font = FontId::new(size, egui::FontFamily::Proportional);
        let row_height = row_height(size);
        println!("size {size}, row_height {row_height}");

        let names: [(&str, SpecialUse); 6] = [
            ("Inbox", SpecialUse::Inbox),
            ("Reports", SpecialUse::Normal),
            ("Work", SpecialUse::Normal),
            ("Trash", SpecialUse::Trash),
            ("Drafts", SpecialUse::Drafts),
            ("Engineering", SpecialUse::Normal),
        ];

        crate::ui::raster::render(
            &out,
            &theme,
            190.0,
            6.0 * (row_height + 1.0) + 4.0,
            5.0,
            move |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let first_top = ui.min_rect().top();
                    let mut centres = Vec::new();
                    for (index, (name, special)) in names.iter().enumerate() {
                        let mailbox = MailboxInfo {
                            name: (*name).to_string(),
                            delimiter: Some("/".into()),
                            special: *special,
                            selectable: true,
                            unseen: 0,
                        };
                        mailbox_row(
                            ui,
                            RowInput {
                                mailbox: &mailbox,
                                depth: 0,
                                selected: index % 2 == 0,
                                has_children: false,
                                collapsed: false,
                                account: 1,
                                font: &font,
                                row_height,
                                palette: &painted.palette,
                            },
                        );
                        // Rows are laid out at a fixed pitch, so the centre of
                        // each is arithmetic rather than something to read back.
                        centres
                            .push(first_top + index as f32 * (row_height + 1.0) + row_height * 0.5);
                    }

                    // A line through the middle of every row, so whether the
                    // contents straddle it is a matter of looking rather than
                    // of arithmetic.
                    if std::env::var("REMAIL_GUIDES").is_ok() {
                        for y in centres {
                            ui.painter().hline(
                                ui.min_rect().x_range(),
                                y,
                                egui::Stroke::new(0.2, Color32::from_rgb(220, 0, 0)),
                            );
                        }
                    }
                });
            },
        );
    }
}
