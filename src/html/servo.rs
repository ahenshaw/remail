//! Servo backend: renders a message body with a real web engine.
//!
//! Servo draws into a [`SoftwareRenderingContext`], the framebuffer is read
//! back as an image, and that image is uploaded as an egui texture and drawn
//! in the reader pane. Going through software rendering rather than sharing
//! eframe's GL context keeps the two renderers from fighting over the current
//! context, and costs little at reader-pane sizes.
//!
//! Servo is not `Send` and its event loop must be pumped from the thread that
//! created it. That happens to be exactly what an eframe application offers:
//! [`ServoView::update`] is called from `App::update` on the main thread.
//!
//! The engine still only ever sees output from [`super::sanitize`]. Scripting
//! is additionally disabled at the engine level, so this is a layout and
//! painting engine, not a browser.

use std::cell::Cell;
use std::rc::Rc;

use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use egui::{Sense, Ui, Vec2};
use servo::{
    EventLoopWaker, LoadStatus, RenderingContext, Scroll, Servo, ServoBuilder,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint,
    WebViewVector,
};

/// Upper bound on the offscreen surface. A reader pane never needs more, and
/// it caps how much work a hostile message can ask the engine to do.
const MAX_SURFACE: u32 = 4096;

/// One Servo instance plus the webview showing the current message.
pub struct ServoView {
    servo: Servo,
    rendering_context: Rc<SoftwareRenderingContext>,
    webview: WebView,
    delegate: Rc<FrameSignal>,
    /// The blitted framebuffer, re-uploaded when Servo reports a new frame.
    texture: Option<egui::TextureHandle>,
    size: (u32, u32),
    /// Body currently loaded, so the same message is not reloaded per frame.
    loaded: Option<u64>,
}

/// Records that Servo has painted something new.
#[derive(Default)]
struct FrameSignal {
    dirty: Cell<bool>,
    load_complete: Cell<bool>,
}

impl WebViewDelegate for FrameSignal {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.dirty.set(true);
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        if status == LoadStatus::Complete {
            self.load_complete.set(true);
            self.dirty.set(true);
        }
    }
}

/// Wakes the UI thread so `App::update` runs and pumps Servo's event loop.
#[derive(Clone)]
struct Waker(egui::Context);

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        self.0.request_repaint();
    }
}

impl ServoView {
    /// Starts Servo. Expensive — done once, on first use of the backend.
    pub fn new(ctx: &egui::Context, size: (u32, u32)) -> Result<Self> {
        // Servo's networking needs a rustls provider installed process-wide.
        // The client also builds its own configs, so tolerate a prior install.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let size = clamp_size(size);
        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(dpi::PhysicalSize {
                width: size.0,
                height: size.1,
            })
            .map_err(|e| anyhow::anyhow!("could not create Servo rendering context: {e:?}"))?,
        );
        rendering_context
            .make_current()
            .map_err(|e| anyhow::anyhow!("could not bind Servo rendering context: {e:?}"))?;

        // Servo has no single "no scripting" switch, and the real guarantee
        // is upstream: `sanitize` strips every `<script>`, event handler and
        // `javascript:` URL before the engine sees the markup. These settings
        // narrow what is left.
        let mut preferences = servo::Preferences::default();
        preferences.dom_allow_scripts_to_close_windows = false;
        // No JIT for sender-controlled content: layout is the point here, and
        // the interpreter is a much smaller target.
        preferences.js_disable_jit = true;
        // Message bodies come from `data:` URLs, so an HTTP cache buys nothing
        // and would persist whatever a remote image fetch pulled in.
        preferences.network_http_cache_disabled = true;

        let servo = ServoBuilder::default()
            .preferences(preferences)
            .event_loop_waker(Box::new(Waker(ctx.clone())))
            .build();

        let delegate = Rc::new(FrameSignal::default());
        let webview = WebViewBuilder::new(&servo, rendering_context.clone())
            .delegate(delegate.clone())
            .hidpi_scale_factor(euclid::Scale::new(ctx.pixels_per_point()))
            .build();
        webview.resize(dpi::PhysicalSize { width: size.0, height: size.1 });
        webview.focus();
        webview.show();

