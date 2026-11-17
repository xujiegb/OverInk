//! On X11 the display usually does not transform sRGB windows. Use SDL's
//! current-display ICC profile there. Other backends retain OS-managed sRGB /
//! scRGB surfaces; applying an ICC transform there would double-manage color.
#![forbid(unsafe_code)]

use crate::{
    appearance::{Text, text},
    icc_native::{DisplayError, Transform},
};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::sdl::Window;
use std::hash::{Hash, Hasher};

/// Convert embedded-profile RGBA8 images without exposing native CMS handles.
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
pub(crate) fn convert_rgba8_to_srgb(pixels: &mut [u8], profile: &[u8]) {
    crate::icc_native::convert_rgba8_to_srgb(pixels, profile);
}

pub struct ColorManager {
    transform: Option<Transform>,
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
            transform: None,
            status: SYSTEM_SRGB,
            revision: 0,
        }
    }
}

impl ColorManager {
    pub fn refresh(&mut self, window: &crate::sdl::Window, extended: bool) {
        self.revision = if extended { 1 } else { 2 };
        // Dropping the previous native transform is handled by RAII, including
        // when the current output no longer needs an application ICC transform.
        self.transform = None;
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

        match Transform::from_srgb_to_display(&profile) {
            Ok(transform) => {
                self.transform = Some(transform);
                self.status = text(
                    "X11 display ICC / Little CMS",
                    "X11 显示器 ICC / Little CMS",
                    "X11 顯示器 ICC / Little CMS",
                );
            }
            Err(DisplayError::InvalidProfile) => {
                self.status = text(
                    "Invalid ICC profile. X11 is using sRGB.",
                    "ICC 配置文件无效，X11 按 sRGB 显示",
                    "ICC 色彩描述檔無效，X11 以 sRGB 顯示",
                );
            }
            Err(DisplayError::ConversionFailed) => {
                self.status = text(
                    "ICC conversion failed. X11 is using sRGB.",
                    "ICC 转换失败，X11 按 sRGB 显示",
                    "ICC 轉換失敗，X11 以 sRGB 顯示",
                );
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
        for bytes in rgba.chunks_mut(4096 * 4) {
            let mut rgb: Vec<[f32; 3]> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| [v[0] as f32 / 255., v[1] as f32 / 255., v[2] as f32 / 255.])
                .collect();
            transform.map_rgb_batch(&mut rgb);
            for (pixel, rgb) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(rgb) {
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
