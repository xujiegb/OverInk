mod canvas;
mod ink;
mod panel;
mod screenshot;
mod text;

use crate::sdl::dialog::{DialogKind, FileDialog};
use crate::{
    appearance::{Language, Preferences, Status, Text, ThemeMonitor, text},
    color::{InkColor, PRESETS},
    display::{Display, Pages},
    export,
    fonts::TextFonts,
    frame_rate::{FramePacer, FrameRate},
    gui,
    icc::ColorManager,
    model::{Bounds, EraseError, Erasure, History, MAX_POINTS, Point, Stroke, Tool},
    sdl::*,
    touch::{self, TouchKey},
};
use screenshot::{Destination, MenuAction, SaveError, Screenshot};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Default, Clone, Copy)]
struct Pen {
    pressure: f32,
    tilt: [f32; 2],
    has_pressure: bool,
    down: bool,
}
impl Pen {
    fn point(self, x: f32, y: f32) -> Point {
        let pressure = if self.has_pressure {
            self.pressure
        } else {
            0.5
        };
        Point::new(x, y, pressure, self.tilt)
    }
    fn finish(&mut self) {
        self.pressure = 0.;
        self.has_pressure = false;
        self.down = false;
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Mouse(u8),
    Pen(u32),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Ink,
    Erase(u64),
    Laser(u64),
}
#[derive(Clone, Copy)]
struct Contact {
    source: Source,
    gesture: Gesture,
    last: Point,
    // Whether the stroke has received real pressure for its initial points.
    measured_pressure: bool,
}
struct FingerStroke {
    gesture: Gesture,
    stroke: Option<Stroke>,
    fragments: Vec<Stroke>,
    last: Option<Point>,
}
struct LaserSample {
    point: Point,
    at: Instant,
    starts_segment: bool,
}
const LEFT_BUTTON: u8 = SDL_BUTTON_LEFT as u8;
const RIGHT_BUTTON: u8 = SDL_BUTTON_RIGHT as u8;
const LASER_LIFETIME: Duration = Duration::from_millis(900);
const MAX_LASER_SAMPLES: usize = 4096;
const FONT_PATH_FIELD: &str = "annotation-font-path";
#[derive(Default)]
struct Commands {
    mode: bool,
    display: Option<u32>,
    quit: bool,
    pick: bool,
    frame_rate: bool,
    background: bool,
    preferences: bool,
    #[cfg(target_os = "linux")]
    gnome_action: Option<crate::gnome::Action>,
    redraw_canvas: bool,
    choose_font: bool,
    font_path: Option<PathBuf>,
    reset_font: bool,
    screenshot: bool,
    screenshot_folder: bool,
}

enum HistoryAction {
    Undo,
    Redo,
    Clear,
}

struct CanvasInput {
    protected: Option<crate::controller::Rect>,
    pens: HashMap<u32, Pen>,
    pen_proximity: BTreeSet<u32>,
    touches: BTreeMap<TouchKey, FingerStroke>,
    contact: Option<Contact>,
    laser: BTreeMap<u64, VecDeque<LaserSample>>,
    next_gesture: u64,
    pending_erasures: BTreeMap<u64, Erasure>,
    ink_damage: Option<Bounds>,
    canvas_units: f32,
    text: text::TextInput,
}

impl Default for CanvasInput {
    fn default() -> Self {
        Self {
            protected: None,
            pens: HashMap::new(),
            pen_proximity: BTreeSet::new(),
            touches: BTreeMap::new(),
            contact: None,
            laser: BTreeMap::new(),
            next_gesture: 0,
            pending_erasures: BTreeMap::new(),
            ink_damage: None,
            canvas_units: 1.,
            text: text::TextInput::default(),
        }
    }
}

impl CanvasInput {
    fn has_contacts(&self) -> bool {
        self.contact.is_some() || !self.touches.is_empty()
    }

    fn cancel_erasure(&mut self, id: u64) {
        if let Some(draft) = self.pending_erasures.remove(&id)
            && let Some(bounds) = draft.affected(!self.pending_erasures.is_empty())
        {
            Bounds::include(&mut self.ink_damage, bounds);
        }
    }

