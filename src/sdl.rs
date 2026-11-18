//! SDL is owned by the main thread. Window/renderer pointers never cross threads.
pub mod dialog;
mod dialog_native;
pub use sdl3_sys::{
    blendmode::*, clipboard::*, error::*, events::*, hints::*, init::*, keyboard::*, keycode::*,
    locale::*, mouse::*, pen::*, pixels::*, properties::*, rect::*, render::*, stdinc::*,
    surface::*, timer::*, touch::*, video::*,
};
use std::{
    ffi::{CStr, CString, c_void},
    ptr::{self, NonNull},
};

/// A native buffer returned by an SDL API that explicitly transfers
/// ownership to the caller via SDL_free. Borrowed SDL strings must not
/// be wrapped by this type.
struct SdlAllocation<T>(NonNull<T>);

impl<T> SdlAllocation<T> {
    /// # Safety
    /// `raw` must be null or a uniquely owned allocation returned by an SDL
    /// API that explicitly requires the caller to release it with SDL_free.
    unsafe fn from_native(raw: *mut T) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T> Drop for SdlAllocation<T> {
    fn drop(&mut self) {
        // SAFETY: Only caller-owned SDL allocations are wrapped; SDL_free is
        // their matching allocator, and each allocation has a single owner.
        unsafe { SDL_free(self.as_ptr().cast()) };
    }
}

pub fn error() -> String {
    unsafe {
        let p = SDL_GetError();
        if p.is_null() || CStr::from_ptr(p).to_bytes().is_empty() {
            "SDL did not report an error message".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
pub fn check(value: bool) -> Result<(), String> {
    if value { Ok(()) } else { Err(error()) }
}
pub fn event_type(event: &SDL_Event) -> SDL_EventType {
    // Every SDL event record starts with its type tag.
    unsafe { SDL_EventType(event.r#type) }
}
pub fn cstring(v: &str) -> CString {
    CString::new(v.replace('\0', "")).unwrap()
}
pub fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        || ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}
pub struct Runtime;
impl Runtime {
    pub fn new(x11: bool, wayland: bool, smoke: bool) -> Result<Self, String> {
        let wayland = wayland
            && cfg!(any(target_os = "linux", target_os = "freebsd"))
            && is_wayland_session();
        let backend = startup_driver(x11, smoke, wayland);
        unsafe {
            if let Some(name) = backend {
                SDL_ClearError();
                check(SDL_SetHintWithPriority(
                    SDL_HINT_VIDEO_DRIVER,
                    name.as_ptr(),
                    SDL_HINT_OVERRIDE,
                ))
                .map_err(|error| {
                    format!(
                        "Selecting {} video failed ({error})",
                        name.to_string_lossy()
                    )
                })?;
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
            SDL_SetHint(SDL_HINT_IME_IMPLEMENTED_UI, c"composition".as_ptr());
            let version = cstring(crate::VERSION);
            SDL_SetAppMetadata(
                c"OverInk".as_ptr(),
                version.as_ptr(),
                c"org.overink.OverInk".as_ptr(),
            );
            SDL_ClearError();
            check(SDL_Init(SDL_INIT_VIDEO)).map_err(|error| {
                let name = backend.map_or_else(|| "SDL".into(), CStr::to_string_lossy);
                format!("Initializing {name} video failed ({error})")
            })?;
            Ok(Self)
        }
    }
}

fn startup_driver(x11: bool, smoke: bool, wayland: bool) -> Option<&'static CStr> {
    if smoke {
        Some(c"dummy")
    } else if x11 {
        Some(c"x11")
    } else if wayland {
        Some(c"wayland")
    } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        Some(c"x11")
    } else {
        None
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
/// Return the SDL Wayland display scaling hint without raw FFI at call sites.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn wayland_scale_to_display_forced() -> bool {
    unsafe { SDL_GetHintBoolean(SDL_HINT_VIDEO_WAYLAND_SCALE_TO_DISPLAY, false) }
}

/// Return usable display bounds only when SDL provides them.
pub fn usable_display_bounds(id: u32) -> Option<[i32; 4]> {
    unsafe {
        let mut bounds = SDL_Rect::default();
        SDL_GetDisplayUsableBounds(SDL_DisplayID(id), &mut bounds)
            .then_some([bounds.x, bounds.y, bounds.w, bounds.h])
    }
}

/// Read the native mode's logical dimensions and pixel density.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn desktop_display_mode(id: u32) -> Option<([i32; 2], f32)> {
    unsafe {
        let mode = SDL_GetDesktopDisplayMode(SDL_DisplayID(id)).as_ref()?;
        Some(([mode.w, mode.h], mode.pixel_density))
    }
}

// Consolidate the checked SDL_Event union reads in one audited boundary.
macro_rules! tagged_event {
    ($event:expr, $field:ident, $($tag:pat_param)|+) => {{
        let event = $event;
        if matches!(event_type(event), $($tag)|+) {
            // SAFETY: The field is read only for SDL event tags that own it.
            Some(unsafe { event.$field })
        } else {
            None
        }
    }};
}

/// Read an SDL finger event only for a matching SDL event type.
pub fn finger_event(event: &SDL_Event) -> Option<SDL_TouchFingerEvent> {
    tagged_event!(
        event,
        tfinger,
        SDL_EVENT_FINGER_DOWN
            | SDL_EVENT_FINGER_MOTION
            | SDL_EVENT_FINGER_UP
            | SDL_EVENT_FINGER_CANCELED
    )
}

/// Returns the owning SDL window for an event after checking its union tag.
pub fn event_window(event: &SDL_Event, ty: SDL_EventType) -> Option<SDL_WindowID> {
    // Never use a caller-supplied tag to read a different SDL union member.
    if event_type(event) != ty {
        return None;
    }
    unsafe {
        let window = match ty {
            SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => event.button.windowID,
            SDL_EVENT_MOUSE_MOTION => event.motion.windowID,
            SDL_EVENT_MOUSE_WHEEL => event.wheel.windowID,
            SDL_EVENT_FINGER_DOWN
            | SDL_EVENT_FINGER_MOTION
            | SDL_EVENT_FINGER_UP
            | SDL_EVENT_FINGER_CANCELED => event.tfinger.windowID,
            SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_PROXIMITY_OUT => event.pproximity.windowID,
            SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => event.ptouch.windowID,
            SDL_EVENT_PEN_MOTION => event.pmotion.windowID,
            SDL_EVENT_PEN_AXIS => event.paxis.windowID,
            SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP => event.key.windowID,
            SDL_EVENT_WINDOW_FOCUS_GAINED
            | SDL_EVENT_WINDOW_FOCUS_LOST
            | SDL_EVENT_WINDOW_MOUSE_LEAVE => event.window.windowID,
            SDL_EVENT_TEXT_INPUT => event.text.windowID,
            SDL_EVENT_TEXT_EDITING => event.edit.windowID,
            _ => return None,
        };
        (window.0 != 0).then_some(window)
    }
}

/// Query indirect touch device capability via SDL's native input API.
pub fn indirect_touch_device(device: SDL_TouchID) -> bool {
    unsafe {
        SDL_WasInit(SDL_INIT_VIDEO) != 0
            && matches!(
                SDL_GetTouchDeviceType(device),
                SDL_TOUCH_DEVICE_INDIRECT_ABSOLUTE | SDL_TOUCH_DEVICE_INDIRECT_RELATIVE
            )
    }
}

/// Sleep with SDL's precise delay without exposing raw native calls to users.
pub fn delay_precise(nanoseconds: u64) {
    unsafe { SDL_DelayPrecise(nanoseconds) }
}

/// Enumerate SDL preferred locales and release SDL-owned storage before return.
pub fn preferred_locales() -> Vec<(String, String)> {
    unsafe {
        let mut count = 0;
        let Some(locales) = SdlAllocation::from_native(SDL_GetPreferredLocales(&mut count)) else {
            return Vec::new();
        };
        let name = |ptr: *const std::ffi::c_char| {
            if ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(ptr).to_str().unwrap_or_default().to_owned()
            }
        };
        (0..count.max(0) as usize)
            .filter_map(|i| (*locales.as_ptr().add(i)).as_ref())
            .map(|locale| (name(locale.language), name(locale.country)))
            .collect()
    }
}

/// Query the operating system's theme through SDL.
pub fn system_theme() -> SDL_SystemTheme {
    unsafe { SDL_GetSystemTheme() }
}

/// Reserve a single SDL user event type, returning zero on failure.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn register_user_event() -> u32 {
    unsafe { SDL_RegisterEvents(1) }
}

/// Push a copied SDL event into its queue.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn push_event(event: &mut SDL_Event) -> bool {
    unsafe { SDL_PushEvent(event) }
}

/// Owned display metadata, copied from SDL before its native allocation is freed.
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub bounds: [i32; 4],
    pub refresh: f64,
    pub scale: f32,
}

