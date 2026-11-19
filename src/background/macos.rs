use super::{
    Image, SnapshotMode,
    system::{CaptureResult, Job},
};
use libloading::Library;
use objc2::{
    msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyClass, AnyObject},
    sel,
};
use std::{
    ffi::{CStr, c_char, c_void},
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub fn start<T: Send + 'static>(
    bounds: [i32; 4],
    pixels: [i32; 2],
    mode: SnapshotMode,
    prepare: impl FnOnce(Image) -> Result<T, String> + Send + 'static,
) -> Result<Job<T>, String> {
    match mode {
        SnapshotMode::Sdr => {
            // Apple permission requests should start on the UI thread, not on
            // the capture worker. A denied app only prompts once per process.
            capture_permission()?;
            Job::start(
                move |cancelled| {
                    if sdr_supported() {
                        screen_capture(bounds, pixels, false, cancelled)
                    } else {
                        // ScreenCaptureKit's single-frame API requires macOS 14.
                        sdr_capture(bounds, cancelled)
                    }
                },
                prepare,
            )
        }
        SnapshotMode::Linear => {
            capture_permission()?;
            Job::start(
                move |cancelled| screen_capture(bounds, pixels, true, cancelled),
                prepare,
            )
        }
        SnapshotMode::LiveOnly(reason) => {
            Err(reason.get(crate::appearance::Language::English).into())
        }
    }
}

#[repr(C)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
struct Size {
    width: f64,
    height: f64,
}

#[repr(C)]
struct CGRect {
    origin: Point,
    size: Size,
}

// The only permitted array count is one. The ABI-compatible repr(u32)
// prevents safe Rust callers from requesting more elements than the single
// initialized output slot provided to Core Graphics.
#[repr(u32)]
enum SingleDisplay {
    One = 1,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    safe fn CGGetDisplaysWithPoint(
        point: Point,
        max: SingleDisplay,
        displays: &mut u32,
        count: &mut u32,
    ) -> i32;
    // CGDisplayCreateImage takes only a scalar display ID. It returns an
    // owned (+1) image reference or null, without borrowing Rust memory.
    safe fn CGDisplayCreateImage(display: u32) -> Option<NonNull<c_void>>;
    // Both APIs take and return scalar values and do not borrow or retain
    // Rust memory. The UI-thread permission flow remains in capture_permission.
    safe fn CGPreflightScreenCaptureAccess() -> bool;
    safe fn CGRequestScreenCaptureAccess() -> bool;
    fn CGColorSpaceCreateWithName(name: NonNull<c_void>) -> Option<NonNull<c_void>>;
    fn CGBitmapContextCreate(
        data: NonNull<c_void>,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: NonNull<c_void>,
        bitmap_info: u32,
    ) -> Option<NonNull<c_void>>;
    fn CGContextDrawImage(context: NonNull<c_void>, rect: CGRect, image: NonNull<c_void>);
    #[cfg(test)]
    fn CGBitmapContextCreateImage(context: NonNull<c_void>) -> Option<NonNull<c_void>>;
    fn CGImageGetWidth(image: NonNull<c_void>) -> usize;
    fn CGImageGetHeight(image: NonNull<c_void>) -> usize;
    fn CGImageGetBitsPerComponent(image: NonNull<c_void>) -> usize;
    fn CGImageGetBitsPerPixel(image: NonNull<c_void>) -> usize;
    fn CGImageGetBytesPerRow(image: NonNull<c_void>) -> usize;
    fn CGImageGetBitmapInfo(image: NonNull<c_void>) -> u32;
    fn CGImageGetColorSpace(image: NonNull<c_void>) -> Option<NonNull<c_void>>;
    fn CGColorSpaceGetName(space: NonNull<c_void>) -> Option<NonNull<c_void>>;
    fn CGImageGetDataProvider(image: NonNull<c_void>) -> Option<NonNull<c_void>>;
    fn CGDataProviderCopyData(provider: NonNull<c_void>) -> Option<NonNull<c_void>>;
    // CoreGraphics constants are nullable C object pointers.
    static kCGColorSpaceExtendedLinearSRGB: Option<NonNull<c_void>>;
    static kCGColorSpaceSRGB: Option<NonNull<c_void>>;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFEqual(left: NonNull<c_void>, right: NonNull<c_void>) -> u8;
    fn CFDataGetLength(data: NonNull<c_void>) -> isize;
    fn CFDataGetBytePtr(data: NonNull<c_void>) -> Option<NonNull<u8>>;
    fn CFRelease(value: NonNull<c_void>);
}

