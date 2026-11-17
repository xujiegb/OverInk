//! Native Little CMS ownership and conversion boundary.
//!
//! All raw profile and transform handles stay here. Public callers receive
//! owned resources and work only with Rust slices or fixed-size RGB values.
use lcms2_sys as cms;

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;

struct Profile(cms::HPROFILE);

impl Profile {
    fn open(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_PROFILE_BYTES {
            return None;
        }
        let len = u32::try_from(bytes.len()).ok()?;
        // SAFETY: Little CMS borrows this byte slice only during this call.
        let raw = unsafe { cms::cmsOpenProfileFromMem(bytes.as_ptr().cast(), len) };
        (!raw.is_null()).then(|| Self(raw))
    }

    fn srgb() -> Option<Self> {
        // SAFETY: The returned handle is owned and closed by Profile::drop.
        let raw = unsafe { cms::cmsCreate_sRGBProfile() };
        (!raw.is_null()).then(|| Self(raw))
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // SAFETY: Profile exclusively owns a non-null Little CMS profile.
        unsafe { cms::cmsCloseProfile(self.0) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayError {
    InvalidProfile,
    ConversionFailed,
}

pub struct Transform(cms::HTRANSFORM);

impl Transform {
    fn rgb(input: &Profile, output: &Profile) -> Option<Self> {
        // SAFETY: The profiles remain live during creation. Little CMS owns
        // the returned transform and allows both profiles to be closed after.
        let raw = unsafe {
            cms::cmsCreateTransform(
                input.0,
                cms::PixelFormat::RGB_FLT,
                output.0,
                cms::PixelFormat::RGB_FLT,
                cms::Intent::RelativeColorimetric,
                cms::FLAGS_BLACKPOINTCOMPENSATION,
            )
        };
        (!raw.is_null()).then(|| Self(raw))
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    fn rgba8(input: &Profile, output: &Profile) -> Option<Self> {
        // SAFETY: The input/output profiles outlive creation, and the created
        // transform has its own independently managed native lifetime.
        let raw = unsafe {
            cms::cmsCreateTransform(
                input.0,
                cms::PixelFormat::RGBA_8,
                output.0,
                cms::PixelFormat::RGBA_8,
                cms::Intent::RelativeColorimetric,
                cms::FLAGS_COPY_ALPHA | cms::FLAGS_BLACKPOINTCOMPENSATION,
            )
        };
        (!raw.is_null()).then(|| Self(raw))
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
        let mut output = [0.0_f32; 3];
        // SAFETY: Both arrays hold exactly one RGB_FLT pixel. Little CMS
        // writes only the three output channels of this one pixel.
        unsafe {
            cms::cmsDoTransform(self.0, input.as_ptr().cast(), output.as_mut_ptr().cast(), 1);
        }
        output
    }

    pub fn map_rgb_batch(&self, colors: &mut [[f32; 3]]) {
        if colors.is_empty() {
            return;
        }
        let Ok(count) = u32::try_from(colors.len()) else {
            return;
        };
        // SAFETY: The transform was created for packed RGB_FLT data, and the
        // slice stores exactly count contiguous RGB triplets. Little CMS
        // explicitly supports in-place transformations.
        unsafe {
            let ptr = colors.as_mut_ptr();
            cms::cmsDoTransform(self.0, ptr.cast_const().cast(), ptr.cast(), count);
        }
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        // SAFETY: Transform exclusively owns the non-null native handle.
        unsafe { cms::cmsDeleteTransform(self.0) };
    }
}

/// Convert a packed RGBA8 image from its embedded profile into sRGB while
/// preserving the alpha channel. Invalid profiles leave the pixels unchanged.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    if pixels.is_empty() || !pixels.len().is_multiple_of(4) {
        return;
    }
    let Ok(count) = u32::try_from(pixels.len() / 4) else {
        return;
    };
    let Some(input) = Profile::open(profile) else {
        return;
    };
    let Some(output) = Profile::srgb() else {
        return;
    };
    let Some(transform) = Transform::rgba8(&input, &output) else {
        return;
    };
    // SAFETY: The transform expects RGBA_8. Pixel length is divisible by four,
    // count matches that length, and Little CMS supports this in-place alias.
    unsafe {
        let ptr = pixels.as_mut_ptr();
        cms::cmsDoTransform(transform.0, ptr.cast_const().cast(), ptr.cast(), count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_profile_does_not_modify_pixels() {
        let mut pixels = vec![11, 22, 33, 44, 55, 66, 77, 88];
        let before = pixels.clone();
        convert_rgba8_to_srgb(&mut pixels, b"not an ICC profile");
        assert_eq!(pixels, before);
    }

    #[test]
    fn rejects_incomplete_rgba8_pixels() {
        let mut pixels = vec![11, 22, 33];
        convert_rgba8_to_srgb(&mut pixels, b"");
        assert_eq!(pixels, [11, 22, 33]);
    }
}
