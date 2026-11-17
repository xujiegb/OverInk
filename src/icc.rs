//! On X11 the display usually does not transform sRGB windows. Use SDL's
//! current-display ICC profile there. Other backends retain OS-managed sRGB /
//! scRGB surfaces; applying an ICC transform there would double-manage color.
use crate::appearance::{Text, text};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::sdl::Window;
use lcms2_sys as cms;
use std::hash::{Hash, Hasher};
use std::ptr;
/// Convert a captured RGBA8 image using the embedded input ICC profile.
///
/// Little CMS allows in-place RGBA8 transforms; the profile and transform stay
/// inside this FFI boundary and are closed on all successful allocation paths.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub(crate) fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    if profile.is_empty()
        || profile.len() > 16 * 1024 * 1024
        || pixels.is_empty()
        || !pixels.len().is_multiple_of(4)
    {
        return;
    }
    let Ok(count) = u32::try_from(pixels.len() / 4) else {
        return;
    };
    unsafe {
        let input = cms::cmsOpenProfileFromMem(profile.as_ptr().cast(), profile.len() as u32);
        if input.is_null() {
            return;
        }
        let srgb = cms::cmsCreate_sRGBProfile();
        if srgb.is_null() {
            cms::cmsCloseProfile(input);
            return;
        }
        let transform = cms::cmsCreateTransform(
            input,
            cms::PixelFormat::RGBA_8,
            srgb,
            cms::PixelFormat::RGBA_8,
            cms::Intent::RelativeColorimetric,
            cms::FLAGS_COPY_ALPHA | cms::FLAGS_BLACKPOINTCOMPENSATION,
        );
        cms::cmsCloseProfile(input);
        cms::cmsCloseProfile(srgb);
        if !transform.is_null() {
            // Little CMS explicitly supports transforming a buffer in place.
            let ptr = pixels.as_mut_ptr();
            cms::cmsDoTransform(transform, ptr.cast_const().cast(), ptr.cast(), count);
            cms::cmsDeleteTransform(transform);
        }
    }
}

pub struct ColorManager {
    transform: cms::HTRANSFORM,
    pub status: Text,
    pub revision: u64,
}
const SYSTEM_SRGB: Text = text(
    "sRGB surface / system compositor",
    "sRGB 表面 / 系统合成",
    "sRGB 表面 / 系統合成",
);

