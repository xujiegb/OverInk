use libloading::{Library, Symbol};
use std::{
    ffi::{c_char, c_int, c_ulong, c_void},
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
pub fn active_areas() -> Option<Vec<[i32; 4]>> {
    unsafe {
        let x11 = Library::new("libX11.so.6")
            .or_else(|_| Library::new("libX11.so"))
            .ok()?;
        let randr = Library::new("libXrandr.so.2")
            .or_else(|_| Library::new("libXrandr.so"))
            .ok()?;
        let open: Symbol<unsafe extern "C" fn(*const c_char) -> *mut c_void> =
            x11.get(b"XOpenDisplay\0").ok()?;
        let close: Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
            x11.get(b"XCloseDisplay\0").ok()?;
        let count: Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
            x11.get(b"XScreenCount\0").ok()?;
        let root: Symbol<unsafe extern "C" fn(*mut c_void, c_int) -> c_ulong> =
            x11.get(b"XRootWindow\0").ok()?;
        let resources: Symbol<unsafe extern "C" fn(*mut c_void, c_ulong) -> *mut Resources> =
            randr.get(b"XRRGetScreenResourcesCurrent\0").ok()?;
        let free_resources: Symbol<unsafe extern "C" fn(*mut Resources)> =
            randr.get(b"XRRFreeScreenResources\0").ok()?;
        let crtc: Symbol<unsafe extern "C" fn(*mut c_void, *mut Resources, c_ulong) -> *mut Crtc> =
            randr.get(b"XRRGetCrtcInfo\0").ok()?;
        let free_crtc: Symbol<unsafe extern "C" fn(*mut Crtc)> =
            randr.get(b"XRRFreeCrtcInfo\0").ok()?;
        let display = open(ptr::null());
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
        close(display);
        (queried && !failed).then_some(areas)
    }
}
