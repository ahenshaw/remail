//! The remail window.
//!
//! Everything it does lives in the library beside it; this is the shell that
//! opens a window over it.

// Windows would otherwise open a console behind the window. Debug builds keep
// it, because that is where the tracing output goes.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context as _, Result};

use remail::{app, config, log_filter, mail};

/// The window icon, decoded from the same PNG the packaging installs.
///
/// A missing icon is a cosmetic problem, so a decode failure is logged and
/// the window opens without one rather than refusing to start.
fn window_icon() -> Option<std::sync::Arc<egui::IconData>> {
    const PNG: &[u8] = include_bytes!("../assets/icons/remail-256.png");

    match image::load_from_memory(PNG) {
        Ok(image) => {
            let image = image.into_rgba8();
            let (width, height) = image.dimensions();
            Some(std::sync::Arc::new(egui::IconData { rgba: image.into_raw(), width, height }))
        }
        Err(e) => {
            tracing::warn!("could not decode the window icon: {e}");
            None
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(log_filter()).init();

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

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("remail")
        .with_inner_size([1320.0, 860.0])
        .with_min_inner_size([760.0, 480.0])
        // Wayland matches this against the .desktop file's basename to find
        // the icon; X11 and Windows use the one set below.
        .with_app_id("remail");
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

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
