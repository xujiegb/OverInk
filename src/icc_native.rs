//! Native Little CMS ownership and conversion boundary.
//!
//! All raw profile and transform handles stay here. Public callers receive
//! owned resources and work only with Rust slices or fixed-size RGB values.
#![deny(clippy::undocumented_unsafe_blocks)]
use lcms2_sys as cms;
use std::{ffi::c_void, ptr::NonNull};

// Creating the standard sRGB profile accepts no pointer or memory owned by
// Rust. A non-null result is transferred to Profile and freed in Drop.
unsafe extern "C" {
    #[link_name = "cmsCreate_sRGBProfile"]
    safe fn native_create_srgb_profile() -> cms::HPROFILE;
}

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;

struct Profile(NonNull<c_void>);

impl Profile {
    fn open(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_PROFILE_BYTES {
            return None;
        }
        let len = u32::try_from(bytes.len()).ok()?;
        // SAFETY: Little CMS borrows this byte slice only during this call.
        let raw = unsafe { cms::cmsOpenProfileFromMem(bytes.as_ptr().cast(), len) };
        NonNull::new(raw).map(Self)
    }

    fn srgb() -> Option<Self> {
        // The parameter-free Little CMS constructor transfers ownership
        // only on success. Profile::drop remains the native release point.
        let raw = native_create_srgb_profile();
        NonNull::new(raw).map(Self)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // SAFETY: Profile exclusively owns a non-null Little CMS profile.
        unsafe { cms::cmsCloseProfile(self.0.as_ptr()) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    InvalidProfile,
    ConversionFailed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PixelLayout {
    RgbFloat,
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    Rgba8,
}

/// Keep the native transform's pixel layout attached to its handle so
/// safe callers cannot invoke Little CMS with an incompatible buffer.
pub struct Transform {
    raw: NonNull<c_void>,
    layout: PixelLayout,
}

enum PixelBuffer<'a> {
    Rgb(&'a mut [[f32; 3]]),
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    Rgba8(&'a mut [u8]),
}

impl Transform {
    fn create(input: &Profile, output: &Profile, layout: PixelLayout) -> Option<Self> {
        let (format, flags) = match layout {
            PixelLayout::RgbFloat => (cms::PixelFormat::RGB_FLT, cms::FLAGS_BLACKPOINTCOMPENSATION),
            #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
            PixelLayout::Rgba8 => (
                cms::PixelFormat::RGBA_8,
                cms::FLAGS_COPY_ALPHA | cms::FLAGS_BLACKPOINTCOMPENSATION,
            ),
        };
        // SAFETY: Both profiles remain live during creation. Little CMS
        // takes its own transform ownership, paired with Transform::drop.
        let raw = unsafe {
            cms::cmsCreateTransform(
                input.0.as_ptr(),
                format,
                output.0.as_ptr(),
                format,
                cms::Intent::RelativeColorimetric,
                flags,
            )
        };
        NonNull::new(raw).map(|raw| Self { raw, layout })
    }

    fn rgb(input: &Profile, output: &Profile) -> Option<Self> {
        Self::create(input, output, PixelLayout::RgbFloat)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    fn rgba8(input: &Profile, output: &Profile) -> Option<Self> {
        Self::create(input, output, PixelLayout::Rgba8)
    }

    pub fn from_srgb_to_display(bytes: &[u8]) -> Result<Self, DisplayError> {
        let output = Profile::open(bytes).ok_or(DisplayError::InvalidProfile)?;
        let input = Profile::srgb().ok_or(DisplayError::ConversionFailed)?;
        Self::rgb(&input, &output).ok_or(DisplayError::ConversionFailed)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn from_display_to_srgb(bytes: &[u8]) -> Option<Self> {
        let input = Profile::open(bytes)?;
        let output = Profile::srgb()?;
        Self::rgb(&input, &output)
    }

    pub fn map_rgb(&self, input: [f32; 3]) -> [f32; 3] {
        // The batch method already owns the audited Little CMS call and
        // supports in-place RGB_FLT transforms, including a single pixel.
        let mut pixel = [input];
        self.map_rgb_batch(&mut pixel);
        pixel[0]
    }

    /// Only matching native pixel layouts reach cmsDoTransform. All size
    /// checks and count conversions happen before crossing the FFI boundary.
    fn apply(&self, pixels: PixelBuffer<'_>) {
        let (data, count) = match (self.layout, pixels) {
            (PixelLayout::RgbFloat, PixelBuffer::Rgb(colors)) => {
                let Ok(count) = u32::try_from(colors.len()) else {
                    return;
                };
                let Some(data) =
                    std::ptr::NonNull::<std::ffi::c_void>::new(colors.as_mut_ptr().cast())
                else {
                    return;
                };
                (data, count)
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
            (PixelLayout::Rgba8, PixelBuffer::Rgba8(colors)) => {
                if !colors.len().is_multiple_of(4) {
                    return;
                }
                let Ok(count) = u32::try_from(colors.len() / 4) else {
                    return;
                };
                let Some(data) =
                    std::ptr::NonNull::<std::ffi::c_void>::new(colors.as_mut_ptr().cast())
                else {
                    return;
                };
                (data, count)
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
            _ => return,
        };
        if count == 0 {
            return;
        }
        // SAFETY: The stored layout matches the checked borrowed pixel
        // buffer. Its native allocation remains valid through this call;
        // Little CMS supports these exact in-place pixel transforms.
        unsafe {
            cms::cmsDoTransform(
                self.raw.as_ptr(),
                data.as_ptr().cast_const(),
                data.as_ptr(),
                count,
            );
        }
    }

    pub fn map_rgb_batch(&self, colors: &mut [[f32; 3]]) {
        self.apply(PixelBuffer::Rgb(colors));
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        // SAFETY: Transform exclusively owns the non-null native handle.
        unsafe { cms::cmsDeleteTransform(self.raw.as_ptr()) };
    }
}

/// Convert a packed RGBA8 image from its embedded profile into sRGB while
/// preserving the alpha channel. Invalid profiles leave the pixels unchanged.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    if pixels.is_empty() || !pixels.len().is_multiple_of(4) {
        return;
    }
    let Some(input) = Profile::open(profile) else {
        return;
    };
    let Some(output) = Profile::srgb() else {
        return;
    };
    let Some(transform) = Transform::rgba8(&input, &output) else {
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
        let input = Profile::srgb().ok_or("Little CMS sRGB profile unavailable")?;
        let output = Profile::srgb().ok_or("Little CMS sRGB profile unavailable")?;
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
        let input = Profile::srgb().ok_or("Little CMS sRGB profile unavailable")?;
        let output = Profile::srgb().ok_or("Little CMS sRGB profile unavailable")?;
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
        let input = Profile::srgb().ok_or("Little CMS sRGB profile unavailable")?;
        let output = Profile::srgb().ok_or("Little CMS sRGB profile unavailable")?;
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
    fn rejects_incomplete_rgba8_pixels() {
        let mut pixels = vec![11, 22, 33];
        convert_rgba8_to_srgb(&mut pixels, b"");
        assert_eq!(pixels, [11, 22, 33]);
    }
}
