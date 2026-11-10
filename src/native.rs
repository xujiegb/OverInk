//! Minimal native input-region adapters. These change only input routing, never
//! the overlay alpha, so existing ink stays visible while clicks pass through.
use crate::sdl::*;
use std::ffi::c_void;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use unix::Passthrough;

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Default)]
pub struct Passthrough {
    applied: Option<(*mut c_void, bool)>,
}
#[cfg(any(target_os = "windows", target_os = "macos"))]
impl Passthrough {
    pub fn new(_window: &Window) -> Self {
        Self::default()
    }
    pub fn invalidate(&mut self) {
        self.applied = None;
    }
}

#[cfg(target_os = "windows")]
impl Passthrough {
    pub fn set(&mut self, window: &Window, pass: bool) -> Result<(), String> {
        unsafe {
            #[link(name = "user32")]
            unsafe extern "system" {
                fn SetWindowPos(
                    h: *mut c_void,
                    a: *mut c_void,
                    x: i32,
                    y: i32,
                    cx: i32,
                    cy: i32,
                    f: u32,
                ) -> i32;
            }
            #[cfg(target_pointer_width = "64")]
            #[link(name = "user32")]
            unsafe extern "system" {
                fn GetWindowLongPtrW(h: *mut c_void, i: i32) -> isize;
                fn SetWindowLongPtrW(h: *mut c_void, i: i32, v: isize) -> isize;
            }
            #[cfg(target_pointer_width = "32")]
            #[link(name = "user32")]
            unsafe extern "system" {
                fn GetWindowLongW(h: *mut c_void, i: i32) -> i32;
                fn SetWindowLongW(h: *mut c_void, i: i32, v: i32) -> i32;
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetLastError(e: u32);
                fn GetLastError() -> u32;
            }
            let hwnd = SDL_GetPointerProperty(
                SDL_GetWindowProperties(window.raw),
                SDL_PROP_WINDOW_WIN32_HWND_POINTER,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return Err("No Win32 window handle".into());
            }
            if self.applied == Some((hwnd, pass)) {
                return Ok(());
            }
            self.invalidate();
            const GWL_EXSTYLE: i32 = -20;
            const WS_EX_TRANSPARENT: isize = 0x20;
            const WS_EX_LAYERED: isize = 0x80000;
            SetLastError(0);
            #[cfg(target_pointer_width = "64")]
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            #[cfg(target_pointer_width = "32")]
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as isize;
            if style == 0 && GetLastError() != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let desired = if pass {
                style | WS_EX_TRANSPARENT | WS_EX_LAYERED
            } else {
                style & !WS_EX_TRANSPARENT
            };
            if desired != style {
                SetLastError(0);
                #[cfg(target_pointer_width = "64")]
                let prev = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired);
                #[cfg(target_pointer_width = "32")]
                let prev = SetWindowLongW(hwnd, GWL_EXSTYLE, desired as i32) as isize;
                if prev == 0 && GetLastError() != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
            // Refresh the style without raising the canvas over the toolbar.
            const SWP_NOSIZE: u32 = 0x1;
            const SWP_NOMOVE: u32 = 0x2;
            const SWP_NOZORDER: u32 = 0x4;
            const SWP_NOACTIVATE: u32 = 0x10;
            const SWP_FRAMECHANGED: u32 = 0x20;
            if SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            self.applied = Some((hwnd, pass));
            Ok(())
        }
    }
}
#[cfg(target_os = "macos")]
impl Passthrough {
    pub fn set(&mut self, window: &Window, pass: bool) -> Result<(), String> {
        use objc2::{
            msg_send,
            runtime::{AnyObject, Bool},
        };

        unsafe {
            let ns = SDL_GetPointerProperty(
                SDL_GetWindowProperties(window.raw),
                SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
                std::ptr::null_mut(),
            );
            if ns.is_null() {
                return Err("No Cocoa window handle".into());
            }
            let cocoa = ns.cast::<AnyObject>();
            if self.applied != Some((ns, pass)) {
                // SDL's Cocoa listener can reset ignoresMouseEvents when it
                // handles motion for a transparent window. Disable that motion
                // while passing input through, as Cocoa does for tooltips.
                let _: () = msg_send![cocoa, setAcceptsMouseMovedEvents: Bool::new(!pass)];
            }
            // A window-shape change or a queued event may have reset the
            // property even if the requested mode is unchanged.
            let ignoring: Bool = msg_send![cocoa, ignoresMouseEvents];
            if ignoring.as_bool() != pass {
                let _: () = msg_send![cocoa, setIgnoresMouseEvents: Bool::new(pass)];
            }
            self.applied = Some((ns, pass));
            Ok(())
        }
    }
}
#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd"
)))]
pub struct Passthrough;
#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd"
)))]
impl Passthrough {
    pub fn new(_: &Window) -> Self {
        Self
    }
    pub fn invalidate(&mut self) {}
    pub fn set(&mut self, _: &Window, _: bool) -> Result<(), String> {
        Err("Input passthrough is unavailable on this platform".into())
    }
}

