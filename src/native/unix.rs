use super::*;
use crate::controller::Rect;
use libloading::Library;
use std::{
    ffi::{CStr, c_char, c_int, c_ulong, c_void},
    ptr::{self, NonNull},
    sync::OnceLock,
};
/// The only native desktop libraries the application may load.
/// Callers cannot construct dynamic paths or select an arbitrary soname.
#[derive(Clone, Copy)]
enum NativeLibrary {
    X11,
    Xext,
    Wayland,
}

impl NativeLibrary {
    fn sonames(self) -> &'static [&'static str] {
        match self {
            Self::X11 => &["libX11.so.6", "libX11.so"],
            Self::Xext => &["libXext.so.6", "libXext.so"],
            Self::Wayland => &["libwayland-client.so.0", "libwayland-client.so"],
        }
    }
}

/// Load only known native desktop ABIs; keep the original soname fallback.
fn library(which: NativeLibrary) -> Result<Library, String> {
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
// Fixed parameters use checked native handles. Variadic payloads preserve
// their protocol-specific C representations inside the unsafe FFI boundary.
type Marshal = unsafe extern "C" fn(
    NonNull<c_void>,
    u32,
    Option<NonNull<c_void>>,
    u32,
    u32,
    ...
) -> Option<NonNull<c_void>>;
type Version = unsafe extern "C" fn(NonNull<c_void>) -> u32;
type Destroy = unsafe extern "C" fn(NonNull<c_void>);

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
    unsafe fn from_native(raw: Option<NonNull<c_void>>, destroy: Destroy) -> Option<Self> {
        raw.map(|raw| Self { raw, destroy })
    }
}

impl Drop for OwnedWaylandHandle {
    fn drop(&mut self) {
        // SAFETY: The guard exclusively owns the non-null native resource,
        // and Wayland keeps its library loaded until after this destructor.
        unsafe { (self.destroy)(self.raw) };
    }
}

/// A protocol-owned wl_region. Unlike a generic Wayland proxy, this must
/// send wl_region.destroy (opcode 0 with the destroy flag), not merely
/// call wl_proxy_destroy. Keep the region alive through the surface commit.
struct OwnedWaylandRegion {
    raw: NonNull<c_void>,
    marshal: Marshal,
    version: Version,
}

impl OwnedWaylandRegion {
    /// # Safety
    /// The pointer must be null or a newly owned wl_region, and both function
    /// pointers must remain loaded until the region guard is destroyed.
    unsafe fn from_native(
        raw: Option<NonNull<c_void>>,
        marshal: Marshal,
        version: Version,
    ) -> Option<Self> {
        raw.map(|raw| Self {
            raw,
            marshal,
            version,
        })
    }
}

impl Drop for OwnedWaylandRegion {
    fn drop(&mut self) {
        // SAFETY: The owned region is live. Opcode 0 is wl_region.destroy
        // and the destroy flag causes libwayland to release the proxy.
        unsafe {
            (self.marshal)(self.raw, 0, None, (self.version)(self.raw), 1);
        }
    }
}