pub fn display_info() -> Result<Vec<DisplayInfo>, String> {
    unsafe {
        let mut count = 0;
        let list = SdlAllocation::from_native(SDL_GetDisplays(&mut count)).ok_or_else(error)?;
        (0..count.max(0) as usize)
            .map(|i| {
                let id = *list.as_ptr().add(i);
                let mut bounds = SDL_Rect::default();
                if !SDL_GetDisplayBounds(id, &mut bounds) {
                    return Err(error());
                }
                let name = SDL_GetDisplayName(id);
                let mode = SDL_GetCurrentDisplayMode(id);
                let refresh = if mode.is_null() {
                    0.0
                } else if (*mode).refresh_rate_numerator > 0 && (*mode).refresh_rate_denominator > 0
                {
                    (*mode).refresh_rate_numerator as f64 / (*mode).refresh_rate_denominator as f64
                } else {
                    (*mode).refresh_rate as f64
                };
                Ok(DisplayInfo {
                    id: id.0,
                    name: if name.is_null() {
                        format!("Display {}", i + 1)
                    } else {
                        CStr::from_ptr(name).to_string_lossy().into_owned()
                    },
                    bounds: [bounds.x, bounds.y, bounds.w, bounds.h],
                    refresh,
                    scale: SDL_GetDisplayContentScale(id),
                })
            })
            .collect::<Result<Vec<_>, String>>()
    }
}

/// SDL event union accessors: check the tag before reading the corresponding
/// field. Do not use `then_some(unsafe { ... })`: it evaluates eagerly.
/// Read the wheel payload only for a mouse-wheel event.
pub fn mouse_wheel_event(event: &SDL_Event) -> Option<SDL_MouseWheelEvent> {
    tagged_event!(event, wheel, SDL_EVENT_MOUSE_WHEEL)
}

/// Read keyboard modifiers without exposing the native SDL function.
pub fn mod_state() -> SDL_Keymod {
    unsafe { SDL_GetModState() }
}

/// Copy SDL-owned clipboard text and release its allocation.
/// Copy text to the native clipboard using SDL.
pub fn set_clipboard_text(value: &str) {
    let text = cstring(value);
    unsafe {
        SDL_SetClipboardText(text.as_ptr());
    }
}

pub fn clipboard_text() -> Option<String> {
    unsafe {
        let text = SdlAllocation::from_native(SDL_GetClipboardText())?;
        Some(CStr::from_ptr(text.as_ptr()).to_string_lossy().into_owned())
    }
}

/// Return text input while the SDL event's native text pointer remains valid.
pub fn text_input_data(event: &SDL_Event, window: SDL_WindowID) -> Option<(String, u64)> {
    if event_type(event) != SDL_EVENT_TEXT_INPUT {
        return None;
    }
    unsafe {
        let text = event.text;
        if text.windowID != window || text.text.is_null() {
            return None;
        }
        Some((
            CStr::from_ptr(text.text).to_string_lossy().into_owned(),
            text.timestamp,
        ))
    }
}

/// Owned IME composition update; event-owned pointers never escape this API.
pub struct TextEditingData {
    pub timestamp: u64,
    pub text: String,
    pub start: i32,
    pub length: i32,
}

pub fn text_editing_data(event: &SDL_Event, window: SDL_WindowID) -> Option<TextEditingData> {
    if event_type(event) != SDL_EVENT_TEXT_EDITING {
        return None;
    }
    unsafe {
        let edit = event.edit;
        if edit.windowID != window {
            return None;
        }
        let text = if edit.text.is_null() {
            String::new()
        } else {
            CStr::from_ptr(edit.text).to_string_lossy().into_owned()
        };
        Some(TextEditingData {
            timestamp: edit.timestamp,
            text,
            start: edit.start,
            length: edit.length,
        })
    }
}

pub fn mouse_button_event(event: &SDL_Event) -> Option<SDL_MouseButtonEvent> {
    tagged_event!(
        event,
        button,
        SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP
    )
}

pub fn mouse_motion_event(event: &SDL_Event) -> Option<SDL_MouseMotionEvent> {
    tagged_event!(event, motion, SDL_EVENT_MOUSE_MOTION)
}

pub fn pen_touch_event(event: &SDL_Event) -> Option<SDL_PenTouchEvent> {
    tagged_event!(event, ptouch, SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP)
}

pub fn pen_axis_event(event: &SDL_Event) -> Option<SDL_PenAxisEvent> {
    tagged_event!(event, paxis, SDL_EVENT_PEN_AXIS)
}

pub fn pen_motion_event(event: &SDL_Event) -> Option<SDL_PenMotionEvent> {
    tagged_event!(event, pmotion, SDL_EVENT_PEN_MOTION)
}

pub fn pen_proximity_event(event: &SDL_Event) -> Option<SDL_PenProximityEvent> {
    tagged_event!(
        event,
        pproximity,
        SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_PROXIMITY_OUT
    )
}

pub fn key_event(event: &SDL_Event) -> Option<SDL_KeyboardEvent> {
    tagged_event!(event, key, SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP)
}

