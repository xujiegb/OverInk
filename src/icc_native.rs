//! Pure-Rust ICC profile parsing and in-place conversion with moxcms.
//! No native CMS handles, handwritten FFI, raw pointers, or unsafe code.
#![forbid(unsafe_code)]

use moxcms::{
    ColorProfile, DataColorSpace, InPlaceTransformExecutor, Layout, Matrix3d, RenderingIntent,
    ToneCurveEvaluator, TransformOptions, Vector3d,
};
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
        // Preserve the Little CMS relative-colorimetric rendering intent.
        // moxcms 0.9.2 automatically applies BPC scaling to some LUT paths
        // when both black points are detected. It has no public BPC toggle,
        // and matrix/shaper transforms are not guaranteed to receive BPC.
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

/// Explicit relative-colorimetric BPC for measured-black RGB matrix/TRC
/// profiles. moxcms 0.9.2's internal LUT black-point scaling is only
/// evaluated for perceptual/saturation intent, not RelativeColorimetric.
/// Offset and scale are applied in PCS D50 XYZ (not gamma-encoded RGB).
/// Unknown/unsupported profiles retain the library's original transform.
struct MatrixBlackPointCompensation {
    rgb_to_xyz: Matrix3d,
    xyz_to_rgb: Matrix3d,
    scale: [f64; 3],
    offset: [f64; 3],
    decode: [Box<dyn ToneCurveEvaluator + Send + Sync>; 3],
    encode: [Box<dyn ToneCurveEvaluator + Send + Sync>; 3],
}

impl MatrixBlackPointCompensation {
    fn new(source: &ColorProfile, destination: &ColorProfile) -> Option<Self> {
        if source.color_space != DataColorSpace::Rgb
            || destination.color_space != DataColorSpace::Rgb
            || source.pcs != DataColorSpace::Xyz
            || destination.pcs != DataColorSpace::Xyz
            // Destination LUT transforms cannot be inverted by RGB colorants.
            || destination.lut_b_to_a_colorimetric.is_some()
            || destination.lut_b_to_a_perceptual.is_some()
            || destination.lut_b_to_a_saturation.is_some()
        {
            return None;
        }
        // The presence of an ICC 'bkpt' tag is necessary for a reliable
        // measured black point; assuming one for an untagged profile would
        // alter normal sRGB, P3 and screenshot colors without evidence.
        if source.black_point.is_none() && destination.black_point.is_none() {
            return None;
        }
        let black = |profile: &ColorProfile| {
            profile.black_point.map_or([0.; 3], |bp| [bp.x, bp.y, bp.z])
        };
        let src = black(source);
        let dst = black(destination);
        const D50: [f64; 3] = [0.9642, 1.0, 0.8249];
        if (0..3).any(|i| {
            !src[i].is_finite()
                || !dst[i].is_finite()
                || src[i] < 0.
                || dst[i] < 0.
                || src[i] >= D50[i] * 0.5
                || dst[i] >= D50[i] * 0.5
        }) || (0..3).all(|i| (src[i] - dst[i]).abs() < 1e-8)
        {
            return None;
        }
        let rgb_to_xyz = Matrix3d {
            v: [
                [destination.red_colorant.x, destination.green_colorant.x, destination.blue_colorant.x],
                [destination.red_colorant.y, destination.green_colorant.y, destination.blue_colorant.y],
                [destination.red_colorant.z, destination.green_colorant.z, destination.blue_colorant.z],
            ],
        };
        let determinant = rgb_to_xyz.determinant()?;
        if !determinant.is_finite() || determinant.abs() < 1e-8
            || rgb_to_xyz.v.iter().flatten().any(|c| !c.is_finite())
        {
            return None;
        }
        let red = destination.red_trc.as_ref()?;
        let green = destination.green_trc.as_ref()?;
        let blue = destination.blue_trc.as_ref()?;
        let decode = [
            red.make_linear_evaluator().ok()?,
            green.make_linear_evaluator().ok()?,
            blue.make_linear_evaluator().ok()?,
        ];
        let encode = [
            red.make_gamma_evaluator().ok()?,
            green.make_gamma_evaluator().ok()?,
            blue.make_gamma_evaluator().ok()?,
        ];
        let scale = std::array::from_fn(|i| (D50[i] - dst[i]) / (D50[i] - src[i]));
        let offset = std::array::from_fn(|i| dst[i] - scale[i] * src[i]);
        Some(Self {
            xyz_to_rgb: rgb_to_xyz.inverse(),
            rgb_to_xyz,
            scale,
            offset,
            decode,
            encode,
        })
    }

