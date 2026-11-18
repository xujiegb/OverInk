#![deny(clippy::undocumented_unsafe_blocks)]

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
/// Reject implausibly large foreign counts before doing pointer arithmetic.
const MAX_X11_SCREENS: c_int = 256;
const MAX_RANDR_CRTCS: c_int = 4096;

fn checked_crtc_count(count: c_int, ptr: *const c_ulong) -> Option<usize> {
    if !(0..=MAX_RANDR_CRTCS).contains(&count) || (count > 0 && ptr.is_null()) {
        return None;
    }
    Some(count as usize)
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
        // Loading is the native adapter's only library-initialization FFI;
        // this guard owns both libraries for the full lifetime of the symbols.
        Some(Self {
            x11: crate::native::native_library(crate::native::NativeLibrary::X11).ok()?,
            randr: crate::native::native_library(crate::native::NativeLibrary::Xrandr).ok()?,
        })
    }
    pub fn active_areas(&self, window: &Window) -> Option<Vec<[i32; 4]>> {
        // SAFETY: Xlib/XRandR symbols retain their loaded libraries through
        // self, and SDL keeps the display live for the borrowed Window.
        // OwnedRandr guards ensure every native allocation is released.
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
            let screen_count = count(display);
            if !(1..=MAX_X11_SCREENS).contains(&screen_count) {
                return None;
            }
            for screen in 0..screen_count {
                let Some(list) = OwnedRandr::from_native(
                    resources(display, root(display, screen)),
                    *free_resources,
                ) else {
                    failed = true;
                    continue;
                };
                // A positive CRTC count must have a backing array. Without
                // this check another screen could hide a malformed response
                // and make the incomplete result look authoritative.
                let Some(crtc_count) = checked_crtc_count(list.data().count, list.data().crtcs)
                else {
                    failed = true;
                    continue;
                };
                if crtc_count > 0 {
                    queried = true;
                    for i in 0..crtc_count {
                        let id = *list.data().crtcs.add(i);
                        let Some(info) =
                            OwnedRandr::from_native(crtc(display, list.as_ptr(), id), *free_crtc)
                        else {
                            failed = true;
                            continue;
                        };
                        if info.data().mode != 0 && info.data().width > 0 && info.data().height > 0
                        {
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

#[cfg(test)]
mod tests {
    use super::{OwnedRandr, checked_crtc_count};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountDrop(Arc<AtomicUsize>);

    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe extern "C" fn free_test_resource(raw: *mut CountDrop) {
        // SAFETY: The test transfers its unique Box allocation to the guard.
        unsafe { drop(Box::from_raw(raw)) };
    }

    #[test]
    fn rejects_invalid_native_crtc_array_lengths() {
        let id: std::ffi::c_ulong = 1;
        let ptr = &id as *const _;
        assert_eq!(checked_crtc_count(0, std::ptr::null()), Some(0));
        assert_eq!(checked_crtc_count(1, std::ptr::null()), None);
        assert_eq!(checked_crtc_count(-1, ptr), None);
        assert_eq!(checked_crtc_count(4097, ptr), None);
        assert_eq!(checked_crtc_count(1, ptr), Some(1));
    }

    #[test]
    fn owned_resource_drops_exactly_once_and_ignores_null() {
        let freed = Arc::new(AtomicUsize::new(0));
        let raw = Box::into_raw(Box::new(CountDrop(freed.clone())));
        // SAFETY: raw is from Box::into_raw and its matching destructor
        // reconstructs that same Box exactly once.
        let owned = unsafe { OwnedRandr::from_native(raw, free_test_resource) }.unwrap();
        drop(owned);
        assert_eq!(freed.load(Ordering::SeqCst), 1);

        // SAFETY: No native allocation is transferred for a null pointer.
        let empty = unsafe {
            OwnedRandr::<CountDrop>::from_native(std::ptr::null_mut(), free_test_resource)
        };
        assert!(empty.is_none());
        assert_eq!(freed.load(Ordering::SeqCst), 1);
    }
}