pub fn capture_mouse(enable: bool) -> bool {
    unsafe { SDL_CaptureMouse(enable) }
}

/// Keep the cursor allocation owned and restore the default on drop.
pub struct SystemCursor(*mut SDL_Cursor);

impl SystemCursor {
    pub fn crosshair() -> Option<Self> {
        unsafe {
            if SDL_WasInit(SDL_INIT_VIDEO) == 0 {
                return None;
            }
            let cursor = SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_CROSSHAIR);
            if cursor.is_null() {
                return None;
            }
            SDL_SetCursor(cursor);
            Some(Self(cursor))
        }
    }
}

impl Drop for SystemCursor {
    fn drop(&mut self) {
        unsafe {
            SDL_SetCursor(SDL_GetDefaultCursor());
            SDL_DestroyCursor(self.0);
        }
    }
}

/// Local timestamp for screenshot filenames, using SDL's platform clock.
pub fn screenshot_file_stem() -> String {
    use sdl3_sys::time::{SDL_DateTime, SDL_GetCurrentTime, SDL_TimeToDateTime};
    let mut time = 0;
    let mut date = SDL_DateTime::default();
    if unsafe { SDL_GetCurrentTime(&mut time) && SDL_TimeToDateTime(time, &mut date, true) } {
        format!(
            "OverInk-{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            date.year, date.month, date.day, date.hour, date.minute, date.second
        )
    } else {
        "OverInk".into()
    }
}

/// Take the next queued SDL event on the SDL-owning thread.
pub fn poll_event(event: &mut SDL_Event) -> bool {
    unsafe { SDL_PollEvent(event) }
}

/// Remove the leftover SDL quit event when dismissing a temporary guide.
#[cfg(target_os = "linux")]
pub fn flush_quit_event() {
    unsafe { SDL_FlushEvent(SDL_EVENT_QUIT.0) };
}

/// Read an SDL display-removal event only after checking its union tag.
pub fn removed_display_id(event: &SDL_Event) -> Option<u32> {
    tagged_event!(event, display, SDL_EVENT_DISPLAY_REMOVED).map(|display| display.displayID.0)
}

/// Read a window event's owning ID only for a tagged window event.
pub fn window_event_id(event: &SDL_Event) -> Option<SDL_WindowID> {
    if (SDL_EVENT_WINDOW_FIRST..=SDL_EVENT_WINDOW_LAST).contains(&event_type(event)) {
        Some(unsafe { event.window.windowID })
    } else {
        None
    }
}

/// A reset may apply to an individual window, or all windows (ID zero).
pub fn render_reset_window_id(event: &SDL_Event) -> Option<SDL_WindowID> {
    tagged_event!(
        event,
        render,
        SDL_EVENT_RENDER_TARGETS_RESET | SDL_EVENT_RENDER_DEVICE_RESET
    )
    .map(|render| render.windowID)
}

/// Check physical buttons on macOS, and SDL's per-window state elsewhere.
pub fn mouse_buttons_released() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { SDL_GetGlobalMouseState(ptr::null_mut(), ptr::null_mut()).0 == 0 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { SDL_GetMouseState(ptr::null_mut(), ptr::null_mut()).0 == 0 }
    }
}

/// Wait without dequeuing the SDL event, preserving the polling loop's order.
pub fn wait_for_event(timeout_ms: i32) {
    unsafe {
        SDL_WaitEventTimeout(ptr::null_mut(), timeout_ms);
    }
}

pub struct Window {
    pub raw: *mut SDL_Window,
    pub id: SDL_WindowID,
}
impl Window {
    /// Set a popup's size and origin, preserving SDL's native error reporting.
    pub fn text_input_active(&self) -> bool {
        unsafe { SDL_TextInputActive(self.raw) }
    }

    pub fn set_text_input_area(&self, area: &SDL_Rect) {
        unsafe {
            SDL_SetTextInputArea(self.raw, area, 0);
        }
    }

    pub fn sync_text_input(&self, enabled: bool) {
        unsafe {
            if enabled != SDL_TextInputActive(self.raw) {
                if enabled {
                    SDL_StartTextInput(self.raw);
                } else {
                    SDL_StopTextInput(self.raw);
                }
            }
        }
    }

    pub fn clear_composition(&self) {
        unsafe { SDL_ClearComposition(self.raw) };
    }

    pub fn sync_and_pump_events(&self) {
        unsafe {
            SDL_SyncWindow(self.raw);
            SDL_PumpEvents();
        }
    }

