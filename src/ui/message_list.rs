//! Middle pane: the message list.
//!
//! Rows are painted directly rather than composed from widgets, and only the
//! visible range is touched each frame. A mailbox with fifty thousand cached
//! messages costs the same to scroll as one with fifty.

use std::collections::BTreeSet;
use std::ops::Range;

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, Vec2, pos2};

use super::{Action, DraggedMessages, format_date_short, paint_truncated};
use crate::config::AccountId;
use crate::mail::{Envelope, Flags, RowKey};

pub struct ListInput<'a> {
    pub account: AccountId,
    pub envelopes: &'a [Envelope],
    /// The keyboard-focused message.
    pub cursor: Option<RowKey>,
    pub selection: &'a BTreeSet<RowKey>,
    /// Show each row's folder. Set when the view spans mailboxes, where the
    /// subject alone does not say where a message lives.
    pub show_folder: bool,
    /// Mailboxes holding mail the user sent. A row from one of these shows
    /// who the message went to, since the sender is the account itself and
    /// says nothing. Tested per row rather than per pane because a search
    /// spans folders, and a result from Sent wants the same treatment there.
    pub outgoing: &'a BTreeSet<String>,
    pub theme: &'a elegance::Theme,
    pub compact: bool,
    /// Font this pane draws in.
    pub font: FontId,
    /// This pane's background, which the stripe is derived from so the two
    /// cannot drift apart.
    pub surface: Color32,
    /// Scroll so the cursor is visible; set after a keyboard move.
    pub scroll_to_cursor: bool,
    /// The rows on screen are about to be replaced by a search that has not
    /// come back yet. Drawn faded, because until it does they are the local
    /// filter's answer rather than the one that was asked for.
    pub stale: bool,
    pub empty_message: &'a str,
}

/// Vertical geometry of a row, in points, derived from the pane's text size.
///
/// Three lines of text need more than three line-heights: without leading
/// above and below, and between the lines, the row reads as a block rather
/// than as a sender, a subject and a preview.
#[derive(Clone, Copy)]
struct RowMetrics {
    height: f32,
    /// Baselines of the three text lines, relative to the row top.
    sender_y: f32,
    subject_y: f32,
    preview_y: f32,
}

impl RowMetrics {
    fn new(size: f32, compact: bool) -> Self {
        if compact {
            let padding = size * 0.42;
            return Self {
                height: (size + padding * 2.0).round(),
                sender_y: padding,
                subject_y: padding,
                preview_y: padding,
            };
        }

        // Leading between lines, and the same again above and below, so rows
        // are separated by as much space as the lines within one.
        let leading = size * 0.42;
        let padding = size * 0.5;
        let line = size * 1.05;

        Self {
            height: (padding * 2.0 + line * 2.0 + size * 0.86 + leading * 2.0).round(),
            sender_y: padding,
            subject_y: padding + line + leading,
            preview_y: padding + (line + leading) * 2.0,
        }
    }
}

/// What the list drew, so the app can prefetch what the user is looking at.
pub struct ListOutput {
    pub action: Option<Action>,
    /// A context-menu choice, applied after `action` so that a right-click on
    /// an unselected row moves the cursor there first.
    pub pending: Option<Action>,
    pub visible: Range<usize>,
}

