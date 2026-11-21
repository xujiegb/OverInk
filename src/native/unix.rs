use super::*;
use crate::controller::Rect;
use libloading::Library;
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    ptr::{self, NonNull},
    rc::Rc,
};
use x11rb::rust_connection::RustConnection;

fn wayland_library() -> Result<Library, String> {
    for name in ["libwayland-client.so.0", "libwayland-client.so"] {
        // SAFETY: Both names refer to the supported Wayland client ABI.
        // Wayland keeps the library loaded while its symbols are in use.
        if let Ok(lib) = unsafe { Library::new(name) } {
            return Ok(lib);
        }
    }
    Err("Missing native library: libwayland-client.so.0".into())
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
            let lib = wayland_library()?;
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
                marshal,
                version,
                _lib: lib,
            })
        }
    }
    fn set(&self, w: &Window, pass: bool, hole: Option<Rect>) -> Result<(), String> {
        let surface = w.wayland_surface().ok_or("No Wayland surface")?;
        // wl_region coordinates are surface-local logical units, not
        // physical pixels like XShape rectangles. Compute and validate
        // the region entirely in safe Rust before crossing the FFI boundary.
        let size = w.size().map(|v| v.ceil() as i32);
        let hole = hole.and_then(|area| area.clipped(size));
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
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct InputRegion {
    window: SDL_WindowID,
    pass: bool,
    hole: Option<Rect>,
}

enum Backend {
    X11(Rc<RustConnection>),
    Wayland(Wayland),
    Unavailable(String),
}

pub struct Passthrough {
    backend: Backend,
    applied: Option<InputRegion>,
}
impl Passthrough {
    pub fn new(w: &Window) -> Self {
        let backend = match crate::sdl::driver().as_str() {
            "x11" => crate::x11::shape_connection().map(Backend::X11),
            "wayland" => Wayland::new(w).map(Backend::Wayland),
            _ => Err("Input passthrough needs X11 or Wayland".into()),
        };
        Self {
            backend: backend.unwrap_or_else(Backend::Unavailable),
            applied: None,
        }
    }
    pub fn set_guarded(
        &mut self,
        w: &Window,
        pass: bool,
        hole: Option<Rect>,
    ) -> Result<(), String> {
        let hole = if pass { None } else { hole };
        let region = InputRegion {
            window: w.id,
            pass,
            hole,
        };
        if self.applied == Some(region) {
            return Ok(());
        }
        match &self.backend {
            Backend::X11(connection) => {
                let id = w.x11_window_number().ok_or("No X11 window")?;
                let size = w.pixel_size();
                let hole = hole.and_then(|area| area.pixels(w).clipped(size));
                crate::x11::set_input(connection, id, size, pass, hole)?;
            }
            Backend::Wayland(wayland) => wayland.set(w, pass, hole)?,
            Backend::Unavailable(error) if pass || hole.is_some() => return Err(error.clone()),
            Backend::Unavailable(_) => {}
        }
        self.applied = Some(region);
        Ok(())
    }
    pub fn invalidate(&mut self) {
        self.applied = None;
    }
    pub fn raise_controller(&self, w: &Window) {
        if let Backend::X11(connection) = &self.backend
            && let Some(id) = w.x11_window_number()
        {
            crate::x11::raise(connection, id);
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
