//! SDL is owned by the main thread. Window/renderer pointers never cross threads.
pub use sdl3_sys::{
    blendmode::*, clipboard::*, error::*, events::*, hints::*, init::*, keyboard::*, keycode::*,
    mouse::*, pen::*, pixels::*, properties::*, rect::*, render::*, stdinc::*, surface::*,
    timer::*, touch::*, video::*,
};
use std::{
    ffi::{CStr, CString},
    ptr,
};

pub fn error() -> String {
    unsafe {
        let p = SDL_GetError();
        if p.is_null() {
            "SDL error".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
pub fn check(value: bool) -> Result<(), String> {
    if value { Ok(()) } else { Err(error()) }
}
pub fn cstring(v: &str) -> CString {
    CString::new(v.replace('\0', "")).unwrap()
}
pub struct Runtime;
impl Runtime {
    pub fn new(x11: bool) -> Result<Self, String> {
        unsafe {
            if x11 {
                SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"x11".as_ptr());
            }
            // Avoid a second mouse/touch stroke synthesized from the same pen packet.
            SDL_SetHint(SDL_HINT_PEN_MOUSE_EVENTS, c"0".as_ptr());
            SDL_SetHint(SDL_HINT_PEN_TOUCH_EVENTS, c"0".as_ptr());
            SDL_SetHint(SDL_HINT_TOUCH_MOUSE_EVENTS, c"0".as_ptr());
            SDL_SetHint(SDL_HINT_MOUSE_TOUCH_EVENTS, c"0".as_ptr());
            // The initial contact must draw even when it gives the canvas focus.
            SDL_SetHint(SDL_HINT_MOUSE_FOCUS_CLICKTHROUGH, c"1".as_ptr());
            SDL_SetAppMetadata(
                c"OverInk".as_ptr(),
                c"0.1.0".as_ptr(),
                c"org.overink.OverInk".as_ptr(),
            );
            check(SDL_Init(SDL_INIT_VIDEO))?;
            Ok(Self)
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe { SDL_Quit() }
    }
}
pub fn driver() -> String {
    unsafe {
        let p = SDL_GetCurrentVideoDriver();
        if p.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
pub struct Window {
    pub raw: *mut SDL_Window,
    pub id: SDL_WindowID,
}
impl Window {
    pub fn new(title: &str, w: i32, h: i32, overlay: bool) -> Result<Self, String> {
        unsafe {
            let title = cstring(title);
            let mut flags = SDL_WINDOW_HIDDEN
                | SDL_WINDOW_HIGH_PIXEL_DENSITY
                | SDL_WINDOW_ALWAYS_ON_TOP
                | SDL_WINDOW_RESIZABLE;
            if overlay {
                flags |= SDL_WINDOW_BORDERLESS | SDL_WINDOW_TRANSPARENT;
            }
            let raw = SDL_CreateWindow(title.as_ptr(), w, h, flags);
            if raw.is_null() {
                return Err(error());
            }
            Ok(Self {
                raw,
                id: SDL_GetWindowID(raw),
            })
        }
    }
    pub fn size(&self) -> [f32; 2] {
        unsafe {
            let (mut w, mut h) = (0, 0);
            SDL_GetWindowSize(self.raw, &mut w, &mut h);
            [w.max(1) as f32, h.max(1) as f32]
        }
    }
    pub fn pixel_size(&self) -> [i32; 2] {
        unsafe {
            let (mut w, mut h) = (0, 0);
            SDL_GetWindowSizeInPixels(self.raw, &mut w, &mut h);
            [w.max(1), h.max(1)]
        }
    }
    pub fn scale(&self) -> f32 {
        unsafe { SDL_GetWindowDisplayScale(self.raw).max(1.) }
    }
    pub fn show(&self, show: bool) -> Result<(), String> {
        unsafe {
            check(if show {
                SDL_ShowWindow(self.raw)
            } else {
                SDL_HideWindow(self.raw)
            })
        }
    }
    pub fn place_on_display(&self, index: usize, overlay: bool) -> Result<(), String> {
        unsafe {
            let mut count = 0;
            let displays = SDL_GetDisplays(&mut count);
            if displays.is_null() || count == 0 {
                return Err(error());
            }
            let display = *displays.add(index.min(count as usize - 1));
            SDL_free(displays.cast());
            let mut r = SDL_Rect::default();
            check(SDL_GetDisplayBounds(display, &mut r))?;
            if overlay {
                check(SDL_SetWindowSize(self.raw, r.w, r.h))?;
                check(SDL_SetWindowPosition(self.raw, r.x, r.y))?;
            } else {
                SDL_SetWindowPosition(self.raw, r.x + 32, r.y + 32);
            }
            Ok(())
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe { SDL_DestroyWindow(self.raw) }
    }
}
pub fn displays() -> Vec<String> {
    unsafe {
        let mut count = 0;
        let p = SDL_GetDisplays(&mut count);
        if p.is_null() {
            return vec![];
        }
        let names = (0..count as usize)
            .map(|i| {
                let n = SDL_GetDisplayName(*p.add(i));
                if n.is_null() {
                    format!("Display {}", i + 1)
                } else {
                    CStr::from_ptr(n).to_string_lossy().into_owned()
                }
            })
            .collect();
        SDL_free(p.cast());
        names
    }
}
pub struct Renderer {
    pub raw: *mut SDL_Renderer,
    pub extended: bool,
    pub name: String,
}
impl Renderer {
    pub fn new(window: &Window, force_sdr: bool) -> Result<Self, String> {
        unsafe {
            let props = SDL_CreateProperties();
            SDL_SetPointerProperty(
                props,
                SDL_PROP_RENDERER_CREATE_WINDOW_POINTER,
                window.raw.cast(),
            );
            let mut raw = ptr::null_mut();
            // SDL handles SDR white level and display changes inside the renderer.
            if !force_sdr && (cfg!(target_os = "windows") || cfg!(target_os = "macos")) {
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_RENDERER_CREATE_OUTPUT_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB_LINEAR.0 as i64,
                );
                raw = SDL_CreateRendererWithProperties(props);
            }
            if raw.is_null() {
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_RENDERER_CREATE_OUTPUT_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB.0 as i64,
                );
                raw = SDL_CreateRendererWithProperties(props);
            }
            SDL_DestroyProperties(props);
            if raw.is_null() {
                return Err(error());
            }
            let p = SDL_GetRendererProperties(raw);
            let extended = SDL_GetNumberProperty(p, SDL_PROP_RENDERER_OUTPUT_COLORSPACE_NUMBER, 0)
                == SDL_COLORSPACE_SRGB_LINEAR.0 as i64;
            let name = CStr::from_ptr(SDL_GetRendererName(raw))
                .to_string_lossy()
                .into_owned();
            SDL_SetRenderDrawBlendMode(raw, SDL_BLENDMODE_BLEND);
            SDL_SetRenderVSync(raw, 1);
            Ok(Self {
                raw,
                extended,
                name,
            })
        }
    }
    pub fn hdr(&self) -> (bool, f32) {
        unsafe {
            let p = SDL_GetRendererProperties(self.raw);
            (
                SDL_GetBooleanProperty(p, SDL_PROP_RENDERER_HDR_ENABLED_BOOLEAN, false),
                SDL_GetFloatProperty(p, SDL_PROP_RENDERER_HDR_HEADROOM_FLOAT, 1.),
            )
        }
    }
    pub fn begin(&self, window: &Window, color: [f32; 4]) -> Result<[f32; 2], String> {
        unsafe {
            let logical = window.size();
            let pixel = window.pixel_size();
            let scale = [pixel[0] as f32 / logical[0], pixel[1] as f32 / logical[1]];
            check(SDL_SetRenderScale(self.raw, scale[0], scale[1]))?;
            SDL_SetRenderClipRect(self.raw, ptr::null());
            check(SDL_SetRenderDrawColorFloat(
                self.raw, color[0], color[1], color[2], color[3],
            ))?;
            check(SDL_RenderClear(self.raw))?;
            Ok(scale)
        }
    }
    pub fn geometry(
        &self,
        texture: *mut SDL_Texture,
        vertices: &[SDL_Vertex],
        indices: &[u32],
    ) -> Result<(), String> {
        if vertices.is_empty() || indices.is_empty() {
            return Ok(());
        }
        unsafe {
            check(SDL_RenderGeometry(
                self.raw,
                texture,
                vertices.as_ptr(),
                vertices.len() as i32,
                indices.as_ptr().cast(),
                indices.len() as i32,
            ))
        }
    }
    pub fn present(&self) -> Result<(), String> {
        unsafe { check(SDL_RenderPresent(self.raw)) }
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { SDL_DestroyRenderer(self.raw) }
    }
}
