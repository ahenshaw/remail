//! Persistent configuration: accounts, server coordinates and UI settings.
//!
//! Configuration lives in a single TOML file inside the platform config
//! directory. Secrets (passwords, OAuth refresh tokens) are deliberately *not*
//! stored here; they go to the OS keyring via [`crate::secrets`].

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Stable identifier for an account. Assigned once and never reused, so cached
/// rows in the SQLite store stay valid across reorderings of the account list.
pub type AccountId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    /// Google OAuth2 with the `XOAUTH2` SASL mechanism.
    OAuth2,
    /// `LOGIN`/`PLAIN` with a password or provider app-password.
    Password,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encryption {
    /// Implicit TLS from the first byte (IMAPS 993, SMTPS 465).
    #[default]
    Tls,
    /// Plaintext greeting upgraded with `STARTTLS` (IMAP 143, SMTP 587).
    StartTls,
}

/// Window theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    #[default]
    Slate,
    Charcoal,
    Frost,
    Paper,
    /// A light theme following Outlook's surfaces: grey chrome, a white
    /// reading pane, and a pale blue selection.
    Outlook,
}

impl ThemeChoice {
    pub fn theme(self) -> elegance::Theme {
        match self {
            ThemeChoice::Slate => elegance::Theme::slate(),
            ThemeChoice::Charcoal => elegance::Theme::charcoal(),
            ThemeChoice::Frost => elegance::Theme::frost(),
            ThemeChoice::Paper => elegance::Theme::paper(),
            ThemeChoice::Outlook => outlook_theme(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Slate => "Slate",
            ThemeChoice::Charcoal => "Charcoal",
            ThemeChoice::Frost => "Frost",
            ThemeChoice::Paper => "Paper",
            ThemeChoice::Outlook => "Outlook",
        }
    }

    pub fn all() -> [ThemeChoice; 5] {
        [
            ThemeChoice::Slate,
            ThemeChoice::Charcoal,
            ThemeChoice::Frost,
            ThemeChoice::Paper,
            ThemeChoice::Outlook,
        ]
    }
}

/// Surfaces sampled from Outlook's light theme.
///
/// The structure matters more than the exact values: grey chrome carrying the
/// folder list, a slightly different grey behind the message list, and the
/// reading column as white paper. The folder pane is taken a little deeper
/// than Outlook's, which draws it identically to the list, so the three panes
/// stay distinguishable.
fn outlook_theme() -> elegance::Theme {
    use egui::Color32;

    let mut theme = elegance::Theme::frost();
    let palette = &mut theme.palette;
    palette.is_dark = false;
    palette.bg = Color32::from_rgb(0xf5, 0xf5, 0xf5);
    palette.card = Color32::from_rgb(0xff, 0xff, 0xff);
    palette.input_bg = Color32::from_rgb(0xff, 0xff, 0xff);
    palette.border = Color32::from_rgb(0xe1, 0xe1, 0xe1);
    palette.text = Color32::from_rgb(0x24, 0x24, 0x24);
    palette.text_muted = Color32::from_rgb(0x61, 0x61, 0x61);
    palette.text_faint = Color32::from_rgb(0x8a, 0x8a, 0x8a);
    palette.blue = Color32::from_rgb(0x0f, 0x6c, 0xbd);
    palette.blue_hover = Color32::from_rgb(0x11, 0x5e, 0xa3);
    theme
}

/// Font family for a pane: one of the two faces egui ships, or any family
/// installed on the system, loaded on demand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PaneFont {
    /// egui's built-in proportional face.
    #[default]
    Sans,
    /// egui's built-in monospace face.
    Mono,
    /// A system family, by name.
    Named(String),
}

impl PaneFont {
    pub fn label(&self) -> &str {
        match self {
            PaneFont::Sans => "Sans (built-in)",
            PaneFont::Mono => "Mono (built-in)",
            PaneFont::Named(name) => name,
        }
    }
}

/// Per-pane text settings. Each pane can differ: a dense folder list and a
/// comfortable reading column want different sizes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneStyle {
    /// Overrides [`UiSettings::font_size`] for this pane when set.
    pub font_size: Option<f32>,
    pub font: PaneFont,
}

impl PaneStyle {
    /// The size this pane actually draws at.
    pub fn size(&self, base: f32) -> f32 {
        self.font_size.unwrap_or(base).clamp(8.0, 32.0)
    }
}

/// Which engine renders `text/html` message bodies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HtmlBackend {
    /// Built-in layout engine that draws sanitized HTML directly with egui.
    #[default]
    Native,
    /// Servo, rendered offscreen and blitted into the reader pane.
    Servo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: AccountId,
    /// Display name in the sidebar.
    pub label: String,
    /// The address messages are sent from.
    pub email: String,
    /// Human name used in the `From:` header.
    #[serde(default)]
    pub display_name: String,

    pub imap_host: String,
    pub imap_port: u16,
    #[serde(default)]
    pub imap_encryption: Encryption,

    pub smtp_host: String,
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_encryption: Encryption,

    /// Login name. Usually the same as `email`.
    pub username: String,
    pub auth: AuthMethod,

    /// OAuth client credentials. Google requires each application to register
    /// its own; there is no usable shared default, so the user supplies these.
    #[serde(default)]
    pub oauth_client_id: String,
    #[serde(default)]
    pub oauth_client_secret: String,

    /// Mailbox to open on startup.
    #[serde(default = "default_inbox")]
    pub default_mailbox: String,
    /// Keep a long-lived IDLE connection open on the default mailbox.
    #[serde(default = "yes")]
    pub use_idle: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_inbox() -> String {
    "INBOX".to_string()
}
fn yes() -> bool {
    true
}