impl Default for ColorManager {
    fn default() -> Self {
        Self {
            transform: ptr::null_mut(),
            status: SYSTEM_SRGB,
            revision: 0,
        }
    }
}
impl ColorManager {
    pub fn refresh(&mut self, window: &crate::sdl::Window, extended: bool) {
        self.revision = if extended { 1 } else { 2 };
        if !self.transform.is_null() {
            unsafe { cms::cmsDeleteTransform(self.transform) };
            self.transform = ptr::null_mut()
        }
        self.status = if extended {
            text(
                "Extended linear sRGB / system display mapping",
                "扩展线性 sRGB / 系统显示映射",
                "延伸線性 sRGB / 系統顯示映射",
            )
        } else {
            SYSTEM_SRGB
        };
        if crate::sdl::driver() != "x11" {
            return;
        }
        let Some(profile) = window.icc_profile() else {
            self.status = text(
                "X11 uses sRGB when an ICC profile is unavailable",
                "X11 未找到 ICC 配置文件，按 sRGB 显示",
                "X11 找不到 ICC 色彩描述檔，以 sRGB 顯示",
            );
            return;
        };
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        profile.hash(&mut hash);
        self.revision = hash.finish();

        unsafe {
            let output = cms::cmsOpenProfileFromMem(profile.as_ptr().cast(), profile.len() as u32);
            if output.is_null() {
                self.status = text(
                    "Invalid ICC profile. X11 is using sRGB.",
                    "ICC 配置文件无效，X11 按 sRGB 显示",
                    "ICC 色彩描述檔無效，X11 以 sRGB 顯示",
                );
                return;
            }
            let input = cms::cmsCreate_sRGBProfile();
            if input.is_null() {
                cms::cmsCloseProfile(output);
                self.status = text(
                    "ICC conversion failed. X11 is using sRGB.",
                    "ICC 转换失败，X11 按 sRGB 显示",
                    "ICC 轉換失敗，X11 以 sRGB 顯示",
                );
                return;
            }
            self.transform = cms::cmsCreateTransform(
                input,
                cms::PixelFormat::RGB_FLT,
                output,
                cms::PixelFormat::RGB_FLT,
                cms::Intent::RelativeColorimetric,
                cms::FLAGS_BLACKPOINTCOMPENSATION,
            );
            cms::cmsCloseProfile(input);
            cms::cmsCloseProfile(output);
            self.status = if self.transform.is_null() {
                text(
                    "ICC conversion failed. X11 is using sRGB.",
                    "ICC 转换失败，X11 按 sRGB 显示",
                    "ICC 轉換失敗，X11 以 sRGB 顯示",
                )
            } else {
                text(
                    "X11 display ICC / Little CMS",
                    "X11 显示器 ICC / Little CMS",
                    "X11 顯示器 ICC / Little CMS",
                )
            };
        }
    }
    pub fn map(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.transform.is_null() {
            return rgb;
        }
        let mut out = [0.0_f32; 3];
        unsafe {
            cms::cmsDoTransform(
                self.transform,
                rgb.as_ptr().cast(),
                out.as_mut_ptr().cast(),
                1,
            )
        }
        out.map(|v| v.clamp(0., 1.))
    }
    pub fn is_identity(&self) -> bool {
        self.transform.is_null()
    }
    pub fn map_rgba8(&self, rgba: &mut [u8]) {
        if self.transform.is_null() {
            return;
        }
        for bytes in rgba.chunks_mut(4096 * 4) {
            let mut rgb: Vec<[f32; 3]> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| [v[0] as f32 / 255., v[1] as f32 / 255., v[2] as f32 / 255.])
                .collect();
            // Derive both FFI aliases from a single mutable raw pointer.
            // Little CMS explicitly supports in-place transformations.
            let pixels = rgb.as_mut_ptr();
            unsafe {
                cms::cmsDoTransform(
                    self.transform,
                    pixels.cast_const().cast(),
                    pixels.cast(),
                    rgb.len() as u32,
                );
            }
            for (pixel, rgb) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(rgb) {
                for i in 0..3 {
                    pixel[i] = (rgb[i].clamp(0., 1.) * 255.).round() as u8;
                }
            }
        }
    }
}
impl Drop for ColorManager {
    fn drop(&mut self) {
        if !self.transform.is_null() {
            unsafe { cms::cmsDeleteTransform(self.transform) }
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
    unsafe {
        let profile =
            cms::cmsOpenProfileFromMem(profile_data.as_ptr().cast(), profile_data.len() as u32);
        if profile.is_null() {
            return result;
        }
        let srgb = cms::cmsCreate_sRGBProfile();
        if srgb.is_null() {
            cms::cmsCloseProfile(profile);
            return result;
        }
        let transform = cms::cmsCreateTransform(
            profile,
            cms::PixelFormat::RGB_FLT,
            srgb,
            cms::PixelFormat::RGB_FLT,
            cms::Intent::RelativeColorimetric,
            cms::FLAGS_BLACKPOINTCOMPENSATION,
        );
        cms::cmsCloseProfile(profile);
        cms::cmsCloseProfile(srgb);
        if !transform.is_null() {
            cms::cmsDoTransform(
                transform,
                rgb.as_ptr().cast(),
                result.rgb.as_mut_ptr().cast(),
                1,
            );
            cms::cmsDeleteTransform(transform);
            result.rgb = result.rgb.map(|v| v.clamp(0., 1.));
        }
    }
    result
}