    fn eraser_budget(&self, id: u64) -> usize {
        Erasure::available_budget(
            self.pending_erasures
                .iter()
                .filter(|(other, _)| **other != id)
                .map(|(_, draft)| draft),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Board {
    White,
    Black,
}

struct App {
    input: CanvasInput,
    parked_input: BTreeMap<u32, CanvasInput>,
    multi: bool,
    history: History,
    tool: Tool,
    color: InkColor,
    hex_entry: crate::color::HexEntry,
    picking: bool,
    width: f32,
    opacity: f32,
    pressure: bool,
    curve: f32,
    pass: bool,
    wait_for_mouse_up: bool,
    hidden: bool,
    board: Option<Board>,
    display: usize,
    displays: Vec<Display>,
    pages: Pages,
    frame_rate: FrameRate,
    preferences: Preferences,
    text_fonts: TextFonts,
    font_path_input: Option<String>,
    screenshot: Screenshot,
    #[cfg(target_os = "linux")]
    gnome_guide: Option<crate::gnome::Guide>,
    language: Language,
    suspended: bool,
    frozen: bool,
    panel: Option<crate::toolbar::Panel>,
    touch_enabled: bool,
    status: Status,
    path: String,
    save_path: PathBuf,
    last_save: Instant,
}
impl App {
    fn new(
        board: bool,
        display: usize,
        displays: Vec<Display>,
        base: &Path,
        preferences: Preferences,
    ) -> Self {
        let save_path = base.join("session.overink.json");
        let mut history = History::default();
        let display = display.min(displays.len().saturating_sub(1));
        let suspended = displays.is_empty();
        let mut pages = Pages::new(displays.get(display).map(|d| d.id));
        pages.sync(&mut history, &displays);
        let mut status = text(
            "Ready. Draw on the canvas. Space switches to mouse mode.",
            "就绪。可在屏幕上书写，按空格键切换到鼠标模式。",
            "就緒。可在螢幕上書寫，按空白鍵切換至滑鼠模式。",
        )
        .into();
        if preferences.restore_session && save_path.exists() {
            match pages.load(&mut history, &save_path, true) {
                Ok(0) => {}
                Ok(pending) => status = pending_pages(pending),
                Err(error) => {
                    status = text("Session restore failed", "恢复会话失败", "還原工作階段失敗")
                        .detail(error)
                }
            }
        }
        Self {
            input: CanvasInput::default(),
            parked_input: BTreeMap::new(),
            multi: false,
            history,
            tool: Tool::Pen,
            color: InkColor::default(),
            hex_entry: crate::color::HexEntry::new(InkColor::default()),
            picking: false,
            width: 5.,
            opacity: 1.,
            pressure: true,
            curve: 1.,
            pass: false,
            wait_for_mouse_up: false,
            hidden: false,
            board: board.then_some(Board::White),
            display,
            displays,
            pages,
            frame_rate: FrameRate::load(&base.join("frame-rate.json")),
            language: preferences.language.unwrap_or_else(Language::system),
            preferences,
            text_fonts: TextFonts::load(&base.join("annotation-font")),
            font_path_input: None,
            screenshot: Screenshot::default(),
            #[cfg(target_os = "linux")]
            gnome_guide: None,
            suspended,
            frozen: false,
            panel: None,
            touch_enabled: true,
            status,
            path: base
                .join("drawing.overink.json")
                .to_string_lossy()
                .into_owned(),
            save_path,
            last_save: Instant::now(),
        }
    }
    fn stroke(&self, tool: Tool) -> Stroke {
        Stroke {
            tool,
            color: self.color,
            width: if tool == Tool::Marker {
                self.width * 3. * self.input.canvas_units
            } else {
                self.width * self.input.canvas_units
            },
            opacity: if tool == Tool::Marker {
                self.opacity * 0.32
            } else if tool == Tool::Pencil {
                self.opacity * 0.68
            } else {
                self.opacity
            },
            pressure_curve: self.curve,
            pressure_enabled: self.pressure
                && matches!(tool, Tool::Pen | Tool::Pencil | Tool::Marker),
            points: vec![],
        }
    }
    fn gesture(&mut self, tool: Tool) -> Gesture {
        match tool {
            Tool::Eraser | Tool::Laser => {
                self.input.next_gesture += 1;
                if tool == Tool::Eraser {
                    Gesture::Erase(self.input.next_gesture)
                } else {
                    Gesture::Laser(self.input.next_gesture)
                }
            }
            _ => Gesture::Ink,
        }
    }
    fn erase_sample(&mut self, id: u64, point: Point) {
        let radius = self.width.max(12.) * self.input.canvas_units;
        let point_count = self.draft_point_count();
        let budget = self.input.eraser_budget(id);
        let draft = self.input.pending_erasures.entry(id).or_default();
        draft.sample(&self.history, point_count, budget, point, radius);
        if let Some(bounds) = draft.take_damage() {
            Bounds::include(&mut self.input.ink_damage, bounds);
        }
    }
    fn commit_erase(&mut self, id: u64) {
        let Some(erase) = self.input.pending_erasures.remove(&id) else {
            return;
        };
        let points = self.draft_point_count();
        if let Err(error) = erase.commit(
            &mut self.history,
            &mut self.input.pending_erasures,
            points,
            &mut self.input.ink_damage,
        ) {
            self.show_sample_limit(match error {
                EraseError::Samples => text(
                    "This erase would exceed the one million sample limit.",
                    "此次擦除会超出一百万采样点上限。",
                    "此次擦除會超過一百萬取樣點上限。",
                ),
                EraseError::Preview => text(
                    "This erase reached the preview limit. Erase in shorter passes.",
                    "此次擦除达到预览上限，请分几次擦除。",
                    "此次擦除達到預覽上限，請分幾次擦除。",
                ),
            });
        }
    }
    fn show_sample_limit(&mut self, message: Text) {
        self.status = message.into();
        self.panel.get_or_insert(crate::toolbar::Panel::Settings);
    }
    fn can_start_ink(&mut self) -> bool {
        if self.draft_point_count() < MAX_POINTS {
            return true;
        }
        self.show_sample_limit(text(
            "One million sample limit reached. Export and clear the canvas.",
            "已达到一百万采样点上限，请导出后清屏。",
            "已達一百萬取樣點上限，請匯出後清除頁面。",
        ));
        false
    }
    fn cancel_gesture(&mut self, gesture: Gesture) {
        match gesture {
            Gesture::Erase(id) => {
                self.input.cancel_erasure(id);
            }
            Gesture::Laser(id) => {
                self.input.laser.remove(&id);
            }
            Gesture::Ink => {}
        }
    }
    fn laser_sample(&mut self, id: u64, point: Point, starts_segment: bool) {
        let trail = self.input.laser.entry(id).or_default();
        let now = Instant::now();
        if !starts_segment
            && let Some(last) = trail.back_mut()
            && last.point.dist(point) <= 0.05
        {
            last.at = now;
            return;
        }
        if trail.len() >= MAX_LASER_SAMPLES {
            trail.pop_front();
        }
        trail.push_back(LaserSample {
            point,
            at: now,
            starts_segment,
        });
    }
    fn begin(&mut self, source: Source, p: Point, eraser: bool) {
        if self.pass
            || self.hidden
            || self.suspended
            || self.tool.is_text()
            || self.input.protected.is_some_and(|r| r.contains(p.x, p.y))
        {
            return;
        }
        self.finish_contact();
        let tool = if eraser { Tool::Eraser } else { self.tool };
        let gesture = self.gesture(tool);
        match gesture {
            Gesture::Erase(id) => self.erase_sample(id, p),
            Gesture::Laser(id) => self.laser_sample(id, p, true),
            Gesture::Ink => {
                if !self.can_start_ink() {
                    return;
                }
                let mut stroke = self.stroke(tool);
                stroke.points.push(p);
                self.history.active = Some(stroke);
            }
        }
        self.input.contact = Some(Contact {
            source,
            gesture,
            last: p,
            measured_pressure: false,
        });
    }
    fn motion(&mut self, source: Source, p: Point) {
        let Some(contact) = self
            .input
            .contact
            .filter(|contact| contact.source == source)
        else {
            return;
        };
        if self
            .input
            .protected
            .is_some_and(|r| r.contains(p.x, p.y) || r.crosses(contact.last, p))
        {
            self.finish_contact();
            return;
        }
        if self.pass || self.hidden || self.suspended {
            return;
        }
        self.input.contact = Some(Contact { last: p, ..contact });
        match contact.gesture {
            Gesture::Erase(id) => self.erase_sample(id, p),
            Gesture::Laser(id) => self.laser_sample(id, p, false),
            Gesture::Ink => {
                let can_append = self.draft_point_count() < MAX_POINTS;
                if let Some(stroke) = &mut self.history.active {
                    Self::append(stroke, p, can_append);
                }
            }
        }
    }
    fn pen_motion(&mut self, source: Source, p: Point, has_pressure: bool) {
        if has_pressure
            && let Some(contact) = &mut self.input.contact
            && contact.source == source
            && !contact.measured_pressure
            && let Some(stroke) = &mut self.history.active
            && stroke.pressure_enabled
        {
            for point in &mut stroke.points {
                point.pressure = p.pressure;
            }
            contact.measured_pressure = true;
        }
        self.motion(source, p);
    }
    fn append(stroke: &mut Stroke, p: Point, can_append: bool) {
        let pressure_enabled = stroke.pressure_enabled;
        let pencil = stroke.tool == Tool::Pencil;
        if stroke.tool.is_shape() {
            if stroke.points.len() == 1 && can_append {
                stroke.points.push(p);
            } else if let Some(end) = stroke.points.get_mut(1) {
                *end = p;
            }
        } else if let Some(last) = stroke.points.last_mut()
            && last.dist(p) <= Point::MERGE_DISTANCE
        {
            last.merge_sample(p, pressure_enabled, pencil);
        } else if can_append
            && stroke.points.len() < 100_000
            && stroke.points.last().is_none_or(|last| {
                last.dist(p) > 0.15
                    || (pressure_enabled && (last.pressure - p.pressure).abs() > 0.00001)
                    || (pencil && last.tilt != p.tilt)
            })
        {
            stroke.points.push(p);
        }
    }
    fn end(&mut self, source: Source) {
        if self
            .input
            .contact
            .is_some_and(|contact| contact.source == source)
        {
            self.finish_contact();
        }
    }
    fn finish_contact(&mut self) {
        if let Some(contact) = self.input.contact.take() {
            if let Gesture::Erase(id) = contact.gesture {
                self.commit_erase(id);
            }
            if let Source::Pen(id) = contact.source
                && let Some(pen) = self.input.pens.get_mut(&id)
            {
                pen.down = false;
            }
        }
        self.history.commit();
    }
    fn discard_touches(&mut self) {
        for finger in std::mem::take(&mut self.input.touches).into_values() {
            self.cancel_gesture(finger.gesture);
        }
    }
    fn stop(&mut self) {
        self.input.text.finish(&mut self.history);
        self.input.text.cancel_press();
        self.finish_contact();
        self.commit_touches();
        self.reset_pens();
    }
    fn reset_pens(&mut self) {
        for pen in self.input.pens.values_mut() {
            pen.finish();
        }
    }
    fn use_mouse(&mut self) {
        self.pass = true;
        self.wait_for_mouse_up = true;
        self.panel = None;
        self.status = text(
            "Mouse mode. Click the desktop or select a drawing tool to resume.",
            "鼠标模式。可点击桌面，选择任一画具即可继续书写。",
            "滑鼠模式。可點按桌面，選擇任一繪圖工具即可繼續書寫。",
        )
        .into();
        self.finish_all_contacts();
    }
    fn use_tool(&mut self, tool: Tool) {
        self.finish_all_contacts();
        self.tool = tool;
        self.pass = false;
        self.wait_for_mouse_up = false;
        self.hidden = false;
        if let Some(hint) = match tool {
            Tool::Text => Some(text(
                "Click to add or edit text. Ctrl/Cmd+Enter finishes.",
                "单击添加或编辑文字，Ctrl/Cmd+Enter 完成。",
                "點按新增或編輯文字，Ctrl/Cmd+Enter 完成。",
            )),
            Tool::Select => Some(text(
                "Drag to move. Double-click to edit. Delete removes the box.",
                "拖动移动，双击编辑，Delete 删除文字框。",
                "拖曳移動，點兩下編輯，Delete 刪除文字框。",
            )),
            _ => None,
        } {
            self.status = hint.into();
        }
    }
    fn toggle_mouse(&mut self) {
        if self.pass {
            self.use_tool(self.tool);
        } else {
            self.use_mouse();
        }
    }
    fn mouse_ready(&mut self) -> bool {
        if self.wait_for_mouse_up && mouse_released() {
            self.wait_for_mouse_up = false;
        }
        self.pass && !self.wait_for_mouse_up
    }
    fn reset_contacts(&mut self) {
        self.input.text = text::TextInput::default();
        self.history.active = None;
        self.input.contact = None;
        self.input.touches.clear();
        for draft in self.input.pending_erasures.values() {
            if let Some(bounds) = draft.affected(self.input.pending_erasures.len() > 1) {
                Bounds::include(&mut self.input.ink_damage, bounds);
            }
        }
        self.input.pending_erasures.clear();
        self.reset_pens();
        self.input.laser.clear();
    }
    fn change_history(&mut self, action: HistoryAction) {
        let draft = match action {
            HistoryAction::Undo => {
                self.input.text.finish(&mut self.history);
                None
            }
            HistoryAction::Redo => None,
            HistoryAction::Clear => self.input.text.take_draft(&self.history),
        };
        self.reset_contacts();
        match action {
            HistoryAction::Undo => self.history.undo(),
            HistoryAction::Redo => self.history.redo(),
            HistoryAction::Clear => self.history.clear_with_text(draft),
        }
    }
    fn swap_page(&mut self, id: u32) {
        let previous = self.pages.active;
        if !self.pages.switch(&mut self.history, id) {
            return;
        }
        let next = self.parked_input.remove(&id).unwrap_or_default();
        let previous_input = std::mem::replace(&mut self.input, next);
        if let Some(previous) = previous {
            self.parked_input.insert(previous, previous_input);
        }
        self.display = self.displays.iter().position(|d| d.id == id).unwrap_or(0);
    }
    fn select_page(&mut self, id: u32) {
        if self.pages.active != Some(id) {
            if self.multi {
                self.input.text.deselect(&mut self.history);
                self.swap_page(id);
            } else {
                self.stop();
                self.reset_contacts();
                self.pages.switch(&mut self.history, id);
                self.display = self.displays.iter().position(|d| d.id == id).unwrap_or(0);
            }
        }
    }
    fn on_page<T>(&mut self, id: Option<u32>, f: impl FnOnce(&mut Self) -> T) -> T {
        let previous = self.pages.active;
        if let (Some(previous), Some(id)) = (previous, id)
            && previous != id
        {
            self.swap_page(id);
            let result = f(self);
            self.swap_page(previous);
            result
        } else {
            f(self)
        }
    }
    fn for_each_page(&mut self, mut f: impl FnMut(&mut Self)) {
        f(self);
        for id in self.parked_input.keys().copied().collect::<Vec<_>>() {
            self.on_page(Some(id), &mut f);
        }
    }
    fn finish_all_contacts(&mut self) {
        self.for_each_page(|app| {
            app.stop();
            app.reset_contacts();
        });
    }
    fn has_contacts(&self) -> bool {
        self.input.has_contacts() || self.parked_input.values().any(CanvasInput::has_contacts)
    }
    fn touch_page(&self, key: TouchKey) -> Option<u32> {
        if self.input.touches.contains_key(&key) {
            self.pages.active
        } else {
            self.parked_input
                .iter()
                .find_map(|(id, input)| input.touches.contains_key(&key).then_some(*id))
        }
    }
    fn refresh_displays(&mut self, displays: Vec<Display>, preferred: Option<u32>) -> (bool, bool) {
        let old = self
            .displays
            .iter()
            .find(|d| Some(d.id) == self.pages.active)
            .cloned();
        let removed = self
            .displays
            .iter()
            .any(|old| !displays.iter().any(|d| d.id == old.id));
        if self.pages.sync(&mut self.history, &displays) {
            self.input = CanvasInput::default();
        }
        self.parked_input
            .retain(|id, _| displays.iter().any(|d| d.id == *id));
        let target = preferred
            .filter(|id| displays.iter().any(|d| d.id == *id))
            .or(self.pages.active)
            .or_else(|| displays.first().map(|d| d.id));
        self.displays = displays;
        if let Some(id) = target {
            self.select_page(id);
        } else {
            self.reset_contacts();
        }
        self.suspended = target.is_none();
        self.display = self
            .displays
            .iter()
            .position(|d| Some(d.id) == target)
            .unwrap_or(0);
        let new = self.displays.get(self.display);
        let layout = old.as_ref().map(|d| (d.id, d.bounds, d.scale))
            != new.map(|d| (d.id, d.bounds, d.scale));
        if removed {
            self.status = text(
                "Display disconnected. Its ink and undo history were cleared.",
                "显示器已断开，已清除其笔迹和撤销记录。",
                "顯示器已中斷連線，已清除其筆跡與復原記錄。",
            )
            .into();
        }
        (layout, removed)
    }
    fn refresh_hz(&self) -> f64 {
        self.displays.get(self.display).map_or(0.0, |d| d.refresh)
    }
    fn draft_point_count(&self) -> usize {
        self.history.point_count()
            + self.history.active.as_ref().map_or(0, |s| s.points.len())
            + self
                .input
                .touches
                .values()
                .flat_map(|f| f.fragments.iter().chain(f.stroke.iter()))
                .map(|s| s.points.len())
                .sum::<usize>()
    }
    fn history_buttons(&self) -> [bool; 3] {
        [
            self.history.can_undo() || self.input.text.has_edit(&self.history),
            self.history.can_redo(),
            !self.history.document().is_empty()
                || self.input.text.editing()
                || self.history.active.is_some()
                || self
                    .input
                    .touches
                    .values()
                    .any(|finger| finger.stroke.is_some()),
        ]
    }
    #[cfg(any(target_os = "macos", test))]
    fn has_wide_ink(&self) -> bool {
        self.history
            .strokes()
            .iter()
            .map(|stroke| stroke.as_ref())
            .chain(self.history.active.iter())
            .chain(
                self.input
                    .touches
                    .values()
                    .flat_map(|finger| finger.fragments.iter().chain(finger.stroke.iter())),
            )
            .any(|stroke| !stroke.points.is_empty() && stroke.color.needs_wide_gamut())
    }
    fn commit_touch(&mut self, finger: FingerStroke) {
        if let Gesture::Erase(id) = finger.gesture {
            self.commit_erase(id);
        }
        let strokes = finger.fragments.into_iter().chain(
            finger
                .stroke
                .into_iter()
                .filter(|stroke| !stroke.points.is_empty()),
        );
        self.history.append(strokes);
    }
    fn commit_touches(&mut self) {
        while let Some((_, finger)) = self.input.touches.pop_first() {
            self.commit_touch(finger);
        }
    }
    fn touch_motion(&mut self, key: TouchKey, point: Point) {
        let point_count = self.draft_point_count();
        let can_append = point_count < MAX_POINTS;
        let Some(finger) = self.input.touches.get_mut(&key) else {
            return;
        };
        let inside = self
            .input
            .protected
            .is_some_and(|r| r.contains(point.x, point.y));
        if let Some(stroke) = &mut finger.stroke
            && stroke.tool.is_shape()
        {
            // A shape keeps its original anchor while the finger crosses the
            // toolbar. Rendering clips the part under the toolbar separately.
            if !inside {
                Self::append(stroke, point, can_append);
            }
            return;
        }
        let crosses = self
            .input
            .protected
            .is_some_and(|r| finger.last.is_some_and(|last| r.crosses(last, point)));
        if inside || crosses {
            if let Gesture::Erase(id) = finger.gesture {
                let budget = Erasure::available_budget(
                    self.input
                        .pending_erasures
                        .iter()
                        .filter(|(other, _)| **other != id)
                        .map(|(_, draft)| draft),
                );
                if let Some(erase) = self.input.pending_erasures.get_mut(&id) {
                    erase.break_path(&self.history, point_count, budget);
                    if let Some(bounds) = erase.take_damage() {
                        Bounds::include(&mut self.input.ink_damage, bounds);
                    }
                }
            }
            if let Some(stroke) = &mut finger.stroke
                && !stroke.points.is_empty()
            {
                let points = std::mem::take(&mut stroke.points);
                finger.fragments.push(Stroke {
                    points,
                    ..stroke.clone()
                });
            }
            finger.last = None;
        }
        if inside {
            return;
        }
        let starts_segment = finger.last.replace(point).is_none();
        match finger.gesture {
            Gesture::Erase(id) => self.erase_sample(id, point),
            Gesture::Laser(id) => self.laser_sample(id, point, starts_segment),
            Gesture::Ink => {
                if let Some(stroke) = &mut finger.stroke {
                    if stroke.points.is_empty() && can_append {
                        stroke.points.push(point);
                    } else {
                        Self::append(stroke, point, can_append);
                    }
                }
            }
        }
    }
    fn touch_event(&mut self, event: SDL_TouchFingerEvent, size: [f32; 2]) {
        let key = TouchKey::from(event);
        if event.r#type == SDL_EVENT_FINGER_CANCELED {
            if let Some(finger) = self.input.touches.remove(&key) {
                self.cancel_gesture(finger.gesture);
            }
            return;
        }
        if event.r#type == SDL_EVENT_FINGER_UP {
            if let Some(point) = touch::point(event, size) {
                self.touch_motion(key, point);
            }
            if let Some(finger) = self.input.touches.remove(&key) {
                self.commit_touch(finger);
            }
            return;
        }
        if !self.touch_enabled
            || self.pass
            || self.hidden
            || self.suspended
            || self.tool.is_text()
            || !self.input.pen_proximity.is_empty()
        {
            return;
        }
        let Some(point) = touch::point(event, size) else {
            return;
        };
        if event.r#type == SDL_EVENT_FINGER_DOWN {
            if self
                .input
                .protected
                .is_some_and(|r| r.contains(point.x, point.y))
            {
                return;
            }
            if self.input.touches.contains_key(&key)
                || self.input.touches.len() >= 16
                || touch::is_indirect(event.touchID)
                || !(0.0..=1.0).contains(&event.x)
                || !(0.0..=1.0).contains(&event.y)
            {
                return;
            }
            let gesture = self.gesture(self.tool);
            let stroke = if gesture == Gesture::Ink {
                if !self.can_start_ink() {
                    return;
                }
                let mut stroke = self.stroke(self.tool);
                // Capacitive contact pressure is often absent or contact area, not pen force.
                stroke.pressure_enabled = false;
                stroke.points.push(point);
                Some(stroke)
            } else {
                None
            };
            self.input.touches.insert(
                key,
                FingerStroke {
                    gesture,
                    stroke,
                    fragments: Vec::new(),
                    last: None,
                },
            );
        }
        self.touch_motion(key, point);
    }
    // Device proximity is independent of the current tool or modal picker.
    fn track_pen(&mut self, event: &SDL_Event) {
        let ty = event_type(event);
        let id = match ty {
            SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_PROXIMITY_OUT => {
                pen_proximity_event(event).map(|pen| pen.which.0)
            }
            SDL_EVENT_PEN_AXIS => pen_axis_event(event).map(|pen| pen.which.0),
            SDL_EVENT_PEN_MOTION => pen_motion_event(event).map(|pen| pen.which.0),
            SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => {
                pen_touch_event(event).map(|pen| pen.which.0)
            }
            _ => None,
        };
        let Some(id) = id else {
            return;
        };
        if ty == SDL_EVENT_PEN_PROXIMITY_OUT {
            self.end(Source::Pen(id));
            self.input.pens.remove(&id);
            self.input.pen_proximity.remove(&id);
            return;
        }
        if self.input.pen_proximity.insert(id) {
            self.discard_touches();
        }
    }
    fn event(&mut self, e: &SDL_Event, overlay: SDL_WindowID, size: [f32; 2]) {
        self.track_pen(e);
        unsafe {
            let ty = SDL_EventType(e.r#type);
            match ty {
                SDL_EVENT_MOUSE_BUTTON_DOWN if e.button.windowID == overlay => {
                    let b = e.button;
                    if b.which == SDL_PEN_MOUSEID || b.which == SDL_TOUCH_MOUSEID {
                        return;
                    }
                    if b.button == RIGHT_BUTTON
                        || (b.button == LEFT_BUTTON
                            && self
                                .input
                                .contact
                                .is_none_or(|c| c.source != Source::Mouse(RIGHT_BUTTON)))
                    {
                        self.begin(
                            Source::Mouse(b.button),
                            Point::new(b.x, b.y, 1., [0.; 2]),
                            b.button == RIGHT_BUTTON,
                        );
                    }
                }
                SDL_EVENT_MOUSE_BUTTON_UP
                    if e.button.windowID == overlay
                        && e.button.which != SDL_PEN_MOUSEID
                        && e.button.which != SDL_TOUCH_MOUSEID =>
                {
                    let b = e.button;
                    let source = Source::Mouse(b.button);
                    if self.input.contact.is_some_and(|contact| {
                        contact.source == source && matches!(contact.gesture, Gesture::Erase(_))
                    }) {
                        self.motion(source, Point::new(b.x, b.y, 1., [0.; 2]));
                    }
                    self.end(source);
                }
                SDL_EVENT_MOUSE_MOTION if e.motion.windowID == overlay => {
                    let m = e.motion;
                    if m.which != SDL_PEN_MOUSEID
                        && m.which != SDL_TOUCH_MOUSEID
                        && let Some(contact) = self.input.contact
                        && matches!(contact.source, Source::Mouse(_))
                    {
                        self.motion(contact.source, Point::new(m.x, m.y, 1., [0.; 2]));
                    }
                }
                SDL_EVENT_FINGER_CANCELED => self.touch_event(e.tfinger, size),
                SDL_EVENT_FINGER_DOWN | SDL_EVENT_FINGER_MOTION | SDL_EVENT_FINGER_UP
                    if e.tfinger.windowID == overlay =>
                {
                    self.touch_event(e.tfinger, size);
                }
                SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP if e.ptouch.windowID == overlay => {
                    let t = e.ptouch;
                    let pen = *self.input.pens.entry(t.which.0).or_default();
                    let source = Source::Pen(t.which.0);
                    let p = pen.point(t.x, t.y);
                    if t.down {
                        self.begin(source, p, t.eraser);
                        let accepted = self
                            .input
                            .contact
                            .is_some_and(|contact| contact.source == source);
                        self.input.pens.entry(t.which.0).or_default().down = accepted;
                        if let Some(contact) = &mut self.input.contact
                            && contact.source == source
                        {
                            contact.measured_pressure = pen.has_pressure;
                        }
                    } else {
                        self.pen_motion(source, p, pen.has_pressure);
                        self.end(source);
                        if let Some(pen) = self.input.pens.get_mut(&t.which.0) {
                            pen.finish();
                        }
                    }
                }
                SDL_EVENT_PEN_AXIS if e.paxis.windowID == overlay => {
                    let a = e.paxis;
                    let source = Source::Pen(a.which.0);
                    let pen = self.input.pens.entry(a.which.0).or_default();
                    let in_contact = pen.down || a.pen_state & SDL_PEN_INPUT_DOWN != 0;
                    match a.axis {
                        SDL_PEN_AXIS_PRESSURE if a.value.is_finite() => {
                            pen.pressure = a.value.clamp(0., 1.);
                            pen.has_pressure = in_contact || pen.pressure > 0.;
                        }
                        SDL_PEN_AXIS_XTILT => pen.tilt[0] = a.value.clamp(-90., 90.),
                        SDL_PEN_AXIS_YTILT => pen.tilt[1] = a.value.clamp(-90., 90.),
                        _ => {}
                    }
                    let point = pen.point(a.x, a.y);
                    let has_pressure = pen.has_pressure;
                    self.pen_motion(source, point, has_pressure);
                }
                SDL_EVENT_PEN_MOTION if e.pmotion.windowID == overlay => {
                    let m = e.pmotion;
                    let pen = *self.input.pens.entry(m.which.0).or_default();
                    self.pen_motion(
                        Source::Pen(m.which.0),
                        pen.point(m.x, m.y),
                        pen.has_pressure,
                    );
                }
                SDL_EVENT_WINDOW_FOCUS_LOST if e.window.windowID == overlay => {
                    if !self.multi {
                        self.discard_touches();
                    }
                    self.finish_contact();
                    self.reset_pens();
                }
                SDL_EVENT_WINDOW_MOUSE_LEAVE
                    if e.window.windowID == overlay
                        && self
                            .input
                            .contact
                            .is_some_and(|contact| matches!(contact.source, Source::Mouse(_))) =>
                {
                    self.finish_contact();
                }
                _ => {}
            }
        }
    }
    fn keyboard(&mut self, e: &SDL_Event, toolbar_keyboard: bool) -> Commands {
        let mut c = Commands::default();
        unsafe {
            if SDL_EventType(e.r#type) != SDL_EVENT_KEY_DOWN {
                return c;
            }
            let k = e.key;
            if k.repeat {
                return c;
            }
            let m = gui::modifiers(k.r#mod);
            if toolbar_keyboard || self.input.text.editing() {
                return c;
            }
            if self.tool.is_text()
                && !self.pass
                && !self.hidden
                && !m.command
                && self.text_key(k.key)
            {
                c.redraw_canvas = true;
                return c;
            }
            if k.key == SDLK_ESCAPE {
                if self.input.contact.is_some() || !self.input.touches.is_empty() {
                    self.reset_contacts();
                } else {
                    self.use_mouse();
                    c.mode = true
                }
                return c;
            }
            if m.command {
                match k.key {
                    SDLK_Z if m.shift => self.change_history(HistoryAction::Redo),
                    SDLK_Z => self.change_history(HistoryAction::Undo),
                    SDLK_Y => self.change_history(HistoryAction::Redo),
                    SDLK_S if m.shift => self.export(),
                    SDLK_S => self.save(),
                    SDLK_O => self.load(),
                    SDLK_Q => c.quit = true,
                    _ => {}
                }
                return c;
            }
            match k.key {
                SDLK_SPACE => {
                    self.toggle_mouse();
                    c.mode = true
                }
                SDLK_H => {
                    self.finish_all_contacts();
                    self.hidden = !self.hidden;
                    c.mode = true
                }
                SDLK_P | SDLK_B | SDLK_M | SDLK_L | SDLK_A | SDLK_R | SDLK_O | SDLK_E | SDLK_K
                | SDLK_T | SDLK_V => {
                    let tool = match k.key {
                        SDLK_B => Tool::Pencil,
                        SDLK_M => Tool::Marker,
                        SDLK_L => Tool::Line,
                        SDLK_A => Tool::Arrow,
                        SDLK_R => Tool::Rectangle,
                        SDLK_O => Tool::Ellipse,
                        SDLK_E => Tool::Eraser,
                        SDLK_K => Tool::Laser,
                        SDLK_T => Tool::Text,
                        SDLK_V => Tool::Select,
                        _ => Tool::Pen,
                    };
                    self.use_tool(tool);
                    c.mode = true;
                }
                SDLK_1 => c.redraw_canvas = self.select_preset(PRESETS[0]),
                SDLK_2 => c.redraw_canvas = self.select_preset(PRESETS[1]),
                SDLK_3 => c.redraw_canvas = self.select_preset(PRESETS[2]),
                SDLK_4 => c.redraw_canvas = self.select_preset(PRESETS[3]),
                _ => {}
            }
        }
        c
    }
    fn save(&mut self) {
        self.finish_all_contacts();
        let p = PathBuf::from(&self.path);
        let result = if p == self.save_path {
            Err(text(
                "Choose a different file for manual saves.",
                "请为手动保存选择其他文件名。",
                "請為手動儲存選擇其他檔名。",
            )
            .get(self.language)
            .into())
        } else {
            self.pages.save(&self.history, &p)
        };
        match result {
            Ok(()) => self.status = text("Saved", "已保存", "已儲存").detail(p.display()),
            Err(e) => self.status = text("Save failed", "保存失败", "儲存失敗").detail(e),
        }
    }
    fn save_session(&mut self) -> Result<(), String> {
        self.pages.save(&self.history, &self.save_path)?;
        self.pages.mark_saved(&mut self.history);
        Ok(())
    }
    fn remove_resume_file(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.save_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
    fn finish_session(&mut self) -> Result<(), String> {
        self.finish_all_contacts();
        if !self.preferences.restore_session {
            self.remove_resume_file()
                .map_err(|e| format!("Clearing the last session on exit: {e}"))
        } else if self.pages.needs_save(&self.history) {
            self.save_session()
                .map_err(|e| format!("Autosave on exit: {e}"))
        } else {
            Ok(())
        }
    }
    fn load(&mut self) {
        self.finish_all_contacts();
        let p = PathBuf::from(&self.path);
        match self.pages.load(&mut self.history, &p, false) {
            Ok(0) => self.status = text("Session loaded", "已打开会话", "已開啟工作階段").into(),
            Ok(pending) => self.status = pending_pages(pending),
            Err(e) => self.status = text("Open failed", "打开失败", "開啟失敗").detail(e),
        }
    }
    fn export(&mut self) {
        self.stop();
        let p = PathBuf::from(&self.path).with_extension("svg");
        let result = export::svg(self.history.document(), &self.text_fonts).and_then(|data| {
            p.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&p, data))
                .map_err(|error| error.to_string())
        });
        match result {
            Ok(()) => self.status = text("Exported", "已导出", "已匯出").detail(p.display()),
            Err(e) => self.status = text("Export failed", "导出失败", "匯出失敗").detail(e),
        }
    }

    fn install_annotation_font(&mut self, source: &Path, destination: &Path) -> bool {
        match self.text_fonts.install(source, destination) {
            Ok(()) => {
                self.font_path_input = None;
                self.status = text(
                    "Annotation font updated",
                    "已更新标注字体",
                    "已更新標註字型",
                )
                .into();
                true
            }
            Err(error) => {
                self.status = text(
                    "Could not use the annotation font",
                    "无法使用此标注字体",
                    "無法使用此標註字型",
                )
                .detail(error);
                false
            }
        }
    }
    fn select_preset(&mut self, rgb: [u8; 3]) -> bool {
        self.select_color(InkColor::from_rgb8(rgb).converted(self.color.space))
    }
    fn select_color(&mut self, color: InkColor) -> bool {
        self.hex_entry = crate::color::HexEntry::new(color);
        self.apply_color(color)
    }
    fn apply_color(&mut self, color: InkColor) -> bool {
        self.color = color;
        self.recolor_text(color)
    }
}
fn pending_pages(count: usize) -> Status {
    text(
        "Saved pages awaiting a matching display",
        "等待匹配显示器的已保存页面",
        "等待配對顯示器的已儲存頁面",
    )
    .detail(count)
}

fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("OVERINK_STATE_DIR") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "windows")]
    if let Some(p) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(p).join("OverInk");
    }
    #[cfg(target_os = "macos")]
    if let Some(p) = std::env::var_os("HOME") {
        return PathBuf::from(p).join("Library/Application Support/OverInk");
    }
    if let Some(p) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(p).join("overink");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/overink")
}
fn mouse_released() -> bool {
    crate::sdl::mouse_buttons_released()
}

