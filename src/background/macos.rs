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

pub fn start(bounds: [i32; 4], pixels: [i32; 2], mode: SnapshotMode) -> Result<Job, String> {
    match mode {
        SnapshotMode::Sdr => Job::start(move |cancelled| sdr_capture(bounds, cancelled)),
        SnapshotMode::Linear => Job::start(move |cancelled| hdr_capture(bounds, pixels, cancelled)),
        SnapshotMode::LiveOnly(reason) => Err(reason.into()),
    }
}

#[repr(C)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetDisplaysWithPoint(point: Point, max: u32, displays: *mut u32, count: *mut u32) -> i32;
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

fn hdr_capture(bounds: [i32; 4], pixels: [i32; 2], cancelled: &AtomicBool) -> CaptureResult {
    // Load on demand so earlier macOS versions can still launch OverInk.
    static FRAMEWORK: OnceLock<Result<Library, String>> = OnceLock::new();
    FRAMEWORK
        .get_or_init(|| unsafe {
            Library::new("/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit")
                .map_err(|error| format!("ScreenCaptureKit is unavailable: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let id = display_id(bounds)?;
    let (sender, receiver) = mpsc::channel();
    autoreleasepool(|_| {
        let shareable =
            AnyClass::get("SCShareableContent").ok_or("ScreenCaptureKit is unavailable")?;
        let configuration =
            AnyClass::get("SCStreamConfiguration").ok_or("ScreenCaptureKit is unavailable")?;
        let supported: bool = unsafe {
            msg_send![configuration, respondsToSelector: sel!(streamConfigurationWithPreset:)]
        };
        if !supported || AnyClass::get("SCScreenshotManager").is_none() {
            return Err("HDR screenshots require macOS 15 or later".to_string());
        }
        let completion =
            block2::RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
                autoreleasepool(|_| {
                    if content.is_null() {
                        let _ = sender.send(Err(capture_error(error)));
                    } else if let Err(error) = begin_screenshot(content, id, pixels, sender.clone())
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
            return Err("The macOS HDR screenshot timed out".into());
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

fn capture_error(error: *mut AnyObject) -> String {
    let description = unsafe {
        let message: *mut AnyObject = msg_send![error, localizedDescription];
        let text: *const c_char = msg_send![message, UTF8String];
        if text.is_null() {
            "No image returned".into()
        } else {
            CStr::from_ptr(text).to_string_lossy()
        }
    };
    format!("macOS HDR screenshot: {description}. Check Screen Recording permission.")
}

fn begin_screenshot(
    content: *mut AnyObject,
    id: u32,
    [width, height]: [i32; 2],
    sender: mpsc::Sender<CaptureResult>,
) -> Result<(), String> {
    let filter_class = AnyClass::get("SCContentFilter").ok_or("ScreenCaptureKit is unavailable")?;
    let configuration =
        AnyClass::get("SCStreamConfiguration").ok_or("ScreenCaptureKit is unavailable")?;
    let manager = AnyClass::get("SCScreenshotManager").ok_or("ScreenCaptureKit is unavailable")?;
    let array = AnyClass::get("NSArray").ok_or("NSArray is unavailable")?;
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
        // SCStreamConfigurationPresetCaptureHDRScreenshotLocalDisplay.
        let config: *mut AnyObject =
            msg_send![configuration, streamConfigurationWithPreset: 2_isize];
        if config.is_null() {
            return Err("Cannot configure an HDR screenshot".into());
        }
        let _: () = msg_send![config, setWidth: width as usize];
        let _: () = msg_send![config, setHeight: height as usize];
        let _: () = msg_send![config, setShowsCursor: false];
        let _: () = msg_send![config, setPixelFormat: u32::from_be_bytes(*b"RGhA")];
        let _: () = msg_send![config, setColorSpaceName: kCGColorSpaceExtendedLinearSRGB];
        let completion =
            block2::RcBlock::new(move |image: *const c_void, error: *mut AnyObject| {
                let result = if image.is_null() {
                    Err(capture_error(error))
                } else {
                    copy_screenshot(image).map(Some)
                };
                let _ = sender.send(result);
            });
        let _: () = msg_send![manager, captureImageWithFilter: &*filter, configuration: config,
            completionHandler: &*completion];
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
        snapshot.copy_hdr_rows(CFDataGetBytePtr(data.0), len, CGImageGetBytesPerRow(image))?;
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
                    return Err("Allow OverInk Screen Recording access in macOS System Settings, then retry freezing the desktop".into());
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