    pub fn sdr_white_level(&self) -> f32 {
        unsafe {
            SDL_GetFloatProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_SDR_WHITE_LEVEL_FLOAT,
                1.,
            )
        }
    }

    #[cfg(target_os = "linux")]
    pub fn set_size(&self, width: i32, height: i32) -> Result<(), String> {
        unsafe { check(SDL_SetWindowSize(self.raw, width, height)) }
    }

    pub fn set_geometry(&self, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
        unsafe {
            check(SDL_SetWindowSize(self.raw, w, h))?;
            check(SDL_SetWindowPosition(self.raw, x, y))
        }
    }

    pub fn set_always_on_top(&self, enabled: bool) {
        unsafe {
            let _ = SDL_SetWindowAlwaysOnTop(self.raw, enabled);
        }
    }

    pub fn raise(&self) {
        unsafe {
            let window = SDL_GetWindowFromID(self.id);
            if !window.is_null() && window == self.raw {
                SDL_RaiseWindow(window);
            }
        }
    }

    pub fn input_focused(&self) -> bool {
        self.flags() & SDL_WINDOW_INPUT_FOCUS != 0
    }

    pub fn hidden(&self) -> bool {
        self.flags() & SDL_WINDOW_HIDDEN != 0
    }

    /// Read the actual SDL window transparency flag.
    pub fn transparent(&self) -> bool {
        self.flags() & SDL_WINDOW_TRANSPARENT != 0
    }

    /// Apply an RGBA shape mask, or clear it when no regions are supplied.
    /// Own and free the temporary surface even if an SDL operation fails.
    #[cfg(not(target_os = "windows"))]
    pub fn set_opaque_shape_regions(
        &self,
        size: [i32; 2],
        opaque_regions: Option<&[SDL_Rect]>,
    ) -> Result<(), String> {
        unsafe {
            let Some(regions) = opaque_regions else {
                return check(SDL_SetWindowShape(self.raw, ptr::null_mut()));
            };
            let surface = OwnedSurface::from_native(SDL_CreateSurface(
                size[0],
                size[1],
                SDL_PIXELFORMAT_RGBA32,
            ))
            .ok_or_else(error)?;
            let opaque = SDL_MapSurfaceRGBA(surface.as_ptr(), 255, 255, 255, 255);
            let transparent = SDL_MapSurfaceRGBA(surface.as_ptr(), 0, 0, 0, 0);
            check(SDL_FillSurfaceRect(
                surface.as_ptr(),
                ptr::null(),
                transparent,
            ))?;
            for region in regions {
                check(SDL_FillSurfaceRect(surface.as_ptr(), region, opaque))?;
            }
            check(SDL_SetWindowShape(self.raw, surface.as_ptr()))
        }
    }

    fn flags(&self) -> SDL_WindowFlags {
        unsafe { SDL_GetWindowFlags(self.raw) }
    }

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
            SDL_ClearError();
            let raw = SDL_CreateWindow(title.as_ptr(), w, h, flags);
            if raw.is_null() {
                let detail = error();
                return Err(format!(
                    "Creating {} on {} failed ({detail})",
                    title.to_string_lossy(),
                    driver()
                ));
            }
            let id = SDL_GetWindowID(raw);
            if id.0 == 0 {
                let message = error();
                SDL_DestroyWindow(raw);
                return Err(format!("SDL did not assign a window ID: {message}"));
            }
            Ok(Self { raw, id })
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
            let id = SDL_GetWindowID(raw);
            if id.0 == 0 {
                let message = error();
                SDL_DestroyWindow(raw);
                return Err(format!("SDL did not assign a window ID: {message}"));
            }
            Ok(Self { raw, id })
        }
    }
    /// Unclamped dimensions for converting local input into screen pixels.
    /// Render sizes may be clamped to one; input picking must reject zero.
    pub fn capture_coordinate_sizes(&self) -> Option<([f32; 2], [i32; 2])> {
        let (mut w, mut h, mut px, mut py) = (0, 0, 0, 0);
        // SAFETY: SDL owns this live window, and the four integer outputs
        // remain writable for these synchronous FFI calls.
        let valid = unsafe {
            SDL_GetWindowSize(self.raw, &mut w, &mut h)
                && SDL_GetWindowSizeInPixels(self.raw, &mut px, &mut py)
        };
        (valid && w > 0 && h > 0 && px > 0 && py > 0).then_some(([w as f32, h as f32], [px, py]))
    }

    pub fn size(&self) -> [f32; 2] {
        unsafe {
            let (mut w, mut h) = (0, 0);
            SDL_GetWindowSize(self.raw, &mut w, &mut h);
            [w.max(1) as f32, h.max(1) as f32]
        }
    }
    pub fn setup_toolbar(&self) {
        extern "C" fn toolbar_hit_test(
            window: *mut SDL_Window,
            point: *const SDL_Point,
            _: *mut c_void,
        ) -> SDL_HitTestResult {
            if window.is_null() {
                return SDL_HITTEST_NORMAL;
            }
            // SDL supplies the live window and a point borrowed for this hit test.
            let Some(point) = (unsafe { point.as_ref() }) else {
                return SDL_HITTEST_NORMAL;
            };
            let scale = unsafe {
                SDL_GetWindowDisplayScale(window) / SDL_GetWindowPixelDensity(window).max(0.1)
            };
            if point.x >= 0
                && (point.x as f32) < crate::toolbar::WIDTH * scale
                && point.y >= 0
                && (point.y as f32) < crate::toolbar::GRIP_HEIGHT * scale
            {
                SDL_HITTEST_DRAGGABLE
            } else {
                SDL_HITTEST_NORMAL
            }
        }

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
            if let Some(surface) = OwnedSurface::from_native(surface) {
                SDL_SetWindowIcon(self.raw, surface.as_ptr());
            }
        }
    }
    pub fn arrange_toolbar(&self, expanded: bool, on_display: Option<u32>) -> Result<(), String> {
        unsafe {
            let units = self.ui_scale();
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
                let (mut current_x, mut current_y) = (0, 0);
                let positioned = SDL_GetWindowPosition(self.raw, &mut current_x, &mut current_y);
                let (x, y) = if on_display.is_some() || !positioned {
                    (
                        bounds.x + if driver == "x11" { 0 } else { 8 },
                        bounds.y + (bounds.h - h) / 2,
                    )
                } else {
                    (current_x, current_y)
                };
                let x = x.clamp(bounds.x, bounds.x + (bounds.w - w).max(0));
                let y = y.clamp(bounds.y, bounds.y + (bounds.h - h).max(0));
                if !positioned || (x, y) != (current_x, current_y) {
                    check(SDL_SetWindowPosition(self.raw, x, y))?;
                }
                // X11 reports the actual geometry through ConfigureNotify.
                if driver != "x11" {
                    check(SDL_SyncWindow(self.raw))?;
                }
            }
            Ok(())
        }
    }
    /// Query the current X11/desktop position without exposing an unsafe FFI call.
    /// Maintain an SDL Wayland transient parent only when it actually changes.
    pub fn set_parent(&self, parent: Option<&Window>) -> Result<(), String> {
        unsafe {
            let raw = parent.map_or(ptr::null_mut(), |window| window.raw);
            if SDL_GetWindowParent(self.raw) == raw {
                return Ok(());
            }
            check(SDL_SetWindowParent(self.raw, raw))
        }
    }

    /// SDL's border query can fail on window managers without server-side
    /// decoration. Preserve the undecorated default in that case.
    #[cfg(not(target_os = "windows"))]
    pub fn border_size(&self) -> [i32; 4] {
        unsafe {
            let (mut top, mut left, mut bottom, mut right) = (0, 0, 0, 0);
            SDL_GetWindowBordersSize(self.raw, &mut top, &mut left, &mut bottom, &mut right);
            [top, left, bottom, right]
        }
    }

    pub fn position(&self) -> Result<[i32; 2], String> {
        unsafe {
            let (mut x, mut y) = (0, 0);
            check(SDL_GetWindowPosition(self.raw, &mut x, &mut y))?;
            Ok([x, y])
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
    pub fn pixel_density(&self) -> f32 {
        let density = unsafe { SDL_GetWindowPixelDensity(self.raw) };
        if density.is_finite() && density > 0. {
            density
        } else {
            1.
        }
    }
    pub fn ui_scale(&self) -> f32 {
        self.scale() / self.pixel_density()
    }
    pub fn show(&self, show: bool) -> Result<(), String> {
        unsafe {
            SDL_ClearError();
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
    /// Copy SDL's optional ICC profile into Rust-owned memory and release
    /// SDL's native allocation before returning it to the color-management code.
    /// Borrow the Win32 HWND backing this live SDL window.
    #[cfg(target_os = "windows")]
    pub fn win32_hwnd(&self) -> Option<std::ptr::NonNull<c_void>> {
        // SAFETY: SDL owns this live window and exposes its native handle via
        // the window property list. The caller must not outlive `self`.
        unsafe {
            std::ptr::NonNull::new(SDL_GetPointerProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_WIN32_HWND_POINTER,
                ptr::null_mut(),
            ))
        }
    }

    /// Borrow the Cocoa NSWindow for this live SDL window without exposing
    /// property-list FFI to the platform input-routing code.
    #[cfg(target_os = "macos")]
    pub fn cocoa_window(&self) -> Option<std::ptr::NonNull<c_void>> {
        // SAFETY: SDL's Cocoa window property is borrowed; the caller is
        // required to keep this SDL Window alive while using the NSWindow.
        unsafe {
            std::ptr::NonNull::new(SDL_GetPointerProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
                ptr::null_mut(),
            ))
        }
    }

    /// Read the Cocoa Metal view tag without exposing the SDL property handle.
    #[cfg(target_os = "macos")]
    pub fn cocoa_metal_view_tag(&self) -> Option<isize> {
        // SAFETY: This numeric SDL window property is read-only and its value
        // does not transfer any native object ownership.
        let tag = unsafe {
            SDL_GetNumberProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_COCOA_METAL_VIEW_TAG_NUMBER,
                0,
            )
        };
        (tag != 0)
            .then_some(tag)
            .and_then(|tag| tag.try_into().ok())
    }

    /// Borrow the native X11 Display for the lifetime of the SDL window.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn x11_display(&self) -> Option<std::ptr::NonNull<c_void>> {
        // SAFETY: The X11 display pointer is held by SDL's video driver and
        // stays valid while the corresponding SDL window remains live.
        unsafe {
            std::ptr::NonNull::new(SDL_GetPointerProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_X11_DISPLAY_POINTER,
                ptr::null_mut(),
            ))
        }
    }

    /// Borrow the Wayland wl_surface from SDL for the lifetime of this window.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn wayland_surface(&self) -> Option<std::ptr::NonNull<c_void>> {
        // SAFETY: The pointer is owned by the active SDL Wayland backend and
        // may only be used while the corresponding SDL Window remains live.
        unsafe {
            std::ptr::NonNull::new(SDL_GetPointerProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER,
                ptr::null_mut(),
            ))
        }
    }

    /// Return the X11 drawable ID associated with this window.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn x11_window_number(&self) -> std::ffi::c_ulong {
        // SAFETY: Reading a number property does not transfer ownership.
        unsafe {
            SDL_GetNumberProperty(
                SDL_GetWindowProperties(self.raw),
                SDL_PROP_WINDOW_X11_WINDOW_NUMBER,
                0,
            ) as std::ffi::c_ulong
        }
    }

    pub fn icc_profile(&self) -> Option<Vec<u8>> {
        unsafe {
            let mut size = 0;
            let Some(profile) =
                SdlAllocation::from_native(SDL_GetWindowICCProfile(self.raw, &mut size))
            else {
                // A missing ICC profile is normal; avoid leaving a stale SDL error.
                SDL_ClearError();
                return None;
            };
            if size > 0 && size <= 16 * 1024 * 1024 {
                Some(std::slice::from_raw_parts(profile.as_ptr().cast::<u8>(), size).to_vec())
            } else {
                None
            }
        }
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
            let video = driver();
            let display = SDL_DisplayID(id);
            let mut r = SDL_Rect::default();
            check(SDL_GetDisplayBounds(display, &mut r))?;
            if overlay && video == "wayland" {
                // XDG fullscreen hides unrelated content behind transparent
                // surfaces. Maximizing keeps the desktop in the compositor.
                check(SDL_MaximizeWindow(self.raw))?;
            } else if overlay {
                check(SDL_SetWindowSize(self.raw, r.w, r.h))?;
                check(SDL_SetWindowPosition(self.raw, r.x, r.y))?;
                // Give the resize time to settle before allocating a software
                // framebuffer. X11 can time out before the window manager's
                // configure event. SDL will still report the later resize.
                SDL_ClearError();
                let synced = SDL_SyncWindow(self.raw);
                if video != "x11" {
                    check(synced)?;
                }
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
        // Destroying an SDL parent also destroys its popup descendants.
        // Never dereference a popup whose parent has already been dropped.
        unsafe {
            if !self.raw.is_null() && SDL_GetWindowFromID(self.id) == self.raw {
                SDL_DestroyWindow(self.raw);
            }
        }
    }
}
/// Release an SDL-owned surface at one native FFI boundary.
fn destroy_owned_surface(raw: *mut SDL_Surface) {
    if raw.is_null() {
        return;
    }
    // SAFETY: Each caller owns the SDL_Surface from a create/readback API,
    // and transfers it exactly once while SDL is active on the main thread.
    unsafe { SDL_DestroySurface(raw) };
}

