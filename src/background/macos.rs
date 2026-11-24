use super::{
    Image, SnapshotMode,
    screenshot::{CaptureArea, Crop},
    system::{CaptureResult, Job},
};
use core_graphics::{
    access::ScreenCaptureAccess, display::CGDisplay, geometry::CGPoint as DisplayPoint,
};
use libloading::Library;
use objc2::{
    msg_send,
    rc::{Allocated, Retained, autoreleasepool},
    runtime::{AnyClass, AnyObject},
    sel,
};
use objc2_core_foundation::{CFString, CGPoint, CGRect as NativeRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGImage,
    kCGColorSpaceExtendedLinearSRGB, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSArray, NSError};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub fn start<T: Send + 'static>(
    bounds: [i32; 4],
    pixels: [i32; 2],
    mode: SnapshotMode,
    crop: Option<Crop>,
    prepare: impl FnOnce(Image, &AtomicBool) -> Result<Option<T>, String> + Send + 'static,
) -> Result<Job<T>, String> {
    match mode {
        SnapshotMode::Sdr => {
            // Apple permission requests should start on the UI thread, not on
            // the capture worker. A denied app only prompts once per process.
            capture_permission()?;
            Job::start(
                move |cancelled| {
                    if sdr_supported() {
                        screen_capture(bounds, pixels, false, crop, cancelled)
                    } else {
                        // ScreenCaptureKit's single-frame API requires macOS 14.
                        crop_capture(sdr_capture(bounds, cancelled), crop)
                    }
                },
                prepare,
            )
        }
        SnapshotMode::Linear => {
            capture_permission()?;
            Job::start(
                move |cancelled| screen_capture(bounds, pixels, true, crop, cancelled),
                prepare,
            )
        }
        SnapshotMode::LiveOnly(reason) => {
            Err(reason.get(crate::appearance::Language::English).into())
        }
    }
}

fn display_id([x, y, w, h]: [i32; 4]) -> Result<u32, String> {
    let center = DisplayPoint::new(
        f64::from(x) + f64::from(w) / 2.,
        f64::from(y) + f64::from(h) / 2.,
    );
    CGDisplay::displays_with_point(center, 1)
        .ok()
        .filter(|(_, count)| *count > 0)
        .and_then(|(displays, _)| displays.first().copied())
        .ok_or_else(|| "Cannot find the selected macOS display".into())
}

fn screen_capture_kit_loaded() -> bool {
    // Load on demand so earlier macOS versions can still launch OverInk.
    static FRAMEWORK: OnceLock<Option<Library>> = OnceLock::new();
    FRAMEWORK
        .get_or_init(|| unsafe {
            Library::new("/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit")
                .ok()
        })
        .is_some()
}

fn capture_permission() -> Result<(), String> {
    if ScreenCaptureAccess.preflight() {
        return Ok(());
    }
    // TCC may cache a denial until the application is restarted. Request once
    // per process rather than showing another OS alert for every screenshot.
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    if *REQUESTED.get_or_init(|| ScreenCaptureAccess.request()) {
        // A newly granted permission may be usable immediately even if TCC's
        // preflight result remains cached until the next process launch.
        Ok(())
    } else {
        Err("Allow OverInk in macOS System Settings > Privacy & Security > Screen & System Audio Recording, then quit and reopen OverInk".into())
    }
}

fn sdr_supported() -> bool {
    screen_capture_kit_loaded()
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
        if !apple_silicon() || !screen_capture_kit_loaded() {
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
        // The native sysctlbyname API takes raw buffer pointers and must not
        // be imported as a safe Rust 2024 FFI function. Use the fixed system
        // utility instead; hdr_supported caches this check once per process.
        std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "sysctl.proc_translated"])
            .output()
            .is_ok_and(|result| {
                result.status.success()
                    && std::str::from_utf8(&result.stdout).is_ok_and(|value| value.trim() == "1")
            })
    }
}

enum ScreenshotReply {
    Finished(CaptureResult),
    RetryFullDisplay,
}

