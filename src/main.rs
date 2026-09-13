//! remail — a fast IMAP and Gmail client.

// Windows would otherwise open a console behind the window. Debug builds keep
// it, because that is where the tracing output goes.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod auth;
mod config;
mod html;
mod mail;
mod secrets;
mod ui;

use anyhow::{Context as _, Result};

/// Directives applied when `REMAIL_LOG` is unset.
///
/// Other crates' warnings are worth seeing, so the default level is `warn`.
/// Two are quietened because they report things the user cannot act on and
/// report them constantly:
///
/// * `fontdb` logs once per font file it cannot read, on every startup. The
///   outcome that would matter — no fonts found at all — is reported by
///   `FontLibrary` itself.
/// * `html5ever` warns "foster parenting not implemented" whenever a message
///   puts content directly inside a `<table>`, which a great deal of mail
///   does. It affects where that content lands in the parsed tree, not
///   whether it is sanitized, and there is nothing to be done about someone
///   else's markup from in here.
const DEFAULT_LOG: &str = "remail=info,warn,fontdb=error,html5ever=error";

fn log_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_env("REMAIL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_directives_are_well_formed() {
        // A malformed directive is dropped silently, so the filter is only
        // as good as this check.
        let rendered = tracing_subscriber::EnvFilter::new(DEFAULT_LOG).to_string();
        for directive in DEFAULT_LOG.split(',') {
            assert!(
                rendered.contains(directive),
                "directive {directive:?} did not survive parsing: {rendered}"
            );
        }
    }

    /// Records the events a subscriber actually lets through.
    #[derive(Clone, Default)]
    struct Seen(std::sync::Arc<std::sync::Mutex<Vec<(String, tracing::Level)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Seen {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let metadata = event.metadata();
            self.0.lock().unwrap().push((metadata.target().to_string(), *metadata.level()));
        }
    }

    #[test]
    fn the_noisy_dependencies_are_quiet_at_the_default_level() {
        use tracing::Level;
        use tracing_subscriber::layer::SubscriberExt as _;

        let seen = Seen::default();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(DEFAULT_LOG))
            .with(seen.clone());

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "html5ever::tree_builder", "foster parenting not implemented");
            tracing::warn!(target: "fontdb", "Failed to load a font");
            tracing::error!(target: "html5ever::tree_builder", "something that did go wrong");
            tracing::warn!(target: "async_imap", "the server said something odd");
            tracing::info!(target: "remail::mail::engine", "connected");
        });

        let seen = seen.0.lock().unwrap().clone();
        let got = |target: &str, level: Level| seen.iter().any(|(t, l)| t == target && *l == level);

        // The two that report constantly and say nothing actionable.
        assert!(!got("html5ever::tree_builder", Level::WARN), "{seen:?}");
        assert!(!got("fontdb", Level::WARN), "{seen:?}");

        // Quietened, not silenced: a real failure still gets through.
        assert!(got("html5ever::tree_builder", Level::ERROR), "{seen:?}");
        // And nothing else was caught by the same net.
        assert!(got("async_imap", Level::WARN), "{seen:?}");
        assert!(got("remail::mail::engine", Level::INFO), "{seen:?}");
    }

    #[test]
    fn the_environment_overrides_the_default() {
        // SAFETY: single-threaded test setting a variable it also removes.
        unsafe { std::env::set_var("REMAIL_LOG", "remail=trace") };
        assert!(log_filter().to_string().contains("remail=trace"));
        unsafe { std::env::remove_var("REMAIL_LOG") };
        assert!(log_filter().to_string().contains("fontdb=error"));
    }
}
