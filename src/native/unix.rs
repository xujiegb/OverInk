use super::*;
use crate::controller::Rect;
use libloading::Library;
use std::{
    ffi::{CStr, c_char, c_int, c_ulong},
    ptr::{self, NonNull},
    sync::OnceLock,
};
/// The only native desktop libraries the application may load.
/// Callers cannot construct dynamic paths or select an arbitrary soname.
#[derive(Clone, Copy)]
pub(crate) enum NativeLibrary {
    X11,
    Xext,
    Xrandr,
    Wayland,
}

impl NativeLibrary {
    fn sonames(self) -> &'static [&'static str] {
        match self {
            Self::X11 => &["libX11.so.6", "libX11.so"],
            Self::Xext => &["libXext.so.6", "libXext.so"],
            Self::Xrandr => &["libXrandr.so.2", "libXrandr.so"],
            Self::Wayland => &["libwayland-client.so.0", "libwayland-client.so"],
        }
    }
}

/// Load only known native desktop ABIs; keep the original soname fallback.
pub(crate) fn library(which: NativeLibrary) -> Result<Library, String> {
    let names = which.sonames();
    for &name in names {
        // SAFETY: The sealed enum fixes the only allowable shared-library
        // names. The native adapter retains Library while symbols are used.
        if let Ok(lib) = unsafe { Library::new(name) } {
            return Ok(lib);
        }
    }
    Err(format!("Missing native library: {}", names[0]))
}
type Marshal = unsafe extern "C" fn(*mut c_void, u32, *const c_void, u32, u32, ...) -> *mut c_void;
type Version = unsafe extern "C" fn(*mut c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut c_void);

/// A uniquely owned native Wayland proxy or event queue, paired with its
/// exact C destructor. The library supplying the destructor must outlive it.
struct OwnedWaylandHandle {
    raw: NonNull<c_void>,
    destroy: Destroy,
}

impl OwnedWaylandHandle {
    /// # Safety
    /// `raw` must be null or a newly owned Wayland object, and `destroy`
    /// must be the matching destructor from a library that outlives the guard.
    unsafe fn from_native(raw: *mut c_void, destroy: Destroy) -> Option<Self> {
        NonNull::new(raw).map(|raw| Self { raw, destroy })
    }

    fn as_ptr(&self) -> *mut c_void {
        self.raw.as_ptr()
    }
}

impl Drop for OwnedWaylandHandle {
    fn drop(&mut self) {
        // SAFETY: The guard exclusively owns the non-null native resource,
        // and Wayland keeps its library loaded until after this destructor.
        unsafe { (self.destroy)(self.raw.as_ptr()) };
    }
}