struct PendingScreenshot {
    sender: mpsc::Sender<ScreenshotReply>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl PendingScreenshot {
    fn active(&self) -> bool {
        !self.cancelled.load(Ordering::Relaxed) && Instant::now() < self.deadline
    }
}

fn crop_capture(result: CaptureResult, crop: Option<Crop>) -> CaptureResult {
    if let Some(crop) = crop {
        result?.map(|image| crop.apply(image)).transpose()
    } else {
        result
    }
}

fn screen_capture(
    bounds: [i32; 4],
    pixels: [i32; 2],
    hdr: bool,
    crop: Option<Crop>,
    cancelled: &Arc<AtomicBool>,
) -> CaptureResult {
    if hdr && !hdr_supported() {
        return Err("HDR screenshots require macOS 15 or later on Apple silicon".into());
    }
    if pixels.iter().any(|&dimension| dimension <= 0) {
        return Err("Invalid macOS screenshot dimensions".into());
    }
    let id = display_id(bounds)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    match request_screenshot(id, pixels, hdr, crop, true, cancelled, deadline)? {
        ScreenshotReply::Finished(result) => result,
        ScreenshotReply::RetryFullDisplay => {
            match request_screenshot(id, pixels, hdr, crop, false, cancelled, deadline)? {
                ScreenshotReply::Finished(result) => result,
                ScreenshotReply::RetryFullDisplay => Err("Full-screen capture failed".into()),
            }
        }
    }
}

fn request_screenshot(
    id: u32,
    pixels: [i32; 2],
    hdr: bool,
    crop: Option<Crop>,
    try_region: bool,
    cancelled: &Arc<AtomicBool>,
    deadline: Instant,
) -> Result<ScreenshotReply, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Ok(ScreenshotReply::Finished(Ok(None)));
    }
    let (sender, receiver) = mpsc::channel();
    let pending = Arc::new(PendingScreenshot {
        sender,
        cancelled: Arc::clone(cancelled),
        deadline,
    });
    autoreleasepool(|_| -> Result<(), String> {
        let shareable =
            AnyClass::get(c"SCShareableContent").ok_or("ScreenCaptureKit is unavailable")?;
        let completion = block2::RcBlock::new(
            move |content: Option<&AnyObject>, error: Option<&NSError>| {
                autoreleasepool(|_| {
                    if !pending.active() {
                        return;
                    }
                    let result =
                        content
                            .ok_or_else(|| capture_error(error, hdr))
                            .and_then(|content| {
                                begin_screenshot(
                                    content, id, pixels, hdr, crop, try_region, &pending,
                                )
                            });
                    if let Err(error) = result {
                        let _ = pending.sender.send(ScreenshotReply::Finished(Err(error)));
                    }
                });
            },
        );
        unsafe {
            let _: () = msg_send![shareable, getShareableContentExcludingDesktopWindows: false,
                onScreenWindowsOnly: true, completionHandler: &*completion];
        }
        Ok(())
    })?;
    while !cancelled.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return Err("The macOS screenshot timed out".into());
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => return Ok(result),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) if cancelled.load(Ordering::Relaxed) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) if Instant::now() >= deadline => {
                return Err("The macOS screenshot timed out".into());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("The macOS screenshot callback disconnected".into());
            }
        }
    }
    Ok(ScreenshotReply::Finished(Ok(None)))
}

fn native_region(area: CaptureArea, content: NativeRect, scale: f64) -> Option<NativeRect> {
    // A backing-size mismatch would imply resampling relative to the
    // full-display request. Preserve the existing software crop in that case.
    if !matches!(scale, 1. | 2.)
        || [content.size.width * scale, content.size.height * scale] != area.full.map(f64::from)
        || area.dimensions() == area.full
    {
        return None;
    }
    let [left, top, _, _] = area.bounds.map(f64::from);
    let [width, height] = area.dimensions().map(f64::from);
    Some(NativeRect::new(
        CGPoint::new(left / scale, top / scale),
        CGSize::new(width / scale, height / scale),
    ))
}

fn capture_error(error: Option<&NSError>, hdr: bool) -> String {
    let Some(error) = error else {
        return "macOS screenshot returned no image or error".into();
    };
    let description = error.localizedDescription();
    let mode = if hdr { "HDR" } else { "SDR" };
    format!("macOS {mode} screenshot: {description}. Check Screen Recording permission.")
}

