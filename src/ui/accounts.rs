//! Account and settings dialogs.
//!
//! Account edits are staged in a working copy and only written back to the
//! config on save, so a half-typed hostname never reaches the mail engine.

use egui::{Context, RichText, Ui};
use elegance::{
    Accent, Badge, BadgeTone, Button, ButtonSize, Callout, CalloutTone, Card, Modal, Select,
    Switch, TextInput, Theme, glyphs,
};

use crate::config::{
    AccountConfig, AccountId, AuthMethod, Config, Encryption, HtmlBackend, PaneFont, PaneStyle,
    ThemeChoice,
};

/// What the dialogs are asking the app to do.
#[derive(Debug, Clone)]
pub enum AccountsAction {
    /// Persist the working copy of this account.
    Save(AccountConfig),
    /// Run the interactive OAuth flow.
    SignIn(AccountId),
    SignOut(AccountId),
    /// Remove the account, its secrets and its cache.
    Remove(AccountId),
    /// Settings changed and need saving.
    SettingsChanged,
}

pub struct AccountsDialog {
    pub open: bool,
    /// The account being edited, or `None` while showing the list.
    pub editing: Option<AccountConfig>,
    /// Password field for the account being edited. Never persisted here.
    pub password: String,
    /// Set when the user asked to delete an account and must confirm.
    pub confirm_remove: Option<AccountId>,
    /// A password typed in the editor, handed over once the account is saved.
    /// Staged rather than applied inline so the keyring write happens after
    /// the account exists in the config.
    pub pending_password: Option<(AccountId, String)>,
    /// An OAuth account that should start its browser flow once saved.
    pub pending_sign_in: Option<AccountId>,
}

impl Default for AccountsDialog {
    fn default() -> Self {
        Self {
            open: true,
            editing: None,
            password: String::new(),
            confirm_remove: None,
            pending_password: None,
            pending_sign_in: None,
        }
    }
}

pub fn show(
    ctx: &Context,
    dialog: &mut AccountsDialog,
    config: &Config,
    keyring_available: bool,
    theme: &Theme,
) -> Option<AccountsAction> {
    let mut action = None;

    let heading = match &dialog.editing {
        Some(account) if config.account(account.id).is_some() => "Edit account",
        Some(_) => "Add account",
        None => "Accounts",
    };

    // The modal needs `&mut` to its open flag while the body needs `&mut`
    // to the rest of the dialog; stage the flag and write it back after.
    let mut open = dialog.open;
    Modal::new("accounts-modal", &mut open)
        .heading(heading)
        .header_icon(glyphs::KEY.to_string())
        .max_width(620.0)
        .show(ctx, |ui| {
            if !keyring_available {
                Callout::new(CalloutTone::Warning)
                    .icon(glyphs::TRIANGLE_ALERT.to_string())
                    .title("No system keyring")
                    .body(
                        "Passwords and tokens cannot be saved. On Linux this usually means \
                         no Secret Service is running.",
                    )
                    .multiline()
                    .tinted()
                    .show(ui, |_| {});
                ui.add_space(8.0);
            }

            match dialog.editing.clone() {
                Some(account) => action = editor(ui, dialog, account, config, theme),
                None => action = list(ui, dialog, config, theme),
            }
        });
    dialog.open = open;

    if let Some(id) = dialog.confirm_remove {
        let mut confirm_open = true;
        let account_name = config
            .account(id)
            .map(|a| a.title().to_string())
            .unwrap_or_else(|| "this account".to_string());

        Modal::new("confirm-remove", &mut confirm_open)
            .heading("Remove account?")
            .header_icon(glyphs::TRIANGLE_ALERT.to_string())
            .header_accent(Accent::Red)
            .alert(true)
            .max_width(420.0)
            .show(ctx, |ui| {
                ui.label(theme.body_text(format!(
                    "{account_name} will be removed, along with its saved password and \
                     cached messages. Nothing is deleted on the server."
                )));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new("Remove").accent(Accent::Red))
                        .clicked()
                    {
                        action = Some(AccountsAction::Remove(id));
                        dialog.confirm_remove = None;
                    }
                    if ui.add(Button::new("Cancel").outline()).clicked() {
                        dialog.confirm_remove = None;
                    }
                });
            });

        if !confirm_open {
            dialog.confirm_remove = None;
        }
    }

    action
}

