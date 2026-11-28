mod direct_touch;
pub use direct_touch::{DirectTouchMonitor, PencilModeMonitor};
pub(crate) use direct_touch::{
    overink_macos_create_overlay, overink_macos_destroy_overlay, overink_macos_order_toolbar,
    overink_macos_overlay_crosshair, overink_macos_raise_overlay, overink_macos_set_passthrough,
};

use crate::sdl::*;
use objc2::{msg_send, runtime::AnyObject};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use std::ptr::NonNull;

pub fn configure_metal(raw: NonNull<SDL_Renderer>, extended: bool) {
    // SAFETY: The live SDL Metal renderer owns its CAMetalLayer. The layer
    // selectors use NSUInteger and the Core Graphics color-space type.
    unsafe {
        let layer = SDL_GetRenderMetalLayer(raw.as_ptr()).cast::<AnyObject>();
        if layer.is_null() {
            return;
        }
        let _: () = msg_send![layer, setMaximumDrawableCount: 2_usize];
        if !extended && let Some(srgb) = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)) {
            let _: () = msg_send![layer, setColorspace: &*srgb];
        }
    }
}

pub fn remove_metal_view(window: &Window) {
    if let Some(tag) = window.cocoa_metal_view_tag() {
        direct_touch::overink_macos_remove_metal_view(window.id.0, tag);
    }
}