fn begin_screenshot(
    content: &AnyObject,
    id: u32,
    [width, height]: [i32; 2],
    hdr: bool,
    crop: Option<Crop>,
    try_region: bool,
    pending: &Arc<PendingScreenshot>,
) -> Result<(), String> {
    let filter_class =
        AnyClass::get(c"SCContentFilter").ok_or("ScreenCaptureKit is unavailable")?;
    let configuration =
        AnyClass::get(c"SCStreamConfiguration").ok_or("ScreenCaptureKit is unavailable")?;
    let manager = AnyClass::get(c"SCScreenshotManager").ok_or("ScreenCaptureKit is unavailable")?;
    if width <= 0 || height <= 0 {
        return Err("Invalid macOS screenshot dimensions".into());
    }
    unsafe {
        let displays: Option<Retained<NSArray<AnyObject>>> = msg_send![content, displays];
        let displays = displays.ok_or("ScreenCaptureKit returned no display list")?;
        let display = (0..displays.count())
            .map(|index| displays.objectAtIndex(index))
            .find(|item| {
                let display_id: u32 = msg_send![&**item, displayID];
                display_id == id
            })
            .ok_or("ScreenCaptureKit did not return the selected display")?;
        let empty = NSArray::<AnyObject>::new();
        let filter: Allocated<AnyObject> = msg_send![filter_class, alloc];
        let filter: Option<Retained<AnyObject>> =
            msg_send![filter, initWithDisplay: &*display, excludingWindows: &*empty];
        let filter = filter.ok_or("Cannot configure the selected display")?;
        // objc2 follows the selector's ownership convention for both the
        // autoreleased preset and the +1 object returned by new.
        let config: Option<Retained<AnyObject>> = if hdr {
            msg_send![configuration, streamConfigurationWithPreset: 2_isize]
        } else {
            msg_send![configuration, new]
        };
        let config = config.ok_or("Cannot configure the macOS screenshot")?;
        let dimensions = [width as u32, height as u32];
        let area = CaptureArea::new(dimensions, crop)?;
        let region = if try_region
            && crop.is_some()
            && filter_class
                .instance_method(sel!(pointPixelScale))
                .is_some()
            && filter_class.instance_method(sel!(contentRect)).is_some()
        {
            let scale: f32 = msg_send![&*filter, pointPixelScale];
            let content: NativeRect = msg_send![&*filter, contentRect];
            native_region(area, content, f64::from(scale))
        } else {
            None
        };
        let [width, height] = if let Some(region) = region {
            let _: () = msg_send![&*config, setSourceRect: region];
            area.dimensions()
        } else {
            dimensions
        };
        let _: () = msg_send![&*config, setWidth: width as usize];
        let _: () = msg_send![&*config, setHeight: height as usize];
        let _: () = msg_send![&*config, setShowsCursor: false];
        let _: () = msg_send![&*config, setCapturesAudio: false];
        let (format, space) = if hdr {
            (*b"RGhA", kCGColorSpaceExtendedLinearSRGB)
        } else {
            (*b"BGRA", kCGColorSpaceSRGB)
        };
        let _: () = msg_send![&*config, setPixelFormat: u32::from_be_bytes(format)];
        let _: () = msg_send![&*config, setColorSpaceName: space];
        if !pending.active() {
            return Ok(());
        }
        let pending = Arc::clone(pending);
        let pixel_crop = if region.is_some() { None } else { crop };
        let completion =
            block2::RcBlock::new(move |image: Option<&CGImage>, error: Option<&NSError>| {
                autoreleasepool(|_| {
                    if !pending.active() {
                        return;
                    }
                    if region.is_some()
                        && image.is_some()
                        && [CGImage::width(image), CGImage::height(image)]
                            != [width as usize, height as usize]
                    {
                        let _ = pending.sender.send(ScreenshotReply::RetryFullDisplay);
                        return;
                    }
                    let result = if let Some(image) = image {
                        if hdr {
                            copy_screenshot(image, space, pixel_crop).map(Some)
                        } else {
                            crop_capture(copy_sdr_screenshot(image).map(Some), pixel_crop)
                        }
                    } else {
                        Err(capture_error(error, hdr))
                    };
                    let reply = if region.is_some() {
                        match result {
                            Ok(Some(image)) => ScreenshotReply::Finished(Ok(Some(image))),
                            _ => ScreenshotReply::RetryFullDisplay,
                        }
                    } else {
                        ScreenshotReply::Finished(result)
                    };
                    let _ = pending.sender.send(reply);
                });
            });
        let _: () = msg_send![manager, captureImageWithFilter: &*filter, configuration: &*config,
            completionHandler: &*completion];
    }
    Ok(())
}

