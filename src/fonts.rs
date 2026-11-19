#![forbid(unsafe_code)]

use egui::{
    Color32, FontData, FontDefinitions, FontFamily, FontId, Ui,
    text::{Galley, LayoutJob, TextFormat},
};
use skrifa::{FontRef, GlyphId, MetadataProvider, string::StringId};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use unicode_segmentation::UnicodeSegmentation;

pub const INCLUDED_FONT: &str = "Sarasa Fixed Slab SC";
const COLOR: &str = "OpenMoji Color";
const MONO: &str = "OpenMoji Black";
const CUSTOM: &str = "Custom annotation font";
const ANNOTATION: &str = "Annotations";
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

pub struct TextFonts {
    customs: Vec<CustomFont>,
    order: Vec<String>,
    pub revision: u64,
}

/// One row in the priority list shown in Settings.
pub struct FontRow {
    pub name: String,
    pub removable: bool,
}

struct CustomFont {
    id: String,
    name: String,
    face: FontFace,
}

impl Default for TextFonts {
    fn default() -> Self {
        Self {
            customs: Vec::new(),
            order: FALLBACK.into_iter().map(str::to_owned).collect(),
            revision: 0,
        }
    }
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

impl CustomFont {
    fn read(path: &Path) -> Result<Self, String> {
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
        Ok(Self {
            id: String::new(),
            name,
            face,
        })
    }
}

fn fonts_directory(legacy: &Path) -> std::path::PathBuf {
    legacy.with_file_name("annotation-fonts")
}

impl TextFonts {
    pub fn load(path: &Path) -> Self {
        let mut fonts = Self::default();
        let directory = fonts_directory(path);
        if let Ok(entries) = fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let file = entry.path();
                if file.extension() != Some(OsStr::new("font")) {
                    continue;
                }
                let Some(id) = file.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if let Ok(mut font) = CustomFont::read(&file) {
                    font.id = format!("custom-{id}");
                    fonts.customs.push(font);
                }
            }
        }
        // Preserve the single user font installed by earlier versions.
        if let Ok(mut font) = CustomFont::read(path) {
            font.id = "legacy".to_owned();
            fonts.customs.push(font);
        }
        let saved: Vec<String> = fs::read(directory.join("order.json"))
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        fonts.order.clear();
        for id in saved
            .into_iter()
            .chain(fonts.customs.iter().map(|font| font.id.clone()))
            .chain(FALLBACK.into_iter().map(str::to_owned))
        {
            let valid =
                FALLBACK.contains(&id.as_str()) || fonts.customs.iter().any(|font| font.id == id);
            if valid && !fonts.order.contains(&id) {
                fonts.order.push(id);
            }
        }
        fonts
    }

    pub fn rows(&self) -> Vec<FontRow> {
        self.order
            .iter()
            .map(|id| FontRow {
                name: self
                    .customs
                    .iter()
                    .find(|font| font.id == *id)
                    .map_or_else(|| id.clone(), |font| font.name.clone()),
                removable: !FALLBACK.contains(&id.as_str()),
            })
            .collect()
    }

    fn save_order(&self, path: &Path) -> Result<(), String> {
        let dir = fonts_directory(path);
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let target = dir.join("order.json");
        let temporary = dir.join(format!("order-{}.tmp", std::process::id()));
        let data = serde_json::to_vec_pretty(&self.order).map_err(|error| error.to_string())?;
        fs::write(&temporary, data)
            .and_then(|()| fs::rename(&temporary, target))
            .inspect_err(|_| {
                let _ = fs::remove_file(&temporary);
            })
            .map_err(|error| error.to_string())
    }

    pub fn install(&mut self, source: &Path, path: &Path) -> Result<(), String> {
        const LIMIT: usize = 32;
        if self.customs.len() >= LIMIT {
            return Err("Too many custom fonts (maximum 32)".into());
        }
        let mut font = CustomFont::read(source)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?;
        let id = format!("{:x}-{:x}", std::process::id(), now.as_nanos());
        let directory = fonts_directory(path);
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let target = directory.join(format!("{id}.font"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| error.to_string())?;
        if let Err(error) = file.write_all(font.face.data.bytes()) {
            let _ = fs::remove_file(&target);
            return Err(error.to_string());
        }
        drop(file);
        font.id = format!("custom-{id}");
        self.order.insert(0, font.id.clone());
        self.customs.push(font);
        if let Err(error) = self.save_order(path) {
            self.customs.pop();
            self.order.remove(0);
            let _ = fs::remove_file(&target);
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn move_font(&mut self, from: usize, to: usize, path: &Path) -> Result<bool, String> {
        if from >= self.order.len() || to >= self.order.len() || from == to {
            return Ok(false);
        }
        let id = self.order.remove(from);
        self.order.insert(to, id);
        if let Err(error) = self.save_order(path) {
            let id = self.order.remove(to);
            self.order.insert(from, id);
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(true)
    }

    pub fn remove_font(&mut self, index: usize, path: &Path) -> Result<bool, String> {
        let Some(id) = self.order.get(index) else {
            return Ok(false);
        };
        if FALLBACK.contains(&id.as_str()) {
            return Ok(false);
        }
        let id = id.clone();
        let target = if id == "legacy" {
            path.to_path_buf()
        } else {
            fonts_directory(path).join(format!(
                "{}.font",
                id.strip_prefix("custom-").ok_or("Invalid font entry")?
            ))
        };
        match fs::remove_file(target) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.to_string()),
        }
        self.order.remove(index);
        self.customs.retain(|font| font.id != id);
        self.save_order(path)?;
        self.revision = self.revision.wrapping_add(1);
        Ok(true)
    }

    pub fn definitions(&self) -> FontDefinitions {
        let mut definitions = bundled().definitions.clone();
        for font in &self.customs {
            definitions
                .font_data
                .insert(font.id.clone(), font.face.data.clone());
            definitions.families.insert(
                FontFamily::Name(font.id.clone().into()),
                vec![font.id.clone()],
            );
        }
        definitions
            .families
            .insert(FontFamily::Name(ANNOTATION.into()), self.order.clone());
        definitions
    }

    fn face(&self, id: &str) -> Option<&FontFace> {
        match id {
            INCLUDED_FONT => Some(&bundled().faces[0]),
            COLOR => Some(&bundled().faces[1]),
            MONO => Some(&bundled().faces[2]),
            _ => self
                .customs
                .iter()
                .find(|font| font.id == id)
                .map(|font| &font.face),
        }
    }

    fn family_for_cluster<'a>(&'a self, cluster: &str) -> &'a str {
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
        for id in &self.order {
            let Some(face) = self.face(id) else { continue };
            let Some(glyph) = face.shape_cluster(cluster) else {
                continue;
            };
            match selector {
                Some('\u{fe0f}') if !face.has_color_glyph(glyph) => {
                    monochrome_fallback.get_or_insert(id.as_str());
                }
                Some('\u{fe0e}') if id == COLOR || face.has_color_glyph(glyph) => (),
                _ => return id.as_str(),
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
        let mut job = LayoutJob::default();
        job.wrap.max_width = width;
        job.keep_trailing_whitespace = true;
        let mut current = ANNOTATION;
        let mut start = 0;
        let mut append = |range, name| {
            let font = FontId::new(size, FontFamily::Name(Arc::from(name)));
            job.append(&text[range], 0., TextFormat::simple(font, color));
        };
        for (index, cluster) in text.grapheme_indices(true) {
            let next = self.family_for_cluster(cluster);
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
