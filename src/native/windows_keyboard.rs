//! Native Windows desktop touch keyboard request. Use the official
//! windows-rs IInputPaneInterop projection rather than an untyped COM vtable;
//! only HWND validity and WinRT apartment entry require platform FFI.
#![deny(clippy::undocumented_unsafe_blocks)]

use crate::sdl::Window;
use std::{marker::PhantomData, rc::Rc};
use windows::{
    UI::ViewManagement::InputPane,
    Win32::{
        Foundation::HWND,
        System::WinRT::{
            IInputPaneInterop, RO_INIT_SINGLETHREADED, RO_INIT_TYPE, RoInitialize, RoUninitialize,
        },
    },
    core::factory,
};

pub(crate) struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    pub(crate) fn enter(mode: RO_INIT_TYPE) -> windows::core::Result<Self> {
        // SAFETY: RoInitialize acts on the current thread. Only create the
        // guard after a successful call, which must be balanced on that thread.
        unsafe { RoInitialize(mode) }?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: The guard was created after a successful RoInitialize.
        // Callers keep it on the initializing thread after the WinRT objects.
        unsafe { RoUninitialize() };
    }
}

pub fn try_show(window: &Window) -> Result<bool, String> {
    let handle = window
        .win32_hwnd()
        .ok_or("The SDL canvas has no HWND for touch keyboard input")?;
    let _apartment = Apartment::enter(RO_INIT_SINGLETHREADED).ok();
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
