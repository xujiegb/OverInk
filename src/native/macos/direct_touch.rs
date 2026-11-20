//! macOS 27 native direct-finger adapter.
//!
//! The Objective-C side owns NSTouch/NSView and its recognizer. No Cocoa
//! object or raw pointer escapes into the application or SDL event model.
use crate::sdl::{self, Window};

unsafe extern "C" {
    safe fn overink_macos_install_touch(window_id: u32) -> i32;
    safe fn overink_macos_uninstall_touch(window_id: u32);
}

pub struct DirectTouchMonitor {
    window_id: u32,
}

impl DirectTouchMonitor {
    pub fn install(window: &Window) -> Result<Option<Self>, String> {
        // The scalar-only bridge validates the AppKit thread and SDL window ID
        // before it retrieves Cocoa objects; no native pointers enter Rust.
        let installed = overink_macos_install_touch(window.id.0);
        match installed {
            1 => Ok(Some(Self {
                window_id: window.id.0,
            })),
            0 => Ok(None), // macOS before 27.
            _ => Err("Could not attach the macOS direct-touch recognizer".into()),
        }
    }
}

impl Drop for DirectTouchMonitor {
    fn drop(&mut self) {
        // The bridge ignores invalid IDs and calls outside the AppKit thread.
        overink_macos_uninstall_touch(self.window_id);
    }
}

/// Called synchronously by the AppKit gesture *action*, not by a worker.
/// The ABI contains only initialized scalars; SDL validates all samples.
#[unsafe(no_mangle)]
pub extern "C" fn overink_macos_touch_event(
    window_id: u32,
    finger_id: u64,
    phase: u32,
    x: f32,
    y: f32,
) {
    let _ = sdl::push_macos_direct_touch(window_id, finger_id, phase, x, y);
}
