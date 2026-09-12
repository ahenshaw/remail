//! User interface: a three-pane reader built with egui.
//!
//! The panes are independent modules that render from application state and
//! return an [`Action`] describing what the user asked for. The app applies
//! actions in one place, which keeps command dispatch and borrow scopes
//! simple.

pub mod accounts;
pub mod compose;
pub mod fonts;
pub mod images;
pub mod message_list;
pub mod reader;
pub mod sidebar;

use egui::{Color32, FontId};

use crate::config::AccountId;
use crate::mail::RowKey;

/// Something the user did that the app needs to act on.
#[derive(Debug, Clone)]
pub enum Action {
    /// Show a mailbox.
    OpenMailbox { account: AccountId, mailbox: String },
    /// Bring an account online.
    Connect(AccountId),
    /// Start the interactive OAuth flow.
    SignIn(AccountId),
    /// Move the keyboard cursor to a message and load it.
    Focus(RowKey),
    /// Extend the multi-selection to a message.
    ToggleSelected(RowKey),
    /// Select a contiguous run ending at a message.
    SelectRange(RowKey),
    ToggleStar(RowKey),
    /// Flip `\Seen` on the current selection.
    ToggleRead,
    Archive,
    Delete,
    Reply { all: bool },
    Forward,
    Compose,
    /// Re-sync the open mailbox.
    Refresh,
    /// Run a server-side search for the current query.
    SearchServer(String),
    /// Load the remote images this message asked for, and remember the
    /// decision for this message.
    LoadRemoteImages,
    /// Trust this sender's remote content from now on.
    AllowRemoteSender,
    /// Open a URL in the system browser.
    OpenUrl(String),
    /// Write an attachment to disk.
    SaveAttachment(usize),
    /// Discard the current listing's search filter.
    ClearSearch,
}

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};

/// Formats a timestamp the way a message list wants it: time for today,
/// weekday for this week, month and day for this year, full date beyond.
pub fn format_date_short(timestamp: i64) -> String {
    let Some(when) = to_local(timestamp) else { return String::new() };
    let now = Local::now();
    let age = now.signed_duration_since(when);

    if age.num_hours() < 24 && when.day() == now.day() {
        when.format("%H:%M").to_string()
    } else if age.num_days() < 7 && age.num_seconds() >= 0 {
        when.format("%a %H:%M").to_string()
    } else if when.year() == now.year() {
        when.format("%-d %b").to_string()
    } else {
        when.format("%-d %b %Y").to_string()
    }
}

/// Full date and time, for the reader header and quoted attributions.
pub fn format_date_long(timestamp: i64) -> String {
    match to_local(timestamp) {
        Some(when) => when.format("%a, %-d %b %Y at %H:%M").to_string(),
        None => "unknown date".to_string(),
    }
}

fn to_local(timestamp: i64) -> Option<DateTime<Local>> {
    if timestamp <= 0 {
        return None;
    }
    Utc.timestamp_opt(timestamp, 0).single().map(|t| t.with_timezone(&Local))
}

/// Human-readable byte count for attachment sizes.
pub fn format_size(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn mixes_towards_the_second_colour() {
        let black = Color32::from_rgb(0, 0, 0);
        let white = Color32::from_rgb(255, 255, 255);
        assert_eq!(mix(black, white, 0.0), black);
        assert_eq!(mix(black, white, 1.0), white);
        assert_eq!(mix(black, white, 0.5), Color32::from_rgb(128, 128, 128));
    }

    #[test]
    fn mixing_clamps_out_of_range_factors() {
        let a = Color32::from_rgb(10, 20, 30);
        let b = Color32::from_rgb(200, 210, 220);
        assert_eq!(mix(a, b, -1.0), a);
        assert_eq!(mix(a, b, 5.0), b);
    }

    #[test]
    fn mixes_each_channel_independently() {
        let a = Color32::from_rgb(0, 100, 200);
        let b = Color32::from_rgb(100, 200, 0);
        assert_eq!(mix(a, b, 0.5), Color32::from_rgb(50, 150, 100));
    }

    #[test]
    fn renders_no_date_for_missing_timestamps() {
        assert_eq!(format_date_short(0), "");
        assert_eq!(format_date_long(0), "unknown date");
    }
}

/// Draws one line of text, truncating with an ellipsis at `max_width`.
///
/// Shared by the panes that paint their own rows, so folder names and subject
/// lines shorten the same way when a pane is narrow. Returns whether the text
/// had to be shortened, which callers use to decide if a tooltip would tell
/// the reader anything they cannot already see.
pub fn paint_truncated(
    painter: &egui::Painter,
    position: egui::Pos2,
    max_width: f32,
    text: &str,
    font: FontId,
    color: Color32,
) -> bool {
    if text.is_empty() || max_width <= 8.0 {
        return false;
    }
    let mut galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
    let mut truncated = false;

    if galley.size().x > max_width {
        truncated = true;
        // Binary search the longest prefix that fits, on character
        // boundaries so multi-byte text never splits mid-character.
        let chars: Vec<char> = text.chars().collect();
        let mut low = 0usize;
        let mut high = chars.len();
        while low < high {
            let mid = (low + high + 1) / 2;
            let candidate: String = chars[..mid].iter().collect::<String>() + "\u{2026}";
            let width = painter.layout_no_wrap(candidate, font.clone(), color).size().x;
            if width <= max_width {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let shortened: String = chars[..low].iter().collect::<String>() + "\u{2026}";
        galley = painter.layout_no_wrap(shortened, font, color);
    }
    painter.galley(position, galley, color);
    truncated
}

/// A selection or hover tint drawn from the theme's accent.
///
/// `subtlety` runs from 0 (the accent at full strength) to 1 (invisible
/// against the surface). Mixing towards the card colour rather than
/// brightening or darkening keeps the result readable on light and dark
/// themes alike, where a fixed adjustment would go the wrong way on one.
pub fn accent_tint(palette: &elegance::Palette, subtlety: f32) -> Color32 {
    mix(palette.blue, palette.card, subtlety)
}

/// Blends two colours in linear space. Used to recede an accent colour
/// towards the body text colour without depending on the theme's polarity,
/// which `gamma_multiply` alone cannot do.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let blend = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Color32::from_rgb(
        blend(a.r(), b.r()),
        blend(a.g(), b.g()),
        blend(a.b(), b.b()),
    )
}
