pub(crate) mod desktop;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod portal;
#[cfg(target_os = "macos")]
use macos::SystemJob;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use portal::SystemJob;
#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
struct SystemJob;
#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
impl SystemJob {
    fn start() -> Result<Self, String> {
        Err("No native color sampler for this display backend".into())
    }
    fn poll(&mut self) -> Option<PickResult> {
        None
    }
    fn cancel(&mut self) {}
}
use crate::{color::InkColor, sdl::*};
use std::{
    ptr,
    time::{Duration, Instant},
};
pub type PickResult = Result<Option<InkColor>, String>;
enum Stage {
    Idle,
    Choosing,
    SampleAfter(desktop::ScreenPoint, Instant),
    LaunchAfter(Instant),
    System(SystemJob),
}
pub struct Picker {
    stage: Stage,
    result: Option<PickResult>,
    cursor: *mut SDL_Cursor,
    started: Instant,
}
impl Default for Picker {
    fn default() -> Self {
        Self {
            stage: Stage::Idle,
            result: None,
            cursor: ptr::null_mut(),
            started: Instant::now(),
        }
    }
}
impl Picker {
    pub fn active(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }
    pub fn choosing(&self) -> bool {
        matches!(self.stage, Stage::Choosing)
    }
    pub fn hides_windows(&self) -> bool {
        matches!(
            self.stage,
            Stage::SampleAfter(..) | Stage::LaunchAfter(..) | Stage::System(..)
        )
    }
    pub fn start(&mut self, wayland_session: bool) {
        self.cancel();
        self.result = None;
        self.started = Instant::now();
        if cfg!(target_os = "macos") || wayland_session {
            self.stage = Stage::LaunchAfter(Instant::now() + Duration::from_millis(100));
        } else {
            self.stage = Stage::Choosing;
            if unsafe { SDL_WasInit(SDL_INIT_VIDEO) } != 0 {
                unsafe {
                    self.cursor = SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_CROSSHAIR);
                    if !self.cursor.is_null() {
                        SDL_SetCursor(self.cursor);
                    }
                }
            }
        }
    }
    fn reset_cursor(&mut self) {
        if !self.cursor.is_null() {
            unsafe {
                SDL_SetCursor(SDL_GetDefaultCursor());
                SDL_DestroyCursor(self.cursor);
            }
            self.cursor = ptr::null_mut();
        }
    }
    fn finish(&mut self, result: PickResult) {
        self.stage = Stage::Idle;
        self.result = Some(result);
        self.reset_cursor();
    }
    pub fn cancel(&mut self) {
        if let Stage::System(ref mut job) = self.stage {
            job.cancel();
        }
        self.finish(Ok(None));
    }
    pub fn event(&mut self, event: &SDL_Event, window: &Window) {
        if !self.active() {
            return;
        }
        unsafe {
            let ty = SDL_EventType(event.r#type);
            if ty == SDL_EVENT_KEY_DOWN && event.key.key == SDLK_ESCAPE {
                self.cancel();
                return;
            }
            if !self.choosing() {
                return;
            }
            if ty == SDL_EVENT_MOUSE_BUTTON_UP
                && event.button.windowID == window.id
                && event.button.button == 3
            {
                self.cancel();
                return;
            }
            let selected = (ty == SDL_EVENT_MOUSE_BUTTON_UP
                && event.button.windowID == window.id
                && event.button.button == 1
                && event.button.which != SDL_TOUCH_MOUSEID
                && event.button.which != SDL_PEN_MOUSEID)
                || (ty == SDL_EVENT_PEN_UP && event.ptouch.windowID == window.id)
                || (ty == SDL_EVENT_FINGER_UP && event.tfinger.windowID == window.id);
            if selected {
                let local = if ty == SDL_EVENT_FINGER_UP {
                    let size = window.size();
                    [event.tfinger.x * size[0], event.tfinger.y * size[1]]
                } else if ty == SDL_EVENT_PEN_UP {
                    [event.ptouch.x, event.ptouch.y]
                } else {
                    [event.button.x, event.button.y]
                };
                match desktop::position(window, local) {
                    Ok(point) => {
                        self.stage =
                            Stage::SampleAfter(point, Instant::now() + Duration::from_millis(100))
                    }
                    Err(e) => self.finish(Err(e)),
                }
            }
        }
    }
    pub fn poll(&mut self, window: &Window) -> Option<PickResult> {
        if self.active() && self.started.elapsed() > Duration::from_secs(120) {
            self.cancel();
            self.result = Some(Err("Screen color selection timed out".into()));
        }
        match &mut self.stage {
            Stage::SampleAfter(point, due) if Instant::now() >= *due => {
                let result = desktop::sample(window, *point).map(Some);
                self.finish(result);
            }
            Stage::LaunchAfter(due) if Instant::now() >= *due => match SystemJob::start() {
                Ok(job) => self.stage = Stage::System(job),
                Err(e) => self.finish(Err(e)),
            },
            Stage::System(job) => {
                if let Some(result) = job.poll() {
                    self.finish(result)
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
    fn cancellation_resets_desktop_and_delayed_system_selection() {
        let mut picker = Picker::default();
        picker.start(false);
        assert_eq!(picker.choosing(), !cfg!(target_os = "macos"));
        assert_eq!(picker.hides_windows(), cfg!(target_os = "macos"));
        picker.cancel();
        assert!(!picker.active());
        assert!(matches!(picker.result, Some(Ok(None))));
        picker.start(true);
        assert!(picker.hides_windows());
        picker.cancel();
        assert!(!picker.active());
    }
}