fn wait_for_events(timeout: Duration) {
    if !timeout.is_zero() {
        // Wait without removing the event from SDL's queue.
        let milliseconds = timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128);
        crate::sdl::wait_for_event(milliseconds as i32);
    }
}

pub struct Options {
    pub board: bool,
    pub x11: bool,
    pub sdr: bool,
    pub display: usize,
    pub smoke: bool,
}
fn capture_visibility(
    canvases: &mut [canvas::Canvas],
    toolbar: &Window,
    tooltips: &mut crate::toolbar::Tooltips,
    toolbar_hidden: &mut bool,
    hide_all: bool,
    screenshot_active: bool,
) -> Result<bool, String> {
    if hide_all {
        for canvas in canvases.iter_mut().filter(|canvas| canvas.visible) {
            canvas.show(toolbar, false)?;
        }
    }
    let hide = hide_all || screenshot_active;
    if hide == *toolbar_hidden {
        return Ok(false);
    }
    tooltips.clear();
    toolbar.show(!hide)?;
    *toolbar_hidden = hide;
    for canvas in canvases {
        canvas.guard_dirty = true;
    }
    Ok(true)
}

fn handle_screenshot_event(
    app: &mut App,
    event: &SDL_Event,
    ty: SDL_EventType,
    canvases: &mut [canvas::Canvas],
) -> bool {
    let screenshot = &mut app.screenshot;
    if !screenshot.active() {
        return false;
    }
    if ty == SDL_EVENT_KEY_DOWN
        && key_event(event).is_some_and(|key| !key.repeat && key.key == SDLK_ESCAPE)
    {
        screenshot.cancel();
        for canvas in canvases {
            canvas.dirty = true;
            canvas.suspend_text_input();
        }
        return true;
    }
    if let Some(canvas) = canvases
        .iter_mut()
        .find(|canvas| screenshot.targets(canvas.window.id))
    {
        let on_canvas = canvas::event_window(event, ty) == Some(canvas.window.id);
        let contact_cancelled =
            matches!(ty, SDL_EVENT_FINGER_CANCELED | SDL_EVENT_PEN_PROXIMITY_OUT);
        if on_canvas || contact_cancelled {
            canvas.dirty |= screenshot.event(
                event,
                &canvas.window,
                app.touch_enabled && app.input.pen_proximity.is_empty(),
            );
        }
        if screenshot.ready()
            && (on_canvas || ty == SDL_EVENT_FINGER_CANCELED)
            && (ty != SDL_EVENT_FINGER_DOWN
                || (app.touch_enabled && app.input.pen_proximity.is_empty()))
        {
            canvas.dirty |= canvas.screenshot_event(event);
        }
    }
    false
}

fn update_preferences(app: &mut App, previous: Preferences, multi_supported: bool) -> bool {
    if let Err(error) = app.preferences.save(&state_dir().join("ui.json")) {
        app.preferences = previous;
        app.status = text("Settings save failed", "保存设置失败", "儲存設定失敗").detail(error);
        return false;
    }
    app.language = app.preferences.language.unwrap_or_else(Language::system);
    if previous.restore_session != app.preferences.restore_session {
        let result = if app.preferences.restore_session {
            app.save_session()
        } else {
            app.remove_resume_file()
        };
        if let Err(error) = result {
            app.status = text(
                "Last session update failed",
                "更新自动恢复会话失败",
                "更新自動復原工作階段失敗",
            )
            .detail(error);
        }
    }
    if previous.wayland != app.preferences.wayland {
        app.status = text(
            "Restart OverInk to switch the display backend.",
            "重启 OverInk 后切换显示后端。",
            "重新啟動 OverInk 後切換顯示後端。",
        )
        .into();
    }
    let multi = multi_supported && app.preferences.per_display_windows;
    if multi != app.multi {
        app.finish_all_contacts();
        app.parked_input.clear();
        app.multi = multi;
        true
    } else {
        false
    }
}

#[cfg(target_os = "linux")]
impl App {
    fn update_gnome_guide(
        &mut self,
        commands: &mut Commands,
        previous: &Preferences,
        smoke: bool,
    ) -> bool {
        use crate::gnome::{Decision, Guide};

        if let Some(action) = commands.gnome_action.take() {
            commands.preferences = true;
            let decision = self.gnome_guide.as_mut().and_then(|guide| {
                guide
                    .act(action)
                    .or_else(|| matches!(guide, Guide::Relogin).then_some(Decision::Relogin))
            });
            if let Some(decision) = decision {
                self.gnome_guide = None;
                self.preferences.wayland = decision != Decision::XWayland;
            }
            return decision == Some(Decision::Relogin);
        }
        if commands.preferences
            && self.preferences.wayland
            && !previous.wayland
            && !smoke
            && crate::gnome::in_wayland_session()
            && let Some(guide) = Guide::check()
        {
            if matches!(guide, Guide::Relogin) {
                return true;
            }
            self.preferences.wayland = false;
            self.gnome_guide = Some(guide);
        }
        false
    }
}

