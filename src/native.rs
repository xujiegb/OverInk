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
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) use unix::{NativeLibrary, library as native_library};

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
    // Calculate a bounded GDI subtraction before any native calls. The
    // rectangle's right/bottom edges are now guaranteed not to overflow i32.
    if size[0] <= 0 || size[1] <= 0 {
        return Err("Invalid Win32 region dimensions".into());
    }
    let hole = hole.and_then(|hole| hole.clipped(size));
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
        #[cfg(target_pointer_width = "64")]
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW};
        #[cfg(target_pointer_width = "32")]
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW};
        use windows::Win32::{
            Foundation::{ERROR_SUCCESS, GetLastError, HWND, SetLastError},
            UI::WindowsAndMessaging::{
                GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                SWP_NOZORDER, SetWindowPos, WS_EX_LAYERED, WS_EX_TRANSPARENT,
            },
        };

        let raw = window
            .win32_hwnd()
            .ok_or("No Win32 window handle")?
            .as_ptr();
        if self.applied == Some((raw, pass)) {
            return Ok(());
        }
        self.invalidate();
        let hwnd = HWND(raw);
        // SAFETY: The HWND is borrowed from the live SDL window; the typed
        // windows bindings preserve the system ABI. Clearing last-error
        // distinguishes a valid zero style from an API failure.
        unsafe {
            SetLastError(ERROR_SUCCESS);
            #[cfg(target_pointer_width = "64")]
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            #[cfg(target_pointer_width = "32")]
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as isize;
            if style == 0 && GetLastError() != ERROR_SUCCESS {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let desired = if pass {
                style | WS_EX_TRANSPARENT.0 as isize | WS_EX_LAYERED.0 as isize
            } else {
                style & !(WS_EX_TRANSPARENT.0 as isize)
            };
            if desired != style {
                SetLastError(ERROR_SUCCESS);
                #[cfg(target_pointer_width = "64")]
                let prev = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired);
                #[cfg(target_pointer_width = "32")]
                let prev = SetWindowLongW(hwnd, GWL_EXSTYLE, desired as i32) as isize;
                if prev == 0 && GetLastError() != ERROR_SUCCESS {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
            // Refresh the style without raising the canvas over the toolbar.
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .map_err(|error| error.to_string())?;
        }
        self.applied = Some((raw, pass));
        Ok(())
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
    {
        use windows::Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{
                HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
                SetWindowPos,
            },
        };
        if let Some(handle) = controller.win32_hwnd() {
            // SAFETY: The HWND is owned by the live SDL controller. The typed
            // Win32 API receives the documented topmost pseudo-handle and flags.
            let _ = unsafe {
                SetWindowPos(
                    HWND(handle.as_ptr()),
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )
            };
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
    {
        use windows::Win32::{
            Foundation::{HWND, POINT, RECT},
            Graphics::Gdi::ScreenToClient,
            UI::WindowsAndMessaging::GetWindowRect,
        };
        let window = HWND(controller.win32_hwnd()?.as_ptr());
        let surface = HWND(canvas.win32_hwnd()?.as_ptr());
        let mut rect = RECT::default();
        // SAFETY: Both HWNDs belong to live SDL windows. RECT and POINT are
        // initialized, writable Win32 values. Typed bindings check the ABI.
        unsafe {
            if GetWindowRect(window, &mut rect).is_err() {
                return None;
            }
            let mut point = POINT {
                x: rect.left,
                y: rect.top,
            };
            if !ScreenToClient(surface, &mut point).as_bool() {
                return None;
            }
            let logical = canvas.size();
            let pixels = canvas.pixel_size();
            if pixels[0] <= 0 || pixels[1] <= 0 {
                return None;
            }
            let scale = [logical[0] / pixels[0] as f32, logical[1] / pixels[1] as f32];
            Some([
                point.x as f32 * scale[0],
                point.y as f32 * scale[1],
                (rect.right - rect.left) as f32 * scale[0],
                (rect.bottom - rect.top) as f32 * scale[1],
            ])
        }
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
