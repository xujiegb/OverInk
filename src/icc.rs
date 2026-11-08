//! On X11 the display usually does not transform sRGB windows. Use SDL's
//! current-display ICC profile there. Other backends retain OS-managed sRGB /
//! scRGB surfaces; applying an ICC transform there would double-manage color.
use crate::sdl::*;
use lcms2_sys as cms;
use std::ptr;
pub struct ColorManager {
    transform: cms::HTRANSFORM,
    pub status: String,
}
impl Default for ColorManager {
    fn default() -> Self {
        Self {
            transform: ptr::null_mut(),
            status: "sRGB surface / system compositor".into(),
        }
    }
}
impl ColorManager {
    pub fn refresh(&mut self, window: &crate::sdl::Window, extended: bool) {
        if !self.transform.is_null() {
            unsafe { cms::cmsDeleteTransform(self.transform) };
            self.transform = ptr::null_mut()
        }
        self.status = if extended {
            "Extended linear sRGB / system display mapping".into()
        } else {
            "sRGB surface / system compositor".into()
        };
        if crate::sdl::driver() != "x11" {
            return;
        }
        unsafe {
            let mut size = 0;
            let data = SDL_GetWindowICCProfile(window.raw, &mut size);
            if data.is_null() || size == 0 || size > 16 * 1024 * 1024 {
                if !data.is_null() {
                    SDL_free(data)
                }
                self.status = "X11: no ICC profile; sRGB assumption".into();
                return;
            }
            let output = cms::cmsOpenProfileFromMem(data, size as u32);
            SDL_free(data);
            if output.is_null() {
                self.status = "X11: invalid ICC profile; sRGB assumption".into();
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
                "X11: ICC transform failed; sRGB assumption".into()
            } else {
                "X11: display ICC / Little CMS".into()
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
}
impl Drop for ColorManager {
    fn drop(&mut self) {
        if !self.transform.is_null() {
            unsafe { cms::cmsDeleteTransform(self.transform) }
        }
    }
}
