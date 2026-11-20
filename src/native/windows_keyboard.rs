//! Native Windows desktop touch keyboard request. The only WinRT/COM
//! operation is contained here; text UI and keyboard policy remain safe Rust.
#![deny(clippy::undocumented_unsafe_blocks)]

use crate::sdl::Window;
use std::ffi::c_void;
use windows::{
    UI::ViewManagement::InputPane,
    Win32::{
        Foundation::HWND,
        System::WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
    },
    core::{GUID, HRESULT, IInspectable_Vtbl, Interface, factory},
};

// IInputPaneInterop is an official Win32 desktop interop interface that is
// not projected in the WinRT InputPane type. The vtable has one method after
// IInspectable, exactly as defined by inputpaneinterop.h.
windows::core::imp::define_interface!(
    IInputPaneInterop,
    IInputPaneInterop_Vtbl,
    0x75cf2c57_9195_4931_8332_f0b409e916af
);
#[repr(C)]
#[allow(non_snake_case)]
pub struct IInputPaneInterop_Vtbl {
    pub base__: IInspectable_Vtbl,
    pub GetForWindow:
        unsafe extern "system" fn(*mut c_void, HWND, *const GUID, *mut *mut c_void) -> HRESULT,
}

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
    let mut raw = std::ptr::null_mut();
    // SAFETY: The HWND is borrowed from a live SDL Window, the activation
    // factory is a COM-owned IInputPaneInterop, and the out value uses the
    // official InputPane IID. The returned interface becomes RAII-owned
    // immediately and is dropped before the COM apartment.
    let pane = unsafe {
        (interop.vtable().GetForWindow)(
            interop.as_raw(),
            HWND(handle.as_non_null().as_ptr()),
            &InputPane::IID,
            &mut raw,
        )
        .ok()
        .map_err(|e| e.to_string())?;
        if raw.is_null() {
            return Err("Windows returned no InputPane for the canvas window".into());
        }
        InputPane::from_raw(raw)
    };
    // Windows decides whether a touch keyboard can be displayed for this
    // foreground HWND (e.g. with a physical keyboard connected).
    pane.TryShow().map_err(|e| e.to_string())
}
