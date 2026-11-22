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
    #[cfg(target_os = "windows")]
    layered_for_passthrough: bool,
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
            let (desired, layered_for_passthrough) = passthrough_style(
                style,
                pass,
                self.layered_for_passthrough,
                WS_EX_TRANSPARENT.0 as isize,
                WS_EX_LAYERED.0 as isize,
            );
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
            // Track ownership as soon as the style changes, even if the next
            // native call fails and we retry the input transition.
            self.layered_for_passthrough = layered_for_passthrough;
            // Changing a frame must not steal focus from a slideshow.
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
#[cfg(any(target_os = "windows", test))]
fn passthrough_style(
    style: isize,
    pass: bool,
    added_layered: bool,
    transparent: isize,
    layered: isize,
) -> (isize, bool) {
    if pass {
        (
            style | transparent | layered,
            added_layered || style & layered == 0,
        )
    } else {
        let remove = transparent | if added_layered { layered } else { 0 };
        (style & !remove, false)
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
        let _ = raise_win32_overlay(controller);
    }
    #[cfg(target_os = "macos")]
    macos::overink_macos_order_toolbar(canvas.id.0, controller.id.0);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    _native.raise_controller(controller);
}

#[cfg(target_os = "windows")]
type WinEventHook = windows::core::Owned<windows::Win32::UI::Accessibility::HWINEVENTHOOK>;

#[cfg(target_os = "windows")]
static OVERLAY_ORDER_EVENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(target_os = "windows")]
pub struct OverlayOrderMonitor {
    _hooks: [Option<WinEventHook>; 2],
    event: u32,
    _main_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(target_os = "windows")]
impl OverlayOrderMonitor {
    pub fn start() -> Self {
        let event = register_user_event();
        let mut hooks = [None, None];
        if event != 0 {
            OVERLAY_ORDER_EVENT.store(event, std::sync::atomic::Ordering::Relaxed);
            let _ = win32_overlay(Win32Overlay::Watch(&mut hooks));
        }
        Self {
            _hooks: hooks,
            event,
            _main_thread: std::marker::PhantomData,
        }
    }

    pub fn changed(&self, ty: SDL_EventType) -> bool {
        self.event != 0 && ty.0 == self.event
    }
}

#[cfg(target_os = "windows")]
impl Drop for OverlayOrderMonitor {
    fn drop(&mut self) {
        OVERLAY_ORDER_EVENT.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(target_os = "windows")]
fn notify_overlay_order_changed() {
    let event = OVERLAY_ORDER_EVENT.load(std::sync::atomic::Ordering::Relaxed);
    if event != 0 {
        push_user_event(event);
    }
}

#[cfg(target_os = "windows")]
extern "system" fn win32_window_event(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    window: windows::Win32::Foundation::HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    use windows::Win32::UI::WindowsAndMessaging::{
        CHILDID_SELF, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW,
    };
    if window.is_invalid() || object != OBJID_WINDOW.0 || child != CHILDID_SELF as i32 {
        return;
    }
    // Only the SDL event loop changes window order. The native callback
    // checks whether a newly shown window can cover the overlay.
    match event {
        EVENT_SYSTEM_FOREGROUND => notify_overlay_order_changed(),
        EVENT_OBJECT_SHOW => {
            let _ = win32_overlay(Win32Overlay::WindowShown(window));
        }
        _ => {}
    }
}

#[cfg(target_os = "windows")]
enum Win32Overlay<'a> {
    Raise(&'a Window),
    Watch(&'a mut [Option<WinEventHook>; 2]),
    WindowShown(windows::Win32::Foundation::HWND),
}

#[cfg(target_os = "windows")]
pub(crate) fn raise_win32_overlay(window: &Window) -> Result<(), String> {
    win32_overlay(Win32Overlay::Raise(window))
}

#[cfg(target_os = "windows")]
#[deny(clippy::undocumented_unsafe_blocks)]
fn win32_overlay(command: Win32Overlay<'_>) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow},
        UI::{
            Accessibility::SetWinEventHook,
            WindowsAndMessaging::{
                EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, GA_ROOT, GWL_EXSTYLE, GetAncestor,
                GetWindowLongW, GetWindowRect, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
                SWP_NOOWNERZORDER, SWP_NOSIZE, SetWindowPos, WINEVENT_OUTOFCONTEXT,
                WINEVENT_SKIPOWNPROCESS, WS_EX_TOPMOST,
            },
        },
    };
    // SAFETY: Raise borrows a live SDL-owned HWND. Watch uses a static
    // out-of-context callback and unhooks on the installing thread.
    // WindowShown queries a WinEvent HWND through checked Win32 APIs; both
    // output structures are initialized and remain live for the queries.
    unsafe {
        match command {
            Win32Overlay::Raise(window) => {
                let handle = window.win32_hwnd().ok_or("No Win32 window handle")?;
                SetWindowPos(
                    HWND(handle.as_non_null().as_ptr()),
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )
                .map_err(|error| error.to_string())
            }
            Win32Overlay::Watch(owners) => {
                for (owner, event) in owners
                    .iter_mut()
                    .zip([EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_SHOW])
                {
                    let hook = SetWinEventHook(
                        event,
                        event,
                        None,
                        Some(win32_window_event),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    );
                    if !hook.is_invalid() {
                        *owner = Some(windows::core::Owned::new(hook));
                    }
                }
                Ok(())
            }
            Win32Overlay::WindowShown(window) => {
                // Ignore child objects, ordinary dialogs and floating tools.
                // A topmost slide window can appear without taking foreground.
                if GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 == 0
                    || GetAncestor(window, GA_ROOT) != window
                {
                    return Ok(());
                }
                let display = MonitorFromWindow(window, MONITOR_DEFAULTTONULL);
                let mut monitor = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                let mut bounds = RECT::default();
                if !display.is_invalid()
                    && GetMonitorInfoW(display, &mut monitor).as_bool()
                    && GetWindowRect(window, &mut bounds).is_ok()
                    && bounds.left <= monitor.rcMonitor.left
                    && bounds.top <= monitor.rcMonitor.top
                    && bounds.right >= monitor.rcMonitor.right
                    && bounds.bottom >= monitor.rcMonitor.bottom
                {
                    notify_overlay_order_changed();
                }
                Ok(())
            }
        }
    }
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
    use super::{checked_native_rect_size, passthrough_style};

    #[test]
    fn switching_back_to_ink_restores_the_original_win32_layered_style() {
        let transparent = 0x20;
        let layered = 0x80000;
        let topmost = 0x8;
        for original in [topmost, topmost | layered] {
            let (pass, added) = passthrough_style(original, true, false, transparent, layered);
            assert_eq!(pass, topmost | transparent | layered);
            let (retry, added) = passthrough_style(pass, true, added, transparent, layered);
            assert_eq!(retry, pass);
            assert_eq!(
                passthrough_style(retry, false, added, transparent, layered),
                (original, false)
            );
        }
    }

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
