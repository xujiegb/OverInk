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
};
use unicode_segmentation::UnicodeSegmentation;

pub const INCLUDED_FONT: &str = "Sarasa Fixed Slab SC";
const COLOR: &str = "OpenMoji Color";
const MONO: &str = "OpenMoji Black";
const CUSTOM: &str = "Custom annotation font";
const ANNOTATION: &str = "Annotations";
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

/// A named face discovered in the operating system's installed-font locations.
/// The file path is an implementation detail; users select a family name.
pub struct InstalledFont {
    pub name: String,
    path: PathBuf,
    index: u32,
}

/// Enumerate installed typefaces only when the font picker is opened.
/// A single cached scan avoids reading font files on every UI frame.
pub fn installed_fonts() -> &'static [InstalledFont] {
    static FONTS: OnceLock<Vec<InstalledFont>> = OnceLock::new();
    FONTS.get_or_init(scan_installed_fonts)
}

fn system_font_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("WINDIR") {
            dirs.push(PathBuf::from(root).join("Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        dirs.extend([
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
            PathBuf::from("/usr/X11R6/lib/X11/fonts"),
        ]);
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".fonts"));
            dirs.push(home.join(".local/share/fonts"));
        }
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(xdg).join("fonts"));
        }
    }
    dirs
}

fn scan_installed_fonts() -> Vec<InstalledFont> {
    let mut found = Vec::new();
    let mut dirs = system_font_directories();
    let mut visited = 0usize;
    while let Some(dir) = dirs.pop() {
        // Bound the scan in the presence of surprising mount or directory layouts.
        if visited >= 40_000 {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited >= 40_000 {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                dirs.push(path);
                continue;
            }
            if !file_type.is_file()
                || !path.extension().and_then(|v| v.to_str()).is_some_and(|ext| {
                    ["ttf", "otf", "ttc", "otc"].iter().any(|s| ext.eq_ignore_ascii_case(s))
                })
                || !fs::metadata(&path)
                    .is_ok_and(|m| m.len() > 0 && m.len() <= MAX_FONT_BYTES)
            {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            for index in 0..128 {
                let Ok(face) = FontRef::from_index(&bytes, index) else {
                    break;
                };
                let family = face
                    .localized_strings(StringId::TYPOGRAPHIC_FAMILY_NAME)
                    .english_or_first()
                    .or_else(|| {
                        face.localized_strings(StringId::FAMILY_NAME)
                            .english_or_first()
                    });
                let Some(name) = family.map(|v| v.chars().take(100).collect::<String>()) else {
                    continue;
                };
                if !name.trim().is_empty() {
                    found.push(InstalledFont {
                        name,
                        path: path.clone(),
                        index,
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    found.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    found
}

#[derive(Default)]
pub struct TextFonts {
    custom: Option<CustomFont>,
    pub revision: u64,
}

struct CustomFont {
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

impl CustomFont {
    fn read_index(path: &Path, index: u32) -> Result<Self, String> {
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
        let mut font_data = FontData::from_owned(data);
        font_data.index = index;
        let face = FontFace::new(font_data)?;
        let name = face
            .font()
            .and_then(|font| {
                font.localized_strings(StringId::FULL_NAME)
                    .english_or_first()
                    .map(|name| name.chars().take(100).collect())
            })
            .unwrap_or_else(|| CUSTOM.into());
        Ok(Self { name, face })
    }
}

impl TextFonts {
    pub fn load(path: &Path) -> Self {
        let index = fs::read_to_string(path.with_extension("index"))
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        Self {
            custom: CustomFont::read_index(path, index).ok(),
            revision: 0,
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.custom.as_ref().map(|font| font.name.as_str())
    }

    pub fn install_installed(&mut self, choice: usize, path: &Path) -> Result<(), String> {
        let selected = installed_fonts().get(choice).ok_or("Font no longer available")?;
        let mut font = CustomFont::read_index(&selected.path, selected.index)?;
        // Display the selected family rather than an internal PostScript/full name.
        font.name.clone_from(&selected.name);
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(&temporary, font.face.data.bytes())
            .and_then(|()| fs::rename(&temporary, path))
            .inspect_err(|_| {
                let _ = fs::remove_file(&temporary);
            })
            .map_err(|error| error.to_string())?;
        // Eg. PingFang.ttc contains several faces; keep the chosen index.
        fs::write(path.with_extension("index"), selected.index.to_string())
            .map_err(|error| error.to_string())?;
        self.custom = Some(font);
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn reset(&mut self, path: &Path) -> Result<(), String> {
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.to_string()),
        }
        match fs::remove_file(path.with_extension("index")) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.to_string()),
        }
        self.custom = None;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn definitions(&self) -> FontDefinitions {
        let mut definitions = bundled().definitions.clone();
        if let Some(custom) = &self.custom {
            definitions
                .font_data
                .insert(CUSTOM.into(), custom.face.data.clone());
            definitions
                .families
                .insert(FontFamily::Name(CUSTOM.into()), vec![CUSTOM.into()]);
            definitions
                .families
                .entry(FontFamily::Name(ANNOTATION.into()))
                .or_default()
                .insert(0, CUSTOM.into());
        }
        definitions
    }

    fn family_for_cluster(&self, cluster: &str) -> &'static str {
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

        // egui resolves a cluster from its first character. Choose a face for
        // emoji sequences before shaping so a plain digit or a partially
        // supported flag does not mask the remaining fonts in the chain.
        let faces = self
            .custom
            .iter()
            .map(|font| (CUSTOM, &font.face))
            .chain(FALLBACK.into_iter().zip(&bundled().faces));
        let mut monochrome_fallback = None;
        for (name, face) in faces {
            let Some(glyph) = face.shape_cluster(cluster) else {
                continue;
            };
            match selector {
                Some('\u{fe0f}') if !face.has_color_glyph(glyph) => {
                    monochrome_fallback.get_or_insert(name);
                }
                Some('\u{fe0e}') if name == COLOR || face.has_color_glyph(glyph) => (),
                _ => return name,
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
