use super::Image;
use crate::{display::Display, sdl::*};
use egui::Rect;

pub enum Job {
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
        _displays: &[Display],
        region: Rect,
        _hdr: bool,
        _portal: bool,
    ) -> Result<Self, String> {
        if window.display_id() != Some(display.id) {
            return Err("The screenshot display has changed".into());
        }
        let size = window.size();
        let crop = Crop { region, size };
        #[cfg(target_os = "windows")]
        {
            super::windows::start(display.id, move |image| crop.apply(image)).map(Self::Native)
        }
        #[cfg(target_os = "macos")]
        {
            let mode = if _hdr {
                super::SnapshotMode::Linear
            } else {
                super::SnapshotMode::Sdr
            };
            super::macos::start(display.bounds, window.pixel_size(), mode, move |image| {
                crop.apply(image)
            })
            .map(Self::Native)
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            if _hdr {
                return Err("HDR capture is unavailable on this display backend".into());
            }
            if _portal {
                let crop = crop.on_portal_desktop(display, _displays);
                return crate::picker::portal::SystemJob::screenshot(move |image| {
                    if !crop.matches_desktop(&image) {
                        return Err(
                            "The portal screenshot does not match the display layout".into()
                        );
                    }
                    crop.apply(image)
                })
                .map(Self::Portal);
            }
            let image = crate::picker::desktop::capture(window)?;
            Ok(Self::X11(Some(crop.apply(image)?)))
        }
    }

    pub fn poll(&mut self) -> Option<Result<Option<Image>, String>> {
        match self {
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
struct Crop {
    region: Rect,
    size: [f32; 2],
}

impl Crop {
    fn apply(self, image: Image) -> Result<Image, String> {
        let [width, height] = [image.width, image.height];
        let sx = f64::from(width) / f64::from(self.size[0]);
        let sy = f64::from(height) / f64::from(self.size[1]);
        let left = (f64::from(self.region.left()) * sx).floor() as u32;
        let top = (f64::from(self.region.top()) * sy).floor() as u32;
        let right = (f64::from(self.region.right()) * sx).ceil() as u32;
        let bottom = (f64::from(self.region.bottom()) * sy).ceil() as u32;
        image.crop([
            left.min(width),
            top.min(height),
            right.min(width),
            bottom.min(height),
        ])
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn on_portal_desktop(self, display: &Display, displays: &[Display]) -> Self {
        let bounds = |[x, y, w, h]: [i32; 4]| {
            Rect::from_min_size(
                egui::pos2(x as f32, y as f32),
                egui::vec2(w as f32, h as f32),
            )
        };
        let monitor = bounds(display.bounds);
        let desktop = displays.iter().fold(monitor, |desktop, output| {
            desktop.union(bounds(output.bounds))
        });
        let scale = monitor.size() / egui::Vec2::from(self.size);
        let offset = monitor.min - desktop.min;
        let map = |point: egui::Pos2| egui::Pos2::ZERO + offset + point.to_vec2() * scale;
        Self {
            region: Rect::from_min_max(map(self.region.min), map(self.region.max)),
            size: desktop.size().into(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn matches_desktop(self, image: &Image) -> bool {
        // The portal returns the whole desktop as a single image.
        let height = f64::from(image.width) / f64::from(self.size[0]) * f64::from(self.size[1]);
        (height - f64::from(image.height)).abs() <= 2.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_selection_crops_fractional_scale_to_pixel_edges() {
        let mut image = Image::buffer(5, 5).unwrap();
        for (i, pixel) in image.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel[0] = i as u8;
        }
        let crop = Crop {
            region: Rect::from_min_max(egui::pos2(1., 2.), egui::pos2(3., 3.)),
            size: [4., 4.],
        };
        let image = crop.apply(image).unwrap();
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
    }
}
