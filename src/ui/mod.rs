//! User interface: a three-pane reader built with egui.
//!
//! The panes are independent modules that render from application state and
//! return an [`Action`] describing what the user asked for. The app applies
//! actions in one place, which keeps command dispatch and borrow scopes
//! simple.

pub mod accounts;
pub mod compose;
pub mod fonts;
pub mod icons;
pub mod images;
pub mod message_list;
pub mod move_to;
pub mod reader;
pub mod sidebar;

use egui::{Color32, FontId};

use crate::config::AccountId;
use crate::mail::RowKey;

/// Something the user did that the app needs to act on.
#[derive(Debug, Clone)]
pub enum Action {
    /// Show a mailbox.
    OpenMailbox {
        account: AccountId,
        mailbox: String,
    },
    /// Bring an account online.
    Connect(AccountId),
    /// Start the interactive OAuth flow.
    SignIn(AccountId),
    /// Move the keyboard cursor to a message and load it.
    Focus(RowKey),
    /// Extend the multi-selection to a message.
    ToggleSelected(RowKey),
    /// Select a contiguous run ending at a message.
    SelectRange(RowKey),
    ToggleStar(RowKey),
    /// Flip `\Seen` on the current selection.
    ToggleRead,
    Archive,
    Delete,
    Reply {
        all: bool,
    },
    Forward,
    Compose,
    /// Re-sync the open mailbox.
    Refresh,
    /// Run a server-side search for the current query.
    SearchServer(String),
    /// Load the remote images this message asked for, and remember the
    /// decision for this message.
    LoadRemoteImages,
    /// Trust this sender's remote content from now on.
    AllowRemoteSender,
    /// Open a URL in the system browser.
    OpenUrl(String),
    /// Write an attachment to disk.
    SaveAttachment(usize),
    /// Hand the open message to the system's print path.
    Print,
    /// Discard the current listing's search filter.
    ClearSearch,
    /// Mark every message in a mailbox as read.
    MarkFolderRead {
        account: AccountId,
        mailbox: String,
    },
    /// Open the dialog for a new folder under this parent.
    NewSubfolder {
        account: AccountId,
        parent: String,
    },
    /// Open the dialog to rename this folder.
    RenameFolder {
        account: AccountId,
        mailbox: String,
    },
    /// Ask to delete this folder.
    DeleteFolder {
        account: AccountId,
        mailbox: String,
    },
    /// Show or hide a folder's children.
    ToggleFolder {
        account: AccountId,
        mailbox: String,
    },
    /// Show or hide an account's whole folder list.
    ToggleAccount(AccountId),
    /// Ask where to move the selected messages.
    MoveTo,
    /// Messages were dropped on a folder in the sidebar.
    DropOnFolder {
        account: AccountId,
        mailbox: String,
        rows: Vec<crate::mail::RowKey>,
    },
}

/// What a drag out of the message list carries.
///
/// The account travels with the rows because a move is one IMAP session
/// acting on one server: dropping a message on another account's folder is
/// not a move at all, and the sidebar has to be able to tell.
#[derive(Debug, Clone)]
pub struct DraggedMessages {
    pub account: AccountId,
    pub rows: Vec<crate::mail::RowKey>,
}

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};

/// Formats a timestamp the way a message list wants it: time for today,
/// weekday for this week, month and day for this year, full date beyond.
pub fn format_date_short(timestamp: i64) -> String {
    let Some(when) = to_local(timestamp) else { return String::new() };
    let now = Local::now();
    let age = now.signed_duration_since(when);

    if age.num_hours() < 24 && when.day() == now.day() {
        when.format("%H:%M").to_string()
    } else if age.num_days() < 7 && age.num_seconds() >= 0 {
        when.format("%a %H:%M").to_string()
    } else if when.year() == now.year() {
        when.format("%-d %b").to_string()
    } else {
        when.format("%-d %b %Y").to_string()
    }
}

/// Full date and time, for the reader header and quoted attributions.
pub fn format_date_long(timestamp: i64) -> String {
    match to_local(timestamp) {
        Some(when) => when.format("%a, %-d %b %Y at %H:%M").to_string(),
        None => "unknown date".to_string(),
    }
}

fn to_local(timestamp: i64) -> Option<DateTime<Local>> {
    if timestamp <= 0 {
        return None;
    }
    Utc.timestamp_opt(timestamp, 0).single().map(|t| t.with_timezone(&Local))
}