pub fn run(options: Options) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let _teardown_pool = crate::native::macos::AutoreleasePool::new();
    #[cfg(target_os = "macos")]
    let startup_pool = crate::native::macos::AutoreleasePool::new();
    let base = state_dir();
    let preferences_path = base.join("ui.json");
    let preferences = Preferences::load(&preferences_path);
    #[cfg(target_os = "linux")]
    let (_runtime, preferences, gnome_status) =
        crate::gnome::start_video(preferences, &preferences_path, options.x11, options.smoke)?;
    #[cfg(not(target_os = "linux"))]
    let _runtime = Runtime::new(options.x11, preferences.wayland, options.smoke)?;
    let video = crate::sdl::driver();
    let wayland = video == "wayland";
    let window = Window::new("OverInk - canvas", 1100, 720, !options.smoke)?;
    let topology = crate::display::Topology::new(&video);
    let displays = topology.connected(&window).unwrap_or_default();
    let xwayland = crate::background::is_xwayland(&window, &video);
    let mut app = App::new(
        options.board || options.smoke,
        options.display,
        displays,
        &base,
        preferences,
    );
    #[cfg(target_os = "linux")]
    if let Some(status) = gnome_status {
        app.status = status;
    }
    app.multi = app.preferences.per_display_windows && !wayland && !options.smoke;
    let mut canvases = vec![canvas::Canvas::new(
        window,
        app.displays.get(app.display),
        &options,
    )?];
    let toolbar = Window::new(
        "OverInk",
        crate::toolbar::WIDTH as i32,
        crate::toolbar::HEIGHT as i32,
        false,
    )?;
    toolbar.setup_toolbar();
    if !options.smoke {
        let _ = toolbar.arrange_toolbar(false, app.pages.active);
        canvas::reconcile(&mut canvases, &mut app, &toolbar, &options)?;
    }
    let toolbar_render = Renderer::new(&toolbar, true)
        .map_err(|error| format!("Creating the toolbar renderer on {video} failed ({error})"))?;
    let mut gui = gui::Gui::new();
    let mut mark = crate::toolbar::brand(&gui.ctx);
    let mut toolbar_color = ColorManager::default();
    toolbar_color.refresh(&toolbar, toolbar_render.extended);
    for canvas in &mut canvases {
        canvas.show(&toolbar, options.smoke || !app.suspended)?;
    }
    toolbar
        .show(true)
        .map_err(|error| format!("Showing the toolbar on {video} failed ({error})"))?;
    if !options.smoke && !wayland {
        let _ = toolbar.arrange_toolbar(false, app.pages.active);
    }
    toolbar.raise();
    if options.smoke {
        app.history = History::default();
        smoke_text_history(&mut app, &mut canvases[0])?;
        smoke_events(&mut app, canvases[0].window.id)?;
    }
    let mut picker = crate::picker::Picker::default();
    let mut file_dialog = FileDialog::default();
    let annotation_font_path = base.join("annotation-font");
    let mut tooltips = crate::toolbar::Tooltips::default();
    let mut toolbar_hidden = false;
    let mut toolbar_expanded = false;
    let mut running = true;
    let mut frame = 0;
    let mut last_color = Instant::now();
    let mut last_displays = Instant::now();
    let mut controller_display = None;
    let mut follow_controller = false;
    let mut requested_display = None;
    let mut topology_dirty = !options.smoke;
    let mut pacer = FramePacer::new(app.frame_rate, app.refresh_hz(), Instant::now());
    let mut appearance = ThemeMonitor::default();
    let mut toolbar_dirty = true;
    let mut toolbar_repaint_at: Option<Instant> = None;
    let mut toolbar_text_input = false;
    let mut restore_toolbar_focus = false;
    let mut text_focus = text::ToolbarTextFocus::default();
    let mut text_pick = None;
    #[cfg(target_os = "macos")]
    drop(startup_pool);
    while running {
        #[cfg(target_os = "macos")]
        let _frame_pool = crate::native::macos::AutoreleasePool::new();
        let mut disconnected = BTreeSet::new();
        let mut controller_scale_changed = false;
        let mut controller_moved = false;
        let mut controller_bounds_changed = false;
        let mut mode_changed = false;
        // Keep the rest of this event batch modal even if Escape ends the screenshot.
        let screenshot_owns_input = app.screenshot.active();
        let mut event = SDL_Event::default();
        while poll_event(&mut event) {
            let ty = event_type(&event);
            if ty == SDL_EVENT_QUIT {
                running = false;
                break;
            }
            if ty == SDL_EVENT_LOCALE_CHANGED && app.preferences.language.is_none() {
                app.language = Language::system();
                tooltips.clear();
                toolbar_dirty = true;
            }
            if (SDL_EVENT_DISPLAY_FIRST..=SDL_EVENT_DISPLAY_LAST).contains(&ty) {
                topology_dirty = !options.smoke;
                if ty == SDL_EVENT_DISPLAY_REMOVED {
                    if let Some(display) = removed_display_id(&event) {
                        disconnected.insert(display);
                    }
                }
            }
            if (SDL_EVENT_WINDOW_FIRST..=SDL_EVENT_WINDOW_LAST).contains(&ty) {
                let id = window_event_id(&event).unwrap_or(SDL_WindowID(0));
                let bounds_changed = matches!(
                    ty,
                    SDL_EVENT_WINDOW_MOVED
                        | SDL_EVENT_WINDOW_DISPLAY_CHANGED
                        | SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED
                        | SDL_EVENT_WINDOW_RESIZED
                        | SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED
                        | SDL_EVENT_WINDOW_SHOWN
                        | SDL_EVENT_WINDOW_HIDDEN
                );
                let surface_changed = matches!(
                    ty,
                    SDL_EVENT_WINDOW_DISPLAY_CHANGED
                        | SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED
                        | SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED
                        | SDL_EVENT_WINDOW_HDR_STATE_CHANGED
                        | SDL_EVENT_WINDOW_ICCPROF_CHANGED
                );
                let redraw = bounds_changed
                    || surface_changed
                    || matches!(ty, SDL_EVENT_WINDOW_EXPOSED | SDL_EVENT_WINDOW_RESTORED);
                if id == toolbar.id {
                    if ty == SDL_EVENT_WINDOW_CLOSE_REQUESTED {
                        running = false;
                        break;
                    }
                    if bounds_changed {
                        tooltips.clear();
                        controller_bounds_changed = true;
                    }
                    let moved = matches!(
                        ty,
                        SDL_EVENT_WINDOW_MOVED | SDL_EVENT_WINDOW_DISPLAY_CHANGED
                    );
                    controller_moved |= moved;
                    controller_scale_changed |= ty == SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED;
                    topology_dirty |= !options.smoke && (moved || controller_scale_changed);
                    toolbar_dirty |= redraw;
                    if matches!(
                        ty,
                        SDL_EVENT_WINDOW_FOCUS_GAINED | SDL_EVENT_WINDOW_FOCUS_LOST
                    ) && app.input.text.editing()
                    {
                        app.input.text.saw_input_focus();
                        let index = canvas::active_index(&canvases, app.pages.active);
                        canvases[index].dirty = true;
                    }
                } else if let Some(canvas) =
                    canvases.iter_mut().find(|canvas| canvas.window.id == id)
                {
                    topology_dirty |= ty == SDL_EVENT_WINDOW_DISPLAY_CHANGED && !options.smoke;
                    canvas.guard_dirty |= bounds_changed;
                    canvas.dirty |= redraw;
                    if surface_changed {
                        canvas.surface_changed();
                        toolbar_dirty = true;
                    }
                    if matches!(ty, SDL_EVENT_WINDOW_SHOWN | SDL_EVENT_WINDOW_FOCUS_GAINED) {
                        canvas.keep_toolbar_above(&toolbar);
                    }
                    if ty == SDL_EVENT_WINDOW_CLOSE_REQUESTED {
                        if app.screenshot.active() {
                            app.screenshot.abandon();
                            restore_toolbar_focus = true;
                        }
                        app.finish_all_contacts();
                        app.hidden = true;
                        mode_changed = true;
                        toolbar_dirty = true;
                    }
                } else if matches!(
                    ty,
                    SDL_EVENT_WINDOW_EXPOSED | SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED
                ) {
                    tooltips.expose(id, &toolbar_color);
                }
            }
            if matches!(
                ty,
                SDL_EVENT_RENDER_TARGETS_RESET | SDL_EVENT_RENDER_DEVICE_RESET
            ) {
                let id = render_reset_window_id(&event).unwrap_or(SDL_WindowID(0));
                for canvas in canvases
                    .iter_mut()
                    .filter(|canvas| id.0 == 0 || canvas.window.id == id)
                {
                    canvas.renderer_reset();
                }
                if id.0 == 0 || id == toolbar.id {
                    // Canvas and toolbar own different SDL renderers. The
                    // toolbar egui atlas/brand/tooltip textures must be rebuilt.
                    gui.forget_renderer_textures();
                    gui = gui::Gui::new();
                    mark = crate::toolbar::brand(&gui.ctx);
                    tooltips.renderer_reset();
                    toolbar_text_input = false;
                    toolbar_repaint_at = None;
                    toolbar_dirty = true;
                } else {
                    tooltips.renderer_reset_window(id);
                }
                app.for_each_page(|page| page.input.text.refocus());
            }

            if screenshot_owns_input {
                // Keep pen proximity and palm rejection current even while
                // screenshot selection owns the event batch.
                if matches!(
                    ty,
                    SDL_EVENT_PEN_PROXIMITY_IN
                        | SDL_EVENT_PEN_PROXIMITY_OUT
                        | SDL_EVENT_PEN_DOWN
                        | SDL_EVENT_PEN_UP
                        | SDL_EVENT_PEN_MOTION
                        | SDL_EVENT_PEN_AXIS
                ) {
                    app.for_each_page(|page| page.track_pen(&event));
                    if !app.input.pen_proximity.is_empty()
                        && app.screenshot.reject_touch_drag()
                        && let Some(canvas) = canvases
                            .iter_mut()
                            .find(|canvas| app.screenshot.targets(canvas.window.id))
                    {
                        canvas.dirty = true;
                    }
                }
                // SDL owns the native dialog's input and delivers its result through
                // the callback. Keep OverInk's drawing and shortcuts modal.
                if !file_dialog.pending() {
                    let cancelled = handle_screenshot_event(&mut app, &event, ty, &mut canvases);
                    mode_changed |= cancelled;
                    restore_toolbar_focus |= cancelled;
                }
                continue;
            }

            let id = canvas::event_window(&event, ty);
            let active = canvas::active_index(&canvases, app.pages.active);
            let was_picking = picker.active();
            picker.event(&event, &canvases[active].window);
            let target = if ty == SDL_EVENT_FINGER_CANCELED {
                let key = touch::key_from_event(&event)
                    .ok_or("SDL finger cancellation event has no finger data")?;
                app.touch_page(key)
                    .and_then(|output| canvases.iter().position(|c| c.output == Some(output)))
                    .or_else(|| {
                        canvases
                            .iter()
                            .position(|canvas| canvas.has_text_touch(key))
                    })
            } else {
                id.and_then(|id| canvases.iter().position(|c| c.window.id == id))
            };
            let drawing = !app.picking
                && !was_picking
                && !toolbar_hidden
                && !app.suspended
                && !file_dialog.pending();
            if !file_dialog.pending() {
                gui.event(&event, toolbar.id, toolbar.size());
            }
            if id == Some(toolbar.id)
                && ty == SDL_EVENT_WINDOW_FOCUS_LOST
                && !toolbar.input_focused()
            {
                gui.sync_text_input(&toolbar, false);
            }
            if ty == SDL_EVENT_PEN_PROXIMITY_OUT {
                // A pen can leave proximity over a different window from the
                // one that received its earlier packets.
                app.for_each_page(|page| page.track_pen(&event));
                for canvas in &mut canvases {
                    canvas.dirty = true;
                }
                toolbar_dirty = true;
            } else if let Some(index) =
                target.or((ty == SDL_EVENT_FINGER_CANCELED && !app.multi).then_some(active))
            {
                let canvas = &canvases[index];
                let activates = drawing
                    && canvas.visible
                    && canvas.on_output(options.smoke)
                    && ((ty == SDL_EVENT_MOUSE_BUTTON_DOWN
                        && mouse_button_event(&event).is_some_and(|button| {
                            button.which != SDL_TOUCH_MOUSEID
                                && button.which != SDL_PEN_MOUSEID
                        }))
                        || matches!(
                            ty,
                            SDL_EVENT_PEN_DOWN | SDL_EVENT_FINGER_DOWN | SDL_EVENT_KEY_DOWN
                        ));
                if activates
                    && app.multi
                    && let Some(output) = canvas.output
                    && app.pages.active != Some(output)
                {
                    canvases[active].flush_text_events(&mut app, gui.ctx.theme())?;
                    app.select_page(output);
                    canvases[active].suspend_text_input();
                    canvases[active].dirty = true;
                    toolbar_dirty = true;
                }
                let canvas = &mut canvases[index];
                if activates {
                    canvas.keep_toolbar_above(&toolbar);
                }
                let before = app.history_buttons();
                let selection_before = app.input.text.selected;
                let panel_before = app.panel;
                let edited = app.on_page(canvas.output, |app| {
                    let had_contacts = app.input.has_contacts();
                    if drawing && canvas.visible && canvas.on_output(options.smoke) {
                        canvas.set_page_geometry(app);
                        app.event(&event, canvas.window.id, canvas.window.size());
                        if app.tool.is_text() && !app.pass {
                            let was_editing = app.input.text.editing();
                            let edited = canvas.text_event(app, &event);
                            if !was_editing && app.input.text.editing() {
                                canvas.keep_toolbar_above(&toolbar);
                            }
                            return edited || had_contacts || app.input.has_contacts();
                        }
                    } else {
                        app.track_pen(&event);
                    }
                    had_contacts || app.input.has_contacts()
                });
                canvas.dirty |= edited;
                toolbar_dirty |= before != app.history_buttons();
                toolbar_dirty |= selection_before != app.input.text.selected;
                toolbar_dirty |= panel_before != app.panel;
                toolbar_dirty |= app.panel.is_some() && (edited || activates);
            } else if matches!(
                ty,
                SDL_EVENT_PEN_PROXIMITY_IN
                    | SDL_EVENT_PEN_DOWN
                    | SDL_EVENT_PEN_UP
                    | SDL_EVENT_PEN_AXIS
                    | SDL_EVENT_PEN_MOTION
            ) {
                // Pen events on the controller still drive palm rejection.
                let index = if id == Some(toolbar.id) && app.multi {
                    canvas::active_index(&canvases, toolbar.display_id())
                } else {
                    active
                };
                let canvas = &mut canvases[index];
                let edited = app.on_page(canvas.output, |page| {
                    let had_touches = !page.input.touches.is_empty();
                    page.track_pen(&event);
                    let canceled_text = canvas.cancel_text_touches(page);
                    canceled_text || (had_touches && page.input.touches.is_empty())
                });
                canvas.dirty |= edited;
            }
            toolbar_dirty |= app.panel == Some(crate::toolbar::Panel::Brush)
                && matches!(ty, SDL_EVENT_PEN_AXIS | SDL_EVENT_PEN_PROXIMITY_OUT);
            if ty == SDL_EVENT_KEY_DOWN
                && drawing
                && (id == Some(toolbar.id)
                    || target.is_some_and(|index| {
                        canvases[index].visible && canvases[index].on_output(options.smoke)
                    }))
            {
                toolbar_dirty = true;
                for canvas in &mut canvases {
                    canvas.dirty = true;
                }
                let was_editing = app.input.text.editing();
                let from_toolbar = id == Some(toolbar.id);
                let toolbar_claims_keys = from_toolbar
                    && (toolbar_text_input
                        || egui::Popup::is_any_open(&gui.ctx)
                        || gui
                            .raw
                            .events
                            .iter()
                            .any(|event| matches!(event, egui::Event::PointerButton { .. })));
                let key =
                    key_event(&event).ok_or("SDL keyboard event has no keyboard data")?;
                if from_toolbar
                    && was_editing
                    && !toolbar_claims_keys
                    && !key.repeat
                    && (key.key == SDLK_ESCAPE
                        || (matches!(key.key, SDLK_RETURN | SDLK_KP_ENTER)
                            && gui::modifiers(key.r#mod).command))
                {
                    canvases[active].finish_text_editor(&mut app, gui.ctx.theme())?;
                    continue;
                }
                let command = app.keyboard(
                    &event,
                    toolbar_claims_keys || from_toolbar && gui.ctx.egui_wants_keyboard_input(),
                );
                if !was_editing && app.input.text.editing() {
                    let index = canvas::active_index(&canvases, app.pages.active);
                    let canvas = &mut canvases[index];
                    canvas.activate_text_editor();
                    canvas.keep_toolbar_above(&toolbar);
                }
                mode_changed |= command.mode;
                if command.quit {
                    running = false;
                    break;
                }
            }
        }
        if app.screenshot.active()
            && app.screenshot.recover_mouse_drag(mouse_released())
            && let Some(canvas) = canvases
                .iter_mut()
                .find(|canvas| app.screenshot.targets(canvas.window.id))
        {
            canvas.dirty = true;
        }
        if !running {
            let active = canvas::active_index(&canvases, app.pages.active);
            canvases[active].flush_text_events(&mut app, gui.ctx.theme())?;
            break;
        }

        if let Some((kind, result)) = file_dialog.poll() {
            toolbar_dirty = true;
            restore_toolbar_focus = !app.screenshot.active();
            match kind {
                DialogKind::Font => match result {
                    Ok(Some(path)) if app.install_annotation_font(&path, &annotation_font_path) => {
                        for canvas in &mut canvases {
                            canvas.dirty = true;
                        }
                    }
                    Ok(_) => (),
                    Err(error) => {
                        app.panel = Some(crate::toolbar::Panel::Settings);
                        app.font_path_input.get_or_insert_with(String::new);
                        gui.ctx
                            .memory_mut(|mem| mem.request_focus(egui::Id::unique(FONT_PATH_FIELD)));
                        app.status = text(
                            "Enter a font file path",
                            "请输入字体文件路径",
                            "請輸入字型檔案路徑",
                        )
                        .detail(error);
                    }
                },
                DialogKind::ScreenshotFolder => match result {
                    Ok(Some(path)) => {
                        let previous = app.preferences.clone();
                        app.preferences.screenshot_folder = Some(path);
                        update_preferences(&mut app, previous, !wayland && !options.smoke);
                    }
                    Ok(None) => (),
                    Err(error) => {
                        app.status = text(
                            "Could not choose a screenshot folder",
                            "无法选择截图文件夹",
                            "無法選擇螢幕擷取資料夾",
                        )
                        .detail(error);
                    }
                },
                DialogKind::ScreenshotFile => {
                    app.screenshot.close_save_as();
                    if let Some(canvas) = canvases
                        .iter_mut()
                        .find(|canvas| app.screenshot.targets(canvas.window.id))
                    {
                        canvas.dirty = true;
                        canvas.suspend_text_input();
                        match result {
                            Ok(Some(path)) => {
                                if let Err(error) = app.screenshot.save(Destination::File(path)) {
                                    app.screenshot.show_error(error);
                                }
                            }
                            Ok(None) => (),
                            Err(error) => app.screenshot.show_error(error),
                        }
                        if canvas.visible {
                            canvas.window.raise();
                        }
                    }
                }
            }
        }

        if !options.smoke
            && (topology_dirty
                || video == "x11" && last_displays.elapsed() > Duration::from_millis(500))
        {
            let moved_to = if follow_controller && controller_moved {
                let reported = toolbar.display_id();
                (std::mem::replace(&mut controller_display, reported) != reported)
                    .then_some(reported)
                    .flatten()
            } else {
                None
            };
            let requested = requested_display.take();
            let displays = topology
                .connected(&canvases[0].window)
                .unwrap_or_else(|error| {
                    app.status = text("Display query failed", "查询显示器失败", "查詢顯示器失敗")
                        .detail(error);
                    toolbar_dirty = true;
                    app.displays.clone()
                })
                .into_iter()
                .filter(|d| !disconnected.contains(&d.id))
                .collect::<Vec<_>>();
            let was_suspended = app.suspended;
            let reported = canvases[0]
                .window
                .display_id()
                .filter(|id| displays.iter().any(|display| display.id == *id));
            let desired = requested.or(moved_to);
            let target = if wayland {
                reported.or(desired)
            } else {
                desired
            };
            let topology_changed = displays != app.displays;
            let previous_page = app.pages.active;
            if target.is_some_and(|id| Some(id) != previous_page)
                && displays
                    .iter()
                    .any(|display| Some(display.id) == previous_page)
            {
                let active = canvas::active_index(&canvases, previous_page);
                canvases[active].flush_text_events(&mut app, gui.ctx.theme())?;
            }
            let (layout, removed) =
                if topology_changed || target.is_some_and(|id| Some(id) != app.pages.active) {
                    toolbar_dirty = true;
                    app.refresh_displays(displays, target)
                } else {
                    (false, false)
                };
            if app.multi
                && previous_page != app.pages.active
                && let Some(canvas) = canvases
                    .iter_mut()
                    .find(|canvas| canvas.output == previous_page)
            {
                canvas.suspend_text_input();
                canvas.dirty = true;
            }
            if requested.is_some()
                && !wayland
                && let Err(error) = toolbar.arrange_toolbar(toolbar_expanded, app.pages.active)
            {
                app.status =
                    text("Toolbar layout failed", "工具栏布局失败", "工具列配置失敗").detail(error);
            }
            if (was_suspended || removed)
                && let Some(id) = app.pages.active
            {
                let _ = toolbar.recover_on_display(id);
            }
            if topology_changed || layout || topology_dirty {
                if (layout || removed) && picker.active() {
                    picker.cancel();
                }
                if (layout || removed || topology_changed) && app.screenshot.active() {
                    app.screenshot.abandon();
                    restore_toolbar_focus = true;
                    mode_changed = true;
                }
                if let Err(error) = canvas::reconcile(&mut canvases, &mut app, &toolbar, &options) {
                    app.status = canvas::failure(error);
                    toolbar_dirty = true;
                }
            }
            if wayland && requested.is_some_and(|id| Some(id) != reported) {
                app.status = text(
                    "The Wayland compositor controls window order and placement.",
                    "Wayland 窗口层级和位置由合成器控制。",
                    "Wayland 視窗層級與位置由合成器控制。",
                )
                .into();
                toolbar_dirty = true;
            }
            if removed
                && app.preferences.restore_session
                && app.pages.needs_save(&app.history)
                && let Err(error) = app.save_session()
            {
                app.status = text(
                    "Save after display removal failed",
                    "断开显示器后保存失败",
                    "中斷顯示器連線後儲存失敗",
                )
                .detail(error);
                toolbar_dirty = true;
            }
            topology_dirty = false;
            last_displays = Instant::now();
        }

        let active = canvas::active_index(&canvases, app.pages.active);
        let connected = app.displays.iter().map(|d| d.id).collect();
        pacer.configure(app.frame_rate, app.refresh_hz(), Instant::now());
        for canvas in &mut canvases {
            let mode = canvas.snapshot_mode(&video, xwayland, options.sdr);
            if let Err(reason) = canvas.update_background(
                &app.displays,
                &connected,
                mode,
                !app.pass && !app.hidden && !picker.active() && !app.screenshot.active(),
            ) {
                app.status = reason.into();
                canvas.dirty = true;
                toolbar_dirty = true;
            }
        }
        if let Some(index) = canvases
            .iter()
            .position(|canvas| app.screenshot.targets(canvas.window.id))
        {
            let canvas = &mut canvases[index];
            if let Some(result) =
                app.screenshot
                    .poll_capture(&canvas.window, &app.displays, wayland || xwayland)
            {
                canvas.dirty = true;
                match result {
                    Ok(true) => canvas.window.raise(),
                    Ok(false) => {
                        toolbar_dirty = true;
                        restore_toolbar_focus = true;
                        mode_changed = true;
                    }
                    Err(error) => {
                        app.panel = Some(crate::toolbar::Panel::Settings);
                        app.status =
                            text("Screenshot failed", "截图失败", "螢幕擷取失敗").detail(error);
                        toolbar_dirty = true;
                        restore_toolbar_focus = true;
                        mode_changed = true;
                    }
                }
            }
        }
        let mut open_save_as = false;
        if let Some((direct, result)) = app.screenshot.poll_save() {
            if let Some(canvas) = canvases
                .iter_mut()
                .find(|canvas| app.screenshot.targets(canvas.window.id))
            {
                canvas.dirty = true;
                if result.is_ok() {
                    canvas.suspend_text_input();
                }
            }
            match result {
                Ok(path) => {
                    app.screenshot.cancel();
                    app.status = text("Screenshot saved", "截图已保存", "螢幕擷取已儲存")
                        .detail(path.display());
                    toolbar_dirty = true;
                    restore_toolbar_focus = true;
                    mode_changed = true;
                }
                Err(SaveError::Exists(path)) if app.screenshot.ready() => {
                    app.screenshot.confirm_replace(path);
                }
                Err(SaveError::Exists(path)) => {
                    app.status = text(
                        "Screenshot not saved; file already exists",
                        "截图未保存：文件已存在",
                        "螢幕擷取未儲存：檔案已存在",
                    )
                    .detail(path.display());
                    toolbar_dirty = true;
                }
                Err(SaveError::Cancelled) => {
                    app.screenshot.cancel();
                    toolbar_dirty = true;
                    restore_toolbar_focus = true;
                    mode_changed = true;
                }
                Err(SaveError::Failed(error)) if app.screenshot.ready() => {
                    app.screenshot.show_error(error);
                    open_save_as = direct;
                }
                Err(SaveError::Failed(error)) => {
                    app.status = text("Screenshot save failed", "保存截图失败", "儲存螢幕擷取失敗")
                        .detail(error);
                    toolbar_dirty = true;
                }
            }
        }
        let hide = picker.hides_windows() || canvases.iter().any(|c| c.background.pending());
        toolbar_dirty |= capture_visibility(
            &mut canvases,
            &toolbar,
            &mut tooltips,
            &mut toolbar_hidden,
            hide,
            app.screenshot.active(),
        )?;
        let mut restore_text_focus = false;
        if let Some(result) = picker.poll(&canvases[active].window) {
            app.picking = false;
            let origin = text_pick.take();
            let canvas = &canvases[active];
            let on_text = origin.as_ref().is_some_and(|(window, output, target)| {
                *window == canvas.window.id
                    && *output == canvas.output
                    && app.text_pick_target().as_ref() == Some(target)
            });
            if on_text {
                app.input.text.refocus();
                restore_text_focus = app.input.text.editing();
            }
            let result = if origin.is_some() && !on_text {
                Ok(None)
            } else {
                result
            };
            toolbar_dirty = true;
            for canvas in &mut canvases {
                canvas.dirty = true;
                canvas.guard_dirty = true;
            }
            app.status = match result {
                Ok(Some(color)) => {
                    let applied = if on_text {
                        app.pick_text_color(color)
                    } else {
                        let color = color.converted(app.color.space);
                        app.select_color(color);
                        Some(color)
                    };
                    if let Some(color) = applied {
                        text("Picked", "已取色", "已取色").detail(color.hex())
                    } else {
                        text("Color selection cancelled", "已取消取色", "已取消取色").into()
                    }
                }
                Ok(None) => text("Color selection cancelled", "已取消取色", "已取消取色").into(),
                Err(error) => {
                    text("Screen color picker failed", "屏幕取色失败", "螢幕取色失敗").detail(error)
                }
            };
        }
        for canvas in &mut canvases {
            if let Some(result) = canvas.background.poll(&canvas.window) {
                canvas.dirty = true;
                toolbar_dirty = true;
                app.status = match result {
                    Ok(()) => text(
                        "Desktop frozen. Select Live desktop to resume the background.",
                        "桌面已冻结，选择实时桌面即可恢复。",
                        "桌面已凍結，選擇即時桌面即可恢復。",
                    )
                    .into(),
                    Err(error) => text(
                        "Showing the live desktop",
                        "已切回实时桌面",
                        "已切回即時桌面",
                    )
                    .detail(error),
                };
            }
        }
        let hide = picker.hides_windows() || canvases.iter().any(|c| c.background.pending());
        toolbar_dirty |= capture_visibility(
            &mut canvases,
            &toolbar,
            &mut tooltips,
            &mut toolbar_hidden,
            hide,
            app.screenshot.active(),
        )?;
        if hide {
            wait_for_events(Duration::from_millis(8));
            continue;
        }

        if last_color.elapsed() > Duration::from_secs(3) {
            let previous_toolbar = toolbar_color.revision;
            toolbar_color.refresh(&toolbar, toolbar_render.extended);
            toolbar_dirty |= previous_toolbar != toolbar_color.revision;
            for canvas in &mut canvases {
                toolbar_dirty |= canvas.refresh_color();
            }
            last_color = Instant::now();
        }
        let expanded = app.panel.is_some() && !app.picking;
        if expanded != toolbar_expanded || controller_scale_changed {
            toolbar_dirty = true;
            controller_bounds_changed = true;
            toolbar_expanded = expanded;
            if let Err(error) = toolbar.arrange_toolbar(expanded, None) {
                app.status =
                    text("Toolbar layout failed", "工具栏布局失败", "工具列配置失敗").detail(error);
            }
        }
        let theme = appearance.theme(app.preferences.theme);
        if gui.ctx.theme() != theme {
            gui.ctx.set_theme(theme);
            toolbar_dirty = true;
            for canvas in &mut canvases {
                canvas.dirty = true;
            }
        }
        let previous_preferences = app.preferences.clone();
        let was_board = app.board;
        app.frozen = canvases[active].background.frozen;
        let mut cmd = Commands::default();
        let mut output = None;
        if !toolbar_hidden
            && (toolbar_dirty
                || !gui.raw.events.is_empty()
                || toolbar_repaint_at.is_some_and(|at| Instant::now() >= at))
        {
            toolbar_dirty = false;
            if app.input.text.editing()
                && (toolbar.input_focused()
                    || gui
                        .raw
                        .events
                        .iter()
                        .any(|event| matches!(event, egui::Event::PointerButton { .. })))
            {
                canvases[active].flush_text_events(&mut app, theme)?;
            }
            let mut input = gui.input(&toolbar);
            input.predicted_dt = pacer.period.as_secs_f32();
            let canvas = &canvases[active];
            let frame = gui.ctx.run_ui(input, |ui| {
                cmd = app.ui(
                    ui,
                    canvas,
                    &video,
                    canvas
                        .snapshot_mode(&video, xwayland, options.sdr)
                        .unavailable(),
                    mark.id(),
                    file_dialog.pending(),
                )
            });
            toolbar_text_input = frame.platform_output.ime.is_some();
            toolbar_repaint_at = gui::repaint_at(&frame);
            output = Some(frame);
        }
        if cmd.quit {
            break;
        }
        mode_changed |= cmd.mode;
        if cmd.choose_font && !file_dialog.pending() {
            app.finish_all_contacts();
            tooltips.clear();
            for canvas in &mut canvases {
                canvas.show(&toolbar, false)?;
            }
            gui.suspend_input(&toolbar);
            file_dialog.open(DialogKind::Font, &toolbar, None);
            cmd.redraw_canvas = true;
            toolbar_dirty = true;
        }
        if cmd.screenshot_folder && !file_dialog.pending() {
            app.finish_all_contacts();
            tooltips.clear();
            for canvas in &mut canvases {
                canvas.show(&toolbar, false)?;
            }
            gui.suspend_input(&toolbar);
            let location = screenshot::default_folder(&app.preferences);
            file_dialog.open(DialogKind::ScreenshotFolder, &toolbar, location.as_deref());
            cmd.redraw_canvas = true;
            toolbar_dirty = true;
        }
        if cmd.screenshot && !file_dialog.pending() && !picker.active() && !app.screenshot.saving()
        {
            let canvas = &mut canvases[active];
            canvas.finish_text_editor(&mut app, theme)?;
            app.finish_all_contacts();
            gui.suspend_input(&toolbar);
            gui.clear_events();
            app.screenshot
                .start(&canvas.window, app.preferences.screenshot_hdr);
            mode_changed = true;
            cmd.redraw_canvas = true;
        }
        if let Some(source) = cmd.font_path.take() {
            app.finish_all_contacts();
            cmd.redraw_canvas |= app.install_annotation_font(&source, &annotation_font_path);
            toolbar_dirty = true;
        }
        if cmd.reset_font {
            match app.text_fonts.reset(&annotation_font_path) {
                Ok(()) => {
                    app.status = text(
                        "Using the included annotation font",
                        "已切换到内置标注字体",
                        "已切換至內建標註字型",
                    )
                    .into();
                    cmd.redraw_canvas = true;
                }
                Err(error) => {
                    app.status = text(
                        "Could not reset the annotation font",
                        "无法还原标注字体",
                        "無法還原標註字型",
                    )
                    .detail(error)
                }
            }
            toolbar_dirty = true;
        }
        #[cfg(target_os = "linux")]
        let relogin = app.update_gnome_guide(&mut cmd, &previous_preferences, options.smoke);
        if cmd.preferences {
            toolbar_dirty = true;
            if app.preferences != previous_preferences {
                topology_dirty |=
                    update_preferences(&mut app, previous_preferences, !wayland && !options.smoke);
            }
            #[cfg(target_os = "linux")]
            if relogin && app.preferences.wayland {
                app.status = crate::gnome::relogin_status();
            }
        }
        if cmd.background {
            if picker.active() {
                picker.cancel();
            }
            app.finish_all_contacts();
            if app.board.is_some() {
                for canvas in &mut canvases {
                    canvas.background.live();
                }
            } else {
                let background = &mut canvases[active].background;
                if app.frozen {
                    background.freeze();
                    if app.pass || app.hidden {
                        background.pause_capture();
                    }
                } else {
                    background.live();
                }
            }
        }
        if cmd.pick && !app.screenshot.active() && !file_dialog.pending() {
            // Toolbar picks are brush picks, not delayed text-target results.
            text_pick = None;
            if picker.active() {
                picker.cancel();
            } else {
                if app.tool.is_text() && !app.pass {
                    app.for_each_page(App::stop);
                } else {
                    app.finish_all_contacts();
                }
                app.picking = true;
                picker.start(wayland || xwayland);
            }
        }
        if let Some(id) = cmd.display {
            requested_display = Some(id);
            topology_dirty = true;
        }
        if cmd.frame_rate {
            if let Err(error) = app.frame_rate.save(&state_dir().join("frame-rate.json")) {
                app.status = text(
                    "Frame-rate setting save failed",
                    "保存帧率设置失败",
                    "儲存影格率設定失敗",
                )
                .detail(error);
            }
            pacer.configure(app.frame_rate, app.refresh_hz(), Instant::now());
        }
        let redraw = cmd.redraw_canvas
            || mode_changed
            || cmd.background
            || cmd.pick
            || was_board != app.board;
        for canvas in &mut canvases {
            canvas.dirty |= redraw;
            canvas.guard_dirty |= controller_bounds_changed || mode_changed || cmd.pick;
        }

        let screenshot_active = app.screenshot.active();
        let hide = picker.hides_windows() || canvases.iter().any(|c| c.background.pending());
        toolbar_dirty |= capture_visibility(
            &mut canvases,
            &toolbar,
            &mut tooltips,
            &mut toolbar_hidden,
            hide,
            screenshot_active,
        )?;
        if toolbar_hidden && let Some(mut output) = output.take() {
            output.shapes.clear();
            output.platform_output.ime = None;
            gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
        }
        if hide {
            wait_for_events(Duration::from_millis(8));
            continue;
        }
        if app.input.text.editing() && !app.picking && !restore_text_focus {
            let toolbar_focused = toolbar.input_focused();
            if toolbar_focused {
                app.input.text.saw_input_focus();
            }
            let canvas = &mut canvases[active];
            let busy = toolbar_focused
                && (controller_moved
                    || !mouse_released()
                    || gui.ctx.input(|input| input.pointer.any_down())
                    || egui::Popup::is_any_open(&gui.ctx));
            match text_focus.update(
                Instant::now(),
                canvas.window.input_focused(),
                toolbar_focused,
                toolbar_text_input,
                busy,
            ) {
                text::FocusAction::Keep => (),
                text::FocusAction::ReturnToCanvas => {
                    gui.sync_text_input(&toolbar, false);
                    canvas.focus_text_editor();
                    canvas.keep_toolbar_above(&toolbar);
                }
                text::FocusAction::Finish => {
                    canvas.finish_text_editor(&mut app, theme)?;
                    toolbar_dirty = true;
                }
            }
        } else {
            text_focus.deadline = None;
        }
        // Stop the canvas editor before enabling the toolbar's IME.
        let toolbar_ime_active = toolbar_text_input
            && !toolbar_hidden
            && toolbar.input_focused()
            && !file_dialog.pending();
        gui.sync_text_input(&toolbar, toolbar_ime_active);
        let mut presented = false;
        if let Some(mut output) = output {
            presented = true;
            if !toolbar_ime_active {
                output.platform_output.ime = None;
            }
            let rail = crate::toolbar::colors(theme == egui::Theme::Dark).rail;
            toolbar_render.begin(&toolbar, rail.to_array().map(|c| f32::from(c) / 255.))?;
            gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
            toolbar_render.present()?;
            tooltips.update(&toolbar, &gui, app.language, &toolbar_color);
        }
        if !follow_controller {
            toolbar.sync_and_pump_events();
            controller_display = toolbar.display_id();
            follow_controller = true;
        }
        let keep_editor_open =
            restore_text_focus || toolbar.input_focused() || text_focus.deadline.is_some();
        let mouse = !picker.choosing() && app.mouse_ready();
        let mut unsafe_layer = false;
        let mut extended_ink_bytes: usize = canvases
            .iter()
            .map(canvas::Canvas::extended_ink_bytes)
            .sum();
        for (index, canvas) in canvases.iter_mut().enumerate() {
            let screenshot = app.screenshot.targets(canvas.window.id);
            let mouse = mouse && !screenshot;
            let visible = if file_dialog.pending() {
                screenshot && dialog::USE_WINDOW_PARENT
            } else {
                screenshot
                    || (options.smoke
                        || !app.hidden
                            && !app.suspended
                            && canvas.output.is_some_and(|id| connected.contains(&id)))
                        && (app.multi || index == active)
                        && (!picker.choosing() || index == active)
            };
            let raise_for_screenshot = visible && screenshot && (cmd.screenshot || !canvas.visible);
            canvas.show(&toolbar, visible)?;
            if raise_for_screenshot {
                canvas.window.raise();
            }
            if visible {
                if index == active && restore_text_focus && app.input.text.editing() {
                    gui.sync_text_input(&toolbar, false);
                    canvas.focus_text_editor();
                    canvas.keep_toolbar_above(&toolbar);
                    text_focus.deadline = Some(Instant::now() + Duration::from_millis(150));
                }
                if let Err(error) = canvas.update_guard(&toolbar, mouse) {
                    app.status = canvas::failure(error);
                    unsafe_layer = true;
                    break;
                }
                let frozen = canvas.background.frozen;
                let history_before = app.history_buttons();
                let selection_before = app.input.text.selected;
                extended_ink_bytes -= canvas.extended_ink_bytes();
                let painted = canvas.paint(
                    &mut app,
                    &options,
                    picker.choosing(),
                    keep_editor_open,
                    theme,
                    ink::MAX_EXTENDED_INK_TEXTURE_BYTES.saturating_sub(extended_ink_bytes),
                )?;
                presented |= painted;
                if screenshot && painted {
                    app.screenshot.presented();
                }
                extended_ink_bytes += canvas.extended_ink_bytes();
                toolbar_dirty |= frozen != canvas.background.frozen;
                toolbar_dirty |= history_before != app.history_buttons();
                toolbar_dirty |= selection_before != app.input.text.selected;
                if !options.smoke
                    && let Err(error) = canvas.apply_input(mouse)
                {
                    app.status = canvas::failure(error);
                    unsafe_layer = true;
                    break;
                }
            }
        }
        if unsafe_layer {
            restore_toolbar_focus |= app.screenshot.active();
            app.screenshot.abandon();
            app.hidden = true;
            toolbar_dirty = true;
            for canvas in &mut canvases {
                canvas.show(&toolbar, false)?;
            }
        } else if mode_changed || controller_bounds_changed {
            let toolbar_display = toolbar.display_id();
            if let Some(canvas) = canvases
                .iter()
                .find(|canvas| canvas.visible && (wayland || canvas.output == toolbar_display))
            {
                canvas.keep_toolbar_above(&toolbar);
            }
        }
        // Consume requests even when the picker cannot start, so an old
        // click does not unexpectedly launch a picker after switching modes.
        if app.input.text.take_screen_pick()
            && !app.picking
            && !app.screenshot.active()
            && !file_dialog.pending()
            && app.text_pick_target().is_some()
        {
            app.input.text.refocus();
            if let Some(target) = app.text_pick_target() {
                let canvas = &mut canvases[active];
                canvas.suspend_text_input();
                text_pick = Some((canvas.window.id, canvas.output, target));
                app.picking = true;
                picker.start(wayland || xwayland);
                toolbar_dirty = true;
                for canvas in &mut canvases {
                    canvas.dirty = true;
                    canvas.guard_dirty = true;
                }
            }
        }
        if let Some(action) = app.screenshot.action()
            && let Some(canvas) = canvases
                .iter_mut()
                .find(|canvas| app.screenshot.targets(canvas.window.id))
        {
            canvas.dirty = true;
            match action {
                MenuAction::Cancel => {
                    app.screenshot.cancel();
                    canvas.suspend_text_input();
                    toolbar_dirty = true;
                    restore_toolbar_focus = true;
                }
                MenuAction::SaveAs => open_save_as = true,
                MenuAction::Replace => {
                    if let Err(error) = app.screenshot.replace_file() {
                        app.screenshot.show_error(error);
                    }
                }
                MenuAction::Save => {
                    let directory = screenshot::default_folder(&app.preferences);
                    if let Some(path) = directory {
                        if let Err(error) = app.screenshot.save(Destination::Folder(path)) {
                            app.screenshot.show_error(error);
                            open_save_as = true;
                        }
                    } else {
                        open_save_as = true;
                    }
                }
            }
        }
        if open_save_as
            && !file_dialog.pending()
            && let Some(index) = canvases
                .iter()
                .position(|canvas| app.screenshot.targets(canvas.window.id))
        {
            if !dialog::USE_WINDOW_PARENT {
                // Keep the always-on-top overlays out of the system dialog's way.
                for canvas in &mut canvases {
                    canvas.show(&toolbar, false)?;
                }
            }
            app.screenshot.open_save_as(
                &mut file_dialog,
                &canvases[index].window,
                &app.preferences,
            );
        }
        if screenshot_active && !app.screenshot.active() {
            toolbar_dirty |= capture_visibility(
                &mut canvases,
                &toolbar,
                &mut tooltips,
                &mut toolbar_hidden,
                hide,
                false,
            )?;
        }
        if restore_toolbar_focus && !toolbar_hidden && !file_dialog.pending() {
            toolbar.raise();
            restore_toolbar_focus = false;
        }
        if !options.smoke
            && app.preferences.restore_session
            && app.last_save.elapsed() > Duration::from_secs(10)
            && !app.has_contacts()
        {
            if app.pages.needs_save(&app.history)
                && let Err(error) = app.save_session()
            {
                app.status = text("Autosave failed", "自动保存失败", "自動儲存失敗").detail(error);
                toolbar_dirty = true;
            }
            app.last_save = Instant::now();
        }
        frame += 1;
        if options.smoke && frame >= 3 {
            let path = state_dir();
            std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            std::fs::write(
                path.join("smoke.svg"),
                export::svg(app.history.document(), &app.text_fonts)?,
            )
            .map_err(|e| e.to_string())?;
            for (name, render) in [
                ("smoke.bmp", &canvases[active].render),
                ("toolbar.bmp", &toolbar_render),
            ] {
                render.save_bmp(&path.join(name))?;
            }
            println!(
                "OverInk smoke OK: {} strokes, native pressure and multi-touch events, egui textures, SDL geometry, SVG, BMP",
                app.history.strokes().len()
            );
            running = false;
        }
        if presented {
            pacer.wait();
        } else if (!toolbar_dirty || toolbar_hidden) && !topology_dirty {
            let mut timeout = if app.wait_for_mouse_up
                || picker.active()
                || file_dialog.pending()
                || app.screenshot.waiting()
            {
                Duration::from_millis(8)
            } else if app.input.text.editing() && toolbar.input_focused() {
                Duration::from_millis(24)
            } else {
                Duration::from_millis(500)
            };
            if let Some(deadline) = text_focus.deadline {
                timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
            }
            if !toolbar_hidden && let Some(at) = toolbar_repaint_at {
                timeout = timeout.min(at.saturating_duration_since(Instant::now()));
            }
            for at in canvases
                .iter()
                .filter(|canvas| canvas.visible)
                .filter_map(|canvas| canvas.text_repaint_at)
            {
                timeout = timeout.min(at.saturating_duration_since(Instant::now()));
            }
            wait_for_events(timeout);
        }
    }
    if !options.smoke {
        app.finish_session()?;
    }
    Ok(())
}

fn smoke_text_history(app: &mut App, canvas: &mut canvas::Canvas) -> Result<(), String> {
    app.use_tool(Tool::Text);
    let window_id = canvas.window.id;
    for (r#type, down) in [
        (SDL_EVENT_MOUSE_BUTTON_DOWN, true),
        (SDL_EVENT_MOUSE_BUTTON_UP, false),
    ] {
        canvas.text_event(
            app,
            &SDL_Event {
                button: SDL_MouseButtonEvent {
                    r#type,
                    windowID: window_id,
                    button: LEFT_BUTTON,
                    down,
                    x: 80.,
                    y: 70.,
                    ..Default::default()
                },
            },
        );
    }
    if !canvas.window.text_input_active() {
        return Err("Canvas text input smoke failed".into());
    }
    canvas.text_event(
        app,
        &SDL_Event {
            text: SDL_TextInputEvent {
                r#type: SDL_EVENT_TEXT_INPUT,
                windowID: window_id,
                text: c"Queued annotation".as_ptr(),
                ..Default::default()
            },
        },
    );
    canvas.flush_text_events(app, egui::Theme::Light)?;
    app.change_history(HistoryAction::Undo);
    if !app.history.text_boxes().is_empty() || app.history.can_undo() {
        return Err("Canvas text draft undo smoke failed".into());
    }
    app.change_history(HistoryAction::Redo);
    if app
        .history
        .text_boxes()
        .first()
        .is_none_or(|item| item.text != "Queued annotation")
    {
        return Err("Canvas text draft redo smoke failed".into());
    }
    app.use_tool(Tool::Pen);
    app.history = History::default();
    canvas.suspend_text_input();
    Ok(())
}

fn smoke_events(app: &mut App, id: SDL_WindowID) -> Result<(), String> {
    let mut e = SDL_Event::default();
    e.ptouch = SDL_PenTouchEvent {
        r#type: SDL_EVENT_PEN_DOWN,
        windowID: id,
        which: SDL_PenID(7),
        x: 90.,
        y: 160.,
        down: true,
        ..Default::default()
    };
    app.event(&e, id, [1100., 720.]);
    for i in 0..128 {
        e.paxis = SDL_PenAxisEvent {
            r#type: SDL_EVENT_PEN_AXIS,
            windowID: id,
            which: SDL_PenID(7),
            axis: SDL_PEN_AXIS_PRESSURE,
            value: i as f32 / 127.,
            x: 90. + i as f32 * 4.,
            y: 160. + (i as f32 * 0.08).sin() * 45.,
            pen_state: SDL_PEN_INPUT_DOWN,
            ..Default::default()
        };
        app.event(&e, id, [1100., 720.]);
    }
    e.ptouch = SDL_PenTouchEvent {
        r#type: SDL_EVENT_PEN_UP,
        windowID: id,
        which: SDL_PenID(7),
        x: 598.,
        y: 160.,
        down: false,
        ..Default::default()
    };
    app.event(&e, id, [1100., 720.]);
    if app.history.strokes().len() != 1
        || app.history.strokes()[0].points.len() < 128
        || app.history.strokes()[0].points[0].pressure != 0.
    {
        return Err("Pen-event smoke failed".into());
    }
    for (i, tool) in [Tool::Arrow, Tool::Rectangle, Tool::Ellipse, Tool::Marker]
        .into_iter()
        .enumerate()
    {
        app.tool = tool;
        app.color = InkColor::from_rgb8(PRESETS[i]);
        app.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(100. + i as f32 * 200., 300., 1., [0.; 2]),
            false,
        );
        app.motion(
            Source::Mouse(LEFT_BUTTON),
            Point::new(250. + i as f32 * 200., 500., 1., [0.; 2]),
        );
        app.end(Source::Mouse(LEFT_BUTTON));
    }
    let mut proximity = SDL_Event::default();
    proximity.pproximity = SDL_PenProximityEvent {
        r#type: SDL_EVENT_PEN_PROXIMITY_OUT,
        which: SDL_PenID(7),
        ..Default::default()
    };
    app.event(&proximity, id, [1100., 720.]);
    app.tool = Tool::Pen;
    for (finger, x) in [(1, 0.15), (2, 0.45)] {
        let event = SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: SDL_EVENT_FINGER_DOWN,
                touchID: SDL_TouchID(9),
                fingerID: SDL_FingerID(finger),
                windowID: id,
                x,
                y: 0.8,
                ..Default::default()
            },
        };
        app.event(&event, id, [1100., 720.]);
    }
    if app.input.touches.len() != 2 {
        return Err("Multi-touch down smoke failed".into());
    }
    for (finger, x) in [(1, 0.35), (2, 0.65)] {
        let event = SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: SDL_EVENT_FINGER_UP,
                touchID: SDL_TouchID(9),
                fingerID: SDL_FingerID(finger),
                windowID: id,
                x,
                y: 0.9,
                ..Default::default()
            },
        };
        app.event(&event, id, [1100., 720.]);
    }
    if app.history.strokes().len() != 7 || !app.input.touches.is_empty() {
        return Err("Multi-touch commit smoke failed".into());
    }
    let annotation = crate::model::TextBox {
        origin: [80., 70.],
        size: [360., 80.],
        font_size: 28.,
        color: InkColor::default(),
        text: "OverInk 文字 😀".into(),
        scroll_offset: 0.,
    };
    app.history.set_text_box(None, annotation);
    app.tool = Tool::Laser;
    app.begin(
        Source::Mouse(LEFT_BUTTON),
        Point::new(88., 103., 1., [0.; 2]),
        false,
    );
    app.motion(
        Source::Mouse(LEFT_BUTTON),
        Point::new(340., 103., 1., [0.; 2]),
    );
    app.end(Source::Mouse(LEFT_BUTTON));
    Ok(())
}

