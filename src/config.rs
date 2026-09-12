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

/// Which of elegance's built-in themes the window uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    #[default]
    Slate,
    Charcoal,
    Frost,
    Paper,
}

impl ThemeChoice {
    pub fn theme(self) -> elegance::Theme {
        self.built_in().theme()
    }

    pub fn built_in(self) -> elegance::BuiltInTheme {
        match self {
            ThemeChoice::Slate => elegance::BuiltInTheme::Slate,
            ThemeChoice::Charcoal => elegance::BuiltInTheme::Charcoal,
            ThemeChoice::Frost => elegance::BuiltInTheme::Frost,
            ThemeChoice::Paper => elegance::BuiltInTheme::Paper,
        }
    }

    pub fn label(self) -> &'static str {
        self.built_in().label()
    }

    pub fn all() -> [ThemeChoice; 4] {
        [ThemeChoice::Slate, ThemeChoice::Charcoal, ThemeChoice::Frost, ThemeChoice::Paper]
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

    /// Name shown in the sidebar, falling back to the address.
    pub fn title(&self) -> &str {
        if self.label.is_empty() {
            &self.email
        } else {
            &self.label
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
    pub font_size: f32,
    pub compact_list: bool,
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
            compact_list: false,
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
