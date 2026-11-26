#![forbid(unsafe_code)]

use super::Image;
use crate::{display::Display, sdl::*};
use egui::Rect;

pub fn hdr_supported() -> bool {
    #[cfg(target_os = "windows")]
    return super::windows::hdr_supported();
    #[cfg(target_os = "macos")]
    return super::macos::hdr_supported();
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    false
}

pub enum Job {
    #[cfg(target_os = "linux")]
    Shell(crate::picker::portal::SystemJob<Image>),
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    Native(super::system::Job<Image>),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Portal(crate::picker::portal::SystemJob<Image>),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    X11(Option<Image>),
}

impl Job {
    pub fn start(
        window: &Window,
        display: &Display,
        displays: &[Display],
        region: Rect,
        hdr: bool,
        portal: bool,
    ) -> Result<Self, String> {
        Self::start_inner(window, display, displays, region, hdr, portal, false)
    }

    /// All platforms use the same OverInk-owned color-picker UI: capture an
    /// SDR desktop frame while the canvas and controller are hidden, then let
    /// the application's own pointer handling select the color. On Wayland
    /// prefer the standard screenshot portal, not a system color sampler.
    /// The hidden canvas may temporarily lack a reported SDL display ID.
    pub fn start_picker(
        window: &Window,
        display: &Display,
        displays: &[Display],
        region: Rect,
    ) -> Result<Self, String> {
        Self::start_inner(window, display, displays, region, false, true, true)
    }

    fn start_inner(
        window: &Window,
        display: &Display,
        _displays: &[Display],
        region: Rect,
        _hdr: bool,
        _portal: bool,
        _prefer_portal: bool,
    ) -> Result<Self, String> {
        if window.display_id() != Some(display.id) && !(_prefer_portal && window.hidden()) {
            return Err("The screenshot display has changed".into());
        }
        let size = window.size();
        let crop = Crop { region, size };
        #[cfg(target_os = "windows")]
        {
            super::windows::start(display.id, _hdr, Some(crop), |image, _| Ok(Some(image)))
                .map(Self::Native)
        }
        #[cfg(target_os = "macos")]
        {
            let mode = if _hdr {
                super::SnapshotMode::Linear
            } else {
                super::SnapshotMode::Sdr
            };
            super::macos::start(
                display.bounds,
                window.pixel_size(),
                mode,
                Some(crop),
                |image, _| Ok(Some(image)),
            )
            .map(Self::Native)
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            #[cfg(target_os = "linux")]
            if crate::gnome::in_wayland_session() && !_prefer_portal {
                // Route GNOME Wayland and XWayland captures asynchronously
                // through the compositor. The X11 framebuffer cannot capture
                // native Wayland windows, and can silently omit screen content.
                // If the extension is absent, report an explicit error rather
                // than presenting an incomplete image as a successful capture.
                // OverInk's own selection UI is never replaced by GNOME's.
                // On XWayland prefer the Shell bridge (it captures both
                // X11 and native Wayland windows without a second selector).
                // If the extension is absent, use a composited screenshot
                // portal only when the full-screen crop can be proven safe.
                let fallback: Option<crate::picker::portal::ScreenshotFallback> =
                    if driver() == "x11" && _displays.len() == 1 {
                        let mapping = window.position().ok().and_then(|[x, y]| {
                            let location = Rect::from_min_size(
                                egui::pos2(x as f32, y as f32),
                                egui::vec2(size[0], size[1]),
                            );
                            crop.for_portal(
                                display,
                                _displays,
                                window.pixel_size().map(|n| n.max(0) as u32),
                                None,
                                None,
                                Some(location),
                            )
                            .ok()
                        });
                        mapping.map(|(mapped, sizes)| {
                            Box::new(move |image: Image| {
                                if !sizes.contains(&[image.width, image.height]) {
                                    return Err(
                                        "The portal screenshot dimensions do not match the desktop"
                                            .into(),
                                    );
                                }
                                mapped.apply(image)
                            })
                                as crate::picker::portal::ScreenshotFallback
                        })
                    } else {
                        None
                    };
                return crate::picker::portal::SystemJob::shell_area(
                    window, region, size, fallback,
                )
                .map(Self::Shell);
            }
            if _hdr {
                return Err("HDR capture is unavailable on this display backend".into());
            }
            if _portal {
                let multi = _displays.len() > 1;
                let pixels = window.pixel_size().map(|n| n.max(0) as u32);
                let density = if multi {
                    if driver() != "wayland" {
                        return Err(
                            "Cannot map a multi-display portal screenshot in XWayland".into()
                        );
                    }
                    if wayland_scale_to_display_forced() {
                        return Err(
                            "Cannot map a multi-display portal screenshot with SDL scaling forced"
                                .into(),
                        );
                    }
                    let density = display_density(display).ok_or("No Wayland pixel density")?;
                    if _displays.iter().any(|output| {
                        display_density(output).is_none_or(|other| (other - density).abs() > 0.0001)
                    }) {
                        return Err(
                            "The portal does not provide a layout for mixed display scaling".into(),
                        );
                    }
                    Some(density)
                } else {
                    None
                };
                // Wayland maximization uses the work area on GNOME; that can be
                // smaller than the display bounds (e.g. the top panel). Map the
                // canvas into the display before cropping the portal's desktop.
                let usable = usable_display_bounds(display.id).map(|[x, y, w, h]| {
                    Rect::from_min_size(
                        egui::pos2(x as f32, y as f32),
                        egui::vec2(w as f32, h as f32),
                    )
                });
                let location = if driver() == "wayland" {
                    // Wayland does not expose a toplevel window's global origin.
                    None
                } else {
                    window.position().ok().map(|[x, y]| {
                        Rect::from_min_size(
                            egui::pos2(x as f32, y as f32),
                            egui::vec2(size[0], size[1]),
                        )
                    })
                };
                let mapping =
                    crop.for_portal(display, _displays, pixels, density, usable, location);
                if driver() == "wayland" && crate::kwin::active() && !window.hidden() {
                    // KWin knows the real client origin, including panel offsets.
                    // Query before capture, off the SDL thread.
                    let display = display.clone();
                    let displays = _displays.to_vec();
                    return crate::picker::portal::SystemJob::screenshot_preflight(multi, move || {
                        let (crop, sizes) = match crate::kwin::canvas_geometry() {
                            Ok(location) => crop.for_portal(
                                &display,
                                &displays,
                                pixels,
                                density,
                                None,
                                Some(location),
                            )?,
                            Err(error) => {
                                mapping.map_err(|original| format!("{original}: {error}"))?
                            }
                        };
                        Ok(move |image| crop.apply_portal(image, sizes))
                    })
                    .map(Self::Portal);
                }
                let (crop, sizes) = mapping?;
                return crate::picker::portal::SystemJob::screenshot(multi, move |image| {
                    crop.apply_portal(image, sizes)
                })
                .map(Self::Portal);
            }
            crate::picker_desktop::capture_region(window, |size| crop.pixel_bounds(size))
                .map(|image| Self::X11(Some(image)))
        }
    }

