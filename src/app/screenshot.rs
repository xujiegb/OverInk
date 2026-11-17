#![forbid(unsafe_code)]

use crate::{
    appearance::{Language, Preferences},
    background::{Image, screenshot::Job},
    display::Display,
    sdl::*,
    toolbar,
    touch::{self, TouchKey},
};
use egui::{Color32, Pos2, Rect, Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2};
use std::{
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const MENU_BUTTON: Vec2 = vec2(85., 35.);
const MENU_PADDING: f32 = 5.;
const MENU_GAP: f32 = 4.;
const MENU_SIZE: Vec2 = vec2(
    3. * MENU_BUTTON.x + 2. * (MENU_GAP + MENU_PADDING),
    MENU_BUTTON.y + 2. * MENU_PADDING,
);
const CAPTURE_DELAY: Duration = Duration::from_millis(120);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
pub enum MenuAction {
    Save,
    SaveAs,
    Replace,
    Cancel,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Selecting(Option<Drag>),
    Preparing(Rect),
    Waiting(Rect, Instant),
    Capturing(Rect, Job, Instant),
    Ready(Rect, Arc<Image>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pointer {
    Mouse(SDL_MouseID),
    Pen(SDL_PenID),
    Touch(TouchKey),
}

#[derive(Clone, Copy)]
struct Drag {
    pointer: Pointer,
    start: Pos2,
    end: Pos2,
}

enum SelectionEvent {
    Down(Pointer, Pos2),
    Move(Pointer, Pos2),
    Up(Pointer, Pos2),
    Cancel(Option<Pointer>),
}

impl SelectionEvent {
    fn from_sdl(event: &SDL_Event, size: [f32; 2], touch_enabled: bool) -> Option<Self> {
        let mouse = |id| id != SDL_TOUCH_MOUSEID && id != SDL_PEN_MOUSEID;
        match event_type(event) {
            ty @ (SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP) => {
                let b = mouse_button_event(event)?;
                if b.button != SDL_BUTTON_LEFT as u8 || !mouse(b.which) {
                    return None;
                }
                let press = if ty == SDL_EVENT_MOUSE_BUTTON_DOWN {
                    Self::Down
                } else {
                    Self::Up
                };
                Some(press(Pointer::Mouse(b.which), pos2(b.x, b.y)))
            }
            SDL_EVENT_MOUSE_MOTION => {
                let m = mouse_motion_event(event)?;
                mouse(m.which).then_some(Self::Move(Pointer::Mouse(m.which), pos2(m.x, m.y)))
            }
            ty @ (SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP) => {
                let p = pen_touch_event(event)?;
                if p.eraser {
                    return None;
                }
                let press = if ty == SDL_EVENT_PEN_DOWN {
                    Self::Down
                } else {
                    Self::Up
                };
                Some(press(Pointer::Pen(p.which), pos2(p.x, p.y)))
            }
            SDL_EVENT_PEN_MOTION => {
                let p = pen_motion_event(event)?;
                Some(Self::Move(Pointer::Pen(p.which), pos2(p.x, p.y)))
            }
            SDL_EVENT_PEN_PROXIMITY_OUT => Some(Self::Cancel(Some(Pointer::Pen(
                pen_proximity_event(event)?.which,
            )))),
            ty @ (SDL_EVENT_FINGER_DOWN | SDL_EVENT_FINGER_MOTION | SDL_EVENT_FINGER_UP)
                if touch_enabled =>
            {
                let finger = finger_event(event)?;
                let pointer = Pointer::Touch(finger.into());
                match ty {
                    SDL_EVENT_FINGER_DOWN if !touch::is_indirect(finger.touchID) => {
                        touch::point(finger, size).map(|p| Self::Down(pointer, pos2(p.x, p.y)))
                    }
                    SDL_EVENT_FINGER_MOTION => {
                        touch::point(finger, size).map(|p| Self::Move(pointer, pos2(p.x, p.y)))
                    }
                    SDL_EVENT_FINGER_UP => Some(
                        touch::point(finger, size).map_or(Self::Cancel(Some(pointer)), |p| {
                            Self::Up(pointer, pos2(p.x, p.y))
                        }),
                    ),
                    _ => None,
                }
            }
            SDL_EVENT_FINGER_CANCELED => Some(Self::Cancel(Some(Pointer::Touch(
                finger_event(event)?.into(),
            )))),
            SDL_EVENT_WINDOW_FOCUS_LOST => Some(Self::Cancel(None)),
            _ => None,
        }
    }
}

fn capture_mouse(pointer: Pointer, captured: bool) -> bool {
    if matches!(pointer, Pointer::Mouse(_)) {
        crate::sdl::capture_mouse(captured)
    } else {
        true
    }
}

#[derive(Debug)]
pub enum SaveError {
    Failed(String),
    Exists(PathBuf),
    Cancelled,
}

impl From<String> for SaveError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

type SaveResult = Result<PathBuf, SaveError>;

// Serialize cancellation against the first irreversible filesystem operation.
const SAVE_PENDING: u8 = 0;
const SAVE_CANCELLED: u8 = 1;
const SAVE_PUBLISHING: u8 = 2;
const SAVE_CANCEL_REQUESTED: u8 = 3;

struct SaveJob {
    direct: bool,
    receiver: mpsc::Receiver<SaveResult>,
    cancelled: Arc<AtomicU8>,
}

impl SaveJob {
    fn try_cancel(&self) -> bool {
        loop {
            match self.cancelled.load(Ordering::Acquire) {
                SAVE_PENDING => {
                    if self
                        .cancelled
                        .compare_exchange(
                            SAVE_PENDING,
                            SAVE_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                SAVE_PUBLISHING => {
                    if self
                        .cancelled
                        .compare_exchange(
                            SAVE_PUBLISHING,
                            SAVE_CANCEL_REQUESTED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return false;
                    }
                }
                SAVE_CANCELLED => return true,
                SAVE_CANCEL_REQUESTED => return false,
                _ => unreachable!("invalid screenshot save state"),
            }
        }
    }
}

impl Drop for SaveJob {
    fn drop(&mut self) {
        let _ = self.try_cancel();
    }
}

#[derive(Default)]
pub struct Screenshot {
    stage: Stage,
    target: SDL_WindowID,
    hdr: bool,
    sdr_white: f32,
    cursor: Option<SystemCursor>,
    action: Option<MenuAction>,
    save_job: Option<SaveJob>,
    error: Option<String>,
    cancel_blocked: bool,
    replace: Option<PathBuf>,
    save_dialog: bool,
}

impl Screenshot {
    pub fn start(&mut self, window: &Window, hdr: bool) {
        self.target = window.id;
        self.hdr = hdr && hdr_available(window);
        let white = window.sdr_white_level();
        self.sdr_white = if white.is_finite() && white > 0. {
            white
        } else {
            1.
        };
        self.stage = Stage::Selecting(None);
        self.cursor = SystemCursor::crosshair();
        self.error = None;
        self.cancel_blocked = false;
        self.replace = None;
    }

    pub fn active(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }

    pub fn targets(&self, window: SDL_WindowID) -> bool {
        self.active() && self.target == window
    }

    pub fn ready(&self) -> bool {
        matches!(self.stage, Stage::Ready(..))
    }

    pub fn waiting(&self) -> bool {
        matches!(self.stage, Stage::Waiting(..) | Stage::Capturing(..)) || self.saving()
    }

    pub fn saving(&self) -> bool {
        self.save_job.is_some()
    }

    pub fn cancel(&mut self) {
        // A file already being published cannot be promised as cancelled.
        if self.save_job.as_ref().is_some_and(|job| !job.try_cancel()) {
            self.cancel_blocked = true;
            return;
        }
        self.clear_capture();
        self.save_job = None;
    }

    pub fn abandon(&mut self) {
        // A display can disappear while a file is being published. Detach its
        // screenshot UI but keep the save receiver until publication finishes.
        if self.save_job.as_ref().is_some_and(SaveJob::try_cancel) {
            self.save_job = None;
        }
        self.clear_capture();
    }

    fn clear_capture(&mut self) {
        if let Stage::Selecting(Some(drag)) = self.stage {
            capture_mouse(drag.pointer, false);
        }
        self.stage = Stage::Idle;
        self.cursor = None;
        self.action = None;
        self.error = None;
        self.cancel_blocked = false;
        self.replace = None;
        self.save_dialog = false;
    }

    pub fn event(&mut self, event: &SDL_Event, window: &Window, touch_enabled: bool) -> bool {
        if !matches!(self.stage, Stage::Selecting(_)) {
            return false;
        }
        let size = window.size();
        SelectionEvent::from_sdl(event, size, touch_enabled)
            .is_some_and(|event| self.select(event, size))
    }

    pub fn recover_mouse_drag(&mut self, released: bool) -> bool {
        if released
            && matches!(
                self.stage,
                Stage::Selecting(Some(Drag {
                    pointer: Pointer::Mouse(_),
                    ..
                }))
            )
        {
            if let Stage::Selecting(Some(drag)) = self.stage {
                capture_mouse(drag.pointer, false);
            }
            self.stage = Stage::Selecting(None);
            self.error = None;
            return true;
        }
        false
    }

    pub fn reject_touch_drag(&mut self) -> bool {
        if matches!(
            self.stage,
            Stage::Selecting(Some(Drag {
                pointer: Pointer::Touch(_),
                ..
            }))
        ) {
            self.stage = Stage::Selecting(None);
            return true;
        }
        false
    }

    fn select(&mut self, event: SelectionEvent, size: [f32; 2]) -> bool {
        let Stage::Selecting(drag) = &mut self.stage else {
            return false;
        };
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        let clamp = |p: Pos2| p.clamp(bounds.min, bounds.max);
        match event {
            SelectionEvent::Down(pointer, point) if drag.is_none() && point.is_finite() => {
                let start = clamp(point);
                // SDL capture is not available on every backend. Still allow
                // in-window dragging; losing focus cancels an unfinished drag.
                let _ = capture_mouse(pointer, true);
                *drag = Some(Drag {
                    pointer,
                    start,
                    end: start,
                });
                true
            }
            SelectionEvent::Move(pointer, point)
                if let Some(drag) = drag
                    && drag.pointer == pointer
                    && point.is_finite() =>
            {
                let end = clamp(point);
                std::mem::replace(&mut drag.end, end) != end
            }
            SelectionEvent::Up(pointer, point)
                if let Some(current) = *drag
                    && current.pointer == pointer =>
            {
                *drag = None;
                capture_mouse(pointer, false);
                self.error = None;
                if point.is_finite() {
                    let region = Rect::from_two_pos(current.start, clamp(point));
                    if region.width() >= 2. && region.height() >= 2. {
                        self.stage = Stage::Preparing(region);
                        self.cursor = None;
                    }
                }
                true
            }
            SelectionEvent::Cancel(pointer)
                if let Some(current) = *drag
                    && pointer.is_none_or(|p| p == current.pointer) =>
            {
                *drag = None;
                capture_mouse(current.pointer, false);
                true
            }
            _ => false,
        }
    }

    pub fn presented(&mut self) {
        if let Stage::Preparing(region) = self.stage {
            self.stage = Stage::Waiting(region, Instant::now() + CAPTURE_DELAY);
        }
    }

    pub fn poll_capture(
        &mut self,
        window: &Window,
        displays: &[Display],
        portal: bool,
    ) -> Option<Result<bool, String>> {
        let result = match &mut self.stage {
            Stage::Waiting(region, due) if Instant::now() >= *due => {
                let display = displays
                    .iter()
                    .find(|display| Some(display.id) == window.display_id());
                let job = display
                    .ok_or_else(|| "The screenshot display is unavailable".to_owned())
                    .and_then(|display| {
                        Job::start(window, display, displays, *region, self.hdr, portal)
                    });
                match job {
                    Ok(job) => {
                        self.stage = Stage::Capturing(*region, job, Instant::now());
                        return None;
                    }
                    Err(error) => Err(error),
                }
            }
            Stage::Capturing(_, _, started) if started.elapsed() > CAPTURE_TIMEOUT => {
                Err("The screenshot timed out".into())
            }
            Stage::Capturing(region, job, _) => match job.poll()? {
                Ok(Some(image)) => {
                    self.stage = Stage::Ready(*region, Arc::new(image));
                    return Some(Ok(true));
                }
                result => result,
            },
            _ => return None,
        };
        self.cancel();
        Some(result.map(|_| false))
    }

    pub fn ui(&mut self, ui: &mut Ui, tr: Language, units: f32) {
        let screen = ui.clip_rect();
        let region = match self.stage {
            Stage::Selecting(drag) => {
                let region = drag.map(|drag| Rect::from_two_pos(drag.start, drag.end));
                shade(ui, screen, region, units);
                return;
            }
            Stage::Ready(region, _) => region,
            _ => return,
        };
        shade(ui, screen, Some(region), units);
        let replace = self.replace.as_ref().map(|path| {
            format!(
                "{} {}",
                path.display(),
                tr.text(
                    "already exists. Replace it?",
                    "已存在。要替换吗？",
                    "已存在。要取代嗎？"
                )
            )
        });
        let detail = self
            .cancel_blocked
            .then(|| {
                tr.text(
                    "The screenshot is being saved and can no longer be cancelled",
                    "截图正在保存，已无法取消",
                    "螢幕擷取正在儲存，已無法取消",
                )
            })
            .or(self.error.as_deref())
            .or(replace.as_deref());
        let size = MENU_SIZE + vec2(0., if detail.is_some() { 26. } else { 0. });
        let scale = toolbar::menu_scale(screen, size, units);
        let rect = toolbar::context_menu_rect(region, screen, size * scale, None, scale);
        ui.painter().rect(
            rect,
            8. * scale,
            ui.visuals().window_fill(),
            ui.visuals().window_stroke(),
            StrokeKind::Inside,
        );
        let mut menu = ui.new_child(
            UiBuilder::new()
                .id_salt("screenshot-actions")
                .max_rect(rect.shrink(MENU_PADDING * scale)),
        );
        menu.spacing_mut().item_spacing = Vec2::splat(MENU_GAP * scale);
        menu.spacing_mut().button_padding = vec2(6., 4.) * scale;
        let primary = if self.replace.is_some() {
            (MenuAction::Replace, tr.text("Replace", "替换", "取代"))
        } else {
            (MenuAction::Save, tr.text("Save", "保存", "儲存"))
        };
        let saving = self.saving();
        menu.horizontal(|ui| {
            for (action, label) in [
                primary,
                (
                    MenuAction::SaveAs,
                    tr.text("Save As…", "另存为…", "另存為…"),
                ),
                (MenuAction::Cancel, tr.text("Cancel", "取消", "取消")),
            ] {
                let enabled =
                    !self.save_dialog && (!saving || matches!(action, MenuAction::Cancel));
                if ui
                    .add_enabled(
                        enabled,
                        toolbar::menu_button(label, scale).min_size(MENU_BUTTON * scale),
                    )
                    .clicked()
                {
                    self.action = Some(action);
                }
            }
        });
        if let Some(detail) = detail {
            let detail = egui::RichText::new(detail).size(toolbar::MENU_FONT_SIZE * scale);
            menu.add(egui::Label::new(detail.clone()).truncate())
                .on_hover_text(detail);
        }
    }

    pub fn action(&mut self) -> Option<MenuAction> {
        self.action.take()
    }

    pub fn show_error(&mut self, message: String) {
        self.cancel_blocked = false;
        self.error = Some(message);
    }

    pub fn confirm_replace(&mut self, path: PathBuf) {
        self.replace = Some(path);
    }

    pub fn replace_file(&mut self) -> Result<(), String> {
        let path = self
            .replace
            .clone()
            .ok_or("No screenshot file to replace")?;
        self.save(Destination::Replace(path))
    }

    pub fn open_save_as(
        &mut self,
        dialog: &mut dialog::FileDialog,
        window: &Window,
        preferences: &Preferences,
    ) {
        let folder = if self.error.is_some() {
            downloads_folder()
        } else {
            default_folder(preferences)
        };
        let location = self
            .replace
            .clone()
            .or_else(|| folder.map(|folder| folder.join(format!("{}.png", file_stem()))));
        self.save_dialog |= dialog.open(
            dialog::DialogKind::ScreenshotFile,
            window,
            location.as_deref(),
        );
    }

    pub fn close_save_as(&mut self) {
        self.save_dialog = false;
    }

    pub fn save(&mut self, destination: Destination) -> Result<(), String> {
        let Stage::Ready(_, image) = &self.stage else {
            return Err("No screenshot to save".into());
        };
        if self.saving() {
            return Err("A screenshot save is already in progress".into());
        }
        let direct = matches!(destination, Destination::Folder(_));
        let image = Arc::clone(image);
        let (hdr, sdr_white) = (self.hdr, self.sdr_white);
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicU8::new(SAVE_PENDING));
        let worker_cancelled = Arc::clone(&cancelled);
        std::thread::Builder::new()
            .name("overink-save-screenshot".into())
            .spawn(move || {
                let _ = sender.send(write_screenshot(
                    &image,
                    destination,
                    hdr,
                    sdr_white,
                    &worker_cancelled,
                ));
            })
            .map_err(|error| error.to_string())?;
        self.error = None;
        self.cancel_blocked = false;
        self.replace = None;
        self.save_job = Some(SaveJob {
            direct,
            receiver,
            cancelled,
        });
        Ok(())
    }

    pub fn poll_save(&mut self) -> Option<(bool, SaveResult)> {
        let job = self.save_job.as_ref()?;
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(SaveError::Failed(
                "Saving the screenshot was interrupted".into(),
            )),
        };
        let direct = job.direct;
        self.save_job = None;
        self.cancel_blocked = false;
        Some((direct, result))
    }
}

fn shade(ui: &Ui, screen: Rect, region: Option<Rect>, units: f32) {
    let shade = Color32::from_black_alpha(76);
    let painter = ui.painter();
    if let Some(region) = region {
        for rect in [
            Rect::from_min_max(screen.min, pos2(screen.right(), region.top())),
            Rect::from_min_max(pos2(screen.left(), region.bottom()), screen.max),
            Rect::from_min_max(pos2(screen.left(), region.top()), region.left_bottom()),
            Rect::from_min_max(region.right_top(), pos2(screen.right(), region.bottom())),
        ] {
            painter.rect_filled(rect, 0., shade);
        }
        painter.rect_stroke(
            region,
            0.,
            Stroke::new(1.5 * units, toolbar::ACTIVE),
            StrokeKind::Inside,
        );
    } else {
        painter.rect_filled(screen, 0., shade);
    }
}

pub fn default_folder(preferences: &Preferences) -> Option<PathBuf> {
    preferences
        .screenshot_folder
        .clone()
        .or_else(downloads_folder)
}

fn downloads_folder() -> Option<PathBuf> {
    dialog::downloads().or_else(|| {
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::var_os(home).map(|path| PathBuf::from(path).join("Downloads"))
    })
}

pub fn hdr_available(window: &Window) -> bool {
    window.hdr_enabled() && crate::background::screenshot::hdr_supported()
}

pub fn unsupported_hdr(tr: Language) -> &'static str {
    tr.text(
        "HDR capture is unavailable for this display. Screenshots are saved in SDR.",
        "当前显示器无法进行 HDR 截图，将保存为 SDR。",
        "目前的顯示器無法進行 HDR 螢幕擷取，將儲存為 SDR。",
    )
}

pub enum Destination {
    Folder(PathBuf),
    File(PathBuf),
    Replace(PathBuf),
}

fn file_stem() -> String {
    crate::sdl::screenshot_file_stem()
}

struct CancellableWriter<'a, W> {
    writer: W,
    cancelled: &'a AtomicU8,
}

impl<W: Write> Write for CancellableWriter<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        check_cancelled(self.cancelled)?;
        self.writer.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        check_cancelled(self.cancelled)?;
        self.writer.flush()
    }
}