fn list(
    ui: &mut Ui,
    dialog: &mut AccountsDialog,
    config: &Config,
    theme: &Theme,
) -> Option<AccountsAction> {
    let mut action = None;

    if config.accounts.is_empty() {
        ui.add_space(8.0);
        ui.label(theme.muted_text("No accounts configured yet."));
        ui.add_space(12.0);
    }

    for account in &config.accounts {
        Card::new().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(account.title()).strong());
                    ui.label(theme.faint_text(format!(
                        "{}:{}",
                        account.imap_host, account.imap_port
                    )));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(Button::new("Edit").size(ButtonSize::Small).outline()).clicked()
                    {
                        dialog.editing = Some(account.clone());
                        dialog.password.clear();
                    }
                    if account.auth == AuthMethod::OAuth2
                        && ui
                            .add(
                                Button::new("Sign in")
                                    .size(ButtonSize::Small)
                                    .accent(Accent::Blue)
                                    .enabled(!account.oauth_client_id.trim().is_empty()),
                            )
                            .on_hover_text("Authorize this account with Google")
                            .clicked()
                    {
                        action = Some(AccountsAction::SignIn(account.id));
                    }
                    if ui
                        .add(
                            Button::new(glyphs::TRASH.to_string())
                                .size(ButtonSize::Small)
                                .outline()
                                .accent(Accent::Red),
                        )
                        .on_hover_text("Remove account")
                        .clicked()
                    {
                        dialog.confirm_remove = Some(account.id);
                    }
                    ui.add(match account.auth {
                        AuthMethod::OAuth2 => Badge::new("oauth", BadgeTone::Ok),
                        AuthMethod::Password => Badge::new("password", BadgeTone::Neutral),
                    });
                });
            });
        });
        ui.add_space(6.0);
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .add(Button::new(format!("{} Gmail", glyphs::PLUS)).accent(Accent::Blue))
            .clicked()
        {
            dialog.editing = Some(AccountConfig::gmail(config.next_account_id(), ""));
            dialog.password.clear();
        }
        if ui
            .add(Button::new(format!("{} IMAP", glyphs::PLUS)).outline())
            .clicked()
        {
            dialog.editing = Some(AccountConfig::imap(config.next_account_id(), ""));
            dialog.password.clear();
        }
    });

    action
}

