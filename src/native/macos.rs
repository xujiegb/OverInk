use crate::sdl::*;
use objc2::{Encoding, RefEncode, msg_send, runtime::AnyObject};
use std::ffi::c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

// The raw pointer also keeps the pool on the thread that created it.
pub struct AutoreleasePool(*mut c_void);

impl AutoreleasePool {
    pub fn new() -> Self {
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

pub fn configure_metal(raw: *mut SDL_Renderer, extended: bool) {
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
        static kCGColorSpaceSRGB: *const c_void;
        fn CGColorSpaceCreateWithName(name: *const c_void) -> *const ColorSpace;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
    }
    unsafe {
        let layer = SDL_GetRenderMetalLayer(raw).cast::<AnyObject>();
        if layer.is_null() {
            return;
        }
        let _: () = msg_send![layer, setMaximumDrawableCount: 2_usize];
        if !extended {
            let srgb = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            if !srgb.is_null() {
                let _: () = msg_send![layer, setColorspace: srgb];
                CFRelease(srgb.cast());
            }
        }
    }
}

pub fn remove_metal_view(window: &Window) {
    unsafe {
        let props = SDL_GetWindowProperties(window.raw);
        let tag = SDL_GetNumberProperty(props, SDL_PROP_WINDOW_COCOA_METAL_VIEW_TAG_NUMBER, 0);
        let ns = SDL_GetPointerProperty(
            props,
            SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
            std::ptr::null_mut(),
        )
        .cast::<AnyObject>();
        if tag == 0 || ns.is_null() {
            return;
        }
        let content: *mut AnyObject = msg_send![ns, contentView];
        let metal: *mut AnyObject = msg_send![content, viewWithTag: tag as isize];
        if !metal.is_null() {
            let _: () = msg_send![metal, removeFromSuperview];
        }
    }
}
