mod direct_touch;
mod pencil;
pub use direct_touch::DirectTouchMonitor;
pub(crate) use direct_touch::{
    overink_macos_create_overlay, overink_macos_destroy_overlay, overink_macos_order_toolbar,
    overink_macos_overlay_crosshair, overink_macos_raise_overlay,
};
pub use pencil::PencilModeMonitor;

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
    let Some(tag) = window.cocoa_metal_view_tag() else {
        return;
    };
    let Some(ns) = window.cocoa_window() else {
        return;
    };
    // SAFETY: The NSWindow is borrowed from the live SDL window and the
    // selectors use their Cocoa object-pointer and NSInteger signatures.
    // Check the intermediate contentView before messaging it.
    unsafe {
        let ns = ns.as_non_null().as_ptr().cast::<AnyObject>();
        let content: Option<NonNull<AnyObject>> = NonNull::new(msg_send![ns, contentView]);
        let Some(content) = content else {
            return;
        };
        let metal: Option<NonNull<AnyObject>> =
            NonNull::new(msg_send![content.as_ptr(), viewWithTag: tag]);
        if let Some(metal) = metal {
            let _: () = msg_send![metal.as_ptr(), removeFromSuperview];
        }
    }
}
