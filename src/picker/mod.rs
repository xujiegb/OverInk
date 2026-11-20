#![forbid(unsafe_code)]

pub(crate) mod desktop;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod portal;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::background::screenshot::Job as PortalCapture;
#[cfg(target_os = "macos")]
use crate::background::system::Job as MacCapture;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd", test))]
use crate::background::Image;
use crate::display::Display;
use crate::{color::InkColor, sdl::*};
use std::time::{Duration, Instant};
pub type PickResult = Result<Option<InkColor>, String>;
enum Stage {
    Idle,
    Choosing,
    SampleAfter(desktop::ScreenPoint, Instant),
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
    Preparing(Instant),
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
    Capturing(CaptureJob),
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
    ChoosingImage(Image),
}

/// The native sampler has no public cancel API on macOS, while some Wayland
/// portals use modal dialogs. Capture the desktop *once* while both OverInk
/// windows are hidden; then use OverInk's own cancellable pointer/UI event
/// loop to select a color from that clean, owned frame.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
enum CaptureJob {
    #[cfg(target_os = "macos")]
    Mac(MacCapture<Image>),
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Portal(PortalCapture),
}
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
impl CaptureJob {
    fn poll(&mut self) -> Option<Result<Option<Image>, String>> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(job) => job.poll(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            Self::Portal(job) => job.poll(),
        }
    }
}

/// Match a canvas-logical pointer position against the privately captured
/// RGBA8 screen buffer, independent of output DPI. Never index an out-of-range
/// pixel or return colors from invalid/cancelled image data.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd", test))]
fn sample_image(image: &Image, point: [f32; 2], size: [f32; 2]) -> Option<InkColor> {
    if image.width == 0
        || image.height == 0
        || point.iter().chain(size.iter()).any(|v| !v.is_finite())
        || size.iter().any(|v| *v <= 0.)
        || point.iter().zip(size).any(|(p, limit)| *p < 0. || *p >= limit)
    {
        return None;
    }
    let x = ((point[0] / size[0]) * image.width as f32).floor() as usize;
    let y = ((point[1] / size[1]) * image.height as f32).floor() as usize;
    let offset = (y.checked_mul(image.width as usize)?.checked_add(x)?).checked_mul(4)?;
    let rgb = image.rgba.get(offset..offset.checked_add(3)?)?;
    Some(InkColor::from_rgb8([rgb[0], rgb[1], rgb[2]]))
}

/// Native controller bounds are in canvas-local logical coordinates.
/// Require finite positive geometry and reject clicks on the draggable grip
/// or action buttons even when an underlying overlay receives the SDL packet.
fn controller_contains([x, y, w, h]: [f32; 4], [px, py]: [f32; 2]) -> bool {
    [x, y, w, h, px, py].into_iter().all(f32::is_finite)
        && w > 0.
        && h > 0.
        && px >= x
        && py >= y
        && px < x + w
        && py < y + h
}

