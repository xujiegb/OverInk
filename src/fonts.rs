#![forbid(unsafe_code)]

use egui::{
    Color32, FontData, FontDefinitions, FontFamily, FontId, Ui,
    text::{Galley, LayoutJob, TextFormat},
};
use skrifa::{FontRef, GlyphId, MetadataProvider, string::StringId};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
use unicode_segmentation::UnicodeSegmentation;

pub const INCLUDED_FONT: &str = "Sarasa Fixed Slab SC";
const COLOR: &str = "OpenMoji Color";
const MONO: &str = "OpenMoji Black";
const CUSTOM: &str = "Custom annotation font";
const ANNOTATION: &str = "Annotations";
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

pub struct TextFonts {
    custom: Vec<CustomFont>,
    order: Vec<String>,
    pub revision: u64,
}

pub struct FontEntry<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub bundled: bool,
}

struct CustomFont {
    key: String,
    name: String,
    face: FontFace,
}

struct FontFace {
    data: Arc<FontData>,
    shaping_font: harfrust::Font,
    color: bool,
}

impl FontFace {
    fn new(data: FontData) -> Result<Self, String> {
        use skrifa::raw::TableProvider as _;
        let font =
            FontRef::from_index(data.bytes(), data.index).map_err(|error| error.to_string())?;
        if font.charmap().mappings().next().is_none() {
            return Err("The font has no Unicode character map".into());
        }
        let color = font.colr().is_ok() || font.sbix().is_ok() || font.cbdt().is_ok();
        let shaping_font = harfrust::Font::new(data.font.clone(), data.index)
            .ok_or("Cannot prepare the font for text shaping")?;
        Ok(Self {
            data: Arc::new(data),
            shaping_font,
            color,
        })
    }

    fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(self.data.bytes(), self.data.index).ok()
    }

    fn has_base(&self, base: char) -> bool {
        self.font()
            .is_some_and(|font| font.charmap().map(base).is_some())
    }

    fn has_color_glyph(&self, glyph: GlyphId) -> bool {
        if !self.color {
            return false;
        }
        let Some(font) = self.font() else {
            return false;
        };
        font.color_glyphs().get(glyph).is_some()
            || font
                .bitmap_strikes()
                .iter()
                .any(|strike| strike.get(glyph).is_some())
    }

    fn shape_cluster(&self, cluster: &str) -> Option<GlyphId> {
        let base = cluster.chars().next()?;
        if !self.has_base(base) {
            return None;
        }
        let font = harfrust::ShaperFont::new(&self.shaping_font);
        let mut buffer = harfrust::Buffer::new();
        buffer.set_flags(harfrust::BufferFlags::REMOVE_DEFAULT_IGNORABLES);
        buffer.push_str(cluster);
        buffer.guess_segment_properties();
        harfrust::shape(&font, &mut buffer, Default::default()).ok()?;
        if !buffer.allocation_successful() {
            return None;
        }
        match buffer.glyph_infos() {
            [glyph] if glyph.glyph_id != GlyphId::NOTDEF.to_u32() => {
                Some(GlyphId::new(glyph.glyph_id))
            }
            _ => None,
        }
    }
}

const FALLBACK: [&str; 3] = [INCLUDED_FONT, COLOR, MONO];

struct BundledFonts {
    faces: [FontFace; 3],
    definitions: FontDefinitions,
}

fn bundled() -> &'static BundledFonts {
    static BUNDLED: OnceLock<BundledFonts> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        let faces = [
            FontData::from_static(include_bytes!(
                "../assets/fonts/SarasaFixedSlabSC-Regular.ttf"
            )),
            FontData::from_static(include_bytes!(
                "../assets/fonts/OpenMoji-color-glyf_colr_0.ttf"
            )),
            FontData::from_static(include_bytes!("../assets/fonts/OpenMoji-black-glyf.ttf")),
        ]
        .map(|data| FontFace::new(data).expect("valid bundled annotation font"));
        let mut definitions = FontDefinitions::default();
        for (name, face) in FALLBACK.into_iter().zip(&faces) {
            definitions.font_data.insert(name.into(), face.data.clone());
            definitions
                .families
                .insert(FontFamily::Name(name.into()), vec![name.into()]);
        }
        definitions.families.insert(
            FontFamily::Name(ANNOTATION.into()),
            FALLBACK.map(str::to_owned).to_vec(),
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            definitions
                .families
                .entry(family)
                .or_default()
                .splice(..0, FALLBACK.map(str::to_owned));
        }
        BundledFonts { faces, definitions }
    })
}

