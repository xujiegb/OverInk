use crate::sdl::*;
use objc2::{Encoding, RefEncode, msg_send, runtime::AnyObject};
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
        fn CGColorSpaceCreateWithName(name: NonNull<c_void>) -> Option<NonNull<c_void>>;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: NonNull<c_void>);
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
            let _: () = msg_send![layer, setColorspace: srgb.as_ptr().cast::<ColorSpace>()];
            CFRelease(srgb);
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
        let content: *mut AnyObject = msg_send![ns, contentView];
        if content.is_null() {
            return;
        }
        let metal: *mut AnyObject = msg_send![content, viewWithTag: tag];
        if !metal.is_null() {
            let _: () = msg_send![metal, removeFromSuperview];
        }
    }
}