fn check_cancelled(cancelled: &AtomicU8) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) == SAVE_CANCELLED {
        return Err(io::Error::other("Screenshot save cancelled"));
    }
    Ok(())
}

fn begin_publish(cancelled: &AtomicU8) -> io::Result<()> {
    cancelled
        .compare_exchange(
            SAVE_PENDING,
            SAVE_PUBLISHING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| io::Error::other("Screenshot save cancelled"))
}

// Preserve cancellation as a distinct result even when a PNG writer surfaces
// it through an ordinary io::Error or an encoder error string.
fn save_failure(cancelled: &AtomicU8, error: impl ToString) -> SaveError {
    if cancelled.load(Ordering::Acquire) == SAVE_CANCELLED {
        SaveError::Cancelled
    } else {
        SaveError::Failed(error.to_string())
    }
}

fn retry_after_name_conflict(cancelled: &AtomicU8) -> Result<(), SaveError> {
    if cancelled
        .compare_exchange(
            SAVE_PUBLISHING,
            SAVE_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        cancelled.store(SAVE_CANCELLED, Ordering::Release);
        return Err(SaveError::Cancelled);
    }
    Ok(())
}

fn png_path(path: PathBuf) -> PathBuf {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    {
        return path;
    }
    let mut name = path.into_os_string();
    name.push(".png");
    name.into()
}

