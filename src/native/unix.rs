use super::*;
use libloading::Library;
use std::{
    ffi::{CStr, c_char, c_int, c_ulong},
    ptr,
};
fn library(names: &[&str]) -> Result<Library, String> {
    for name in names {
        if let Ok(lib) = unsafe { Library::new(name) } {
            return Ok(lib);
        }
    }
    Err(format!("Missing native library: {}", names[0]))
}
type Marshal = unsafe extern "C" fn(*mut c_void, u32, *const c_void, u32, u32, ...) -> *mut c_void;
type Version = unsafe extern "C" fn(*mut c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut c_void);
struct Wayland {
    lib: Library,
    queue: *mut c_void,
    registry: *mut c_void,
    compositor: *mut c_void,
    marshal: Marshal,
    version: Version,
    destroy: Destroy,
    queue_destroy: Destroy,
}
#[derive(Default)]
struct Globals {
    compositor: u32,
    version: u32,
}
unsafe extern "C" fn global(
    data: *mut c_void,
    _registry: *mut c_void,
    name: u32,
    interface: *const c_char,
    version: u32,
) {
    unsafe {
        if !interface.is_null() && CStr::from_ptr(interface).to_bytes() == b"wl_compositor" {
            let s = &mut *data.cast::<Globals>();
            s.compositor = name;
            s.version = version.min(4);
        }
    }
}
unsafe extern "C" fn removed(_data: *mut c_void, _registry: *mut c_void, _name: u32) {}
#[repr(C)]
struct Listener {
    global: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const c_char, u32),
    removed: unsafe extern "C" fn(*mut c_void, *mut c_void, u32),
}
static LISTENER: Listener = Listener { global, removed };
impl Wayland {
    fn new(w: &Window) -> Result<Self, String> {
        unsafe {
            let lib = library(&["libwayland-client.so.0", "libwayland-client.so"])?;
            let marshal: Marshal = *lib
                .get(b"wl_proxy_marshal_flags\0")
                .map_err(|e| e.to_string())?;
            let version: Version = *lib
                .get(b"wl_proxy_get_version\0")
                .map_err(|e| e.to_string())?;
            let destroy: Destroy = *lib.get(b"wl_proxy_destroy\0").map_err(|e| e.to_string())?;
            let queue_destroy: Destroy = *lib
                .get(b"wl_event_queue_destroy\0")
                .map_err(|e| e.to_string())?;
            let create_queue: unsafe extern "C" fn(*mut c_void) -> *mut c_void = *lib
                .get(b"wl_display_create_queue\0")
                .map_err(|e| e.to_string())?;
            let create_wrapper: unsafe extern "C" fn(*mut c_void) -> *mut c_void = *lib
                .get(b"wl_proxy_create_wrapper\0")
                .map_err(|e| e.to_string())?;
            let wrapper_destroy: Destroy = *lib
                .get(b"wl_proxy_wrapper_destroy\0")
                .map_err(|e| e.to_string())?;
            let set_queue: unsafe extern "C" fn(*mut c_void, *mut c_void) = *lib
                .get(b"wl_proxy_set_queue\0")
                .map_err(|e| e.to_string())?;
            let add_listener: unsafe extern "C" fn(
                *mut c_void,
                *const c_void,
                *mut c_void,
            ) -> c_int = *lib
                .get(b"wl_proxy_add_listener\0")
                .map_err(|e| e.to_string())?;
            let roundtrip: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int = *lib
                .get(b"wl_display_roundtrip_queue\0")
                .map_err(|e| e.to_string())?;
            let registry_interface: *const c_void = *lib
                .get(b"wl_registry_interface\0")
                .map_err(|e| e.to_string())?;
            let compositor_interface: *const c_void = *lib
                .get(b"wl_compositor_interface\0")
                .map_err(|e| e.to_string())?;
            let display = SDL_GetPointerProperty(
                SDL_GetWindowProperties(w.raw),
                SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER,
                ptr::null_mut(),
            );
            if display.is_null() {
                return Err("No Wayland display".into());
            }
            let queue = create_queue(display);
            if queue.is_null() {
                return Err("Cannot create Wayland event queue".into());
            }
            let wrapper = create_wrapper(display);
            if wrapper.is_null() {
                queue_destroy(queue);
                return Err("Cannot wrap Wayland display".into());
            }
            set_queue(wrapper, queue);
            let registry = marshal(
                wrapper,
                1,
                registry_interface,
                1,
                0,
                ptr::null_mut::<c_void>(),
            );
            wrapper_destroy(wrapper);
            if registry.is_null() {
                queue_destroy(queue);
                return Err("No Wayland registry".into());
            }
            let mut globals = Box::new(Globals::default());
            if add_listener(
                registry,
                (&LISTENER as *const Listener).cast(),
                (&mut *globals as *mut Globals).cast(),
            ) != 0
                || roundtrip(display, queue) < 0
                || globals.compositor == 0
            {
                destroy(registry);
                queue_destroy(queue);
                return Err("Cannot bind Wayland compositor".into());
            }
            let compositor = marshal(
                registry,
                0,
                compositor_interface,
                globals.version,
                0,
                globals.compositor,
                c"wl_compositor".as_ptr(),
                globals.version,
                ptr::null_mut::<c_void>(),
            );
            // No more registry dispatch: destroy it before the callback's data is dropped.
            destroy(registry);
            if compositor.is_null() {
                queue_destroy(queue);
                return Err("Cannot bind Wayland compositor".into());
            }
            Ok(Self {
                lib,
                queue,
                registry: ptr::null_mut(),
                compositor,
                marshal,
                version,
                destroy,
                queue_destroy,
            })
        }
    }
    fn set(&self, w: &Window, pass: bool) -> Result<(), String> {
        unsafe {
            let surface = SDL_GetPointerProperty(
                SDL_GetWindowProperties(w.raw),
                SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER,
                ptr::null_mut(),
            );
            if surface.is_null() {
                return Err("No Wayland surface".into());
            }
            let region = if pass {
                let interface: *const c_void = *self
                    .lib
                    .get(b"wl_region_interface\0")
                    .map_err(|e| e.to_string())?;
                let p = (self.marshal)(
                    self.compositor,
                    1,
                    interface,
                    (self.version)(self.compositor),
                    0,
                    ptr::null_mut::<c_void>(),
                );
                if p.is_null() {
                    return Err("Cannot create input region".into());
                }
                p
            } else {
                ptr::null_mut()
            };
            (self.marshal)(surface, 5, ptr::null(), (self.version)(surface), 0, region); // wl_surface.set_input_region
            (self.marshal)(surface, 6, ptr::null(), (self.version)(surface), 0); // wl_surface.commit
            if !region.is_null() {
                (self.marshal)(region, 0, ptr::null(), (self.version)(region), 1);
            }
            Ok(())
        }
    }
}
impl Drop for Wayland {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.compositor);
            if !self.registry.is_null() {
                (self.destroy)(self.registry)
            }
            (self.queue_destroy)(self.queue)
        }
    }
}
pub struct Passthrough {
    wayland: Option<Wayland>,
    failure: Option<String>,
}
impl Passthrough {
    pub fn new(w: &Window) -> Self {
        let (wayland, failure) = if crate::sdl::driver() == "wayland" {
            match Wayland::new(w) {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };
        Self { wayland, failure }
    }
    pub fn set(&mut self, w: &Window, pass: bool) -> Result<(), String> {
        self.set_guarded(w, pass, None)
    }
    pub fn set_guarded(
        &mut self,
        w: &Window,
        pass: bool,
        hole: Option<crate::controller::Rect>,
    ) -> Result<(), String> {
        if let Some(ref wl) = self.wayland {
            return wl.set(w, pass);
        }
        if let Some(ref e) = self.failure {
            return Err(e.clone());
        }
        if crate::sdl::driver() != "x11" {
            return if pass {
                Err("Input passthrough needs X11 or Wayland".into())
            } else {
                Ok(())
            };
        }
        unsafe {
            let x = library(&["libX11.so.6", "libX11.so"])?;
            let ext = library(&["libXext.so.6", "libXext.so"])?;
            let props = SDL_GetWindowProperties(w.raw);
            let display =
                SDL_GetPointerProperty(props, SDL_PROP_WINDOW_X11_DISPLAY_POINTER, ptr::null_mut());
            let window =
                SDL_GetNumberProperty(props, SDL_PROP_WINDOW_X11_WINDOW_NUMBER, 0) as c_ulong;
            if display.is_null() || window == 0 {
                return Err("No X11 window".into());
            }
            let query: libloading::Symbol<
                unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int,
            > = ext
                .get(b"XShapeQueryExtension\0")
                .map_err(|e| e.to_string())?;
            let (mut event, mut error) = (0, 0);
            if query(display, &mut event, &mut error) == 0 {
                return Err("X11 Shape extension unavailable".into());
            }
            if pass {
                let f: libloading::Symbol<
                    unsafe extern "C" fn(
                        *mut c_void,
                        c_ulong,
                        c_int,
                        c_int,
                        c_int,
                        *const c_void,
                        c_int,
                        c_int,
                        c_int,
                    ),
                > = ext
                    .get(b"XShapeCombineRectangles\0")
                    .map_err(|e| e.to_string())?;
                f(display, window, 2, 0, 0, ptr::null(), 0, 0, 0);
            } else if let Some(hole) = hole {
                #[repr(C)]
                struct Rectangle {
                    x: i16,
                    y: i16,
                    width: u16,
                    height: u16,
                }
                let mut rectangles = Vec::new();
                for area in crate::controller::outside(w.pixel_size(), Some(hole.pixels(w))) {
                    rectangles.push(Rectangle {
                        x: area.x.try_into().map_err(|_| "Input region is too large")?,
                        y: area.y.try_into().map_err(|_| "Input region is too large")?,
                        width: area.w.try_into().map_err(|_| "Input region is too large")?,
                        height: area.h.try_into().map_err(|_| "Input region is too large")?,
                    });
                }
                let f: libloading::Symbol<
                    unsafe extern "C" fn(
                        *mut c_void,
                        c_ulong,
                        c_int,
                        c_int,
                        c_int,
                        *const Rectangle,
                        c_int,
                        c_int,
                        c_int,
                    ),
                > = ext
                    .get(b"XShapeCombineRectangles\0")
                    .map_err(|e| e.to_string())?;
                f(
                    display,
                    window,
                    2,
                    0,
                    0,
                    rectangles.as_ptr(),
                    rectangles.len() as c_int,
                    0,
                    0,
                );
            } else {
                let f: libloading::Symbol<
                    unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_int, c_int, c_ulong, c_int),
                > = ext.get(b"XShapeCombineMask\0").map_err(|e| e.to_string())?;
                f(display, window, 2, 0, 0, 0, 0);
            }
            let flush: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
                x.get(b"XFlush\0").map_err(|e| e.to_string())?;
            flush(display);
            Ok(())
        }
    }
}

pub fn raise_controller(window: &Window) -> Result<(), String> {
    if crate::sdl::driver() != "x11" {
        return Ok(());
    }
    unsafe {
        let lib = library(&["libX11.so.6", "libX11.so"])?;
        let props = SDL_GetWindowProperties(window.raw);
        let display =
            SDL_GetPointerProperty(props, SDL_PROP_WINDOW_X11_DISPLAY_POINTER, ptr::null_mut());
        let id = SDL_GetNumberProperty(props, SDL_PROP_WINDOW_X11_WINDOW_NUMBER, 0) as c_ulong;
        if display.is_null() || id == 0 {
            return Err("No X11 controller handle".into());
        }
        let raise: libloading::Symbol<unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int> =
            lib.get(b"XRaiseWindow\0").map_err(|e| e.to_string())?;
        let flush: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
            lib.get(b"XFlush\0").map_err(|e| e.to_string())?;
        // Raising without setting keyboard focus lets the pen keep drawing.
        raise(display, id);
        flush(display);
        Ok(())
    }
}
