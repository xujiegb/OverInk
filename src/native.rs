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
#[deny(clippy::undocumented_unsafe_blocks)]
pub fn set_controller_region(
    window: &Window,
    size: [i32; 2],
    hole: Option<crate::controller::Rect>,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, HGDIOBJ, RGN_DIFF, SetWindowRgn},
    };
    // SAFETY: The window remains live, and GDI region handles are checked
    // before use. This function destroys regions on failure and transfers
    // ownership to Windows only after SetWindowRgn succeeds.
    unsafe {
        let hwnd = HWND(
            window
                .win32_hwnd()
                .ok_or("No Win32 window handle")?
                .as_ptr(),
        );
        let region = if let Some(hole) = hole {
            let full = CreateRectRgn(0, 0, size[0], size[1]);
            let cut = CreateRectRgn(hole.x, hole.y, hole.x + hole.w, hole.y + hole.h);
            let valid = !full.is_invalid()
                && !cut.is_invalid()
                && CombineRgn(Some(full), Some(full), Some(cut), RGN_DIFF).0 != 0;
            if !cut.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(cut.0));
            }
            if !valid {
                if !full.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(full.0));
                }
                return Err("Could not create the Win32 drawing region".into());
            }
            Some(full)
        } else {
            None
        };
        if SetWindowRgn(hwnd, region, true) == 0 {
            if let Some(region) = region {
                let _ = DeleteObject(HGDIOBJ(region.0));
            }
            return Err("SetWindowRgn failed".into());
        }
        // Windows owns the region after SetWindowRgn succeeds.
        Ok(())
    }
}

#[cfg(target_os = "windows")]
#[deny(clippy::undocumented_unsafe_blocks)]
impl Passthrough {
    pub fn set(&mut self, window: &Window, pass: bool) -> Result<(), String> {
        // SAFETY: The Win32 HWND is borrowed from a live SDL window. The
        // imported User32 functions use their system calling conventions,
        // and all style changes are checked via GetLastError.
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
            let hwnd = window
                .win32_hwnd()
                .ok_or("No Win32 window handle")?
                .as_ptr();
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
#[deny(clippy::undocumented_unsafe_blocks)]
impl Passthrough {
    pub fn set(&mut self, window: &Window, pass: bool) -> Result<(), String> {
        use objc2::{
            msg_send,
            runtime::{AnyObject, Bool},
        };

        // SAFETY: SDL owns the live NSWindow. The Cocoa selectors and Bool
        // return/argument types match the NSWindow methods being called.
        unsafe {
            let ns = window
                .cocoa_window()
                .ok_or("No Cocoa window handle")?
                .as_ptr();
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

#[deny(clippy::undocumented_unsafe_blocks)]
pub fn keep_controller_above(canvas: &Window, controller: &Window, _native: &Passthrough) {
    let _ = canvas;
    #[cfg(target_os = "windows")]
    // SAFETY: The HWND is borrowed from a live SDL window, and SetWindowPos
    // is called with the documented HWND_TOPMOST pseudo-handle and flags.
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
        let hwnd = controller
            .win32_hwnd()
            .map_or(std::ptr::null_mut(), |handle| handle.as_ptr());
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
    {
        use objc2::{msg_send, runtime::AnyObject};

        let window = controller.cocoa_window();
        let under = canvas.cocoa_window();
        if let (Some(window), Some(under)) = (window, under) {
            // SAFETY: Both pointers are SDL-owned live Cocoa NSWindows. The
            // selectors have these NSInteger/void signatures on macOS.
            // objc2 performs typed sends, avoiding untyped objc_msgSend casts.
            unsafe {
                let window = window.as_ptr().cast::<AnyObject>();
                let under = under.as_ptr().cast::<AnyObject>();
                let canvas_level: isize = msg_send![under, level];
                let level = canvas_level.saturating_add(1);
                let current_level: isize = msg_send![window, level];
                if current_level != level {
                    let _: () = msg_send![window, setLevel: level];
                }
                let number: isize = msg_send![under, windowNumber];
                let _: () = msg_send![window, orderWindow: 1isize, relativeTo: number];
            }
        }
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    _native.raise_controller(controller);
}

pub fn show_canvas(canvas: &Window, controller: &Window, shown: bool) -> Result<(), String> {
    if crate::sdl::driver() == "wayland" {
        // Transient toplevels stay above the fullscreen drawing surface.
        // Detach before hiding it so Hide cannot also hide the controls.
        controller.set_parent(shown.then_some(canvas))?;
    }
    canvas.show(shown)
}

#[deny(clippy::undocumented_unsafe_blocks)]
pub fn controller_bounds(canvas: &Window, controller: &Window) -> Option<[f32; 4]> {
    #[cfg(target_os = "windows")]
    // SAFETY: Both HWNDs are validated before invoking User32. The output
    // structs have the C layout of RECT and POINT and remain valid for calls.
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
        let window = controller.win32_hwnd()?.as_ptr();
        let surface = canvas.win32_hwnd()?.as_ptr();
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
    {
        let [x, y] = canvas.position().ok()?;
        let [cx, cy] = controller.position().ok()?;
        let [top, left, bottom, right] = controller.border_size();
        let size = controller.size();
        Some([
            (cx - x - left) as f32,
            (cy - y - top) as f32,
            size[0] + (left + right) as f32,
            size[1] + (top + bottom) as f32,
        ])
    }
}
