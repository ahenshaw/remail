//! The built-in renderer: draws the block model with egui widgets.
//!
//! Everything it emits is a normal egui widget, so message text participates
//! in selection and copy, follows the application theme, and costs nothing to
//! keep on screen. Layout is single-pass and allocation-light: the expensive
//! work (sanitize, parse, lower) happened once when the body arrived.

use std::collections::HashMap;

use egui::{Color32, FontId, Label, RichText, Sense, Stroke, Ui, Vec2};

use super::layout::{Block, Document, Inline, Row, Style};

/// Supplies decoded images to the renderer. The reader implements this over
/// the message's own `cid:` parts and, when the user allows it, remote data.
pub trait ImageSource {
    /// Returns a texture for an image URL, decoding and uploading on first
    /// use. `None` means "not available", and the renderer draws a placeholder.
    fn texture(&mut self, ctx: &egui::Context, src: &str) -> Option<egui::TextureHandle>;
}

pub struct RenderOptions {
    /// Base body text size in points.
    pub base_size: f32,
    /// Family for body text. Code keeps its own monospace face regardless.
    pub family: egui::FontFamily,
    /// Upper bound on image width, so a 2000px banner does not blow out the
    /// reader pane.
    pub max_image_width: f32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            base_size: 14.0,
            family: egui::FontFamily::Proportional,
            max_image_width: 720.0,
        }
    }
}

/// Draws a document. Returns a URL if the user activated a link this frame.
pub fn show(
    ui: &mut Ui,
    document: &Document,
    images: &mut dyn ImageSource,
    options: &RenderOptions,
) -> Option<String> {
    let mut clicked = None;
    for block in &document.blocks {
        draw_block(ui, block, images, options, &mut clicked);
    }
    clicked
}

fn draw_block(
    ui: &mut Ui,
    block: &Block,
    images: &mut dyn ImageSource,
    options: &RenderOptions,
    clicked: &mut Option<String>,
) {
    match block {
        Block::Paragraph { inlines, quote_depth } => {
            quoted(ui, *quote_depth, |ui| {
                draw_inlines(ui, inlines, options, options.base_size, images, clicked);
            });
            ui.add_space(options.base_size * 0.45);
        }

        Block::Heading { level, inlines } => {
            ui.add_space(options.base_size * 0.5);
            let size = options.base_size * heading_scale(*level);
            draw_inlines(ui, inlines, options, size, images, clicked);
            ui.add_space(options.base_size * 0.3);
        }

        Block::ListItem { depth, marker, inlines, quote_depth } => {
            quoted(ui, *quote_depth, |ui| {
                ui.horizontal_top(|ui| {
                    ui.add_space(options.base_size * (1.2 + 1.4 * *depth as f32));
                    ui.label(
                        RichText::new(marker)
                            .size(options.base_size)
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        draw_inlines(ui, inlines, options, options.base_size, images, clicked);
                    });
                });
            });
            ui.add_space(options.base_size * 0.2);
        }

        Block::Pre { text, quote_depth } => {
            quoted(ui, *quote_depth, |ui| {
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(8.0)
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        // Code keeps its own line breaks; let it scroll rather
                        // than reflow, which would change its meaning.
                        egui::ScrollArea::horizontal()
                            .id_salt(text.as_ptr() as usize)
                            .show(ui, |ui| {
                                ui.add(
                                    Label::new(
                                        RichText::new(text)
                                            .monospace()
                                            .size(options.base_size * 0.92),
                                    )
                                    .selectable(true)
                                    .wrap_mode(egui::TextWrapMode::Extend),
                                );
                            });
                    });
            });
            ui.add_space(options.base_size * 0.45);
        }

        Block::Rule => {
            ui.add_space(options.base_size * 0.3);
            ui.separator();
            ui.add_space(options.base_size * 0.3);
        }

        Block::Table { rows, quote_depth } => {
            quoted(ui, *quote_depth, |ui| {
                draw_table(ui, rows, images, options, clicked);
            });
            ui.add_space(options.base_size * 0.5);
        }
    }
}

/// Wraps content in the indent and left rule that marks quoted text.
fn quoted(ui: &mut Ui, depth: u8, add_contents: impl FnOnce(&mut Ui)) {
    if depth == 0 {
        add_contents(ui);
        return;
    }

    // Alternate the rule colour by depth so nested quotes stay distinguishable.
    let accent = quote_color(ui, depth);
    let indent = 10.0;

    ui.horizontal_top(|ui| {
        ui.add_space(indent * depth as f32);
        let rule = ui.available_rect_before_wrap();
        let response = ui
            .vertical(|ui| {
                ui.add_space(2.0);
                add_contents(ui);
            })
            .response;
        ui.painter().vline(
            rule.left() - 6.0,
            response.rect.y_range(),
            Stroke::new(2.0, accent),
        );
    });
}

