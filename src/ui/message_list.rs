//! Middle pane: the message list.
//!
//! Rows are painted directly rather than composed from widgets, and only the
//! visible range is touched each frame. A mailbox with fifty thousand cached
//! messages costs the same to scroll as one with fifty.

use std::collections::BTreeSet;
use std::ops::Range;

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, Vec2, pos2};

use super::{Action, format_date_short, paint_truncated};
use crate::mail::{Envelope, Flags};

pub struct ListInput<'a> {
    pub envelopes: &'a [Envelope],
    /// The keyboard-focused message.
    pub cursor: Option<u32>,
    pub selection: &'a BTreeSet<u32>,
    pub compact: bool,
    pub base_size: f32,
    /// Font family this pane draws in.
    pub family: egui::FontFamily,
    /// Scroll so the cursor is visible; set after a keyboard move.
    pub scroll_to_cursor: bool,
    pub empty_message: &'a str,
}

/// What the list drew, so the app can prefetch what the user is looking at.
pub struct ListOutput {
    pub action: Option<Action>,
    pub visible: Range<usize>,
}

pub fn show(ui: &mut Ui, input: ListInput<'_>) -> ListOutput {
    let mut action = None;

    if input.envelopes.is_empty() {
        ui.add_space(32.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(input.empty_message)
                    .color(ui.visuals().weak_text_color()),
            );
        });
        return ListOutput { action: None, visible: 0..0 };
    }

    let row_height = if input.compact {
        input.base_size * 2.0
    } else {
        input.base_size * 3.6
    };

    let cursor_index = input
        .cursor
        .and_then(|uid| input.envelopes.iter().position(|e| e.uid == uid));

    let mut visible = 0..0;
    let scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    scroll.show_rows(ui, row_height, input.envelopes.len(), |ui, range| {
        visible = range.clone();
        ui.set_width(ui.available_width());

        for index in range {
            let envelope = &input.envelopes[index];
            let is_cursor = input.cursor == Some(envelope.uid);
            let is_selected = input.selection.contains(&envelope.uid);

            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());

            if ui.is_rect_visible(rect) {
                draw_row(ui, rect, envelope, is_cursor, is_selected, &response, &input);
            }

            // The star sits in its own hit area at the right edge.
            let star_rect = Rect::from_min_size(
                pos2(rect.right() - 26.0, rect.top() + 6.0),
                Vec2::splat(20.0),
            );
            let star = ui.interact(
                star_rect,
                ui.id().with(("star", envelope.uid)),
                Sense::click(),
            );
            if star.clicked() {
                action = Some(Action::ToggleStar(envelope.uid));
            } else if response.clicked() {
                let modifiers = ui.input(|i| i.modifiers);
                action = Some(if modifiers.shift {
                    Action::SelectRange(envelope.uid)
                } else if modifiers.command {
                    Action::ToggleSelected(envelope.uid)
                } else {
                    Action::Focus(envelope.uid)
                });
            }

            if input.scroll_to_cursor && Some(index) == cursor_index {
                ui.scroll_to_rect(rect, None);
            }
        }
    });

    ListOutput { action, visible }
}

fn draw_row(
    ui: &Ui,
    rect: Rect,
    envelope: &Envelope,
    is_cursor: bool,
    is_selected: bool,
    response: &egui::Response,
    input: &ListInput<'_>,
) {
    let visuals = ui.visuals();
    let painter = ui.painter();
    let family = input.family.clone();
    let font = |size: f32| FontId::new(size, family.clone());

    let background = if is_cursor {
        visuals.selection.bg_fill.gamma_multiply(0.55)
    } else if is_selected {
        visuals.selection.bg_fill.gamma_multiply(0.3)
    } else if response.hovered() {
        visuals.widgets.hovered.bg_fill.gamma_multiply(0.4)
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
    let normal = visuals.text_color();
    let weak = visuals.weak_text_color();

    let size = input.base_size;
    let left = rect.left() + 10.0;
    let right = rect.right() - 34.0;

    // Unread marker doubles as the left gutter.
    if unread {
        painter.circle_filled(
            pos2(left + 3.0, rect.top() + size * 0.95),
            3.5,
            visuals.selection.bg_fill,
        );
    }
    let text_left = left + 14.0;

    let date = format_date_short(envelope.date);
    let date_width = if date.is_empty() {
        0.0
    } else {
        let galley = painter.layout_no_wrap(
            date.clone(),
            font(size * 0.82),
            weak,
        );
        let width = galley.size().x;
        painter.galley(pos2(right - width, rect.top() + 7.0), galley, weak);
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
        pos2(text_left, rect.top() + 6.0),
        first_line_width,
        &sender,
        font(size * 0.95),
        if unread { strong } else { normal },
    );

    if input.compact {
        // One line: sender, then subject sharing the row.
        let subject_left = text_left + first_line_width * 0.32;
        let _ = paint_truncated(
            painter,
            pos2(subject_left, rect.top() + 6.0),
            (right - date_width - subject_left).max(40.0),
            display_subject(envelope),
            font(size * 0.95),
            if unread { strong } else { normal },
        );
    } else {
        let _ = paint_truncated(
            painter,
            pos2(text_left, rect.top() + size * 1.5),
            right - text_left,
            display_subject(envelope),
            font(size * 0.95),
            if unread { strong } else { normal },
        );

        if !envelope.preview.is_empty() {
            let _ = paint_truncated(
                painter,
                pos2(text_left, rect.top() + size * 2.6),
                right - text_left,
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
            "\u{1F4CE}",
            font(size * 0.8),
            weak,
        );
    }

    let starred = envelope.flags.has(Flags::FLAGGED);
    painter.text(
        pos2(rect.right() - 16.0, rect.top() + 8.0),
        Align2::CENTER_TOP,
        if starred { "\u{2605}" } else { "\u{2606}" },
        font(size * 0.95),
        if starred { Color32::from_rgb(230, 180, 60) } else { weak.gamma_multiply(0.6) },
    );
}

fn display_subject(envelope: &Envelope) -> &str {
    if envelope.subject.trim().is_empty() {
        "(no subject)"
    } else {
        &envelope.subject
    }
}