pub fn ui_definitions() -> FontDefinitions {
    bundled().definitions.clone()
}

impl Default for TextFonts {
    fn default() -> Self {
        Self {
            custom: Vec::new(),
            order: FALLBACK.map(str::to_owned).to_vec(),
            revision: 0,
        }
    }
}

fn font_directory(path: &Path) -> PathBuf {
    path.with_file_name("annotation-fonts")
}

fn order_file(path: &Path) -> PathBuf {
    path.with_file_name("annotation-font-order.json")
}

impl CustomFont {
    fn read(path: &Path, key: String) -> Result<Self, String> {
        if !fs::metadata(path)
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("Select a font file".into());
        }
        let mut data = Vec::new();
        File::open(path)
            .and_then(|file| file.take(MAX_FONT_BYTES + 1).read_to_end(&mut data))
            .map_err(|error| error.to_string())?;
        if data.len() as u64 > MAX_FONT_BYTES {
            return Err("Font exceeds 64 MiB".into());
        }
        let face = FontFace::new(FontData::from_owned(data))?;
        let name = face
            .font()
            .and_then(|font| {
                font.localized_strings(StringId::FULL_NAME)
                    .english_or_first()
                    .map(|name| name.chars().take(100).collect())
            })
            .unwrap_or_else(|| CUSTOM.into());
        Ok(Self { key, name, face })
    }
}

impl TextFonts {
    pub fn load(path: &Path) -> Self {
        let mut fonts = Self::default();
        if let Ok(entries) = fs::read_dir(font_directory(path)) {
            for entry in entries.flatten() {
                let Some(key) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !key.starts_with("custom-")
                    || !key["custom-".len()..].chars().all(|ch| ch.is_ascii_digit())
                {
                    continue;
                }
                if let Ok(custom) = CustomFont::read(&entry.path(), key) {
                    fonts.custom.push(custom);
                }
            }
        }
        // Keep installations made by older releases available until removed.
        if path.exists() && let Ok(old) = CustomFont::read(path, "legacy".into()) {
            fonts.custom.push(old);
        }
        fonts.custom.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        let saved = fs::read(order_file(path)).ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
            .unwrap_or_default();
        fonts.order.clear();
        for key in saved {
            if fonts.known(&key) && !fonts.order.contains(&key) {
                fonts.order.push(key);
            }
        }
        for custom in &fonts.custom {
            if !fonts.order.contains(&custom.key) {
                fonts.order.push(custom.key.clone());
            }
        }
        for builtin in FALLBACK {
            if !fonts.order.iter().any(|key| key == builtin) {
                fonts.order.push(builtin.into());
            }
        }
        fonts
    }

    fn known(&self, key: &str) -> bool {
        FALLBACK.contains(&key) || self.custom.iter().any(|font| font.key == key)
    }

