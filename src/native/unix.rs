use super::*;
use crate::controller::Rect;
use libloading::Library;
use std::{
    ffi::{CStr, c_char, c_int, c_ulong},
    ptr,
    sync::OnceLock,
};
/// Load only the fixed desktop ABI sonames requested by our native backends.
/// The caller never supplies paths or names from configuration or the network.
pub(crate) fn library(names: &[&str]) -> Result<Library, String> {
    for &name in names {
        if !matches!(
            name,
            "libX11.so.6"
                | "libX11.so"
                | "libXext.so.6"
                | "libXext.so"
                | "libXrandr.so.2"
                | "libXrandr.so"
                | "libwayland-client.so.0"
                | "libwayland-client.so"
        ) {
            return Err("Unsupported native library".into());
        }
        // SAFETY: Only the enumerated desktop library sonames can reach this
        // loader. Symbols never outlive their owning native library.
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
    _lib: Library,
    queue: *mut c_void,
    compositor: *mut c_void,
    region_interface: *const c_void,
    applied: Option<(*mut c_void, bool, Option<Rect>, [i32; 2])>,
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
impl Wayland {
    fn new(w: &Window) -> Result<Self, String> {
        extern "C" fn global(
            data: *mut c_void,
            _registry: *mut c_void,
            name: u32,
            interface: *const c_char,
            version: u32,
        ) {
            if data.is_null() || interface.is_null() {
                return;
            }
            // The private queue dispatches this listener during the roundtrip
            // below, while its Globals storage is alive.
            let interface = unsafe { CStr::from_ptr(interface) };
            if interface == c"wl_compositor" {
                let globals = unsafe { &mut *data.cast::<Globals>() };
                globals.compositor = name;
                globals.version = version.min(4);
            }
        }
        extern "C" fn removed(_data: *mut c_void, _registry: *mut c_void, _name: u32) {}
        #[repr(C)]
        struct Listener {
            global: extern "C" fn(*mut c_void, *mut c_void, u32, *const c_char, u32),
            removed: extern "C" fn(*mut c_void, *mut c_void, u32),
        }
        static LISTENER: Listener = Listener { global, removed };

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
            let region_interface = *lib
                .get(b"wl_region_interface\0")
                .map_err(|e| e.to_string())?;
            let display = w.wayland_display().ok_or("No Wayland display")?.as_ptr();
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
                _lib: lib,
                queue,
                compositor,
                region_interface,
                applied: None,
                marshal,
                version,
                destroy,
                queue_destroy,
            })
        }
    }
    fn set(&mut self, w: &Window, pass: bool, hole: Option<Rect>) -> Result<(), String> {
        unsafe {
            let surface = w.wayland_surface().ok_or("No Wayland surface")?.as_ptr();
            // wl_region coordinates are surface-local logical units, not
            // physical pixels like XShape rectangles.
            let size = w.size().map(|v| v.ceil() as i32);
            let hole = if pass {
                None
            } else {
                hole.and_then(|area| area.clipped(size))
            };
            if self.applied == Some((surface, pass, hole, size)) {
                return Ok(());
            }
            // A NULL input region means the full surface. An empty region
            // means passthrough. For a controller hole, add the full region
            // and subtract only the protected controller rectangle.
            let region = if pass || hole.is_some() {
                let p = (self.marshal)(
                    self.compositor,
                    1,
                    self.region_interface,
                    (self.version)(self.compositor),
                    0,
                    ptr::null_mut::<c_void>(),
                );
                if p.is_null() {
                    return Err("Cannot create input region".into());
                }
                if let Some(hole) = hole {
                    // wl_region.add = opcode 1; wl_region.subtract = opcode 2.
                    (self.marshal)(
                        p,
                        1,
                        ptr::null(),
                        (self.version)(p),
                        0,
                        0_i32,
                        0_i32,
                        size[0],
                        size[1],
                    );
                    (self.marshal)(
                        p,
                        2,
                        ptr::null(),
                        (self.version)(p),
                        0,
                        hole.x,
                        hole.y,
                        hole.w,
                        hole.h,
                    );
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
            self.applied = Some((surface, pass, hole, size));
            Ok(())
        }
    }
}
impl Drop for Wayland {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.compositor);
            (self.queue_destroy)(self.queue)
        }
    }
}
#[repr(C)]
struct Rectangle {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}
type CombineRectangles = unsafe extern "C" fn(
    *mut c_void,
    c_ulong,
    c_int,
    c_int,
    c_int,
    *const Rectangle,
    c_int,
    c_int,
    c_int,
);
type CombineMask = unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_int, c_int, c_ulong, c_int);

#[derive(Clone, Copy, PartialEq, Eq)]
struct InputRegion {
    display: *mut c_void,
    window: c_ulong,
    size: [i32; 2],
    pass: bool,
    hole: Option<Rect>,
}

struct X11 {
    rectangles: CombineRectangles,
    mask: CombineMask,
    raise: unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int,
    flush: unsafe extern "C" fn(*mut c_void) -> c_int,
    applied: Option<InputRegion>,
}

