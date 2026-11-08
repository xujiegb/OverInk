use super::Image;
use crate::sdl::Window;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn capture(window: &Window) -> Result<Image, String> {
    crate::picker::desktop::capture(window)
}
#[cfg(target_os = "macos")]
pub fn capture(_window: &Window) -> Result<Image, String> {
    Err("Use the system screenshot service on macOS".into())
}
#[cfg(target_os = "windows")]
pub fn capture(window: &Window) -> Result<Image, String> {
    use std::{ffi::c_void, ptr};
    #[repr(C)]
    struct Header {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bits: u16,
        compression: u32,
        image_size: u32,
        xppm: i32,
        yppm: i32,
        used: u32,
        important: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDC(w: *mut c_void) -> *mut c_void;
        fn ReleaseDC(w: *mut c_void, dc: *mut c_void) -> i32;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
        fn CreateCompatibleBitmap(dc: *mut c_void, w: i32, h: i32) -> *mut c_void;
        fn SelectObject(dc: *mut c_void, object: *mut c_void) -> *mut c_void;
        fn BitBlt(
            dst: *mut c_void,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            src: *mut c_void,
            sx: i32,
            sy: i32,
            rop: u32,
        ) -> i32;
        fn GetDIBits(
            dc: *mut c_void,
            bitmap: *mut c_void,
            start: u32,
            lines: u32,
            bits: *mut c_void,
            info: *mut Header,
            usage: u32,
        ) -> i32;
        fn DeleteDC(dc: *mut c_void) -> i32;
        fn DeleteObject(object: *mut c_void) -> i32;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmFlush() -> i32;
    }
    let origin = crate::picker::desktop::position(window, [0.; 2])?;
    let [w, h] = window.pixel_size();
    let mut image = Image::buffer(w as u32, h as u32)?;
    unsafe {
        DwmFlush();
        let screen = GetDC(ptr::null_mut());
        if screen.is_null() {
            return Err("Cannot access the desktop".into());
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        let result = (|| {
            if memory.is_null() || bitmap.is_null() {
                return Err("Cannot allocate a desktop capture".into());
            }
            let old = SelectObject(memory, bitmap);
            let copied = BitBlt(
                memory,
                0,
                0,
                w,
                h,
                screen,
                origin.x,
                origin.y,
                0x00CC0020 | 0x40000000,
            );
            SelectObject(memory, old);
            if copied == 0 {
                return Err("Cannot capture the desktop".into());
            }
            let mut header = Header {
                size: 40,
                width: w,
                height: -h,
                planes: 1,
                bits: 32,
                compression: 0,
                image_size: 0,
                xppm: 0,
                yppm: 0,
                used: 0,
                important: 0,
            };
            if GetDIBits(
                memory,
                bitmap,
                0,
                h as u32,
                image.rgba.as_mut_ptr().cast(),
                &mut header,
                0,
            ) != h
            {
                return Err("Cannot read the desktop capture".into());
            }
            for pixel in image.rgba.as_chunks_mut::<4>().0.iter_mut() {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            Ok(image)
        })();
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        if !memory.is_null() {
            DeleteDC(memory);
        }
        ReleaseDC(ptr::null_mut(), screen);
        result
    }
}