pub fn show(ui: &mut Ui, input: ListInput<'_>) -> ListOutput {
    let mut action = None;
    // A menu choice has to land after the focus change it may depend on, so
    // it is held back rather than overwriting `action`.
    let mut pending = None;
    // Set on the frame a drag begins, so the preview can be drawn once the
    // row loop has finished rather than inside it.
    let mut dragging: Option<Vec<RowKey>> = None;

    if input.envelopes.is_empty() {
        ui.add_space(32.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(input.empty_message).color(ui.visuals().weak_text_color()),
            );
        });
        return ListOutput { action: None, pending: None, visible: 0..0 };
    }

    let metrics = RowMetrics::new(input.font.size, input.compact);
    let row_height = metrics.height;

    let cursor_index =
        input.cursor.as_ref().and_then(|key| input.envelopes.iter().position(|e| &e.key() == key));

    let mut visible = 0..0;
    let scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    scroll.show_rows(ui, row_height, input.envelopes.len(), |ui, range| {
        visible = range.clone();
        ui.set_width(ui.available_width());
        // Faded as a whole rather than colour by colour: the stripes, the
        // accents and the markers all have to recede together, or the rows
        // stop looking like the same rows.
        if input.stale {
            ui.multiply_opacity(0.45);
        }

        for index in range {
            let envelope = &input.envelopes[index];
            let key = envelope.key();
            let is_cursor = input.cursor.as_ref() == Some(&key);
            let is_selected = input.selection.contains(&key);

            let (rect, response) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), row_height),
                Sense::click_and_drag(),
            );

            // Dragging a row that is part of the selection takes the whole
            // selection; dragging one outside it takes just that row, which
            // is what the pointer is visibly on.
            if response.drag_started() {
                let rows = if input.selection.contains(&key) {
                    input.selection.iter().cloned().collect()
                } else {
                    vec![key.clone()]
                };
                dragging = Some(rows.clone());
                response.dnd_set_drag_payload(DraggedMessages { account: input.account, rows });
            }

            // The star and the trashcan are hit areas laid over the row, and
            // whichever the pointer is on takes the hover from underneath it.
            // Asking where the pointer is instead keeps the row lit, and the
            // trashcan on it, while it is being aimed at.
            let hovered = response.hovered() || ui.rect_contains_pointer(rect);

            if ui.is_rect_visible(rect) {
                draw_row(
                    ui,
                    rect,
                    envelope,
                    RowState { index, is_cursor, is_selected, hovered },
                    &input,
                    metrics,
                );
            }

            let star_rect = star_rect(rect, metrics);
            // Right-click acts on the selection when the row is part of it,
            // and on the row alone otherwise — so the menu never silently
            // operates on something other than what was clicked.
            let menu = elegance::ContextMenu::new(("message-menu", &key)).show(&response, |ui| {
                let mut chosen = None;
                if ui.add(elegance::MenuItem::new("Move to\u{2026}")).clicked() {
                    chosen = Some(Action::MoveTo);
                }
                if ui.add(elegance::MenuItem::new("Archive")).clicked() {
                    chosen = Some(Action::Archive);
                }
                ui.separator();
                if ui.add(elegance::MenuItem::new("Delete")).clicked() {
                    chosen = Some(Action::Delete);
                }
                chosen
            });
            if let Some(Some(chosen)) = menu {
                if !input.selection.contains(&key) {
                    action = Some(Action::Focus(key.clone()));
                }
                pending = Some(chosen);
            }

            let trash = hovered.then(|| {
                ui.interact(
                    trash_rect(rect, metrics, input.compact),
                    ui.id().with(("trash", envelope.uid)),
                    Sense::click(),
                )
            });

            let star = ui.interact(star_rect, ui.id().with(("star", envelope.uid)), Sense::click());
            if trash.is_some_and(|trash| trash.clicked()) {
                // Delete acts on the selection, so a row outside it is
                // focused first — the same order a right-click takes, and
                // for the same reason: the click must not act on something
                // other than the row it landed on.
                if !input.selection.contains(&key) {
                    action = Some(Action::Focus(key.clone()));
                }
                pending = Some(Action::Delete);
            } else if star.clicked() {
                action = Some(Action::ToggleStar(key.clone()));
            } else if response.clicked() {
                let modifiers = ui.input(|i| i.modifiers);
                action = Some(if modifiers.shift {
                    Action::SelectRange(key.clone())
                } else if modifiers.command {
                    Action::ToggleSelected(key.clone())
                } else {
                    Action::Focus(key.clone())
                });
            }

            if input.scroll_to_cursor && Some(index) == cursor_index {
                ui.scroll_to_rect(rect, None);
            }
        }
    });

    // The payload outlives the frame the drag started on, so the preview
    // follows the pointer wherever it goes — including over the sidebar,
    // which is the whole point.
    let carried = dragging.or_else(|| {
        egui::DragAndDrop::payload::<DraggedMessages>(ui.ctx()).map(|payload| payload.rows.clone())
    });
    if let Some(rows) = carried {
        drag_preview(ui, &rows, &input, metrics);
    }

    ListOutput { action, pending, visible }
}