/// Temporary SDL surface whose native allocation is released even when an
/// operation returns early or unwinds.
struct OwnedSurface(*mut SDL_Surface);

impl OwnedSurface {
    /// # Safety
    /// `raw` must be null or a uniquely owned SDL_Surface allocation.
    /// Borrowed surfaces and pointers already owned elsewhere are forbidden.
    unsafe fn from_native(raw: *mut SDL_Surface) -> Option<Self> {
        (!raw.is_null()).then(|| Self(raw))
    }

    fn as_ptr(&self) -> *mut SDL_Surface {
        self.0
    }
}

impl Drop for OwnedSurface {
    fn drop(&mut self) {
        destroy_owned_surface(self.0);
    }
}

/// Consume one uniquely owned SDL texture on the SDL-owning thread.
/// Ink, snapshot and UI textures share this native destruction boundary.
fn destroy_owned_texture(raw: *mut SDL_Texture) {
    if raw.is_null() {
        return;
    }
    // SAFETY: Every caller transfers the sole SDL_CreateTexture handle from
    // an owned texture's Drop. The renderer is still live and SDL is confined
    // to the main thread; no caller may use the pointer after this call.
    unsafe { SDL_DestroyTexture(raw) };
}

/// Owned SDL texture used by the ink renderer. Native calls and its
/// destructor stay in this adapter; live textures are dropped normally
/// before their renderer is recreated.
pub struct InkTexture {
    raw: *mut SDL_Texture,
    dimensions: [i32; 2],
}

impl InkTexture {
    pub fn new(
        renderer: &Renderer,
        dimensions: [i32; 2],
        access: SDL_TextureAccess,
        blend: SDL_BlendMode,
    ) -> Result<Self, String> {
        Self::with_format(renderer, dimensions, access, blend, SDL_PIXELFORMAT_RGBA32)
    }