fn display_id([x, y, w, h]: [i32; 4]) -> Result<u32, String> {
    let center = Point {
        x: f64::from(x) + f64::from(w) / 2.,
        y: f64::from(y) + f64::from(h) / 2.,
    };
    let (mut id, mut count) = (0, 0);
    if CGGetDisplaysWithPoint(center, SingleDisplay::One, &mut id, &mut count) != 0 || count == 0 {
        return Err("Cannot find the selected macOS display".into());
    }
    Ok(id)
}

fn screen_capture_kit() -> Result<(), String> {
    // Load on demand so earlier macOS versions can still launch OverInk.
    static FRAMEWORK: OnceLock<Result<Library, String>> = OnceLock::new();
    FRAMEWORK
        .get_or_init(|| unsafe {
            Library::new("/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit")
                .map_err(|error| format!("ScreenCaptureKit is unavailable: {error}"))
        })
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
}

fn capture_permission() -> Result<(), String> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    // TCC may cache a denial until the application is restarted. Request once
    // per process rather than showing another OS alert for every screenshot.
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    if *REQUESTED.get_or_init(CGRequestScreenCaptureAccess) {
        // A newly granted permission may be usable immediately even if TCC's
        // preflight result remains cached until the next process launch.
        Ok(())
    } else {
        Err("Allow OverInk in macOS System Settings > Privacy & Security > Screen & System Audio Recording, then quit and reopen OverInk".into())
    }
}

fn sdr_supported() -> bool {
    screen_capture_kit().is_ok()
        && AnyClass::get(c"SCShareableContent").is_some()
        && AnyClass::get(c"SCStreamConfiguration").is_some()
        && AnyClass::get(c"SCScreenshotManager").is_some_and(|manager| {
            manager
                .class_method(sel!(captureImageWithFilter:configuration:completionHandler:))
                .is_some()
        })
}

pub fn hdr_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if !apple_silicon() || screen_capture_kit().is_err() {
            return false;
        }
        let Some(configuration) = AnyClass::get(c"SCStreamConfiguration") else {
            return false;
        };
        let Some(manager) = AnyClass::get(c"SCScreenshotManager") else {
            return false;
        };
        configuration
            .class_method(sel!(streamConfigurationWithPreset:))
            .is_some()
            && manager
                .class_method(sel!(captureImageWithFilter:configuration:completionHandler:))
                .is_some()
    })
}

fn apple_silicon() -> bool {
    #[cfg(target_arch = "aarch64")]
    return true;
    #[cfg(target_arch = "x86_64")]
    {
        // A translated x86_64 build can use the HDR API on Apple silicon.
        // Only the fixed, NUL-terminated sysctl.proc_translated name is
        // queried. The kernel writes at most the initialized i32 buffer size
        // supplied in len; the new-value input is always absent.
        unsafe extern "C" {
            safe fn sysctlbyname(
                name: NonNull<c_char>,
                value: &mut i32,
                len: &mut usize,
                new_value: Option<NonNull<c_void>>,
                new_len: usize,
            ) -> i32;
        }
        let mut translated = 0;
        let mut len = size_of::<i32>();
        let Some(name) = NonNull::new(c"sysctl.proc_translated".as_ptr().cast_mut()) else {
            return false;
        };
        let result = sysctlbyname(name, &mut translated, &mut len, None, 0);
        result == 0 && len == size_of::<i32>() && translated == 1
    }
}

