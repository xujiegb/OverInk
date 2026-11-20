#![deny(clippy::undocumented_unsafe_blocks)]

use crate::{color::InkColor, sdl::*};

#[derive(Clone, Copy, Debug)]
pub struct ScreenPoint {
    pub x: i32,
    pub y: i32,
    pub root: std::ffi::c_ulong,
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn local_pixels(window: &Window, local: [f32; 2]) -> Result<[i32; 2], String> {
    let (logical, physical) = window
        .capture_coordinate_sizes()
        .ok_or("Cannot pick from a zero-sized or unavailable window")?;
    scaled_pixels(local, logical, physical)
}

/// Reject zero-sized/minimized windows and out-of-range coordinates before
/// narrowing into the 32-bit pixel coordinates required by Win32 and X11.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn scaled_pixels(
    local: [f32; 2],
    logical: [f32; 2],
    physical: [i32; 2],
) -> Result<[i32; 2], String> {
    let mut pixels = [0; 2];
    for i in 0..2 {
        if !local[i].is_finite() || !logical[i].is_finite() || logical[i] <= 0. || physical[i] <= 0
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

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod x11 {
    use super::*;
    use libloading::{Library, Symbol};
    use std::ffi::{c_char, c_int, c_ulong, c_void};
    use std::ptr::NonNull;
    fn library() -> Result<Library, String> {
        crate::native::native_library(crate::native::NativeLibrary::X11)
            .map_err(|_| "X11 screen capture is unavailable".into())
    }
    fn display(window: &Window) -> Result<NativeWindowHandle<'_>, String> {
        window
            .x11_display()
            .ok_or_else(|| "No X11 display for screen picking".into())
    }
    pub fn is_xwayland(window: &Window) -> bool {
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
            || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        {
            return true;
        }
        // SAFETY: SDL owns the active X11 Display. The XQueryExtension symbol
        // is borrowed from a Library that remains live for this call, and
        // all of its output pointers refer to initialized local integers.
        unsafe {
            let Ok(lib) = library() else { return false };
            let Ok(display) = display(window) else {
                return false;
            };
            let query = lib.get::<unsafe extern "C" fn(
                NonNull<c_void>,
                NonNull<c_char>,
                &mut c_int,
                &mut c_int,
                &mut c_int,
            ) -> c_int>(b"XQueryExtension\0");
            let (mut opcode, mut event, mut error) = (0, 0, 0);
            let Some(extension) = NonNull::new(c"XWAYLAND".as_ptr().cast_mut()) else {
                return false;
            };
            query.is_ok_and(|query| {
                query(
                    display.as_non_null(),
                    extension,
                    &mut opcode,
                    &mut event,
                    &mut error,
                ) != 0
            })
        }
    }
    pub fn position(window: &Window, local: [f32; 2]) -> Result<ScreenPoint, String> {
        // SAFETY: SDL owns the live Display and drawable, and the Xlib
        // geometry/translation symbol signatures match their C declarations.
        // All writable output pointers refer to initialized local variables,
        // and the dynamic Library outlives every borrowed Symbol.
        unsafe {
            let lib = library()?;
            let d = display(window)?;
            let id = window.x11_window_number();
            let geometry: Symbol<
                unsafe extern "C" fn(
                    NonNull<c_void>,
                    c_ulong,
                    &mut c_ulong,
                    &mut c_int,
                    &mut c_int,
                    &mut u32,
                    &mut u32,
                    &mut u32,
                    &mut u32,
                ) -> c_int,
            > = lib.get(b"XGetGeometry\0").map_err(|e| e.to_string())?;
            let (mut root, mut x, mut y, mut width, mut height, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            if geometry(
                d.as_non_null(),
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
                    NonNull<c_void>,
                    c_ulong,
                    c_ulong,
                    c_int,
                    c_int,
                    &mut c_int,
                    &mut c_int,
                    &mut c_ulong,
                ) -> c_int,
            > = lib
                .get(b"XTranslateCoordinates\0")
                .map_err(|e| e.to_string())?;
            let [px, py] = local_pixels(window, local)?;
            let mut child = 0;
            if translate(
                d.as_non_null(),
                id,
                root,
                px,
                py,
                &mut x,
                &mut y,
                &mut child,
            ) == 0
            {
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
        data: Option<NonNull<c_char>>,
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
    // Validate XImage's row stride before passing image coordinates to
    // XGetPixel. Foreign metadata may not describe enough storage for a row.
    fn valid_row_stride(width: i32, xoffset: i32, bits_per_pixel: i32, stride: i32) -> bool {
        if width <= 0 || xoffset < 0 || !(1..=64).contains(&bits_per_pixel) || stride <= 0 {
            return false;
        }
        let Some(required_bits) = i64::from(width)
            .checked_add(i64::from(xoffset))
            .and_then(|pixels| pixels.checked_mul(i64::from(bits_per_pixel)))
            .and_then(|bits| bits.checked_add(7))
        else {
            return false;
        };
        required_bits / 8 <= i64::from(stride)
    }

    // The Xlib image constructor takes an exact fixed C ABI. Keeping the
    // symbol signature named prevents large inferred types in capture paths.
    type GetXImage = unsafe extern "C" fn(
        NonNull<c_void>,
        c_ulong,
        c_int,
        c_int,
        u32,
        u32,
        c_ulong,
        c_int,
    ) -> Option<NonNull<Image>>;

    /// Own an XGetImage result while retaining the library that supplied its
    /// XDestroyImage function. Every early return now releases the XImage.
    struct OwnedImage<'lib> {
        raw: NonNull<Image>,
        destroy: Symbol<'lib, unsafe extern "C" fn(NonNull<Image>) -> c_int>,
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

    /// Shared X11 image readback for both the color picker and screenshots.
    /// Exactly one native call boundary owns the XGetImage result and its
    /// dynamically loaded X11 destructor.
    fn read_pixels(
        window: &Window,
        root: c_ulong,
        origin: [i32; 2],
        dimensions: [i32; 2],
        errors: [&'static str; 3],
        mut consume: impl FnMut(i32, i32, c_ulong, [c_ulong; 3]) -> Result<(), String>,
    ) -> Result<(), String> {
        let [bounds_error, image_error, visual_error] = errors;
        let [x, y] = origin;
        let [width, height] = dimensions;
        if width <= 0 || height <= 0 {
            return Err(bounds_error.into());
        }
        // SAFETY: SDL owns the live X11 Display. Symbol types match the Xlib
        // C signatures and all output parameters are writable stack values.
        // The root geometry is checked before requesting an image. OwnedImage
        // retains the XDestroyImage symbol and frees the image on every exit.
        unsafe {
            let lib = library()?;
            let display = display(window)?;
            let geometry: Symbol<
                unsafe extern "C" fn(
                    NonNull<c_void>,
                    c_ulong,
                    &mut c_ulong,
                    &mut c_int,
                    &mut c_int,
                    &mut u32,
                    &mut u32,
                    &mut u32,
                    &mut u32,
                ) -> c_int,
            > = lib.get(b"XGetGeometry\0").map_err(|e| e.to_string())?;
            let (mut returned_root, mut rx, mut ry, mut rw, mut rh, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            if geometry(
                display.as_non_null(),
                root,
                &mut returned_root,
                &mut rx,
                &mut ry,
                &mut rw,
                &mut rh,
                &mut border,
                &mut depth,
            ) == 0
                || x < 0
                || y < 0
                || x as i64 + width as i64 > rw as i64
                || y as i64 + height as i64 > rh as i64
            {
                return Err(bounds_error.into());
            }
            let get: Symbol<GetXImage> = lib.get(b"XGetImage\0").map_err(|e| e.to_string())?;
            let pixel: Symbol<unsafe extern "C" fn(NonNull<Image>, c_int, c_int) -> c_ulong> =
                lib.get(b"XGetPixel\0").map_err(|e| e.to_string())?;
            let destroy: Symbol<unsafe extern "C" fn(NonNull<Image>) -> c_int> =
                lib.get(b"XDestroyImage\0").map_err(|e| e.to_string())?;
            let raw = get(
                display.as_non_null(),
                root,
                x,
                y,
                width as u32,
                height as u32,
                c_ulong::MAX,
                2,
            )
            .ok_or(image_error)?;
            let image = OwnedImage { raw, destroy };
            // SAFETY: XGetImage returned a non-null, owned image that stays
            // alive through the OwnedImage guard. Reject malformed metadata
            // before asking XGetPixel to address its native pixel buffer.
            let header = image.raw.as_ref();
            if header.width != width
                || header.height != height
                || header.format != 2 // XGetImage requested ZPixmap
                || header.data.is_none()
                || !valid_row_stride(
                    header.width,
                    header.xoffset,
                    header.bits_per_pixel,
                    header.bytes_per_line,
                )
            {
                return Err(image_error.into());
            }
            let masks = [header.red_mask, header.green_mask, header.blue_mask];
            if masks.contains(&0) {
                return Err(visual_error.into());
            }
            for y in 0..height {
                for x in 0..width {
                    consume(x, y, pixel(image.raw, x, y), masks)?;
                }
            }
            Ok(())
        }
    }

    pub fn sample(window: &Window, point: ScreenPoint) -> Result<InkColor, String> {
        let mut rgb = [0.; 3];
        read_pixels(
            window,
            point.root,
            [point.x, point.y],
            [1, 1],
            [
                "Screen coordinate is out of bounds",
                "Cannot read the selected desktop pixel",
                "Screen picking requires a TrueColor visual",
            ],
            |_, _, value, masks| {
                for (i, mask) in masks.into_iter().enumerate() {
                    rgb[i] = channel(value, mask)?;
                }
                Ok(())
            },
        )?;
        Ok(crate::icc::display_sample_to_srgb(window, rgb))
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
        if w <= 0 || h <= 0 {
            return Err("Invalid desktop capture dimensions".into());
        }
        let mut out = crate::background::Image::buffer(w as u32, h as u32)?;
        read_pixels(
            window,
            origin.root,
            [origin.x, origin.y],
            [w, h],
            [
                "Capture is outside the connected display bounds",
                "Cannot capture the desktop",
                "Desktop capture requires a TrueColor visual",
            ],
            |x, y, value, masks| {
                let offset = (y as usize * w as usize + x as usize) * 4;
                for (i, mask) in masks.into_iter().enumerate() {
                    out.rgba[offset + i] = (channel(value, mask)? * 255.).round() as u8;
                }
                out.rgba[offset + 3] = 255;
                Ok(())
            },
        )?;
        // SDL owns its ICC profile allocation; Window copies and frees it.
        if let Some(profile) = window.icc_profile() {
            out.convert_to_srgb(&profile);
        }
        Ok(out)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn rejects_truncated_ximage_rows_before_native_pixel_reads() {
            assert!(valid_row_stride(10, 0, 32, 40));
            assert!(!valid_row_stride(10, 0, 32, 39));
            assert!(valid_row_stride(9, 1, 24, 32));
            assert!(!valid_row_stride(9, 1, 24, 29));
            assert!(valid_row_stride(5, 0, 1, 1));
            assert!(!valid_row_stride(5, 0, 0, 1));
            assert!(!valid_row_stride(5, -1, 32, 32));
            assert!(!valid_row_stride(i32::MAX, 0, 32, i32::MAX));
        }

        #[test]
        fn decodes_eight_ten_and_sixteen_bit_visuals() {
            assert_eq!(channel(0x12abef, 0xff0000), Ok(18. / 255.));
            assert_eq!(channel(0x12abef, 0xff00), Ok(171. / 255.));
            assert_eq!(channel(0xffff, 0xf800), Ok(1.));
            assert_eq!(channel(0x3ff00000, 0x3ff00000), Ok(1.));
            assert!(channel(0, 0).is_err());
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use x11::{capture, is_xwayland, position, sample};

#[cfg(all(test, any(target_os = "linux", target_os = "freebsd")))]
mod coordinate_tests {
    use super::scaled_pixels;

    #[test]
    fn scales_and_rejects_invalid_window_dimensions() {
        assert_eq!(
            scaled_pixels([10.5, 11.], [100., 100.], [200, 150]),
            Ok([21, 16])
        );
        assert!(scaled_pixels([3., 4.], [0., 100.], [200, 200]).is_err());
        assert!(scaled_pixels([3., 4.], [100., 100.], [0, 200]).is_err());
        assert!(scaled_pixels([f32::NAN, 4.], [100., 100.], [200, 200]).is_err());
        assert!(scaled_pixels([f32::MAX, 4.], [100., 100.], [200, 200]).is_err());
    }
}
