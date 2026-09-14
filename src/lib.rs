//! remail — a fast IMAP and Gmail client.
//!
//! The binary is a window over this library: the mail engine, its cache, the
//! query language and the rendering pipeline are all here, and none of them
//! needs a window to run. Anything that drives mail without drawing it — a
//! command-line tool, an integration test — starts from the same pieces the
//! interface does.

pub mod app;
pub mod auth;
pub mod config;
pub mod html;
pub mod mail;
pub mod secrets;
pub mod ui;

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
pub const DEFAULT_LOG: &str = "remail=info,warn,fontdb=error,html5ever=error";

pub fn log_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_env("REMAIL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG))
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
