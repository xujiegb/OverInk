//! SDL is owned by the main thread. Window/renderer pointers never cross threads.
#![deny(clippy::undocumented_unsafe_blocks)]
pub mod dialog;
mod dialog_native;
pub use sdl3_sys::{
    blendmode::*, clipboard::*, error::*, events::*, hints::*, init::*, keyboard::*, keycode::*,
    locale::*, mouse::*, pen::*, pixels::*, properties::*, rect::*, render::*, stdinc::*,
    surface::*, touch::*, video::*,
};
use std::{
    ffi::{CStr, CString, c_void},
    marker::PhantomData,
    ptr::{self, NonNull},
};

// Runtime owns SDL video initialization and shutdown. Native device and
// cursor queries read this lifecycle flag instead of calling SDL_WasInit
// through a safe extern that cannot enforce its main-thread precondition.
static SDL_VIDEO_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Read SDL's Wayland scaling hint while the SDL-owning thread is already in
// its audited native initialization boundary. Screenshot workers only need
// the scalar result, not a callable C API accepting arbitrary pointers.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
static WAYLAND_SCALE_TO_DISPLAY_FORCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A borrowed native backend handle whose lifetime is tied to the SDL
/// Window that owns the underlying system object. This also inherits the
/// Window's thread-safety restrictions rather than returning a free-floating
/// NonNull pointer to safe callers.
pub struct NativeWindowHandle<'a> {
    raw: NonNull<c_void>,
    _window: PhantomData<&'a Window>,
}

impl NativeWindowHandle<'_> {
    /// Keep the non-null identity and the SDL window borrow tied together
    /// across all supported native backends.
    pub fn as_non_null(&self) -> NonNull<c_void> {
        self.raw
    }
}

/// Known SDL window property keys, with no arbitrary raw C-string input.
enum NativeWindowProperty {
    #[cfg(target_os = "windows")]
    Win32Hwnd,
    #[cfg(target_os = "macos")]
    CocoaWindow,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    X11Display,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    WaylandSurface,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    WaylandDisplay,
}

impl NativeWindowProperty {
    fn key(&self) -> Option<NonNull<std::ffi::c_char>> {
        let key = match self {
            #[cfg(target_os = "windows")]
            Self::Win32Hwnd => SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            #[cfg(target_os = "macos")]
            Self::CocoaWindow => SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Self::X11Display => SDL_PROP_WINDOW_X11_DISPLAY_POINTER,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Self::WaylandSurface => SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Self::WaylandDisplay => SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER,
        };
        NonNull::new(key.cast_mut())
    }
}

/// A native buffer returned by an SDL API that explicitly transfers
/// ownership to the caller via SDL_free. Borrowed SDL strings must not
/// be wrapped by this type.
struct SdlAllocation<T>(NonNull<T>);

