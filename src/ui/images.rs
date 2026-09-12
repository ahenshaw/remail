//! Image sourcing for the reader.
//!
//! Three kinds of `<img src>` survive sanitization, and each resolves
//! differently:
//!
//! * `cid:` — a part of this very message, already in memory.
//! * `data:` — inline base64, decoded once and cached.
//! * `http(s):` — fetched over the network, and only when the user has said
//!   this message may load remote content.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::html::native::{ImageSource, TextureCache};
use crate::mail::MessageBody;

/// Image bytes by URL. `None` records a permanent failure, so a URL that
/// cannot be fetched is not retried on every frame.
type FetchedImages = HashMap<String, Option<Arc<Vec<u8>>>>;

/// Fetches remote images in the background, caching results by URL.
pub struct RemoteImages {
    runtime: tokio::runtime::Handle,
    results: Arc<Mutex<FetchedImages>>,
    inflight: HashSet<String>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl RemoteImages {
    pub fn new(
        runtime: tokio::runtime::Handle,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            results: Arc::new(Mutex::new(HashMap::new())),
            inflight: HashSet::new(),
            repaint: Arc::new(repaint),
        }
    }

    /// Returns the bytes for a URL, starting a fetch on first request.
    fn get(&mut self, url: &str) -> Option<Arc<Vec<u8>>> {
        if let Some(entry) = self.results.lock().unwrap().get(url) {
            return entry.clone();
        }
        if !self.inflight.insert(url.to_string()) {
            return None;
        }

        let url = url.to_string();
        let results = self.results.clone();
        let repaint = self.repaint.clone();
        self.runtime.spawn(async move {
            let bytes = fetch(&url).await;
            results.lock().unwrap().insert(url, bytes.map(Arc::new));
            repaint();
        });
        None
    }

    /// Drops cached results, e.g. when the user opens a different message.
    pub fn clear(&mut self) {
        self.results.lock().unwrap().clear();
        self.inflight.clear();
    }
}

/// Caps on what a remote image fetch may cost, so a hostile sender cannot
/// stall the reader or exhaust memory.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

async fn fetch(url: &str) -> Option<Vec<u8>> {
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build().ok()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    if response.content_length().is_some_and(|n| n as usize > MAX_IMAGE_BYTES) {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    (bytes.len() <= MAX_IMAGE_BYTES).then(|| bytes.to_vec())
}

/// Resolves the images of one open message.
pub struct BodyImages<'a> {
    pub body: &'a MessageBody,
    pub textures: &'a mut TextureCache,
    pub remote: &'a mut RemoteImages,
    /// Whether this message is allowed to load remote content.
    pub allow_remote: bool,
}

impl ImageSource for BodyImages<'_> {
    fn texture(&mut self, ctx: &egui::Context, src: &str) -> Option<egui::TextureHandle> {
        if self.textures.contains(src) {
            return self.textures.get_or_decode(ctx, src, &[]);
        }

        if let Some(cid) = src.strip_prefix("cid:") {
            let cid = cid.trim_matches(['<', '>']);
            let part = self.body.inline.iter().find(|p| p.content_id == cid)?;
            return self.textures.get_or_decode(ctx, src, &part.data);
        }

        if src.starts_with("data:") {
            let bytes = decode_data_url(src)?;
            return self.textures.get_or_decode(ctx, src, &bytes);
        }

        if self.allow_remote && (src.starts_with("http://") || src.starts_with("https://")) {
            let bytes = self.remote.get(src)?;
            return self.textures.get_or_decode(ctx, src, &bytes);
        }

        None
    }
}

/// Decodes `data:image/png;base64,…`. Only base64 payloads are accepted;
/// percent-encoded data URLs do not appear in mail.
fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let (meta, payload) = url.strip_prefix("data:")?.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    if payload.len() > MAX_IMAGE_BYTES {
        return None;
    }
    STANDARD.decode(payload.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_data_urls() {
        // "hi" in base64.
        let bytes = decode_data_url("data:image/png;base64,aGk=").unwrap();
        assert_eq!(bytes, b"hi");
    }

    #[test]
    fn rejects_non_base64_data_urls() {
        assert!(decode_data_url("data:image/png,%89PNG").is_none());
        assert!(decode_data_url("https://example.com/a.png").is_none());
    }
}