fn quote_color(ui: &Ui, depth: u8) -> Color32 {
    let visuals = ui.visuals();
    match depth % 3 {
        1 => visuals.selection.bg_fill.gamma_multiply(0.9),
        2 => visuals.warn_fg_color.gamma_multiply(0.7),
        _ => visuals.weak_text_color(),
    }
}

fn draw_table(
    ui: &mut Ui,
    rows: &[Row],
    images: &mut dyn ImageSource,
    options: &RenderOptions,
    clicked: &mut Option<String>,
) {
    let columns = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    if columns == 0 {
        return;
    }

    // A one-column table is layout scaffolding, not data; flowing it avoids
    // the grid lines and column sizing that would make a newsletter look odd.
    if columns == 1 {
        for row in rows {
            for cell in &row.cells {
                draw_inlines(ui, cell, options, options.base_size, images, clicked);
            }
        }
        return;
    }

    let salt = rows.as_ptr() as usize;
    let spacing = 14.0;
    // Share the pane between columns. Without this a long cell widens the
    // grid past the viewport, and everything after it gets clipped.
    let cell_width = ((ui.available_width() - spacing * columns as f32)
        / columns as f32)
        .max(72.0);

    egui::Grid::new(("html-table", salt))
        .striped(true)
        .spacing(Vec2::new(spacing, 6.0))
        .show(ui, |ui| {
            for row in rows {
                for cell in &row.cells {
                    ui.vertical(|ui| {
                        ui.set_max_width(cell_width);
                        let size = options.base_size;
                        if row.header {
                            let bold: Vec<Inline> = cell.iter().cloned().map(embolden).collect();
                            draw_inlines(ui, &bold, options, size, images, clicked);
                        } else {
                            draw_inlines(ui, cell, options, size, images, clicked);
                        }
                    });
                }
                ui.end_row();
            }
        });
}

fn embolden(inline: Inline) -> Inline {
    match inline {
        Inline::Text { text, mut style, link } => {
            style.bold = true;
            Inline::Text { text, style, link }
        }
        other => other,
    }
}

/// Draws one inline run as wrapped text, images and links.
fn draw_inlines(
    ui: &mut Ui,
    inlines: &[Inline],
    options: &RenderOptions,
    size: f32,
    images: &mut dyn ImageSource,
    clicked: &mut Option<String>,
) {
    if inlines.is_empty() {
        return;
    }

    ui.horizontal_wrapped(|ui| {
        // Runs already carry their own spaces; widget spacing would double them.
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.set_min_height(size * 1.2);

        for inline in inlines {
            match inline {
                Inline::Break => {
                    // Force the wrap layout onto a new line.
                    ui.end_row();
                }
                Inline::Text { text, style, link } => {
                    draw_text(ui, text, style, link.as_deref(), size, &options.family, clicked);
                }
                Inline::Image { src, alt, width, height } => {
                    draw_image(ui, src, alt, *width, *height, images, options);
                }
            }
        }
    });
}

fn draw_text(
    ui: &mut Ui,
    text: &str,
    style: &Style,
    link: Option<&str>,
    size: f32,
    family: &egui::FontFamily,
    clicked: &mut Option<String>,
) {
    // Code is monospace whatever the pane is set to; its alignment carries
    // meaning that a proportional face would destroy.
    let family = if style.monospace {
        egui::FontFamily::Monospace
    } else {
        family.clone()
    };
    let mut rich = RichText::new(text).font(FontId::new(size * style.scale, family));
    if style.bold {
        rich = rich.strong();
    }
    if style.italic {
        rich = rich.italics();
    }
    if style.strike {
        rich = rich.strikethrough();
    }

    match link {
        Some(href) => {
            // Link colour comes from the theme so it stays legible in both
            // light and dark, whatever the sender specified.
            let response = ui.add(egui::Link::new(rich.color(ui.visuals().hyperlink_color)));
            if response.clicked() {
                *clicked = Some(href.to_string());
            }
            response.on_hover_text(href);
        }
        None => {
            if let Some([r, g, b]) = style.color {
                rich = rich.color(readable(ui, Color32::from_rgb(r, g, b)));
            }
            if style.underline {
                rich = rich.underline();
            }
            ui.add(Label::new(rich).selectable(true));
        }
    }
}