pub struct Picker {
    stage: Stage,
    result: Option<PickResult>,
    cursor: Option<SystemCursor>,
    started: Instant,
}
impl Default for Picker {
    fn default() -> Self {
        Self {
            stage: Stage::Idle,
            result: None,
            cursor: None,
            started: Instant::now(),
        }
    }
}
impl Picker {
    pub fn active(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }
    pub fn choosing(&self) -> bool {
        match self.stage {
            Stage::Choosing => true,
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
            Stage::ChoosingImage(_) => true,
            _ => false,
        }
    }
    /// Hide both windows only during capture; the entire interactive picking
    /// phase uses our own visible, movable Cancel / Quit controller.
    pub fn hides_windows(&self) -> bool {
        match self.stage {
            Stage::SampleAfter(..) => true,
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
            Stage::Preparing(..) | Stage::Capturing(..) => true,
            _ => false,
        }
    }
    pub fn hides_canvas(&self) -> bool {
        self.hides_windows()
    }
    pub fn start(&mut self, wayland_session: bool) {
        self.cancel();
        self.result = None;
        self.started = Instant::now();
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
        if cfg!(target_os = "macos") || wayland_session {
            self.stage = Stage::Preparing(Instant::now() + Duration::from_millis(120));
            return;
        }
        let _ = wayland_session;
        self.stage = Stage::Choosing;
        self.cursor = SystemCursor::crosshair();
    }
    fn reset_cursor(&mut self) {
        self.cursor = None;
    }
    fn finish(&mut self, result: PickResult) {
        self.stage = Stage::Idle;
        self.result = Some(result);
        self.reset_cursor();
    }
    pub fn cancel(&mut self) {
        // Replacing Capturing drops its job, which signals the macOS capture
        // worker or closes the Wayland portal request immediately. Results
        // from old jobs can never be mistaken for a new pick.
        self.finish(Ok(None));
    }
    pub fn event(&mut self, event: &SDL_Event, window: &Window, toolbar: &Window) {
        if !self.active() {
            return;
        }
        let ty = event_type(event);
        if ty == SDL_EVENT_KEY_DOWN && key_event(event).is_some_and(|key| key.key == SDLK_ESCAPE) {
            self.cancel();
            return;
        }
        if !self.choosing() {
            return;
        }
        if ty == SDL_EVENT_MOUSE_BUTTON_UP
            && mouse_button_event(event)
                .is_some_and(|button| button.windowID == window.id && button.button == 3)
        {
            self.cancel();
            return;
        }
        let local = match ty {
            SDL_EVENT_MOUSE_BUTTON_UP => mouse_button_event(event)
                .filter(|button| {
                    button.windowID == window.id
                        && button.button == 1
                        && button.which != SDL_TOUCH_MOUSEID
                        && button.which != SDL_PEN_MOUSEID
                })
                .map(|button| [button.x, button.y]),
            SDL_EVENT_PEN_UP => pen_touch_event(event)
                .filter(|pen| pen.windowID == window.id)
                .map(|pen| [pen.x, pen.y]),
            SDL_EVENT_FINGER_UP => finger_event(event)
                .filter(|finger| finger.windowID == window.id)
                .map(|finger| {
                    let size = window.size();
                    [finger.x * size[0], finger.y * size[1]]
                }),
            _ => None,
        };
        if let Some(local) = local {
            // A transparent fullscreen canvas may still receive the mouse
            // release when the native controller is above it. Do not turn
            // clicks intended for Cancel / Quit / the draggable grip into
            // a desktop sample. The overlap is recomputed on every click,
            // so moving the controller immediately updates the exclusion.
            if crate::native::controller_bounds(window, toolbar)
                .is_some_and(|bounds| controller_contains(bounds, local))
            {
                return;
            }
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
            if let Stage::ChoosingImage(image) = &self.stage {
                let result = sample_image(image, local, window.size())
                    .map_or(Err("The chosen pixel is outside the display".into()), |color| {
                        Ok(Some(color))
                    });
                self.finish(result);
                return;
            }
            match desktop::position(window, local) {
                Ok(point) => {
                    self.stage =
                        Stage::SampleAfter(point, Instant::now() + Duration::from_millis(100));
                }
                Err(error) => self.finish(Err(error)),
            }
        }
    }
    pub fn poll(
        &mut self,
        window: &Window,
        _toolbar: &Window,
        _display: Option<&Display>,
        _displays: &[Display],
    ) -> Option<PickResult> {
        if self.active() && self.started.elapsed() > Duration::from_secs(120) {
            self.cancel();
            self.result = Some(Err("Screen color selection timed out".into()));
        }
        match &mut self.stage {
            Stage::SampleAfter(point, due) if Instant::now() >= *due => {
                let result = desktop::sample(window, *point).map(Some);
                self.finish(result);
            }
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
            Stage::Preparing(due) if Instant::now() >= *due => {
                let bounds = _display.ok_or_else(|| "The picker display was disconnected".to_owned());
                #[cfg(target_os = "macos")]
                let capture = bounds.and_then(|display| {
                    crate::background::macos::start(
                        display.bounds,
                        window.pixel_size(),
                        crate::background::SnapshotMode::Sdr,
                        Ok,
                    )
                    .map(CaptureJob::Mac)
                });
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                let capture = bounds.and_then(|display| {
                    crate::background::screenshot::Job::start(
                        window,
                        display,
                        _displays,
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::from(window.size())),
                        false,
                        true,
                    )
                    .map(CaptureJob::Portal)
                });
                match capture {
                    Ok(job) => self.stage = Stage::Capturing(job),
                    Err(error) => self.finish(Err(error)),
                }
            }
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
            Stage::Capturing(job) => {
                if let Some(result) = job.poll() {
                    match result {
                        Ok(Some(image)) => {
                            self.stage = Stage::ChoosingImage(image);
                            self.cursor = SystemCursor::crosshair();
                        }
                        Ok(None) => self.finish(Ok(None)),
                        Err(error) => self.finish(Err(error)),
                    }
                }
            }
            _ => {}
        }
        self.result.take()
    }
}
impl Drop for Picker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_picker_samples_rgba8_using_logical_window_coordinates() {
        let mut image = Image::buffer(2, 2).expect("test image allocation");
        image.rgba.copy_from_slice(&[
            255, 0, 0, 255,
            0, 255, 0, 255,
            0, 0, 255, 255,
            255, 255, 255, 255,
        ]);
        let size = [800., 600.];
        assert_eq!(sample_image(&image, [100., 100.], size).map(|c| c.srgb8()), Some([255, 0, 0]));
        assert_eq!(sample_image(&image, [500., 100.], size).map(|c| c.srgb8()), Some([0, 255, 0]));
        assert_eq!(sample_image(&image, [100., 500.], size).map(|c| c.srgb8()), Some([0, 0, 255]));
        assert_eq!(sample_image(&image, [500., 500.], size).map(|c| c.srgb8()), Some([255, 255, 255]));
        assert!(sample_image(&image, [-1., 10.], size).is_none());
        assert!(sample_image(&image, [800., 10.], size).is_none());
        assert!(sample_image(&image, [10., f32::NAN], size).is_none());
        assert!(sample_image(&image, [10., 10.], [0., 600.]).is_none());
        image.rgba.truncate(2);
        assert!(sample_image(&image, [100., 100.], size).is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn cancellation_discards_cached_pixels_and_can_restart() {
        let image = Image::buffer(2, 2).expect("test image allocation");
        let mut picker = Picker::default();
        picker.stage = Stage::ChoosingImage(image);
        assert!(picker.choosing());
        assert!(!picker.hides_windows());
        picker.cancel();
        assert!(!picker.active());
        assert!(matches!(picker.result, Some(Ok(None))));
        picker.start(true);
        assert!(picker.hides_windows());
        assert!(!picker.choosing());
        picker.cancel();
        assert!(!picker.active());
    }

    #[test]
    fn controller_exclusion_moves_with_draggable_toolbar() {
        let bounds = [120., 80., 56., 180.];
        assert!(controller_contains(bounds, [125., 85.]));
        assert!(controller_contains(bounds, [125., 240.]));
        assert!(!controller_contains(bounds, [119., 85.]));
        assert!(!controller_contains(bounds, [176., 85.]));
        assert!(!controller_contains(bounds, [125., 260.]));
        assert!(!controller_contains([400., 80., 56., 180.], [125., 85.]));
        assert!(!controller_contains(
            [f32::NAN, 80., 56., 180.],
            [125., 85.]
        ));
        assert!(!controller_contains([120., 80., 0., 180.], [125., 85.]));
    }

    #[test]
    fn cancellation_resets_desktop_and_delayed_system_selection() {
        let mut picker = Picker::default();
        picker.start(false);
        assert_eq!(picker.choosing(), !cfg!(target_os = "macos"));
        assert_eq!(picker.hides_windows(), cfg!(target_os = "macos"));
        assert_eq!(picker.hides_canvas(), cfg!(target_os = "macos"));
        picker.cancel();
        assert!(!picker.active());
        assert!(matches!(picker.result, Some(Ok(None))));
        picker.start(true);
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
        assert!(picker.hides_windows());
        #[cfg(target_os = "windows")]
        assert!(!picker.hides_canvas());
        picker.cancel();
        assert!(!picker.active());
    }
}