/// Paints what is being dragged, following the pointer.
///
/// A translucent copy of the rows themselves rather than a generic badge:
/// the thing being carried should look like the thing that was picked up.
fn drag_preview(ui: &Ui, rows: &[RowKey], input: &ListInput<'_>, metrics: RowMetrics) {
    let Some(pointer) = ui.ctx().pointer_interact_pos() else { return };

    // At most three, offset like a stack of paper. Beyond that the count
    // says more than another identical row would.
    const STACK: usize = 3;
    let carried: Vec<&Envelope> = rows
        .iter()
        .filter_map(|row| input.envelopes.iter().find(|e| &e.key() == row))
        .take(STACK)
        .collect();
    if carried.is_empty() {
        return;
    }

    let width = 320.0_f32.min(ui.available_width().max(220.0));
    let offset = 4.0;
    let depth = (carried.len() - 1) as f32 * offset;

    // Below and right of the pointer, so the cursor is not sitting on top of
    // the thing it is carrying.
    let origin = pointer + egui::vec2(12.0, 8.0);

    egui::Area::new(egui::Id::new("message-drag-preview"))
        .order(egui::Order::Tooltip)
        .fixed_pos(origin)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            ui.set_opacity(0.72);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(width, metrics.height + depth), Sense::hover());

            // Back to front, so the first row of the selection ends on top.
            for (depth_index, envelope) in carried.iter().enumerate().rev() {
                let shift = depth_index as f32 * offset;
                let row = Rect::from_min_size(
                    rect.min + Vec2::new(shift, shift),
                    Vec2::new(width, metrics.height),
                );
                ui.painter().rect_filled(row, 4.0, input.surface);
                draw_row(
                    ui,
                    row,
                    envelope,
                    RowState { index: 0, is_cursor: false, is_selected: true, hovered: false },
                    input,
                    metrics,
                );
            }

            if rows.len() > 1 {
                count_badge(ui, rect, rows.len(), input);
            }
        });
}

/// How many messages are being carried, when it is more than one.
fn count_badge(ui: &Ui, rect: Rect, count: usize, input: &ListInput<'_>) {
    let palette = &input.theme.palette;
    let font = FontId::new(input.font.size * 0.9, input.font.family.clone());
    let label = count.to_string();

    let galley = ui.painter().layout_no_wrap(label, font, Color32::WHITE);
    let padding = Vec2::new(7.0, 3.0);
    let badge =
        Rect::from_min_size(rect.left_top() - Vec2::new(6.0, 6.0), galley.size() + padding * 2.0);

    ui.painter().rect_filled(badge, badge.height() * 0.5, palette.blue);
    ui.painter().galley(badge.min + padding, galley, Color32::WHITE);
}

/// Where a row sits in the list and how the pointer and selection see it.
struct RowState {
    /// Absolute index, so the stripe pattern does not shift while scrolling.
    index: usize,
    is_cursor: bool,
    is_selected: bool,
    hovered: bool,
}