    pub fn entries(&self) -> Vec<FontEntry<'_>> {
        self.order.iter().filter_map(|key| {
            if FALLBACK.contains(&key.as_str()) {
                Some(FontEntry { key, name: key, bundled: true })
            } else {
                self.custom.iter().find(|font| font.key == *key)
                    .map(|font| FontEntry { key, name: &font.name, bundled: false })
            }
        }).collect()
    }

    pub fn display_name(&self, key: &str) -> Option<&str> {
        if FALLBACK.contains(&key) {
            Some(FALLBACK.iter().copied().find(|name| *name == key)?)
        } else {
            self.custom.iter().find(|font| font.key == key).map(|font| font.name.as_str())
        }
    }

    fn save_order(&self, path: &Path) -> Result<(), String> {
        let file = order_file(path);
        let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
        let data = serde_json::to_vec_pretty(&self.order).map_err(|error| error.to_string())?;
        fs::write(&temporary, data)
            .and_then(|()| fs::rename(&temporary, &file))
            .inspect_err(|_| { let _ = fs::remove_file(&temporary); })
            .map_err(|error| error.to_string())
    }

    pub fn install(&mut self, source: &Path, path: &Path) -> Result<(), String> {
        let mut suffix = 1_usize;
        let destination = loop {
            let candidate = format!("custom-{suffix}");
            let location = font_directory(path).join(&candidate);
            if !location.exists() && !self.known(&candidate) {
                break (candidate, location);
            }
            suffix = suffix.checked_add(1).ok_or("Too many fonts installed")?;
        };
        let (key, file) = destination;
        let font = CustomFont::read(source, key.clone())?;
        fs::create_dir_all(font_directory(path)).map_err(|error| error.to_string())?;
        let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, font.face.data.bytes())
            .and_then(|()| fs::rename(&temporary, &file))
            .inspect_err(|_| { let _ = fs::remove_file(&temporary); })
            .map_err(|error| error.to_string())?;
        self.custom.push(font);
        self.order.insert(0, key);
        if let Err(error) = self.save_order(path) {
            self.custom.pop();
            self.order.remove(0);
            let _ = fs::remove_file(file);
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn remove(&mut self, key: &str, path: &Path) -> Result<(), String> {
        let Some(index) = self.custom.iter().position(|font| font.key == key) else {
            return Err("Bundled fonts cannot be removed".into());
        };
        let file = if key == "legacy" {
            path.to_path_buf()
        } else {
            font_directory(path).join(key)
        };
        fs::remove_file(file).map_err(|error| error.to_string())?;
        self.custom.remove(index);
        self.order.retain(|item| item != key);
        self.save_order(path)?;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn move_priority(&mut self, from: usize, to: usize, path: &Path) -> Result<(), String> {
        if from >= self.order.len() || to >= self.order.len() || from == to {
            return Ok(());
        }
        let moved = self.order.remove(from);
        self.order.insert(to, moved);
        if let Err(error) = self.save_order(path) {
            let moved = self.order.remove(to);
            self.order.insert(from, moved);
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn definitions(&self) -> FontDefinitions {
        let mut definitions = bundled().definitions.clone();
        for font in &self.custom {
            definitions.font_data.insert(font.key.clone(), font.face.data.clone());
        }
        definitions.families.insert(
            FontFamily::Name(ANNOTATION.into()), self.order.clone()
        );
        for key in &self.order {
            let mut chain = Vec::with_capacity(self.order.len());
            chain.push(key.clone());
            chain.extend(self.order.iter().filter(|other| *other != key).cloned());
            definitions.families.insert(FontFamily::Name(key.clone().into()), chain);
        }
        definitions
    }

    fn family_for_cluster(&self, cluster: &str) -> &str {
        let mut chars = cluster.chars();
        let Some(base) = chars.next() else {
            return ANNOTATION;
        };
        let mut selector = None;
        let mut sequence = false;
        for ch in chars {
            if selector.is_none() && matches!(ch, '\u{fe0e}' | '\u{fe0f}') {
                selector = Some(ch);
            }
            sequence |= matches!(ch, '\u{200d}' | '\u{20e3}' | '\u{1f3fb}'..='\u{1f3ff}' | '\u{e0020}'..='\u{e007f}')
                || matches!(base, '\u{1f1e6}'..='\u{1f1ff}')
                    && matches!(ch, '\u{1f1e6}'..='\u{1f1ff}');
        }
        if selector.is_none() && !sequence {
            return ANNOTATION;
        }
        let mut monochrome_fallback = None;
        for key in &self.order {
            let face = if let Some(font) = self.custom.iter().find(|font| font.key == *key) {
                &font.face
            } else if let Some(index) = FALLBACK.iter().position(|builtin| *builtin == key.as_str()) {
                &bundled().faces[index]
            } else {
                continue;
            };
            let Some(glyph) = face.shape_cluster(cluster) else {
                continue;
            };
            match selector {
                Some('\u{fe0f}') if !face.has_color_glyph(glyph) => {
                    monochrome_fallback.get_or_insert(key.as_str());
                }
                Some('\u{fe0e}') if key.as_str() == COLOR || face.has_color_glyph(glyph) => (),
                _ => return key,
            }
        }
        monochrome_fallback.unwrap_or(ANNOTATION)
    }

    pub fn layout(
        &self,
        ui: &Ui,
        text: &str,
        size: f32,
        color: Color32,
        width: f32,
    ) -> Arc<Galley> {
        self.layout_preferred(ui, text, size, color, width, None)
    }

    pub fn layout_preferred(
        &self,
        ui: &Ui,
        text: &str,
        size: f32,
        color: Color32,
        width: f32,
        preferred: Option<&str>,
    ) -> Arc<Galley> {
        let ordinary = preferred.filter(|key| self.known(key)).unwrap_or(ANNOTATION);
        let mut job = LayoutJob::default();
        job.wrap.max_width = width;
        job.keep_trailing_whitespace = true;
        let mut current = ordinary;
        let mut start = 0;
        let mut append = |range, name| {
            let font = FontId::new(size, FontFamily::Name(Arc::from(name)));
            job.append(&text[range], 0., TextFormat::simple(font, color));
        };
        for (index, cluster) in text.grapheme_indices(true) {
            let special = self.family_for_cluster(cluster);
            let next = if special == ANNOTATION { ordinary } else { special };
            if next != current {
                if start < index {
                    append(start..index, current);
                }
                start = index;
                current = next;
            }
        }
        append(start..text.len(), current);
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    }

    pub fn font_id(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name(ANNOTATION.into()))
    }

    pub fn font_id_preferred(&self, size: f32, preferred: Option<&str>) -> FontId {
        let name = preferred.filter(|name| self.known(name)).unwrap_or(ANNOTATION);
        FontId::new(size, FontFamily::Name(Arc::from(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(fonts: &TextFonts, text: &str) -> Vec<(char, bool)> {
        let context = egui::Context::default();
        context.set_fonts(fonts.definitions());
        let mut glyphs = Vec::new();
        let frame = context.run_ui(egui::RawInput::default(), |ui| {
            let galley = fonts.layout(ui, text, 28., Color32::WHITE, 800.);
            glyphs = galley
                .rows
                .iter()
                .flat_map(|row| &row.glyphs)
                .filter(|glyph| !glyph.uv_rect.is_nothing())
                .map(|glyph| (glyph.chr, glyph.is_color))
                .collect();
        });
        frame.drop_without_applying_deltas();
        glyphs
    }

    #[test]
    fn empty_cluster_has_a_safe_default_family() {
        let fonts = TextFonts::default();
        assert_eq!(fonts.family_for_cluster(""), ANNOTATION);
        assert_eq!(fonts.family_for_cluster("ABC"), ANNOTATION);
    }

    #[test]
    fn chinese_text_and_emoji_sequences_share_a_line() {
        let fonts = TextFonts::default();
        assert_eq!(
            glyphs(&fonts, "中臺 1️⃣ 👩🏽‍🎨 🇨🇳 🏳️‍🌈 ☀️"),
            [
                ('中', false),
                ('臺', false),
                ('1', true),
                ('👩', true),
                ('🇨', true),
                ('🏳', true),
                ('☀', true)
            ]
        );
    }

    #[test]
    fn explicit_text_presentation_can_use_the_mono_emoji() {
        let fonts = TextFonts::default();
        assert_eq!(glyphs(&fonts, "✏︎ ✏️"), [('✏', false), ('✏', true)]);
    }

    #[test]
    fn explicit_emoji_presentation_prefers_actual_color_over_a_default_outline() {
        assert_eq!(
            glyphs(&TextFonts::default(), "⚽ ⚽️ ⚽︎"),
            [('⚽', false), ('⚽', true), ('⚽', false)]
        );
    }
}
