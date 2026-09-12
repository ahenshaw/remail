//! System font discovery and registration.
//!
//! egui only knows about the faces it has been handed, so a family the user
//! picks has to be found on disk, read, and installed into the context before
//! it can be drawn with. Installing fonts re-rasterizes every glyph, so it
//! happens once per family and never per frame.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use egui::{Context, FontData, FontDefinitions, FontFamily};

use crate::config::PaneFont;

pub struct FontLibrary {
    db: fontdb::Database,
    /// Family names offered in the picker, sorted case-insensitively.
    families: Vec<String>,
    /// Definitions as currently installed, extended as families are used.
    definitions: FontDefinitions,
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

        tracing::debug!("found {} font families", families.len());

        Self {
            db,
            families,
            definitions: FontDefinitions::default(),
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
    fn install(&mut self, ctx: &Context, name: &str) -> bool {
        let Some(data) = self.face_data(name) else { return false };

        let key = format!("system:{name}");
        self.definitions
            .font_data
            .insert(key.clone(), Arc::new(FontData::from_owned(data)));

        // Fall back to the built-in faces so glyphs the chosen family
        // lacks — emoji, most often — still render.
        let mut chain = vec![key];
        chain.extend(
            self.definitions
                .families
                .get(&FontFamily::Proportional)
                .cloned()
                .unwrap_or_default(),
        );
        self.definitions
            .families
            .insert(FontFamily::Name(name.into()), chain);

        // Re-rasterizes every glyph, which is why this is once per family.
        ctx.set_fonts(self.definitions.clone());
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
            definitions: FontDefinitions::default(),
            installed: BTreeSet::new(),
            failed: BTreeSet::new(),
            resolved: HashMap::new(),
        };
        let ctx = Context::default();
        assert_eq!(library.resolve(&ctx, &PaneFont::Sans), FontFamily::Proportional);
        assert_eq!(library.resolve(&ctx, &PaneFont::Mono), FontFamily::Monospace);
    }

    /// A missing family never reaches the font set, so this stays off the
    /// path that needs a live frame.
    #[test]
    fn a_missing_family_falls_back_and_is_not_retried() {
        let mut library = FontLibrary {
            db: fontdb::Database::new(),
            families: Vec::new(),
            definitions: FontDefinitions::default(),
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
        let names: Vec<String> =
            library.families().iter().map(|f| f.to_lowercase()).collect();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]), "not sorted");
    }
}
