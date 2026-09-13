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
    pub theme: &'a elegance::Theme,
    pub compact: bool,
    /// Font this pane draws in.
    pub font: FontId,
    /// This pane's background, which the stripe is derived from so the two
    /// cannot drift apart.
    pub surface: Color32,
    /// Scroll so the cursor is visible; set after a keyboard move.
    pub scroll_to_cursor: bool,
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

            if ui.is_rect_visible(rect) {
                draw_row(
                    ui,
                    rect,
                    envelope,
                    RowState { index, is_cursor, is_selected, hovered: response.hovered() },
                    &input,
                    metrics,
                );
            }

            // The star sits in its own hit area at the right edge.
            let star_rect = Rect::from_min_size(
                pos2(rect.right() - 30.0, rect.top() + metrics.sender_y - 2.0),
                Vec2::splat(22.0),
            );
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

            let star = ui.interact(star_rect, ui.id().with(("star", envelope.uid)), Sense::click());
            if star.clicked() {
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
    // colour still marks the column without shouting on every row.
    let subject_color =
        if unread { palette.blue } else { super::mix(palette.blue, palette.text_muted, 0.4) };

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
    let right = rect.right() - 38.0;
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

    let sender = envelope
        .from
        .first()
        .map(|a| a.short().to_string())
        .unwrap_or_else(|| "(unknown sender)".to_string());

    let first_line_width = (right - date_width - text_left).max(40.0);
    let _ = paint_truncated(
        painter,
        pos2(text_left, rect.top() + metrics.sender_y),
        first_line_width,
        &sender,
        font(size * 0.95),
        // egui has one weight per family, so "strong" is a colour, not a
        // heavier face. Senders take it whether or not they are unread.
        strong,
    );

    if input.compact {
        // One line: sender, then subject sharing the row. There is no preview
        // line to hang a folder chip from, so the folder is appended to the
        // subject instead.
        let subject_left = text_left + first_line_width * 0.32;
        let _ = paint_truncated(
            painter,
            pos2(subject_left, rect.top() + metrics.subject_y),
            (right - date_width - subject_left).max(40.0),
            &compact_subject(envelope, input.show_folder),
            font(size * 0.95),
            subject_color,
        );
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
        let folder = envelope.folder_label();
        if input.show_folder && !folder.is_empty() {
            // A filled chip rather than tinted text: this is the answer to
            // "where did this come from", so it should not read as part of
            // the preview line running alongside it.
            let galley =
                painter.layout_no_wrap(folder.to_string(), font(size * 0.72), palette.blue);
            let padding = Vec2::new(5.0, 1.5);
            let chip = Rect::from_min_size(
                pos2(preview_left, rect.top() + metrics.preview_y - 1.5),
                galley.size() + padding * 2.0,
            );
            painter.rect_filled(chip, 3.0, super::accent_tint(palette, 0.86));
            painter.galley(chip.min + padding, galley, palette.blue);
            preview_left = chip.right() + 7.0;
        }

        if !envelope.preview.is_empty() {
            let _ = paint_truncated(
                painter,
                pos2(preview_left, rect.top() + metrics.preview_y),
                right - preview_left,
                &envelope.preview,
                font(size * 0.82),
                weak,
            );
        }
    }

    if envelope.has_attachments {
        painter.text(
            pos2(right - 4.0, rect.bottom() - 8.0),
            Align2::RIGHT_BOTTOM,
            super::icons::ATTACHMENT,
            // Undo the bundled emoji font's shrink, then step back down: a
            // secondary marker, but not a speck.
            font(super::icons::size_beside_text(size) * 0.82),
            weak,
        );
    }

    let starred = envelope.flags.has(Flags::FLAGGED);
    painter.text(
        pos2(rect.right() - 18.0, rect.top() + metrics.sender_y),
        Align2::CENTER_TOP,
        if starred { super::icons::STAR_FILLED } else { super::icons::STAR_HOLLOW },
        font(size * 0.95),
        if starred { Color32::from_rgb(230, 180, 60) } else { weak.gamma_multiply(0.6) },
    );
}

/// Subject for a compact row, with the folder appended while searching.
fn compact_subject(envelope: &Envelope, show_folder: bool) -> String {
    let subject = display_subject(envelope);
    let folder = envelope.folder_label();
    if show_folder && !folder.is_empty() {
        format!("{subject}  \u{2014} {folder}")
    } else {
        subject.to_string()
    }
}

fn display_subject(envelope: &Envelope) -> &str {
    if envelope.subject.trim().is_empty() { "(no subject)" } else { &envelope.subject }
}

#[cfg(test)]
mod tests {
    use super::*;

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
