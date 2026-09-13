//! System font discovery and registration.
//!
//! egui only knows about the faces it has been handed, so a family the user
//! picks has to be found on disk, read, and installed into the context before
//! it can be drawn with. Installing fonts re-rasterizes every glyph, so it
//! happens once per family and never per frame.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use egui::{Context, FontData, FontFamily};

use crate::config::PaneFont;

pub struct FontLibrary {
    db: fontdb::Database,
    /// Family names offered in the picker, sorted case-insensitively.
    families: Vec<String>,
    /// Families handed to egui so far, kept only so a reinstall can rebuild
    /// the set. The registry itself is read back from the context rather than
    /// mirrored here; see [`FontLibrary::install`].
    faces: Vec<(String, Arc<FontData>)>,
    installed: BTreeSet<String>,
    /// Families that could not be loaded, so the failure is not retried
    /// every frame.
    failed: BTreeSet<String>,
    /// Resolved families, keyed by the name asked for.
    resolved: HashMap<String, FontFamily>,
}

impl FontLibrary {
    /// Scans the system font directories. Takes tens of milliseconds, so it
    /// runs once at startup.
    pub fn load() -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();

        let mut families: Vec<String> = db
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        families.sort_by_key(|name| name.to_lowercase());
        families.dedup();

        // fontdb logs each file it cannot read; those are suppressed by
        // default because they are not actionable. Finding nothing at all is,
        // so say so once.
        if families.is_empty() {
            tracing::warn!("no system fonts found; only the built-in faces will be offered");
        } else {
            tracing::debug!("found {} font families", families.len());
        }

