#![forbid(unsafe_code)]

use egui::{
    Color32, FontData, FontDefinitions, FontFamily, FontId, Ui,
    text::{Galley, LayoutJob, TextFormat},
};
use skrifa::{FontRef, GlyphId, MetadataProvider, string::StringId};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use unicode_segmentation::UnicodeSegmentation;

pub const INCLUDED_FONT: &str = "Sarasa Fixed Slab SC";
const COLOR: &str = "OpenMoji Color";
const MONO: &str = "OpenMoji Black";
const ANNOTATION: &str = "Annotations";
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

pub struct TextFonts {
    custom: Vec<CustomFont>,
    order: Vec<String>,
    pub revision: u64,
}

impl Default for TextFonts {
    fn default() -> Self {
        Self {
            custom: Vec::new(),
            order: FALLBACK.iter().map(|name| (*name).to_owned()).collect(),
            revision: 0,
        }
    }
}

struct CustomFont {
    id: String,
    name: String,
    face: FontFace,
}

/// A font installed in the operating system, displayed by its font name.
pub struct SystemFont {
    pub name: String,
    pub path: PathBuf,
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
            .unwrap_or_else(|| "Imported font".into());
        let id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("legacy");
        Ok(Self {
            id: id.to_owned(),
            name,
            face,
        })
    }
}

