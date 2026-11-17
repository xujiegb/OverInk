use crate::{color::InkColor, sdl::*};

#[derive(Clone, Copy, Debug)]
pub struct ScreenPoint {
    pub x: i32,
    pub y: i32,
    pub root: std::ffi::c_ulong,
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "freebsd"))]
fn local_pixels(window: &Window, local: [f32; 2]) -> Result<[i32; 2], String> {
    scaled_pixels(local, window.size(), window.pixel_size())
}

/// Reject zero-sized/minimized windows and out-of-range coordinates before
/// narrowing into the 32-bit pixel coordinates required by Win32 and X11.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "freebsd"))]
fn scaled_pixels(
    local: [f32; 2],
    logical: [f32; 2],
    physical: [i32; 2],
) -> Result<[i32; 2], String> {
    let mut pixels = [0; 2];
    for i in 0..2 {
        if !local[i].is_finite()
            || !logical[i].is_finite()
            || logical[i] <= 0.
            || physical[i] <= 0
        {
            return Err("Invalid screen coordinate or window size".into());
        }
        let coordinate = (local[i] * physical[i] as f32 / logical[i]).floor();
        if !coordinate.is_finite() || !(i32::MIN as f32..2_147_483_648.).contains(&coordinate) {
            return Err("Screen coordinate is out of range".into());
        }
        pixels[i] = coordinate as i32;
    }
    Ok(pixels)
}

