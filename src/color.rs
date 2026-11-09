//! Scene colors are tagged. Hex presets are always sRGB, regardless of monitor gamut.
use serde::{Deserialize, Serialize};

pub const PRESETS: [[u8; 3]; 4] = [
    [0x63, 0x68, 0x96],
    [0x3c, 0x77, 0x5e],
    [0xad, 0x3f, 0x55],
    [0xec, 0xad, 0x45],
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Space {
    #[default]
    Srgb,
    DisplayP3,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct InkColor {
    pub rgb: [f32; 3],
    pub space: Space,
}
impl Default for InkColor {
    fn default() -> Self {
        Self::from_rgb8(PRESETS[0])
    }
}
impl InkColor {
    pub fn from_rgb8(rgb: [u8; 3]) -> Self {
        Self::from_channels(rgb, Space::Srgb)
    }
    pub fn from_channels(rgb: [u8; 3], space: Space) -> Self {
        Self {
            rgb: rgb.map(|v| v as f32 / 255.),
            space,
        }
    }
    pub fn converted(self, space: Space) -> Self {
        if self.space == space {
            return self;
        }
        let [r, g, b] = self.linear_srgb();
        let linear = match space {
            Space::Srgb => gamut_map([r, g, b]),
            // Inverse of the Display P3 to linear sRGB matrix below.
            Space::DisplayP3 => [
                0.822462 * r + 0.1775381 * g,
                0.0331942 * r + 0.9668058 * g,
                0.0170827 * r + 0.0723975 * g + 0.91052 * b,
            ],
        };
        Self {
            rgb: linear.map(|v| encode(v.clamp(0., 1.))),
            space,
        }
    }
    pub fn linear_srgb(self) -> [f32; 3] {
        let a = self.rgb.map(decode);
        if self.space == Space::Srgb {
            return a;
        }
        [
            1.2249401 * a[0] - 0.2249402 * a[1],
            -0.042057 * a[0] + 1.042057 * a[1],
            -0.0196376 * a[0] - 0.0786361 * a[1] + 1.0982735 * a[2],
        ]
    }
    /// Float sRGB encoding expected by SDL_RenderGeometry. Linear/HDR renderers
    /// decode this themselves; never apply the display's SDR-white scale twice.
    pub fn output_rgb(self, extended: bool) -> [f32; 3] {
        let rgb = self.linear_srgb();
        let rgb = if extended { rgb } else { gamut_map(rgb) };
        rgb.map(|v| {
            if extended && v < 0.0 {
                12.92 * v
            } else {
                encode(v)
            }
        })
    }
    pub fn channels8(self) -> [u8; 3] {
        bytes(self.rgb)
    }
    pub fn srgb8(self) -> [u8; 3] {
        bytes(self.output_rgb(false))
    }
    pub fn hex(self) -> String {
        hex(self.channels8())
    }
    pub fn css(self) -> String {
        hex(self.srgb8())
    }
}
fn bytes(rgb: [f32; 3]) -> [u8; 3] {
    rgb.map(|v| (v.clamp(0., 1.) * 255.).round() as u8)
}
fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}
pub fn decode(v: f32) -> f32 {
    if v.abs() <= 0.04045 {
        v / 12.92
    } else {
        v.signum() * ((v.abs() + 0.055) / 1.055).powf(2.4)
    }
}
pub fn encode(v: f32) -> f32 {
    if v.abs() <= 0.0031308 {
        12.92 * v
    } else {
        v.signum() * (1.055 * v.abs().powf(1.0 / 2.4) - 0.055)
    }
}
/// Compress out-of-sRGB chroma toward a neutral of the same luminance.
/// SDR annotations stay SDR even on HDR desktops; they are never sent as PQ.
pub fn gamut_map(rgb: [f32; 3]) -> [f32; 3] {
    let y = (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]).clamp(0.0, 1.0);
    let mut t = 1.0_f32;
    for v in rgb {
        let d = v - y;
        if d > 0.0 {
            t = t.min((1.0 - y) / d)
        } else if d < 0.0 {
            t = t.min(-y / d)
        }
    }
    rgb.map(|v| (y + t * (v - y)).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preset_roundtrips() {
        for rgb in PRESETS {
            assert_eq!(InkColor::from_rgb8(rgb).srgb8(), rgb)
        }
    }
    #[test]
    fn sdl_extended_transfer_preserves_negative_p3_channels() {
        let c = InkColor {
            rgb: [1., 0., 0.],
            space: Space::DisplayP3,
        };
        let out = c.output_rgb(true);
        let restored = out.map(|v| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        for (a, b) in restored.into_iter().zip(c.linear_srgb()) {
            assert!((a - b).abs() < 1e-6)
        }
    }
    #[test]
    fn transfer_roundtrip() {
        for i in 0..=8192 {
            let v = i as f32 / 8192.0;
            assert!((encode(decode(v)) - v).abs() < 1e-6)
        }
    }
    #[test]
    fn p3_neutral_and_gamut() {
        let c = InkColor {
            rgb: [0.5; 3],
            space: Space::DisplayP3,
        };
        let a = c.output_rgb(false);
        for v in a {
            assert!((v - 0.5).abs() < 1e-5)
        }
        let a = gamut_map([1.22, -0.04, -0.02]);
        assert!(a.into_iter().all(|v| (0.0..=1.0).contains(&v)));
    }
}

fn parse_hex(text: &str, space: Space) -> Result<InkColor, &'static str> {
    let value = text.trim();
    let digits = value.strip_prefix('#').unwrap_or(value);
    if !digits.is_ascii()
        || !matches!(digits.len(), 3 | 6)
        || !digits.bytes().all(|v| v.is_ascii_hexdigit())
    {
        return Err("Use #RRGGBB or #RGB");
    }
    let mut rgb = [0; 3];
    for (i, component) in rgb.iter_mut().enumerate() {
        *component = if digits.len() == 3 {
            u8::from_str_radix(&digits[i..i + 1], 16).unwrap() * 17
        } else {
            u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).unwrap()
        };
    }
    Ok(InkColor::from_channels(rgb, space))
}

