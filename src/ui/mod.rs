//! User interface: a three-pane reader built with egui.
//!
//! The panes are independent modules that render from application state and
//! return an [`Action`] describing what the user asked for. The app applies
//! actions in one place, which keeps command dispatch and borrow scopes
//! simple.

pub mod accounts;
pub mod compose;
pub mod images;
pub mod message_list;
pub mod reader;
pub mod sidebar;

use egui::{Color32, FontFamily, FontId};

use crate::config::{AccountId, PaneFont, PaneStyle};

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
    Focus(u32),
    /// Extend the multi-selection to a message.
    ToggleSelected(u32),
    /// Select a contiguous run ending at a message.
    SelectRange(u32),
    ToggleStar(u32),
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
    /// Load the remote images this message asked for.
    LoadRemoteImages,
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
    fn renders_no_date_for_missing_timestamps() {
        assert_eq!(format_date_short(0), "");
        assert_eq!(format_date_long(0), "unknown date");
    }
}

/// The font a pane draws its own text in.
pub fn pane_font(style: PaneStyle, base_size: f32) -> FontId {
    let family = match style.font {
        PaneFont::Sans => FontFamily::Proportional,
        PaneFont::Mono => FontFamily::Monospace,
    };
    FontId::new(style.size(base_size), family)
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