pub fn keep_controller_above(canvas: &Window, controller: &Window, _native: &Passthrough) {
    let _ = canvas;
    #[cfg(target_os = "windows")]
    unsafe {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetWindowPos(
                h: *mut c_void,
                after: *mut c_void,
                x: i32,
                y: i32,
                w: i32,
                hgt: i32,
                flags: u32,
            ) -> i32;
        }
        let hwnd = SDL_GetPointerProperty(
            SDL_GetWindowProperties(controller.raw),
            SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        );
        if !hwnd.is_null() {
            // Keep drawing focus on the canvas; raising the controller must not
            // interrupt a pen stroke or activate the window beneath it.
            SetWindowPos(
                hwnd,
                (-1isize) as *mut c_void,
                0,
                0,
                0,
                0,
                0x1 | 0x2 | 0x10 | 0x200,
            );
        }
    }
    #[cfg(target_os = "macos")]
    unsafe {
        #[link(name = "objc")]
        unsafe extern "C" {
            fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
            fn objc_msgSend();
        }
        let window = SDL_GetPointerProperty(
            SDL_GetWindowProperties(controller.raw),
            SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
            std::ptr::null_mut(),
        );
        let under = SDL_GetPointerProperty(
            SDL_GetWindowProperties(canvas.raw),
            SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
            std::ptr::null_mut(),
        );
        if !window.is_null() && !under.is_null() {
            let get: unsafe extern "C" fn(*mut c_void, *mut c_void) -> isize =
                std::mem::transmute(objc_msgSend as *const ());
            let set: unsafe extern "C" fn(*mut c_void, *mut c_void, isize) =
                std::mem::transmute(objc_msgSend as *const ());
            let order: unsafe extern "C" fn(*mut c_void, *mut c_void, isize, isize) =
                std::mem::transmute(objc_msgSend as *const ());
            let level = get(under, sel_registerName(c"level".as_ptr())).saturating_add(1);
            if get(window, sel_registerName(c"level".as_ptr())) != level {
                set(window, sel_registerName(c"setLevel:".as_ptr()), level);
            }
            order(
                window,
                sel_registerName(c"orderWindow:relativeTo:".as_ptr()),
                1,
                get(under, sel_registerName(c"windowNumber".as_ptr())),
            );
        }
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    _native.raise_controller(controller);
}

pub fn show_canvas(canvas: &Window, controller: &Window, shown: bool) -> Result<(), String> {
    if crate::sdl::driver() == "wayland" {
        unsafe {
            // Transient toplevels stay above the fullscreen drawing surface.
            // Detach before hiding it so Hide cannot also hide the controls.
            let parent = if shown {
                canvas.raw
            } else {
                std::ptr::null_mut()
            };
            if SDL_GetWindowParent(controller.raw) != parent {
                check(SDL_SetWindowParent(controller.raw, parent))?;
            }
        }
    }
    canvas.show(shown)
}

pub fn controller_bounds(canvas: &Window, controller: &Window) -> Option<[f32; 4]> {
    #[cfg(target_os = "windows")]
    unsafe {
        #[repr(C)]
        struct Rect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[repr(C)]
        struct Point {
            x: i32,
            y: i32,
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowRect(w: *mut c_void, rect: *mut Rect) -> i32;
            fn ScreenToClient(w: *mut c_void, point: *mut Point) -> i32;
        }
        let window = SDL_GetPointerProperty(
            SDL_GetWindowProperties(controller.raw),
            SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        );
        let surface = SDL_GetPointerProperty(
            SDL_GetWindowProperties(canvas.raw),
            SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        );
        if window.is_null() || surface.is_null() {
            return None;
        }
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(window, &mut rect) == 0 {
            return None;
        }
        let mut point = Point {
            x: rect.left,
            y: rect.top,
        };
        if ScreenToClient(surface, &mut point) == 0 {
            return None;
        }
        let logical = canvas.size();
        let pixels = canvas.pixel_size();
        let scale = [logical[0] / pixels[0] as f32, logical[1] / pixels[1] as f32];
        Some([
            point.x as f32 * scale[0],
            point.y as f32 * scale[1],
            (rect.right - rect.left) as f32 * scale[0],
            (rect.bottom - rect.top) as f32 * scale[1],
        ])
    }
    #[cfg(not(target_os = "windows"))]
    unsafe {
        let (mut x, mut y, mut cx, mut cy) = (0, 0, 0, 0);
        if !SDL_GetWindowPosition(canvas.raw, &mut x, &mut y)
            || !SDL_GetWindowPosition(controller.raw, &mut cx, &mut cy)
        {
            return None;
        }
        let (mut top, mut left, mut bottom, mut right) = (0, 0, 0, 0);
        SDL_GetWindowBordersSize(controller.raw, &mut top, &mut left, &mut bottom, &mut right);
        let size = controller.size();
        Some([
            (cx - x - left) as f32,
            (cy - y - top) as f32,
            size[0] + (left + right) as f32,
            size[1] + (top + bottom) as f32,
        ])
    }
}