fn draw_row(
    ui: &Ui,
    rect: Rect,
    envelope: &Envelope,
    state: RowState,
    input: &ListInput<'_>,
    metrics: RowMetrics,
) {
    let RowState { index, is_cursor, is_selected, hovered } = state;
    let visuals = ui.visuals();
    let painter = ui.painter();
    let family = input.font.family.clone();
    let font = |scale: f32| FontId::new(scale, family.clone());

    // Stripes go down first so selection and hover read as states layered on
    // top of them rather than as another stripe colour. The index is the
    // absolute row, so the pattern does not shift while scrolling.
    //
    // Derived from this pane's own fill rather than `faint_bg_color`, which
    // is not guaranteed to differ from the panel colour the app sets.
    let palette = &input.theme.palette;
    if index % 2 == 1 {
        painter.rect_filled(rect, 0.0, palette.depth_tint(input.surface, 0.022));
    }

    let background = if is_cursor {
        super::accent_tint(palette, 0.78)
    } else if is_selected {
        super::accent_tint(palette, 0.88)
    } else if hovered {
        super::accent_tint(palette, 0.94)
    } else {
        Color32::TRANSPARENT
    };
    if background != Color32::TRANSPARENT {
        painter.rect_filled(rect.shrink2(Vec2::new(2.0, 1.0)), 4.0, background);
    }

    painter.hline(
        rect.x_range(),
        rect.bottom(),
        Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.5)),
    );

    let unread = envelope.flags.is_unread();
    let strong = visuals.strong_text_color();
    let weak = visuals.weak_text_color();

    // Subjects carry the accent. A read one recedes towards body text so the
    // colour still marks the column without shouting on every row. Both are
    // then deepened: the accent at full strength is a mid-tone, which on a
    // white list reads as lighter than the sender above it and lets the
    // subject — the line actually being scanned — sit back from the row.
    let subject_color =
        if unread { palette.blue } else { super::mix(palette.blue, palette.text_muted, 0.4) };
    let subject_color = super::deepen(subject_color, palette, 0.25);

    let size = input.font.size;
    // A bar down the whole row, not a dot beside one line: at a glance the
    // eye picks up the run of unread messages, not six separate marks.
    if unread {
        let bar = Rect::from_min_max(
            pos2(rect.left() + 3.0, rect.top() + 4.0),
            pos2(rect.left() + 6.5, rect.bottom() - 4.0),
        );
        painter.rect_filled(bar, 1.75, palette.blue);
    }

    let left = rect.left() + 12.0;
    let right = rect.right() - marker_strip(input.compact);
    let text_left = left;

    let date = format_date_short(envelope.date);
    let date_width = if date.is_empty() {
        0.0
    } else {
        let galley = painter.layout_no_wrap(date.clone(), font(size * 0.82), weak);
        let width = galley.size().x;
        painter.galley(
            pos2(right - width, rect.top() + metrics.sender_y + size * 0.1),
            galley,
            weak,
        );
        width + 10.0
    };

    // The attachment marker shares a line with text in both densities — the
    // preview on a full row, the subject and the date on a compact one — so
    // it is measured before either is laid out. Drawn without reserving the
    // width, it simply went on top: over the preview on a full row, over the
    // clock on a compact one.
    let clip_font = font(super::icons::size_beside_text(size) * 0.82);
    let clip_width = if envelope.has_attachments {
        let galley =
            painter.layout_no_wrap(super::icons::ATTACHMENT.to_string(), clip_font.clone(), weak);
        galley.size().x + 6.0
    } else {
        0.0
    };

    let sender = if envelope.is_outgoing(input.outgoing) {
        recipients(envelope)
    } else {
        envelope
            .from
            .first()
            .map(|a| a.short().to_string())
            .unwrap_or_else(|| "(unknown sender)".to_string())
    };

    let first_line_width = (right - date_width - text_left).max(40.0);
    // A compact row puts the subject on this same line, so the sender gets
    // its column and not the width of the line.
    let sender_width = if input.compact {
        (first_line_width * COMPACT_SENDER_SHARE - 8.0).max(30.0)
    } else {
        first_line_width
    };
    let _ = paint_truncated(
        painter,
        pos2(text_left, rect.top() + metrics.sender_y),
        sender_width,
        &sender,
        font(size * 0.95),
        // egui has one weight per family, so "strong" is a colour, not a
        // heavier face. Senders take it whether or not they are unread.
        strong,
    );

    if input.compact {
        // One line: sender, then subject sharing the row. The folder chip
        // ends that run, before the marker and the clock, in width taken out
        // of the subject rather than out of nothing.
        let subject_left = text_left + first_line_width * COMPACT_SENDER_SHARE;
        let chip = input
            .show_folder
            .then(|| FolderChip::new(painter, envelope.folder_label(), font(size * 0.72), palette))
            .flatten();
        let reserved = clip_width + FolderChip::reserved(&chip);

        let _ = paint_truncated(
            painter,
            pos2(subject_left, rect.top() + metrics.subject_y),
            compact_subject_width(right, subject_left, date_width, reserved),
            display_subject(envelope),
            font(size * 0.95),
            subject_color,
        );
        if let Some(chip) = chip {
            let x = right - date_width - clip_width - chip.size.x;
            chip.paint(painter, pos2(x, rect.top() + metrics.subject_y - 1.5), palette);
        }
    } else {
        let _ = paint_truncated(
            painter,
            pos2(text_left, rect.top() + metrics.subject_y),
            right - text_left,
            display_subject(envelope),
            font(size * 0.95),
            subject_color,
        );

        let mut preview_left = text_left;
        if let Some(chip) = input
            .show_folder
            .then(|| FolderChip::new(painter, envelope.folder_label(), font(size * 0.72), palette))
            .flatten()
        {
            let width = chip.size.x;
            chip.paint(painter, pos2(preview_left, rect.top() + metrics.preview_y - 1.5), palette);
            preview_left += width + FolderChip::GAP;
        }

        if !envelope.preview.is_empty() {
            // Body text, at full strength. This line is what says whether a
            // message is worth opening, and every shade quieter than the
            // text colour was a shade of working to read it. Size carries
            // the hierarchy on its own: the synopsis is set well below the
            // subject, which keeps the accent, so nothing here competes.
            let _ = paint_truncated(
                painter,
                pos2(preview_left, rect.top() + metrics.preview_y),
                preview_width(right, preview_left, clip_width),
                &envelope.preview,
                font(size * 0.82),
                palette.text,
            );
        }
    }

    if envelope.has_attachments {
        // A full row has a preview line to end with. A compact row has only
        // the one line, so the marker goes to the left of the date rather
        // than on top of it.
        let (at, anchor) = if input.compact {
            (
                pos2(right - date_width, rect.top() + metrics.subject_y + size * 0.1),
                Align2::RIGHT_TOP,
            )
        } else {
            (pos2(right - 2.0, rect.bottom() - 8.0), Align2::RIGHT_BOTTOM)
        };
        // The font undoes the bundled emoji font's shrink, then steps back
        // down: a secondary marker, but not a speck.
        painter.text(at, anchor, super::icons::ATTACHMENT, clip_font, weak);
    }

    let starred = envelope.flags.has(Flags::FLAGGED);
    painter.text(
        pos2(rect.right() - 18.0, rect.top() + metrics.sender_y),
        Align2::CENTER_TOP,
        if starred { super::icons::STAR_FILLED } else { super::icons::STAR_HOLLOW },
        font(size * 0.95),
        if starred { Color32::from_rgb(230, 180, 60) } else { weak.gamma_multiply(0.6) },
    );

    // Only under the pointer: a row the user is not on has nothing to say
    // about deleting it, and a trashcan on every row would be a column of
    // them down the pane.
    if hovered {
        painter.text(
            trash_rect(rect, metrics, input.compact).center(),
            Align2::CENTER_CENTER,
            super::icons::TRASH,
            font(super::icons::size_beside_text(size) * 0.82),
            weak,
        );
    }
}