        Self {
            db,
            families,
            faces: Vec::new(),
            installed: BTreeSet::new(),
            failed: BTreeSet::new(),
            resolved: HashMap::new(),
        }
    }

    pub fn families(&self) -> &[String] {
        &self.families
    }

    /// Returns the family to draw a pane with, installing it on first use.
    ///
    /// A family that cannot be loaded falls back to the built-in proportional
    /// face rather than failing the frame. So does one that has been handed
    /// to egui but is not bound yet, which is why this is called every frame
    /// rather than once.
    ///
    /// Must be called during a frame: confirming that a family is bound reads
    /// the live font set.
    pub fn resolve(&mut self, ctx: &Context, font: &PaneFont) -> FontFamily {
        let name = match font {
            PaneFont::Sans => return FontFamily::Proportional,
            PaneFont::Mono => return FontFamily::Monospace,
            PaneFont::Named(name) => name,
        };

        if let Some(family) = self.resolved.get(name) {
            return family.clone();
        }
        if self.failed.contains(name) {
            return FontFamily::Proportional;
        }

        if !self.installed.contains(name) {
            if !self.install(ctx, name) {
                tracing::warn!("could not load font family {name:?}");
                self.failed.insert(name.clone());
                return FontFamily::Proportional;
            }
            // `set_fonts` only takes effect on the next pass, and laying out
            // with an unbound family panics. Draw the built-in face for now.
            ctx.request_repaint();
            return FontFamily::Proportional;
        }

        // Handed to egui on an earlier frame. Ask whether it has actually
        // been bound rather than assuming how many passes that took.
        let family = FontFamily::Name(name.as_str().into());
        if ctx.fonts(|fonts| fonts.families().contains(&family)) {
            self.resolved.insert(name.clone(), family.clone());
            return family;
        }

        ctx.request_repaint();
        FontFamily::Proportional
    }

    /// Hands a family's face data to egui. Returns whether it was found.
    ///
    /// The registry is read back out of the context and added to, rather than
    /// built up from `FontDefinitions::default()`. `set_fonts` replaces the
    /// registry outright: starting from the defaults would drop every face
    /// the defaults do not name, and the theme registers one of those — the
    /// symbols font that draws the interface's icons. Losing it left glyphs
    /// like the folder and trash marks to be found in whatever face would
    /// take them, which is a different face with different metrics, so
    /// controls sized from their text quietly changed height.
    fn install(&mut self, ctx: &Context, name: &str) -> bool {
        let Some(data) = self.face_data(name) else { return false };

        let key = format!("system:{name}");
        self.faces.push((key, Arc::new(FontData::from_owned(data))));

        let mut definitions = ctx.fonts(|fonts| fonts.definitions().clone());
        for (key, data) in &self.faces {
            definitions.font_data.insert(key.clone(), data.clone());

            // Fall back to the built-in faces so glyphs the chosen family
            // lacks — emoji, most often — still render.
            let family = key.strip_prefix("system:").unwrap_or(key);
            let mut chain = vec![key.clone()];
            chain.extend(
                definitions.families.get(&FontFamily::Proportional).cloned().unwrap_or_default(),
            );
            definitions.families.insert(FontFamily::Name(family.into()), chain);
        }

        // Re-rasterizes every glyph, which is why this is once per family.
        ctx.set_fonts(definitions);
        self.installed.insert(name.to_string());
        true
    }

    /// Reads the bytes of a family's regular upright face.
    fn face_data(&self, name: &str) -> Option<Vec<u8>> {
        let id = self.db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(name)],
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        })?;

        self.db.with_face_data(id, |data, index| {
            // epaint reads the first face in a file, so a font that lives
            // inside a collection at another index cannot be used as-is.
            (index == 0).then(|| data.to_vec())
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_families_need_no_lookup() {
        // Resolving these must not touch the database, so they are safe to
        // call before any font has been scanned.
        let mut library = FontLibrary {
            db: fontdb::Database::new(),
            families: Vec::new(),
            faces: Vec::new(),
            installed: BTreeSet::new(),
            failed: BTreeSet::new(),
            resolved: HashMap::new(),
        };
        let ctx = Context::default();
        assert_eq!(library.resolve(&ctx, &PaneFont::Sans), FontFamily::Proportional);
        assert_eq!(library.resolve(&ctx, &PaneFont::Mono), FontFamily::Monospace);
    }

    /// Installing a pane font must not cost the interface its icons.
    ///
    /// `set_fonts` replaces the whole registry, so the set handed to it has
    /// to be the one already in the context. Built from
    /// `FontDefinitions::default()` instead, it silently dropped the symbols
    /// font the theme registers: the icon glyphs then came from whatever
    /// face would take them, which changed both how they looked and how tall
    /// the controls drawn around them were.
    #[test]
    fn a_pane_font_does_not_evict_the_theme_symbols() {
        let ctx = Context::default();
        crate::config::ThemeChoice::Outlook.theme().install(&ctx);

        let mut library = FontLibrary::load();
        // Any real family will do; the point is that installing one is what
        // rewrites the registry.
        let Some(family) = library.families().first().cloned() else {
            return; // A machine with no system fonts has nothing to install.
        };

        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(200.0, 60.0),
            )),
            ..Default::default()
        };

        let registered = |ctx: &Context, key: &str| -> bool {
            ctx.fonts(|fonts| fonts.definitions().font_data.contains_key(key))
        };

        let mut out = ctx.run_ui(raw.clone(), |ui| {
            assert!(registered(ui.ctx(), "elegance-symbols"), "the theme registers them");
        });
        out.textures_delta.clear();

        // Two passes: one to hand the family over, one to see the set that
        // came back.
        for _ in 0..2 {
            let mut out = ctx.run_ui(raw.clone(), |ui| {
                let _ = library.resolve(ui.ctx(), &PaneFont::Named(family.clone()));
            });
            out.textures_delta.clear();
        }

        let mut out = ctx.run_ui(raw, |ui| {
            assert!(
                registered(ui.ctx(), &format!("system:{family}")),
                "{family} never reached the registry, so this proves nothing"
            );
            assert!(
                registered(ui.ctx(), "elegance-symbols"),
                "installing {family} evicted the theme's symbols font"
            );
        });
        out.textures_delta.clear();
    }

    /// A missing family never reaches the font set, so this stays off the
    /// path that needs a live frame.
    #[test]
    fn a_missing_family_falls_back_and_is_not_retried() {
        let mut library = FontLibrary {
            db: fontdb::Database::new(),
            families: Vec::new(),
            faces: Vec::new(),
            installed: BTreeSet::new(),
            failed: BTreeSet::new(),
            resolved: HashMap::new(),
        };
        let ctx = Context::default();
        let font = PaneFont::Named("No Such Font".into());
        assert_eq!(library.resolve(&ctx, &font), FontFamily::Proportional);
        assert!(library.failed.contains("No Such Font"));
        // Second call takes the cached-failure path.
        assert_eq!(library.resolve(&ctx, &font), FontFamily::Proportional);
    }

    #[test]
    fn system_scan_finds_families() {
        let library = FontLibrary::load();
        // A machine with no fonts at all is possible; only assert ordering.
        let names: Vec<String> = library.families().iter().map(|f| f.to_lowercase()).collect();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]), "not sorted");
    }
}