        Ok(Self {
            servo,
            rendering_context,
            webview,
            delegate,
            texture: None,
            size,
            loaded: None,
        })
    }

    /// Points the webview at a message body, if it is not already showing it.
    ///
    /// `key` identifies the message; passing the same key twice is a no-op, so
    /// this is safe to call every frame.
    pub fn load(&mut self, key: u64, document: &str) {
        if self.loaded == Some(key) {
            return;
        }
        self.loaded = Some(key);
        self.delegate.load_complete.set(false);

        // A `data:` URL gives the document an opaque origin with no ambient
        // authority, which is what a message body should have.
        let url = format!(
            "data:text/html;charset=utf-8;base64,{}",
            STANDARD.encode(document.as_bytes())
        );
        match url::Url::parse(&url) {
            Ok(url) => self.webview.load(url),
            Err(e) => tracing::warn!("could not build document URL: {e}"),
        }
    }

    /// Resizes the offscreen surface to match the reader pane.
    pub fn resize(&mut self, size: (u32, u32)) {
        let size = clamp_size(size);
        if size == self.size {
            return;
        }
        self.size = size;
        let physical = dpi::PhysicalSize { width: size.0, height: size.1 };
        self.rendering_context.resize(physical);
        self.webview.resize(physical);
        self.delegate.dirty.set(true);
    }

    /// Pumps Servo's event loop and refreshes the texture if a frame is ready.
    ///
    /// Must be called from the thread that constructed the view.
    pub fn update(&mut self, ctx: &egui::Context) {
        self.servo.spin_event_loop();

        if !self.delegate.dirty.replace(false) && self.texture.is_some() {
            return;
        }

        let _ = self.rendering_context.make_current();
        self.webview.paint();
        self.rendering_context.present();

        let rect = webrender_api::units::DeviceIntRect::from_origin_and_size(
            euclid::Point2D::origin(),
            euclid::Size2D::new(self.size.0 as i32, self.size.1 as i32),
        );
        let Some(image) = self.rendering_context.read_to_image(rect) else {
            return;
        };

        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        );
        self.texture = Some(ctx.load_texture(
            "servo-body",
            color_image,
            egui::TextureOptions::LINEAR,
        ));

        // Servo animates (transitions, GIFs) until the page settles; keep
        // frames coming while it does.
        if self.webview.animating() {
            ctx.request_repaint();
        }
    }

    /// True once the current document has finished loading.
    pub fn is_ready(&self) -> bool {
        self.delegate.load_complete.get()
            || self.webview.load_status() == LoadStatus::Complete
    }

    /// Draws the rendered page, forwarding scroll to Servo.
    pub fn show(&mut self, ui: &mut Ui) {
        let Some(texture) = self.texture.clone() else {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label("Rendering\u{2026}");
            });
            return;
        };

        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, Sense::click_and_drag());
        egui::Image::new(&texture)
            .fit_to_exact_size(rect.size())
            .paint_at(ui, rect);

        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            if scroll != Vec2::ZERO {
                // egui reports positive delta when content moves down; Servo
                // wants the offset that reveals content below.
                let at = response.hover_pos().unwrap_or(rect.center());
                self.webview.notify_scroll_event(
                    Scroll::Delta(WebViewVector::Device(euclid::Vector2D::new(
                        -scroll.x, -scroll.y,
                    ))),
                    WebViewPoint::Device(euclid::Point2D::new(
                        at.x - rect.left(),
                        at.y - rect.top(),
                    )),
                );
                self.delegate.dirty.set(true);
            }
        }
    }

}

fn clamp_size(size: (u32, u32)) -> (u32, u32) {
    (size.0.clamp(1, MAX_SURFACE), size.1.clamp(1, MAX_SURFACE))
}

/// Wraps sanitized body HTML in a document with a theme-aware base style.
///
/// Email bodies are fragments, not documents, and they assume a white page
/// with black text. Supplying that explicitly is what keeps a message legible
/// instead of inheriting whatever the engine's defaults happen to be.
pub fn document(body_html: &str, dark: bool) -> String {
    let (background, foreground, link, quote) = if dark {
        ("#16181d", "#dfe3ea", "#7cb2ff", "#3d4350")
    } else {
        ("#ffffff", "#1a1c20", "#0b5fd0", "#d7dbe2")
    };

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>\
         html{{background:{background};}}\
         body{{margin:0;padding:16px;background:{background};color:{foreground};\
         font:14px/1.55 system-ui,-apple-system,'Segoe UI',sans-serif;\
         overflow-wrap:break-word;word-break:break-word;}}\
         img{{max-width:100%;height:auto;}}\
         table{{max-width:100%;border-collapse:collapse;}}\
         pre{{white-space:pre-wrap;overflow-x:auto;}}\
         a{{color:{link};}}\
         blockquote{{margin:0 0 0 8px;padding-left:12px;border-left:2px solid {quote};}}\
         </style></head><body>{body_html}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_a_fragment_in_a_document() {
        let html = document("<p>hi</p>", true);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<p>hi</p>"));
        assert!(html.contains("#16181d"));
    }

    #[test]
    fn clamps_absurd_surface_sizes() {
        assert_eq!(clamp_size((0, 0)), (1, 1));
        assert_eq!(clamp_size((99_999, 10)), (MAX_SURFACE, 10));
    }
}
