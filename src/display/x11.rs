use crate::sdl::*;
use libloading::{Library, Symbol};
use std::{
    ffi::{c_int, c_ulong, c_void},
    ptr,
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
// Some XRandR backends retain an SDL display while its output is disabled.
// Check active CRTCs as well, so that disabled outputs cannot retain a page.
pub struct Randr {
    x11: Library,
    randr: Library,
}
impl Randr {
    pub fn new() -> Option<Self> {
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
            let display = SDL_GetPointerProperty(
                SDL_GetWindowProperties(window.raw),
                SDL_PROP_WINDOW_X11_DISPLAY_POINTER,
                ptr::null_mut(),
            );
            if display.is_null() {
                return None;
            }
            let mut areas = Vec::new();
            let mut queried = false;
            let mut failed = false;
            for screen in 0..count(display) {
                let list = resources(display, root(display, screen));
                if list.is_null() {
                    failed = true;
                    continue;
                }
                if (*list).count > 0 && !(*list).crtcs.is_null() {
                    queried = true;
                    for i in 0..(*list).count as usize {
                        let info = crtc(display, list, *(*list).crtcs.add(i));
                        if info.is_null() {
                            failed = true;
                            continue;
                        }
                        if (*info).mode != 0 && (*info).width > 0 && (*info).height > 0 {
                            areas.push([
                                (*info).x,
                                (*info).y,
                                (*info).width as i32,
                                (*info).height as i32,
                            ]);
                        }
                        free_crtc(info);
                    }
                }
                free_resources(list);
            }
            (queried && !failed).then_some(areas)
        }
    }
}