struct Wayland {
    // Rust drops fields in declaration order. Destroy the compositor before
    // its event queue, and unload the library only after both native handles.
    compositor: OwnedWaylandHandle,
    _queue: OwnedWaylandHandle,
    region_interface: NonNull<c_void>,
    applied: Option<WaylandApplied>,
    marshal: Marshal,
    version: Version,
    _lib: Library,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct WaylandApplied {
    surface: NonNull<c_void>,
    pass: bool,
    hole: Option<Rect>,
    size: [i32; 2],
}

#[derive(Default)]
struct Globals {
    compositor: u32,
    version: u32,
}
impl Wayland {
    fn new(w: &Window) -> Result<Self, String> {
        extern "C" fn global(
            data: Option<NonNull<Globals>>,
            _registry: Option<NonNull<c_void>>,
            name: u32,
            interface: Option<NonNull<c_char>>,
            version: u32,
        ) {
            let (Some(mut data), Some(interface)) = (data, interface) else {
                return;
            };
            // SAFETY: The listener checks both nullable inputs above. The
            // roundtrip synchronously dispatches callbacks while the Box
            // holding Globals and the Wayland-owned interface name are live.
            unsafe {
                if CStr::from_ptr(interface.as_ptr()) == c"wl_compositor" {
                    let globals = data.as_mut();
                    globals.compositor = name;
                    globals.version = version.min(4);
                }
            }
        }
        extern "C" fn removed(
            _data: Option<NonNull<Globals>>,
            _registry: Option<NonNull<c_void>>,
            _name: u32,
        ) {
        }
        #[repr(C)]
        struct Listener {
            global: extern "C" fn(
                Option<NonNull<Globals>>,
                Option<NonNull<c_void>>,
                u32,
                Option<NonNull<c_char>>,
                u32,
            ),
            removed: extern "C" fn(Option<NonNull<Globals>>, Option<NonNull<c_void>>, u32),
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
            let create_queue: unsafe extern "C" fn(NonNull<c_void>) -> Option<NonNull<c_void>> =
                *lib.get(b"wl_display_create_queue\0")
                    .map_err(|e| e.to_string())?;
            let create_wrapper: unsafe extern "C" fn(NonNull<c_void>) -> Option<NonNull<c_void>> =
                *lib.get(b"wl_proxy_create_wrapper\0")
                    .map_err(|e| e.to_string())?;
            let wrapper_destroy: Destroy = *lib
                .get(b"wl_proxy_wrapper_destroy\0")
                .map_err(|e| e.to_string())?;
            let set_queue: unsafe extern "C" fn(NonNull<c_void>, NonNull<c_void>) = *lib
                .get(b"wl_proxy_set_queue\0")
                .map_err(|e| e.to_string())?;
            // Wayland retains the listener table and user data until the
            // registry proxy is destroyed. LISTENER is static and Globals is
            // pinned by its Box across the synchronous registry roundtrip.
            let add_listener: unsafe extern "C" fn(
                NonNull<c_void>,
                &'static Listener,
                NonNull<Globals>,
            ) -> c_int = *lib
                .get(b"wl_proxy_add_listener\0")
                .map_err(|e| e.to_string())?;
            let roundtrip: unsafe extern "C" fn(NonNull<c_void>, NonNull<c_void>) -> c_int = *lib
                .get(b"wl_display_roundtrip_queue\0")
                .map_err(|e| e.to_string())?;
            let registry_interface = NonNull::new(
                *lib.get(b"wl_registry_interface\0")
                    .map_err(|e| e.to_string())?,
            )
            .ok_or("No Wayland registry interface")?;
            let compositor_interface = NonNull::new(
                *lib.get(b"wl_compositor_interface\0")
                    .map_err(|e| e.to_string())?,
            )
            .ok_or("No Wayland compositor interface")?;
            let region_interface = NonNull::new(
                *lib.get(b"wl_region_interface\0")
                    .map_err(|e| e.to_string())?,
            )
            .ok_or("No Wayland region interface")?;
            let display = w
                .wayland_display()
                .ok_or("No Wayland display")?
                .as_non_null();
            // Every created resource is guarded immediately. An error or
            // unwind runs the correct native destructor before unloading lib.
            let queue = OwnedWaylandHandle::from_native(create_queue(display), queue_destroy)
                .ok_or("Cannot create Wayland event queue")?;
            let registry_raw = {
                let wrapper =
                    OwnedWaylandHandle::from_native(create_wrapper(display), wrapper_destroy)
                        .ok_or("Cannot wrap Wayland display")?;
                set_queue(wrapper.raw, queue.raw);
                marshal(
                    wrapper.raw,
                    1,
                    Some(registry_interface),
                    1,
                    0,
                    ptr::null_mut::<c_void>(),
                )
            };
            let registry = OwnedWaylandHandle::from_native(registry_raw, destroy)
                .ok_or("No Wayland registry")?;
            let mut globals = Box::new(Globals::default());
            if add_listener(registry.raw, &LISTENER, NonNull::from(globals.as_mut())) != 0
                || roundtrip(display, queue.raw) < 0
                || globals.compositor == 0
            {
                // Never destroy the callback's userdata before its registry.
                drop(registry);
                return Err("Cannot bind Wayland compositor".into());
            }
            let compositor_raw = marshal(
                registry.raw,
                0,
                Some(compositor_interface),
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
        let surface = w.wayland_surface().ok_or("No Wayland surface")?;
        // wl_region coordinates are surface-local logical units, not
        // physical pixels like XShape rectangles. Compute and validate
        // the region entirely in safe Rust before crossing the FFI boundary.
        let size = w.size().map(|v| v.ceil() as i32);
        let hole = if pass {
            None
        } else {
            hole.and_then(|area| area.clipped(size))
        };
        let applied = WaylandApplied {
            surface: surface.as_non_null(),
            pass,
            hole,
            size,
        };
        if self.applied == Some(applied) {
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
                    self.compositor.raw,
                    1,
                    Some(self.region_interface),
                    (self.version)(self.compositor.raw),
                    0,
                    ptr::null_mut::<c_void>(),
                );
                // SAFETY: wl_compositor.create_region returns an owned
                // wl_region, released by this guard after the surface commit.
                let region = OwnedWaylandRegion::from_native(p, self.marshal, self.version)
                    .ok_or("Cannot create input region")?;
                if let Some(hole) = hole {
                    // wl_region.add = opcode 1; wl_region.subtract = opcode 2.
                    (self.marshal)(
                        region.raw,
                        1,
                        None,
                        (self.version)(region.raw),
                        0,
                        0_i32,
                        0_i32,
                        size[0],
                        size[1],
                    );
                    (self.marshal)(
                        region.raw,
                        2,
                        None,
                        (self.version)(region.raw),
                        0,
                        hole.x,
                        hole.y,
                        hole.w,
                        hole.h,
                    );
                }
                Some(region)
            } else {
                None
            };
            let region_ptr = region
                .as_ref()
                .map_or(ptr::null_mut(), |region| region.raw.as_ptr());
            (self.marshal)(
                surface.as_non_null(),
                5,
                None,
                (self.version)(surface.as_non_null()),
                0,
                region_ptr,
            ); // wl_surface.set_input_region
            (self.marshal)(
                surface.as_non_null(),
                6,
                None,
                (self.version)(surface.as_non_null()),
                0,
            ); // wl_surface.commit
            // The region is destroyed through RAII, even if a Rust unwind
            // occurs between creation and the surface commit.
        }
        self.applied = Some(applied);
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
// All Xlib display handles come from a checked, live SDL window. The
// rectangles remain borrowed through the synchronous XShape call; None
// represents the null pointer used for an empty shape.
type CombineRectangles = unsafe extern "C" fn(
    NonNull<c_void>,
    c_ulong,
    c_int,
    c_int,
    c_int,
    Option<NonNull<Rectangle>>,
    c_int,
    c_int,
    c_int,
);
type CombineMask =
    unsafe extern "C" fn(NonNull<c_void>, c_ulong, c_int, c_int, c_int, c_ulong, c_int);

#[derive(Clone, Copy, PartialEq, Eq)]
struct InputRegion {
    display: NonNull<c_void>,
    window: c_ulong,
    size: [i32; 2],
    pass: bool,
    hole: Option<Rect>,
}

struct X11 {
    rectangles: CombineRectangles,
    mask: CombineMask,
    raise: unsafe extern "C" fn(NonNull<c_void>, c_ulong) -> c_int,
    flush: unsafe extern "C" fn(NonNull<c_void>) -> c_int,
    applied: Option<InputRegion>,
}

/// Only known X11 window actions may cross the dynamic Xlib FFI boundary.
enum X11Command<'a> {
    Shape(Option<&'a [Rectangle]>),
    Raise,
}

fn x11_window(w: &Window) -> Result<(NativeWindowHandle<'_>, c_ulong), String> {
    let display = w.x11_display().ok_or("No X11 window")?;
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
                    (self.rectangles)(
                        display.as_non_null(),
                        window,
                        2,
                        0,
                        0,
                        (!rectangles.is_empty())
                            .then(|| NonNull::from(rectangles).cast::<Rectangle>()),
                        count,
                        0,
                        0,
                    );
                }
                X11Command::Shape(None) => {
                    (self.mask)(display.as_non_null(), window, 2, 0, 0, 0, 0);
                }
                X11Command::Raise => {
                    // Do not transfer keyboard focus away from the canvas.
                    (self.raise)(display.as_non_null(), window);
                }
            }
            (self.flush)(display.as_non_null());
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
            let query: unsafe extern "C" fn(NonNull<c_void>, &mut c_int, &mut c_int) -> c_int =
                *ext.get(b"XShapeQueryExtension\0")
                    .map_err(|e| e.to_string())?;
            let (mut event, mut error) = (0, 0);
            if query(display.as_non_null(), &mut event, &mut error) == 0 {
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
            display: display.as_non_null(),
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
        // At most four regions surround the toolbar. Keep the native
        // XShape buffer on the stack instead of allocating a second Vec.
        let mut rectangles: [Rectangle; 4] = std::array::from_fn(|_| Rectangle {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
        let mut count = 0;
        if hole.is_some() {
            for area in crate::controller::outside(size, hole) {
                let next = rectangles
                    .get_mut(count)
                    .ok_or("Too many X11 input regions")?;
                *next = Rectangle {
                    x: area.x.try_into().map_err(|_| "Input region is too large")?,
                    y: area.y.try_into().map_err(|_| "Input region is too large")?,
                    width: area.w.try_into().map_err(|_| "Input region is too large")?,
                    height: area.h.try_into().map_err(|_| "Input region is too large")?,
                };
                count += 1;
            }
        }
        let shape = (pass || hole.is_some()).then_some(&rectangles[..count]);
        self.dispatch(w, X11Command::Shape(shape))?;
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
        ptr::NonNull,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    static TEST_HANDLE: Mutex<Option<Box<DropCounter>>> = Mutex::new(None);

    extern "C" fn release_test_handle(raw: NonNull<c_void>) {
        let mut slot = TEST_HANDLE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_mut()
            .is_some_and(|resource| NonNull::from(resource.as_mut()).cast() == raw)
        {
            // The matching Rust-owned allocation is released exactly once.
            drop(slot.take());
        }
    }

    #[test]
    fn owned_wayland_handle_releases_exactly_once() {
        let releases = Arc::new(AtomicUsize::new(0));
        let raw = {
            let mut registry = TEST_HANDLE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Form the native identity after the Box moves into the registry.
            NonNull::from(
                registry
                    .insert(Box::new(DropCounter(Arc::clone(&releases))))
                    .as_mut(),
            )
            .cast()
        };
        // This registered Rust allocation has a stable address and a
        // known matching destructor, so no foreign ownership assumption
        // is needed to exercise the native guard's Drop implementation.
        let owned = OwnedWaylandHandle {
            raw,
            destroy: release_test_handle,
        };
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        drop(owned);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }
}