    pub fn with_format(
        renderer: &Renderer,
        [width, height]: [i32; 2],
        access: SDL_TextureAccess,
        blend: SDL_BlendMode,
        format: SDL_PixelFormat,
    ) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err("Ink texture dimensions must be positive".into());
        }
        unsafe {
            let raw = SDL_CreateTexture(renderer.raw, format, access, width, height);
            if raw.is_null() {
                return Err(error());
            }
            let texture = Self {
                raw,
                dimensions: [width, height],
            };
            check(SDL_SetTextureBlendMode(raw, blend))?;
            check(SDL_SetTextureScaleMode(raw, SDL_SCALEMODE_NEAREST))?;
            Ok(texture)
        }
    }

    pub fn raw(&self) -> *mut SDL_Texture {
        self.raw
    }

    pub fn update_rgba8(&self, pixels: &[u8]) -> Result<(), String> {
        let [width, height] = self.dimensions;
        let pitch = usize::try_from(width)
            .ok()
            .and_then(|w| w.checked_mul(4))
            .and_then(|row| i32::try_from(row).ok())
            .ok_or("Invalid ink texture upload pitch")?;
        let expected = usize::try_from(height)
            .ok()
            .and_then(|h| h.checked_mul(pitch as usize))
            .ok_or("Invalid ink texture upload dimensions")?;
        if pixels.len() != expected {
            return Err("Ink texture pixel buffer size mismatch".into());
        }
        unsafe {
            check(SDL_UpdateTexture(
                self.raw,
                ptr::null(),
                pixels.as_ptr().cast(),
                pitch,
            ))
        }
    }

    pub fn set_modulation(&self, rgb: [f32; 3], alpha: f32) -> Result<(), String> {
        unsafe {
            check(SDL_SetTextureColorModFloat(
                self.raw, rgb[0], rgb[1], rgb[2],
            ))?;
            check(SDL_SetTextureAlphaModFloat(self.raw, alpha))
        }
    }

    pub fn draw(
        &self,
        renderer: &Renderer,
        source: Option<&SDL_FRect>,
        destination: Option<&SDL_FRect>,
    ) -> Result<(), String> {
        unsafe {
            check(SDL_RenderTexture(
                renderer.raw,
                self.raw,
                source.map_or(ptr::null(), |rect| rect),
                destination.map_or(ptr::null(), |rect| rect),
            ))
        }
    }
}

impl Drop for InkTexture {
    fn drop(&mut self) {
        destroy_owned_texture(self.raw);
    }
}

/// Owned background snapshot texture, including HDR linear-sRGB textures.
pub struct SnapshotTexture(*mut SDL_Texture);

impl SnapshotTexture {
    pub fn new(
        renderer: &Renderer,
        dimensions: [u32; 2],
        linear: bool,
        pixels: &[u8],
    ) -> Result<Self, String> {
        let [width, height] = dimensions;
        let w = i32::try_from(width).map_err(|_| "Snapshot width is too large")?;
        let h = i32::try_from(height).map_err(|_| "Snapshot height is too large")?;
        let bytes_per_pixel = if linear { 8usize } else { 4usize };
        let row_bytes = (width as usize)
            .checked_mul(bytes_per_pixel)
            .ok_or("Snapshot row is too wide")?;
        let pitch = i32::try_from(row_bytes).map_err(|_| "Snapshot row is too wide")?;
        let len = row_bytes
            .checked_mul(height as usize)
            .ok_or("Snapshot image is too large")?;
        if w <= 0 || h <= 0 || pixels.len() != len {
            return Err("Invalid snapshot image dimensions or pixel buffer".into());
        }
        if linear && !renderer.extended {
            return Err("The renderer cannot display the captured HDR range".into());
        }
        unsafe {
            let raw = if linear {
                let props = SDL_CreateProperties();
                if props.0 == 0 {
                    return Err(error());
                }
                let result = (|| {
                    check(SDL_SetNumberProperty(
                        props,
                        SDL_PROP_TEXTURE_CREATE_FORMAT_NUMBER,
                        SDL_PIXELFORMAT_RGBA64_FLOAT.0 as i64,
                    ))?;
                    check(SDL_SetNumberProperty(
                        props,
                        SDL_PROP_TEXTURE_CREATE_COLORSPACE_NUMBER,
                        SDL_COLORSPACE_SRGB_LINEAR.0 as i64,
                    ))?;
                    check(SDL_SetNumberProperty(
                        props,
                        SDL_PROP_TEXTURE_CREATE_WIDTH_NUMBER,
                        width as i64,
                    ))?;
                    check(SDL_SetNumberProperty(
                        props,
                        SDL_PROP_TEXTURE_CREATE_HEIGHT_NUMBER,
                        height as i64,
                    ))?;
                    // A display snapshot is already mapped. Do not tone-map it again.
                    check(SDL_SetFloatProperty(
                        props,
                        SDL_PROP_TEXTURE_CREATE_HDR_HEADROOM_FLOAT,
                        0.,
                    ))?;
                    Ok::<_, String>(SDL_CreateTextureWithProperties(renderer.raw, props))
                })();
                SDL_DestroyProperties(props);
                result?
            } else {
                SDL_CreateTexture(
                    renderer.raw,
                    SDL_PIXELFORMAT_RGBA32,
                    SDL_TEXTUREACCESS_STATIC,
                    w,
                    h,
                )
            };
            if raw.is_null() {
                return Err(error());
            }
            let texture = Self(raw);
            check(SDL_UpdateTexture(
                raw,
                ptr::null(),
                pixels.as_ptr().cast(),
                pitch,
            ))?;
            check(SDL_SetTextureBlendMode(raw, SDL_BLENDMODE_NONE))?;
            check(SDL_SetTextureScaleMode(raw, SDL_SCALEMODE_LINEAR))?;
            Ok(texture)
        }
    }

    pub fn draw(
        &self,
        renderer: &Renderer,
        destination: &SDL_FRect,
        linear: bool,
    ) -> Result<(), String> {
        let draw = || unsafe {
            check(SDL_RenderTexture(
                renderer.raw,
                self.0,
                ptr::null(),
                destination,
            ))
        };
        if !linear {
            return draw();
        }
        let white = renderer.sdr_white_point();
        if !white.is_finite() || white <= 0. {
            return Err("Invalid SDR reference white level".into());
        }
        // SDL applies the SDR reference white to draws of linear snapshots.
        // Cancel only that multiplier without changing the caller's color scale.
        renderer.with_absolute_color_scale(1. / white, draw)
    }
}

impl Drop for SnapshotTexture {
    fn drop(&mut self) {
        destroy_owned_texture(self.0);
    }
}

/// Owned SDL texture backing egui's font atlas and custom UI images.
pub struct GuiTexture(*mut SDL_Texture);

impl GuiTexture {
    pub fn new(renderer: &Renderer, width: usize, height: usize) -> Result<Self, String> {
        let w = i32::try_from(width).map_err(|_| "egui texture width is too large")?;
        let h = i32::try_from(height).map_err(|_| "egui texture height is too large")?;
        if w <= 0 || h <= 0 {
            return Err("egui texture dimensions must be positive".into());
        }
        unsafe {
            let raw = SDL_CreateTexture(
                renderer.raw,
                SDL_PIXELFORMAT_RGBA32,
                SDL_TEXTUREACCESS_STATIC,
                w,
                h,
            );
            if raw.is_null() {
                return Err(error());
            }
            let texture = Self(raw);
            check(SDL_SetTextureBlendMode(raw, SDL_BLENDMODE_BLEND))?;
            SDL_SetTextureScaleMode(raw, SDL_SCALEMODE_LINEAR);
            Ok(texture)
        }
    }