fn editor(
    ui: &mut Ui,
    dialog: &mut AccountsDialog,
    mut account: AccountConfig,
    config: &Config,
    theme: &Theme,
) -> Option<AccountsAction> {
    let mut action = None;
    let is_new = config.account(account.id).is_none();
    let oauth_ready =
        account.auth == AuthMethod::OAuth2 && !account.oauth_client_id.trim().is_empty();

    egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.add(TextInput::new(&mut account.email).label("Email address").hint("you@example.com"));
        ui.add(TextInput::new(&mut account.display_name).label("Display name").hint("Your Name"));
        ui.add(
            TextInput::new(&mut account.label)
                .label("Short name")
                .hint("shown in the sidebar"),
        );

        ui.add_space(10.0);
        ui.label(theme.heading_text("Authentication"));
        let mut oauth = account.auth == AuthMethod::OAuth2;
        if ui.add(Switch::new(&mut oauth, "Use Google OAuth2")).changed() {
            account.auth = if oauth { AuthMethod::OAuth2 } else { AuthMethod::Password };
        }

        if oauth {
            Callout::new(CalloutTone::Info)
                .icon(glyphs::INFO.to_string())
                .body(
                    "Google requires each application to register its own OAuth client. \
                     Create a Desktop app client in Google Cloud Console and paste its \
                     id and secret here.",
                )
                .multiline()
                .show(ui, |_| {});
            ui.add_space(6.0);
            ui.add(TextInput::new(&mut account.oauth_client_id).label("OAuth client id"));
            ui.add(
                TextInput::new(&mut account.oauth_client_secret)
                    .label("OAuth client secret")
                    .password(true)
                    .revealable(true),
            );

            if !is_new {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !account.oauth_client_id.trim().is_empty();
                    if ui
                        .add(
                            Button::new("Sign in with Google")
                                .accent(Accent::Blue)
                                .size(ButtonSize::Small)
                                .enabled(ready),
                        )
                        .clicked()
                    {
                        action = Some(AccountsAction::SignIn(account.id));
                    }
                    if ui
                        .add(Button::new("Sign out").size(ButtonSize::Small).outline())
                        .clicked()
                    {
                        action = Some(AccountsAction::SignOut(account.id));
                    }
                });
            }
        } else {
            ui.add(TextInput::new(&mut account.username).label("Username"));
            ui.add(
                TextInput::new(&mut dialog.password)
                    .label("Password")
                    .hint("app password recommended")
                    .password(true)
                    .revealable(true),
            );
        }

        ui.add_space(10.0);
        ui.label(theme.heading_text("Servers"));
        ui.horizontal(|ui| {
            ui.add(TextInput::new(&mut account.imap_host).label("IMAP host").desired_width(220.0));
            port_field(ui, "IMAP port", &mut account.imap_port);
            encryption_select(ui, "imap-enc", &mut account.imap_encryption);
        });
        ui.horizontal(|ui| {
            ui.add(TextInput::new(&mut account.smtp_host).label("SMTP host").desired_width(220.0));
            port_field(ui, "SMTP port", &mut account.smtp_port);
            encryption_select(ui, "smtp-enc", &mut account.smtp_encryption);
        });

        ui.add_space(10.0);
        ui.add(TextInput::new(&mut account.default_mailbox).label("Open on startup"));
        ui.add(Switch::new(&mut account.use_idle, "Keep a push connection open (IDLE)"));
        ui.add(Switch::new(&mut account.enabled, "Account enabled"));
    });

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let valid = !account.email.trim().is_empty() && !account.imap_host.trim().is_empty();
        if ui
            .add(Button::new("Save").accent(Accent::Blue).enabled(valid))
            .clicked()
        {
            if account.username.trim().is_empty() {
                account.username = account.email.clone();
            }
            let id = account.id;
            let password = std::mem::take(&mut dialog.password);
            action = Some(AccountsAction::Save(account.clone()));
            dialog.editing = None;

            // The password is a separate, keyring-bound side effect; queue it
            // for the app to apply after the account itself is saved.
            if !password.is_empty() {
                dialog.pending_password = Some((id, password));
            }
            // Configuring a client id does not authorize anything. For a new
            // OAuth account, go straight on to the browser rather than leaving
            // the user to find the sign-in button.
            if is_new && oauth_ready {
                dialog.pending_sign_in = Some(id);
            }
        }
        if ui.add(Button::new("Back").outline()).clicked() {
            dialog.editing = None;
            dialog.password.clear();
        }
    });

    // Keep the working copy in sync with what was typed this frame.
    if let Some(editing) = &mut dialog.editing {
        *editing = account;
    }

    action
}

fn port_field(ui: &mut Ui, label: &str, port: &mut u16) {
    let mut text = port.to_string();
    if ui
        .add(TextInput::new(&mut text).label(label).desired_width(80.0))
        .changed()
    {
        // An empty or nonsense port keeps the previous value rather than
        // silently becoming zero.
        if let Ok(parsed) = text.trim().parse::<u16>() {
            *port = parsed;
        }
    }
}

fn encryption_select(ui: &mut Ui, id: &str, encryption: &mut Encryption) {
    let mut choice = *encryption;
    ui.add(
        Select::new(id, &mut choice)
            .label("Security")
            .options([(Encryption::Tls, "TLS"), (Encryption::StartTls, "STARTTLS")])
            .width(120.0),
    );
    *encryption = choice;
}

