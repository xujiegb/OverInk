//! Native Windows desktop touch keyboard request. Use the official
//! windows-rs IInputPaneInterop projection rather than an untyped COM vtable;
//! only HWND validity and WinRT apartment entry require platform FFI.
#![deny(clippy::undocumented_unsafe_blocks)]

use crate::sdl::Window;
use windows::{
    UI::ViewManagement::InputPane,
    Win32::{
        Foundation::HWND,
        System::WinRT::{IInputPaneInterop, RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
    },
    core::factory,
};

struct Apartment(bool);

impl Apartment {
    fn enter() -> Self {
        // SAFETY: The SDL main thread owns the WinRT UI operation. Only
        // uninitialize an apartment successfully initialized here; if SDL
        // already initialized COM in a different mode, retain its ownership.
        let initialized = unsafe { RoInitialize(RO_INIT_SINGLETHREADED).is_ok() };
        Self(initialized)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: A successful RoInitialize above must be balanced on
            // the same SDL main thread, after all InputPane refs are dropped.
            unsafe { RoUninitialize() };
        }
    }
}

pub fn try_show(window: &Window) -> Result<bool, String> {
    let handle = window
        .win32_hwnd()
        .ok_or("The SDL canvas has no HWND for touch keyboard input")?;
    let _apartment = Apartment::enter();
    let interop = factory::<InputPane, IInputPaneInterop>().map_err(|e| e.to_string())?;
    // SAFETY: The HWND belongs to a live SDL canvas. windows-rs provides
    // the official IInputPaneInterop COM signature, constructs InputPane
    // with the correct IID, and takes ownership of the resulting interface.
    // No raw vtable, GUID, out-pointer, or manual from_raw is needed.
    let pane: InputPane = unsafe { interop.GetForWindow(HWND(handle.as_non_null().as_ptr())) }
        .map_err(|e| e.to_string())?;
    // Windows decides whether a touch keyboard can be displayed for this
    // foreground HWND (e.g. with a physical keyboard connected).
    pane.TryShow().map_err(|e| e.to_string())
}
