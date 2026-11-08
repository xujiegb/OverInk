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
        Self {
            rgb: rgb.map(|v| v as f32 / 255.0),
            space: Space::Srgb,
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
    pub fn srgb8(self) -> [u8; 3] {
        self.output_rgb(false)
            .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
    }
    pub fn css(self) -> String {
        let [r, g, b] = self.srgb8();
        format!("#{r:02X}{g:02X}{b:02X}")
    }
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