/// The settings dialog.
pub fn settings(
    ctx: &Context,
    open: &mut bool,
    config: &mut Config,
    servo_available: bool,
    theme: &Theme,
) -> Option<AccountsAction> {
    let mut action = None;

    Modal::new("settings-modal", open)
        .heading("Settings")
        .header_icon(glyphs::SETTINGS.to_string())
        .max_width(520.0)
        .show(ctx, |ui| {
            let before = config.ui.clone();

            ui.label(theme.heading_text("Appearance"));
            let mut choice = config.ui.theme;
            ui.add(
                Select::new("theme-select", &mut choice)
                    .label("Theme")
                    .options(ThemeChoice::all().map(|t| (t, t.label())))
                    .width(180.0),
            );
            config.ui.theme = choice;

            ui.add(
                elegance::Slider::new(&mut config.ui.font_size, 11.0..=20.0)
                    .label("Text size"),
            );
            ui.add(Switch::new(&mut config.ui.compact_list, "Compact message list"));

            ui.add_space(10.0);
            ui.label(theme.heading_text("Panes"));
            ui.label(theme.faint_text(
                "Each pane can override the base size. A denser folder list and a \
                 larger reading column usually read best.",
            ));
            ui.add_space(4.0);

            let base = config.ui.font_size;
            pane_row(ui, "Folders", "folders", &mut config.ui.folders, base);
            pane_row(ui, "Messages", "messages", &mut config.ui.messages, base);
            pane_row(ui, "Reading", "reading", &mut config.ui.reading, base);

            let overridden = config.ui.folders.font_size.is_some()
                || config.ui.messages.font_size.is_some()
                || config.ui.reading.font_size.is_some();
            if ui
                .add(
                    Button::new("Match base size")
                        .size(ButtonSize::Small)
                        .outline()
                        .enabled(overridden),
                )
                .on_hover_text("Drop every per-pane size override")
                .clicked()
            {
                config.ui.folders.font_size = None;
                config.ui.messages.font_size = None;
                config.ui.reading.font_size = None;
            }

            ui.add_space(12.0);
            ui.label(theme.heading_text("Reading"));

            let mut backend = config.ui.html_backend;
            ui.add(
                Select::new("backend-select", &mut backend)
                    .label("HTML renderer")
                    .options([
                        (HtmlBackend::Native, "Built-in"),
                        (HtmlBackend::Servo, "Servo"),
                    ])
                    .width(180.0)
                    .enabled(servo_available),
            );
            config.ui.html_backend = backend;

            if !servo_available {
                ui.label(theme.faint_text(
                    "Servo backend not compiled in; rebuild with --features servo.",
                ));
            }

            ui.add(Switch::new(
                &mut config.ui.load_remote_content,
                "Load remote images automatically",
            ));
            if config.ui.load_remote_content {
                ui.label(theme.faint_text(
                    "Senders can tell when a message is opened. Leaving this off asks per message.",
                ));
            }
            ui.add(
                elegance::Slider::new(&mut config.ui.mark_read_after_secs, 0.0..=10.0)
                    .label("Mark read after (seconds, 0 = never)"),
            );

            ui.add_space(12.0);
            ui.label(theme.heading_text("Syncing"));
            ui.add(
                elegance::Slider::new(&mut config.ui.initial_sync_count, 100..=5000)
                    .label("Messages per mailbox"),
            );
            ui.add(
                elegance::Slider::new(&mut config.ui.poll_interval_secs, 30..=900)
                    .label("Poll interval (seconds)"),
            );
            ui.label(theme.faint_text(
                "Polling only applies to servers without IDLE support.",
            ));

            if !settings_equal(&before, &config.ui) {
                action = Some(AccountsAction::SettingsChanged);
            }
        });

    action
}

/// One pane's font family and size.
fn pane_row(ui: &mut Ui, label: &str, id: &str, style: &mut PaneStyle, base: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([74.0, 20.0], egui::Label::new(label));

        let mut font = style.font;
        ui.add(
            Select::new(id, &mut font)
                .options(PaneFont::all().map(|f| (f, f.label())))
                .width(84.0),
        );
        style.font = font;

        // The slider starts at whatever the pane draws at today; touching it
        // pins an override, which "Match base size" clears again.
        let mut size = style.size(base);
        if ui
            .add(
                elegance::Slider::new(&mut size, 9.0..=26.0)
                    .decimals(0)
                    .desired_width(150.0),
            )
            .changed()
        {
            style.font_size = Some(size);
        }
    });
}

/// Field-wise comparison; `UiSettings` holds floats, so `PartialEq` on the
/// struct would be the wrong tool for "did the user change something".
fn settings_equal(a: &crate::config::UiSettings, b: &crate::config::UiSettings) -> bool {
    a.theme == b.theme
        && a.html_backend == b.html_backend
        && a.load_remote_content == b.load_remote_content
        && a.poll_interval_secs == b.poll_interval_secs
        && a.initial_sync_count == b.initial_sync_count
        && a.compact_list == b.compact_list
        && (a.font_size - b.font_size).abs() < f32::EPSILON
        && (a.mark_read_after_secs - b.mark_read_after_secs).abs() < f32::EPSILON
        && a.folders == b.folders
        && a.messages == b.messages
        && a.reading == b.reading
}