#[cfg(target_os = "windows")]
pub fn position(window: &Window, local: [f32; 2]) -> Result<ScreenPoint, String> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn ClientToScreen(window: *mut std::ffi::c_void, point: *mut Point) -> i32;
    }
    let [x, y] = local_pixels(window, local)?;
    let mut point = Point { x, y };
    let hwnd = window.win32_hwnd().ok_or("No Win32 window handle")?;
    // SAFETY: The HWND is borrowed from the still-live SDL window, and the
    // output POINT is valid and writable for the duration of this call.
    if unsafe { ClientToScreen(hwnd.as_ptr(), &mut point) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(ScreenPoint {
        x: point.x,
        y: point.y,
        root: 0,
    })
}
#[cfg(target_os = "windows")]
pub fn sample(_window: &Window, point: ScreenPoint) -> Result<InkColor, String> {
    use std::{ffi::c_void, ptr};
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDC(window: *mut c_void) -> *mut c_void;
        fn ReleaseDC(window: *mut c_void, dc: *mut c_void) -> i32;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetPixel(dc: *mut c_void, x: i32, y: i32) -> u32;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmFlush() -> i32;
    }
    unsafe {
        // Both OverInk windows have been hidden before this readback. GDI returns
        // the system's SDR RGB representation, not the source scene's HDR values.
        DwmFlush();
        let dc = GetDC(ptr::null_mut());
        if dc.is_null() {
            return Err("Cannot read the desktop".into());
        }
        let pixel = GetPixel(dc, point.x, point.y);
        ReleaseDC(ptr::null_mut(), dc);
        if pixel == u32::MAX {
            return Err("The desktop pixel is unavailable".into());
        }
        let _ = point.root;
        Ok(InkColor::from_rgb8([
            pixel as u8,
            (pixel >> 8) as u8,
            (pixel >> 16) as u8,
        ]))
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod x11 {
    use super::*;
    use libloading::{Library, Symbol};
    use std::ffi::{c_char, c_int, c_ulong, c_void};
    fn library() -> Result<Library, String> {
        for name in ["libX11.so.6", "libX11.so"] {
            if let Ok(lib) = unsafe { Library::new(name) } {
                return Ok(lib);
            }
        }
        Err("X11 screen capture is unavailable".into())
    }
    fn display(window: &Window) -> Result<*mut c_void, String> {
        window
            .x11_display()
            .map(|display| display.as_ptr())
            .ok_or_else(|| "No X11 display for screen picking".into())
    }
    pub fn is_xwayland(window: &Window) -> bool {
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
            || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        {
            return true;
        }
        unsafe {
            let Ok(lib) = library() else { return false };
            let Ok(display) = display(window) else {
                return false;
            };
            let query = lib.get::<unsafe extern "C" fn(
                *mut c_void,
                *const c_char,
                *mut c_int,
                *mut c_int,
                *mut c_int,
            ) -> c_int>(b"XQueryExtension\0");
            let (mut opcode, mut event, mut error) = (0, 0, 0);
            query.is_ok_and(|query| {
                query(
                    display,
                    c"XWAYLAND".as_ptr(),
                    &mut opcode,
                    &mut event,
                    &mut error,
                ) != 0
            })
        }
    }
    pub fn position(window: &Window, local: [f32; 2]) -> Result<ScreenPoint, String> {
        unsafe {
            let lib = library()?;
            let d = display(window)?;
            let id = window.x11_window_number();
            let geometry: Symbol<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    *mut c_ulong,
                    *mut c_int,
                    *mut c_int,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                ) -> c_int,
            > = lib.get(b"XGetGeometry\0").map_err(|e| e.to_string())?;
            let (mut root, mut x, mut y, mut width, mut height, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            if geometry(
                d,
                id,
                &mut root,
                &mut x,
                &mut y,
                &mut width,
                &mut height,
                &mut border,
                &mut depth,
            ) == 0
            {
                return Err("Cannot locate the canvas on the display".into());
            }
            let translate: Symbol<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    c_ulong,
                    c_int,
                    c_int,
                    *mut c_int,
                    *mut c_int,
                    *mut c_ulong,
                ) -> c_int,
            > = lib
                .get(b"XTranslateCoordinates\0")
                .map_err(|e| e.to_string())?;
            let [px, py] = local_pixels(window, local)?;
            let mut child = 0;
            if translate(d, id, root, px, py, &mut x, &mut y, &mut child) == 0 {
                return Err("Cannot convert the selected screen coordinate".into());
            }
            Ok(ScreenPoint { x, y, root })
        }
    }
    #[repr(C)]
    struct Image {
        width: c_int,
        height: c_int,
        xoffset: c_int,
        format: c_int,
        data: *mut c_char,
        byte_order: c_int,
        bitmap_unit: c_int,
        bitmap_bit_order: c_int,
        bitmap_pad: c_int,
        depth: c_int,
        bytes_per_line: c_int,
        bits_per_pixel: c_int,
        red_mask: c_ulong,
        green_mask: c_ulong,
        blue_mask: c_ulong,
    }
    /// Own an XGetImage result while retaining the library that supplied its
    /// XDestroyImage function. Every early return now releases the XImage.
    struct OwnedImage<'lib> {
        raw: *mut Image,
        destroy: Symbol<'lib, unsafe extern "C" fn(*mut Image) -> c_int>,
    }

    impl<'lib> OwnedImage<'lib> {
        fn new(
            raw: *mut Image,
            destroy: Symbol<'lib, unsafe extern "C" fn(*mut Image) -> c_int>,
            message: &'static str,
        ) -> Result<Self, String> {
            if raw.is_null() {
                return Err(message.into());
            }
            Ok(Self { raw, destroy })
        }

        fn masks(&self) -> [c_ulong; 3] {
            // SAFETY: XGetImage supplied a live non-null XImage, whose
            // prefix matches Image. The guard owns it until Drop.
            unsafe {
                [
                    (*self.raw).red_mask,
                    (*self.raw).green_mask,
                    (*self.raw).blue_mask,
                ]
            }
        }
    }

    impl Drop for OwnedImage<'_> {
        fn drop(&mut self) {
            // SAFETY: This non-null XImage is owned exclusively by the guard.
            // The borrowed Symbol also keeps the X11 library loaded.
            unsafe {
                (self.destroy)(self.raw);
            }
        }
    }

    pub fn sample(window: &Window, point: ScreenPoint) -> Result<InkColor, String> {
        unsafe {
            let lib = library()?;
            let d = display(window)?;
            let geometry: Symbol<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    *mut c_ulong,
                    *mut c_int,
                    *mut c_int,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                ) -> c_int,
            > = lib.get(b"XGetGeometry\0").map_err(|e| e.to_string())?;
            let (mut root, mut x, mut y, mut width, mut height, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            if geometry(
                d,
                point.root,
                &mut root,
                &mut x,
                &mut y,
                &mut width,
                &mut height,
                &mut border,
                &mut depth,
            ) == 0
                || point.x < 0
                || point.y < 0
                || point.x as u32 >= width
                || point.y as u32 >= height
            {
                return Err("Screen coordinate is out of bounds".into());
            }
            let get: Symbol<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    c_int,
                    c_int,
                    u32,
                    u32,
                    c_ulong,
                    c_int,
                ) -> *mut Image,
            > = lib.get(b"XGetImage\0").map_err(|e| e.to_string())?;
            let pixel: Symbol<unsafe extern "C" fn(*mut Image, c_int, c_int) -> c_ulong> =
                lib.get(b"XGetPixel\0").map_err(|e| e.to_string())?;
            let destroy: Symbol<unsafe extern "C" fn(*mut Image) -> c_int> =
                lib.get(b"XDestroyImage\0").map_err(|e| e.to_string())?;
            let image = OwnedImage::new(
                get(d, point.root, point.x, point.y, 1, 1, c_ulong::MAX, 2),
                destroy,
                "Cannot read the selected desktop pixel",
            )?;
            let masks = image.masks();
            let value = pixel(image.raw, 0, 0);
            let mut rgb = [0.; 3];
            for (i, mask) in masks.into_iter().enumerate() {
                rgb[i] = channel(value, mask)?;
            }
            Ok(crate::icc::display_sample_to_srgb(window, rgb))
        }
    }
    fn channel(pixel: c_ulong, mask: c_ulong) -> Result<f32, String> {
        if mask == 0 {
            return Err("Screen picking requires a TrueColor visual".into());
        }
        let shift = mask.trailing_zeros();
        let max = mask >> shift;
        Ok(((pixel & mask) >> shift) as f32 / max as f32)
    }
    pub fn capture(window: &Window) -> Result<crate::background::Image, String> {
        let origin = position(window, [0.; 2])?;
        let [w, h] = window.pixel_size();
        let mut out = crate::background::Image::buffer(w as u32, h as u32)?;
        unsafe {
            let lib = library()?;
            let d = display(window)?;
            let geometry: Symbol<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    *mut c_ulong,
                    *mut c_int,
                    *mut c_int,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                ) -> c_int,
            > = lib.get(b"XGetGeometry\0").map_err(|e| e.to_string())?;
            let (mut root, mut x, mut y, mut rw, mut rh, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            if geometry(
                d,
                origin.root,
                &mut root,
                &mut x,
                &mut y,
                &mut rw,
                &mut rh,
                &mut border,
                &mut depth,
            ) == 0
                || origin.x < 0
                || origin.y < 0
                || origin.x as i64 + w as i64 > rw as i64
                || origin.y as i64 + h as i64 > rh as i64
            {
                return Err("Capture is outside the connected display bounds".into());
            }
            let get: Symbol<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    c_int,
                    c_int,
                    u32,
                    u32,
                    c_ulong,
                    c_int,
                ) -> *mut Image,
            > = lib.get(b"XGetImage\0").map_err(|e| e.to_string())?;
            let pixel: Symbol<unsafe extern "C" fn(*mut Image, c_int, c_int) -> c_ulong> =
                lib.get(b"XGetPixel\0").map_err(|e| e.to_string())?;
            let destroy: Symbol<unsafe extern "C" fn(*mut Image) -> c_int> =
                lib.get(b"XDestroyImage\0").map_err(|e| e.to_string())?;
            let image = OwnedImage::new(
                get(
                    d,
                    origin.root,
                    origin.x,
                    origin.y,
                    w as u32,
                    h as u32,
                    c_ulong::MAX,
                    2,
                ),
                destroy,
                "Cannot capture the desktop",
            )?;
            let masks = image.masks();
            if masks.contains(&0) {
                return Err("Desktop capture requires a TrueColor visual".into());
            }
            for y in 0..h {
                for x in 0..w {
                    let value = pixel(image.raw, x, y);
                    let offset = (y as usize * w as usize + x as usize) * 4;
                    for (i, mask) in masks.into_iter().enumerate() {
                        out.rgba[offset + i] = (channel(value, mask)? * 255.).round() as u8;
                    }
                    out.rgba[offset + 3] = 255;
                }
            }
        }
        // Keep SDL-owned ICC bytes out of the X11 unsafe readback region.
        // The Window adapter copies the profile and frees SDL's allocation.
        if let Some(profile) = window.icc_profile() {
            out.convert_to_srgb(&profile);
        }
        Ok(out)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn decodes_eight_ten_and_sixteen_bit_visuals() {
            assert_eq!(channel(0x12abef, 0xff0000).unwrap(), 18. / 255.);
            assert_eq!(channel(0x12abef, 0xff00).unwrap(), 171. / 255.);
            assert_eq!(channel(0xffff, 0xf800).unwrap(), 1.);
            assert_eq!(channel(0x3ff00000, 0x3ff00000).unwrap(), 1.);
            assert!(channel(0, 0).is_err());
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use x11::{capture, is_xwayland, position, sample};

#[cfg(all(test, any(target_os = "windows", target_os = "linux", target_os = "freebsd")))]
mod coordinate_tests {
    use super::scaled_pixels;

    #[test]
    fn scales_and_rejects_invalid_window_dimensions() {
        assert_eq!(scaled_pixels([10.5, 11.], [100., 100.], [200, 150]), Ok([21, 16]));
        assert!(scaled_pixels([3., 4.], [0., 100.], [200, 200]).is_err());
        assert!(scaled_pixels([3., 4.], [100., 100.], [0, 200]).is_err());
        assert!(scaled_pixels([f32::NAN, 4.], [100., 100.], [200, 200]).is_err());
        assert!(scaled_pixels([f32::MAX, 4.], [100., 100.], [200, 200]).is_err());
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "freebsd")))]
pub fn position(_window: &Window, _local: [f32; 2]) -> Result<ScreenPoint, String> {
    Err("Use the system color sampler on this platform".into())
}
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "freebsd")))]
pub fn sample(_window: &Window, point: ScreenPoint) -> Result<InkColor, String> {
    let _ = (point.x, point.y, point.root);
    Err("Use the system color sampler on this platform".into())
}