fn screen_capture(
    bounds: [i32; 4],
    pixels: [i32; 2],
    hdr: bool,
    cancelled: &AtomicBool,
) -> CaptureResult {
    if hdr && !hdr_supported() {
        return Err("HDR screenshots require macOS 15 or later on Apple silicon".into());
    }
    if pixels.iter().any(|&dimension| dimension <= 0) {
        return Err("Invalid macOS screenshot dimensions".into());
    }
    let id = display_id(bounds)?;
    let (sender, receiver) = mpsc::channel();
    autoreleasepool(|_| -> Result<(), String> {
        let shareable =
            AnyClass::get(c"SCShareableContent").ok_or("ScreenCaptureKit is unavailable")?;
        let completion =
            block2::RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
                autoreleasepool(|_| {
                    if let Some(content) = NonNull::new(content) {
                        if let Err(error) =
                            begin_screenshot(content, id, pixels, hdr, sender.clone())
                        {
                            let _ = sender.send(Err(error));
                        }
                    } else {
                        let _ = sender.send(Err(capture_error(NonNull::new(error), hdr)));
                    }
                });
            });
        unsafe {
            let _: () = msg_send![shareable, getShareableContentExcludingDesktopWindows: false,
                onScreenWindowsOnly: true, completionHandler: &*completion];
        }
        Ok(())
    })?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cancelled.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return Err("The macOS screenshot timed out".into());
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("The macOS screenshot callback disconnected".into());
            }
        }
    }
    Ok(None)
}

fn capture_error(error: Option<NonNull<AnyObject>>, hdr: bool) -> String {
    let Some(error) = error else {
        return "macOS screenshot returned no image or error".into();
    };
    let description = unsafe {
        let message: Option<NonNull<AnyObject>> =
            NonNull::new(msg_send![error.as_ptr(), localizedDescription]);
        let Some(message) = message else {
            return "macOS screenshot returned an error without a description".into();
        };
        let text: Option<NonNull<c_char>> = NonNull::new(msg_send![message.as_ptr(), UTF8String]);
        if let Some(text) = text {
            CStr::from_ptr(text.as_ptr()).to_string_lossy()
        } else {
            "No image returned".into()
        }
    };
    let mode = if hdr { "HDR" } else { "SDR" };
    format!("macOS {mode} screenshot: {description}. Check Screen Recording permission.")
}

