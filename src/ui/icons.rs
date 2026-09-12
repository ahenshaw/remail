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