#[cfg(test)]
mod touch_tests {
    use super::*;
    pub(super) fn app() -> App {
        App {
            input: CanvasInput::default(),
            parked_input: BTreeMap::new(),
            multi: false,
            history: History::default(),
            tool: Tool::Pen,
            color: InkColor::default(),
            hex_entry: crate::color::HexEntry::new(InkColor::default()),
            picking: false,
            width: 5.,
            opacity: 1.,
            pressure: true,
            curve: 1.,
            pass: false,
            wait_for_mouse_up: false,
            hidden: false,
            board: Some(Board::White),
            display: 0,
            displays: vec![],
            pages: Pages::default(),
            frame_rate: FrameRate::Auto,
            preferences: Preferences::default(),
            text_fonts: TextFonts::default(),
            font_path_input: None,
            screenshot: Screenshot::default(),
            #[cfg(target_os = "linux")]
            gnome_guide: None,
            language: Language::English,
            suspended: false,
            frozen: false,
            panel: None,
            touch_enabled: true,
            status: Status::default(),
            path: String::new(),
            save_path: PathBuf::new(),
            last_save: Instant::now(),
        }
    }
    fn finger(app: &mut App, ty: SDL_EventType, device: u64, id: u64, x: f32, y: f32) {
        let event = SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: ty,
                touchID: SDL_TouchID(device),
                fingerID: SDL_FingerID(id),
                windowID: if ty == SDL_EVENT_FINGER_CANCELED {
                    SDL_WindowID(0)
                } else {
                    SDL_WindowID(1)
                },
                x,
                y,
                ..Default::default()
            },
        };
        app.event(&event, SDL_WindowID(1), [800., 600.]);
    }

    #[test]
    fn text_tool_accepts_ink_cleanup_events_and_rejects_new_strokes() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.3, 0.1);
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(30., 30., 1., [0.; 2]),
            false,
        );
        a.tool = Tool::Text;
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.2, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_CANCELED, 1, 2, 0.3, 0.1);
        let overlay = SDL_WindowID(1);
        a.event(
            &SDL_Event {
                window: SDL_WindowEvent {
                    r#type: SDL_EVENT_WINDOW_FOCUS_LOST,
                    windowID: overlay,
                    ..Default::default()
                },
            },
            overlay,
            [800., 600.],
        );
        assert!(!a.input.has_contacts());
        assert_eq!(a.history.strokes().len(), 2);

        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(30., 30., 1., [0.; 2]),
            false,
        );
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 3, 0.4, 0.1);
        assert!(!a.input.has_contacts());
        assert!(a.history.active.is_none());
    }
    #[test]
    fn mouse_buttons_own_their_contacts_and_erase_is_one_edit() {
        fn button(a: &mut App, kind: SDL_EventType, button: u8, x: f32) {
            a.event(
                &SDL_Event {
                    button: SDL_MouseButtonEvent {
                        r#type: kind,
                        windowID: SDL_WindowID(1),
                        button,
                        x,
                        y: 80.,
                        ..Default::default()
                    },
                },
                SDL_WindowID(1),
                [800., 600.],
            );
        }
        let mut a = app();
        button(&mut a, SDL_EVENT_MOUSE_BUTTON_DOWN, LEFT_BUTTON, 250.);
        button(
            &mut a,
            SDL_EVENT_MOUSE_BUTTON_UP,
            SDL_BUTTON_MIDDLE as u8,
            250.,
        );
        assert!(a.history.active.is_some());
        button(&mut a, SDL_EVENT_MOUSE_BUTTON_UP, LEFT_BUTTON, 250.);
        button(&mut a, SDL_EVENT_MOUSE_BUTTON_DOWN, LEFT_BUTTON, 100.);
        // The right button commits the open left stroke before erasing.
        button(&mut a, SDL_EVENT_MOUSE_BUTTON_DOWN, RIGHT_BUTTON, 100.);
        button(&mut a, SDL_EVENT_MOUSE_BUTTON_UP, LEFT_BUTTON, 100.);
        a.motion(
            Source::Mouse(RIGHT_BUTTON),
            Point::new(250., 80., 1., [0.; 2]),
        );
        a.end(Source::Mouse(RIGHT_BUTTON));
        assert!(a.history.active.is_none() && a.history.strokes().is_empty());
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 2);
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 1);
    }
    fn add_ink(a: &mut App, xs: &[f32]) {
        for &x in xs {
            let source = Source::Mouse(LEFT_BUTTON);
            a.begin(source, Point::new(x, 60., 1., [0.; 2]), false);
            a.end(source);
        }
    }
    #[test]
    fn escape_cancels_erase_preview_without_changing_redo() {
        let mut a = app();
        add_ink(&mut a, &[80., 240.]);
        a.history.undo();
        a.history.changed = false;
        a.use_tool(Tool::Eraser);
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(80., 60., 1., [0.; 2]),
            false,
        );
        let mut fragments = 0;
        assert!(Erasure::preview(
            &a.history.strokes()[0],
            0,
            a.input.pending_erasures.values(),
            |_, _, _| fragments += 1,
        ));
        assert_eq!(fragments, 0);
        assert_eq!(a.history.strokes().len(), 1);
        a.keyboard(
            &SDL_Event {
                key: SDL_KeyboardEvent {
                    r#type: SDL_EVENT_KEY_DOWN,
                    key: SDLK_ESCAPE,
                    ..Default::default()
                },
            },
            false,
        );
        assert!(a.input.pending_erasures.is_empty());
        assert!(a.input.contact.is_none() && !a.history.changed);
        a.history.redo();
        assert_eq!(a.history.strokes().len(), 2);
    }
    #[test]
    fn history_shortcuts_cancel_a_touch_erase_before_changing_indices() {
        let mut a = app();
        add_ink(&mut a, &[80., 240.]);
        a.use_tool(Tool::Eraser);
        for (key, expected) in [(SDLK_Z, 1), (SDLK_Y, 2)] {
            finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
            assert_eq!(a.input.pending_erasures.len(), 1);
            a.keyboard(
                &SDL_Event {
                    key: SDL_KeyboardEvent {
                        r#type: SDL_EVENT_KEY_DOWN,
                        key,
                        r#mod: if cfg!(target_os = "macos") {
                            SDL_KMOD_LGUI
                        } else {
                            SDL_KMOD_LCTRL
                        },
                        ..Default::default()
                    },
                },
                false,
            );
            assert!(a.input.pending_erasures.is_empty());
            finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.1, 0.1);
            assert_eq!(a.history.strokes().len(), expected);
            assert_eq!(a.history.strokes()[0].points[0].x, 80.);
        }
    }

    #[test]
    fn wide_output_tracks_visible_ink_through_commit_and_undo() {
        use crate::color::Space;

        let mut a = app();
        assert!(!a.has_wide_ink());
        add_ink(&mut a, &[80.]);
        assert!(!a.has_wide_ink());
        a.color = InkColor::from_channels([0, 255, 0], Space::DisplayP3);
        a.begin(Source::Pen(1), Point::new(70., 80., 1., [0.; 2]), false);
        assert!(a.has_wide_ink());
        a.end(Source::Pen(1));
        assert!(a.has_wide_ink());
        a.history.undo();
        assert!(!a.has_wide_ink());
        a.history.redo();
        assert!(a.has_wide_ink());
    }
    #[test]
    fn finger_cancel_and_palm_rejection_discard_only_their_erase_previews() {
        let mut a = app();
        add_ink(&mut a, &[80., 240., 480.]);
        a.use_tool(Tool::Eraser);
        a.begin(
            Source::Mouse(RIGHT_BUTTON),
            Point::new(240., 60., 1., [0.; 2]),
            true,
        );
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.6, 0.1);
        assert_eq!(a.input.pending_erasures.len(), 3);
        finger(&mut a, SDL_EVENT_FINGER_CANCELED, 1, 1, 0.1, 0.1);
        assert_eq!(a.input.pending_erasures.len(), 2);
        assert_eq!(a.history.strokes().len(), 3);
        a.event(
            &SDL_Event {
                pproximity: SDL_PenProximityEvent {
                    r#type: SDL_EVENT_PEN_PROXIMITY_IN,
                    which: SDL_PenID(7),
                    windowID: SDL_WindowID(1),
                    ..Default::default()
                },
            },
            SDL_WindowID(1),
            [800., 600.],
        );
        assert!(a.input.touches.is_empty());
        assert_eq!(a.input.pending_erasures.len(), 1);
        a.end(Source::Mouse(RIGHT_BUTTON));
        assert_eq!(
            a.history
                .strokes()
                .iter()
                .map(|s| s.points[0].x)
                .collect::<Vec<_>>(),
            [80., 480.]
        );
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 3);
    }
    #[test]
    fn concurrent_erasers_commit_independently_after_indices_shift() {
        let mut a = app();
        add_ink(&mut a, &[80., 240., 480.]);
        a.use_tool(Tool::Eraser);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.6, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.1, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 2, 0.6, 0.1);
        assert_eq!(a.history.strokes().len(), 1);
        assert_eq!(a.history.strokes()[0].points[0].x, 240.);
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 2);
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 3);
    }
    #[test]
    fn concurrent_erases_split_one_stroke_and_undo_independently() {
        let mut a = app();
        let mouse = Source::Mouse(LEFT_BUTTON);
        a.begin(mouse, Point::new(10., 60., 0.2, [0.; 2]), false);
        a.motion(mouse, Point::new(700., 60., 0.8, [45., 0.]));
        a.end(mouse);
        a.use_tool(Tool::Eraser);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.6, 0.1);
        let mut fragments = 0;
        assert!(Erasure::preview(
            &a.history.strokes()[0],
            0,
            a.input.pending_erasures.values(),
            |_, _, _| fragments += 1,
        ));
        assert_eq!(fragments, 3);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.1, 0.1);
        assert_eq!(a.history.strokes().len(), 2);
        assert_eq!(a.history.point_count(), 4);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 2, 0.6, 0.1);
        let strokes = a.history.strokes();
        assert_eq!(strokes.len(), 3);
        assert_eq!(a.history.point_count(), 6);
        assert_eq!(strokes[0].points[0].x, 10.);
        assert_eq!(strokes[2].points.last().unwrap().x, 700.);
        for (left, right, x) in [
            (&strokes[0], &strokes[1], 80.),
            (&strokes[1], &strokes[2], 480.),
        ] {
            assert!(left.points.last().unwrap().x < x && right.points[0].x > x);
        }
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 2);
        assert_eq!(a.history.point_count(), 4);
        a.history.undo();
        assert_eq!(a.history.strokes().len(), 1);
        assert_eq!(a.history.point_count(), 2);
        a.history.redo();
        a.history.redo();
        assert_eq!(a.history.strokes().len(), 3);
        assert_eq!(a.history.point_count(), 6);
    }
    #[test]
    fn pen_tilt_before_pressure_and_tool_changes_keep_proximity() {
        let mut a = app();
        let id = SDL_WindowID(1);
        let size = [800., 600.];
        a.event(
            &SDL_Event {
                ptouch: SDL_PenTouchEvent {
                    r#type: SDL_EVENT_PEN_DOWN,
                    windowID: id,
                    which: SDL_PenID(4),
                    x: 100.,
                    y: 80.,
                    down: true,
                    ..Default::default()
                },
            },
            id,
            size,
        );
        a.event(
            &SDL_Event {
                paxis: SDL_PenAxisEvent {
                    r#type: SDL_EVENT_PEN_AXIS,
                    windowID: id,
                    which: SDL_PenID(4),
                    x: 200.,
                    y: 80.,
                    axis: SDL_PEN_AXIS_XTILT,
                    value: 20.,
                    ..Default::default()
                },
            },
            id,
            size,
        );
        let points = &a.history.active.as_ref().unwrap().points;
        assert_eq!(points.len(), 2);
        assert!(points.iter().all(|p| p.pressure == 0.5));
        a.use_tool(Tool::Pencil);
        a.reset_contacts();
        a.history.clear();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 5, 0.4, 0.4);
        assert!(a.input.touches.is_empty() && a.input.pen_proximity.contains(&4));
    }
    #[test]
    fn first_pen_pressure_replaces_fallback_and_stationary_samples_merge() {
        let mut app = app();
        app.tool = Tool::Pencil;
        let id = SDL_WindowID(1);
        let pen = SDL_PenID(4);
        let size = [800., 600.];
        let axis = |app: &mut App, axis, value, x, pen_state| {
            app.event(
                &SDL_Event {
                    paxis: SDL_PenAxisEvent {
                        r#type: SDL_EVENT_PEN_AXIS,
                        windowID: id,
                        which: pen,
                        axis,
                        value,
                        x,
                        y: 80.,
                        pen_state,
                        ..Default::default()
                    },
                },
                id,
                size,
            );
        };
        let contact = |app: &mut App, down, x| {
            app.event(
                &SDL_Event {
                    ptouch: SDL_PenTouchEvent {
                        r#type: if down {
                            SDL_EVENT_PEN_DOWN
                        } else {
                            SDL_EVENT_PEN_UP
                        },
                        windowID: id,
                        which: pen,
                        x,
                        y: 80.,
                        down,
                        ..Default::default()
                    },
                },
                id,
                size,
            );
        };
        let pressure = SDL_PEN_AXIS_PRESSURE;
        let down = SDL_PEN_INPUT_DOWN;
        let hover = Default::default();
        axis(&mut app, pressure, 0., 100., hover);
        contact(&mut app, true, 100.);
        axis(&mut app, SDL_PEN_AXIS_XTILT, 20., 101., down);
        axis(&mut app, pressure, 0.02, 101., down);
        axis(&mut app, pressure, 0.7, 130., down);
        axis(&mut app, pressure, 0.8, 130., down);
        axis(&mut app, SDL_PEN_AXIS_XTILT, 60., 130., down);
        axis(&mut app, pressure, 0.01, 130., down);
        contact(&mut app, false, 130.);
        let points = &app.history.strokes()[0].points;
        assert_eq!(points.len(), 3);
        assert_eq!([points[0].pressure, points[1].pressure], [0.02; 2]);
        assert_eq!(points[2].pressure, 0.8);
        assert_eq!(points[2].tilt[0], 60.);

        // Windows can report the first nonzero pressure before the down event.
        axis(&mut app, pressure, 0.03, 200., hover);
        contact(&mut app, true, 200.);
        axis(&mut app, pressure, 0.9, 240., down);
        contact(&mut app, false, 240.);
        assert_eq!(app.history.strokes()[1].points[0].pressure, 0.03);
    }
    #[test]
    fn laser_trails_are_separate_across_releases_and_fingers() {
        let mut a = app();
        a.tool = Tool::Laser;
        for x in [20., 400.] {
            let mouse = Source::Mouse(LEFT_BUTTON);
            a.begin(mouse, Point::new(x, 10., 1., [0.; 2]), false);
            a.motion(mouse, Point::new(x, 20., 1., [0.; 2]));
            a.end(mouse);
        }
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.3, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.8, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.3, 0.3);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 2, 0.8, 0.3);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.3, 0.5);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 2, 0.8, 0.5);
        assert_eq!(a.input.laser.len(), 4);
        assert!(a.input.laser.values().all(|trail| {
            trail.len() >= 2
                && trail
                    .iter()
                    .all(|sample| sample.point.x == trail[0].point.x)
        }));
        assert!(a.history.strokes().is_empty());
    }
    #[test]
    fn mouse_commits_the_unfinished_stroke_and_any_tool_resumes_drawing() {
        let mut a = app();
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(10., 20., 0.5, [0.; 2]),
            false,
        );
        a.motion(
            Source::Mouse(LEFT_BUTTON),
            Point::new(50., 60., 0.7, [0.; 2]),
        );
        a.use_mouse();
        assert!(a.pass);
        assert!(a.history.active.is_none());
        assert!(a.input.contact.is_none());
        assert_eq!(a.history.strokes().len(), 1);
        assert_eq!(a.history.strokes()[0].points.len(), 2);
        a.motion(
            Source::Mouse(LEFT_BUTTON),
            Point::new(90., 90., 1., [0.; 2]),
        );
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(100., 100., 1., [0.; 2]),
            false,
        );
        assert!(a.history.active.is_none());
        for tool in Tool::ALL {
            a.hidden = true;
            a.use_tool(tool);
            assert!(!a.pass && !a.hidden);
            assert_eq!(a.tool, tool);
            a.use_mouse();
        }
        assert_eq!(a.history.strokes().len(), 1);
        assert_eq!(a.history.strokes()[0].points.len(), 2);
    }
    #[test]
    fn controller_area_rejects_pen_touch_and_eraser_edits() {
        let mut a = app();
        a.input.protected = Some(crate::controller::Rect {
            x: 100,
            y: 100,
            w: 300,
            h: 200,
        });
        a.begin(Source::Pen(1), Point::new(150., 150., 1., [0.; 2]), false);
        assert!(a.history.active.is_none());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.3);
        assert!(a.input.touches.is_empty());
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(50., 150., 1., [0.; 2]),
            false,
        );
        a.motion(
            Source::Mouse(LEFT_BUTTON),
            Point::new(150., 150., 1., [0.; 2]),
        );
        assert!(a.history.active.is_none());
        assert_eq!(a.history.strokes().len(), 1);
        assert!(a.history.strokes()[0].points.iter().all(|p| p.x < 100.));
        a.use_tool(Tool::Eraser);
        let count = a.history.strokes().len();
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(150., 150., 1., [0.; 2]),
            true,
        );
        assert_eq!(a.history.strokes().len(), count);
    }

    #[test]
    fn fast_pointer_crossing_stops_ink_and_eraser_at_the_toolbar() {
        for source in [Source::Mouse(LEFT_BUTTON), Source::Pen(1)] {
            let mut a = app();
            a.input.protected = Some(crate::controller::Rect {
                x: 100,
                y: 100,
                w: 56,
                h: 100,
            });
            let p = |x| Point::new(x, 150., 1., [0.; 2]);
            a.begin(source, p(45.), false);
            a.motion(source, p(65.));
            a.motion(source, p(230.));
            assert!(a.input.contact.is_none());
            assert_eq!(a.history.strokes()[0].points.last().unwrap().x, 65.);

            let far_side = Stroke {
                points: vec![p(175.), p(215.)],
                ..a.stroke(Tool::Pen)
            };
            a.history.append(std::iter::once(far_side));
            let far_side = a.history.strokes()[1].clone();
            a.begin(source, p(75.), true);
            a.motion(source, p(230.));
            assert!(a.input.contact.is_none());
            assert_eq!(a.history.strokes().len(), 2);
            assert!(std::sync::Arc::ptr_eq(&a.history.strokes()[1], &far_side));
        }
    }

    #[test]
    fn touch_shapes_keep_one_anchor_across_the_toolbar() {
        for tool in Tool::ALL.into_iter().filter(|tool| tool.is_shape()) {
            let mut a = app();
            a.use_tool(tool);
            a.input.protected = Some(crate::controller::Rect {
                x: 100,
                y: 100,
                w: 56,
                h: 100,
            });
            finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.05, 0.25);
            finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.1, 0.25);
            finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.15, 0.25);
            finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.45, 0.25);
            finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.5, 0.3);
            let [shape] = a.history.strokes() else {
                panic!("expected one unbroken shape");
            };
            assert_eq!(shape.tool, tool);
            assert_eq!(shape.points.len(), 2);
            assert_eq!((shape.points[0].x, shape.points[1].x), (40., 400.));
        }
    }

    #[test]
    fn sample_limit_opens_a_panel_for_pointer_touch_and_rejected_erase() {
        let mut a = app();
        let p = |x, y| Point::new(x, y, 1., [0.; 2]);
        a.history.set_document(
            crate::model::Document {
                strokes: vec![
                    Stroke {
                        points: vec![p(1000., 1000.); MAX_POINTS - 2],
                        ..a.stroke(Tool::Pen)
                    }
                    .into(),
                    Stroke {
                        points: vec![p(20., 50.), p(120., 50.)],
                        ..a.stroke(Tool::Pen)
                    }
                    .into(),
                ],
                ..crate::model::Document::default()
            },
            true,
        );
        let panel = Some(crate::toolbar::Panel::Settings);
        let mouse = Source::Mouse(LEFT_BUTTON);
        a.begin(mouse, p(70., 50.), false);
        assert!(a.input.contact.is_none());
        assert_eq!(a.panel, panel);
        assert!(a.status.get(Language::English).contains("limit reached"));

        a.panel = None;
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        assert!(a.input.touches.is_empty());
        assert_eq!(a.panel, panel);
        assert!(a.status.get(Language::English).contains("limit reached"));

        a.panel = None;
        a.begin(mouse, p(70., 50.), true);
        a.end(mouse);
        assert_eq!(a.history.strokes().len(), 2);
        assert_eq!(a.history.point_count(), MAX_POINTS);
        assert!(!a.history.can_undo());
        assert_eq!(a.panel, panel);
        assert!(
            a.status
                .get(Language::English)
                .contains("erase would exceed")
        );
    }

    #[test]
    fn touch_ink_crossing_the_controller_creates_two_fragments_and_one_undo() {
        let mut a = app();
        a.input.protected = Some(crate::controller::Rect {
            x: 100,
            y: 100,
            w: 200,
            h: 100,
        });
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.05, 0.25);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.1, 0.25);
        // The touch jumps to the far side without a sample inside the toolbar.
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.45, 0.25);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.5, 0.25);
        let [left, right] = a.history.strokes() else {
            panic!("expected two ink fragments");
        };
        assert_eq!(left.points.len(), 2);
        assert_eq!(right.points.len(), 2);
        assert!(left.points.iter().all(|p| p.x < 100.));
        assert!(right.points.iter().all(|p| p.x > 300.));
        assert_eq!(a.history_buttons(), [true, false, true]);
        a.history.undo();
        assert_eq!(a.history_buttons(), [false, true, false]);
    }

    #[test]
    fn touch_laser_breaks_at_the_controller_without_losing_the_fading_trail() {
        let mut a = app();
        a.input.protected = Some(crate::controller::Rect {
            x: 100,
            y: 100,
            w: 200,
            h: 100,
        });
        a.use_tool(Tool::Laser);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.05, 0.25);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.1, 0.25);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.2, 0.25);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 1, 0.45, 0.25);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.5, 0.25);
        let trail = a.input.laser.values().next().unwrap();
        assert_eq!(trail.len(), 4);
        assert!(trail[0].starts_segment && !trail[1].starts_segment);
        assert!(trail[2].starts_segment && !trail[3].starts_segment);
        assert!(trail[1].point.x < 100. && trail[2].point.x > 300.);
    }
    #[test]
    fn tool_hotkeys_and_space_switch_between_mouse_and_drawing() {
        let mut a = app();
        a.use_mouse();
        for (key, tool) in [
            (SDLK_P, Tool::Pen),
            (SDLK_B, Tool::Pencil),
            (SDLK_E, Tool::Eraser),
        ] {
            let event = SDL_Event {
                key: SDL_KeyboardEvent {
                    r#type: SDL_EVENT_KEY_DOWN,
                    key,
                    ..Default::default()
                },
            };
            assert!(a.keyboard(&event, false).mode);
            assert!(!a.pass);
            assert_eq!(a.tool, tool);
            let space = SDL_Event {
                key: SDL_KeyboardEvent {
                    r#type: SDL_EVENT_KEY_DOWN,
                    key: SDLK_SPACE,
                    ..Default::default()
                },
            };
            assert!(a.keyboard(&space, false).mode);
            assert!(a.pass);
        }
    }
    fn screen(id: u32, x: i32, refresh: f64) -> Display {
        Display {
            id,
            name: format!("Screen {id}"),
            bounds: [x, 0, 1920, 1080],
            refresh,
            scale: 1.0,
        }
    }
    #[test]
    fn display_selection_survives_a_refresh_without_controller_movement() {
        let mut a = app();
        let displays = vec![screen(10, 0, 60.0), screen(20, 1920, 120.0)];
        a.displays = displays.clone();
        a.pages = Pages::new(Some(20)); // selected by --display 2
        a.refresh_displays(displays.clone(), None);
        assert_eq!(a.pages.active, Some(20));
        a.refresh_displays(displays.clone(), Some(10)); // settings selection
        a.refresh_displays(displays.clone(), None);
        assert_eq!(a.pages.active, Some(10));
        a.refresh_displays(displays, Some(20)); // toolbar actually crosses screens
        assert_eq!(a.pages.active, Some(20));
    }
    #[test]
    fn controller_moves_between_pages_and_refresh_rates_without_mixing_strokes() {
        let mut a = app();
        a.displays = vec![screen(10, 0, 60.0), screen(20, 1920, 120.0)];
        a.pages = Pages::new(Some(10));
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.2, 0.2);
        let (layout, removed) =
            a.refresh_displays(vec![screen(20, 1920, 120.0), screen(10, 0, 60.0)], Some(20));
        assert!(layout);
        assert!(!removed);
        assert!(a.history.strokes().is_empty());
        assert_eq!(a.frame_rate.hz(a.refresh_hz()), 120.0);
        a.refresh_displays(vec![screen(20, 1920, 90.0), screen(10, 0, 60.0)], Some(10));
        assert_eq!(a.history.strokes().len(), 1);
        assert_eq!(a.pages.active, Some(10));
        assert_eq!(a.display, 1);
        a.refresh_displays(vec![screen(20, 1920, 90.0), screen(10, 0, 60.0)], Some(20));
        assert_eq!(a.refresh_hz(), 90.0);
    }
    #[test]
    fn unplugging_clears_contacts_and_cannot_be_undone_or_draw_on_a_missing_screen() {
        let mut a = app();
        a.displays = vec![screen(10, 0, 60.0)];
        a.pages = Pages::new(Some(10));
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        let (layout, removed) = a.refresh_displays(vec![], None);
        assert!(layout && removed && a.suspended);
        assert!(a.input.touches.is_empty());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        a.history.undo();
        a.history.redo();
        assert!(a.history.strokes().is_empty());
        assert!(a.input.touches.is_empty());
        a.refresh_displays(vec![screen(30, 0, 60.0)], Some(30));
        assert!(!a.suspended);
        assert!(a.history.strokes().is_empty());
    }
    #[test]
    fn multi_window_touches_and_unplug_are_local_to_their_output() {
        let left = screen(10, 0, 60.0);
        let right = screen(20, 1920, 120.0);
        let mut a = app();
        a.multi = true;
        a.refresh_displays(vec![left.clone(), right.clone()], Some(10));
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.1, 0.1);
        a.select_page(20);
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 2, 2, 0.25, 0.1);
        a.on_page(Some(10), |left_page| {
            assert!(left_page.input.has_contacts());
            finger(left_page, SDL_EVENT_FINGER_UP, 1, 1, 0.1, 0.2);
            assert_eq!(left_page.history.strokes()[0].points[0].x, 80.);
        });
        assert_eq!(a.pages.active, Some(20));
        assert!(a.input.has_contacts());
        finger(&mut a, SDL_EVENT_FINGER_UP, 2, 2, 0.25, 0.2);
        assert_eq!(a.history.strokes()[0].points[0].x, 200.);
        a.refresh_displays(vec![right.clone()], None);
        a.refresh_displays(vec![left, right], None);
        a.select_page(10);
        a.history.undo();
        assert!(a.history.strokes().is_empty());
        a.select_page(20);
        assert_eq!(a.history.strokes()[0].points[0].x, 200.);
    }

    #[test]
    fn disabling_session_recovery_discards_the_cache_and_keeps_manual_saves() {
        let state = std::env::temp_dir().join(format!(
            "overink-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let displays = vec![screen(10, 0, 60.0), screen(20, 1920, 120.0)];
        let mut a = App::new(true, 0, displays.clone(), &state, Preferences::default());
        add_ink(&mut a, &[120.]);
        a.select_page(20);
        add_ink(&mut a, &[440.]);
        a.save();
        a.finish_session().unwrap();
        assert!(a.save_path.is_file());
        assert!(Path::new(&a.path).is_file());

        let resumed = App::new(true, 1, displays.clone(), &state, Preferences::default());
        assert_eq!(resumed.history.strokes()[0].points[0].x, 440.);
        Preferences {
            restore_session: false,
            ..Preferences::default()
        }
        .save(&state.join("ui.json"))
        .unwrap();
        let mut private = App::new(
            true,
            1,
            displays,
            &state,
            Preferences::load(&state.join("ui.json")),
        );
        assert!(private.history.strokes().is_empty());
        add_ink(&mut private, &[640.]);
        private.finish_session().unwrap();
        assert!(!private.save_path.exists());
        assert!(Path::new(&private.path).is_file());
        private.load();
        assert_eq!(private.history.strokes()[0].points[0].x, 440.);
        std::fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn svg_export_creates_the_default_directory_on_first_launch() {
        let state = std::env::temp_dir().join(format!("overink-svg-export-{}", std::process::id()));
        let mut app = App::new(
            true,
            0,
            vec![screen(10, 0, 60.)],
            &state,
            Preferences {
                restore_session: false,
                ..Preferences::default()
            },
        );
        assert!(!state.exists());
        add_ink(&mut app, &[20., 40.]);
        app.export();
        let svg = std::fs::read_to_string(Path::new(&app.path).with_extension("svg")).unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("<path"));
        std::fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn finger_stroke_is_saved_in_logical_units_without_fake_pen_pressure() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 3, 0.1, 0.2);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 3, 0.5, 0.5);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 3, 0.5, 0.5);
        assert!(a.input.touches.is_empty());
        let s = &a.history.strokes()[0];
        assert_eq!(s.points.len(), 2);
        assert_eq!([s.points[1].x, s.points[1].y], [400., 300.]);
        assert!(!s.pressure_enabled);
        a.history.undo();
        assert!(a.history.strokes().is_empty());
        a.history.redo();
        assert_eq!(a.history.strokes().len(), 1);
    }
    #[test]
    fn multiple_fingers_and_devices_do_not_join_strokes() {
        let mut a = app();
        for (d, f, x) in [(1, 1, 0.1), (1, 2, 0.4), (2, 1, 0.7)] {
            finger(&mut a, SDL_EVENT_FINGER_DOWN, d, f, x, 0.1);
        }
        assert_eq!(a.input.touches.len(), 3);
        for (d, f, x) in [(1, 1, 0.1), (1, 2, 0.4), (2, 1, 0.7)] {
            finger(&mut a, SDL_EVENT_FINGER_UP, d, f, x, 0.7);
        }
        assert_eq!(a.history.strokes().len(), 3);
        for s in a.history.strokes() {
            assert_eq!(s.points[0].x, s.points[1].x);
        }
    }
    #[test]
    fn canceled_and_focus_lost_contacts_are_discarded() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        finger(&mut a, SDL_EVENT_FINGER_CANCELED, 1, 1, 0.2, 0.2);
        assert!(a.input.touches.is_empty() && a.history.strokes().is_empty());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.2, 0.2);
        a.event(
            &SDL_Event {
                window: SDL_WindowEvent {
                    r#type: SDL_EVENT_WINDOW_FOCUS_LOST,
                    windowID: SDL_WindowID(1),
                    ..Default::default()
                },
            },
            SDL_WindowID(1),
            [800., 600.],
        );
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 2, 0.5, 0.5);
        assert!(a.input.touches.is_empty() && a.history.strokes().is_empty());
    }
    #[test]
    fn pen_proximity_rejects_palms_and_requires_a_new_finger_down() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        let pen = |ty| SDL_Event {
            pproximity: SDL_PenProximityEvent {
                r#type: ty,
                which: SDL_PenID(7),
                windowID: SDL_WindowID(1),
                ..Default::default()
            },
        };
        a.event(
            &pen(SDL_EVENT_PEN_PROXIMITY_IN),
            SDL_WindowID(1),
            [800., 600.],
        );
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.3, 0.3);
        assert!(a.input.touches.is_empty());
        a.event(
            &pen(SDL_EVENT_PEN_PROXIMITY_OUT),
            SDL_WindowID(1),
            [800., 600.],
        );
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.4, 0.4);
        assert!(a.history.strokes().is_empty());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 3, 0.2, 0.2);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 3, 0.4, 0.4);
        assert_eq!(a.history.strokes().len(), 1);
    }
    #[test]
    fn emulated_touch_mouse_packets_are_ignored() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        let event = SDL_Event {
            button: SDL_MouseButtonEvent {
                r#type: SDL_EVENT_MOUSE_BUTTON_DOWN,
                windowID: SDL_WindowID(1),
                which: SDL_TOUCH_MOUSEID,
                button: 1,
                down: true,
                x: 160.,
                y: 120.,
                ..Default::default()
            },
        };
        a.event(&event, SDL_WindowID(1), [800., 600.]);
        assert!(a.history.active.is_none());
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.4, 0.4);
        assert_eq!(a.history.strokes().len(), 1);
    }
    #[test]
    fn disabled_or_passthrough_touch_cannot_draw() {
        let mut a = app();
        a.touch_enabled = false;
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        a.touch_enabled = true;
        a.pass = true;
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.2, 0.2);
        assert!(a.input.touches.is_empty() && a.history.strokes().is_empty());
    }
}
