//! remail — a fast IMAP and Gmail client.

mod app;
mod auth;
mod config;
mod html;
mod mail;
mod secrets;
mod ui;

use anyhow::{Context as _, Result};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("REMAIL_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("remail=info,warn")),
        )
        .init();

    let config = config::Config::load().unwrap_or_else(|e| {
        tracing::warn!("could not read configuration, starting with defaults: {e}");
        config::Config::default()
    });

    // The cache is an optimization, never a requirement: if the data directory
    // is unusable, run from memory rather than refusing to start.
    let store = match config::data_dir().map(|dir| dir.join("cache.sqlite")) {
        Ok(path) => mail::Store::open(&path).or_else(|e| {
            tracing::warn!("could not open the message cache at {}: {e}", path.display());
            mail::Store::open_memory()
        }),
        Err(e) => {
            tracing::warn!("no data directory available: {e}");
            mail::Store::open_memory()
        }
    }
    .context("opening the message cache")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("remail")
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([760.0, 480.0])
            .with_app_id("remail"),
        ..Default::default()
    };

    eframe::run_native(
        "remail",
        options,
        Box::new(move |cc| {
            let app = app::RemailApp::new(&cc.egui_ctx, config, store)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not start the window: {e}"))
}