fn begin_screenshot(
    content: NonNull<AnyObject>,
    id: u32,
    [width, height]: [i32; 2],
    hdr: bool,
    sender: mpsc::Sender<CaptureResult>,
) -> Result<(), String> {
    let filter_class =
        AnyClass::get(c"SCContentFilter").ok_or("ScreenCaptureKit is unavailable")?;
    let configuration =
        AnyClass::get(c"SCStreamConfiguration").ok_or("ScreenCaptureKit is unavailable")?;
    let manager = AnyClass::get(c"SCScreenshotManager").ok_or("ScreenCaptureKit is unavailable")?;
    let array = AnyClass::get(c"NSArray").ok_or("NSArray is unavailable")?;
    if width <= 0 || height <= 0 {
        return Err("Invalid macOS screenshot dimensions".into());
    }
    unsafe {
        let displays: Option<NonNull<AnyObject>> =
            NonNull::new(msg_send![content.as_ptr(), displays]);
        let displays = displays.ok_or("ScreenCaptureKit returned no display list")?;
        let count: usize = msg_send![displays.as_ptr(), count];
        let mut selected = None;
        for index in 0..count {
            let item: Option<NonNull<AnyObject>> =
                NonNull::new(msg_send![displays.as_ptr(), objectAtIndex: index]);
            let Some(item) = item else {
                continue;
            };
            let display_id: u32 = msg_send![item.as_ptr(), displayID];
            if display_id == id {
                selected = Some(item);
                break;
            }
        }
        let display = selected.ok_or("ScreenCaptureKit did not return the selected display")?;
        let empty: Option<NonNull<AnyObject>> = NonNull::new(msg_send![array, array]);
        let filter: Option<NonNull<AnyObject>> = NonNull::new(msg_send![filter_class, alloc]);
        let filter = filter.ok_or("Cannot allocate the selected display filter")?;
        let filter: Option<NonNull<AnyObject>> = NonNull::new(msg_send![
            filter.as_ptr(),
            initWithDisplay: display.as_ptr(),
            excludingWindows: empty.map_or(ptr::null_mut(), NonNull::as_ptr)
        ]);
        let filter = filter
            .and_then(|filter| Retained::from_raw(filter.as_ptr()))
            .ok_or("Cannot configure the selected display")?;
        // The HDR preset is available on supported Apple silicon systems.
        // Ordinary SDR captures use the standard configuration on macOS 14+.
        // The HDR preset is autoreleased (+0); +new returns an owned (+1)
        // object. Retained balances both cases, including early returns.
        let config: Option<NonNull<AnyObject>> = NonNull::new(if hdr {
            msg_send![configuration, streamConfigurationWithPreset: 2_isize]
        } else {
            msg_send![configuration, new]
        });
        let config = config
            .and_then(|config| {
                if hdr {
                    Retained::retain(config.as_ptr())
                } else {
                    Retained::from_raw(config.as_ptr())
                }
            })
            .ok_or("Cannot configure the macOS screenshot")?;
        let _: () = msg_send![&*config, setWidth: width as usize];
        let _: () = msg_send![&*config, setHeight: height as usize];
        let _: () = msg_send![&*config, setShowsCursor: false];
        let _: () = msg_send![&*config, setCapturesAudio: false];
        if hdr {
            let _: () = msg_send![&*config, setPixelFormat: u32::from_be_bytes(*b"RGhA")];
            let space = kCGColorSpaceExtendedLinearSRGB
                .map_or(ptr::null(), |name| name.as_ptr().cast_const());
            let _: () = msg_send![&*config, setColorSpaceName: space];
        } else {
            let _: () = msg_send![&*config, setPixelFormat: u32::from_be_bytes(*b"BGRA")];
            let space = kCGColorSpaceSRGB.map_or(ptr::null(), |name| name.as_ptr().cast_const());
            let _: () = msg_send![&*config, setColorSpaceName: space];
        }
        let completion =
            block2::RcBlock::new(move |image: *const c_void, error: *mut AnyObject| {
                autoreleasepool(|_| {
                    let result = if let Some(image) = NonNull::new(image.cast_mut()) {
                        if hdr {
                            copy_screenshot(image).map(Some)
                        } else {
                            copy_sdr_screenshot(image).map(Some)
                        }
                    } else {
                        Err(capture_error(NonNull::new(error), hdr))
                    };
                    let _ = sender.send(result);
                });
            });
        let _: () = msg_send![manager, captureImageWithFilter: &*filter, configuration: &*config,
            completionHandler: &*completion];
    }
    Ok(())
}

struct Data(NonNull<c_void>);
impl Drop for Data {
    fn drop(&mut self) {
        // SAFETY: Data exclusively owns a non-null Core Foundation object
        // created by a +1 Copy/Create API and releases it exactly once.
        unsafe { CFRelease(self.0) }
    }
}

fn copy_screenshot(image: NonNull<c_void>) -> Result<Image, String> {
    unsafe {
        let bitmap = CGImageGetBitmapInfo(image);
        let float_components = bitmap & (1 << 8) != 0;
        let alpha_last = matches!(bitmap & 0x1f, 1 | 3 | 5);
        let little_endian = matches!(bitmap & 0x7000, 0 | 0x1000);
        if CGImageGetBitsPerComponent(image) != 16
            || CGImageGetBitsPerPixel(image) != 64
            || !float_components
            || !alpha_last
            || !little_endian
        {
            return Err("ScreenCaptureKit did not return an FP16 HDR screenshot".into());
        }
        let name = CGImageGetColorSpace(image).and_then(|space| CGColorSpaceGetName(space));
        let expected =
            kCGColorSpaceExtendedLinearSRGB.ok_or("Extended linear sRGB is unavailable")?;
        if !name.is_some_and(|name| CFEqual(name, expected) != 0) {
            return Err("The HDR screenshot did not use extended linear sRGB".into());
        }
        let width = u32::try_from(CGImageGetWidth(image)).map_err(|_| "Capture is too large")?;
        let height = u32::try_from(CGImageGetHeight(image)).map_err(|_| "Capture is too large")?;
        let mut snapshot = Image::hdr_buffer(width, height)?;
        let provider =
            CGImageGetDataProvider(image).ok_or("The HDR screenshot has no pixel data")?;
        let data = Data(CGDataProviderCopyData(provider).ok_or("Cannot read the HDR screenshot")?);
        let len =
            usize::try_from(CFDataGetLength(data.0)).map_err(|_| "Invalid screenshot data")?;
        let bytes = CFDataGetBytePtr(data.0).ok_or("The HDR screenshot has no pixel data")?;
        // CFData owns the initialized byte buffer until the row copy completes.
        let pixels = std::slice::from_raw_parts(bytes.as_ptr(), len);
        snapshot.copy_hdr_rows(len, CGImageGetBytesPerRow(image), |offset, row| {
            row.copy_from_slice(&pixels[offset..offset + row.len()]);
        })?;
        Ok(snapshot)
    }
}

