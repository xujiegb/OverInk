//! Safe Little CMS ownership and conversion boundary.
//!
//! lcms2 owns every profile and transform, releasing the underlying C handles
//! through its RAII guards. Callers only provide validated Rust pixel slices.
#![forbid(unsafe_code)]

use lcms2::{
    Flags, Intent, PixelFormat, Profile, Transform as LcmsTransform,
};

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;

fn open_profile(bytes: &[u8]) -> Option<Profile> {
    if bytes.is_empty() || bytes.len() > MAX_PROFILE_BYTES {
        return None;
    }
    // Little CMS uses a 32-bit profile-buffer length.
    u32::try_from(bytes.len()).ok()?;
    Profile::new_icc(bytes).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    InvalidProfile,
    ConversionFailed,
}

/// Different native pixel formats are represented by different Rust types:
/// a floating RGB transform cannot receive a four-channel byte buffer.
enum PixelTransform {
    Rgb(LcmsTransform<[f32; 3], [f32; 3]>),
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    Rgba8(LcmsTransform<[u8; 4], [u8; 4]>),
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
    fn rgb(input: &Profile, output: &Profile) -> Option<Self> {
        LcmsTransform::<[f32; 3], [f32; 3]>::new_flags(
            input,
            PixelFormat::RGB_FLT,
            output,
            PixelFormat::RGB_FLT,
            Intent::RelativeColorimetric,
            Flags::BLACKPOINT_COMPENSATION,
        )
        .ok()
        .map(|transform| Self {
            inner: PixelTransform::Rgb(transform),
        })
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    fn rgba8(input: &Profile, output: &Profile) -> Option<Self> {
        LcmsTransform::<[u8; 4], [u8; 4]>::new_flags(
            input,
            PixelFormat::RGBA_8,
            output,
            PixelFormat::RGBA_8,
            Intent::RelativeColorimetric,
            Flags::BLACKPOINT_COMPENSATION | Flags::COPY_ALPHA,
        )
        .ok()
        .map(|transform| Self {
            inner: PixelTransform::Rgba8(transform),
        })
    }

    pub fn from_srgb_to_display(bytes: &[u8]) -> Result<Self, DisplayError> {
        let output = open_profile(bytes).ok_or(DisplayError::InvalidProfile)?;
        Self::rgb(&Profile::new_srgb(), &output).ok_or(DisplayError::ConversionFailed)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn from_display_to_srgb(bytes: &[u8]) -> Option<Self> {
        let input = open_profile(bytes)?;
        Self::rgb(&input, &Profile::new_srgb())
    }

    pub fn map_rgb(&self, input: [f32; 3]) -> [f32; 3] {
        let mut pixel = [input];
        self.map_rgb_batch(&mut pixel);
        pixel[0]
    }

    fn apply(&self, pixels: PixelBuffer<'_>) {
        match (&self.inner, pixels) {
            (PixelTransform::Rgb(transform), PixelBuffer::Rgb(colors)) => {
                if colors.is_empty() {
                    return;
                }
                // The safe wrapper handles a bounded pixel count. Each chunk
                // remains an ordinary mutable slice with a fixed RGB layout.
                for chunk in colors.chunks_mut(u32::MAX as usize) {
                    transform.transform_in_place(chunk);
                }
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
            (PixelTransform::Rgba8(transform), PixelBuffer::Rgba8(bytes)) => {
                if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
                    return;
                }
                let (pixels, remainder) = bytes.as_chunks_mut::<4>();
                if !remainder.is_empty() {
                    return;
                }
                for chunk in pixels.chunks_mut(u32::MAX as usize) {
                    transform.transform_in_place(chunk);
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

/// Convert packed RGBA8 to sRGB while preserving alpha. Invalid ICC profiles
/// and incomplete pixel buffers leave their input untouched.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    if pixels.is_empty() || !pixels.len().is_multiple_of(4) {
        return;
    }
    let Some(input) = open_profile(profile) else {
        return;
    };
    let Some(transform) = Transform::rgba8(&input, &Profile::new_srgb()) else {
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
    fn owned_srgb_profile_and_transform_round_trip() -> Result<(), &'static str> {
        let input = Profile::new_srgb();
        let output = Profile::new_srgb();
        let transform =
            Transform::rgb(&input, &output).ok_or("Could not create an sRGB transform")?;
        let rgb = [0.2, 0.5, 0.8];
        let mapped = transform.map_rgb(rgb);
        for (actual, expected) in mapped.into_iter().zip(rgb) {
            assert!(
                (actual - expected).abs() < 0.02,
                "sRGB identity transform changed a channel: {actual} != {expected}"
            );
        }
        Ok(())
    }

    #[test]
    fn single_rgb_and_batch_use_identical_transform() -> Result<(), &'static str> {
        let input = Profile::new_srgb();
        let output = Profile::new_srgb();
        let transform =
            Transform::rgb(&input, &output).ok_or("Could not create an sRGB transform")?;
        let colors = [[0.15, 0.35, 0.8], [0.7, 0.2, 0.1]];
        let single = colors.map(|color| transform.map_rgb(color));
        let mut batch = colors;
        transform.map_rgb_batch(&mut batch);
        for (one, many) in single.into_iter().zip(batch) {
            assert_eq!(one, many);
        }
        Ok(())
    }

    #[test]
    fn mismatched_pixel_layout_cannot_reach_native_transform() -> Result<(), &'static str> {
        let input = Profile::new_srgb();
        let output = Profile::new_srgb();
        let rgba =
            Transform::rgba8(&input, &output).ok_or("Could not create an RGBA8 transform")?;
        let rgb = Transform::rgb(&input, &output).ok_or("Could not create an RGB transform")?;
        let mut floats = [[0.2, 0.5, 0.8]];
        let mut bytes = [20, 60, 90, 210];
        rgba.map_rgb_batch(&mut floats);
        rgb.apply(PixelBuffer::Rgba8(&mut bytes));
        assert_eq!(floats, [[0.2, 0.5, 0.8]]);
        assert_eq!(bytes, [20, 60, 90, 210]);
        Ok(())
    }

    #[test]
    fn rgba8_identity_preserves_alpha_channel() -> Result<(), &'static str> {
        let profile = Profile::new_srgb();
        let transform = Transform::rgba8(&profile, &profile)
            .ok_or("Could not create an RGBA8 transform")?;
        let mut bytes = [40, 80, 120, 17, 200, 150, 100, 231];
        transform.apply(PixelBuffer::Rgba8(&mut bytes));
        assert_eq!(bytes[3], 17);
        assert_eq!(bytes[7], 231);
        Ok(())
    }

    #[test]
    fn rejects_incomplete_rgba8_pixels() {
        let mut pixels = vec![11, 22, 33];
        convert_rgba8_to_srgb(&mut pixels, b"");
        assert_eq!(pixels, [11, 22, 33]);
    }
}