/// The pill naming the folder a message lives in, for a listing that spans
/// more than one.
///
/// A filled chip rather than tinted text: this is the answer to "where did
/// this come from", so it should not read as part of the line running
/// alongside it. Laid out before the text it sits next to, because both
/// densities have to take its width out of that text rather than let it be
/// drawn over — or, as the compact row did, append it to a string that is
/// then ellipsized and lose it on every subject long enough to matter.
struct FolderChip {
    galley: std::sync::Arc<egui::Galley>,
    size: Vec2,
}

impl FolderChip {
    const PADDING: Vec2 = Vec2::new(5.0, 1.5);
    /// Space between the chip and whatever text it sits beside.
    const GAP: f32 = 7.0;

    fn new(
        painter: &egui::Painter,
        folder: &str,
        font: FontId,
        palette: &elegance::Palette,
    ) -> Option<Self> {
        if folder.is_empty() {
            return None;
        }
        let galley = painter.layout_no_wrap(folder.to_string(), font, palette.blue);
        let size = galley.size() + Self::PADDING * 2.0;
        Some(Self { galley, size })
    }

    /// What the chip costs the text beside it: itself, and a gap.
    fn reserved(chip: &Option<Self>) -> f32 {
        chip.as_ref().map_or(0.0, |chip| chip.size.x + Self::GAP)
    }

