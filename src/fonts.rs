use crate::sdl::{self, Window};
use egui::{
    Color32, FontData, FontDefinitions, FontFamily, FontId, Ui,
    text::{Galley, LayoutJob, TextFormat},
};
use flate2::read::GzDecoder;
use skrifa::{FontRef, GlyphId, MetadataProvider, string::StringId};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc},
};
use unicode_segmentation::UnicodeSegmentation;

const DEFAULT: &str = "Noto Sans SC";
const COLOR: &str = "OpenMoji Color";
const MONO: &str = "OpenMoji Black";
const CUSTOM: &str = "Custom annotation font";
const ANNOTATION: &str = "Annotations";
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

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

    fn font(&self) -> FontRef<'_> {
        FontRef::from_index(self.data.bytes(), self.data.index).expect("validated annotation font")
    }

    fn has_base(&self, base: char) -> bool {
        self.font().charmap().map(base).is_some()
    }

    fn has_color_glyph(&self, glyph: GlyphId) -> bool {
        if !self.color {
            return false;
        }
        let font = self.font();
        font.color_glyphs().get(glyph).is_some()
            || font
                .bitmap_strikes()
                .iter()
                .any(|strike| strike.get(glyph).is_some())
    }

    fn shape_cluster(&self, cluster: &str) -> Option<GlyphId> {
        if !self.has_base(cluster.chars().next().unwrap()) {
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

const FALLBACK: [&str; 3] = [DEFAULT, COLOR, MONO];

struct BundledFonts {
    faces: [FontFace; 3],
    definitions: FontDefinitions,
}

fn bundled() -> &'static BundledFonts {
    static BUNDLED: OnceLock<BundledFonts> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        let mut noto = Vec::new();
        GzDecoder::new(&include_bytes!("../assets/fonts/NotoSansSC-Regular.otf.gz")[..])
            .read_to_end(&mut noto)
            .expect("bundled Noto Sans SC font");
        let faces = [
            FontData::from_owned(noto),
            FontData::from_static(include_bytes!(
                "../assets/fonts/OpenMoji-color-colr0_svg.ttf"
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
        BundledFonts { faces, definitions }
    })
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
            .localized_strings(StringId::FULL_NAME)
            .english_or_first()
            .map_or_else(|| CUSTOM.into(), |name| name.chars().take(100).collect());
        Ok(Self { name, face })
    }
}

impl TextFonts {
    pub fn load(path: &Path) -> Self {
        Self {
            custom: CustomFont::read(path).ok(),
            revision: 0,
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.custom.as_ref().map(|font| font.name.as_str())
    }

    pub fn install(&mut self, source: &Path, path: &Path) -> Result<(), String> {
        let font = CustomFont::read(source)?;
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
                .get_mut(&FontFamily::Name(ANNOTATION.into()))
                .unwrap()
                .insert(0, CUSTOM.into());
        }
        definitions
    }

    fn family_for_cluster(&self, cluster: &str) -> &'static str {
        let mut chars = cluster.chars();
        let base = chars.next().unwrap();
        let selector = chars
            .clone()
            .find(|ch| matches!(ch, '\u{fe0e}' | '\u{fe0f}'));
        let sequence = chars.any(|ch| {
            matches!(ch, '\u{200d}' | '\u{20e3}' | '\u{1f3fb}'..='\u{1f3ff}' | '\u{e0020}'..='\u{e007f}')
                || matches!(base, '\u{1f1e6}'..='\u{1f1ff}') && matches!(ch, '\u{1f1e6}'..='\u{1f1ff}')
        });
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

type DialogResult = Result<Option<PathBuf>, String>;

#[derive(Default)]
pub struct FontDialog(Option<mpsc::Receiver<DialogResult>>);

impl FontDialog {
    pub fn pending(&self) -> bool {
        self.0.is_some()
    }

    pub fn open(&mut self, parent: &Window) {
        if self.pending() {
            return;
        }
        let (send, receive) = mpsc::channel();
        self.0 = Some(receive);
        unsafe {
            sdl3_sys::dialog::SDL_ShowOpenFileDialog(
                Some(font_chosen),
                Box::into_raw(Box::new(send)).cast(),
                parent.raw,
                std::ptr::null(),
                0,
                std::ptr::null(),
                false,
            );
        }
    }

    pub fn poll(&mut self) -> Option<DialogResult> {
        let result = match self.0.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("Font picker closed".into()),
        };
        self.0 = None;
        Some(result)
    }
}

unsafe extern "C" fn font_chosen(user: *mut c_void, files: *const *const c_char, _filter: c_int) {
    // SDL invokes the callback once, on a platform thread or synchronously on
    // failure. Copy the result before SDL releases its file list.
    unsafe {
        let send = Box::from_raw(user.cast::<mpsc::Sender<DialogResult>>());
        let result = if files.is_null() {
            Err(sdl::error())
        } else if (*files).is_null() {
            Ok(None)
        } else {
            let path = CStr::from_ptr(*files);
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt as _;
                std::ffi::OsStr::from_bytes(path.to_bytes())
            };
            #[cfg(not(unix))]
            let path = path.to_string_lossy().into_owned();
            Ok(Some(PathBuf::from(path)))
        };
        let _ = send.send(result);
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
        assert_eq!(glyphs(&fonts, "❤︎"), [('❤', false)]);
        assert_eq!(glyphs(&fonts, "❤"), [('❤', true)]);
    }

    #[test]
    fn explicit_emoji_presentation_prefers_actual_color_over_a_default_outline() {
        assert_eq!(
            glyphs(&TextFonts::default(), "⚽ ⚽️ ⚽︎"),
            [('⚽', false), ('⚽', true), ('⚽', false)]
        );
    }
}