impl TextFonts {
    /// Restore the last saved priority order. Built-in fonts are always present,
    /// and invalid or missing user-font entries are ignored.
    pub fn load(legacy_path: &Path) -> Self {
        let mut result = Self::default();
        let Some(base) = legacy_path.parent() else {
            return result;
        };
        let saved: Vec<String> = std::fs::read(base.join("annotation-fonts.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut priorities = Vec::new();
        for id in saved.into_iter().take(35) {
            if priorities.contains(&id) {
                continue;
            }
            if FALLBACK.contains(&id.as_str()) {
                priorities.push(id);
            } else if is_custom_id(&id) {
                let file = font_file(base, legacy_path, &id);
                if let Ok(mut font) = CustomFont::read(&file) {
                    font.id = id.clone();
                    result.custom.push(font);
                    priorities.push(id);
                }
            }
        }
        if !priorities.iter().any(|id| id == "legacy")
            && result.custom.is_empty()
            && let Ok(mut font) = CustomFont::read(legacy_path)
        {
            font.id = "legacy".into();
            result.custom.push(font);
            priorities.insert(0, "legacy".into());
        }
        for included in FALLBACK {
            if !priorities.iter().any(|id| id == included) {
                priorities.push(included.to_owned());
            }
        }
        result.order = priorities;
        result
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, bool)> {
        self.order.iter().filter_map(|id| {
            if FALLBACK.contains(&id.as_str()) {
                return Some((id.as_str(), id.as_str(), true));
            }
            self.custom
                .iter()
                .find(|font| font.id == *id)
                .map(|font| (id.as_str(), font.name.as_str(), false))
        })
    }

    fn save_priority(&self, legacy_path: &Path) -> Result<(), String> {
        let base = legacy_path.parent().ok_or("No font settings directory")?;
        fs::create_dir_all(base).map_err(|error| error.to_string())?;
        let data = serde_json::to_vec_pretty(&self.order).map_err(|error| error.to_string())?;
        fs::write(base.join("annotation-fonts.json"), data).map_err(|error| error.to_string())
    }

    /// Add a system font to a private copy, without retaining a dependency on
    /// the OS installation path or distributing an additional bundled font.
    pub fn install(&mut self, source: &Path, legacy_path: &Path) -> Result<(), String> {
        if self.custom.len() >= 32 {
            return Err("At most 32 imported annotation fonts can be enabled".into());
        }
        let mut font = CustomFont::read(source)?;
        if self
            .custom
            .iter()
            .any(|existing| existing.name == font.name)
        {
            return Err("Font is already in the priority list".into());
        }
        let base = legacy_path.parent().ok_or("No font settings directory")?;
        let folder = base.join("annotation-fonts");
        fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let id = format!("font-{nanos}-{}", std::process::id());
        let path = folder.join(format!("{id}.font"));
        fs::write(&path, font.face.data.bytes()).map_err(|error| error.to_string())?;
        font.id = id.clone();
        self.order.insert(0, id);
        self.custom.insert(0, font);
        if let Err(error) = self.save_priority(legacy_path) {
            self.order.remove(0);
            self.custom.remove(0);
            let _ = fs::remove_file(path);
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn move_font(&mut self, from: usize, to: usize, legacy_path: &Path) -> Result<(), String> {
        if from >= self.order.len() || to >= self.order.len() || from == to {
            return Ok(());
        }
        let item = self.order.remove(from);
        self.order.insert(to, item);
        self.save_priority(legacy_path)?;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn remove_font(&mut self, id: &str, legacy_path: &Path) -> Result<(), String> {
        if !is_custom_id(id) {
            return Err("Built-in fonts cannot be removed".into());
        }
        let index = self
            .order
            .iter()
            .position(|item| item == id)
            .ok_or("Font is not in the priority list")?;
        let base = legacy_path.parent().ok_or("No font settings directory")?;
        let file = font_file(base, legacy_path, id);
        match fs::remove_file(file) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.to_string()),
        }
        self.order.remove(index);
        self.custom.retain(|font| font.id != id);
        self.save_priority(legacy_path)?;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn reset(&mut self, legacy_path: &Path) -> Result<(), String> {
        let ids: Vec<String> = self.custom.iter().map(|font| font.id.clone()).collect();
        for id in &ids {
            self.remove_font(id, legacy_path)?;
        }
        self.order = FALLBACK.iter().map(|name| (*name).to_owned()).collect();
        self.save_priority(legacy_path)?;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn definitions(&self) -> FontDefinitions {
        let mut definitions = bundled().definitions.clone();
        for font in &self.custom {
            definitions
                .font_data
                .insert(font.id.as_str().into(), font.face.data.clone());
            definitions.families.insert(
                FontFamily::Name(font.id.as_str().into()),
                vec![font.id.as_str().into()],
            );
        }
        definitions.families.insert(
            FontFamily::Name(ANNOTATION.into()),
            self.order.iter().cloned().collect(),
        );
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
        for id in &self.order {
            let face = if let Some(index) = FALLBACK.iter().position(|name| *name == id) {
                &bundled().faces[index]
            } else if let Some(font) = self.custom.iter().find(|font| font.id == *id) {
                &font.face
            } else {
                continue;
            };
            let Some(glyph) = face.shape_cluster(cluster) else {
                continue;
            };
            match selector {
                Some('\u{fe0f}') if !face.has_color_glyph(glyph) => {
                    monochrome_fallback.get_or_insert(id.as_str());
                }
                Some('\u{fe0e}') if id == COLOR || face.has_color_glyph(glyph) => (),
                _ => return id,
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

fn is_custom_id(id: &str) -> bool {
    id == "legacy"
        || (id.starts_with("font-")
            && id.len() <= 80
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
}

fn font_file(base: &Path, legacy_path: &Path, id: &str) -> PathBuf {
    if id == "legacy" {
        legacy_path.to_owned()
    } else {
        base.join("annotation-fonts").join(format!("{id}.font"))
    }
}

/// Read font names from standard OS font directories. Paths remain internal;
/// the settings UI only exposes family names and never asks for a file path.
pub fn installed_system_fonts() -> Vec<SystemFont> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(windir) = std::env::var_os("WINDIR") {
            roots.push(PathBuf::from(windir).join("Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        roots.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        roots.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        roots.push(home.join("Library/Fonts"));
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        roots.push(home.join(".local/share/fonts"));
    }
    let mut available = std::collections::BTreeMap::<String, PathBuf>::new();
    let mut examined = 0usize;
    while let Some(folder) = roots.pop() {
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if roots.len() < 4096 {
                    roots.push(path);
                }
                continue;
            }
            let supported = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(
                        ext.to_ascii_lowercase().as_str(),
                        "ttf" | "otf" | "ttc" | "otc"
                    )
                });
            if !supported {
                continue;
            }
            examined += 1;
            if examined > 512 {
                break;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_FONT_BYTES {
                continue;
            }
            let Ok(data) = fs::read(&path) else {
                continue;
            };
            let Ok(font) = FontRef::from_index(&data, 0) else {
                continue;
            };
            let name = font
                .localized_strings(StringId::FULL_NAME)
                .english_or_first()
                .map(|name| name.chars().take(100).collect::<String>());
            if let Some(name) = name.filter(|name| !name.is_empty()) {
                available.entry(name).or_insert(path);
            }
        }
        if examined > 512 {
            break;
        }
    }
    available
        .into_iter()
        .map(|(name, path)| SystemFont { name, path })
        .collect()
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