    pub fn poll(&mut self) -> Option<Result<Option<Image>, String>> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Shell(job) => job.poll(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Self::Native(job) => job.poll(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Self::Portal(job) => job.poll(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Self::X11(image) => image.take().map(|image| Ok(Some(image))),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Crop {
    region: Rect,
    size: [f32; 2],
}

impl Crop {
    #[cfg(any(not(target_os = "windows"), test))]
    pub(super) fn apply(self, image: Image) -> Result<Image, String> {
        let bounds = self.pixel_bounds([image.width, image.height]);
        image.crop(bounds)
    }

    fn pixel_bounds(self, [width, height]: [u32; 2]) -> [u32; 4] {
        let sx = f64::from(width) / f64::from(self.size[0]);
        let sy = f64::from(height) / f64::from(self.size[1]);
        let left = (f64::from(self.region.left()) * sx).floor() as u32;
        let top = (f64::from(self.region.top()) * sy).floor() as u32;
        let right = (f64::from(self.region.right()) * sx).ceil() as u32;
        let bottom = (f64::from(self.region.bottom()) * sy).ceil() as u32;
        [
            left.min(width),
            top.min(height),
            right.min(width),
            bottom.min(height),
        ]
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn apply_portal(self, image: Image, sizes: [[u32; 2]; 3]) -> Result<Image, String> {
        if !sizes.contains(&[image.width, image.height]) {
            return Err(
                "The portal screenshot dimensions do not match the selected desktop".into(),
            );
        }
        self.apply(image)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn for_portal(
        self,
        display: &Display,
        displays: &[Display],
        pixels: [u32; 2],
        desktop_density: Option<f32>,
        usable: Option<Rect>,
        location: Option<Rect>,
    ) -> Result<(Self, [[u32; 2]; 3]), String> {
        let bounds = |[x, y, w, h]: [i32; 4]| {
            Rect::from_min_size(
                egui::pos2(x as f32, y as f32),
                egui::vec2(w as f32, h as f32),
            )
        };
        let monitor = bounds(display.bounds);
        if !monitor.is_positive()
            || self
                .size
                .iter()
                .any(|size| !size.is_finite() || *size <= 0.)
        {
            return Err("Invalid screenshot canvas size".into());
        }
        // Never infer an unknown window origin from its size: doing so can
        // silently capture a different portion of the desktop on Wayland.
        let matches_canvas = |area: &Rect| {
            area.is_positive()
                && monitor.contains_rect(*area)
                && (area.width() - self.size[0]).abs() <= 2.
                && (area.height() - self.size[1]).abs() <= 2.
        };
        let visible = if let Some(area) = location {
            // A reported X11 window origin takes precedence. Never replace
            // an invalid actual location with a guessed fullscreen rectangle.
            matches_canvas(&area).then_some(area)
        } else {
            (self.size == <[f32; 2]>::from(monitor.size()))
                .then_some(monitor)
                .or_else(|| usable.filter(matches_canvas))
        }
        .ok_or("Cannot determine the screenshot canvas position on this display")?;
        let scale_x = visible.width() / self.size[0];
        let scale_y = visible.height() / self.size[1];
        let map = |point: egui::Pos2| {
            egui::pos2(
                visible.min.x - monitor.min.x + point.x * scale_x,
                visible.min.y - monitor.min.y + point.y * scale_y,
            )
        };
        let mapped = Rect::from_min_max(map(self.region.min), map(self.region.max));
        let mut desktop = monitor;
        if desktop_density.is_some() {
            for (index, output) in displays.iter().enumerate() {
                let area = bounds(output.bounds);
                if !area.is_positive()
                    || displays[..index]
                        .iter()
                        .any(|other| area.intersect(bounds(other.bounds)).is_positive())
                {
                    return Err(
                        "The portal screenshot layout is ambiguous for overlapping displays".into(),
                    );
                }
                desktop = desktop.union(area);
            }
        }
        let offset = monitor.min - desktop.min;
        let size: [f32; 2] = desktop.size().into();
        let scale = if display.scale.is_finite() && display.scale > 0. {
            display.scale
        } else {
            1.
        };
        let display_pixels = size.map(|n| (n * scale).round() as u32);
        let native_pixels = desktop_density.map_or(display_pixels, |density| {
            size.map(|n| (n * density).round() as u32)
        });
        let fullscreen_pixels =
            if displays.len() == 1 && (visible.size() - monitor.size()).length() <= 2. {
                pixels
            } else {
                [0, 0]
            };
        Ok((
            Self {
                region: mapped.translate(offset),
                size,
            },
            [size.map(|n| n as u32), native_pixels, fullscreen_pixels],
        ))
    }
}

/// Physical bounds shared by the native readback paths and software crop.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CaptureArea {
    pub full: [u32; 2],
    pub bounds: [u32; 4],
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
impl CaptureArea {
    pub fn new([width, height]: [u32; 2], crop: Option<Crop>) -> Result<Self, String> {
        let full = [width, height];
        let bounds = crop.map_or([0, 0, width, height], |crop| crop.pixel_bounds(full));
        if bounds[0] >= bounds[2] || bounds[1] >= bounds[3] {
            return Err("The selection is outside the screenshot".into());
        }
        Ok(Self { full, bounds })
    }

    pub fn dimensions(self) -> [u32; 2] {
        let [left, top, right, bottom] = self.bounds;
        [right - left, bottom - top]
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn display_density(display: &Display) -> Option<f32> {
    // SDL's Wayland desktop mode expresses the native pixels per logical point.
    let (size, density) = desktop_display_mode(display.id)?;
    (size == [display.bounds[2], display.bounds[3]] && density.is_finite() && density > 0.)
        .then_some(density)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_region_readback_matches_full_crop_with_padded_rows() -> Result<(), String> {
        let full = [5, 4];
        let stride = 48;
        // The last row ends at the final pixel, without trailing padding.
        let mut pixels = [0x55_u8; 184];
        for y in 0..4 {
            for x in 0..5 {
                let offset = y * stride + x * 8;
                let channels = [0x4800_u16, 0xb800, 0x4000 + (y * 5 + x) as u16, 0];
                for (dst, value) in pixels[offset..offset + 8]
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(channels)
                {
                    *dst = value.to_ne_bytes();
                }
            }
        }
        let crop = Crop {
            region: Rect::from_min_max(egui::pos2(1.2, 1.1), egui::pos2(3.3, 3.7)),
            size: [5., 4.],
        };
        let reference = crop.apply(Image::from_hdr_capture(&pixels, stride, full, None)?)?;
        let region = Image::from_hdr_capture(&pixels, stride, full, Some(crop))?;
        assert_eq!([region.width, region.height], [3, 3]);
        assert_eq!(region.rgba, reference.rgba);
        assert!(Image::from_hdr_capture(&pixels[..183], stride, full, Some(crop)).is_err());
        assert!(Image::from_hdr_capture(&pixels, 39, full, Some(crop)).is_err());
        Ok(())
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn portal_crop_uses_the_monitor_origin_and_expected_desktop_sizes() -> Result<(), String> {
        let display = |id, bounds| Display {
            id,
            name: id.to_string(),
            bounds,
            refresh: 60.,
            scale: 1.5,
        };
        let left = display(1, [-800, -200, 800, 600]);
        let right = display(2, [0, 0, 800, 600]);
        let local = Rect::from_min_size(egui::pos2(30., 20.), egui::vec2(10., 12.));
        let selection = Crop {
            region: local,
            size: [800., 600.],
        };
        let (crop, sizes) = selection.for_portal(
            &right,
            &[left, right.clone()],
            [1200, 900],
            Some(1.5),
            None,
            None,
        )?;
        assert_eq!(sizes, [[1600, 800], [2400, 1200], [0, 0]]);
        assert_eq!(crop.region, local.translate(egui::vec2(800., 200.)));
        assert!(!sizes.contains(&[1200, 900]));
        assert!(!sizes.contains(&[3200, 1600]));
        assert!(
            selection
                .for_portal(
                    &right,
                    &[right.clone(), right.clone()],
                    [1200, 900],
                    Some(1.5),
                    None,
                    None,
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn portal_crop_accounts_for_gnome_work_area() -> Result<(), String> {
        let display = Display {
            id: 7,
            name: "GNOME".into(),
            bounds: [0, 0, 1440, 900],
            refresh: 60.,
            scale: 1.,
        };
        // The maximized Wayland overlay starts below a 34-point system bar.
        let selected = Crop {
            region: Rect::from_min_size(egui::pos2(100., 50.), egui::vec2(200., 150.)),
            size: [1440., 866.],
        };
        let usable = Rect::from_min_size(egui::pos2(0., 34.), egui::vec2(1440., 866.));
        let (crop, _) = selected.for_portal(
            &display,
            std::slice::from_ref(&display),
            [1440, 866],
            None,
            Some(usable),
            None,
        )?;
        assert_eq!(crop.size, [1440., 900.]);
        assert_eq!(crop.region.min, egui::pos2(100., 84.));
        assert_eq!(crop.region.max, egui::pos2(300., 234.));
        Ok(())
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn portal_crop_rejects_unknown_position_instead_of_guessing() -> Result<(), String> {
        let display = Display {
            id: 7,
            name: "GNOME".into(),
            bounds: [0, 0, 1200, 900],
            refresh: 60.,
            scale: 1.,
        };
        let selection = Crop {
            region: Rect::from_min_size(egui::pos2(100., 100.), egui::vec2(200., 100.)),
            size: [600., 450.],
        };
        assert!(
            selection
                .for_portal(
                    &display,
                    std::slice::from_ref(&display),
                    [1200, 900],
                    None,
                    None,
                    None,
                )
                .is_err()
        );
        // XWayland can supply the actual global window rectangle.
        let location = Rect::from_min_size(egui::pos2(300., 200.), egui::vec2(600., 450.));
        let (crop, _) = selection.for_portal(
            &display,
            std::slice::from_ref(&display),
            [1200, 900],
            None,
            None,
            Some(location),
        )?;
        assert_eq!(crop.region.min, egui::pos2(400., 300.));
        assert_eq!(crop.region.max, egui::pos2(600., 400.));
        Ok(())
    }

    #[test]
    fn logical_selection_crops_fractional_scale_to_pixel_edges() -> Result<(), String> {
        let mut image = Image::buffer(5, 5)?;
        for (i, pixel) in image.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel[0] = i as u8;
        }
        let crop = Crop {
            region: Rect::from_min_max(egui::pos2(1., 2.), egui::pos2(3., 3.)),
            size: [4., 4.],
        };
        let area = CaptureArea::new([image.width, image.height], Some(crop))?;
        assert_eq!(area.bounds, [1, 2, 4, 4]);
        assert_eq!(area.dimensions(), [3, 2]);
        let image = crop.apply(image)?;
        assert_eq!([image.width, image.height], [3, 2]);
        assert_eq!(
            image
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>(),
            [11, 12, 13, 16, 17, 18]
        );
        Ok(())
    }
}