struct Wayland {
    // Rust drops fields in declaration order. Destroy the compositor before
    // its event queue, and unload the library only after both native handles.
    compositor: OwnedWaylandHandle,
    _queue: OwnedWaylandHandle,
    region_interface: *const c_void,
    applied: Option<(*mut c_void, bool, Option<Rect>, [i32; 2])>,
    marshal: Marshal,
    version: Version,
    _lib: Library,
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
            // SAFETY: The listener checks both pointers above. The Wayland
            // roundtrip dispatches it synchronously while its boxed Globals
            // and the protocol-owned interface C string are both live.
            unsafe {
                if CStr::from_ptr(interface) == c"wl_compositor" {
                    let globals = &mut *data.cast::<Globals>();
                    globals.compositor = name;
                    globals.version = version.min(4);
                }
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
            let lib = library(NativeLibrary::Wayland)?;
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
            // Every created resource is guarded immediately. An error or
            // unwind runs the correct native destructor before unloading lib.
            let queue = OwnedWaylandHandle::from_native(create_queue(display), queue_destroy)
                .ok_or("Cannot create Wayland event queue")?;
            let registry_raw = {
                let wrapper =
                    OwnedWaylandHandle::from_native(create_wrapper(display), wrapper_destroy)
                        .ok_or("Cannot wrap Wayland display")?;
                set_queue(wrapper.as_ptr(), queue.as_ptr());
                marshal(
                    wrapper.as_ptr(),
                    1,
                    registry_interface,
                    1,
                    0,
                    ptr::null_mut::<c_void>(),
                )
            };
            let registry = OwnedWaylandHandle::from_native(registry_raw, destroy)
                .ok_or("No Wayland registry")?;
            let mut globals = Box::new(Globals::default());
            if add_listener(
                registry.as_ptr(),
                (&LISTENER as *const Listener).cast(),
                (&mut *globals as *mut Globals).cast(),
            ) != 0
                || roundtrip(display, queue.as_ptr()) < 0
                || globals.compositor == 0
            {
                // Never destroy the callback's userdata before its registry.
                drop(registry);
                return Err("Cannot bind Wayland compositor".into());
            }
            let compositor_raw = marshal(
                registry.as_ptr(),
                0,
                compositor_interface,
                globals.version,
                0,
                globals.compositor,
                c"wl_compositor".as_ptr(),
                globals.version,
                ptr::null_mut::<c_void>(),
            );
            // No further registry dispatch can see the boxed callback data.
            drop(registry);
            let compositor = OwnedWaylandHandle::from_native(compositor_raw, destroy)
                .ok_or("Cannot bind Wayland compositor")?;
            Ok(Self {
                compositor,
                _queue: queue,
                region_interface,
                applied: None,
                marshal,
                version,
                _lib: lib,
            })
        }
    }
    fn set(&mut self, w: &Window, pass: bool, hole: Option<Rect>) -> Result<(), String> {
        let surface = w.wayland_surface().ok_or("No Wayland surface")?.as_ptr();
        // wl_region coordinates are surface-local logical units, not
        // physical pixels like XShape rectangles. Compute and validate
        // the region entirely in safe Rust before crossing the FFI boundary.
        let size = w.size().map(|v| v.ceil() as i32);
        let hole = if pass {
            None
        } else {
            hole.and_then(|area| area.clipped(size))
        };
        if self.applied == Some((surface, pass, hole, size)) {
            return Ok(());
        }
        // SAFETY: The Wayland display, compositor and wl_surface are owned
        // by live SDL/Wayland objects. All protocol function pointers were
        // loaded from libwayland-client and are used on the SDL thread.
        unsafe {
            // A NULL input region means the full surface. An empty region
            // means passthrough. For a controller hole, add the full region
            // and subtract only the protected controller rectangle.
            let region = if pass || hole.is_some() {
                let p = (self.marshal)(
                    self.compositor.as_ptr(),
                    1,
                    self.region_interface,
                    (self.version)(self.compositor.as_ptr()),
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
        }
        self.applied = Some((surface, pass, hole, size));
        Ok(())
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

/// Only known X11 window actions may cross the dynamic Xlib FFI boundary.
enum X11Command<'a> {
    Shape(Option<&'a [Rectangle]>),
    Raise,
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
    /// Execute a native X11 operation without exposing function pointers.
    /// Rectangles stay borrowed and live through XShape's synchronous call.
    fn dispatch(&self, w: &Window, command: X11Command<'_>) -> Result<(), String> {
        let (display, window) = x11_window(w)?;
        let count = match &command {
            X11Command::Shape(Some(rectangles)) => {
                c_int::try_from(rectangles.len()).map_err(|_| "Too many X11 shape rectangles")?
            }
            _ => 0,
        };
        // SAFETY: Both native handles came from a live SDL X11 Window. The
        // Xlib/Xext libraries are retained in the static LIBRARIES cell;
        // borrowed rectangle storage remains alive until the call returns.
        unsafe {
            match command {
                X11Command::Shape(Some(rectangles)) => {
                    let data = if rectangles.is_empty() {
                        ptr::null()
                    } else {
                        rectangles.as_ptr()
                    };
                    (self.rectangles)(display, window, 2, 0, 0, data, count, 0, 0);
                }
                X11Command::Shape(None) => {
                    (self.mask)(display, window, 2, 0, 0, 0, 0);
                }
                X11Command::Raise => {
                    // Do not transfer keyboard focus away from the canvas.
                    (self.raise)(display, window);
                }
            }
            (self.flush)(display);
        }
        Ok(())
    }

    fn new(w: &Window) -> Result<Self, String> {
        unsafe {
            // XShape adds hooks to SDL's X Display. Keep their code loaded
            // until after SDL has closed that connection.
            static LIBRARIES: OnceLock<Result<(Library, Library), String>> = OnceLock::new();
            let (x11, ext) = LIBRARIES
                .get_or_init(|| Ok((library(NativeLibrary::X11)?, library(NativeLibrary::Xext)?)))
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
        self.dispatch(w, X11Command::Shape(rectangles.as_deref()))?;
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
        if let Backend::X11(x11) = &self.backend {
            let _ = x11.dispatch(w, X11Command::Raise);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OwnedWaylandHandle;
    use std::{
        ffi::c_void,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe extern "C" fn release_test_handle(raw: *mut c_void) {
        // SAFETY: The test hands its unique Box allocation to the native
        // guard, and this simulated destructor reconstructs it exactly once.
        unsafe { drop(Box::from_raw(raw.cast::<DropCounter>())) };
    }

    #[test]
    fn owned_wayland_handle_releases_exactly_once_and_rejects_null() {
        let releases = Arc::new(AtomicUsize::new(0));
        let raw = Box::into_raw(Box::new(DropCounter(Arc::clone(&releases)))).cast();
        // SAFETY: The newly allocated Box is uniquely owned and matched with
        // its exact destructor while the function remains available.
        let owned = unsafe { OwnedWaylandHandle::from_native(raw, release_test_handle) }
            .expect("live native handle");
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        drop(owned);
        assert_eq!(releases.load(Ordering::SeqCst), 1);

        // SAFETY: A null native resource transfers no ownership.
        let empty =
            unsafe { OwnedWaylandHandle::from_native(std::ptr::null_mut(), release_test_handle) };
        assert!(empty.is_none());
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }
}