    fn paint(self, painter: &egui::Painter, min: egui::Pos2, palette: &elegance::Palette) {
        let chip = Rect::from_min_size(min, self.size);
        painter.rect_filled(chip, 3.0, super::accent_tint(palette, 0.86));
        painter.galley(chip.min + Self::PADDING, self.galley, palette.blue);
    }
}

/// Width the subject gets on a compact row: the line, less the date, less the
/// attachment marker when there is one.
fn compact_subject_width(right: f32, subject_left: f32, date_width: f32, clip: f32) -> f32 {
    (right - date_width - clip - subject_left).max(40.0)
}

/// Width the preview gets on a full row, less the attachment marker that ends
/// the same line.
fn preview_width(right: f32, preview_left: f32, clip: f32) -> f32 {
    (right - preview_left - clip).max(0.0)
}

/// Share of the first line a compact row gives the sender before the subject
/// starts. The sender is held to it and the subject begins at it, from this
/// one number, because the two used to be written out separately and the
/// sender was given the whole line — so any name longer than its column was
/// printed straight over the subject.
const COMPACT_SENDER_SHARE: f32 = 0.32;

/// Width of the strip down the right of a row that the text stops short of.
///
/// Reserved whether or not the trashcan is showing, so a row does not reflow
/// under the pointer. A full row stacks the star above the trashcan and needs
/// the width of one; a compact row is a single line, so they sit side by side
/// and it needs the width of both.
fn marker_strip(compact: bool) -> f32 {
    if compact { 60.0 } else { 38.0 }
}

/// Hit area for the star, at the top of the strip.
///
/// Kept inside the row. A compact row at a small text size is shorter than
/// the 22pt this wants, and the overhang belonged to the row underneath: a
/// click at the top of one row toggled the star of the row above it.
fn star_rect(rect: Rect, metrics: RowMetrics) -> Rect {
    const SIDE: f32 = 22.0;
    let side = SIDE.min(rect.height());
    let top = (rect.top() + metrics.sender_y - 2.0).clamp(rect.top(), rect.bottom() - side);
    Rect::from_min_size(pos2(rect.right() - 30.0, top), Vec2::splat(side))
}

/// Hit area for the trashcan, at the bottom of the strip.
///
/// Only offered on a full row: a compact one is a single line tall, and the
/// strip beside it is already the star's.
///
/// Held below the star rather than simply placed near the bottom. At a small
/// text size the row is short enough that two 22pt areas at opposite ends of
/// it still meet in the middle, and two hit areas sharing a pixel send the
/// click to whichever was asked for it first — which would have been the
/// star, silently, on the rows where it happened.
fn trash_rect(rect: Rect, metrics: RowMetrics, compact: bool) -> Rect {
    const SIDE: f32 = 22.0;
    let star = star_rect(rect, metrics);
    if compact {
        // Beside the star rather than below it, level with the one line the
        // row has. A shared edge is still a shared pixel, so it stops short
        // of the star by one.
        return Rect::from_min_size(
            pos2(star.left() - SIDE - 1.0, star.top()),
            Vec2::new(SIDE, star.height()),
        );
    }
    // A shared edge is still a shared pixel, so the gap is a real one.
    let top = (rect.bottom() - SIDE - 4.0).max(star.bottom() + 1.0);
    let height = (rect.bottom() - top).clamp(0.0, SIDE);
    Rect::from_min_size(pos2(rect.right() - 29.0, top), Vec2::new(SIDE, height))
}

