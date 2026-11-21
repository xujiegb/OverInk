//! On X11 the display usually does not transform sRGB windows. Use SDL's
//! current-display ICC profile there. Other backends retain OS-managed sRGB /
//! scRGB surfaces; applying an ICC transform there would double-manage color.
#![forbid(unsafe_code)]

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::sdl::Window;
use crate::{
    appearance::{Text, text},
    icc_native::{DisplayError, Transform},
};
use std::hash::{Hash, Hasher};

/// Convert embedded-profile RGBA8 images without exposing native CMS handles.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub(crate) fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    crate::icc_native::convert_rgba8_to_srgb(pixels, profile);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DisplayColor {
    System { extended: bool },
    X11(Option<u64>),
}

pub struct ColorManager {
    source: Option<DisplayColor>,
    transform: Option<Transform>,
    pub status: Text,
    pub revision: u64,
}

const SYSTEM_SRGB: Text = text("sRGB surface / system compositor");

impl Default for ColorManager {
    fn default() -> Self {
        Self {
            source: None,
            transform: None,
            status: SYSTEM_SRGB,
            revision: 0,
        }
    }
}

impl ColorManager {
    pub fn refresh(&mut self, window: &crate::sdl::Window, extended: bool) {
        let x11 = crate::sdl::driver() == "x11";
        let profile = x11.then(|| window.icc_profile()).flatten();
        let source = if x11 {
            DisplayColor::X11(profile.as_ref().map(|profile| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                profile.hash(&mut hash);
                hash.finish()
            }))
        } else {
            DisplayColor::System { extended }
        };
        if self.source == Some(source) {
            return;
        }
        self.source = Some(source);
        self.revision = self.revision.wrapping_add(1);
        self.transform = None;
        let Some(profile) = profile else {
            self.status = match source {
                DisplayColor::System { extended: true } => {
                    text("Extended linear sRGB / system display mapping")
                }
                DisplayColor::System { extended: false } => SYSTEM_SRGB,
                DisplayColor::X11(_) => text("X11 uses sRGB when an ICC profile is unavailable"),
            };
            return;
        };
        match Transform::from_srgb_to_display(&profile) {
            Ok(transform) => {
                self.transform = Some(transform);
                self.status = text("X11 display ICC / moxcms");
            }
            Err(DisplayError::InvalidProfile) => {
                self.status = text("Invalid ICC profile. X11 is using sRGB.");
            }
            Err(DisplayError::ConversionFailed) => {
                self.status = text("ICC conversion failed. X11 is using sRGB.");
            }
        }
    }

    pub fn map(&self, rgb: [f32; 3]) -> [f32; 3] {
        let Some(transform) = &self.transform else {
            return rgb;
        };
        transform.map_rgb(rgb).map(|v| v.clamp(0., 1.))
    }

    pub fn is_identity(&self) -> bool {
        self.transform.is_none()
    }

    pub fn map_rgba8(&self, rgba: &mut [u8]) {
        let Some(transform) = &self.transform else {
            return;
        };
        // Reuse the staging allocation between batches instead of allocating
        // and freeing a new RGB vector for each 4096-pixel chunk.
        let mut rgb = Vec::new();
        for bytes in rgba.chunks_mut(4096 * 4) {
            rgb.clear();
            rgb.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|v| [v[0] as f32 / 255., v[1] as f32 / 255., v[2] as f32 / 255.]),
            );
            transform.map_rgb_batch(&mut rgb);
            for (pixel, rgb) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(rgb.iter()) {
                for i in 0..3 {
                    pixel[i] = (rgb[i].clamp(0., 1.) * 255.).round() as u8;
                }
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn display_sample_to_srgb(window: &Window, rgb: [f32; 3]) -> crate::color::InkColor {
    let mut result = crate::color::InkColor {
        rgb,
        space: crate::color::Space::Srgb,
    };
    let Some(profile_data) = window.icc_profile() else {
        return result;
    };
    let Some(transform) = Transform::from_display_to_srgb(&profile_data) else {
        return result;
    };
    result.rgb = transform.map_rgb(rgb).map(|v| v.clamp(0., 1.));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_rgba8_preserves_alpha_across_chunks() -> Result<(), String> {
        let profile = moxcms::ColorProfile::new_srgb()
            .encode()
            .map_err(|error| error.to_string())?;
        let transform =
            Transform::from_srgb_to_display(&profile).map_err(|error| format!("{error:?}"))?;
        let manager = ColorManager {
            source: None,
            transform: Some(transform),
            status: SYSTEM_SRGB,
            revision: 0,
        };
        let mut rgba = vec![0u8; 4097 * 4];
        for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel.copy_from_slice(&[40, 100, 180, (index % 251) as u8]);
        }
        manager.map_rgba8(&mut rgba);
        for (index, pixel) in rgba.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(pixel[3], (index % 251) as u8);
        }
        Ok(())
    }
}