pub struct HexEntry {
    pub text: String,
    pub invalid: bool,
    last_color: String,
}
impl HexEntry {
    pub fn new(color: InkColor) -> Self {
        let text = color.hex();
        Self {
            last_color: text.clone(),
            text,
            invalid: false,
        }
    }
    pub fn sync(&mut self, color: InkColor) {
        let hex = color.hex();
        if self.last_color != hex {
            self.text = hex.clone();
            self.last_color = hex;
            self.invalid = false;
        }
    }
    pub fn apply(&mut self, space: Space) -> Option<InkColor> {
        match parse_hex(&self.text, space) {
            Ok(color) => {
                self.invalid = false;
                self.last_color = color.hex();
                Some(color)
            }
            Err(_) => {
                self.invalid = true;
                None
            }
        }
    }
    pub fn normalize(&mut self) {
        if !self.invalid {
            self.text = self.last_color.clone();
        }
    }
    pub fn revert(&mut self) {
        self.text = self.last_color.clone();
        self.invalid = false;
    }
}

#[cfg(test)]
mod hex_tests {
    use super::*;
    #[test]
    fn parses_common_hex_formats_and_roundtrips() {
        for (text, expected) in [
            ("#FFFFFF", [255; 3]),
            ("ffffff", [255; 3]),
            (" #abc ", [0xaa, 0xbb, 0xcc]),
            ("000", [0; 3]),
            ("#3C775E", PRESETS[1]),
        ] {
            let color = parse_hex(text, Space::Srgb).unwrap();
            assert_eq!(color.srgb8(), expected);
            assert_eq!(
                parse_hex(&color.css(), Space::Srgb).unwrap().srgb8(),
                expected
            );
            assert_eq!(color.space, Space::Srgb);
        }
    }
    #[test]
    fn p3_hex_and_srgb_presets_keep_the_selected_space() {
        let color = InkColor::from_channels([255, 0, 0], Space::DisplayP3);
        let mut entry = HexEntry::new(color);
        assert_eq!(entry.text, "#FF0000");
        assert_ne!(color.css(), entry.text);
        entry.text = "#00FF00".into();
        let green = entry.apply(Space::DisplayP3).unwrap();
        assert_eq!(green.space, Space::DisplayP3);
        assert_eq!(green.channels8(), [0, 255, 0]);
        assert_eq!(green.hex(), entry.text);
        for rgb in PRESETS {
            let preset = InkColor::from_rgb8(rgb).converted(Space::DisplayP3);
            assert_eq!(preset.space, Space::DisplayP3);
            assert_eq!(preset.srgb8(), rgb);
        }
    }
    #[test]
    fn rejects_incomplete_alpha_and_non_ascii_codes() {
        for text in [
            "",
            "#",
            "#FF",
            "#FFFF",
            "#FFFFFF00",
            "#GGGGGG",
            "##FFFFFF",
            "#ＦＦＦ",
            "12 345",
            "red",
        ] {
            assert!(parse_hex(text, Space::Srgb).is_err(), "{text}");
        }
    }
    #[test]
    fn invalid_edit_retains_the_last_color_and_external_choices_resynchronize() {
        let initial = InkColor::default();
        let mut entry = HexEntry::new(initial);
        entry.text = "#12".into();
        assert!(entry.apply(Space::Srgb).is_none());
        assert!(entry.invalid);
        entry.revert();
        assert_eq!(entry.text, initial.css());
        entry.text = "#abc".into();
        let color = entry.apply(Space::Srgb).unwrap();
        entry.sync(color);
        assert_eq!(entry.text, "#abc");
        entry.normalize();
        assert_eq!(entry.text, "#AABBCC");
        entry.sync(InkColor::from_rgb8(PRESETS[3]));
        assert_eq!(entry.text, "#ECAD45");
    }
}