/// Who a sent message went to, for the line that carries the sender
/// everywhere else.
///
/// One name and a count, rather than as many as happen to fit: the column is
/// narrow, and a list of names truncated mid-word says less than a name and
/// the number of people beside it.
fn recipients(envelope: &Envelope) -> String {
    let mut addrs = envelope.to.iter().chain(envelope.cc.iter());
    let Some(first) = addrs.next() else { return "(no recipients)".to_string() };
    match addrs.count() {
        0 => first.short().to_string(),
        rest => format!("{}, +{rest}", first.short()),
    }
}

fn display_subject(envelope: &Envelope) -> &str {
    if envelope.subject.trim().is_empty() { "(no subject)" } else { &envelope.subject }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::mail::Addr;

    fn addr(name: &str, email: &str) -> Addr {
        Addr { name: name.to_string(), email: email.to_string() }
    }

    #[test]
    fn a_sent_message_is_summarised_by_who_it_went_to() {
        let one = Envelope {
            to: vec![addr("Walter Brill", "wbrill6@example.com")],
            ..Default::default()
        };
        assert_eq!(recipients(&one), "Walter Brill");

        // An address with no display name still has to name someone.
        let bare = Envelope { to: vec![addr("", "karen@example.com")], ..Default::default() };
        assert_eq!(recipients(&bare), "karen@example.com");
    }

    #[test]
    fn the_rest_of_the_recipients_are_counted_not_listed() {
        let many = Envelope {
            to: vec![addr("Michele", "m@example.com"), addr("Lance Wolin", "lance@example.net")],
            cc: vec![addr("", "andrew@example.org")],
            ..Default::default()
        };
        // Two more beyond the first, counting Cc: everyone who got it.
        assert_eq!(recipients(&many), "Michele, +2");
    }

    #[test]
    fn a_sent_message_with_no_recipients_says_so() {
        assert_eq!(recipients(&Envelope::default()), "(no recipients)");
    }

    /// The two markers share the strip down the right of a row, one at each
    /// end of it. They must not share any of it with each other — a click
    /// meant for one would land on whichever was asked first — nor reach
    /// back into the text.
    #[test]
    fn the_star_and_the_trashcan_keep_out_of_each_other() {
        for compact in [false, true] {
            for size in [10.0_f32, 14.0, 16.4, 26.0] {
                let metrics = RowMetrics::new(size, compact);
                let row = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(430.0, metrics.height));
                let star = star_rect(row, metrics);
                let trash = trash_rect(row, metrics, compact);
                let what = format!("size {size}, compact {compact}");
                let text_ends = row.right() - marker_strip(compact);

                assert!(
                    !star.intersects(trash),
                    "{what}: the star {star:?} and the trashcan {trash:?} overlap"
                );
                assert!(row.contains_rect(trash), "{what}: the trashcan leaves the row");
                assert!(row.contains_rect(star), "{what}: the star leaves the row");
                assert!(
                    trash.left() >= text_ends,
                    "{what}: the trashcan reaches into the text column"
                );
                assert!(star.left() >= text_ends, "{what}: the star reaches into the text column");
            }
        }
    }

    /// The folder chip is laid out before the subject and its width taken
    /// out of it. Appended to the subject instead, as it was, it went through
    /// the same ellipsis and vanished on every subject long enough to need
    /// one — which on a compact row is most of them.
    #[test]
    fn the_folder_chip_is_paid_for_out_of_the_compact_subject() {
        let theme = crate::config::ThemeChoice::Outlook.theme();
        let (reserved, none) = crate::ui::raster::measure(&theme, Vec2::new(400.0, 80.0), |ui| {
            let font = FontId::new(12.0, egui::FontFamily::Proportional);
            let chip = FolderChip::new(ui.painter(), "All Mail", font.clone(), &theme.palette);
            let empty = FolderChip::new(ui.painter(), "", font, &theme.palette);
            (FolderChip::reserved(&chip), FolderChip::reserved(&empty))
        });

        assert!(reserved > 0.0, "a chip that costs nothing was never laid out");
        assert_eq!(none, 0.0, "a row with no folder to name still paid for one");

        let full = compact_subject_width(400.0, 60.0, 70.0, 0.0);
        let beside_chip = compact_subject_width(400.0, 60.0, 70.0, reserved);
        assert!(beside_chip < full, "the subject kept its width beside the chip");
        assert!(
            (full - beside_chip - reserved).abs() < 0.01,
            "the subject gave up {:.1}pt for a chip that needs {reserved:.1}",
            full - beside_chip
        );
    }

    /// Whatever shares a line with the attachment marker has to be given
    /// less room for it. Both of these once took the full span and the
    /// marker was drawn over the end of them — over the preview on a full
    /// row, over the clock on a compact one.
    #[test]
    fn the_attachment_marker_is_paid_for_out_of_the_text() {
        const RIGHT: f32 = 400.0;
        const CLIP: f32 = 18.0;

        let without = compact_subject_width(RIGHT, 60.0, 70.0, 0.0);
        let with = compact_subject_width(RIGHT, 60.0, 70.0, CLIP);
        assert!(with < without, "a compact subject kept its width beside the marker");
        assert!((without - with - CLIP).abs() < 0.01, "it did not give up the marker's width");

        let without = preview_width(RIGHT, 12.0, 0.0);
        let with = preview_width(RIGHT, 12.0, CLIP);
        assert!(with < without, "a preview kept its width beside the marker");
        assert!((without - with - CLIP).abs() < 0.01, "it did not give up the marker's width");

        // A narrow pane must not hand out a negative width.
        assert!(preview_width(40.0, 30.0, CLIP) >= 0.0);
        assert!(compact_subject_width(40.0, 30.0, 70.0, CLIP) >= 0.0);
    }

    /// A compact row puts the sender and the subject on one line. The sender
    /// is held to its column so the two cannot be written over each other,
    /// which is what a sender given the width of the whole line did.
    #[test]
    fn a_compact_sender_stops_where_the_subject_starts() {
        let line = 300.0_f32;
        let sender_width = (line * COMPACT_SENDER_SHARE - 8.0).max(30.0);
        let subject_left = line * COMPACT_SENDER_SHARE;
        assert!(
            sender_width <= subject_left,
            "the sender runs {:.1}pt past where the subject begins",
            sender_width - subject_left
        );
    }

    #[test]
    fn rows_leave_room_for_every_line() {
        for size in [10.0_f32, 14.0, 20.0, 26.0] {
            let m = RowMetrics::new(size, false);
            assert!(m.sender_y > 0.0, "no padding above the first line");
            assert!(m.subject_y > m.sender_y + size, "sender and subject overlap");
            assert!(m.preview_y > m.subject_y + size, "subject and preview overlap");
            assert!(m.height >= m.preview_y + size, "preview is clipped at size {size}");
        }
    }

    #[test]
    fn rows_grow_with_the_text() {
        let small = RowMetrics::new(11.0, false);
        let large = RowMetrics::new(22.0, false);
        assert!(large.height > small.height * 1.8, "height did not track size");
    }

    #[test]
    fn a_compact_row_holds_one_centred_line() {
        let m = RowMetrics::new(14.0, true);
        assert_eq!(m.sender_y, m.subject_y);
        assert!(m.height >= m.sender_y + 14.0);
        assert!(m.height < RowMetrics::new(14.0, false).height);
    }
}