fn copy_screenshot(
    image: &CGImage,
    expected: &CFString,
    crop: Option<Crop>,
) -> Result<Image, String> {
    let bitmap = CGImage::bitmap_info(Some(image)).bits();
    let float_components = bitmap & (1 << 8) != 0;
    let alpha_last = matches!(bitmap & 0x1f, 1 | 3 | 5);
    let little_endian = matches!(bitmap & 0x7000, 0 | 0x1000);
    if CGImage::bits_per_component(Some(image)) != 16
        || CGImage::bits_per_pixel(Some(image)) != 64
        || !float_components
        || !alpha_last
        || !little_endian
    {
        return Err("ScreenCaptureKit did not return an FP16 HDR screenshot".into());
    }
    let space = CGImage::color_space(Some(image));
    if CGColorSpace::name(space.as_deref()).as_deref() != Some(expected) {
        return Err("The HDR screenshot did not use extended linear sRGB".into());
    }
    let width = u32::try_from(CGImage::width(Some(image))).map_err(|_| "Capture is too large")?;
    let height = u32::try_from(CGImage::height(Some(image))).map_err(|_| "Capture is too large")?;
    Image::byte_len(width, height, super::Encoding::LinearRgba16)?;
    let provider =
        CGImage::data_provider(Some(image)).ok_or("The HDR screenshot has no pixel data")?;
    let data = CGDataProvider::data(Some(&provider)).ok_or("Cannot read the HDR screenshot")?;
    // SAFETY: CopyData returns an immutable CFData that is owned here.
    let pixels = unsafe { data.as_bytes_unchecked() };
    Image::from_hdr_capture(
        pixels,
        CGImage::bytes_per_row(Some(image)),
        [width, height],
        crop,
    )
}

fn copy_sdr_screenshot(image: &CGImage) -> Result<Image, String> {
    // SAFETY: The correctly sized RGB bitmap buffer remains valid and
    // exclusively owned until the bitmap context is dropped.
    unsafe {
        let width =
            u32::try_from(CGImage::width(Some(image))).map_err(|_| "Capture is too large")?;
        let height =
            u32::try_from(CGImage::height(Some(image))).map_err(|_| "Capture is too large")?;
        let mut snapshot = Image::buffer(width, height)?;
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or("Invalid SDR screenshot row width")?;
        let space = CGColorSpace::with_name(Some(kCGColorSpaceSRGB))
            .ok_or("Cannot create an sRGB screenshot color space")?;
        // kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big is RGBA8.
        let buffer = snapshot.rgba.as_mut_ptr().cast();
        let context = CGBitmapContextCreate(
            buffer,
            width as usize,
            height as usize,
            8,
            stride,
            Some(&space),
            0x4001,
        )
        .ok_or("Cannot create the SDR screenshot bitmap")?;
        // The bitmap context stores top-to-bottom RGBA rows. The Core
        // Graphics user-space convention already maps the image into them.
        // Flipping the CTM here would invert the output scanlines.
        CGContext::draw_image(
            Some(&context),
            NativeRect::new(
                CGPoint::new(0., 0.),
                CGSize::new(width as f64, height as f64),
            ),
            Some(image),
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
    #[allow(deprecated)] // Required before ScreenCaptureKit's single-frame API.
    let image = objc2_core_graphics::CGDisplayCreateImage(id)
        .ok_or("Core Graphics could not capture the display. Check Screen Recording permission.")?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    copy_sdr_screenshot(&image).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_rect_is_display_local_and_uses_native_pixels() {
        let area = CaptureArea {
            full: [3840, 2160],
            bounds: [11, 17, 611, 417],
        };
        let content = NativeRect::new(CGPoint::new(-1920., 0.), CGSize::new(1920., 1080.));
        let Some(rect) = native_region(area, content, 2.) else {
            panic!("expected a native 2x capture region");
        };
        assert_eq!([rect.origin.x, rect.origin.y], [5.5, 8.5]);
        assert_eq!([rect.size.width, rect.size.height], [300., 200.]);
        assert!(native_region(area, content, 1.).is_none());
    }

    #[test]
    fn sdr_conversion_preserves_top_and_bottom_rows() -> Result<(), String> {
        // Two opaque rows in raw CGImage pixel order: red above blue.
        let mut pixels = [255, 0, 0, 255, 0, 0, 255, 255_u8];
        // SAFETY: This test creates and owns bitmap contexts through the
        // same +1 Core Graphics APIs as production capture.
        unsafe {
            let color = CGColorSpace::with_name(Some(kCGColorSpaceSRGB))
                .ok_or("Cannot create a test color space")?;
            let buffer = pixels.as_mut_ptr().cast();
            let context = CGBitmapContextCreate(buffer, 1, 2, 8, 4, Some(&color), 0x4001)
                .ok_or("Cannot create a test bitmap")?;
            let source = objc2_core_graphics::CGBitmapContextCreateImage(Some(&context))
                .ok_or("Cannot create a test image")?;
            let actual = copy_sdr_screenshot(&source)?;
            assert_eq!(
                actual.rgba, pixels,
                "SDR screenshot must not flip vertically"
            );
        }
        Ok(())
    }
}
