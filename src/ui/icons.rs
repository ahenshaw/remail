//! The glyphs the interface draws, and a check that they exist.
//!
//! egui bundles a deliberately small font set: Ubuntu-Light, a subset of Noto
//! Emoji, and an icon font. A codepoint outside that set renders as a box,
//! silently and only at runtime. Several shipped that way before this check
//! existed — a printer, a card-index, two disclosure triangles.
//!
//! Every glyph drawn directly by this application is listed here, so the set
//! can be checked against the bundled fonts in one place. `scripts/glyphs.py`
//! does that check; run it after changing anything in this module.
//!
//! Mailbox icons are not in it: they are drawn as line art by
//! [`draw_mailbox`], because no glyph could be made to sit correctly beside
//! the text.
//!
//! egui\'s own `Fonts::has_glyphs` looks like the right tool and is not: it
//! reports glyphs as missing that demonstrably render, so it cannot tell a
//! real gap from a false alarm.

/// Star on a flagged message.
pub const STAR_FILLED: &str = "\u{2605}";
/// Star on an unflagged message.
pub const STAR_HOLLOW: &str = "\u{2606}";
/// Marks a message with attachments.
pub const ATTACHMENT: &str = "\u{1F4CE}";
/// Print action. U+1F5A8 PRINTER is *not* in the bundled set; this is
/// U+1F5B6 PRINTER ICON, which is.
pub const PRINTER: &str = "\u{1F5B6}";
/// Stands in for an image that was blocked or could not be decoded.
pub const IMAGE: &str = "\u{1F5BC}";
/// Bullets for unordered lists, by nesting depth.
pub const BULLETS: [&str; 3] = ["\u{2022}", "\u{25CB}", "\u{25AA}"];

/// Nominal font size that draws an icon as tall as the text beside it.
///
/// epaint shrinks the bundled emoji fonts on load — `FontTweak { scale: 0.81 }`
/// for Noto Emoji — so asking for the text's size yields a glyph about a fifth
/// too small, and asking for less than that (as this once did) compounds it.
/// The factor undoes that shrink and then reaches for the font's ascender, so
/// an icon stands at least as tall as the tallest letter it sits beside.
pub fn size_beside_text(text_size: f32) -> f32 {
    const EMOJI_SHRINK: f32 = 0.81;
    /// Roughly the ascender of a humanist sans, as a fraction of em.
    const ASCENDER: f32 = 0.92;

    text_size * ASCENDER / EMOJI_SHRINK
}

/// Draws a mailbox icon as line art inside `rect`.
///
/// These were emoji, and emoji could not be made to sit right beside text.
/// The bundled fonts shrink them on load, place them centred on the em box
/// rather than on the baseline like letters, and lack glyphs for some of the
/// mailboxes entirely — three separate problems, none fixable from the
/// outside. Geometry has none of them: it lands exactly where it is put,
/// scales with the text, tints cleanly, and can never be a missing glyph.
pub fn draw_mailbox(
    painter: &egui::Painter,
    rect: egui::Rect,
    special: crate::mail::SpecialUse,
    color: egui::Color32,
) {
    use crate::mail::SpecialUse as S;
    use egui::{Shape, Stroke};

    let stroke = Stroke::new((rect.width() * 0.09).max(1.0), color);
    // Unit coordinates, so every icon is drawn in the same square.
    let p = |x: f32, y: f32| {
        egui::pos2(
            rect.left() + x * rect.width(),
            rect.top() + y * rect.height(),
        )
    };
    let line = |points: Vec<egui::Pos2>| Shape::line(points, stroke);
    let closed = |points: Vec<egui::Pos2>| Shape::closed_line(points, stroke);

    match special {
        S::Inbox => {
            // A tray with mail dropping into it.
            painter.add(line(vec![
                p(0.08, 0.50),
                p(0.08, 0.88),
                p(0.92, 0.88),
                p(0.92, 0.50),
            ]));
            painter.add(line(vec![p(0.50, 0.12), p(0.50, 0.56)]));
            painter.add(line(vec![p(0.30, 0.38), p(0.50, 0.58), p(0.70, 0.38)]));
        }
        S::Sent => {
            // The same tray, with mail leaving it.
            painter.add(line(vec![
                p(0.08, 0.50),
                p(0.08, 0.88),
                p(0.92, 0.88),
                p(0.92, 0.50),
            ]));
            painter.add(line(vec![p(0.50, 0.12), p(0.50, 0.56)]));
            painter.add(line(vec![p(0.30, 0.32), p(0.50, 0.12), p(0.70, 0.32)]));
        }
        S::Drafts => {
            // A page with a turned corner.
            painter.add(closed(vec![
                p(0.18, 0.10),
                p(0.62, 0.10),
                p(0.84, 0.32),
                p(0.84, 0.90),
                p(0.18, 0.90),
            ]));
            painter.add(line(vec![p(0.62, 0.10), p(0.62, 0.32), p(0.84, 0.32)]));
        }
        S::Trash => {
            painter.add(line(vec![p(0.12, 0.24), p(0.88, 0.24)]));
            painter.add(line(vec![p(0.38, 0.24), p(0.38, 0.14), p(0.62, 0.14), p(0.62, 0.24)]));
            painter.add(line(vec![p(0.22, 0.24), p(0.28, 0.90), p(0.72, 0.90), p(0.78, 0.24)]));
        }
        S::Junk => {
            // A warning triangle.
            painter.add(closed(vec![p(0.50, 0.12), p(0.94, 0.86), p(0.06, 0.86)]));
            painter.add(line(vec![p(0.50, 0.40), p(0.50, 0.62)]));
            painter.add(line(vec![p(0.50, 0.72), p(0.50, 0.75)]));
        }
        S::Archive => {
            // A carton: lid across the top, body below.
            painter.add(closed(vec![p(0.08, 0.16), p(0.92, 0.16), p(0.92, 0.36), p(0.08, 0.36)]));
            painter.add(line(vec![p(0.16, 0.36), p(0.16, 0.88), p(0.84, 0.88), p(0.84, 0.36)]));
            painter.add(line(vec![p(0.40, 0.56), p(0.60, 0.56)]));
        }
        S::All => {
            // An envelope.
            painter.add(closed(vec![
                p(0.06, 0.22),
                p(0.94, 0.22),
                p(0.94, 0.82),
                p(0.06, 0.82),
            ]));
            painter.add(line(vec![p(0.06, 0.26), p(0.50, 0.58), p(0.94, 0.26)]));
        }
        S::Normal => {
            // A folder: tab, then body.
            painter.add(line(vec![
                p(0.07, 0.82),
                p(0.07, 0.20),
                p(0.40, 0.20),
                p(0.48, 0.34),
                p(0.93, 0.34),
                p(0.93, 0.82),
                p(0.07, 0.82),
            ]));
        }
    }
}
