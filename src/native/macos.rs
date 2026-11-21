mod direct_touch;
mod pencil;
pub use direct_touch::DirectTouchMonitor;
pub use pencil::PencilModeMonitor;

use crate::sdl::*;
use objc2::{Encoding, RefEncode, msg_send, runtime::AnyObject};
use objc2_core_foundation::{CFRetained, CFType};
use std::{ffi::c_void, ptr::NonNull};

pub fn configure_metal(raw: NonNull<SDL_Renderer>, extended: bool) {
    #[repr(C)]
    struct ColorSpace {
        _opaque: [u8; 0],
    }
    // Core Graphics exposes only a pointer to the opaque CGColorSpace.
    unsafe impl RefEncode for ColorSpace {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColorSpace", &[]));
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        static kCGColorSpaceSRGB: Option<NonNull<c_void>>;
        fn CGColorSpaceCreateWithName(name: NonNull<c_void>) -> Option<NonNull<CFType>>;
    }
    unsafe {
        let layer = SDL_GetRenderMetalLayer(raw.as_ptr()).cast::<AnyObject>();
        if layer.is_null() {
            return;
        }
        let _: () = msg_send![layer, setMaximumDrawableCount: 2_usize];
        if !extended
            && let Some(name) = kCGColorSpaceSRGB
            && let Some(srgb) = CGColorSpaceCreateWithName(name)
        {
            let srgb = CFRetained::from_raw(srgb);
            let color_space = CFRetained::as_ptr(&srgb).cast::<ColorSpace>();
            let _: () = msg_send![layer, setColorspace: color_space.as_ptr()];
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