    fn map(&self, rgb: [f32; 3]) -> [f32; 3] {
        if rgb.iter().any(|v| !v.is_finite() || !(0. ..=1.).contains(v)) {
            return rgb;
        }
        let linear = std::array::from_fn(|i| {
            f64::from(self.decode[i].evaluate_value(rgb[i]))
        });
        if linear.iter().any(|v| !v.is_finite()) {
            return rgb;
        }
        let pcs = self.rgb_to_xyz.mul_vector(Vector3d { v: linear }).v;
        let adjusted = std::array::from_fn(|i| self.scale[i] * pcs[i] + self.offset[i]);
        let corrected = self.xyz_to_rgb.mul_vector(Vector3d { v: adjusted }).v;
        if corrected.iter().any(|v| !v.is_finite()) {
            return rgb;
        }
        std::array::from_fn(|i| {
            let encoded = self.encode[i].evaluate_value(corrected[i].clamp(0., 1.) as f32);
            if encoded.is_finite() { encoded.clamp(0., 1.) } else { rgb[i] }
        })
    }
}

pub struct Transform {
    inner: PixelTransform,
    bpc: Option<MatrixBlackPointCompensation>,
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
                bpc: MatrixBlackPointCompensation::new(input, output),
            })
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    fn rgba8(input: &ColorProfile, output: &ColorProfile) -> Option<Self> {
        input
            .create_in_place_transform_8bit(Layout::Rgba, output, options())
            .ok()
            .map(|executor| Self {
                inner: PixelTransform::Rgba8(executor),
                bpc: MatrixBlackPointCompensation::new(input, output),
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
                    if let Some(bpc) = &self.bpc {
                        for color in chunk {
                            *color = bpc.map(*color);
                        }
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
                    if let Some(bpc) = &self.bpc {
                        for pixel in chunk.as_chunks_mut::<4>().0 {
                            let color = [
                                f32::from(pixel[0]) / 255.,
                                f32::from(pixel[1]) / 255.,
                                f32::from(pixel[2]) / 255.,
                            ];
                            let mapped = bpc.map(color);
                            for i in 0..3 {
                                pixel[i] = (mapped[i] * 255.).round() as u8;
                            }
                        }
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
    fn measured_black_point_is_compensated_in_relative_rgb_transform() -> Result<(), &'static str> {
        let srgb = ColorProfile::new_srgb();
        let mut measured = ColorProfile::new_srgb();
        measured.black_point = Some(moxcms::Xyzd {
            x: 0.00964,
            y: 0.01,
            z: 0.00825,
        });
        let plain = Transform::rgb(&srgb, &srgb).ok_or("Missing sRGB transform")?;
        let adjusted = Transform::rgb(&srgb, &measured).ok_or("Missing measured display transform")?;
        assert!(adjusted.bpc.is_some(), "Expected explicit matrix RGB BPC");
        let near_black = [0.02, 0.02, 0.02];
        let ordinary = plain.map_rgb(near_black);
        let corrected = adjusted.map_rgb(near_black);
        assert!(corrected.iter().zip(ordinary).all(|(a, b)| *a > b));
        let white = adjusted.map_rgb([1., 1., 1.]);
        assert!(white.iter().all(|v| (*v - 1.).abs() < 0.02));
        Ok(())
    }

    #[test]
    fn untagged_rgb_profiles_do_not_apply_black_point_compensation() -> Result<(), &'static str> {
        let profile = ColorProfile::new_srgb();
        let transform = Transform::rgb(&profile, &profile).ok_or("Missing sRGB transform")?;
        assert!(transform.bpc.is_none());
        Ok(())
    }

    #[test]
    fn incomplete_rgba8_buffer_is_unchanged() {
        let mut pixels = [11, 22, 33];
        convert_rgba8_to_srgb(&mut pixels, b"");
        assert_eq!(pixels, [11, 22, 33]);
    }
}