    pub fn update(
        &self,
        dimensions: [usize; 2],
        rgba: &[u8],
        rect: Option<&SDL_Rect>,
    ) -> Result<(), String> {
        let [width, height] = dimensions;
        let pitch = width
            .checked_mul(4)
            .and_then(|n| i32::try_from(n).ok())
            .ok_or("egui texture row is too wide")?;
        let bytes = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .ok_or("egui texture is too large")?;
        if rgba.len() != bytes {
            return Err("egui texture pixel buffer size mismatch".into());
        }
        unsafe {
            check(SDL_UpdateTexture(
                self.0,
                rect.map_or(ptr::null(), |area| area),
                rgba.as_ptr().cast(),
                pitch,
            ))
        }
    }

    pub fn geometry(
        &self,
        renderer: &Renderer,
        vertices: &[SDL_Vertex],
        indices: &[u32],
    ) -> Result<(), String> {
        renderer.geometry(self.0, vertices, indices)
    }
}

impl Drop for GuiTexture {
    fn drop(&mut self) {
        destroy_owned_texture(self.0);
    }
}

/// Run a cleanup action if a rendering callback unwinds. Normal paths
/// explicitly restore and disarm it so SDL failures can still be reported.
struct RestoreOnDrop<F: FnOnce()> {
    cleanup: Option<F>,
}

impl<F: FnOnce()> RestoreOnDrop<F> {
    fn new(cleanup: F) -> Self {
        Self {
            cleanup: Some(cleanup),
        }
    }

    fn disarm(&mut self) {
        self.cleanup = None;
    }
}

impl<F: FnOnce()> Drop for RestoreOnDrop<F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

pub struct Renderer {
    pub raw: *mut SDL_Renderer,
    pub extended: bool,
    pub name: String,
}
impl Renderer {
    /// Release this renderer once, including when macOS recreates the Metal
    /// output. Drop uses the same path to avoid accidental double-destruction.
    fn destroy_native(&mut self) {
        if self.raw.is_null() {
            return;
        }
        // SAFETY: This handle uniquely belongs to Renderer, and SDL calls
        // happen on the owning main thread. Nulling the handle prevents a
        // later Drop from releasing the same renderer again.
        unsafe { SDL_DestroyRenderer(self.raw) };
        self.raw = ptr::null_mut();
    }

    pub fn new(window: &Window, force_sdr: bool) -> Result<Self, String> {
        Self::create(window, force_sdr, None)
    }
    pub fn software(window: &Window) -> Result<Self, String> {
        Self::create(window, true, Some(c"software"))
    }
    fn create(window: &Window, force_sdr: bool, driver: Option<&CStr>) -> Result<Self, String> {
        unsafe {
            let props = SDL_CreateProperties();
            if props.0 == 0 {
                return Err(error());
            }
            if !SDL_SetPointerProperty(
                props,
                SDL_PROP_RENDERER_CREATE_WINDOW_POINTER,
                window.raw.cast(),
            ) {
                let message = error();
                SDL_DestroyProperties(props);
                return Err(message);
            }
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
                SDL_ClearError();
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
                SDL_ClearError();
                raw = SDL_CreateRendererWithProperties(props);
            }
            let failure = raw.is_null().then(error);
            SDL_DestroyProperties(props);
            if let Some(error) = failure {
                return Err(error);
            }
            let p = SDL_GetRendererProperties(raw);
            let extended = SDL_GetNumberProperty(p, SDL_PROP_RENDERER_OUTPUT_COLORSPACE_NUMBER, 0)
                == SDL_COLORSPACE_SRGB_LINEAR.0 as i64;
            let name = SDL_GetRendererName(raw);
            let name = if name.is_null() {
                "unknown".into()
            } else {
                CStr::from_ptr(name).to_string_lossy().into_owned()
            };
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
        self.destroy_native();
        // SDL keeps its view attached for reuse. A fresh view gives SDR its
        // own BGRA8 drawable pool after previously presenting FP16.
        if self.name == "metal" {
            crate::native::macos::remove_metal_view(window);
        }
        *self = Self::new(window, !extended)?;
        Ok(())
    }
    /// Write a renderer snapshot to BMP for headless smoke tests.
    /// The returned surface is always destroyed, including on write failures.
    pub fn save_bmp(&self, path: &std::path::Path) -> Result<(), String> {
        unsafe {
            let surface = OwnedSurface::from_native(SDL_RenderReadPixels(self.raw, ptr::null()))
                .ok_or_else(error)?;
            let name = cstring(&path.to_string_lossy());
            check(SDL_SaveBMP(surface.as_ptr(), name.as_ptr()))
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
            check(SDL_SetRenderTarget(self.raw, target))?;
            let mut unwind_restore = RestoreOnDrop::new(|| {
                let _ = SDL_SetRenderTarget(self.raw, previous);
            });
            let result = draw();
            let restore = check(SDL_SetRenderTarget(self.raw, previous));
            unwind_restore.disarm();
            result.and(restore)
        }
    }
    pub fn set_clip_rect(&self, clip: Option<&SDL_Rect>) {
        unsafe {
            SDL_SetRenderClipRect(self.raw, clip.map_or(ptr::null(), |rect| rect));
        }
    }
    /// Query the renderer's SDR reference white, keeping SDL properties internal.
    pub fn sdr_white_point(&self) -> f32 {
        unsafe {
            SDL_GetFloatProperty(
                SDL_GetRendererProperties(self.raw),
                SDL_PROP_RENDERER_SDR_WHITE_POINT_FLOAT,
                1.,
            )
        }
    }

    /// Clear a damaged rectangle without blending, restoring normal draw
    /// blending even if rendering the rectangle returns an error.
    pub fn clear_rect_transparent(&self, rect: &SDL_FRect) -> Result<(), String> {
        unsafe {
            check(SDL_SetRenderDrawColorFloat(self.raw, 0., 0., 0., 0.))?;
            check(SDL_SetRenderDrawBlendMode(self.raw, SDL_BLENDMODE_NONE))?;
            let draw = check(SDL_RenderFillRect(self.raw, rect));
            let restore = check(SDL_SetRenderDrawBlendMode(self.raw, SDL_BLENDMODE_BLEND));
            draw.and(restore)
        }
    }

    pub fn set_render_scale(&self, scale: [f32; 2]) -> Result<(), String> {
        unsafe { check(SDL_SetRenderScale(self.raw, scale[0], scale[1])) }
    }

    /// Set a clip region and return failures (unlike the UI's best-effort clip).
    pub fn try_set_clip_rect(&self, clip: Option<&SDL_Rect>) -> Result<(), String> {
        unsafe {
            check(SDL_SetRenderClipRect(
                self.raw,
                clip.map_or(ptr::null(), |rect| rect),
            ))
        }
    }

    /// Temporarily replace SDL's color scale and restore it after drawing.
    pub fn with_absolute_color_scale(
        &self,
        scale: f32,
        draw: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_temporary_color_scale(scale, false, draw)
    }

