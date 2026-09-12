//! Middle pane: the message list.
//!
//! Rows are painted directly rather than composed from widgets, and only the
//! visible range is touched each frame. A mailbox with fifty thousand cached
//! messages costs the same to scroll as one with fifty.

use std::collections::BTreeSet;
use std::ops::Range;

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, Vec2, pos2};

use super::{Action, format_date_short, paint_truncated};
use crate::mail::{Envelope, Flags, RowKey};

pub struct ListInput<'a> {
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
        input.font.size * 2.0
    } else {
        input.font.size * 3.6
    };

    let cursor_index = input
        .cursor
        .as_ref()
        .and_then(|key| input.envelopes.iter().position(|e| &e.key() == key));

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

            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::click());

            if ui.is_rect_visible(rect) {
                draw_row(
                    ui, rect, envelope, index, is_cursor, is_selected, &response, &input,
                );
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

    ListOutput { action, visible }
}

fn draw_row(
    ui: &Ui,
    rect: Rect,
    envelope: &Envelope,
    index: usize,
    is_cursor: bool,
    is_selected: bool,
    response: &egui::Response,
    input: &ListInput<'_>,
) {
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
    } else if response.hovered() {
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
    let subject_color = if unread {
        palette.blue
    } else {
        super::mix(palette.blue, palette.text_muted, 0.4)
    };

    let size = input.font.size;
    let left = rect.left() + 10.0;
    let right = rect.right() - 34.0;

    // Unread marker doubles as the left gutter.
    if unread {
        painter.circle_filled(pos2(left + 3.0, rect.top() + size * 0.95), 3.5, palette.blue);
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
        // egui has one weight per family, so "strong" is a colour, not a
        // heavier face. Senders take it whether or not they are unread.
        strong,
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
            subject_color,
        );
    } else {
        let _ = paint_truncated(
            painter,
            pos2(text_left, rect.top() + size * 1.5),
            right - text_left,
            display_subject(envelope),
            font(size * 0.95),
            subject_color,
        );

        let mut preview_left = text_left;
        if input.show_folder && !envelope.mailbox.is_empty() {
            let leaf = envelope
                .mailbox
                .rsplit(['/', '.'])
                .next()
                .unwrap_or(&envelope.mailbox);
            let galley =
                painter.layout_no_wrap(leaf.to_string(), font(size * 0.74), palette.blue);
            let width = galley.size().x;
            painter.galley(pos2(preview_left, rect.top() + size * 2.66), galley, palette.blue);
            preview_left += width + 8.0;
        }

        if !envelope.preview.is_empty() {
            let _ = paint_truncated(
                painter,
                pos2(preview_left, rect.top() + size * 2.6),
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