/// The only permitted SDL lifecycle operations. Native handles only enter
/// these variants from their unique Rust owners; no foreign function pointer
/// or raw buffer can be supplied by application code.
enum SdlNativeAction<'a> {
    IsMainThread,
    ModState,
    DelayPrecise(u64),
    ScreenshotTimestamp,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    RegisterUserEvent,
    Shutdown,
    FreeAllocation(NonNull<c_void>),
    CreateProperties,
    DestroyProperties(SDL_PropertiesID),
    CreateCrosshair,
    SetCursor(Option<NonNull<SDL_Cursor>>),
    CurrentCursor,
    DefaultCursor,
    DestroyCursor(NonNull<SDL_Cursor>),
    DestroySurface(NonNull<SDL_Surface>),
    DestroyTexture(NonNull<SDL_Texture>),
    DestroyRenderer(NonNull<SDL_Renderer>),
    DestroyWindow(SDL_WindowID, NonNull<SDL_Window>),
    SaveBmp(&'a OwnedSurface, &'a CStr),
    InkTextureUpload(&'a OwnedTexture, &'a [u8], i32),
    TextureModulation(&'a OwnedTexture, [f32; 3], f32),
    GuiTextureUpload(&'a OwnedTexture, Option<&'a SDL_Rect>, &'a [u8], i32),
    ClipboardWrite(&'a CStr),
    ClipboardRead,
    SystemTheme,
    IndirectTouch(SDL_TouchID),
    CaptureMouse(bool),
    MouseButtonsReleased,
    PollEvent(&'a mut SDL_Event),
    WaitEvent(i32),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    PushUserEvent(&'a mut SDL_Event),
    #[cfg(target_os = "linux")]
    FlushQuitEvent,
}

enum SdlNativeResult {
    Done,
    ModState(SDL_Keymod),
    ScreenshotTimestamp(String),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    EventType(u32),
    Properties(SDL_PropertiesID),
    Cursor(Option<NonNull<SDL_Cursor>>),
    Success(bool),
    ClipboardText(Option<String>),
    Theme(SDL_SystemTheme),
}

impl SdlNativeResult {
    fn properties(self) -> Option<SDL_PropertiesID> {
        if let Self::Properties(id) = self {
            Some(id)
        } else {
            None
        }
    }

    fn cursor(self) -> Option<NonNull<SDL_Cursor>> {
        if let Self::Cursor(cursor) = self {
            cursor
        } else {
            None
        }
    }

    fn succeeded(self) -> bool {
        matches!(self, Self::Success(true))
    }

    fn clipboard_text(self) -> Option<String> {
        if let Self::ClipboardText(text) = self {
            text
        } else {
            None
        }
    }

    fn mod_state(self) -> SDL_Keymod {
        if let Self::ModState(modifiers) = self {
            modifiers
        } else {
            SDL_KMOD_NONE
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn event_type(self) -> u32 {
        if let Self::EventType(ty) = self {
            ty
        } else {
            0
        }
    }

    fn screenshot_timestamp(self) -> String {
        if let Self::ScreenshotTimestamp(stem) = self {
            stem
        } else {
            "OverInk".into()
        }
    }

    fn theme(self) -> SDL_SystemTheme {
        if let Self::Theme(theme) = self {
            theme
        } else {
            SDL_SYSTEM_THEME_UNKNOWN
        }
    }
}

/// Native SDL operations are restricted to typed actions; application code
/// never receives unsafe FFI functions or naked SDL-owned allocations.
fn sdl_native(action: SdlNativeAction<'_>) -> SdlNativeResult {
    // SAFETY: Destruction variants are created only by unique Rust owners.
    // Cursor, device, event-poll and video operations are checked at the
    // owning main-thread wrappers. SDL_PushEvent copies the locally built
    // user event, and SDL_WaitEventTimeout receives no output buffer.
    // Getter outputs are initialized local scalars. SDL_free only receives
    // allocations created by SDL and kept in RAII guards.
    unsafe {
        match action {
            SdlNativeAction::IsMainThread => SdlNativeResult::Success(SDL_IsMainThread()),
            SdlNativeAction::ModState => SdlNativeResult::ModState(SDL_GetModState()),
            SdlNativeAction::DelayPrecise(nanoseconds) => {
                sdl3_sys::timer::SDL_DelayPrecise(nanoseconds);
                SdlNativeResult::Done
            }
            SdlNativeAction::ScreenshotTimestamp => {
                use sdl3_sys::time::{SDL_DateTime, SDL_GetCurrentTime, SDL_TimeToDateTime};
                let mut ticks = 0;
                let mut date = SDL_DateTime::default();
                let stem = if SDL_GetCurrentTime(&mut ticks)
                    && SDL_TimeToDateTime(ticks, &mut date, true)
                {
                    format!(
                        "OverInk-{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
                        date.year, date.month, date.day, date.hour, date.minute, date.second
                    )
                } else {
                    "OverInk".into()
                };
                SdlNativeResult::ScreenshotTimestamp(stem)
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            SdlNativeAction::RegisterUserEvent => SdlNativeResult::EventType(SDL_RegisterEvents(1)),
            SdlNativeAction::Shutdown => {
                SDL_Quit();
                SdlNativeResult::Done
            }
            SdlNativeAction::FreeAllocation(raw) => {
                SDL_free(raw.as_ptr());
                SdlNativeResult::Done
            }
            SdlNativeAction::CreateProperties => {
                SdlNativeResult::Properties(SDL_CreateProperties())
            }
            SdlNativeAction::DestroyProperties(id) => {
                SDL_DestroyProperties(id);
                SdlNativeResult::Done
            }
            SdlNativeAction::CreateCrosshair => SdlNativeResult::Cursor(NonNull::new(
                SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_CROSSHAIR),
            )),
            SdlNativeAction::SetCursor(cursor) => SdlNativeResult::Success(SDL_SetCursor(
                cursor.map_or(ptr::null_mut(), NonNull::as_ptr),
            )),
            SdlNativeAction::CurrentCursor => {
                SdlNativeResult::Cursor(NonNull::new(SDL_GetCursor()))
            }
            SdlNativeAction::DefaultCursor => {
                SdlNativeResult::Cursor(NonNull::new(SDL_GetDefaultCursor()))
            }
            SdlNativeAction::DestroyCursor(cursor) => {
                SDL_DestroyCursor(cursor.as_ptr());
                SdlNativeResult::Done
            }
            SdlNativeAction::DestroySurface(surface) => {
                SDL_DestroySurface(surface.as_ptr());
                SdlNativeResult::Done
            }
            SdlNativeAction::DestroyTexture(texture) => {
                SDL_DestroyTexture(texture.as_ptr());
                SdlNativeResult::Done
            }
            SdlNativeAction::DestroyRenderer(renderer) => {
                SDL_DestroyRenderer(renderer.as_ptr());
                SdlNativeResult::Done
            }
            SdlNativeAction::DestroyWindow(id, window) => {
                if SDL_GetWindowFromID(id) == window.as_ptr() {
                    SDL_DestroyWindow(window.as_ptr());
                }
                SdlNativeResult::Done
            }
            SdlNativeAction::SaveBmp(surface, name) => {
                SdlNativeResult::Success(SDL_SaveBMP(surface.as_non_null().as_ptr(), name.as_ptr()))
            }
            SdlNativeAction::InkTextureUpload(texture, pixels, pitch) => {
                SdlNativeResult::Success(SDL_UpdateTexture(
                    texture.as_non_null().as_ptr(),
                    ptr::null(),
                    pixels.as_ptr().cast(),
                    pitch,
                ))
            }
            SdlNativeAction::TextureModulation(texture, rgb, alpha) => {
                let raw = texture.as_non_null().as_ptr();
                SdlNativeResult::Success(
                    SDL_SetTextureColorModFloat(raw, rgb[0], rgb[1], rgb[2])
                        && SDL_SetTextureAlphaModFloat(raw, alpha),
                )
            }
            SdlNativeAction::GuiTextureUpload(texture, rect, rgba, pitch) => {
                SdlNativeResult::Success(SDL_UpdateTexture(
                    texture.as_non_null().as_ptr(),
                    rect.map_or(ptr::null(), |area| area),
                    rgba.as_ptr().cast(),
                    pitch,
                ))
            }
            SdlNativeAction::ClipboardWrite(value) => {
                SdlNativeResult::Success(SDL_SetClipboardText(value.as_ptr()))
            }
            SdlNativeAction::ClipboardRead => {
                // SDL owns the returned allocation until the RAII guard drops.
                let text = NonNull::new(SDL_GetClipboardText()).map(|raw| {
                    let owned = SdlAllocation(raw);
                    CStr::from_ptr(owned.0.as_ptr())
                        .to_string_lossy()
                        .into_owned()
                });
                SdlNativeResult::ClipboardText(text)
            }
            SdlNativeAction::SystemTheme => SdlNativeResult::Theme(SDL_GetSystemTheme()),
            SdlNativeAction::IndirectTouch(device) => SdlNativeResult::Success(matches!(
                SDL_GetTouchDeviceType(device),
                SDL_TOUCH_DEVICE_INDIRECT_ABSOLUTE | SDL_TOUCH_DEVICE_INDIRECT_RELATIVE
            )),
            SdlNativeAction::CaptureMouse(enabled) => {
                SdlNativeResult::Success(SDL_CaptureMouse(enabled))
            }
            SdlNativeAction::MouseButtonsReleased => {
                let (mut x, mut y) = (0.0, 0.0);
                #[cfg(target_os = "macos")]
                let buttons = SDL_GetGlobalMouseState(&mut x, &mut y);
                #[cfg(not(target_os = "macos"))]
                let buttons = SDL_GetMouseState(&mut x, &mut y);
                SdlNativeResult::Success(buttons.0 == 0)
            }
            SdlNativeAction::PollEvent(event) => SdlNativeResult::Success(SDL_PollEvent(event)),
            SdlNativeAction::WaitEvent(timeout) => {
                SdlNativeResult::Success(SDL_WaitEventTimeout(ptr::null_mut(), timeout))
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            SdlNativeAction::PushUserEvent(event) => SdlNativeResult::Success(SDL_PushEvent(event)),
            #[cfg(target_os = "linux")]
            SdlNativeAction::FlushQuitEvent => {
                SDL_FlushEvent(SDL_EVENT_QUIT.0);
                SdlNativeResult::Done
            }
        }
    }
}

impl<T> Drop for SdlAllocation<T> {
    fn drop(&mut self) {
        let _ = sdl_native(SdlNativeAction::FreeAllocation(self.0.cast()));
    }
}

/// Restrict native string getters to known SDL functions; a caller cannot
/// inject a function returning an invalid C string into this safe API.
enum SdlStringSource {
    Error,
    VideoDriver,
}

/// Copy a borrowed SDL C string while its provider's allocation is live.
fn sdl_string(source: SdlStringSource) -> Option<String> {
    // SAFETY: These exact SDL getters return null or valid NUL-terminated
    // borrowed strings. Copy the bytes before another SDL call can replace
    // the underlying storage.
    unsafe {
        let text = match source {
            SdlStringSource::Error => SDL_GetError(),
            SdlStringSource::VideoDriver => SDL_GetCurrentVideoDriver(),
        };
        if text.is_null() {
            None
        } else {
            Some(CStr::from_ptr(text).to_string_lossy().into_owned())
        }
    }
}

pub fn error() -> String {
    sdl_string(SdlStringSource::Error)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "SDL did not report an error message".into())
}
pub fn check(value: bool) -> Result<(), String> {
    if value { Ok(()) } else { Err(error()) }
}
pub fn event_type(event: &SDL_Event) -> SDL_EventType {
    // SAFETY: SDL_Event's discriminant occupies the shared first field of
    // every union variant, and the borrowed event is initialized.
    unsafe { SDL_EventType(event.r#type) }
}
pub fn cstring(v: &str) -> CString {
    CString::new(v).unwrap_or_else(|_| {
        // Only malformed inputs need a second allocation to remove NULs.
        CString::new(v.replace('\0', "")).unwrap_or_default()
    })
}
pub fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        || ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}
/// SDL's video lifetime is confined to its creating main thread.
pub struct Runtime {
    _main_thread: PhantomData<std::rc::Rc<()>>,
}
impl Runtime {
    pub fn new(x11: bool, wayland: bool, smoke: bool) -> Result<Self, String> {
        if !is_main_thread() {
            return Err("SDL video must initialize on the main thread".into());
        }
        // A second Runtime could call SDL_Quit while the first one still
        // owns windows, renderers and textures. Never allow overlapping owners.
        if SDL_VIDEO_READY.load(std::sync::atomic::Ordering::Acquire) {
            return Err("An SDL video runtime is already active".into());
        }
        let wayland = wayland
            && cfg!(any(target_os = "linux", target_os = "freebsd"))
            && is_wayland_session();
        let backend = startup_driver(x11, smoke, wayland);
        // SAFETY: The SDL main-thread check succeeded. SDL copies the fixed
        // static hint names and NUL-terminated values; Runtime owns shutdown.
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
            SDL_VIDEO_READY.store(true, std::sync::atomic::Ordering::Release);
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            {
                // Query the SDL hint in this existing native initialization
                // scope, not through a safe extern with a pointer argument.
                let name = SDL_HINT_VIDEO_WAYLAND_SCALE_TO_DISPLAY;
                let enabled = !name.is_null() && SDL_GetHintBoolean(name, false);
                WAYLAND_SCALE_TO_DISPLAY_FORCED
                    .store(enabled, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(Self {
                _main_thread: PhantomData,
            })
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
        // The runtime cannot cross threads in safe Rust. Do not shut SDL
        // down if a foreign callback somehow causes off-main destruction.
        if is_main_thread() {
            SDL_VIDEO_READY.store(false, std::sync::atomic::Ordering::Release);
            let _ = sdl_native(SdlNativeAction::Shutdown);
        }
    }
}
pub fn driver() -> String {
    sdl_string(SdlStringSource::VideoDriver).unwrap_or_else(|| "unknown".into())
}

/// Check SDL's native main-thread identity without exposing an FFI function.
fn is_main_thread() -> bool {
    sdl_native(SdlNativeAction::IsMainThread).succeeded()
}

/// SDL's Wayland scaling hint as observed at video initialization.
/// OverInk never mutates this hint after initialization.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn wayland_scale_to_display_forced() -> bool {
    WAYLAND_SCALE_TO_DISPLAY_FORCED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Display IDs come from SDL enumeration. Only these known queries can
/// cross into the native display backend; no foreign buffers escape.
enum DisplayBoundsKind {
    Full,
    Usable,
}
enum NativeDisplayQuery {
    Bounds(u32, DisplayBoundsKind),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    DesktopMode(u32),
}
enum NativeDisplayResult {
    Bounds([i32; 4]),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    DesktopMode([i32; 2], f32),
}

fn native_display_query(query: NativeDisplayQuery) -> Result<NativeDisplayResult, String> {
    if !is_main_thread() {
        return Err("SDL display query requires the main thread".into());
    }
    // SAFETY: The SDL video subsystem and display ID are checked by SDL.
    // These queries only borrow initialized output rectangles or SDL-owned
    // desktop-mode storage, and copy all scalar fields before returning.
    unsafe {
        match query {
            NativeDisplayQuery::Bounds(id, kind) => {
                let mut bounds = SDL_Rect::default();
                let display = SDL_DisplayID(id);
                check(match kind {
                    DisplayBoundsKind::Full => SDL_GetDisplayBounds(display, &mut bounds),
                    DisplayBoundsKind::Usable => SDL_GetDisplayUsableBounds(display, &mut bounds),
                })?;
                Ok(NativeDisplayResult::Bounds([
                    bounds.x, bounds.y, bounds.w, bounds.h,
                ]))
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            NativeDisplayQuery::DesktopMode(id) => {
                let mode = SDL_GetDesktopDisplayMode(SDL_DisplayID(id))
                    .as_ref()
                    .ok_or_else(error)?;
                Ok(NativeDisplayResult::DesktopMode(
                    [mode.w, mode.h],
                    mode.pixel_density,
                ))
            }
        }
    }
}

fn query_display_bounds(id: u32, kind: DisplayBoundsKind) -> Result<[i32; 4], String> {
    match native_display_query(NativeDisplayQuery::Bounds(id, kind))? {
        NativeDisplayResult::Bounds(bounds) => Ok(bounds),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        NativeDisplayResult::DesktopMode(..) => Err("Unexpected SDL display query result".into()),
    }
}

/// Return usable display bounds only when SDL provides them.
pub fn usable_display_bounds(id: u32) -> Option<[i32; 4]> {
    query_display_bounds(id, DisplayBoundsKind::Usable).ok()
}

/// Read the native mode's logical dimensions and pixel density.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn desktop_display_mode(id: u32) -> Option<([i32; 2], f32)> {
    match native_display_query(NativeDisplayQuery::DesktopMode(id)).ok()? {
        NativeDisplayResult::DesktopMode(size, density) => Some((size, density)),
        NativeDisplayResult::Bounds(_) => None,
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

/// Returns the owning SDL window without reading an unchecked union
/// field. Every typed accessor verifies the event's *actual* tag.
pub fn event_window(event: &SDL_Event, ty: SDL_EventType) -> Option<SDL_WindowID> {
    // Never trust a caller-supplied tag as proof of a union's active field.
    if event_type(event) != ty {
        return None;
    }
    let window = match ty {
        SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => {
            mouse_button_event(event)?.windowID
        }
        SDL_EVENT_MOUSE_MOTION => mouse_motion_event(event)?.windowID,
        SDL_EVENT_MOUSE_WHEEL => mouse_wheel_event(event)?.windowID,
        SDL_EVENT_FINGER_DOWN
        | SDL_EVENT_FINGER_MOTION
        | SDL_EVENT_FINGER_UP
        | SDL_EVENT_FINGER_CANCELED => finger_event(event)?.windowID,
        SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_PROXIMITY_OUT => {
            pen_proximity_event(event)?.windowID
        }
        SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => pen_touch_event(event)?.windowID,
        SDL_EVENT_PEN_MOTION => pen_motion_event(event)?.windowID,
        SDL_EVENT_PEN_AXIS => pen_axis_event(event)?.windowID,
        SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP => key_event(event)?.windowID,
        SDL_EVENT_WINDOW_FOCUS_GAINED
        | SDL_EVENT_WINDOW_FOCUS_LOST
        | SDL_EVENT_WINDOW_MOUSE_LEAVE => {
            tagged_event!(
                event,
                window,
                SDL_EVENT_WINDOW_FOCUS_GAINED
                    | SDL_EVENT_WINDOW_FOCUS_LOST
                    | SDL_EVENT_WINDOW_MOUSE_LEAVE
            )?
            .windowID
        }
        SDL_EVENT_TEXT_INPUT => tagged_event!(event, text, SDL_EVENT_TEXT_INPUT)?.windowID,
        SDL_EVENT_TEXT_EDITING => tagged_event!(event, edit, SDL_EVENT_TEXT_EDITING)?.windowID,
        _ => return None,
    };
    (window.0 != 0).then_some(window)
}

/// Query indirect touch device capability via SDL's native input API.
pub fn indirect_touch_device(device: SDL_TouchID) -> bool {
    // Query native devices only while the Runtime-owned SDL video subsystem
    // is live, and always on SDL's owning main thread.
    is_main_thread()
        && SDL_VIDEO_READY.load(std::sync::atomic::Ordering::Acquire)
        && sdl_native(SdlNativeAction::IndirectTouch(device)).succeeded()
}

/// Sleep with SDL's precise delay without exposing raw native calls to users.
pub fn delay_precise(nanoseconds: u64) {
    let _ = sdl_native(SdlNativeAction::DelayPrecise(nanoseconds));
}

/// Validate SDL-provided array counts before forming slices over foreign
/// allocations. Real desktop and locale lists are far below these limits.
fn bounded_sdl_count(count: i32, maximum: usize) -> Option<usize> {
    usize::try_from(count).ok().filter(|&n| n <= maximum)
}

/// Enumerate SDL preferred locales and release SDL-owned storage before return.
pub fn preferred_locales() -> Vec<(String, String)> {
    // SAFETY: SDL owns the returned array, whose bounded count is checked
    // before slicing. SdlAllocation releases it after copying each locale.
    unsafe {
        let mut count = 0;
        let Some(locales) = NonNull::new(SDL_GetPreferredLocales(&mut count)).map(SdlAllocation)
        else {
            return Vec::new();
        };
        let Some(count) = bounded_sdl_count(count, 256) else {
            return Vec::new();
        };
        // SDL allocated the returned pointer array, and the count is now
        // bounded before any pointer arithmetic or slice construction.
        let entries = std::slice::from_raw_parts(locales.0.as_ptr(), count);
        let name = |ptr: Option<NonNull<std::ffi::c_char>>| {
            if let Some(ptr) = ptr {
                CStr::from_ptr(ptr.as_ptr())
                    .to_str()
                    .unwrap_or_default()
                    .to_owned()
            } else {
                String::new()
            }
        };
        entries
            .iter()
            .filter_map(|locale| locale.as_ref())
            .map(|locale| {
                (
                    name(NonNull::new(locale.language.cast_mut())),
                    name(NonNull::new(locale.country.cast_mut())),
                )
            })
            .collect()
    }
}

/// Query the operating system's theme through SDL.
pub fn system_theme() -> SDL_SystemTheme {
    // SDL's video/theme query must run on the main thread. Avoid querying
    // shared video state from arbitrary safe Rust worker threads.
    if is_main_thread() {
        sdl_native(SdlNativeAction::SystemTheme).theme()
    } else {
        SDL_SYSTEM_THEME_UNKNOWN
    }
}

/// Reserve one user event type while the video subsystem is active.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn register_user_event() -> u32 {
    if !is_main_thread() || !SDL_VIDEO_READY.load(std::sync::atomic::Ordering::Acquire) {
        return 0;
    }
    sdl_native(SdlNativeAction::RegisterUserEvent).event_type()
}

/// Only registered SDL user event types may be used to wake the portal
/// monitor; safe callers can no longer enqueue an arbitrary SDL union.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn push_user_event(ty: u32) -> bool {
    if !(SDL_EVENT_USER.0..SDL_EVENT_LAST.0).contains(&ty) {
        return false;
    }
    // Only the SDL-defined type field is set. The rest of the union stays
    // zeroed and carries no borrowed native data.
    let mut event = SDL_Event::default();
    event.r#type = ty;
    sdl_native(SdlNativeAction::PushUserEvent(&mut event)).succeeded()
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
    // SAFETY: SDL returns a caller-owned display ID list. Validate the count,
    // copy native metadata synchronously, and release the list through RAII.
    unsafe {
        let mut count = 0;
        let list = NonNull::new(SDL_GetDisplays(&mut count))
            .map(SdlAllocation)
            .ok_or_else(error)?;
        let count = bounded_sdl_count(count, 256)
            .ok_or_else(|| "SDL returned an invalid display count".to_owned())?;
        // Keep the owned SDL allocation alive while iterating a bounded
        // borrowed slice; Rust performs subsequent indexing safely.
        let displays = std::slice::from_raw_parts(list.0.as_ptr(), count);
        displays
            .iter()
            .copied()
            .enumerate()
            .map(|(i, id)| {
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

/// Read keyboard modifiers only on SDL's owning thread.
pub fn mod_state() -> SDL_Keymod {
    if is_main_thread() && SDL_VIDEO_READY.load(std::sync::atomic::Ordering::Acquire) {
        sdl_native(SdlNativeAction::ModState).mod_state()
    } else {
        SDL_KMOD_NONE
    }
}

/// Copy text to the native clipboard using an owned NUL-terminated argument.
pub fn set_clipboard_text(value: &str) {
    let text = cstring(value);
    let _ = sdl_native(SdlNativeAction::ClipboardWrite(&text));
}

/// Copy SDL-owned clipboard text before releasing its allocation through RAII.
pub fn clipboard_text() -> Option<String> {
    sdl_native(SdlNativeAction::ClipboardRead).clipboard_text()
}

/// The two SDL text event types share a single validated native string
/// extraction boundary. Null input text is ignored; null IME text means empty.
enum EventTextKind {
    Input,
    Editing,
}

fn event_text(
    event: &SDL_Event,
    window: SDL_WindowID,
    kind: EventTextKind,
) -> Option<(String, u64, i32, i32)> {
    let (pointer, timestamp, start, length, null_is_empty) = match kind {
        EventTextKind::Input => {
            let input = tagged_event!(event, text, SDL_EVENT_TEXT_INPUT)?;
            if input.windowID != window {
                return None;
            }
            (input.text, input.timestamp, 0, 0, false)
        }
        EventTextKind::Editing => {
            let edit = tagged_event!(event, edit, SDL_EVENT_TEXT_EDITING)?;
            if edit.windowID != window {
                return None;
            }
            (edit.text, edit.timestamp, edit.start, edit.length, true)
        }
    };
    if pointer.is_null() && !null_is_empty {
        return None;
    }
    let text = if pointer.is_null() {
        String::new()
    } else {
        // SAFETY: The tagged accessor verified the active SDL event union
        // member. SDL owns the NUL-terminated text until this event is freed;
        // the string is copied before the event borrow ends.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    };
    Some((text, timestamp, start, length))
}

/// Return text input while the SDL event's native text pointer remains valid.
pub fn text_input_data(event: &SDL_Event, window: SDL_WindowID) -> Option<(String, u64)> {
    let (text, timestamp, _, _) = event_text(event, window, EventTextKind::Input)?;
    Some((text, timestamp))
}

/// Owned IME composition update; event-owned pointers never escape this API.
pub struct TextEditingData {
    pub timestamp: u64,
    pub text: String,
    pub start: i32,
    pub length: i32,
}

pub fn text_editing_data(event: &SDL_Event, window: SDL_WindowID) -> Option<TextEditingData> {
    let (text, timestamp, start, length) = event_text(event, window, EventTextKind::Editing)?;
    Some(TextEditingData {
        timestamp,
        text,
        start,
        length,
    })
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
    if !is_main_thread() {
        return false;
    }
    sdl_native(SdlNativeAction::CaptureMouse(enable)).succeeded()
}

/// Keep the cursor allocation owned and restore the default on drop.
pub struct SystemCursor(NonNull<SDL_Cursor>);

impl SystemCursor {
    pub fn crosshair() -> Option<Self> {
        if !is_main_thread() || !SDL_VIDEO_READY.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let cursor = sdl_native(SdlNativeAction::CreateCrosshair).cursor()?;
        if !sdl_native(SdlNativeAction::SetCursor(Some(cursor))).succeeded() {
            let _ = sdl_native(SdlNativeAction::DestroyCursor(cursor));
            return None;
        }
        Some(Self(cursor))
    }
}

impl Drop for SystemCursor {
    fn drop(&mut self) {
        // A later cursor may have replaced this one before Drop, so only
        // restore the default when we still own the active cursor.
        if sdl_native(SdlNativeAction::CurrentCursor).cursor() == Some(self.0) {
            let default = sdl_native(SdlNativeAction::DefaultCursor).cursor();
            let _ = sdl_native(SdlNativeAction::SetCursor(default));
        }
        let _ = sdl_native(SdlNativeAction::DestroyCursor(self.0));
    }
}

/// Local timestamp for screenshot filenames, using SDL's platform clock.
pub fn screenshot_file_stem() -> String {
    sdl_native(SdlNativeAction::ScreenshotTimestamp).screenshot_timestamp()
}

/// Take the next queued SDL event on the SDL-owning thread.
pub fn poll_event(event: &mut SDL_Event) -> bool {
    is_main_thread() && sdl_native(SdlNativeAction::PollEvent(event)).succeeded()
}

/// Remove the leftover SDL quit event when dismissing a temporary guide.
#[cfg(target_os = "linux")]
pub fn flush_quit_event() {
    if is_main_thread() {
        let _ = sdl_native(SdlNativeAction::FlushQuitEvent);
    }
}

/// Read an SDL display-removal event only after checking its union tag.
pub fn removed_display_id(event: &SDL_Event) -> Option<u32> {
    tagged_event!(event, display, SDL_EVENT_DISPLAY_REMOVED).map(|display| display.displayID.0)
}

/// Read a window event's owning ID only for a tagged window event.
pub fn window_event_id(event: &SDL_Event) -> Option<SDL_WindowID> {
    if (SDL_EVENT_WINDOW_FIRST..=SDL_EVENT_WINDOW_LAST).contains(&event_type(event)) {
        // SAFETY: The tag matches an SDL window event, so the window union
        // variant is the active member and its window ID is initialized.
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
    is_main_thread() && sdl_native(SdlNativeAction::MouseButtonsReleased).succeeded()
}

/// Wait without dequeuing the SDL event, preserving the polling loop's order.
pub fn wait_for_event(timeout_ms: i32) {
    if is_main_thread() {
        let _ = sdl_native(SdlNativeAction::WaitEvent(timeout_ms));
    }
}

/// Boolean window queries return owned scalars rather than native flags.
enum WindowBooleanQuery<'a> {
    TextInputActive,
    InputFocused,
    Hidden,
    Transparent,
    HdrEnabled,
    // Typed output borrows are valid for the synchronous SDL queries.
    Size(bool, &'a mut [i32; 2]),
    Position(&'a mut [i32; 4]),
    #[cfg(not(target_os = "windows"))]
    Borders(&'a mut [i32; 4]),
}

/// Numeric SDL property reads must use one of these known keys.
enum WindowNumberQuery {
    DisplayId,
    #[cfg(target_os = "macos")]
    CocoaMetalViewTag,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    X11Window,
}

/// Coordinate outputs share one audited SDL boundary.
enum WindowCoordinateQuery {
    Position,
    #[cfg(not(target_os = "windows"))]
    Borders,
}

/// Window floating-point metrics are read using one constrained SDL boundary.
enum WindowFloatQuery {
    SdrWhiteLevel,
    DisplayScale,
    PixelDensity,
}

/// Allowed state changes for one live SDL window. Callers provide only
/// ordinary values and borrowed, initialized rectangles.
enum WindowCommand<'a> {
    InputArea(&'a SDL_Rect),
    SyncInput(bool),
    ClearComposition,
    SyncAndPumpEvents,
    SetSize([i32; 2]),
    SetPosition([i32; 2]),
    SyncWindow(bool),
    Maximize,
    CenterOnDisplay(u32),
    SetGeometry {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    AlwaysOnTop(bool),
    RaiseIfCurrent,
    Show(bool),
    SetParent(Option<&'a Window>),
    ToolbarSetup([i32; 2], &'a mut [u8]),
    #[cfg(not(target_os = "windows"))]
    ShapeRegions([i32; 2], Option<&'a [SDL_Rect]>),
}

/// Closed SDL window constructors, retaining borrowed title and parent
/// references only until native creation completes.
enum WindowCreation<'a> {
    Main(&'a CStr, i32, i32, bool),
    Tooltip(&'a Window, [i32; 4]),
}

/// SDL only borrows the hit-test point while this callback runs.
extern "C" fn toolbar_hit_test(
    window: *mut SDL_Window,
    point: *const SDL_Point,
    _: *mut c_void,
) -> SDL_HitTestResult {
    if window.is_null() {
        return SDL_HITTEST_NORMAL;
    }
    // SAFETY: SDL invokes this callback with a valid live window
    // and a point borrowed for the duration of the call. The null
    // check protects against malformed callback inputs.
    let (point, scale) = unsafe {
        let Some(point) = point.as_ref() else {
            return SDL_HITTEST_NORMAL;
        };
        let scale = SDL_GetWindowDisplayScale(window) / SDL_GetWindowPixelDensity(window).max(0.1);
        (point, scale)
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

pub struct Window {
    // Only the SDL adapter may use or destroy this owned native handle.
    raw: NonNull<SDL_Window>,
    pub id: SDL_WindowID,
}
impl Window {
    /// Copy only a known numeric SDL window property; never expose a
    /// property handle or unchecked key to higher-level callers.
    fn query_number(&self, query: WindowNumberQuery) -> i64 {
        // SAFETY: SDL owns the still-live window. This only reads a copied
        // number from the window's native properties or its display ID.
        unsafe {
            match query {
                WindowNumberQuery::DisplayId => SDL_GetDisplayForWindow(self.raw.as_ptr()).0 as i64,
                #[cfg(target_os = "macos")]
                WindowNumberQuery::CocoaMetalViewTag => SDL_GetNumberProperty(
                    SDL_GetWindowProperties(self.raw.as_ptr()),
                    SDL_PROP_WINDOW_COCOA_METAL_VIEW_TAG_NUMBER,
                    0,
                ),
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                WindowNumberQuery::X11Window => SDL_GetNumberProperty(
                    SDL_GetWindowProperties(self.raw.as_ptr()),
                    SDL_PROP_WINDOW_X11_WINDOW_NUMBER,
                    0,
                ),
            }
        }
    }

    /// Copy initialized coordinate outputs from the already-audited native
    /// Window query boundary. Position failures remain errors; unsupported
    /// decorations still return the zero defaults.
    fn query_coordinates(&self, query: WindowCoordinateQuery) -> Result<[i32; 4], String> {
        if !is_main_thread() {
            return Err("SDL window position query requires the main thread".into());
        }
        let mut output = [0; 4];
        match query {
            WindowCoordinateQuery::Position => {
                check(self.query_bool(WindowBooleanQuery::Position(&mut output)))?;
            }
            #[cfg(not(target_os = "windows"))]
            WindowCoordinateQuery::Borders => {
                let _ = self.query_bool(WindowBooleanQuery::Borders(&mut output));
            }
        }
        Ok(output)
    }

    /// Read native Window state or checked, synchronous output coordinates.
    /// An existing unsafe FFI block handles all SDL_Window pointer queries.
    fn query_bool(&self, query: WindowBooleanQuery<'_>) -> bool {
        // SAFETY: Window owns the live SDL window on the SDL thread.
        // Each branch reads only scalar flags or boolean SDL properties.
        unsafe {
            match query {
                WindowBooleanQuery::Size(in_pixels, output) => {
                    let (mut width, mut height) = (0, 0);
                    let success = if in_pixels {
                        SDL_GetWindowSizeInPixels(self.raw.as_ptr(), &mut width, &mut height)
                    } else {
                        SDL_GetWindowSize(self.raw.as_ptr(), &mut width, &mut height)
                    };
                    if success {
                        *output = [width, height];
                    }
                    success
                }
                WindowBooleanQuery::Position(output) => {
                    let (mut x, mut y) = (0, 0);
                    let success = SDL_GetWindowPosition(self.raw.as_ptr(), &mut x, &mut y);
                    if success {
                        *output = [x, y, 0, 0];
                    }
                    success
                }
                #[cfg(not(target_os = "windows"))]
                WindowBooleanQuery::Borders(output) => {
                    let (mut top, mut left, mut bottom, mut right) = (0, 0, 0, 0);
                    let success = SDL_GetWindowBordersSize(
                        self.raw.as_ptr(),
                        &mut top,
                        &mut left,
                        &mut bottom,
                        &mut right,
                    );
                    *output = [top, left, bottom, right];
                    success
                }
                WindowBooleanQuery::TextInputActive => SDL_TextInputActive(self.raw.as_ptr()),
                WindowBooleanQuery::InputFocused => {
                    SDL_GetWindowFlags(self.raw.as_ptr()) & SDL_WINDOW_INPUT_FOCUS != 0
                }
                WindowBooleanQuery::Hidden => {
                    SDL_GetWindowFlags(self.raw.as_ptr()) & SDL_WINDOW_HIDDEN != 0
                }
                WindowBooleanQuery::Transparent => {
                    SDL_GetWindowFlags(self.raw.as_ptr()) & SDL_WINDOW_TRANSPARENT != 0
                }
                WindowBooleanQuery::HdrEnabled => {
                    SDL_GetBooleanProperty(
                        SDL_GetWindowProperties(self.raw.as_ptr()),
                        SDL_PROP_WINDOW_HDR_ENABLED_BOOLEAN,
                        false,
                    ) || {
                        let display = SDL_GetDisplayForWindow(self.raw.as_ptr());
                        display.0 != 0
                            && SDL_GetBooleanProperty(
                                SDL_GetDisplayProperties(display),
                                SDL_PROP_DISPLAY_HDR_ENABLED_BOOLEAN,
                                false,
                            )
                    }
                }
            }
        }
    }

    /// Read one SDL-owned window metric without exporting native handles.
    fn query_float(&self, query: WindowFloatQuery) -> f32 {
        // SAFETY: The SDL window belongs to this live Window instance.
        // These queries return scalar copies and do not transfer ownership.
        unsafe {
            match query {
                WindowFloatQuery::SdrWhiteLevel => SDL_GetFloatProperty(
                    SDL_GetWindowProperties(self.raw.as_ptr()),
                    SDL_PROP_WINDOW_SDR_WHITE_LEVEL_FLOAT,
                    1.,
                ),
                WindowFloatQuery::DisplayScale => SDL_GetWindowDisplayScale(self.raw.as_ptr()),
                WindowFloatQuery::PixelDensity => SDL_GetWindowPixelDensity(self.raw.as_ptr()),
            }
        }
    }

    /// Dispatch a closed set of window operations on SDL's owning thread.
    /// Input rectangles remain borrowed only for the synchronous FFI call.
    fn run_command(&self, command: WindowCommand<'_>) -> Result<(), String> {
        // SAFETY: Window holds the live native SDL_Window. Callers can only
        // select the operations listed above and supply initialized Rust
        // scalars or a borrowed SDL_Rect, never an arbitrary native pointer.
        unsafe {
            match command {
                WindowCommand::InputArea(area) => {
                    SDL_SetTextInputArea(self.raw.as_ptr(), area, 0);
                    Ok(())
                }
                WindowCommand::SyncInput(enabled) => {
                    if enabled != SDL_TextInputActive(self.raw.as_ptr()) {
                        if enabled {
                            SDL_StartTextInput(self.raw.as_ptr());
                        } else {
                            SDL_StopTextInput(self.raw.as_ptr());
                        }
                    }
                    Ok(())
                }
                WindowCommand::ClearComposition => {
                    SDL_ClearComposition(self.raw.as_ptr());
                    Ok(())
                }
                WindowCommand::SyncAndPumpEvents => {
                    SDL_SyncWindow(self.raw.as_ptr());
                    SDL_PumpEvents();
                    Ok(())
                }
                WindowCommand::SetSize([width, height]) => {
                    check(SDL_SetWindowSize(self.raw.as_ptr(), width, height))
                }
                WindowCommand::SetPosition([x, y]) => {
                    check(SDL_SetWindowPosition(self.raw.as_ptr(), x, y))
                }
                WindowCommand::SyncWindow(clear_error) => {
                    if clear_error {
                        SDL_ClearError();
                    }
                    check(SDL_SyncWindow(self.raw.as_ptr()))
                }
                WindowCommand::Maximize => check(SDL_MaximizeWindow(self.raw.as_ptr())),
                WindowCommand::CenterOnDisplay(id) => {
                    let center = SDL_WINDOWPOS_CENTERED_DISPLAY(SDL_DisplayID(id));
                    check(SDL_SetWindowPosition(self.raw.as_ptr(), center, center))
                }
                WindowCommand::SetGeometry { x, y, w, h } => {
                    check(SDL_SetWindowSize(self.raw.as_ptr(), w, h))?;
                    check(SDL_SetWindowPosition(self.raw.as_ptr(), x, y))
                }
                WindowCommand::AlwaysOnTop(enabled) => {
                    let _ = SDL_SetWindowAlwaysOnTop(self.raw.as_ptr(), enabled);
                    Ok(())
                }
                WindowCommand::RaiseIfCurrent => {
                    let window = SDL_GetWindowFromID(self.id);
                    if !window.is_null() && window == self.raw.as_ptr() {
                        SDL_RaiseWindow(window);
                    }
                    Ok(())
                }
                WindowCommand::Show(show) => {
                    SDL_ClearError();
                    check(if show {
                        SDL_ShowWindow(self.raw.as_ptr())
                    } else {
                        SDL_HideWindow(self.raw.as_ptr())
                    })
                }
                WindowCommand::SetParent(parent) => {
                    let raw = parent.map_or(ptr::null_mut(), |window| window.raw.as_ptr());
                    if SDL_GetWindowParent(self.raw.as_ptr()) == raw {
                        return Ok(());
                    }
                    check(SDL_SetWindowParent(self.raw.as_ptr(), raw))
                }
                WindowCommand::ToolbarSetup([width, height], pixels) => {
                    SDL_SetWindowResizable(self.raw.as_ptr(), false);
                    if SDL_SetWindowHitTest(
                        self.raw.as_ptr(),
                        Some(toolbar_hit_test),
                        ptr::null_mut(),
                    ) {
                        SDL_SetWindowBordered(self.raw.as_ptr(), false);
                    }
                    // The pixel slice remains mutably borrowed until the
                    // surface has been released by the OwnedSurface guard.
                    let pitch = width.checked_mul(4).ok_or("Invalid toolbar icon pitch")?;
                    let expected = usize::try_from(height)
                        .ok()
                        .and_then(|h| usize::try_from(pitch).ok().and_then(|p| h.checked_mul(p)))
                        .ok_or("Invalid toolbar icon dimensions")?;
                    if width <= 0 || height <= 0 || pixels.len() != expected {
                        return Err("Invalid toolbar icon pixel buffer".into());
                    }
                    let surface = SDL_CreateSurfaceFrom(
                        width,
                        height,
                        SDL_PIXELFORMAT_RGBA32,
                        pixels.as_mut_ptr().cast(),
                        pitch,
                    );
                    if let Some(surface) = NonNull::new(surface).map(OwnedSurface) {
                        SDL_SetWindowIcon(self.raw.as_ptr(), surface.as_non_null().as_ptr());
                    }
                    Ok(())
                }
                #[cfg(not(target_os = "windows"))]
                WindowCommand::ShapeRegions(size, opaque_regions) => {
                    let Some(regions) = opaque_regions else {
                        return check(SDL_SetWindowShape(self.raw.as_ptr(), ptr::null_mut()));
                    };
                    let surface = OwnedSurface(
                        NonNull::new(SDL_CreateSurface(size[0], size[1], SDL_PIXELFORMAT_RGBA32))
                            .ok_or_else(error)?,
                    );
                    let opaque =
                        SDL_MapSurfaceRGBA(surface.as_non_null().as_ptr(), 255, 255, 255, 255);
                    let transparent =
                        SDL_MapSurfaceRGBA(surface.as_non_null().as_ptr(), 0, 0, 0, 0);
                    check(SDL_FillSurfaceRect(
                        surface.as_non_null().as_ptr(),
                        ptr::null(),
                        transparent,
                    ))?;
                    for region in regions {
                        check(SDL_FillSurfaceRect(
                            surface.as_non_null().as_ptr(),
                            region,
                            opaque,
                        ))?;
                    }
                    check(SDL_SetWindowShape(
                        self.raw.as_ptr(),
                        surface.as_non_null().as_ptr(),
                    ))
                }
            }
        }
    }

    /// Query whether this window currently accepts text input.
    pub fn text_input_active(&self) -> bool {
        self.query_bool(WindowBooleanQuery::TextInputActive)
    }

    pub fn set_text_input_area(&self, area: &SDL_Rect) {
        let _ = self.run_command(WindowCommand::InputArea(area));
    }

    pub fn sync_text_input(&self, enabled: bool) {
        let _ = self.run_command(WindowCommand::SyncInput(enabled));
    }

    pub fn clear_composition(&self) {
        let _ = self.run_command(WindowCommand::ClearComposition);
    }

    pub fn sync_and_pump_events(&self) {
        let _ = self.run_command(WindowCommand::SyncAndPumpEvents);
    }

    pub fn sdr_white_level(&self) -> f32 {
        self.query_float(WindowFloatQuery::SdrWhiteLevel)
    }

    #[cfg(target_os = "linux")]
    pub fn set_size(&self, width: i32, height: i32) -> Result<(), String> {
        self.run_command(WindowCommand::SetSize([width, height]))
    }

    pub fn set_geometry(&self, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
        self.run_command(WindowCommand::SetGeometry { x, y, w, h })
    }

    pub fn set_always_on_top(&self, enabled: bool) {
        let _ = self.run_command(WindowCommand::AlwaysOnTop(enabled));
    }

    pub fn raise(&self) {
        let _ = self.run_command(WindowCommand::RaiseIfCurrent);
    }

    pub fn input_focused(&self) -> bool {
        self.query_bool(WindowBooleanQuery::InputFocused)
    }

    pub fn hidden(&self) -> bool {
        self.query_bool(WindowBooleanQuery::Hidden)
    }

    /// Read the actual SDL window transparency flag.
    pub fn transparent(&self) -> bool {
        self.query_bool(WindowBooleanQuery::Transparent)
    }

    /// Apply an RGBA shape mask, or clear it when no regions are supplied.
    /// Own and free the temporary surface even if an SDL operation fails.
    #[cfg(not(target_os = "windows"))]
    pub fn set_opaque_shape_regions(
        &self,
        size: [i32; 2],
        opaque_regions: Option<&[SDL_Rect]>,
    ) -> Result<(), String> {
        self.run_command(WindowCommand::ShapeRegions(size, opaque_regions))
    }

    /// Create either kind of SDL window at one native ownership boundary.
    /// Only this function receives the newly allocated native pointer.
    fn construct(kind: WindowCreation<'_>) -> Result<Self, String> {
        // SAFETY: Both SDL constructors return a newly owned window. The
        // pointer is checked before querying its ID, and is destroyed if
        // SDL fails to assign an ID. A valid pointer is owned only by Self.
        unsafe {
            let raw = match &kind {
                WindowCreation::Main(title, w, h, overlay) => {
                    let mut flags = SDL_WINDOW_HIDDEN
                        | SDL_WINDOW_HIGH_PIXEL_DENSITY
                        | SDL_WINDOW_ALWAYS_ON_TOP
                        | SDL_WINDOW_RESIZABLE;
                    if *overlay {
                        flags |= SDL_WINDOW_BORDERLESS | SDL_WINDOW_TRANSPARENT;
                    }
                    SDL_ClearError();
                    SDL_CreateWindow(title.as_ptr(), *w, *h, flags)
                }
                WindowCreation::Tooltip(parent, [x, y, w, h]) => SDL_CreatePopupWindow(
                    parent.raw.as_ptr(),
                    *x,
                    *y,
                    *w,
                    *h,
                    SDL_WINDOW_TOOLTIP
                        | SDL_WINDOW_HIDDEN
                        | SDL_WINDOW_HIGH_PIXEL_DENSITY
                        | SDL_WINDOW_NOT_FOCUSABLE,
                ),
            };
            let raw = NonNull::new(raw).ok_or_else(|| {
                let detail = error();
                match kind {
                    WindowCreation::Main(title, ..) => format!(
                        "Creating {} on {} failed ({detail})",
                        title.to_string_lossy(),
                        driver()
                    ),
                    WindowCreation::Tooltip(..) => detail,
                }
            })?;
            let id = SDL_GetWindowID(raw.as_ptr());
            if id.0 == 0 {
                let message = error();
                SDL_DestroyWindow(raw.as_ptr());
                return Err(format!("SDL did not assign a window ID: {message}"));
            }
            Ok(Self { raw, id })
        }
    }

    pub fn new(title: &str, w: i32, h: i32, overlay: bool) -> Result<Self, String> {
        let title = cstring(title);
        Self::construct(WindowCreation::Main(&title, w, h, overlay))
    }

    pub fn tooltip(parent: &Self, x: i32, y: i32, w: i32, h: i32) -> Result<Self, String> {
        Self::construct(WindowCreation::Tooltip(parent, [x, y, w, h]))
    }
    /// Read either window geometry in one audited FFI boundary. Each call
    /// performs exactly one SDL query, preserving the rendering fast path.
    fn query_size(&self, in_pixels: bool) -> Option<[i32; 2]> {
        if !is_main_thread() {
            return None;
        }
        let mut output = [0; 2];
        self.query_bool(WindowBooleanQuery::Size(in_pixels, &mut output))
            .then_some(output)
    }

    /// Unclamped dimensions for converting local input into screen pixels.
    /// Render sizes may be clamped to one; input picking must reject zero.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "freebsd"))]
    pub fn capture_coordinate_sizes(&self) -> Option<([f32; 2], [i32; 2])> {
        let [width, height] = self.query_size(false)?;
        let [px, py] = self.query_size(true)?;
        (width > 0 && height > 0 && px > 0 && py > 0)
            .then_some(([width as f32, height as f32], [px, py]))
    }

    pub fn size(&self) -> [f32; 2] {
        let [width, height] = self.query_size(false).unwrap_or([0, 0]);
        [width.max(1) as f32, height.max(1) as f32]
    }
    pub fn setup_toolbar(&self) {
        let icon = crate::toolbar::image();
        let size = [icon.width() as i32, icon.height() as i32];
        let mut pixels = icon.into_raw();
        let _ = self.run_command(WindowCommand::ToolbarSetup(size, &mut pixels));
    }
    pub fn arrange_toolbar(&self, expanded: bool, on_display: Option<u32>) -> Result<(), String> {
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
        let bounds = on_display
            .or_else(|| self.display_id())
            .and_then(usable_display_bounds);
        if let Some([_, _, available_width, available_height]) = bounds {
            w = w.min((available_width - 16).max(1));
            h = h.min((available_height - 16).max(1));
        }
        self.run_command(WindowCommand::SetSize([w, h]))?;
        let video = driver();
        if let Some([left, top, available_width, available_height]) =
            bounds.filter(|_| video != "wayland")
        {
            let current = self.position().ok();
            let [current_x, current_y] = current.unwrap_or([0, 0]);
            let (x, y) = if on_display.is_some() || current.is_none() {
                (
                    left + if video == "x11" { 0 } else { 8 },
                    top + (available_height - h) / 2,
                )
            } else {
                (current_x, current_y)
            };
            let x = x.clamp(left, left + (available_width - w).max(0));
            let y = y.clamp(top, top + (available_height - h).max(0));
            if current.is_none() || (x, y) != (current_x, current_y) {
                self.run_command(WindowCommand::SetPosition([x, y]))?;
            }
            // X11 reports the actual geometry through ConfigureNotify.
            if video != "x11" {
                self.run_command(WindowCommand::SyncWindow(false))?;
            }
        }
        Ok(())
    }
    /// Maintain an SDL Wayland transient parent only when it actually changes.
    pub fn set_parent(&self, parent: Option<&Window>) -> Result<(), String> {
        self.run_command(WindowCommand::SetParent(parent))
    }

    /// SDL's border query can fail on window managers without server-side
    /// decoration. Preserve the undecorated default in that case.
    #[cfg(not(target_os = "windows"))]
    pub fn border_size(&self) -> [i32; 4] {
        self.query_coordinates(WindowCoordinateQuery::Borders)
            .unwrap_or([0; 4])
    }

    pub fn position(&self) -> Result<[i32; 2], String> {
        let [x, y, _, _] = self.query_coordinates(WindowCoordinateQuery::Position)?;
        Ok([x, y])
    }
    pub fn pixel_size(&self) -> [i32; 2] {
        let [width, height] = self.query_size(true).unwrap_or([0, 0]);
        [width.max(1), height.max(1)]
    }
    pub fn scale(&self) -> f32 {
        self.query_float(WindowFloatQuery::DisplayScale).max(1.)
    }
    pub fn pixel_density(&self) -> f32 {
        let density = self.query_float(WindowFloatQuery::PixelDensity);
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
        self.run_command(WindowCommand::Show(show))
    }
    pub fn display_id(&self) -> Option<u32> {
        let id = self.query_number(WindowNumberQuery::DisplayId) as u32;
        (id != 0).then_some(id)
    }
    /// Borrow a native backend handle from the window property list.
    /// The returned handle cannot outlive this Window in safe Rust.
    fn native_pointer_property(
        &self,
        property: NativeWindowProperty,
    ) -> Option<NativeWindowHandle<'_>> {
        let key = property.key()?;
        // SAFETY: The known key belongs to SDL's live Window property
        // table; SDL returns a borrowed pointer or null without transferring
        // ownership. NativeWindowHandle ties it to the Window borrow.
        unsafe {
            NonNull::new(SDL_GetPointerProperty(
                SDL_GetWindowProperties(self.raw.as_ptr()),
                key.as_ptr(),
                ptr::null_mut(),
            ))
            .map(|raw| NativeWindowHandle {
                raw,
                _window: PhantomData,
            })
        }
    }

    /// Borrow the Win32 HWND backing this live SDL window.
    #[cfg(target_os = "windows")]
    pub fn win32_hwnd(&self) -> Option<NativeWindowHandle<'_>> {
        self.native_pointer_property(NativeWindowProperty::Win32Hwnd)
    }

    /// Borrow the Cocoa NSWindow for this live SDL window without exposing
    /// property-list FFI to the platform input-routing code.
    #[cfg(target_os = "macos")]
    pub fn cocoa_window(&self) -> Option<NativeWindowHandle<'_>> {
        self.native_pointer_property(NativeWindowProperty::CocoaWindow)
    }

    /// Read the Cocoa Metal view tag without exposing the SDL property handle.
    #[cfg(target_os = "macos")]
    pub fn cocoa_metal_view_tag(&self) -> Option<isize> {
        let tag = self.query_number(WindowNumberQuery::CocoaMetalViewTag);
        (tag != 0)
            .then_some(tag)
            .and_then(|tag| tag.try_into().ok())
    }

    /// Borrow the native X11 Display for the lifetime of the SDL window.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn x11_display(&self) -> Option<NativeWindowHandle<'_>> {
        self.native_pointer_property(NativeWindowProperty::X11Display)
    }

    /// Borrow the Wayland wl_surface from SDL for the lifetime of this window.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn wayland_surface(&self) -> Option<NativeWindowHandle<'_>> {
        self.native_pointer_property(NativeWindowProperty::WaylandSurface)
    }

    /// Borrow SDL's Wayland display connection while the window remains live.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn wayland_display(&self) -> Option<NativeWindowHandle<'_>> {
        self.native_pointer_property(NativeWindowProperty::WaylandDisplay)
    }

    /// Return the X11 drawable ID associated with this window.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn x11_window_number(&self) -> std::ffi::c_ulong {
        self.query_number(WindowNumberQuery::X11Window) as std::ffi::c_ulong
    }

    pub fn icc_profile(&self) -> Option<Vec<u8>> {
        // SAFETY: The live Window owns the SDL handle. SDL allocates the
        // profile bytes, and the checked slice is copied before RAII release.
        unsafe {
            let mut size = 0;
            let Some(profile) = NonNull::new(SDL_GetWindowICCProfile(self.raw.as_ptr(), &mut size))
                .map(SdlAllocation)
            else {
                // A missing ICC profile is normal; avoid leaving a stale SDL error.
                SDL_ClearError();
                return None;
            };
            if size > 0 && size <= 16 * 1024 * 1024 {
                Some(std::slice::from_raw_parts(profile.0.as_ptr().cast::<u8>(), size).to_vec())
            } else {
                None
            }
        }
    }

    pub fn hdr_enabled(&self) -> bool {
        self.query_bool(WindowBooleanQuery::HdrEnabled)
    }
    pub fn place_on_display(&self, id: u32, overlay: bool) -> Result<(), String> {
        let video = driver();
        let [x, y, width, height] = query_display_bounds(id, DisplayBoundsKind::Full)?;
        if overlay && video == "wayland" {
            // XDG fullscreen hides unrelated content behind transparent
            // surfaces. Maximizing keeps the desktop in the compositor.
            self.run_command(WindowCommand::Maximize)?;
        } else if overlay {
            self.set_geometry(x, y, width, height)?;
            // Give the resize time to settle before allocating a software
            // framebuffer. X11 can time out before the window manager's
            // configure event. SDL will still report the later resize.
            let synced = self.run_command(WindowCommand::SyncWindow(true));
            if video != "x11" {
                synced?;
            }
        } else {
            self.run_command(WindowCommand::CenterOnDisplay(id))?;
        }
        Ok(())
    }
    pub fn recover_on_display(&self, id: u32) -> Result<(), String> {
        if driver() == "wayland" {
            return Ok(());
        }
        // The display bounds and window position already have safe SDL
        // adapters. Compare in i64 to avoid overflow on unusual virtual
        // desktop layouts, without another raw FFI call here.
        let [left, top, width, height] = query_display_bounds(id, DisplayBoundsKind::Usable)?;
        let [x, y] = self.position()?;
        let size = self.size();
        let center_x = i64::from(x) + (size[0] * 0.5) as i64;
        let center_y = i64::from(y) + (size[1] * 0.5) as i64;
        let right = i64::from(left) + i64::from(width);
        let bottom = i64::from(top) + i64::from(height);
        if center_x < i64::from(left)
            || center_x >= right
            || center_y < i64::from(top)
            || center_y >= bottom
        {
            self.place_on_display(id, false)?;
        }
        Ok(())
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        // Only destroy the SDL-owned window while its ID still resolves to our
        // handle; its native identity can be invalidated by parent teardown.
        let _ = sdl_native(SdlNativeAction::DestroyWindow(self.id, self.raw));
    }
}
/// SDL properties have unique caller ownership and must be destroyed even
/// when renderer or texture construction returns early or unwinds.
struct OwnedProperties(SDL_PropertiesID);

impl OwnedProperties {
    fn new() -> Result<Self, String> {
        // No caller can construct a property ID except the SDL lifecycle
        // boundary. The unique owner frees it through Drop.
        let id = sdl_native(SdlNativeAction::CreateProperties)
            .properties()
            .ok_or_else(error)?;
        (id.0 != 0).then(|| Self(id)).ok_or_else(error)
    }

    fn id(&self) -> SDL_PropertiesID {
        self.0
    }
}

impl Drop for OwnedProperties {
    fn drop(&mut self) {
        let _ = sdl_native(SdlNativeAction::DestroyProperties(self.0));
    }
}

/// Release a uniquely owned SDL surface via the native resource dispatcher.
fn destroy_owned_surface(raw: NonNull<SDL_Surface>) {
    let _ = sdl_native(SdlNativeAction::DestroySurface(raw));
}

/// A limited source of new caller-owned SDL surfaces. No borrowed surfaces
/// or arbitrary native surface pointers may be passed to the safe constructor.
enum SurfaceSource<'a> {
    Readback(&'a Renderer),
    #[cfg(test)]
    Test([i32; 2]),
}

/// Temporary SDL surface whose native allocation is released even when an
/// operation returns early or unwinds.
struct OwnedSurface(NonNull<SDL_Surface>);

impl OwnedSurface {
    /// Acquire one of the known SDL-created surfaces and copy its dimensions
    /// before returning to entirely safe test and screenshot code.
    fn acquire(source: SurfaceSource<'_>) -> Result<(Self, [i32; 2]), String> {
        // SAFETY: Both SDL calls return unique, caller-owned surfaces and
        // the owned guard releases them on all success and failure paths.
        // The readback surface is live while reading its C-layout fields.
        unsafe {
            let raw = match source {
                SurfaceSource::Readback(renderer) => {
                    let renderer = renderer.raw.ok_or("Renderer has been destroyed")?;
                    SDL_RenderReadPixels(renderer.as_ptr(), ptr::null())
                }
                #[cfg(test)]
                SurfaceSource::Test([width, height]) => {
                    SDL_CreateSurface(width, height, SDL_PIXELFORMAT_RGBA32)
                }
            };
            let surface = Self(NonNull::new(raw).ok_or_else(error)?);
            let dimensions = [
                (*surface.as_non_null().as_ptr()).w,
                (*surface.as_non_null().as_ptr()).h,
            ];
            if dimensions[0] <= 0 || dimensions[1] <= 0 {
                return Err("SDL returned invalid surface dimensions".into());
            }
            Ok((surface, dimensions))
        }
    }

    fn as_non_null(&self) -> NonNull<SDL_Surface> {
        self.0
    }
}

impl Drop for OwnedSurface {
    fn drop(&mut self) {
        destroy_owned_surface(self.0);
    }
}

/// Consume one uniquely owned SDL texture via the native resource dispatcher.
fn destroy_owned_texture(raw: NonNull<SDL_Texture>) {
    let _ = sdl_native(SdlNativeAction::DestroyTexture(raw));
}

/// Uniquely owned, non-null SDL texture shared by the ink and GUI layers.
/// Texture configuration is transactional: on error, the guard destroys the
/// newly created native texture before returning to the caller.
struct OwnedTexture(NonNull<SDL_Texture>);

impl OwnedTexture {
    fn create(
        renderer: &Renderer,
        format: SDL_PixelFormat,
        access: SDL_TextureAccess,
        size: [i32; 2],
        blend: SDL_BlendMode,
        scale: SDL_ScaleMode,
    ) -> Result<Self, String> {
        let [width, height] = size;
        let renderer = renderer.raw.ok_or("Renderer has been destroyed")?;
        // SAFETY: SDL_CreateTexture yields a unique, caller-owned handle on
        // the SDL thread. The NonNull guard releases it if configuration
        // fails, and only one OwnedTexture takes ownership on success.
        unsafe {
            let raw = NonNull::new(SDL_CreateTexture(
                renderer.as_ptr(),
                format,
                access,
                width,
                height,
            ))
            .ok_or_else(error)?;
            let texture = Self(raw);
            check(SDL_SetTextureBlendMode(
                texture.as_non_null().as_ptr(),
                blend,
            ))?;
            check(SDL_SetTextureScaleMode(
                texture.as_non_null().as_ptr(),
                scale,
            ))?;
            Ok(texture)
        }
    }

    fn as_non_null(&self) -> NonNull<SDL_Texture> {
        self.0
    }
}

impl Drop for OwnedTexture {
    fn drop(&mut self) {
        destroy_owned_texture(self.0);
    }
}

/// Owned SDL texture used by the ink renderer. Native calls and its
/// destructor stay in this adapter; live textures are dropped normally
/// before their renderer is recreated.
pub struct InkTexture {
    raw: OwnedTexture,
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
        let raw = OwnedTexture::create(
            renderer,
            format,
            access,
            [width, height],
            blend,
            SDL_SCALEMODE_NEAREST,
        )?;
        Ok(Self {
            raw,
            dimensions: [width, height],
        })
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
        // The dimensions, pitch, and byte count were validated above; the
        // native dispatcher synchronously borrows the owned texture and pixels.
        check(sdl_native(SdlNativeAction::InkTextureUpload(&self.raw, pixels, pitch)).succeeded())
    }

    pub fn set_modulation(&self, rgb: [f32; 3], alpha: f32) -> Result<(), String> {
        // Both native modulation calls use only the live texture and copied
        // scalar values. A failed color update never performs the alpha update.
        check(sdl_native(SdlNativeAction::TextureModulation(&self.raw, rgb, alpha)).succeeded())
    }

    pub fn draw(
        &self,
        renderer: &Renderer,
        source: Option<&SDL_FRect>,
        destination: Option<&SDL_FRect>,
    ) -> Result<(), String> {
        renderer.draw_owned_texture(OwnedTextureRef::Ink(self), source, destination)
    }
}

/// Owned background snapshot texture, including HDR linear-sRGB textures.
pub struct SnapshotTexture(OwnedTexture);

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
        let renderer = renderer.raw.ok_or("Renderer has been destroyed")?;
        // SAFETY: The renderer is live. The properties guard and owned
        // texture retain single ownership until native configuration ends.
        unsafe {
            let raw = if linear {
                let props = OwnedProperties::new()?;
                check(SDL_SetNumberProperty(
                    props.id(),
                    SDL_PROP_TEXTURE_CREATE_FORMAT_NUMBER,
                    SDL_PIXELFORMAT_RGBA64_FLOAT.0 as i64,
                ))?;
                check(SDL_SetNumberProperty(
                    props.id(),
                    SDL_PROP_TEXTURE_CREATE_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB_LINEAR.0 as i64,
                ))?;
                check(SDL_SetNumberProperty(
                    props.id(),
                    SDL_PROP_TEXTURE_CREATE_WIDTH_NUMBER,
                    width as i64,
                ))?;
                check(SDL_SetNumberProperty(
                    props.id(),
                    SDL_PROP_TEXTURE_CREATE_HEIGHT_NUMBER,
                    height as i64,
                ))?;
                // A display snapshot is already mapped. Do not tone-map it again.
                check(SDL_SetFloatProperty(
                    props.id(),
                    SDL_PROP_TEXTURE_CREATE_HDR_HEADROOM_FLOAT,
                    0.,
                ))?;
                SDL_CreateTextureWithProperties(renderer.as_ptr(), props.id())
            } else {
                SDL_CreateTexture(
                    renderer.as_ptr(),
                    SDL_PIXELFORMAT_RGBA32,
                    SDL_TEXTUREACCESS_STATIC,
                    w,
                    h,
                )
            };
            // SAFETY: Both branches create a new caller-owned SDL texture,
            // and this guard uniquely releases it on any later error.
            let texture = Self(OwnedTexture(NonNull::new(raw).ok_or_else(error)?));
            check(SDL_UpdateTexture(
                texture.0.as_non_null().as_ptr(),
                ptr::null(),
                pixels.as_ptr().cast(),
                pitch,
            ))?;
            check(SDL_SetTextureBlendMode(
                texture.0.as_non_null().as_ptr(),
                SDL_BLENDMODE_NONE,
            ))?;
            check(SDL_SetTextureScaleMode(
                texture.0.as_non_null().as_ptr(),
                SDL_SCALEMODE_LINEAR,
            ))?;
            Ok(texture)
        }
    }

    pub fn draw(
        &self,
        renderer: &Renderer,
        destination: &SDL_FRect,
        linear: bool,
    ) -> Result<(), String> {
        let draw = || {
            renderer.draw_owned_texture(OwnedTextureRef::Snapshot(self), None, Some(destination))
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

/// Validate an egui upload without dereferencing any native SDL texture.
/// The SDL backend is allowed to read only the advertised source rectangle.
fn gui_upload_valid(texture: [i32; 2], source: [i32; 2], rect: Option<&SDL_Rect>) -> bool {
    match rect {
        None => source == texture,
        Some(area) => {
            area.x >= 0
                && area.y >= 0
                && area.w > 0
                && area.h > 0
                && [area.w, area.h] == source
                && area
                    .x
                    .checked_add(area.w)
                    .is_some_and(|end| end <= texture[0])
                && area
                    .y
                    .checked_add(area.h)
                    .is_some_and(|end| end <= texture[1])
        }
    }
}

/// Owned SDL texture backing egui's font atlas and custom UI images.
pub struct GuiTexture {
    raw: OwnedTexture,
    dimensions: [i32; 2],
}

impl GuiTexture {
    pub fn new(renderer: &Renderer, width: usize, height: usize) -> Result<Self, String> {
        let w = i32::try_from(width).map_err(|_| "egui texture width is too large")?;
        let h = i32::try_from(height).map_err(|_| "egui texture height is too large")?;
        if w <= 0 || h <= 0 {
            return Err("egui texture dimensions must be positive".into());
        }
        let raw = OwnedTexture::create(
            renderer,
            SDL_PIXELFORMAT_RGBA32,
            SDL_TEXTUREACCESS_STATIC,
            [w, h],
            SDL_BLENDMODE_BLEND,
            SDL_SCALEMODE_LINEAR,
        )?;
        Ok(Self {
            raw,
            dimensions: [w, h],
        })
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
        let dimensions = [
            i32::try_from(width).map_err(|_| "egui texture width exceeds i32")?,
            i32::try_from(height).map_err(|_| "egui texture height exceeds i32")?,
        ];
        if !gui_upload_valid(self.dimensions, dimensions, rect) {
            return Err("egui texture upload has invalid dimensions or bounds".into());
        }
        // SDL receives only the checked borrowed region and the exact RGBA
        // payload through the existing native operation dispatcher.
        check(
            sdl_native(SdlNativeAction::GuiTextureUpload(
                &self.raw, rect, rgba, pitch,
            ))
            .succeeded(),
        )
    }

    pub fn geometry(
        &self,
        renderer: &Renderer,
        vertices: &[SDL_Vertex],
        indices: &[u32],
    ) -> Result<(), String> {
        renderer.geometry(GeometryTexture::Gui(self), vertices, indices)
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

/// One of the only legal sources for a geometry texture. The native pointer
/// cannot outlive the borrowed Rust texture.
pub enum GeometryTexture<'a> {
    Solid,
    Ink(&'a InkTexture),
    Gui(&'a GuiTexture),
}

impl GeometryTexture<'_> {
    fn raw(&self) -> Option<NonNull<SDL_Texture>> {
        match self {
            Self::Solid => None,
            Self::Ink(texture) => Some(texture.raw.0),
            Self::Gui(texture) => Some(texture.raw.0),
        }
    }
}

/// A borrow of a live, adapter-owned texture, not an arbitrary SDL handle.
enum OwnedTextureRef<'a> {
    Ink(&'a InkTexture),
    Snapshot(&'a SnapshotTexture),
}

impl OwnedTextureRef<'_> {
    fn raw(&self) -> NonNull<SDL_Texture> {
        match self {
            Self::Ink(texture) => texture.raw.0,
            Self::Snapshot(texture) => texture.0.0,
        }
    }
}

/// Query only the renderer color-space information that callers need.
enum RendererPropertyQuery {
    HdrState,
    SdrWhitePoint,
}

/// Renderer target changes only borrow an owned ink target or restore the
/// previous native target returned by SDL; no higher-level caller can pass
/// an arbitrary texture pointer.
enum RenderTarget<'a> {
    Ink(&'a InkTexture),
    Previous(Option<NonNull<SDL_Texture>>),
}

/// Closed set of SDL renderer commands; callers cannot supply native function
/// pointers or arbitrary memory for the renderer to access.
enum RenderCommand<'a> {
    Clear([f32; 4]),
    TransparentRect(&'a SDL_FRect),
    Clip(Option<&'a SDL_Rect>),
    Scale([f32; 2]),
    GetColorScale(&'a mut f32),
    SetColorScale(f32),
    ReadTarget(&'a mut Option<NonNull<SDL_Texture>>),
    SetTarget(RenderTarget<'a>),
    Flush,
    Present,
}

pub struct Renderer {
    // Only the SDL adapter may create, use or destroy this native handle.
    // Callers draw through checked, borrowed APIs rather than raw pointers.
    raw: Option<NonNull<SDL_Renderer>>,
    pub extended: bool,
    pub name: String,
}
impl Renderer {
    /// Read only known SDL renderer color properties at the FFI boundary.
    /// HDR uses both fields from the same property handle.
    fn query_color_property(&self, query: RendererPropertyQuery) -> (bool, f32) {
        let Some(renderer) = self.raw else {
            return (false, 1.);
        };
        // SAFETY: Renderer owns a live SDL renderer. These SDL getters
        // return copied numeric values without exporting native pointers.
        unsafe {
            let props = SDL_GetRendererProperties(renderer.as_ptr());
            match query {
                RendererPropertyQuery::HdrState => (
                    SDL_GetBooleanProperty(props, SDL_PROP_RENDERER_HDR_ENABLED_BOOLEAN, false),
                    SDL_GetFloatProperty(props, SDL_PROP_RENDERER_HDR_HEADROOM_FLOAT, 1.),
                ),
                RendererPropertyQuery::SdrWhitePoint => (
                    false,
                    SDL_GetFloatProperty(props, SDL_PROP_RENDERER_SDR_WHITE_POINT_FLOAT, 1.),
                ),
            }
        }
    }

    /// Render an adapter-owned texture. The enum keeps the native pointer
    /// tied to an active Rust borrow, including across color-scale callbacks.
    fn draw_owned_texture(
        &self,
        texture: OwnedTextureRef<'_>,
        source: Option<&SDL_FRect>,
        destination: Option<&SDL_FRect>,
    ) -> Result<(), String> {
        let Some(renderer) = self.raw else {
            return Err("Renderer has been destroyed".into());
        };
        // SAFETY: Both the renderer and selected texture are live while the
        // synchronous SDL call reads the borrowed source/destination bounds.
        unsafe {
            check(SDL_RenderTexture(
                renderer.as_ptr(),
                texture.raw().as_ptr(),
                source.map_or(ptr::null(), |rect| rect),
                destination.map_or(ptr::null(), |rect| rect),
            ))
        }
    }

    /// The only FFI dispatch for ordinary clear, clip, scale, flush and
    /// presentation commands. Borrowed rectangles remain live through each
    /// synchronous call; the renderer is owned by this adapter.
    fn run_command(&self, command: RenderCommand<'_>) -> Result<(), String> {
        let Some(renderer) = self.raw else {
            return Err("Renderer has been destroyed".into());
        };
        // SAFETY: Renderer owns a live SDL_Renderer on SDL's main thread.
        // Each command passes only initialized scalars or a borrowed SDL_Rect
        // whose lifetime covers the synchronous native call.
        unsafe {
            match command {
                RenderCommand::Clear([r, g, b, a]) => {
                    check(SDL_SetRenderDrawColorFloat(renderer.as_ptr(), r, g, b, a))?;
                    check(SDL_RenderClear(renderer.as_ptr()))
                }
                RenderCommand::TransparentRect(rect) => {
                    check(SDL_SetRenderDrawColorFloat(
                        renderer.as_ptr(),
                        0.,
                        0.,
                        0.,
                        0.,
                    ))?;
                    check(SDL_SetRenderDrawBlendMode(
                        renderer.as_ptr(),
                        SDL_BLENDMODE_NONE,
                    ))?;
                    let draw = check(SDL_RenderFillRect(renderer.as_ptr(), rect));
                    let restore = check(SDL_SetRenderDrawBlendMode(
                        renderer.as_ptr(),
                        SDL_BLENDMODE_BLEND,
                    ));
                    draw.and(restore)
                }
                RenderCommand::Clip(clip) => check(SDL_SetRenderClipRect(
                    renderer.as_ptr(),
                    clip.map_or(ptr::null(), |rect| rect),
                )),
                RenderCommand::Scale([x, y]) => check(SDL_SetRenderScale(renderer.as_ptr(), x, y)),
                RenderCommand::GetColorScale(previous) => {
                    check(SDL_GetRenderColorScale(renderer.as_ptr(), previous))
                }
                RenderCommand::SetColorScale(scale) => {
                    check(SDL_SetRenderColorScale(renderer.as_ptr(), scale))
                }
                RenderCommand::ReadTarget(previous) => {
                    *previous = NonNull::new(SDL_GetRenderTarget(renderer.as_ptr()));
                    Ok(())
                }
                RenderCommand::SetTarget(target) => {
                    let raw = match target {
                        RenderTarget::Ink(texture) => texture.raw.as_non_null().as_ptr(),
                        RenderTarget::Previous(previous) => {
                            previous.map_or(ptr::null_mut(), NonNull::as_ptr)
                        }
                    };
                    check(SDL_SetRenderTarget(renderer.as_ptr(), raw))
                }
                RenderCommand::Flush => check(SDL_FlushRenderer(renderer.as_ptr())),
                RenderCommand::Present => check(SDL_RenderPresent(renderer.as_ptr())),
            }
        }
    }

    /// Release this renderer once, including when macOS recreates the Metal
    /// output. Drop uses the same path to avoid accidental double-destruction.
    fn destroy_native(&mut self) {
        let Some(raw) = self.raw.take() else {
            return;
        };
        // Taking this handle preserves single ownership and prevents a
        // second release during renderer or Metal output recreation.
        let _ = sdl_native(SdlNativeAction::DestroyRenderer(raw));
    }

    pub fn new(window: &Window, force_sdr: bool) -> Result<Self, String> {
        Self::create(window, force_sdr, None)
    }
    pub fn software(window: &Window) -> Result<Self, String> {
        Self::create(window, true, Some(c"software"))
    }
    fn create(window: &Window, force_sdr: bool, driver: Option<&CStr>) -> Result<Self, String> {
        // SAFETY: SDL owns the live Window and copies borrowed CStr data.
        // New renderer handles are checked and subsequently owned by Renderer.
        unsafe {
            let props = OwnedProperties::new()?;
            if !SDL_SetPointerProperty(
                props.id(),
                SDL_PROP_RENDERER_CREATE_WINDOW_POINTER,
                window.raw.as_ptr().cast(),
            ) {
                let message = error();
                return Err(message);
            }
            if let Some(driver) = driver {
                SDL_SetStringProperty(
                    props.id(),
                    SDL_PROP_RENDERER_CREATE_NAME_STRING,
                    driver.as_ptr(),
                );
            }
            let mut raw = None;
            // SDL's transparent Win32 swap chain uses the DXGI bitblt model.
            // Submit sRGB ink to that path so DWM composites it as SDR.
            let transparent = SDL_GetWindowFlags(window.raw.as_ptr()) & SDL_WINDOW_TRANSPARENT != 0;
            let extended_output =
                cfg!(target_os = "macos") || (cfg!(target_os = "windows") && !transparent);
            if !force_sdr && extended_output {
                SDL_SetNumberProperty(
                    props.id(),
                    SDL_PROP_RENDERER_CREATE_OUTPUT_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB_LINEAR.0 as i64,
                );
                SDL_ClearError();
                raw = NonNull::new(SDL_CreateRendererWithProperties(props.id()));
                #[cfg(target_os = "macos")]
                if raw.is_none() {
                    crate::native::macos::remove_metal_view(window);
                }
            }
            if raw.is_none() {
                SDL_SetNumberProperty(
                    props.id(),
                    SDL_PROP_RENDERER_CREATE_OUTPUT_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB.0 as i64,
                );
                SDL_ClearError();
                raw = NonNull::new(SDL_CreateRendererWithProperties(props.id()));
            }
            let Some(raw) = raw else {
                let message = error();
                return Err(message);
            };
            drop(props);
            let p = SDL_GetRendererProperties(raw.as_ptr());
            let extended = SDL_GetNumberProperty(p, SDL_PROP_RENDERER_OUTPUT_COLORSPACE_NUMBER, 0)
                == SDL_COLORSPACE_SRGB_LINEAR.0 as i64;
            let name = SDL_GetRendererName(raw.as_ptr());
            let name = if name.is_null() {
                "unknown".into()
            } else {
                CStr::from_ptr(name).to_string_lossy().into_owned()
            };
            #[cfg(target_os = "macos")]
            if name == "metal" {
                crate::native::macos::configure_metal(raw, extended);
            }
            SDL_SetRenderDrawBlendMode(raw.as_ptr(), SDL_BLENDMODE_BLEND);
            // One shared frame clock paces both windows. Two blocking presents
            // would halve the requested rate on some desktop drivers.
            SDL_SetRenderVSync(raw.as_ptr(), 0);
            Ok(Self {
                raw: Some(raw),
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
        let (surface, _) = OwnedSurface::acquire(SurfaceSource::Readback(self))?;
        let name = cstring(&path.to_string_lossy());
        // The owned surface and filename remain borrowed for the duration of
        // SDL's synchronous save. The native dispatcher keeps the FFI private.
        check(sdl_native(SdlNativeAction::SaveBmp(&surface, &name)).succeeded())
    }

    pub fn hdr(&self) -> (bool, f32) {
        self.query_color_property(RendererPropertyQuery::HdrState)
    }
    pub fn begin(&self, window: &Window, color: [f32; 4]) -> Result<[f32; 2], String> {
        let logical = window.size();
        let pixel = window.pixel_size();
        let scale = [pixel[0] as f32 / logical[0], pixel[1] as f32 / logical[1]];
        self.set_render_scale(scale)?;
        self.set_clip_rect(None);
        self.clear(color)?;
        Ok(scale)
    }
    pub fn clear(&self, color: [f32; 4]) -> Result<(), String> {
        self.run_command(RenderCommand::Clear(color))
    }
    /// Borrow the entire render target for the duration of the draw.
    /// A caller cannot supply an arbitrary or previously destroyed pointer.
    pub fn with_target(
        &self,
        target: &InkTexture,
        draw: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        // The SDL command boundary copies the previous native target.
        // Draw and panic-safe restoration now happen entirely in safe Rust.
        let mut previous = None;
        self.run_command(RenderCommand::ReadTarget(&mut previous))?;
        self.run_command(RenderCommand::SetTarget(RenderTarget::Ink(target)))?;
        let mut unwind_restore = RestoreOnDrop::new(|| {
            let _ = self.run_command(RenderCommand::SetTarget(RenderTarget::Previous(previous)));
        });
        let result = draw();
        let restore = self.run_command(RenderCommand::SetTarget(RenderTarget::Previous(previous)));
        unwind_restore.disarm();
        result.and(restore)
    }
    pub fn set_clip_rect(&self, clip: Option<&SDL_Rect>) {
        let _ = self.run_command(RenderCommand::Clip(clip));
    }
    /// Query the renderer's SDR reference white, keeping SDL properties internal.
    pub fn sdr_white_point(&self) -> f32 {
        self.query_color_property(RendererPropertyQuery::SdrWhitePoint)
            .1
    }

    /// Clear a damaged rectangle without blending, restoring normal draw
    /// blending even if rendering the rectangle returns an error.
    pub fn clear_rect_transparent(&self, rect: &SDL_FRect) -> Result<(), String> {
        self.run_command(RenderCommand::TransparentRect(rect))
    }

    pub fn set_render_scale(&self, scale: [f32; 2]) -> Result<(), String> {
        self.run_command(RenderCommand::Scale(scale))
    }

    /// Set a clip region and return failures (unlike the UI's best-effort clip).
    pub fn try_set_clip_rect(&self, clip: Option<&SDL_Rect>) -> Result<(), String> {
        self.run_command(RenderCommand::Clip(clip))
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
        // The renderer command adapter copies the native color-scale value.
        // The drawing callback and RAII restoration now run in safe Rust.
        let mut previous = 1.;
        self.run_command(RenderCommand::GetColorScale(&mut previous))?;
        let target = if relative { previous * scale } else { scale };
        self.run_command(RenderCommand::SetColorScale(target))?;
        let mut unwind_restore = RestoreOnDrop::new(|| {
            let _ = self.run_command(RenderCommand::SetColorScale(previous));
        });
        let result = draw();
        let restore = self.run_command(RenderCommand::SetColorScale(previous));
        unwind_restore.disarm();
        result.and(restore)
    }

    pub fn flush(&self) -> Result<(), String> {
        self.run_command(RenderCommand::Flush)
    }

    pub fn geometry(
        &self,
        texture: GeometryTexture<'_>,
        vertices: &[SDL_Vertex],
        indices: &[u32],
    ) -> Result<(), String> {
        let Some(renderer) = self.raw else {
            return Err("Renderer has been destroyed".into());
        };
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
                renderer.as_ptr(),
                texture.raw().map_or(ptr::null_mut(), NonNull::as_ptr),
                vertices.as_ptr(),
                vertex_count,
                indices.as_ptr().cast(),
                index_count,
            ))
        }
    }
    pub fn present(&self) -> Result<(), String> {
        self.run_command(RenderCommand::Present)
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.destroy_native();
    }
}

/// Test-only backing surface for an SDL software renderer. The surface is
/// transferred into TestRenderer, so it cannot be dropped while SDL still
/// owns a renderer that reads or writes its pixel buffer.
#[cfg(test)]
pub struct TestSurface {
    surface: OwnedSurface,
    dimensions: [i32; 2],
}

/// The SDL software renderer and its backing surface are one RAII value.
/// Field declaration order is intentional: the renderer is dropped before
/// the surface it borrows. No Renderer can escape by value.
#[cfg(test)]
pub struct TestRenderer {
    renderer: Renderer,
    _surface: TestSurface,
}

#[cfg(test)]
impl std::ops::Deref for TestRenderer {
    type Target = Renderer;

    fn deref(&self) -> &Self::Target {
        &self.renderer
    }
}

#[cfg(test)]
impl TestSurface {
    pub fn new([width, height]: [i32; 2]) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err("Test surface dimensions must be positive".into());
        }
        let (surface, dimensions) = OwnedSurface::acquire(SurfaceSource::Test([width, height]))?;
        Ok(Self {
            surface,
            dimensions,
        })
    }

    pub fn software_renderer(self) -> Result<TestRenderer, String> {
        // SAFETY: SDL receives a pointer into the live surface owned by self.
        // TestRenderer takes ownership of that surface and drops its Renderer
        // first, including when SDL configuration fails during construction.
        unsafe {
            let raw = NonNull::new(SDL_CreateSoftwareRenderer(
                self.surface.as_non_null().as_ptr(),
            ))
            .ok_or_else(error)?;
            let renderer = Renderer {
                raw: Some(raw),
                extended: false,
                name: "software".into(),
            };
            check(SDL_SetRenderDrawBlendMode(
                raw.as_ptr(),
                SDL_BLENDMODE_BLEND,
            ))?;
            Ok(TestRenderer {
                renderer,
                _surface: self,
            })
        }
    }

    pub fn size(&self) -> [i32; 2] {
        self.dimensions
    }

    fn readback(renderer: &Renderer) -> Result<Self, String> {
        let (surface, dimensions) = OwnedSurface::acquire(SurfaceSource::Readback(renderer))?;
        Ok(Self {
            surface,
            dimensions,
        })
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
                self.surface.as_non_null().as_ptr(),
                x,
                y,
                &mut r,
                &mut g,
                &mut b,
                &mut a,
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
mod tests {
    use super::*;

    #[test]
    fn destroyed_renderer_is_idempotent_and_rejects_commands() {
        let mut renderer = Renderer {
            raw: None,
            extended: false,
            name: String::new(),
        };
        renderer.destroy_native();
        renderer.destroy_native();
        assert_eq!(renderer.hdr(), (false, 1.));
        assert!(renderer.clear([0.; 4]).is_err());
        assert!(renderer.flush().is_err());
        assert!(renderer.present().is_err());
    }

    #[test]
    fn cstring_keeps_valid_strings_and_strips_embedded_nuls() {
        assert_eq!(cstring("OverInk").as_c_str(), c"OverInk");
        assert_eq!(cstring("Over\0Ink\0").as_c_str(), c"OverInk");
        assert_eq!(cstring("\0").as_c_str(), c"");
    }

    #[test]
    fn rejects_invalid_native_array_counts() {
        assert_eq!(bounded_sdl_count(0, 256), Some(0));
        assert_eq!(bounded_sdl_count(1, 256), Some(1));
        assert_eq!(bounded_sdl_count(256, 256), Some(256));
        assert_eq!(bounded_sdl_count(-1, 256), None);
        assert_eq!(bounded_sdl_count(257, 256), None);
        assert_eq!(bounded_sdl_count(i32::MAX, 256), None);
    }

    #[test]
    fn gui_upload_rejects_invalid_full_and_partial_regions() {
        let texture = [64, 32];
        assert!(gui_upload_valid(texture, texture, None));
        assert!(!gui_upload_valid(texture, [32, 32], None));

        let valid = SDL_Rect {
            x: 56,
            y: 28,
            w: 8,
            h: 4,
        };
        assert!(gui_upload_valid(texture, [8, 4], Some(&valid)));
        assert!(!gui_upload_valid(texture, [8, 5], Some(&valid)));

        let outside = SDL_Rect { x: 57, ..valid };
        assert!(!gui_upload_valid(texture, [8, 4], Some(&outside)));
        let negative = SDL_Rect { x: -1, ..valid };
        assert!(!gui_upload_valid(texture, [8, 4], Some(&negative)));
        let overflowing = SDL_Rect {
            x: i32::MAX,
            ..valid
        };
        assert!(!gui_upload_valid(texture, [8, 4], Some(&overflowing)));
    }

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
    fn null_text_event_semantics_remain_distinct() -> Result<(), &'static str> {
        let mut event = SDL_Event::default();
        event.r#type = SDL_EVENT_TEXT_EDITING.0;
        let edit = text_editing_data(&event, SDL_WindowID(0))
            .ok_or("SDL text editing event should have a payload")?;
        assert!(edit.text.is_empty());
        assert_eq!(edit.start, 0);
        assert_eq!(edit.length, 0);

        event.r#type = SDL_EVENT_TEXT_INPUT.0;
        assert!(text_input_data(&event, SDL_WindowID(0)).is_none());
        assert!(text_editing_data(&event, SDL_WindowID(0)).is_none());
        Ok(())
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
