//! Minimal native input-region adapters. These change only input routing, never
//! the overlay alpha, so existing ink stays visible while clicks pass through.
use crate::sdl::*;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod unix;
#[cfg(target_os = "windows")]
pub mod windows_keyboard;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use unix::Passthrough;

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Default)]
pub struct Passthrough {
    applied: Option<(SDL_WindowID, bool)>,
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
    let hwnd_handle = window
        .win32_hwnd()
        .ok_or("No Win32 window handle")?
        .as_non_null();
    // SAFETY: The window remains live, and GDI region handles are checked
    // before use. This function destroys regions on failure and transfers
    // ownership to Windows only after SetWindowRgn succeeds.
    unsafe {
        let hwnd = HWND(hwnd_handle.as_ptr());
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

        if self.applied == Some((window.id, pass)) {
            return Ok(());
        }
        let identity = window
            .win32_hwnd()
            .ok_or("No Win32 window handle")?
            .as_non_null();
        self.invalidate();
        let hwnd = HWND(identity.as_ptr());
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
        self.applied = Some((window.id, pass));
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

        let identity = window
            .cocoa_window()
            .ok_or("No Cocoa window handle")?
            .as_non_null();
        // SAFETY: SDL owns the live NSWindow. The Cocoa selectors and Bool
        // return/argument types match the NSWindow methods being called.
        unsafe {
            let cocoa = identity.as_ptr().cast::<AnyObject>();
            if self.applied != Some((window.id, pass)) {
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
        }
        self.applied = Some((window.id, pass));
        Ok(())
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
                    HWND(handle.as_non_null().as_ptr()),
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
                let window = window.as_non_null().as_ptr().cast::<AnyObject>();
                let under = under.as_non_null().as_ptr().cast::<AnyObject>();
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

// Validate native RECT dimensions before narrowing to floating-point canvas
// units. The difference of two valid i32 coordinates need not fit in i32.
#[cfg(any(target_os = "windows", test))]
fn checked_native_rect_size(left: i32, top: i32, right: i32, bottom: i32) -> Option<[i64; 2]> {
    let width = i64::from(right) - i64::from(left);
    let height = i64::from(bottom) - i64::from(top);
    (width > 0 && height > 0).then_some([width, height])
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
        let window = HWND(controller.win32_hwnd()?.as_non_null().as_ptr());
        let surface = HWND(canvas.win32_hwnd()?.as_non_null().as_ptr());
        let mut rect = RECT::default();
        // SAFETY: Both HWNDs belong to live SDL windows. RECT and POINT are
        // initialized, writable Win32 values. Typed bindings check the ABI.
        let point = unsafe {
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
            point
        };
        // Win32 RECT coordinates can span the full signed 32-bit range.
        // Reject invalid sizes and widen differences outside the FFI block.
        let [width, height] =
            checked_native_rect_size(rect.left, rect.top, rect.right, rect.bottom)?;
        let logical = canvas.size();
        let pixels = canvas.pixel_size();
        if pixels[0] <= 0 || pixels[1] <= 0 {
            return None;
        }
        let scale = [logical[0] / pixels[0] as f32, logical[1] / pixels[1] as f32];
        Some([
            point.x as f32 * scale[0],
            point.y as f32 * scale[1],
            width as f32 * scale[0],
            height as f32 * scale[1],
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

#[cfg(test)]
mod tests {
    use super::checked_native_rect_size;

    #[test]
    fn win32_rectangle_dimensions_are_checked_without_overflow() {
        assert_eq!(checked_native_rect_size(1, 2, 101, 202), Some([100, 200]));
        assert_eq!(checked_native_rect_size(1, 2, 1, 202), None);
        assert_eq!(checked_native_rect_size(1, 2, 101, -2), None);
        assert_eq!(
            checked_native_rect_size(i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            Some([i64::from(u32::MAX), i64::from(u32::MAX)])
        );
    }
}
