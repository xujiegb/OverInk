//! On X11 the display usually does not transform sRGB windows. Use SDL's
//! current-display ICC profile there. Other backends retain OS-managed sRGB /
//! scRGB surfaces; applying an ICC transform there would double-manage color.
use crate::{
    appearance::{Text, text},
    sdl::*,
};
use lcms2_sys as cms;
use std::ptr;
use std::{
    ffi::c_void,
    hash::{Hash, Hasher},
};
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

fn query_profile(window: &Window, size: &mut usize) -> *mut c_void {
    unsafe {
        let profile = SDL_GetWindowICCProfile(window.raw, size);
        if profile.is_null() {
            // An ICC profile is optional. Do not leave this error behind for
            // a later SDL operation that may fail without setting its own.
            SDL_ClearError();
        }
        profile
    }
}

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
        unsafe {
            let mut size = 0;
            let data = query_profile(window, &mut size);
            if data.is_null() || size == 0 || size > 16 * 1024 * 1024 {
                if !data.is_null() {
                    SDL_free(data)
                }
                self.status = text(
                    "X11 uses sRGB when an ICC profile is unavailable",
                    "X11 未找到 ICC 配置文件，按 sRGB 显示",
                    "X11 找不到 ICC 色彩描述檔，以 sRGB 顯示",
                );
                return;
            }
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            std::slice::from_raw_parts(data.cast::<u8>(), size).hash(&mut hash);
            self.revision = hash.finish();
            let output = cms::cmsOpenProfileFromMem(data, size as u32);
            SDL_free(data);
            if output.is_null() {
                self.status = text(
                    "Invalid ICC profile. X11 is using sRGB.",
                    "ICC 配置文件无效，X11 按 sRGB 显示",
                    "ICC 色彩描述檔無效，X11 以 sRGB 顯示",
                );
                return;
            }
            let input = cms::cmsCreate_sRGBProfile();
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
            unsafe {
                cms::cmsDoTransform(
                    self.transform,
                    rgb.as_ptr().cast(),
                    rgb.as_mut_ptr().cast(),
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
    unsafe {
        let mut size = 0;
        let data = query_profile(window, &mut size);
        if data.is_null() {
            return result;
        }
        let profile = if size > 0 && size <= 16 * 1024 * 1024 {
            cms::cmsOpenProfileFromMem(data, size as u32)
        } else {
            ptr::null_mut()
        };
        SDL_free(data);
        if profile.is_null() {
            return result;
        }
        let srgb = cms::cmsCreate_sRGBProfile();
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
