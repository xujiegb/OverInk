use crate::sdl::*;
use libloading::{Library, Symbol};
use std::{
    ffi::{c_int, c_ulong, c_void},
    ptr::NonNull,
    sync::OnceLock,
};
#[repr(C)]
struct Resources {
    timestamp: c_ulong,
    config_timestamp: c_ulong,
    count: c_int,
    crtcs: *mut c_ulong,
}
#[repr(C)]
struct Crtc {
    timestamp: c_ulong,
    x: c_int,
    y: c_int,
    width: u32,
    height: u32,
    mode: c_ulong,
}
/// An XRandR-owned allocation, paired with the exact native destructor.
/// `Randr` keeps the libraries providing these functions loaded.
struct OwnedRandr<T> {
    raw: NonNull<T>,
    free: unsafe extern "C" fn(*mut T),
}

impl<T> OwnedRandr<T> {
    /// # Safety
    /// `raw` must be either null or an owned pointer produced by the XRandR
    /// function corresponding to `free`; the library must outlive this guard.
    unsafe fn from_native(raw: *mut T, free: unsafe extern "C" fn(*mut T)) -> Option<Self> {
        NonNull::new(raw).map(|raw| Self { raw, free })
    }

    fn data(&self) -> &T {
        // SAFETY: from_native establishes an owned, non-null allocation.
        unsafe { self.raw.as_ref() }
    }

    fn as_ptr(&self) -> *mut T {
        self.raw.as_ptr()
    }
}

impl<T> Drop for OwnedRandr<T> {
    fn drop(&mut self) {
        // SAFETY: This is the allocation's matching native destructor and
        // `Randr` retains the backing library for the duration of the query.
        unsafe { (self.free)(self.raw.as_ptr()) };
    }
}

// Some XRandR backends retain an SDL display while its output is disabled.
// Check active CRTCs as well, so that disabled outputs cannot retain a page.
pub struct Randr {
    x11: Library,
    randr: Library,
}
impl Randr {
    pub fn get() -> Option<&'static Self> {
        // Xlib may call extension hooks when SDL closes the borrowed Display.
        static API: OnceLock<Option<Randr>> = OnceLock::new();
        API.get_or_init(Self::new).as_ref()
    }
    fn new() -> Option<Self> {
        unsafe {
            Some(Self {
                x11: Library::new("libX11.so.6")
                    .or_else(|_| Library::new("libX11.so"))
                    .ok()?,
                randr: Library::new("libXrandr.so.2")
                    .or_else(|_| Library::new("libXrandr.so"))
                    .ok()?,
            })
        }
    }
    pub fn active_areas(&self, window: &Window) -> Option<Vec<[i32; 4]>> {
        unsafe {
            let count: Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
                self.x11.get(b"XScreenCount\0").ok()?;
            let root: Symbol<unsafe extern "C" fn(*mut c_void, c_int) -> c_ulong> =
                self.x11.get(b"XRootWindow\0").ok()?;
            let resources: Symbol<unsafe extern "C" fn(*mut c_void, c_ulong) -> *mut Resources> =
                self.randr.get(b"XRRGetScreenResourcesCurrent\0").ok()?;
            let free_resources: Symbol<unsafe extern "C" fn(*mut Resources)> =
                self.randr.get(b"XRRFreeScreenResources\0").ok()?;
            let crtc: Symbol<
                unsafe extern "C" fn(*mut c_void, *mut Resources, c_ulong) -> *mut Crtc,
            > = self.randr.get(b"XRRGetCrtcInfo\0").ok()?;
            let free_crtc: Symbol<unsafe extern "C" fn(*mut Crtc)> =
                self.randr.get(b"XRRFreeCrtcInfo\0").ok()?;
            let display = window.x11_display()?.as_ptr();
            let mut areas = Vec::new();
            let mut queried = false;
            let mut failed = false;
            for screen in 0..count(display) {
                let Some(list) =
                    OwnedRandr::from_native(resources(display, root(display, screen)), *free_resources)
                else {
                    failed = true;
                    continue;
                };
                if list.data().count > 0 && !list.data().crtcs.is_null() {
                    queried = true;
                    for i in 0..list.data().count as usize {
                        let id = *list.data().crtcs.add(i);
                        let Some(info) =
                            OwnedRandr::from_native(crtc(display, list.as_ptr(), id), *free_crtc)
                        else {
                            failed = true;
                            continue;
                        };
                        if info.data().mode != 0 && info.data().width > 0 && info.data().height > 0 {
                            if let (Ok(width), Ok(height)) = (
                                i32::try_from(info.data().width),
                                i32::try_from(info.data().height),
                            ) {
                                areas.push([info.data().x, info.data().y, width, height]);
                            } else {
                                failed = true;
                            }
                        }
                    }
                }
            }
            (queried && !failed).then_some(areas)
        }
    }
}
