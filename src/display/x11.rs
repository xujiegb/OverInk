//! Pure-Rust XRandR topology query; no Xlib/XRandR FFI or native pointers.
//! SDL owns window and drawing state. A separate protocol connection reads
//! output geometry to filter displays that SDL still reports after disconnect.
#![forbid(unsafe_code)]

use x11rb::{
    connection::Connection as _,
    protocol::randr::ConnectionExt as _,
    rust_connection::RustConnection,
};

const MAX_X11_SCREENS: usize = 256;
const MAX_RANDR_CRTCS: usize = 4096;

fn bounded_count(count: usize, max: usize) -> bool {
    count <= max
}

/// Return a display rectangle only for a currently active video mode.
fn active_crtc_area(mode: u32, x: i16, y: i16, width: u16, height: u16) -> Option<[i32; 4]> {
    (mode != 0 && width != 0 && height != 0).then_some([
        i32::from(x),
        i32::from(y),
        i32::from(width),
        i32::from(height),
    ])
}

/// Owned X11 protocol connection; Rust drops its resources without a native
/// destructor or a caller-managed foreign allocation.
pub struct Randr {
    connection: RustConnection,
}

impl Randr {
    pub fn get() -> Option<Self> {
        let (connection, _) = x11rb::connect(None).ok()?;
        Some(Self { connection })
    }

    /// An incomplete query must not hide outputs: return None to let SDL's
    /// complete display list be used unchanged. A confirmed empty active
    /// result, however, filters out stale SDL outputs.
    pub fn active_areas(&self) -> Option<Vec<[i32; 4]>> {
        let screens = &self.connection.setup().roots;
        if screens.is_empty() || !bounded_count(screens.len(), MAX_X11_SCREENS) {
            return None;
        }

        let mut areas = Vec::new();
        let mut queried = false;
        for screen in screens {
            let resources = self
                .connection
                .randr_get_screen_resources_current(screen.root)
                .ok()?
                .reply()
                .ok()?;
            if !bounded_count(resources.crtcs.len(), MAX_RANDR_CRTCS) {
                return None;
            }
            queried |= !resources.crtcs.is_empty();
            for crtc in resources.crtcs {
                let info = self
                    .connection
                    .randr_get_crtc_info(crtc, resources.config_timestamp)
                    .ok()?
                    .reply()
                    .ok()?;
                // A stale configuration timestamp must not make the query
                // authoritative: fall back to SDL rather than hide outputs.
                if info.status != x11rb::protocol::randr::SetConfig::SUCCESS {
                    return None;
                }
                if let Some(area) =
                    active_crtc_area(info.mode, info.x, info.y, info.width, info.height)
                {
                    areas.push(area);
                }
            }
        }
        queried.then_some(areas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_implausible_x11_counts() {
        assert!(bounded_count(0, MAX_X11_SCREENS));
        assert!(bounded_count(256, MAX_X11_SCREENS));
        assert!(!bounded_count(257, MAX_X11_SCREENS));
        assert!(bounded_count(4096, MAX_RANDR_CRTCS));
        assert!(!bounded_count(4097, MAX_RANDR_CRTCS));
    }

    #[test]
    fn only_active_randr_modes_have_screen_rectangles() {
        assert_eq!(active_crtc_area(0, 1, 2, 100, 200), None);
        assert_eq!(active_crtc_area(7, 1, 2, 0, 200), None);
        assert_eq!(active_crtc_area(7, 1, 2, 100, 0), None);
        assert_eq!(
            active_crtc_area(7, i16::MIN, i16::MAX, u16::MAX, 800),
            Some([i32::from(i16::MIN), i32::from(i16::MAX), i32::from(u16::MAX), 800])
        );
    }
}
