//! SDL is owned by the main thread. Window/renderer pointers never cross threads.
pub use sdl3_sys::{
    blendmode::*, clipboard::*, error::*, events::*, hints::*, init::*, keyboard::*, keycode::*,
    locale::*, mouse::*, pen::*, pixels::*, properties::*, rect::*, render::*, stdinc::*,
    surface::*, timer::*, touch::*, video::*,
};
use std::{
    ffi::{CStr, CString, c_void},
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
            let hint = SDL_GetHint(SDL_HINT_VIDEO_DRIVER);
            if x11 {
                SDL_SetHintWithPriority(SDL_HINT_VIDEO_DRIVER, c"x11".as_ptr(), SDL_HINT_OVERRIDE);
            } else if cfg!(any(target_os = "linux", target_os = "freebsd"))
                && std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty())
                && (std::env::var_os("WAYLAND_DISPLAY").is_some()
                    || std::env::var_os("WAYLAND_SOCKET").is_some()
                    || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland"))
                && (hint.is_null() || CStr::from_ptr(hint) == c"wayland")
            {
                // A desktop annotation needs XWayland's stacking requests
                // even if the session exports a default Wayland SDL driver.
                SDL_SetHintWithPriority(
                    SDL_HINT_VIDEO_DRIVER,
                    c"x11,wayland".as_ptr(),
                    SDL_HINT_OVERRIDE,
                );
            }
            // Avoid a second mouse/touch stroke synthesized from the same pen packet.
            SDL_SetHint(SDL_HINT_PEN_MOUSE_EVENTS, c"0".as_ptr());
            SDL_SetHint(SDL_HINT_PEN_TOUCH_EVENTS, c"0".as_ptr());
            SDL_SetHint(SDL_HINT_TOUCH_MOUSE_EVENTS, c"0".as_ptr());
            SDL_SetHint(SDL_HINT_MOUSE_TOUCH_EVENTS, c"0".as_ptr());
            // The initial contact must draw even when it gives the canvas focus.
            SDL_SetHint(SDL_HINT_MOUSE_FOCUS_CLICKTHROUGH, c"1".as_ptr());
            // A canvas drag must release on entering the separate controller.
            SDL_SetHint(SDL_HINT_MOUSE_AUTO_CAPTURE, c"0".as_ptr());
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
unsafe extern "C" fn toolbar_hit_test(
    window: *mut SDL_Window,
    point: *const SDL_Point,
    _: *mut c_void,
) -> SDL_HitTestResult {
    unsafe {
        if let Some(point) = point.as_ref() {
            let scale =
                SDL_GetWindowDisplayScale(window) / SDL_GetWindowPixelDensity(window).max(0.1);
            if point.x >= 0
                && (point.x as f32) < crate::toolbar::WIDTH * scale
                && point.y >= 0
                && (point.y as f32) < crate::toolbar::GRIP_HEIGHT * scale
            {
                return SDL_HITTEST_DRAGGABLE;
            }
        }
    }
    SDL_HITTEST_NORMAL
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
    pub fn tooltip(parent: &Self, x: i32, y: i32, w: i32, h: i32) -> Result<Self, String> {
        unsafe {
            let raw = SDL_CreatePopupWindow(
                parent.raw,
                x,
                y,
                w,
                h,
                SDL_WINDOW_TOOLTIP
                    | SDL_WINDOW_HIDDEN
                    | SDL_WINDOW_HIGH_PIXEL_DENSITY
                    | SDL_WINDOW_NOT_FOCUSABLE,
            );
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
    pub fn setup_toolbar(&self) {
        unsafe {
            SDL_SetWindowResizable(self.raw, false);
            if SDL_SetWindowHitTest(self.raw, Some(toolbar_hit_test), ptr::null_mut()) {
                SDL_SetWindowBordered(self.raw, false);
            }
            let icon = crate::toolbar::image();
            let (w, h) = (icon.width() as i32, icon.height() as i32);
            let mut pixels = icon.into_raw();
            let surface = SDL_CreateSurfaceFrom(
                w,
                h,
                SDL_PIXELFORMAT_RGBA32,
                pixels.as_mut_ptr().cast(),
                w * 4,
            );
            if !surface.is_null() {
                SDL_SetWindowIcon(self.raw, surface);
                SDL_DestroySurface(surface);
            }
        }
    }
    pub fn arrange_toolbar(&self, expanded: bool, on_display: Option<u32>) -> Result<(), String> {
        unsafe {
            let units = self.scale() / SDL_GetWindowPixelDensity(self.raw).max(0.1);
            let width = crate::toolbar::WIDTH
                + if expanded {
                    crate::toolbar::PANEL_WIDTH
                } else {
                    0.
                };
            let (mut w, mut h) = (
                (width * units).round() as i32,
                (crate::toolbar::HEIGHT * units).round() as i32,
            );
            let mut bounds = SDL_Rect::default();
            let display = on_display.or_else(|| self.display_id());
            let bounded = display
                .is_some_and(|id| SDL_GetDisplayUsableBounds(SDL_DisplayID(id), &mut bounds));
            if bounded {
                w = w.min((bounds.w - 16).max(1));
                h = h.min((bounds.h - 16).max(1));
            }
            check(SDL_SetWindowSize(self.raw, w, h))?;
            let driver = driver();
            if bounded && driver != "wayland" {
                let (mut x, mut y) = (0, 0);
                if on_display.is_some() || !SDL_GetWindowPosition(self.raw, &mut x, &mut y) {
                    x = bounds.x + if driver == "x11" { 0 } else { 8 };
                    y = bounds.y + (bounds.h - h) / 2;
                }
                x = x.clamp(bounds.x, bounds.x + (bounds.w - w).max(0));
                y = y.clamp(bounds.y, bounds.y + (bounds.h - h).max(0));
                check(SDL_SetWindowPosition(self.raw, x, y))?;
                check(SDL_SyncWindow(self.raw))?;
            }
            Ok(())
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
    pub fn display_id(&self) -> Option<u32> {
        let id = unsafe { SDL_GetDisplayForWindow(self.raw) }.0;
        (id != 0).then_some(id)
    }
    pub fn hdr_enabled(&self) -> bool {
        unsafe {
            SDL_GetBooleanProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_HDR_ENABLED_BOOLEAN,
                false,
            ) || self.display_id().is_some_and(|id| {
                SDL_GetBooleanProperty(
                    SDL_GetDisplayProperties(SDL_DisplayID(id)),
                    SDL_PROP_DISPLAY_HDR_ENABLED_BOOLEAN,
                    false,
                )
            })
        }
    }
    pub fn place_on_display(&self, id: u32, overlay: bool) -> Result<(), String> {
        unsafe {
            let display = SDL_DisplayID(id);
            let mut r = SDL_Rect::default();
            check(SDL_GetDisplayBounds(display, &mut r))?;
            if overlay && driver() == "wayland" {
                // XDG fullscreen hides unrelated content behind transparent
                // surfaces. Maximizing keeps the desktop in the compositor.
                check(SDL_MaximizeWindow(self.raw))?;
            } else if overlay {
                check(SDL_SetWindowSize(self.raw, r.w, r.h))?;
                check(SDL_SetWindowPosition(self.raw, r.x, r.y))?;
                // Finish the pending native resize before a software renderer
                // allocates its window surface at the old hidden-window size.
                check(SDL_SyncWindow(self.raw))?;
            } else {
                let center = SDL_WINDOWPOS_CENTERED_DISPLAY(display);
                check(SDL_SetWindowPosition(self.raw, center, center))?;
            }
            Ok(())
        }
    }
    pub fn recover_on_display(&self, id: u32) -> Result<(), String> {
        unsafe {
            if driver() == "wayland" {
                return Ok(());
            }
            let mut bounds = SDL_Rect::default();
            check(SDL_GetDisplayUsableBounds(SDL_DisplayID(id), &mut bounds))?;
            let (mut x, mut y) = (0, 0);
            check(SDL_GetWindowPosition(self.raw, &mut x, &mut y))?;
            let size = self.size();
            let center = [x + (size[0] * 0.5) as i32, y + (size[1] * 0.5) as i32];
            if center[0] < bounds.x
                || center[0] >= bounds.x + bounds.w
                || center[1] < bounds.y
                || center[1] >= bounds.y + bounds.h
            {
                self.place_on_display(id, false)?;
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
pub struct Renderer {
    pub raw: *mut SDL_Renderer,
    pub extended: bool,
    pub name: String,
}
impl Renderer {
    pub fn new(window: &Window, force_sdr: bool) -> Result<Self, String> {
        Self::create(window, force_sdr, None)
    }
    pub fn software(window: &Window) -> Result<Self, String> {
        Self::create(window, true, Some(c"software"))
    }
    fn create(window: &Window, force_sdr: bool, driver: Option<&CStr>) -> Result<Self, String> {
        unsafe {
            let props = SDL_CreateProperties();
            SDL_SetPointerProperty(
                props,
                SDL_PROP_RENDERER_CREATE_WINDOW_POINTER,
                window.raw.cast(),
            );
            if let Some(driver) = driver {
                SDL_SetStringProperty(props, SDL_PROP_RENDERER_CREATE_NAME_STRING, driver.as_ptr());
            }
            let mut raw = ptr::null_mut();
            // SDL's transparent Win32 swap chain uses the DXGI bitblt model.
            // Submit sRGB ink to that path so DWM composites it as SDR.
            let transparent = SDL_GetWindowFlags(window.raw) & SDL_WINDOW_TRANSPARENT != 0;
            let extended_output =
                cfg!(target_os = "macos") || (cfg!(target_os = "windows") && !transparent);
            if !force_sdr && extended_output {
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_RENDERER_CREATE_OUTPUT_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB_LINEAR.0 as i64,
                );
                raw = SDL_CreateRendererWithProperties(props);
                #[cfg(target_os = "macos")]
                if raw.is_null() {
                    crate::native::macos::remove_metal_view(window);
                }
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
            #[cfg(target_os = "macos")]
            if name == "metal" {
                crate::native::macos::configure_metal(raw, extended);
            }
            SDL_SetRenderDrawBlendMode(raw, SDL_BLENDMODE_BLEND);
            // One shared frame clock paces both windows. Two blocking presents
            // would halve the requested rate on some desktop drivers.
            SDL_SetRenderVSync(raw, 0);
            Ok(Self {
                raw,
                extended,
                name,
            })
        }
    }
    #[cfg(target_os = "macos")]
    pub fn change_color_output(&mut self, window: &Window, extended: bool) -> Result<(), String> {
        unsafe { SDL_DestroyRenderer(self.raw) };
        self.raw = ptr::null_mut();
        // SDL keeps its view attached for reuse. A fresh view gives SDR its
        // own BGRA8 drawable pool after previously presenting FP16.
        if self.name == "metal" {
            crate::native::macos::remove_metal_view(window);
        }
        *self = Self::new(window, !extended)?;
        Ok(())
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
            self.clear(color)?;
            Ok(scale)
        }
    }
    pub fn clear(&self, [r, g, b, a]: [f32; 4]) -> Result<(), String> {
        unsafe {
            check(SDL_SetRenderDrawColorFloat(self.raw, r, g, b, a))?;
            check(SDL_RenderClear(self.raw))
        }
    }
    pub fn with_target(
        &self,
        target: *mut SDL_Texture,
        draw: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        unsafe {
            let previous = SDL_GetRenderTarget(self.raw);
            let result = check(SDL_SetRenderTarget(self.raw, target)).and_then(|()| draw());
            let restore = check(SDL_SetRenderTarget(self.raw, previous));
            result.and(restore)
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
        if !self.raw.is_null() {
            unsafe { SDL_DestroyRenderer(self.raw) }
        }
    }
}
