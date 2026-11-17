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
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
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

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetDisplaysWithPoint(point: Point, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGColorSpaceCreateWithName(name: *const c_void) -> *const c_void;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: *const c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGContextTranslateCTM(context: *mut c_void, tx: f64, ty: f64);
    fn CGContextScaleCTM(context: *mut c_void, sx: f64, sy: f64);
    fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *const c_void);
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGImageGetBitsPerComponent(image: *const c_void) -> usize;
    fn CGImageGetBitsPerPixel(image: *const c_void) -> usize;
    fn CGImageGetBytesPerRow(image: *const c_void) -> usize;
    fn CGImageGetBitmapInfo(image: *const c_void) -> u32;
    fn CGImageGetColorSpace(image: *const c_void) -> *const c_void;
    fn CGColorSpaceGetName(space: *const c_void) -> *const c_void;
    fn CGImageGetDataProvider(image: *const c_void) -> *const c_void;
    fn CGDataProviderCopyData(provider: *const c_void) -> *const c_void;
    static kCGColorSpaceExtendedLinearSRGB: *const c_void;
    static kCGColorSpaceSRGB: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFEqual(left: *const c_void, right: *const c_void) -> u8;
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFRelease(value: *const c_void);
}

fn display_id([x, y, w, h]: [i32; 4]) -> Result<u32, String> {
    let center = Point {
        x: f64::from(x) + f64::from(w) / 2.,
        y: f64::from(y) + f64::from(h) / 2.,
    };
    let (mut id, mut count) = (0, 0);
    if unsafe { CGGetDisplaysWithPoint(center, 1, &mut id, &mut count) } != 0 || count == 0 {
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
    if unsafe { CGPreflightScreenCaptureAccess() } {
        return Ok(());
    }
    // TCC may cache a denial until the application is restarted. Request once
    // per process rather than showing another OS alert for every screenshot.
    static REQUESTED: OnceLock<()> = OnceLock::new();
    REQUESTED.get_or_init(|| {
        let _ = unsafe { CGRequestScreenCaptureAccess() };
    });
    Err("Allow OverInk in macOS System Settings > Privacy & Security > Screen & System Audio Recording, then quit and reopen OverInk".into())
}

fn sdr_supported() -> bool {
    screen_capture_kit().is_ok()
        && AnyClass::get(c"SCShareableContent").is_some()
        && AnyClass::get(c"SCStreamConfiguration").is_some()
        && AnyClass::get(c"SCScreenshotManager").is_some_and(|manager| unsafe {
            msg_send![
                manager, respondsToSelector: sel!(captureImageWithFilter:configuration:completionHandler:)
            ]
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
        unsafe {
            let preset: bool = msg_send![
                configuration, respondsToSelector: sel!(streamConfigurationWithPreset:)
            ];
            let capture: bool = msg_send![
                manager, respondsToSelector: sel!(captureImageWithFilter:configuration:completionHandler:)
            ];
            preset && capture
        }
    })
}

fn apple_silicon() -> bool {
    #[cfg(target_arch = "aarch64")]
    return true;
    #[cfg(target_arch = "x86_64")]
    {
        // A translated x86_64 build can use the HDR API on Apple silicon.
        unsafe extern "C" {
            fn sysctlbyname(
                name: *const c_char,
                value: *mut i32,
                len: *mut usize,
                new_value: *const c_void,
                new_len: usize,
            ) -> i32;
        }
        let mut translated = 0;
        let mut len = size_of::<i32>();
        let result = unsafe {
            sysctlbyname(
                c"sysctl.proc_translated".as_ptr(),
                &mut translated,
                &mut len,
                ptr::null(),
                0,
            )
        };
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
    let id = display_id(bounds)?;
    let (sender, receiver) = mpsc::channel();
    autoreleasepool(|_| -> Result<(), String> {
        let shareable =
            AnyClass::get(c"SCShareableContent").ok_or("ScreenCaptureKit is unavailable")?;
        let completion =
            block2::RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
                autoreleasepool(|_| {
                    if content.is_null() {
                        let _ = sender.send(Err(capture_error(error, hdr)));
                    } else if let Err(error) =
                        begin_screenshot(content, id, pixels, hdr, sender.clone())
                    {
                        let _ = sender.send(Err(error));
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

fn capture_error(error: *mut AnyObject, hdr: bool) -> String {
    let description = unsafe {
        let message: *mut AnyObject = msg_send![error, localizedDescription];
        let text: *const c_char = msg_send![message, UTF8String];
        if text.is_null() {
            "No image returned".into()
        } else {
            CStr::from_ptr(text).to_string_lossy()
        }
    };
    let mode = if hdr { "HDR" } else { "SDR" };
    format!("macOS {mode} screenshot: {description}. Check Screen Recording permission.")
}

fn begin_screenshot(
    content: *mut AnyObject,
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
    unsafe {
        let displays: *mut AnyObject = msg_send![content, displays];
        let count: usize = msg_send![displays, count];
        let mut display = ptr::null_mut::<AnyObject>();
        for index in 0..count {
            let item: *mut AnyObject = msg_send![displays, objectAtIndex: index];
            let display_id: u32 = msg_send![item, displayID];
            if display_id == id {
                display = item;
                break;
            }
        }
        if display.is_null() {
            return Err("ScreenCaptureKit did not return the selected display".into());
        }
        let empty: *mut AnyObject = msg_send![array, array];
        let filter: *mut AnyObject = msg_send![filter_class, alloc];
        let filter: *mut AnyObject =
            msg_send![filter, initWithDisplay: display, excludingWindows: empty];
        let filter = Retained::from_raw(filter).ok_or("Cannot configure the selected display")?;
        // The HDR preset is available on supported Apple silicon systems.
        // Ordinary SDR captures use the standard configuration on macOS 14+.
        let config: *mut AnyObject = if hdr {
            msg_send![configuration, streamConfigurationWithPreset: 2_isize]
        } else {
            msg_send![configuration, new]
        };
        if config.is_null() {
            return Err("Cannot configure the macOS screenshot".into());
        }
        let _: () = msg_send![config, setWidth: width as usize];
        let _: () = msg_send![config, setHeight: height as usize];
        let _: () = msg_send![config, setShowsCursor: false];
        let _: () = msg_send![config, setCapturesAudio: false];
        if hdr {
            let _: () = msg_send![config, setPixelFormat: u32::from_be_bytes(*b"RGhA")];
            let _: () = msg_send![config, setColorSpaceName: kCGColorSpaceExtendedLinearSRGB];
        } else {
            let _: () = msg_send![config, setPixelFormat: u32::from_be_bytes(*b"BGRA")];
            let _: () = msg_send![config, setColorSpaceName: kCGColorSpaceSRGB];
        }
        let completion =
            block2::RcBlock::new(move |image: *const c_void, error: *mut AnyObject| {
                let result = if image.is_null() {
                    Err(capture_error(error, hdr))
                } else if hdr {
                    copy_screenshot(image).map(Some)
                } else {
                    copy_sdr_screenshot(image).map(Some)
                };
                let _ = sender.send(result);
            });
        let _: () = msg_send![manager, captureImageWithFilter: &*filter, configuration: config,
            completionHandler: &*completion];
        if !hdr {
            // +new returns an owned object, unlike the autoreleased HDR preset.
            let _: () = msg_send![config, release];
        }
    }
    Ok(())
}

struct Data(*const c_void);
impl Drop for Data {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn copy_screenshot(image: *const c_void) -> Result<Image, String> {
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
        let space = CGImageGetColorSpace(image);
        let name = if space.is_null() {
            ptr::null()
        } else {
            CGColorSpaceGetName(space)
        };
        if name.is_null() || CFEqual(name, kCGColorSpaceExtendedLinearSRGB) == 0 {
            return Err("The HDR screenshot did not use extended linear sRGB".into());
        }
        let width = u32::try_from(CGImageGetWidth(image)).map_err(|_| "Capture is too large")?;
        let height = u32::try_from(CGImageGetHeight(image)).map_err(|_| "Capture is too large")?;
        let mut snapshot = Image::hdr_buffer(width, height)?;
        let provider = CGImageGetDataProvider(image);
        if provider.is_null() {
            return Err("The HDR screenshot has no pixel data".into());
        }
        let data = CGDataProviderCopyData(provider);
        if data.is_null() {
            return Err("Cannot read the HDR screenshot".into());
        }
        let data = Data(data);
        let len =
            usize::try_from(CFDataGetLength(data.0)).map_err(|_| "Invalid screenshot data")?;
        let bytes = CFDataGetBytePtr(data.0);
        if bytes.is_null() {
            return Err("The HDR screenshot has no pixel data".into());
        }
        // CFData owns the initialized byte buffer until the row copy completes.
        let pixels = std::slice::from_raw_parts(bytes, len);
        snapshot.copy_hdr_rows(len, CGImageGetBytesPerRow(image), |offset, row| {
            row.copy_from_slice(&pixels[offset..offset + row.len()]);
        })?;
        Ok(snapshot)
    }
}

fn copy_sdr_screenshot(image: *const c_void) -> Result<Image, String> {
    unsafe {
        let width = u32::try_from(CGImageGetWidth(image)).map_err(|_| "Capture is too large")?;
        let height = u32::try_from(CGImageGetHeight(image)).map_err(|_| "Capture is too large")?;
        let mut snapshot = Image::buffer(width, height)?;
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or("Invalid SDR screenshot row width")?;
        let space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        if space.is_null() {
            return Err("Cannot create an sRGB screenshot color space".into());
        }
        let space = Data(space);
        // kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big is RGBA8.
        let context = CGBitmapContextCreate(
            snapshot.rgba.as_mut_ptr().cast(),
            width as usize,
            height as usize,
            8,
            stride,
            space.0,
            0x4001,
        );
        if context.is_null() {
            return Err("Cannot create the SDR screenshot bitmap".into());
        }
        let context = Data(context);
        // Core Graphics uses a bottom-left origin. Output rows are top-down.
        CGContextTranslateCTM(context.0.cast_mut(), 0., height as f64);
        CGContextScaleCTM(context.0.cast_mut(), 1., -1.);
        CGContextDrawImage(
            context.0.cast_mut(),
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
    use std::{
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("overink-freeze-{}-{unique}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("desktop.png");
    let result = (|| {
        let [x, y, w, h] = bounds;
        let mut child = Command::new("/usr/sbin/screencapture")
            .args(["-x", "-t", "png", &format!("-R{x},{y},{w},{h}")])
            .arg(&file)
            .spawn()
            .map_err(|e| e.to_string())?;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if !status.success() {
                    return Err("Allow OverInk Screen Recording access in macOS System Settings, then retry the screenshot".into());
                }
                return Image::load(&file).map(Some);
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    let _ = std::fs::remove_file(file);
    let _ = std::fs::remove_dir(dir);
    result
}