impl AccountConfig {
    /// A Gmail account using OAuth2. Ports and hosts are Google's published
    /// endpoints.
    pub fn gmail(id: AccountId, email: &str) -> Self {
        Self {
            id,
            label: email.to_string(),
            email: email.to_string(),
            display_name: String::new(),
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_encryption: Encryption::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_encryption: Encryption::Tls,
            username: email.to_string(),
            auth: AuthMethod::OAuth2,
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
            default_mailbox: default_inbox(),
            use_idle: true,
            enabled: true,
        }
    }

    /// A generic IMAP account with password authentication.
    pub fn imap(id: AccountId, email: &str) -> Self {
        let domain = email.split('@').nth(1).unwrap_or("example.com");
        Self {
            id,
            label: email.to_string(),
            email: email.to_string(),
            display_name: String::new(),
            imap_host: format!("imap.{domain}"),
            imap_port: 993,
            imap_encryption: Encryption::Tls,
            smtp_host: format!("smtp.{domain}"),
            smtp_port: 587,
            smtp_encryption: Encryption::StartTls,
            username: email.to_string(),
            auth: AuthMethod::Password,
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
            default_mailbox: default_inbox(),
            use_idle: true,
            enabled: true,
        }
    }

    /// Full name for dialogs and notifications.
    pub fn title(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.email
        } else {
            &self.label
        }
    }

    /// Short name for the sidebar, where horizontal space is scarce.
    ///
    /// Uses the configured label, unless it is just the address again (the
    /// old default), in which case the local part is a far better fit.
    pub fn short_name(&self) -> &str {
        let label = self.label.trim();
        if !label.is_empty() && label != self.email.trim() {
            return label;
        }
        match self.email.split('@').next() {
            Some(local) if !local.is_empty() => local,
            _ => &self.email,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub theme: ThemeChoice,
    pub html_backend: HtmlBackend,
    /// Fetch `<img src="http…">` referenced by messages. Off by default
    /// because remote images are the standard read-tracking beacon.
    pub load_remote_content: bool,
    /// Poll interval for accounts whose server does not offer IDLE.
    pub poll_interval_secs: u64,
    /// Envelopes fetched per mailbox on the initial sync.
    pub initial_sync_count: u32,
    /// Base text size, used by any pane without its own override.
    pub font_size: f32,
    pub folders: PaneStyle,
    pub messages: PaneStyle,
    pub reading: PaneStyle,
    pub compact_list: bool,
    /// Width of the folder pane, in points. Restored on startup.
    pub folders_width: f32,
    /// Width of the message list pane, in points.
    pub messages_width: f32,
    /// Mark a message `\Seen` after it has been open this long. 0 disables.
    pub mark_read_after_secs: f32,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Slate,
            html_backend: HtmlBackend::Native,
            load_remote_content: false,
            poll_interval_secs: 120,
            initial_sync_count: 500,
            font_size: 14.0,
            folders: PaneStyle::default(),
            messages: PaneStyle::default(),
            reading: PaneStyle::default(),
            compact_list: false,
            folders_width: 200.0,
            messages_width: 380.0,
            mark_read_after_secs: 1.5,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub ui: UiSettings,
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    /// Loads the config, returning defaults when the file does not exist yet.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        // Write-then-rename so a crash mid-write cannot truncate the config.
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn account(&self, id: AccountId) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn account_mut(&mut self, id: AccountId) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// Lowest unused account id.
    pub fn next_account_id(&self) -> AccountId {
        (0..).find(|id| !self.accounts.iter().any(|a| a.id == *id)).unwrap_or(0)
    }
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("org", "remail", "remail")
        .context("could not determine platform config directory")
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// Directory holding the message cache database.
pub fn data_dir() -> Result<PathBuf> {
    let dir = project_dirs()?.data_dir().to_path_buf();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(email: &str, label: &str) -> AccountConfig {
        let mut a = AccountConfig::gmail(0, email);
        a.label = label.to_string();
        a
    }

    #[test]
    fn shortens_the_address_when_no_label_is_set() {
        assert_eq!(account("andrew.henshaw@example.com", "").short_name(), "andrew.henshaw");
    }

    #[test]
    fn prefers_a_real_label() {
        assert_eq!(account("andrew@example.com", "Work").short_name(), "Work");
    }

    #[test]
    fn treats_a_label_equal_to_the_address_as_unset() {
        // The old default filled the label with the address, which is exactly
        // the long string the sidebar has no room for.
        let a = account("andrew.henshaw@example.com", "andrew.henshaw@example.com");
        assert_eq!(a.short_name(), "andrew.henshaw");
    }

    #[test]
    fn falls_back_to_the_whole_address_when_there_is_no_local_part() {
        assert_eq!(account("@example.com", "").short_name(), "@example.com");
    }

    #[test]
    fn pane_size_falls_back_to_the_base() {
        let base = PaneStyle::default();
        assert_eq!(base.size(15.0), 15.0);

        let pinned = PaneStyle { font_size: Some(11.0), ..Default::default() };
        assert_eq!(pinned.size(15.0), 11.0);

        // Absurd values from a hand-edited config stay usable.
        let silly = PaneStyle { font_size: Some(900.0), ..Default::default() };
        assert_eq!(silly.size(15.0), 32.0);
    }
}