/// Keeps sender-specified colours from vanishing into the background.
///
/// Senders overwhelmingly design for a white background; on a dark theme a
/// literal `#111111` would be invisible. When contrast is too low, fall back
/// to the theme's text colour rather than guessing at a correction.
fn readable(ui: &Ui, color: Color32) -> Color32 {
    let background = ui.visuals().panel_fill;
    if contrast_ratio(color, background) < 2.5 {
        ui.visuals().text_color()
    } else {
        color
    }
}

fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let lighter = relative_luminance(a).max(relative_luminance(b));
    let darker = relative_luminance(a).min(relative_luminance(b));
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: Color32) -> f32 {
    let channel = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

fn draw_image(
    ui: &mut Ui,
    src: &str,
    alt: &str,
    width: Option<u32>,
    height: Option<u32>,
    images: &mut dyn ImageSource,
    options: &RenderOptions,
) {
    let ctx = ui.ctx().clone();
    let Some(texture) = images.texture(&ctx, src) else {
        draw_image_placeholder(ui, alt, options);
        return;
    };

    let native = texture.size_vec2();
    // Honour the sender's dimensions when given, but never exceed the pane.
    let requested = match (width, height) {
        (Some(w), Some(h)) => Vec2::new(w as f32, h as f32),
        (Some(w), None) => Vec2::new(w as f32, native.y * (w as f32 / native.x.max(1.0))),
        (None, Some(h)) => Vec2::new(native.x * (h as f32 / native.y.max(1.0)), h as f32),
        (None, None) => native,
    };
    let limit = options.max_image_width.min(ui.available_width().max(64.0));
    let scale = (limit / requested.x).min(1.0);

    ui.add(
        egui::Image::new(&texture)
            .fit_to_exact_size(requested * scale)
            .corner_radius(2.0),
    )
    .on_hover_text(if alt.is_empty() { src } else { alt });
}

/// Stands in for an image that was blocked or could not be decoded, so the
/// layout does not silently lose content.
fn draw_image_placeholder(ui: &mut Ui, alt: &str, options: &RenderOptions) {
    let label = if alt.trim().is_empty() { "image" } else { alt };
    let text = format!("{} {label}", crate::ui::icons::IMAGE);
    let font = FontId::proportional(options.base_size * 0.85);
    let galley = ui.painter().layout_no_wrap(
        text,
        font,
        ui.visuals().weak_text_color(),
    );

    let padding = Vec2::new(8.0, 4.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + padding * 2.0, Sense::hover());
    ui.painter().rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.5)),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.min + padding, galley, ui.visuals().weak_text_color());
}

fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.7,
        2 => 1.45,
        3 => 1.25,
        4 => 1.12,
        5 => 1.02,
        _ => 0.95,
    }
}

/// Decodes image bytes into an egui texture, caching by key.
///
/// Decoding is the expensive part and happens once per image per message;
/// repeated frames hit the cache.
#[derive(Default)]
pub struct TextureCache {
    /// `None` records a decode failure so it is not retried every frame.
    entries: HashMap<String, Option<egui::TextureHandle>>,
}

impl TextureCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns the cached texture for `key`, decoding `bytes` on first use.
    pub fn get_or_decode(
        &mut self,
        ctx: &egui::Context,
        key: &str,
        bytes: &[u8],
    ) -> Option<egui::TextureHandle> {
        if let Some(entry) = self.entries.get(key) {
            return entry.clone();
        }
        let handle = decode(ctx, key, bytes);
        self.entries.insert(key.to_string(), handle.clone());
        handle
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
}

fn decode(ctx: &egui::Context, key: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let decoded = image::load_from_memory(bytes).ok()?;
    // Cap the upload: a huge source image costs GPU memory for no visible gain.
    let decoded = if decoded.width() > 2048 || decoded.height() > 2048 {
        decoded.thumbnail(2048, 2048)
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture(key, color_image, egui::TextureOptions::LINEAR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_detects_invisible_text() {
        let black = Color32::from_rgb(0, 0, 0);
        let near_black = Color32::from_rgb(20, 20, 20);
        assert!(contrast_ratio(black, near_black) < 2.5);
        assert!(contrast_ratio(black, Color32::WHITE) > 2.5);
    }
}