    pub fn with_color_scale(
        &self,
        scale: f32,
        draw: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if scale == 1. {
            return draw();
        }
        self.with_temporary_color_scale(scale, true, draw)
    }

    /// Single SDL FFI boundary for both relative and absolute color scaling;
    /// use the same read/set/restore sequence for either path.
    fn with_temporary_color_scale(
        &self,
        scale: f32,
        relative: bool,
        draw: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        // SAFETY: This renderer is live, and the SDL-owned scale output is
        // copied into initialized Rust storage. Both variants restore the
        // previous value after the drawing callback returns.
        unsafe {
            let mut previous = 1.;
            check(SDL_GetRenderColorScale(self.raw, &mut previous))?;
            let target = if relative { previous * scale } else { scale };
            check(SDL_SetRenderColorScale(self.raw, target))?;
            let mut unwind_restore = RestoreOnDrop::new(|| {
                let _ = SDL_SetRenderColorScale(self.raw, previous);
            });
            let result = draw();
            let restore = check(SDL_SetRenderColorScale(self.raw, previous));
            unwind_restore.disarm();
            result.and(restore)
        }
    }

    pub fn flush(&self) -> Result<(), String> {
        unsafe { check(SDL_FlushRenderer(self.raw)) }
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
        let vertex_count =
            i32::try_from(vertices.len()).map_err(|_| "SDL geometry vertex count exceeds i32")?;
        let index_count =
            i32::try_from(indices.len()).map_err(|_| "SDL geometry index count exceeds i32")?;
        if indices.iter().any(|&index| index >= vertex_count as u32) {
            return Err("SDL geometry index refers outside the vertex buffer".into());
        }
        // SAFETY: Both input slices remain valid throughout SDL's synchronous
        // read, their lengths fit the C int ABI, and all indices reference
        // vertices within the supplied slice.
        unsafe {
            check(SDL_RenderGeometry(
                self.raw,
                texture,
                vertices.as_ptr(),
                vertex_count,
                indices.as_ptr().cast(),
                index_count,
            ))
        }
    }
    pub fn present(&self) -> Result<(), String> {
        unsafe { check(SDL_RenderPresent(self.raw)) }
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.destroy_native();
    }
}

/// Test-only owned surface for SDL's software renderer. The test harness
/// must drop the renderer before this backing surface.
#[cfg(test)]
pub struct TestSurface(*mut SDL_Surface);

#[cfg(test)]
impl TestSurface {
    pub fn new([width, height]: [i32; 2]) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err("Test surface dimensions must be positive".into());
        }
        unsafe {
            let raw = SDL_CreateSurface(width, height, SDL_PIXELFORMAT_RGBA32);
            if raw.is_null() {
                return Err(error());
            }
            Ok(Self(raw))
        }
    }

    pub fn software_renderer(&self) -> Result<Renderer, String> {
        unsafe {
            let raw = SDL_CreateSoftwareRenderer(self.0);
            if raw.is_null() {
                return Err(error());
            }
            let renderer = Renderer {
                raw,
                extended: false,
                name: "software".into(),
            };
            check(SDL_SetRenderDrawBlendMode(raw, SDL_BLENDMODE_BLEND))?;
            Ok(renderer)
        }
    }

    pub fn size(&self) -> [i32; 2] {
        unsafe { [(*self.0).w, (*self.0).h] }
    }

    fn readback(renderer: &Renderer) -> Result<Self, String> {
        unsafe {
            let raw = SDL_RenderReadPixels(renderer.raw, ptr::null());
            if raw.is_null() {
                return Err(error());
            }
            Ok(Self(raw))
        }
    }

    /// Read a pixel from an owned test surface; share this one native read
    /// boundary with both single-pixel and full-frame test assertions.
    fn read_pixel(&self, x: i32, y: i32) -> Result<[u8; 4], String> {
        let [mut r, mut g, mut b, mut a] = [0; 4];
        // SAFETY: TestSurface holds the live SDL_Surface allocation, and the
        // four initialized output bytes are valid writable memory. SDL checks
        // whether the requested pixel lies inside the surface.
        unsafe {
            check(SDL_ReadSurfacePixel(
                self.0, x, y, &mut r, &mut g, &mut b, &mut a,
            ))?;
        }
        Ok([r, g, b, a])
    }

    pub fn pixel(renderer: &Renderer, x: i32, y: i32) -> Result<[u8; 4], String> {
        Self::readback(renderer)?.read_pixel(x, y)
    }

    pub fn pixels(renderer: &Renderer) -> Result<Vec<[u8; 4]>, String> {
        let image = Self::readback(renderer)?;
        let [width, height] = image.size();
        let mut pixels = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push(image.read_pixel(x, y)?);
            }
        }
        Ok(pixels)
    }
}

#[cfg(test)]
impl Drop for TestSurface {
    fn drop(&mut self) {
        destroy_owned_surface(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_guard_runs_on_unwind_and_disarms_on_success() {
        use std::cell::Cell;
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let calls = Cell::new(0);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _restore = RestoreOnDrop::new(|| calls.set(calls.get() + 1));
            panic!("simulated rendering callback failure");
        }));
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);

        {
            let mut restore = RestoreOnDrop::new(|| calls.set(calls.get() + 1));
            restore.disarm();
        }
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn tagged_event_accessors_reject_other_union_members() {
        let mut event = SDL_Event::default();
        event.r#type = SDL_EVENT_MOUSE_MOTION.0;
        assert!(mouse_motion_event(&event).is_some());
        assert!(mouse_button_event(&event).is_none());
        assert!(pen_touch_event(&event).is_none());
        assert!(pen_motion_event(&event).is_none());
        assert!(pen_proximity_event(&event).is_none());
        assert!(key_event(&event).is_none());

        event.r#type = SDL_EVENT_PEN_MOTION.0;
        assert!(pen_motion_event(&event).is_some());
        assert!(mouse_motion_event(&event).is_none());

        event.r#type = SDL_EVENT_KEY_DOWN.0;
        assert!(key_event(&event).is_some());
        assert!(pen_motion_event(&event).is_none());
    }

    #[test]
    fn event_window_rejects_a_mismatched_union_tag() {
        let mut event = SDL_Event::default();
        event.r#type = SDL_EVENT_MOUSE_MOTION.0;
        assert!(event_window(&event, SDL_EVENT_KEY_DOWN).is_none());
        assert!(event_window(&event, SDL_EVENT_TEXT_EDITING).is_none());
    }

    #[test]
    fn x11_is_default_and_wayland_is_opt_in() {
        assert_eq!(startup_driver(false, false, true), Some(c"wayland"));
        assert_eq!(startup_driver(true, false, true), Some(c"x11"));
        assert_eq!(startup_driver(true, true, true), Some(c"dummy"));
        assert_eq!(startup_driver(true, false, false), Some(c"x11"));
        assert_eq!(
            startup_driver(false, false, false),
            if cfg!(any(target_os = "linux", target_os = "freebsd")) {
                Some(c"x11")
            } else {
                None
            }
        );
    }
}