fn copy_sdr_screenshot(image: NonNull<c_void>) -> Result<Image, String> {
    unsafe {
        let width = u32::try_from(CGImageGetWidth(image)).map_err(|_| "Capture is too large")?;
        let height = u32::try_from(CGImageGetHeight(image)).map_err(|_| "Capture is too large")?;
        let mut snapshot = Image::buffer(width, height)?;
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or("Invalid SDR screenshot row width")?;
        let srgb = kCGColorSpaceSRGB.ok_or("The sRGB screenshot color space is unavailable")?;
        let space = Data(
            CGColorSpaceCreateWithName(srgb)
                .ok_or("Cannot create an sRGB screenshot color space")?,
        );
        // kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big is RGBA8.
        let buffer = NonNull::new(snapshot.rgba.as_mut_ptr().cast())
            .ok_or("Cannot obtain SDR screenshot pixel storage")?;
        let context = CGBitmapContextCreate(
            buffer,
            width as usize,
            height as usize,
            8,
            stride,
            space.0,
            0x4001,
        );
        let context = Data(context.ok_or("Cannot create the SDR screenshot bitmap")?);
        // The bitmap context stores top-to-bottom RGBA rows. The Core
        // Graphics user-space convention already maps the image into them.
        // Flipping the CTM here would invert the output scanlines.
        CGContextDrawImage(
            context.0,
            CGRect {
                origin: Point { x: 0., y: 0. },
                size: Size {
                    width: width as f64,
                    height: height as f64,
                },
            },
            image,
        );
        Ok(snapshot)
    }
}

fn sdr_capture(bounds: [i32; 4], cancelled: &AtomicBool) -> CaptureResult {
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    // macOS 11–13 fallback: capture directly in OverInk instead of spawning
    // /usr/sbin/screencapture, which could trigger a different TCC identity.
    let id = display_id(bounds)?;
    let image = Data(CGDisplayCreateImage(id).ok_or(
        "Core Graphics could not capture the display. Check Screen Recording permission.",
    )?);
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    copy_sdr_screenshot(image.0).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_conversion_preserves_top_and_bottom_rows() -> Result<(), String> {
        // Two opaque rows in raw CGImage pixel order: red above blue.
        let mut pixels = [255, 0, 0, 255, 0, 0, 255, 255_u8];
        // SAFETY: This test creates and owns bitmap contexts through the
        // same +1 Core Graphics APIs as production capture.
        unsafe {
            let srgb = kCGColorSpaceSRGB.ok_or("The test sRGB color space is unavailable")?;
            let color =
                Data(CGColorSpaceCreateWithName(srgb).ok_or("Cannot create a test color space")?);
            let buffer = NonNull::new(pixels.as_mut_ptr().cast())
                .ok_or("Cannot obtain the test pixel buffer")?;
            let context = Data(
                CGBitmapContextCreate(buffer, 1, 2, 8, 4, color.0, 0x4001)
                    .ok_or("Cannot create a test bitmap")?,
            );
            let source =
                Data(CGBitmapContextCreateImage(context.0).ok_or("Cannot create a test image")?);
            let actual = copy_sdr_screenshot(source.0)?;
            assert_eq!(
                actual.rgba, pixels,
                "SDR screenshot must not flip vertically"
            );
        }
        Ok(())
    }
}
