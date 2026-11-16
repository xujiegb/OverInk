use crate::{
    appearance::{Language, Preferences},
    background::{Image, screenshot::Job},
    display::Display,
    sdl::*,
    toolbar,
};
use egui::{Color32, Pos2, Rect, Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

const MENU_SIZE: Vec2 = vec2(286., 42.);
const CAPTURE_DELAY: Duration = Duration::from_millis(120);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
pub enum MenuAction {
    Save,
    SaveAs,
    Cancel,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Selecting(Option<(Pos2, Pos2)>),
    Preparing(Rect),
    Waiting(Rect, Instant),
    Capturing(Rect, Job, Instant),
    Ready(Rect, Arc<Image>),
}
type SaveResult = Result<PathBuf, String>;

#[derive(Default)]
pub struct Screenshot {
    stage: Stage,
    target: SDL_WindowID,
    hdr: bool,
    sdr_white: f32,
    cursor: Option<Crosshair>,
    action: Option<MenuAction>,
    save_job: Option<(bool, mpsc::Receiver<SaveResult>)>,
    error: Option<String>,
    save_dialog: bool,
}

impl Screenshot {
    pub fn start(&mut self, window: &Window, hdr: bool) {
        self.target = window.id;
        self.hdr = hdr;
        let white = unsafe {
            SDL_GetFloatProperty(
                SDL_GetWindowProperties(window.raw),
                SDL_PROP_WINDOW_SDR_WHITE_LEVEL_FLOAT,
                1.,
            )
        };
        self.sdr_white = if white.is_finite() && white > 0. {
            white
        } else {
            1.
        };
        self.stage = Stage::Selecting(None);
        self.cursor = Crosshair::new();
        self.error = None;
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
        if matches!(self.stage, Stage::Selecting(Some(_))) {
            unsafe { SDL_CaptureMouse(false) };
        }
        self.stage = Stage::Idle;
        self.cursor = None;
        self.action = None;
        self.error = None;
    }

    pub fn event(&mut self, event: &SDL_Event, window: &Window) -> bool {
        let Stage::Selecting(drag) = &mut self.stage else {
            return false;
        };
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::from(window.size()));
        unsafe {
            match SDL_EventType(event.r#type) {
                SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => {
                    let button = event.button;
                    if button.button != SDL_BUTTON_LEFT as u8
                        || button.which == SDL_TOUCH_MOUSEID
                        || button.which == SDL_PEN_MOUSEID
                    {
                        return false;
                    }
                    let point = pos2(button.x, button.y).clamp(bounds.min, bounds.max);
                    if button.down {
                        *drag = Some((point, point));
                        SDL_CaptureMouse(true);
                    } else if let Some((start, _)) = drag.take() {
                        SDL_CaptureMouse(false);
                        let region = Rect::from_two_pos(start, point);
                        if region.width() >= 2. && region.height() >= 2. {
                            self.stage = Stage::Preparing(region);
                            self.cursor = None;
                        }
                    }
                    true
                }
                SDL_EVENT_MOUSE_MOTION
                    if let Some((_, end)) = drag
                        && event.motion.which != SDL_TOUCH_MOUSEID
                        && event.motion.which != SDL_PEN_MOUSEID =>
                {
                    *end = pos2(event.motion.x, event.motion.y).clamp(bounds.min, bounds.max);
                    true
                }
                SDL_EVENT_WINDOW_FOCUS_LOST if drag.take().is_some() => {
                    SDL_CaptureMouse(false);
                    true
                }
                _ => false,
            }
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

    pub fn ui(&mut self, ui: &mut Ui, tr: Language) {
        let screen = ui.clip_rect();
        let region = match self.stage {
            Stage::Selecting(drag) => {
                let region = drag.map(|(start, end)| Rect::from_two_pos(start, end));
                shade(ui, screen, region);
                return;
            }
            Stage::Ready(region, _) => region,
            _ => return,
        };
        shade(ui, screen, Some(region));
        let size = MENU_SIZE + vec2(0., if self.error.is_some() { 42. } else { 0. });
        let rect = toolbar::context_menu_rect(region, screen, size, None);
        ui.painter().rect(
            rect,
            6.,
            ui.visuals().window_fill(),
            ui.visuals().window_stroke(),
            StrokeKind::Inside,
        );
        let mut menu = ui.new_child(
            UiBuilder::new()
                .id_salt("screenshot-actions")
                .max_rect(rect.shrink(5.)),
        );
        menu.spacing_mut().item_spacing.x = 4.;
        menu.add_enabled_ui(!self.save_dialog && !self.saving(), |ui| {
            ui.horizontal(|ui| {
                for (action, label) in [
                    (MenuAction::Save, tr.text("Save", "保存", "儲存")),
                    (
                        MenuAction::SaveAs,
                        tr.text("Save As…", "另存为…", "另存為…"),
                    ),
                    (MenuAction::Cancel, tr.text("Cancel", "取消", "取消")),
                ] {
                    if ui.add_sized([89., 32.], egui::Button::new(label)).clicked() {
                        self.action = Some(action);
                    }
                }
            });
        });
        if let Some(error) = &self.error {
            menu.add(egui::Label::new(error).truncate())
                .on_hover_text(error);
        }
    }

    pub fn action(&mut self) -> Option<MenuAction> {
        self.action.take()
    }

    pub fn show_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn open_save_as(
        &mut self,
        dialog: &mut dialog::FileDialog,
        window: &Window,
        preferences: &Preferences,
    ) {
        let folder = default_folder(preferences)
            .filter(|folder| folder.is_dir())
            .or_else(|| downloads_folder().filter(|folder| folder.is_dir()));
        let location = folder.map(|folder| folder.join(format!("{}.png", file_stem())));
        dialog.open(
            dialog::DialogKind::ScreenshotFile,
            window,
            location.as_deref(),
        );
        self.save_dialog = true;
    }

    pub fn close_save_as(&mut self) {
        self.save_dialog = false;
    }

    pub fn save(&mut self, destination: Destination) -> Result<(), String> {
        let Stage::Ready(_, image) = &self.stage else {
            return Err("No screenshot to save".into());
        };
        let direct = matches!(destination, Destination::Folder(_));
        let image = Arc::clone(image);
        let (hdr, sdr_white) = (self.hdr, self.sdr_white);
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("overink-save-screenshot".into())
            .spawn(move || {
                let _ = sender.send(write_screenshot(&image, destination, hdr, sdr_white));
            })
            .map_err(|error| error.to_string())?;
        self.error = None;
        self.save_job = Some((direct, receiver));
        Ok(())
    }

    pub fn poll_save(&mut self) -> Option<(bool, SaveResult)> {
        let (direct, receiver) = self.save_job.as_ref()?;
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Saving the screenshot was interrupted".into())
            }
        };
        let direct = *direct;
        self.save_job = None;
        Some((direct, result))
    }
}