fn write_screenshot(
    image: &Image,
    destination: Destination,
    hdr: bool,
    white: f32,
    cancelled: &AtomicU8,
) -> SaveResult {
    check_cancelled(cancelled).map_err(|error| save_failure(cancelled, error))?;
    let destination = match destination {
        Destination::File(path) => Destination::File(png_path(path)),
        folder => folder,
    };
    let folder = match &destination {
        Destination::Folder(folder) => folder.as_path(),
        Destination::File(file) | Destination::Replace(file) => file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    };
    let mut output = tempfile::NamedTempFile::new_in(folder).map_err(|error| error.to_string())?;
    let mut writer = BufWriter::new(CancellableWriter {
        writer: output.as_file_mut(),
        cancelled,
    });
    image
        .write_png(&mut writer, hdr, white)
        .map_err(|error| save_failure(cancelled, error))?;
    writer
        .flush()
        .map_err(|error| save_failure(cancelled, error))?;
    drop(writer);
    // Cancellation releases the UI immediately. Check before publishing a
    // file, including each retry. An in-flight filesystem call may still finish.
    match destination {
        Destination::Folder(folder) => {
            let stem = file_stem();
            for suffix in 0.. {
                let name = if suffix == 0 {
                    format!("{stem}.png")
                } else {
                    format!("{stem}-{suffix}.png")
                };
                let path = folder.join(name);
                begin_publish(cancelled).map_err(|_| SaveError::Cancelled)?;
                match output.persist_noclobber(&path) {
                    Ok(_) => return Ok(path),
                    Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        output = error.file;
                        retry_after_name_conflict(cancelled)?;
                    }
                    Err(error) => return Err(error.error.to_string().into()),
                }
            }
            unreachable!()
        }
        Destination::File(path) => {
            begin_publish(cancelled).map_err(|_| SaveError::Cancelled)?;
            output.persist_noclobber(&path).map_err(|error| {
                if error.error.kind() == io::ErrorKind::AlreadyExists {
                    SaveError::Exists(path.clone())
                } else {
                    SaveError::Failed(error.error.to_string())
                }
            })?;
            Ok(path)
        }
        Destination::Replace(path) => {
            begin_publish(cancelled).map_err(|_| SaveError::Cancelled)?;
            output
                .persist(&path)
                .map_err(|error| error.error.to_string())?;
            Ok(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_actions_accept_clicks_in_the_scaled_button_area() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440., 900.));
        let region = Rect::from_min_size(pos2(400., 200.), vec2(320., 240.));
        let units = 2.;
        let rect = toolbar::context_menu_rect(region, screen, MENU_SIZE * units, None, units);
        let pos = rect.min
            + vec2(
                MENU_PADDING + MENU_BUTTON.x + MENU_GAP + MENU_BUTTON.x * 0.5,
                MENU_PADDING + MENU_BUTTON.y * 0.5,
            ) * units;
        let mut screenshot = Screenshot {
            stage: Stage::Ready(region, Arc::new(Image::buffer(1, 1).unwrap())),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut render = |events| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ui| {
                    screenshot.ui(ui, Language::English, units);
                },
            )
            .drop_without_applying_deltas();
        };
        render(vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            render(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
        }
        assert!(matches!(screenshot.action(), Some(MenuAction::SaveAs)));
    }

    #[test]
    fn saving_png_never_recreates_a_missing_default_folder_or_overwrites_an_automatic_name() {
        let folder = tempfile::tempdir().unwrap();
        let missing = folder.path().join("missing");
        let mut image = Image::buffer(2, 1).unwrap();
        image
            .rgba
            .copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255]);

        let cancelled = AtomicU8::new(SAVE_PENDING);
        assert!(
            write_screenshot(
                &image,
                Destination::Folder(missing.clone()),
                false,
                1.,
                &cancelled
            )
            .is_err()
        );
        assert!(!missing.exists());
        let save = || {
            write_screenshot(
                &image,
                Destination::Folder(folder.path().into()),
                false,
                1.,
                &AtomicU8::new(SAVE_PENDING),
            )
            .unwrap()
        };
        let (first, second) = (save(), save());
        assert_ne!(first, second);
        assert_eq!(
            image::open(first).unwrap().into_rgb8().into_raw(),
            [255, 0, 0, 0, 255, 0]
        );
        assert!(second.is_file());
    }

    #[test]
    fn save_as_confirms_the_final_png_path_before_replacing_it() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("capture.png");
        std::fs::write(&path, b"existing file").unwrap();
        let image = Image::buffer(1, 1).unwrap();
        for chosen in [path.clone(), path.with_extension("")] {
            let result = write_screenshot(
                &image,
                Destination::File(chosen),
                false,
                1.,
                &AtomicU8::new(SAVE_PENDING),
            );
            assert!(matches!(result, Err(SaveError::Exists(existing)) if existing == path));
            assert_eq!(std::fs::read(&path).unwrap(), b"existing file");
        }
        let saved = write_screenshot(
            &image,
            Destination::Replace(path.clone()),
            false,
            1.,
            &AtomicU8::new(SAVE_PENDING),
        );
        assert_eq!(saved.unwrap(), path);
        assert!(image::open(path).is_ok());
        assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 1);
    }

    #[test]
    fn cancelling_save_disconnects_its_result_and_prevents_file_creation() {
        let cancelled = Arc::new(AtomicU8::new(SAVE_PENDING));
        let (sender, receiver) = mpsc::channel();
        let mut screenshot = Screenshot {
            save_job: Some(SaveJob {
                direct: true,
                receiver,
                cancelled: Arc::clone(&cancelled),
            }),
            ..Default::default()
        };
        screenshot.cancel();
        assert_eq!(cancelled.load(Ordering::Acquire), SAVE_CANCELLED);
        let folder = tempfile::tempdir().unwrap();
        let image = Image::buffer(1, 1).unwrap();
        let result = write_screenshot(
            &image,
            Destination::Folder(folder.path().into()),
            false,
            1.,
            &cancelled,
        );
        assert!(result.is_err());
        assert!(sender.send(result).is_err());
        assert!(screenshot.poll_save().is_none());
        assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_cannot_hide_an_already_publishing_save() {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicU8::new(SAVE_PUBLISHING));
        let mut screenshot = Screenshot {
            stage: Stage::Ready(Rect::NOTHING, Arc::new(Image::buffer(1, 1).unwrap())),
            save_job: Some(SaveJob {
                direct: true,
                receiver,
                cancelled: Arc::clone(&cancelled),
            }),
            ..Default::default()
        };
        screenshot.cancel();
        assert!(screenshot.ready());
        assert!(screenshot.saving());
        assert_eq!(cancelled.load(Ordering::Acquire), SAVE_CANCEL_REQUESTED);
        assert!(screenshot.cancel_blocked);
        sender.send(Ok(PathBuf::from("saved.png"))).unwrap();
        assert!(matches!(screenshot.poll_save(), Some((true, Ok(_)))));
        screenshot.cancel();
        assert!(!screenshot.active());
    }

    #[test]
    fn cancelled_during_filename_collision_prevents_retry() {
        let state = AtomicU8::new(SAVE_PUBLISHING);
        state.store(SAVE_CANCEL_REQUESTED, Ordering::Release);
        assert!(matches!(
            retry_after_name_conflict(&state),
            Err(SaveError::Cancelled)
        ));
        assert_eq!(state.load(Ordering::Acquire), SAVE_CANCELLED);

        let state = AtomicU8::new(SAVE_PUBLISHING);
        assert!(retry_after_name_conflict(&state).is_ok());
        assert_eq!(state.load(Ordering::Acquire), SAVE_PENDING);
    }

    #[test]
    fn detached_screenshot_keeps_publishing_save_until_result() {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicU8::new(SAVE_PUBLISHING));
        let mut screenshot = Screenshot {
            stage: Stage::Ready(Rect::NOTHING, Arc::new(Image::buffer(1, 1).unwrap())),
            save_job: Some(SaveJob {
                direct: false,
                receiver,
                cancelled: Arc::clone(&cancelled),
            }),
            ..Default::default()
        };
        screenshot.abandon();
        assert!(!screenshot.active());
        assert!(screenshot.saving());
        assert_eq!(cancelled.load(Ordering::Acquire), SAVE_CANCEL_REQUESTED);
        sender.send(Ok(PathBuf::from("detached.png"))).unwrap();
        assert!(matches!(screenshot.poll_save(), Some((false, Ok(_)))));
        assert!(!screenshot.saving());
    }

    #[test]
    fn detached_screenshot_cancels_save_before_publication() {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicU8::new(SAVE_PENDING));
        let mut screenshot = Screenshot {
            stage: Stage::Ready(Rect::NOTHING, Arc::new(Image::buffer(1, 1).unwrap())),
            save_job: Some(SaveJob {
                direct: true,
                receiver,
                cancelled: Arc::clone(&cancelled),
            }),
            ..Default::default()
        };
        screenshot.abandon();
        assert!(!screenshot.active());
        assert!(!screenshot.saving());
        assert_eq!(cancelled.load(Ordering::Acquire), SAVE_CANCELLED);
        assert!(sender.send(Ok(PathBuf::from("never.png"))).is_err());
    }

    #[test]
    fn mouse_recovery_clears_missing_release() {
        let mut screenshot = Screenshot {
            stage: Stage::Selecting(Some(Drag {
                pointer: Pointer::Mouse(SDL_MouseID(1)),
                start: pos2(1., 1.),
                end: pos2(8., 9.),
            })),
            ..Default::default()
        };
        assert!(!screenshot.recover_mouse_drag(false));
        assert!(screenshot.recover_mouse_drag(true));
        assert!(matches!(screenshot.stage, Stage::Selecting(None)));
    }

    #[test]
    fn cancelling_before_publication_does_not_replace_existing_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("existing.png");
        std::fs::write(&path, b"original").unwrap();
        let state = AtomicU8::new(SAVE_PENDING);
        state.store(SAVE_CANCELLED, Ordering::Release);
        let image = Image::buffer(1, 1).unwrap();
        assert!(matches!(
            write_screenshot(
                &image,
                Destination::Replace(path.clone()),
                false,
                1.,
                &state
            ),
            Err(SaveError::Cancelled)
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }

    #[test]
    fn pen_proximity_cancels_a_touch_selection_without_cancelling_pen_selection() {
        let mut screenshot = Screenshot {
            stage: Stage::Selecting(Some(Drag {
                pointer: Pointer::Touch(TouchKey {
                    device: 3,
                    finger: 5,
                }),
                start: pos2(10., 10.),
                end: pos2(20., 30.),
            })),
            ..Default::default()
        };
        assert!(screenshot.reject_touch_drag());
        assert!(matches!(screenshot.stage, Stage::Selecting(None)));
        assert!(!screenshot.reject_touch_drag());
    }

    #[test]
    fn pen_and_touch_drags_ignore_synthetic_mouse_events_and_other_contacts() {
        let size = [200., 100.];
        let mut screenshot = Screenshot {
            stage: Stage::Selecting(None),
            ..Default::default()
        };
        let pen = |ty, x, y| SDL_Event {
            ptouch: SDL_PenTouchEvent {
                r#type: ty,
                which: SDL_PenID(7),
                x,
                y,
                ..Default::default()
            },
        };
        let touch = |ty, finger, x, y| SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: ty,
                touchID: SDL_TouchID(3),
                fingerID: SDL_FingerID(finger),
                x,
                y,
                ..Default::default()
            },
        };
        let send = |shot: &mut Screenshot, event: SDL_Event| {
            SelectionEvent::from_sdl(&event, size, true)
                .is_some_and(|event| shot.select(event, size))
        };
        assert!(send(&mut screenshot, pen(SDL_EVENT_PEN_DOWN, 10., 20.)));
        assert!(!send(
            &mut screenshot,
            touch(SDL_EVENT_FINGER_DOWN, 1, 0.25, 0.5)
        ));
        assert!(!send(
            &mut screenshot,
            SDL_Event {
                button: SDL_MouseButtonEvent {
                    r#type: SDL_EVENT_MOUSE_BUTTON_UP,
                    which: SDL_PEN_MOUSEID,
                    button: SDL_BUTTON_LEFT as u8,
                    ..Default::default()
                }
            }
        ));
        assert!(send(&mut screenshot, pen(SDL_EVENT_PEN_UP, 220., 60.)));
        assert!(matches!(screenshot.stage, Stage::Preparing(rect)
            if rect.min == pos2(10., 20.) && rect.max == pos2(200., 60.)));

        screenshot.stage = Stage::Selecting(None);
        assert!(send(
            &mut screenshot,
            touch(SDL_EVENT_FINGER_DOWN, 1, 0.25, 0.5)
        ));
        assert!(!send(
            &mut screenshot,
            touch(SDL_EVENT_FINGER_CANCELED, 2, 0., 0.)
        ));
        assert!(send(
            &mut screenshot,
            touch(SDL_EVENT_FINGER_CANCELED, 1, 0., 0.)
        ));
        assert!(matches!(screenshot.stage, Stage::Selecting(None)));
        assert!(send(
            &mut screenshot,
            touch(SDL_EVENT_FINGER_DOWN, 2, 0.25, 0.5)
        ));
        assert!(send(
            &mut screenshot,
            touch(SDL_EVENT_FINGER_UP, 2, 0.75, 0.9)
        ));
        assert!(matches!(screenshot.stage, Stage::Preparing(rect)
            if rect.min == pos2(50., 50.) && rect.max == pos2(150., 90.)));
    }
}