fn x11_window(w: &Window) -> Result<(*mut c_void, c_ulong), String> {
    let display = w.x11_display().ok_or("No X11 window")?.as_ptr();
    let window = w.x11_window_number();
    if window == 0 {
        return Err("No X11 window".into());
    }
    Ok((display, window))
}

impl X11 {
    fn new(w: &Window) -> Result<Self, String> {
        unsafe {
            // XShape adds hooks to SDL's X Display. Keep their code loaded
            // until after SDL has closed that connection.
            static LIBRARIES: OnceLock<Result<(Library, Library), String>> = OnceLock::new();
            let (x11, ext) = LIBRARIES
                .get_or_init(|| {
                    Ok((
                        library(&["libX11.so.6", "libX11.so"])?,
                        library(&["libXext.so.6", "libXext.so"])?,
                    ))
                })
                .as_ref()
                .map_err(Clone::clone)?;
            let (display, _) = x11_window(w)?;
            let query: unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int = *ext
                .get(b"XShapeQueryExtension\0")
                .map_err(|e| e.to_string())?;
            let (mut event, mut error) = (0, 0);
            if query(display, &mut event, &mut error) == 0 {
                return Err("X11 Shape extension unavailable".into());
            }
            Ok(Self {
                rectangles: *ext
                    .get(b"XShapeCombineRectangles\0")
                    .map_err(|e| e.to_string())?,
                mask: *ext.get(b"XShapeCombineMask\0").map_err(|e| e.to_string())?,
                raise: *x11.get(b"XRaiseWindow\0").map_err(|e| e.to_string())?,
                flush: *x11.get(b"XFlush\0").map_err(|e| e.to_string())?,
                applied: None,
            })
        }
    }

    fn set(&mut self, w: &Window, pass: bool, hole: Option<Rect>) -> Result<(), String> {
        let (display, window) = x11_window(w)?;
        let size = w.pixel_size();
        let hole = if pass {
            None
        } else {
            hole.and_then(|hole| hole.pixels(w).clipped(size))
        };
        let region = InputRegion {
            display,
            window,
            size,
            pass,
            hole,
        };
        if self.applied == Some(region) {
            return Ok(());
        }
        // Convert and validate every coordinate before crossing into Xlib.
        // ShapeInput = 2, ShapeSet = 0, Unsorted = 0.
        let rectangles = if pass || hole.is_some() {
            let mut rectangles = Vec::new();
            if hole.is_some() {
                for area in crate::controller::outside(size, hole) {
                    rectangles.push(Rectangle {
                        x: area.x.try_into().map_err(|_| "Input region is too large")?,
                        y: area.y.try_into().map_err(|_| "Input region is too large")?,
                        width: area.w.try_into().map_err(|_| "Input region is too large")?,
                        height: area.h.try_into().map_err(|_| "Input region is too large")?,
                    });
                }
            }
            Some(rectangles)
        } else {
            None
        };
        // SAFETY: The SDL window and X11 Display remain live throughout the
        // calls. Native XRectangle arguments are checked and the borrowed
        // buffer remains alive until XShapeCombineRectangles returns.
        unsafe {
            if let Some(rectangles) = rectangles.as_ref() {
                let data = if rectangles.is_empty() {
                    ptr::null()
                } else {
                    rectangles.as_ptr()
                };
                (self.rectangles)(
                    display,
                    window,
                    2,
                    0,
                    0,
                    data,
                    rectangles.len() as c_int,
                    0,
                    0,
                );
            } else {
                (self.mask)(display, window, 2, 0, 0, 0, 0);
            }
            (self.flush)(display);
        }
        self.applied = Some(region);
        Ok(())
    }
}

enum Backend {
    X11(X11),
    Wayland(Wayland),
    Unavailable(String),
}

pub struct Passthrough {
    backend: Backend,
}
impl Passthrough {
    pub fn new(w: &Window) -> Self {
        let backend = match crate::sdl::driver().as_str() {
            "x11" => X11::new(w).map(Backend::X11),
            "wayland" => Wayland::new(w).map(Backend::Wayland),
            _ => Err("Input passthrough needs X11 or Wayland".into()),
        };
        Self {
            backend: backend.unwrap_or_else(Backend::Unavailable),
        }
    }
    pub fn set_guarded(
        &mut self,
        w: &Window,
        pass: bool,
        hole: Option<Rect>,
    ) -> Result<(), String> {
        match &mut self.backend {
            Backend::X11(x11) => x11.set(w, pass, hole),
            Backend::Wayland(wayland) => wayland.set(w, pass, hole),
            Backend::Unavailable(error) if pass || hole.is_some() => Err(error.clone()),
            Backend::Unavailable(_) => Ok(()),
        }
    }
    pub fn invalidate(&mut self) {
        match &mut self.backend {
            Backend::X11(x11) => x11.applied = None,
            Backend::Wayland(wayland) => wayland.applied = None,
            Backend::Unavailable(_) => {}
        }
    }
    pub fn raise_controller(&self, w: &Window) {
        if let Backend::X11(x11) = &self.backend
            && let Ok((display, window)) = x11_window(w)
        {
            unsafe {
                // Do not transfer keyboard focus away from an active canvas.
                (x11.raise)(display, window);
                (x11.flush)(display);
            }
        }
    }
}
