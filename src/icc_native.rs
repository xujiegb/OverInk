//! Pure-Rust ICC profile parsing and in-place conversion with moxcms.
//! No native CMS handles, handwritten FFI, raw pointers, or unsafe code.
#![forbid(unsafe_code)]

use moxcms::{ColorProfile, InPlaceTransformExecutor, Layout, RenderingIntent, TransformOptions};
use std::sync::Arc;

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RGB_PIXELS_PER_BATCH: usize = (u32::MAX as usize) / 3;
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
const MAX_RGBA_PIXELS_PER_BATCH: usize = (u32::MAX as usize) / 4;

fn open_profile(bytes: &[u8]) -> Option<ColorProfile> {
    if bytes.is_empty() || bytes.len() > MAX_PROFILE_BYTES {
        return None;
    }
    ColorProfile::new_from_slice(bytes).ok()
}

fn options() -> TransformOptions {
    TransformOptions {
        // Keep the old Little CMS relative-colorimetric rendering intent.
        // moxcms does not expose its black-point compensation flag.
        rendering_intent: RenderingIntent::RelativeColorimetric,
        ..TransformOptions::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    InvalidProfile,
    ConversionFailed,
}

/// A transform's native channel layout is part of its Rust type. Callers
/// cannot route packed RGBA8 bytes into the floating-point RGB executor.
enum PixelTransform {
    Rgb(Arc<dyn InPlaceTransformExecutor<f32> + Send + Sync>),
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    Rgba8(Arc<dyn InPlaceTransformExecutor<u8> + Send + Sync>),
}

pub struct Transform {
    inner: PixelTransform,
}

enum PixelBuffer<'a> {
    Rgb(&'a mut [[f32; 3]]),
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    Rgba8(&'a mut [u8]),
}

impl Transform {
    fn rgb(input: &ColorProfile, output: &ColorProfile) -> Option<Self> {
        input
            .create_in_place_transform_f32(Layout::Rgb, output, options())
            .ok()
            .map(|executor| Self {
                inner: PixelTransform::Rgb(executor),
            })
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    fn rgba8(input: &ColorProfile, output: &ColorProfile) -> Option<Self> {
        input
            .create_in_place_transform_8bit(Layout::Rgba, output, options())
            .ok()
            .map(|executor| Self {
                inner: PixelTransform::Rgba8(executor),
            })
    }

    pub fn from_srgb_to_display(bytes: &[u8]) -> Result<Self, DisplayError> {
        let destination = open_profile(bytes).ok_or(DisplayError::InvalidProfile)?;
        Self::rgb(&ColorProfile::new_srgb(), &destination).ok_or(DisplayError::ConversionFailed)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn from_display_to_srgb(bytes: &[u8]) -> Option<Self> {
        let source = open_profile(bytes)?;
        Self::rgb(&source, &ColorProfile::new_srgb())
    }

    pub fn map_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut pixel = [rgb];
        self.map_rgb_batch(&mut pixel);
        pixel[0]
    }

    /// Convert checked RGB/RGBA buffers without copying pixels or flattening
    /// through raw pointers. Failure leaves subsequent chunks untouched.
    fn apply(&self, pixels: PixelBuffer<'_>) {
        match (&self.inner, pixels) {
            (PixelTransform::Rgb(executor), PixelBuffer::Rgb(colors)) => {
                for chunk in colors.chunks_mut(MAX_RGB_PIXELS_PER_BATCH) {
                    if executor.transform(chunk.as_flattened_mut()).is_err() {
                        break;
                    }
                }
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
            (PixelTransform::Rgba8(executor), PixelBuffer::Rgba8(bytes)) => {
                if !bytes.len().is_multiple_of(4) {
                    return;
                }
                for chunk in bytes.chunks_mut(MAX_RGBA_PIXELS_PER_BATCH * 4) {
                    if executor.transform(chunk).is_err() {
                        break;
                    }
                }
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
            _ => {}
        }
    }

    pub fn map_rgb_batch(&self, colors: &mut [[f32; 3]]) {
        self.apply(PixelBuffer::Rgb(colors));
    }
}

/// Convert packed RGBA8 pixels into sRGB while preserving alpha.
/// Invalid profiles or incomplete buffers leave their input untouched.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    if pixels.is_empty() || !pixels.len().is_multiple_of(4) {
        return;
    }
    let Some(source) = open_profile(profile) else {
        return;
    };
    let Some(transform) = Transform::rgba8(&source, &ColorProfile::new_srgb()) else {
        return;
    };
    transform.apply(PixelBuffer::Rgba8(pixels));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_profile_does_not_modify_pixels() {
        let mut pixels = vec![11, 22, 33, 44, 55, 66, 77, 88];
        convert_rgba8_to_srgb(&mut pixels, b"not an ICC profile");
        assert_eq!(pixels, [11, 22, 33, 44, 55, 66, 77, 88]);
    }

    #[test]
    fn srgb_identity_transform_preserves_colors() -> Result<(), &'static str> {
        let profile = ColorProfile::new_srgb();
        let transform = Transform::rgb(&profile, &profile).ok_or("sRGB transform unavailable")?;
        let samples = [[0.2, 0.5, 0.8], [0.7, 0.2, 0.1]];
        let mut batch = samples;
        transform.map_rgb_batch(&mut batch);
        for (actual, expected) in batch.into_iter().zip(samples) {
            for (actual, expected) in actual.into_iter().zip(expected) {
                assert!(
                    (actual - expected).abs() < 0.02,
                    "sRGB identity transform changed a channel: {actual} != {expected}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn single_and_batch_use_the_same_transform() -> Result<(), &'static str> {
        let profile = ColorProfile::new_srgb();
        let transform = Transform::rgb(&profile, &profile).ok_or("sRGB transform unavailable")?;
        let colors = [[0.15, 0.35, 0.8], [0.7, 0.2, 0.1]];
        let singles = colors.map(|color| transform.map_rgb(color));
        let mut batch = colors;
        transform.map_rgb_batch(&mut batch);
        assert_eq!(singles, batch);
        Ok(())
    }

    #[test]
    fn mismatched_pixel_layouts_do_not_modify_buffers() -> Result<(), &'static str> {
        let profile = ColorProfile::new_srgb();
        let rgb = Transform::rgb(&profile, &profile).ok_or("RGB transform unavailable")?;
        let rgba = Transform::rgba8(&profile, &profile).ok_or("RGBA transform unavailable")?;
        let mut floats = [[0.2, 0.5, 0.8]];
        let mut bytes = [20, 60, 90, 210];
        rgba.map_rgb_batch(&mut floats);
        rgb.apply(PixelBuffer::Rgba8(&mut bytes));
        assert_eq!(floats, [[0.2, 0.5, 0.8]]);
        assert_eq!(bytes, [20, 60, 90, 210]);
        Ok(())
    }

    #[test]
    fn rgba8_identity_preserves_alpha() -> Result<(), &'static str> {
        let profile = ColorProfile::new_srgb();
        let transform = Transform::rgba8(&profile, &profile).ok_or("RGBA transform unavailable")?;
        let mut pixels = [40, 80, 120, 17, 200, 150, 100, 231];
        transform.apply(PixelBuffer::Rgba8(&mut pixels));
        assert_eq!([pixels[3], pixels[7]], [17, 231]);
        Ok(())
    }

    #[test]
    fn encoded_display_p3_profile_round_trips_through_srgb() -> Result<(), &'static str> {
        // Exercise the actual ICC byte parser and both RGB conversion
        // directions, not only synthetic in-memory sRGB profile objects.
        let display = ColorProfile::new_display_p3();
        let bytes = display
            .encode()
            .map_err(|_| "Cannot encode a Display P3 ICC profile")?;
        let forward = Transform::from_srgb_to_display(&bytes)
            .map_err(|_| "Cannot parse or create Display P3 output transform")?;
        let decoded = open_profile(&bytes).ok_or("Cannot decode Display P3 ICC bytes")?;
        let inverse = Transform::rgb(&decoded, &ColorProfile::new_srgb())
            .ok_or("Cannot create Display P3 input transform")?;
        let initial = [0.3, 0.5, 0.7];
        let restored = inverse.map_rgb(forward.map_rgb(initial));
        for (actual, expected) in restored.into_iter().zip(initial) {
            assert!(
                (actual - expected).abs() < 0.05,
                "Display P3 conversion exceeded round-trip tolerance"
            );
        }
        Ok(())
    }

    #[test]
    fn incomplete_rgba8_buffer_is_unchanged() {
        let mut pixels = [11, 22, 33];
        convert_rgba8_to_srgb(&mut pixels, b"");
        assert_eq!(pixels, [11, 22, 33]);
    }
}
