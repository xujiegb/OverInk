//! Minimal native input-region adapters. These change only input routing, never
//! the overlay alpha, so existing ink stays visible while clicks pass through.
use crate::sdl::*;
use std::ffi::c_void;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use unix::Passthrough;

#[cfg(target_os = "windows")]
pub struct Passthrough;
#[cfg(target_os = "windows")]
impl Passthrough {
    pub fn new(_window: &Window) -> Self {
        Self
    }
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
            #[cfg(target_pointer_width = "64")]
            let style = GetWindowLongPtrW(hwnd, -20);
            #[cfg(target_pointer_width = "32")]
            let style = GetWindowLongW(hwnd, -20) as isize;
            let style = if pass {
                style | 0x20 | 0x80000
            } else {
                style & !0x20
            }; // TRANSPARENT, LAYERED
            SetLastError(0);
            #[cfg(target_pointer_width = "64")]
            let prev = SetWindowLongPtrW(hwnd, -20, style);
            #[cfg(target_pointer_width = "32")]
            let prev = SetWindowLongW(hwnd, -20, style as i32) as isize;
            if prev == 0 && GetLastError() != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            if SetWindowPos(
                hwnd,
                (-1isize) as *mut c_void,
                0,
                0,
                0,
                0,
                0x1 | 0x2 | 0x10 | 0x20,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(())
        }
    }
}
#[cfg(target_os = "macos")]
pub struct Passthrough;
#[cfg(target_os = "macos")]
impl Passthrough {
    pub fn new(_window: &Window) -> Self {
        Self
    }
    pub fn set(&mut self, window: &Window, pass: bool) -> Result<(), String> {
        unsafe {
            #[link(name = "objc")]
            unsafe extern "C" {
                fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
                fn objc_msgSend();
            }
            let ns = SDL_GetPointerProperty(
                SDL_GetWindowProperties(window.raw),
                SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
                std::ptr::null_mut(),
            );
            if ns.is_null() {
                return Err("No Cocoa window handle".into());
            }
            let send: unsafe extern "C" fn(*mut c_void, *mut c_void, bool) =
                std::mem::transmute(objc_msgSend as *const ());
            send(
                ns,
                sel_registerName(c"setIgnoresMouseEvents:".as_ptr()),
                pass,
            );
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
    pub fn set(&mut self, _: &Window, _: bool) -> Result<(), String> {
        Err("Input passthrough is unavailable on this platform".into())
    }
}

pub fn keep_controller_above(controller: &Window) {
    let _ = controller;
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let _ = unix::raise_controller(controller);
}