/// Human-readable byte count for attachment sizes.
pub fn format_size(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Draws one line of text, truncating with an ellipsis at `max_width`.
///
/// Shared by the panes that paint their own rows, so folder names and subject
/// lines shorten the same way when a pane is narrow. Returns whether the text
/// had to be shortened, which callers use to decide if a tooltip would tell
/// the reader anything they cannot already see.
/// Where the ink sits inside a line of text.
///
/// The nominal font size says nothing about this: a line box reserves room
/// for ascenders and descenders, so its centre is not where the letters look
/// centred, and its top is not where they start.
pub struct TextMetrics {
    /// Baseline, measured down from the top of the line box.
    baseline: f32,
    /// Height of a capital letter, measured from the ink of an "X".
    cap_height: f32,
}

impl TextMetrics {
    /// The y that a mark beside the text should be centred on.
    ///
    /// The middle of the capitals, which is what the eye reads as the middle
    /// of a line of text. Two other answers are available and both are wrong:
    /// the middle of the line box sits well under the letters, because the
    /// box reserves a descender's worth of space that most words never use;
    /// and standing a mark on the baseline leaves it riding high whenever it
    /// is taller than a capital, which an icon usually is.
    pub fn caps_centre(&self, top: f32) -> f32 {
        top + self.baseline - self.cap_height * 0.5
    }

    /// Where to draw a galley so its capitals are centred on `centre`.
    ///
    /// Centring the line box instead only works for a font whose baseline
    /// sits at the middle of the capitals — which the bundled face does,
    /// almost exactly, and most others do not. Candara's box is half as tall
    /// again as its capitals and hangs well below them, so centring the box
    /// leaves the letters pressed against the top of the row.
    pub fn top_for_centred_caps(&self, centre: f32) -> f32 {
        centre + self.cap_height * 0.5 - self.baseline
    }

    pub fn measure(painter: &egui::Painter, font: &FontId) -> Self {
        let galley = painter.layout_no_wrap("X".to_string(), font.clone(), Color32::PLACEHOLDER);
        let glyph = galley.rows.first().and_then(|row| {
            row.row.glyphs.first().map(|glyph| (row.pos.y + glyph.pos.y, glyph.uv_rect.size.y))
        });
        // A font with no glyph for "X" is not worth a special case; the
        // proportions of a typical face are close enough to keep going.
        let (baseline, cap_height) = glyph.unwrap_or((font.size * 0.8, font.size * 0.7));
        Self { baseline, cap_height }
    }
}

pub fn paint_truncated(
    painter: &egui::Painter,
    position: egui::Pos2,
    max_width: f32,
    text: &str,
    font: FontId,
    color: Color32,
) -> bool {
    if text.is_empty() || max_width <= 8.0 {
        return false;
    }
    let mut galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
    let mut truncated = false;

    if galley.size().x > max_width {
        truncated = true;
        // Binary search the longest prefix that fits, on character
        // boundaries so multi-byte text never splits mid-character.
        let chars: Vec<char> = text.chars().collect();
        let mut low = 0usize;
        let mut high = chars.len();
        while low < high {
            let mid = (low + high).div_ceil(2);
            let candidate: String = chars[..mid].iter().collect::<String>() + "\u{2026}";
            let width = painter.layout_no_wrap(candidate, font.clone(), color).size().x;
            if width <= max_width {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let shortened: String = chars[..low].iter().collect::<String>() + "\u{2026}";
        galley = painter.layout_no_wrap(shortened, font, color);
    }
    painter.galley(position, galley, color);
    truncated
}

/// A selection or hover tint drawn from the theme's accent.
///
/// `subtlety` runs from 0 (the accent at full strength) to 1 (invisible
/// against the surface). Mixing towards the card colour rather than
/// brightening or darkening keeps the result readable on light and dark
/// themes alike, where a fixed adjustment would go the wrong way on one.
pub fn accent_tint(palette: &elegance::Palette, subtlety: f32) -> Color32 {
    mix(palette.blue, palette.card, subtlety)
}

/// Pushes a colour further from the page and harder to miss.
///
/// `amount` runs from 0 (unchanged) to 1 (plain body text). The target is the
/// theme's text colour rather than black, for the reason `accent_tint` mixes
/// towards the card: on Slate and Charcoal the body text is the light end of
/// the scale, and darkening there would sink the text into its own
/// background. Deepening is the same gesture in both polarities — away from
/// the surface, towards whatever this theme reads as ink.
pub fn deepen(color: Color32, palette: &elegance::Palette, amount: f32) -> Color32 {
    mix(color, palette.text, amount)
}

/// Blends two colours in linear space. Used to recede an accent colour
/// towards the body text colour without depending on the theme's polarity,
/// which `gamma_multiply` alone cannot do.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let blend = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Color32::from_rgb(blend(a.r(), b.r()), blend(a.g(), b.g()), blend(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn mixes_towards_the_second_colour() {
        let black = Color32::from_rgb(0, 0, 0);
        let white = Color32::from_rgb(255, 255, 255);
        assert_eq!(mix(black, white, 0.0), black);
        assert_eq!(mix(black, white, 1.0), white);
        assert_eq!(mix(black, white, 0.5), Color32::from_rgb(128, 128, 128));
    }

    /// Brightness has to be judged whole, not channel by channel: the blue
    /// accent's red is darker than the body text's, so deepening on a light
    /// theme lifts that one channel while the colour overall goes down.
    fn luminance(c: Color32) -> f32 {
        0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32
    }

    /// The point of deepening towards the theme's text rather than towards
    /// black: on a dark theme the same call has to lighten, or it would push
    /// the text into its own background.
    #[test]
    fn deepening_follows_the_theme_polarity() {
        let accent = Color32::from_rgb(0x0f, 0x6c, 0xbd);

        let mut light = elegance::Theme::frost().palette;
        light.is_dark = false;
        light.text = Color32::from_rgb(0x24, 0x24, 0x24);
        assert!(luminance(deepen(accent, &light, 0.25)) < luminance(accent));

        let mut dark = elegance::Theme::slate().palette;
        dark.is_dark = true;
        dark.text = Color32::from_rgb(0xe6, 0xe6, 0xe6);
        assert!(luminance(deepen(accent, &dark, 0.25)) > luminance(accent));

        assert_eq!(deepen(accent, &light, 0.0), accent);
        assert_eq!(deepen(accent, &light, 1.0), light.text);
    }

    #[test]
    fn mixing_clamps_out_of_range_factors() {
        let a = Color32::from_rgb(10, 20, 30);
        let b = Color32::from_rgb(200, 210, 220);
        assert_eq!(mix(a, b, -1.0), a);
        assert_eq!(mix(a, b, 5.0), b);
    }

    #[test]
    fn mixes_each_channel_independently() {
        let a = Color32::from_rgb(0, 100, 200);
        let b = Color32::from_rgb(100, 200, 0);
        assert_eq!(mix(a, b, 0.5), Color32::from_rgb(50, 150, 100));
    }

    #[test]
    fn renders_no_date_for_missing_timestamps() {
        assert_eq!(format_date_short(0), "");
        assert_eq!(format_date_long(0), "unknown date");
    }
}

#[cfg(test)]
pub mod raster {

    /// A minimal software rasteriser for egui's output.
    ///
    /// Enough to look at what the interface actually draws, without a window
    /// or a GPU: textured, vertex-coloured triangles into an RGBA buffer.
    struct Canvas {
        width: usize,
        height: usize,
        pixels: Vec<[f32; 4]>,
        font: Option<(usize, usize, Vec<f32>)>,
    }

    impl Canvas {
        fn new(width: usize, height: usize, ground: egui::Color32) -> Self {
            let [r, g, b, _] = ground.to_normalized_gamma_f32();
            Self { width, height, pixels: vec![[r, g, b, 1.0]; width * height], font: None }
        }

        fn set_font(&mut self, image: &egui::ColorImage) {
            let coverage = image.pixels.iter().map(|p| p.a() as f32 / 255.0).collect();
            self.font = Some((image.width(), image.height(), coverage));
        }

        fn sample(&self, u: f32, v: f32) -> f32 {
            let Some((w, h, data)) = &self.font else { return 1.0 };
            let x = ((u * *w as f32).round() as isize).clamp(0, *w as isize - 1) as usize;
            let y = ((v * *h as f32).round() as isize).clamp(0, *h as isize - 1) as usize;
            data[y * w + x]
        }

        fn blend(&mut self, x: usize, y: usize, rgba: [f32; 4]) {
            let dst = &mut self.pixels[y * self.width + x];
            let a = rgba[3];
            for c in 0..3 {
                dst[c] = rgba[c] * a + dst[c] * (1.0 - a);
            }
        }

        fn triangle(&mut self, v: [&egui::epaint::Vertex; 3], textured: bool, scale: f32) {
            let pt = |p: egui::Pos2| (p.x * scale, p.y * scale);
            let (x0, y0) = pt(v[0].pos);
            let (x1, y1) = pt(v[1].pos);
            let (x2, y2) = pt(v[2].pos);

            let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
            if area.abs() < 1e-6 {
                return;
            }

            let min_x = x0.min(x1).min(x2).floor().max(0.0) as usize;
            let max_x = (x0.max(x1).max(x2).ceil() as usize).min(self.width - 1);
            let min_y = y0.min(y1).min(y2).floor().max(0.0) as usize;
            let max_y = (y0.max(y1).max(y2).ceil() as usize).min(self.height - 1);

            // 2x2 supersampling, so edges are not a staircase.
            const S: usize = 2;
            for py in min_y..=max_y {
                for px in min_x..=max_x {
                    let mut hits = 0.0;
                    let mut acc = [0.0_f32; 4];
                    for sy in 0..S {
                        for sx in 0..S {
                            let x = px as f32 + (sx as f32 + 0.5) / S as f32;
                            let y = py as f32 + (sy as f32 + 0.5) / S as f32;
                            let w0 = ((x1 - x) * (y2 - y) - (x2 - x) * (y1 - y)) / area;
                            let w1 = ((x2 - x) * (y0 - y) - (x0 - x) * (y2 - y)) / area;
                            let w2 = 1.0 - w0 - w1;
                            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                                continue;
                            }
                            let mut colour = [0.0_f32; 4];
                            for (weight, vertex) in [(w0, v[0]), (w1, v[1]), (w2, v[2])] {
                                let c = vertex.color.to_normalized_gamma_f32();
                                for i in 0..4 {
                                    colour[i] += weight * c[i];
                                }
                            }
                            if textured {
                                let mut uv = [0.0_f32; 2];
                                for (weight, vertex) in [(w0, v[0]), (w1, v[1]), (w2, v[2])] {
                                    uv[0] += weight * vertex.uv.x;
                                    uv[1] += weight * vertex.uv.y;
                                }
                                colour[3] *= self.sample(uv[0], uv[1]);
                            }
                            for i in 0..4 {
                                acc[i] += colour[i];
                            }
                            hits += 1.0;
                        }
                    }
                    if hits > 0.0 {
                        let n = (S * S) as f32;
                        let mut colour = [0.0_f32; 4];
                        for i in 0..4 {
                            colour[i] = acc[i] / hits;
                        }
                        colour[3] *= hits / n;
                        self.blend(px, py, colour);
                    }
                }
            }
        }

        fn write_png(&self, path: &str) {
            let mut rgba = Vec::with_capacity(self.width * self.height * 4);
            for pixel in &self.pixels {
                for channel in &pixel[..3] {
                    rgba.push((channel.clamp(0.0, 1.0) * 255.0).round() as u8);
                }
                rgba.push(255);
            }
            image::save_buffer(
                path,
                &rgba,
                self.width as u32,
                self.height as u32,
                image::ColorType::Rgba8,
            )
            .expect("writing the png");
        }
    }

    /// Renders `build` and writes it to `path`, magnified.
    /// Renders `build` to a PNG at `path`, magnified by `scale`.
    ///
    /// `theme` is installed into the context, so widgets are painted the way
    /// the application paints them rather than in whatever the default
    /// happens to be — which is why several of these came out washed out.
    pub fn render(
        path: &str,
        theme: &elegance::Theme,
        width: f32,
        height: f32,
        scale: f32,
        build: impl Fn(&mut egui::Ui),
    ) {
        let ctx = egui::Context::default();
        theme.clone().install(&ctx);

        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(width, height),
            )),
            ..Default::default()
        };

        let mut canvas =
            Canvas::new((width * scale) as usize, (height * scale) as usize, theme.palette.bg);

        // Two passes: the first uploads the font atlas, and only the first,
        // so the glyphs have to be taken from it rather than from the second.
        let mut first = ctx.run_ui(raw.clone(), |ui| build(ui));
        for (id, deltas) in &first.textures_delta.set {
            if *id != egui::TextureId::Managed(0) {
                continue;
            }
            for delta in deltas.iter() {
                let egui::ImageData::Color(image) = &delta.image;
                canvas.set_font(image);
            }
        }
        first.textures_delta.clear();

        let mut output = ctx.run_ui(raw, |ui| build(ui));
        output.textures_delta.clear();

        for primitive in ctx.tessellate(output.shapes, 1.0) {
            let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive else { continue };
            let textured = mesh.texture_id == egui::TextureId::Managed(0);
            let (triangles, _) = mesh.indices.as_chunks::<3>();
            for triangle in triangles {
                let v = [
                    &mesh.vertices[triangle[0] as usize],
                    &mesh.vertices[triangle[1] as usize],
                    &mesh.vertices[triangle[2] as usize],
                ];
                // Rects use a fixed white texel; only glyph meshes sample.
                let glyph =
                    textured && v.iter().any(|vertex| vertex.uv.x > 0.001 || vertex.uv.y > 0.001);
                canvas.triangle(v, glyph, scale);
            }
        }

        canvas.write_png(path);
        println!("wrote {path}");
    }
}