fn shade(ui: &Ui, screen: Rect, region: Option<Rect>) {
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
            Stroke::new(1.5, toolbar::ACTIVE),
            StrokeKind::Inside,
        );
    } else {
        painter.rect_filled(screen, 0., shade);
    }
}

struct Crosshair(*mut SDL_Cursor);
impl Crosshair {
    fn new() -> Option<Self> {
        unsafe {
            let cursor = SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_CROSSHAIR);
            if cursor.is_null() {
                return None;
            }
            SDL_SetCursor(cursor);
            Some(Self(cursor))
        }
    }
}
impl Drop for Crosshair {
    fn drop(&mut self) {
        unsafe {
            SDL_SetCursor(SDL_GetDefaultCursor());
            SDL_DestroyCursor(self.0);
        }
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

pub fn unsupported_hdr(tr: Language) -> &'static str {
    tr.text(
        "HDR screenshots require Windows or macOS 15 or later.",
        "HDR 截图需要 Windows 或 macOS 15 及以上版本。",
        "HDR 螢幕擷取需要 Windows 或 macOS 15 以上版本。",
    )
}

pub enum Destination {
    Folder(PathBuf),
    File(PathBuf),
}

fn file_stem() -> String {
    use sdl3_sys::time::{SDL_DateTime, SDL_GetCurrentTime, SDL_TimeToDateTime};
    let mut time = 0;
    let mut date = SDL_DateTime::default();
    if unsafe { SDL_GetCurrentTime(&mut time) && SDL_TimeToDateTime(time, &mut date, true) } {
        format!(
            "OverInk-{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            date.year, date.month, date.day, date.hour, date.minute, date.second
        )
    } else {
        "OverInk".into()
    }
}

fn write_screenshot(image: &Image, destination: Destination, hdr: bool, white: f32) -> SaveResult {
    let folder = match &destination {
        Destination::Folder(folder) => folder.as_path(),
        Destination::File(file) => file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    };
    let mut output = tempfile::NamedTempFile::new_in(folder).map_err(|error| error.to_string())?;
    let mut writer = BufWriter::new(output.as_file_mut());
    image.write_png(&mut writer, hdr, white)?;
    writer.flush().map_err(|error| error.to_string())?;
    drop(writer);
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
                match output.persist_noclobber(&path) {
                    Ok(_) => return Ok(path),
                    Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        output = error.file
                    }
                    Err(error) => return Err(error.error.to_string()),
                }
            }
            unreachable!()
        }
        Destination::File(path) => {
            let has_png = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
            if has_png {
                output
                    .persist(&path)
                    .map_err(|error| error.error.to_string())?;
                Ok(path)
            } else {
                let mut name = path.into_os_string();
                name.push(".png");
                let path = PathBuf::from(name);
                output
                    .persist_noclobber(&path)
                    .map_err(|error| error.error.to_string())?;
                Ok(path)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_png_never_recreates_a_missing_default_folder_or_overwrites_an_automatic_name() {
        let folder = tempfile::tempdir().unwrap();
        let missing = folder.path().join("missing");
        let mut image = Image::buffer(2, 1).unwrap();
        image
            .rgba
            .copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255]);

        assert!(write_screenshot(&image, Destination::Folder(missing.clone()), false, 1.).is_err());
        assert!(!missing.exists());
        let save = || {
            write_screenshot(&image, Destination::Folder(folder.path().into()), false, 1.).unwrap()
        };
        let (first, second) = (save(), save());
        assert_ne!(first, second);
        assert_eq!(
            image::open(first).unwrap().into_rgb8().into_raw(),
            [255, 0, 0, 0, 255, 0]
        );
        assert!(second.is_file());
    }
}
