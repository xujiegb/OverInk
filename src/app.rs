#![forbid(unsafe_code)]

mod canvas;
mod ink;
mod osk;
mod panel;
#[cfg(test)]
mod pen_input_tests;
mod screenshot;
mod stylus;
mod text;

use crate::sdl::dialog::{DialogKind, FileDialog};
use crate::{
    appearance::{KeyboardSource, Language, Preferences, Status, Text, ThemeMonitor, text},
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

#[derive(Clone, Copy)]
struct PendingPenDown {
    timestamp: u64,
    point: Point,
    eraser: bool,
}

#[derive(Default, Clone, Copy)]
struct Pen {
    pressure: f32,
    tilt: [f32; 2],
    has_pressure: bool,
    down: bool,
    pending_down: Option<PendingPenDown>,
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
        self.pending_down = None;
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
    font_path: Option<PathBuf>,
    reset_font: bool,
    font_reorder: Option<(usize, usize)>,
    font_remove: Option<String>,
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
    pen_double: stylus::PenDoubleTap,
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
    font_catalog: Option<Vec<crate::fonts::SystemFont>>,
    font_search: String,
    adding_font: bool,
    screenshot: Screenshot,
    #[cfg(target_os = "linux")]
    gnome_guide: Option<crate::gnome::Guide>,
    language: Language,
    suspended: bool,
    frozen: bool,
    panel: Option<crate::toolbar::Panel>,
    panel_retiring: Option<crate::toolbar::Panel>,
    touch_enabled: bool,
    status: Status,
    path: String,
    save_path: PathBuf,
    last_save: Instant,
}
impl App {
    fn mouse_mode_status() -> Status {
        text("Mouse mode. Click the desktop or select a drawing tool to resume.").into()
    }

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
        let mut status = Self::mouse_mode_status();
        if preferences.restore_session && save_path.exists() {
            match pages.load(&mut history, &save_path, true) {
                Ok(0) => {}
                Ok(pending) => status = pending_pages(pending),
                Err(error) => status = text("Session restore failed").detail(error),
            }
        }
        Self {
            input: CanvasInput::default(),
            pen_double: stylus::PenDoubleTap::default(),
            parked_input: BTreeMap::new(),
            multi: false,
            history,
            // Mouse mode is active on launch; remember Pen for Space to resume.
            tool: Tool::Pen,
            color: InkColor::default(),
            hex_entry: crate::color::HexEntry::new(InkColor::default()),
            picking: false,
            width: 5.,
            opacity: 1.,
            pressure: true,
            curve: 1.,
            pass: true,
            // No button was pressed to enter mouse mode during startup.
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
            font_catalog: None,
            font_search: String::new(),
            adding_font: false,
            screenshot: Screenshot::default(),
            #[cfg(target_os = "linux")]
            gnome_guide: None,
            suspended,
            frozen: false,
            panel: None,
            panel_retiring: None,
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
                EraseError::Samples => {
                    text("This erase would exceed the one million sample limit.")
                }
                EraseError::Preview => {
                    text("This erase reached the preview limit. Erase in shorter passes.")
                }
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
    /// SDL's Cocoa backend sends PEN_DOWN before PEN_MOTION for the same
    /// NSEvent, and the DOWN event carries the *previous* pen coordinates.
    /// Start from the following same-timestamp motion/axis instead. On other
    /// platforms the DOWN coordinates are authoritative.
    fn begin_pen(&mut self, id: u32, p: Point, eraser: bool, has_pressure: bool) {
        let source = Source::Pen(id);
        self.begin(source, p, eraser);
        let accepted = self
            .input
            .contact
            .is_some_and(|contact| contact.source == source);
        self.input.pens.entry(id).or_default().down = accepted;
        if let Some(contact) = &mut self.input.contact
            && contact.source == source
        {
            contact.measured_pressure = has_pressure;
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
        self.status = Self::mouse_mode_status();
        self.finish_all_contacts();
    }
    fn use_tool(&mut self, tool: Tool) {
        self.finish_all_contacts();
        self.tool = tool;
        self.pass = false;
        self.wait_for_mouse_up = false;
        self.hidden = false;
        if let Some(hint) = match tool {
            Tool::Text => Some(text("Click to add or edit text. Ctrl/Cmd+Enter finishes.")),
            Tool::Select => Some(text(
                "Drag to move. Double-click to edit. Delete removes the box.",
            )),
            _ => None,
        } {
            self.status = hint.into();
        }
    }
    fn switch_pen_eraser(&mut self) -> bool {
        if self.pass || self.picking || self.screenshot.active() {
            return false;
        }
        let next = match self.tool {
            Tool::Pen => Tool::Eraser,
            Tool::Eraser => Tool::Pen,
            _ => Tool::Pen,
        };
        self.use_tool(next);
        self.panel = None;
        true
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
        // Traversing ordered page IDs avoids allocating a temporary Vec on
        // every Sidecar pen event. on_page restores the key set on return.
        let mut next = self.parked_input.keys().next().copied();
        while let Some(id) = next {
            self.on_page(Some(id), &mut f);
            next = self
                .parked_input
                .range((std::ops::Bound::Excluded(id), std::ops::Bound::Unbounded))
                .next()
                .map(|(id, _)| *id);
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
            self.status =
                text("Display disconnected. Its ink and undo history were cleared.").into();
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
                    tool: stroke.tool,
                    color: stroke.color,
                    width: stroke.width,
                    opacity: stroke.opacity,
                    pressure_curve: stroke.pressure_curve,
                    pressure_enabled: stroke.pressure_enabled,
                    points,
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
            // Some touch/Sidecar backends reuse the same touch/finger IDs
            // without delivering the previous UP. A new DOWN is an explicit
            // stroke boundary: commit the stale stroke, never append to it.
            if let Some(previous) = self.input.touches.remove(&key) {
                self.commit_touch(previous);
            }
            if self
                .input
                .protected
                .is_some_and(|r| r.contains(point.x, point.y))
            {
                return;
            }
            if self.input.touches.len() >= 16
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
    // Release state is global to the physical pen, but proximity remains
    // local to the canvas. This helper is safe to call on all parked pages:
    // unlike track_pen, it does not mark an unrelated page as pen-proximate
    // or cancel the page's ongoing finger gestures.
    fn release_pen(&mut self, id: u32, definitive: bool) {
        let had_contact = self
            .input
            .contact
            .is_some_and(|contact| contact.source == Source::Pen(id));
        self.end(Source::Pen(id));
        if (definitive || had_contact)
            && let Some(pen) = self.input.pens.get_mut(&id)
        {
            pen.finish();
        }
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
            SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => pen_touch_event(event).map(|pen| pen.which.0),
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
        // Only an explicit pen-up ends contact here. Proximity-out already
        // releases and forgets this pen above.
        if ty == SDL_EVENT_PEN_UP {
            self.release_pen(id, true);
        }
        if self.input.pen_proximity.insert(id) {
            self.discard_touches();
        }
    }
    fn event(&mut self, e: &SDL_Event, overlay: SDL_WindowID, size: [f32; 2]) {
        let ty = event_type(e);
        // Accept the final PEN_UP endpoint before ending the pen contact.
        if ty != SDL_EVENT_PEN_UP {
            self.track_pen(e);
        }
        match ty {
            SDL_EVENT_MOUSE_BUTTON_DOWN
                if mouse_button_event(e).is_some_and(|button| button.windowID == overlay) =>
            {
                let Some(b) = mouse_button_event(e) else {
                    return;
                };
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
                if mouse_button_event(e).is_some_and(|button| {
                    button.windowID == overlay
                        && button.which != SDL_PEN_MOUSEID
                        && button.which != SDL_TOUCH_MOUSEID
                }) =>
            {
                let Some(b) = mouse_button_event(e) else {
                    return;
                };
                let source = Source::Mouse(b.button);
                if self.input.contact.is_some_and(|contact| {
                    contact.source == source && matches!(contact.gesture, Gesture::Erase(_))
                }) {
                    self.motion(source, Point::new(b.x, b.y, 1., [0.; 2]));
                }
                self.end(source);
            }
            SDL_EVENT_MOUSE_MOTION
                if mouse_motion_event(e).is_some_and(|motion| motion.windowID == overlay) =>
            {
                let Some(m) = mouse_motion_event(e) else {
                    return;
                };
                if m.which != SDL_PEN_MOUSEID
                    && m.which != SDL_TOUCH_MOUSEID
                    && let Some(contact) = self.input.contact
                    && matches!(contact.source, Source::Mouse(_))
                {
                    self.motion(contact.source, Point::new(m.x, m.y, 1., [0.; 2]));
                }
            }
            SDL_EVENT_FINGER_CANCELED => {
                if let Some(finger) = finger_event(e) {
                    self.touch_event(finger, size);
                }
            }
            SDL_EVENT_FINGER_DOWN | SDL_EVENT_FINGER_MOTION | SDL_EVENT_FINGER_UP
                if finger_event(e).is_some_and(|finger| finger.windowID == overlay) =>
            {
                if let Some(finger) = finger_event(e) {
                    self.touch_event(finger, size);
                }
            }
            SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP
                if pen_touch_event(e).is_some_and(|pen| pen.windowID == overlay) =>
            {
                let Some(t) = pen_touch_event(e) else {
                    return;
                };
                let pen = *self.input.pens.entry(t.which.0).or_default();
                let source = Source::Pen(t.which.0);
                let p = pen.point(t.x, t.y);
                if t.down {
                    if cfg!(target_os = "macos") && t.timestamp != 0 {
                        // In SDL 3.4.20 Cocoa_HandlePenPointEvent calls
                        // SDL_SendPenTouch before SDL_SendPenMotion, so the
                        // PEN_DOWN x/y are the previous cached pen position.
                        // Defer beginning until a matching same-timestamp
                        // PEN_MOTION or PEN_AXIS provides the actual position.
                        self.input.pens.entry(t.which.0).or_default().pending_down =
                            Some(PendingPenDown {
                                timestamp: t.timestamp,
                                point: p,
                                eraser: t.eraser,
                            });
                    } else {
                        self.begin_pen(t.which.0, p, t.eraser, pen.has_pressure);
                    }
                } else {
                    if let Some(pending) = self
                        .input
                        .pens
                        .get_mut(&t.which.0)
                        .and_then(|pen| pen.pending_down.take())
                    {
                        // A stationary tap can have no motion or changed axis.
                        // In that case the stored DOWN coordinate is valid.
                        self.begin_pen(t.which.0, pending.point, pending.eraser, pen.has_pressure);
                    }
                    self.pen_motion(source, p, pen.has_pressure);
                    self.end(source);
                    // The common PEN_UP reconciliation at the end of event()
                    // resets the cached pen exactly once.
                }
            }
            SDL_EVENT_PEN_AXIS if pen_axis_event(e).is_some_and(|pen| pen.windowID == overlay) => {
                let Some(a) = pen_axis_event(e) else {
                    return;
                };
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
                let pending = pen.pending_down.take();
                if let Some(pending) = pending {
                    // Cocoa generates motion then axis with the same
                    // timestamp. If motion didn't change the SDL pen position,
                    // the axis still carries the current true coordinates.
                    let starts_at = if pending.timestamp == a.timestamp {
                        point
                    } else {
                        pending.point
                    };
                    self.begin_pen(a.which.0, starts_at, pending.eraser, has_pressure);
                    if pending.timestamp != a.timestamp {
                        self.pen_motion(source, point, has_pressure);
                    }
                } else {
                    self.pen_motion(source, point, has_pressure);
                }
            }
            SDL_EVENT_PEN_MOTION
                if pen_motion_event(e).is_some_and(|pen| pen.windowID == overlay) =>
            {
                let Some(m) = pen_motion_event(e) else {
                    return;
                };
                let pen = self.input.pens.entry(m.which.0).or_default();
                let point = pen.point(m.x, m.y);
                let has_pressure = pen.has_pressure;
                let pending = pen.pending_down.take();
                let source = Source::Pen(m.which.0);
                if let Some(pending) = pending {
                    let same_cocoa_event = pending.timestamp == m.timestamp;
                    self.begin_pen(
                        m.which.0,
                        if same_cocoa_event {
                            point
                        } else {
                            pending.point
                        },
                        pending.eraser,
                        has_pressure,
                    );
                    if !same_cocoa_event {
                        self.pen_motion(source, point, has_pressure);
                    }
                } else {
                    self.pen_motion(source, point, has_pressure);
                }
            }
            SDL_EVENT_WINDOW_FOCUS_LOST if window_event_id(e) == Some(overlay) => {
                if !self.multi {
                    self.discard_touches();
                }
                self.finish_contact();
                self.reset_pens();
            }
            SDL_EVENT_WINDOW_MOUSE_LEAVE
                if window_event_id(e) == Some(overlay)
                    && self
                        .input
                        .contact
                        .is_some_and(|contact| matches!(contact.source, Source::Mouse(_))) =>
            {
                self.finish_contact();
            }
            _ => {}
        }
        if ty == SDL_EVENT_PEN_UP {
            self.track_pen(e);
        }
    }
    fn keyboard(&mut self, e: &SDL_Event, toolbar_keyboard: bool) -> Commands {
        let mut c = Commands::default();
        if event_type(e) != SDL_EVENT_KEY_DOWN {
            return c;
        }
        let Some(k) = key_event(e) else {
            return c;
        };
        if k.repeat {
            return c;
        }
        let m = gui::modifiers(k.r#mod);
        if toolbar_keyboard || self.input.text.editing() {
            return c;
        }
        if self.tool.is_text() && !self.pass && !self.hidden && !m.command && self.text_key(k.key) {
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
        c
    }
    fn save(&mut self) {
        self.finish_all_contacts();
        let p = PathBuf::from(&self.path);
        let result = if p == self.save_path {
            Err(text("Choose a different file for manual saves.")
                .get(self.language)
                .into())
        } else {
            self.pages.save(&self.history, &p)
        };
        match result {
            Ok(()) => self.status = text("Saved").detail(p.display()),
            Err(e) => self.status = text("Save failed").detail(e),
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
            Ok(0) => self.status = text("Session loaded").into(),
            Ok(pending) => self.status = pending_pages(pending),
            Err(e) => self.status = text("Open failed").detail(e),
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
            Ok(()) => self.status = text("Exported").detail(p.display()),
            Err(e) => self.status = text("Export failed").detail(e),
        }
    }

    fn install_annotation_font(&mut self, source: &Path, destination: &Path) -> bool {
        match self.text_fonts.install(source, destination) {
            Ok(()) => {
                self.status = text("Annotation font updated").into();
                true
            }
            Err(error) => {
                self.status = text("Could not use the annotation font").detail(error);
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
    text("Saved pages awaiting a matching display").detail(count)
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

// Optional, five-second counters for locating event storms and idle redraws
// without adding a profiler dependency or logging on every frame.
#[derive(Default)]
struct PerformanceCounts {
    iterations: u64,
    events: u64,
    window_events: u64,
    motion_events: u64,
    toolbar_frames: u64,
    canvas_frames: u64,
    toolbar_ui_time: Duration,
    toolbar_present_time: Duration,
    canvas_present_time: Duration,
    precise_waits: u64,
    frame_sleeps: u64,
    event_waits: u64,
}

struct PerformanceMonitor {
    started: Instant,
    counts: PerformanceCounts,
}

impl PerformanceMonitor {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            counts: PerformanceCounts::default(),
        }
    }

    fn event(&mut self, ty: SDL_EventType) {
        self.counts.events += 1;
        if (SDL_EVENT_WINDOW_FIRST..=SDL_EVENT_WINDOW_LAST).contains(&ty) {
            self.counts.window_events += 1;
        } else if matches!(
            ty,
            SDL_EVENT_MOUSE_MOTION | SDL_EVENT_PEN_MOTION | SDL_EVENT_FINGER_MOTION
        ) {
            self.counts.motion_events += 1;
        }
    }

    fn report(&mut self) {
        if self.started.elapsed() < Duration::from_secs(5) {
            return;
        }
        let c = &self.counts;
        eprintln!(
            "OverInk perf/5s: loops={} events={} window={} motion={} toolbar={} canvas={} toolbar_ui_ms={:.1} toolbar_present_ms={:.1} canvas_present_ms={:.1} precise_wait={} frame_sleep={} event_wait={}",
            c.iterations,
            c.events,
            c.window_events,
            c.motion_events,
            c.toolbar_frames,
            c.canvas_frames,
            c.toolbar_ui_time.as_secs_f64() * 1000.,
            c.toolbar_present_time.as_secs_f64() * 1000.,
            c.canvas_present_time.as_secs_f64() * 1000.,
            c.precise_waits,
            c.frame_sleeps,
            c.event_waits,
        );
        self.started = Instant::now();
        self.counts = PerformanceCounts::default();
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
            && !(ty == SDL_EVENT_MOUSE_MOTION
                && !canvas.pending_screenshot_input()
                && screenshot.ignores_hover_motion(event, mouse_released()))
        {
            canvas.dirty |= canvas.screenshot_event(event);
        }
    }
    false
}

fn update_preferences(app: &mut App, previous: Preferences, multi_supported: bool) -> bool {
    if let Err(error) = app.preferences.save(&state_dir().join("ui.json")) {
        app.preferences = previous;
        app.status = text("Settings save failed").detail(error);
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
            app.status = text("Last session update failed").detail(error);
        }
    }
    if previous.wayland != app.preferences.wayland {
        app.status = text("Restart OverInk to switch the display backend.").into();
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
        previous_wayland: bool,
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
            && !previous_wayland
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

/// Drains temporary Objective-C objects at the end of each scope on macOS.
fn with_autorelease_pool<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "macos")]
    {
        objc2::rc::autoreleasepool(|_| f())
    }
    #[cfg(not(target_os = "macos"))]
    {
        f()
    }
}

pub fn run(options: Options) -> Result<(), String> {
    // Keep an outer pool alive through native shutdown, including early errors.
    with_autorelease_pool(|| run_inner(options))
}

fn run_inner(options: Options) -> Result<(), String> {
    let base = state_dir();
    let preferences_path = base.join("ui.json");
    let preferences = Preferences::load(&preferences_path);
    #[cfg(target_os = "linux")]
    let (_runtime, preferences, gnome_status) =
        crate::gnome::start_video(preferences, &preferences_path, options.x11, options.smoke)?;
    #[cfg(not(target_os = "linux"))]
    let _runtime =
        with_autorelease_pool(|| Runtime::new(options.x11, preferences.wayland, options.smoke))?;
    let video = crate::sdl::driver();
    let wayland = video == "wayland";
    let window =
        with_autorelease_pool(|| Window::new("OverInk - canvas", 1100, 720, !options.smoke))?;
    let topology = crate::display::Topology::new(&video);
    let displays = with_autorelease_pool(|| topology.connected(&window)).unwrap_or_default();
    let xwayland = crate::background::is_xwayland(&window, &video);
    let mut app = with_autorelease_pool(|| {
        App::new(
            options.board || options.smoke,
            options.display,
            displays,
            &base,
            preferences,
        )
    });
    #[cfg(target_os = "linux")]
    if let Some(status) = gnome_status {
        app.status = status;
    }
    app.multi = app.preferences.per_display_windows && !wayland && !options.smoke;
    let mut canvases = vec![with_autorelease_pool(|| {
        canvas::Canvas::new(window, app.displays.get(app.display), &options)
    })?];
    let toolbar = with_autorelease_pool(|| {
        Window::toolbar(
            "OverInk",
            crate::toolbar::WIDTH as i32,
            crate::toolbar::HEIGHT as i32,
        )
    })?;
    with_autorelease_pool(|| toolbar.setup_toolbar());
    // With NSIsTouchNative set, AppKit no longer synthesizes mouse clicks for
    // direct touches. The separate toolbar is an SDL window too, so it needs
    // the same direct-touch bridge as every canvas.
    #[cfg(target_os = "macos")]
    let _toolbar_direct_touch = if options.smoke {
        None
    } else {
        match crate::native::macos::DirectTouchMonitor::install(&toolbar) {
            Ok(monitor) => monitor,
            Err(error) => {
                eprintln!("OverInk: toolbar direct touch unavailable: {error}");
                None
            }
        }
    };
    #[cfg(target_os = "macos")]
    let pencil_mode = match crate::native::macos::PencilModeMonitor::install() {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            eprintln!("OverInk: Sidecar Apple Pencil double-tap unavailable: {error}");
            None
        }
    };
    if !options.smoke {
        let _ = with_autorelease_pool(|| toolbar.arrange_toolbar(false, app.pages.active));
        with_autorelease_pool(|| canvas::reconcile(&mut canvases, &mut app, &toolbar, &options))?;
    }
    let toolbar_render = with_autorelease_pool(|| Renderer::new(&toolbar, true))
        .map_err(|error| format!("Creating the toolbar renderer on {video} failed ({error})"))?;
    let mut gui = with_autorelease_pool(gui::Gui::new);
    let mut mark = with_autorelease_pool(|| crate::toolbar::brand(&gui.ctx));
    let mut toolbar_color = ColorManager::default();
    with_autorelease_pool(|| toolbar_color.refresh(&toolbar, toolbar_render.extended));
    for canvas in &mut canvases {
        with_autorelease_pool(|| canvas.show(&toolbar, options.smoke || !app.suspended))?;
    }
    with_autorelease_pool(|| toolbar.show(true))
        .map_err(|error| format!("Showing the toolbar on {video} failed ({error})"))?;
    if !options.smoke && !wayland {
        let _ = with_autorelease_pool(|| toolbar.arrange_toolbar(false, app.pages.active));
    }
    with_autorelease_pool(|| toolbar.raise());
    if options.smoke {
        app.history = History::default();
        with_autorelease_pool(|| smoke_text_history(&mut app, &mut canvases[0]))?;
        with_autorelease_pool(|| smoke_events(&mut app, canvases[0].window.id))?;
    }
    let mut picker = crate::picker::Picker::default();
    let mut file_dialog = FileDialog::default();
    let annotation_font_path = base.join("annotation-font");
    let mut tooltips = crate::toolbar::Tooltips::default();
    let mut toolbar_hidden = false;
    let mut toolbar_expanded = false;
    let mut toolbar_shape_size = None;
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
    let mut performance = std::env::var_os("OVERINK_PROFILE").map(|_| PerformanceMonitor::new());
    while running {
        with_autorelease_pool(|| -> Result<(), String> {
            if let Some(monitor) = &mut performance {
                monitor.counts.iterations += 1;
            }
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
                if let Some(monitor) = &mut performance {
                    monitor.event(ty);
                }
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
                    if ty == SDL_EVENT_DISPLAY_REMOVED
                        && let Some(display) = removed_display_id(&event)
                    {
                        disconnected.insert(display);
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
                        // Replacing Gui drops its owned atlas textures while
                        // the original SDL renderer is still alive.
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
                        let cancelled =
                            handle_screenshot_event(&mut app, &event, ty, &mut canvases);
                        mode_changed |= cancelled;
                        restore_toolbar_focus |= cancelled;
                    }
                    continue;
                }

                let id = canvas::event_window(&event, ty);
                let active = canvas::active_index(&canvases, app.pages.active);
                let was_picking = picker.active();
                picker.event(&event, &canvases[active].window, &toolbar);
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
                if drawing
                    && ty == SDL_EVENT_PEN_BUTTON_DOWN
                    && !app.pass
                    && pen_button_event(&event).is_some_and(|button| {
                        app.pen_double
                            .pressed(button.which.0, button.button, button.timestamp)
                    })
                    && app.switch_pen_eraser()
                {
                    mode_changed = true;
                    toolbar_dirty = true;
                    for canvas in &mut canvases {
                        canvas.dirty = true;
                    }
                    continue;
                }
                if !file_dialog.pending() {
                    gui.event(&event, toolbar.id, toolbar.size());
                }
                if id == Some(toolbar.id)
                    && ty == SDL_EVENT_WINDOW_FOCUS_LOST
                    && !toolbar.input_focused()
                {
                    gui.sync_text_input(&toolbar, false);
                }
                if ty == SDL_EVENT_WINDOW_FOCUS_LOST {
                    app.pen_double.clear();
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
                                button.which != SDL_TOUCH_MOUSEID && button.which != SDL_PEN_MOUSEID
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
                    let toolbar_claims_keys =
                        from_toolbar
                            && (toolbar_text_input
                                || egui::Popup::is_any_open(&gui.ctx)
                                || gui.raw.events.iter().any(|event| {
                                    matches!(event, egui::Event::PointerButton { .. })
                                }));
                    let key = key_event(&event).ok_or("SDL keyboard event has no keyboard data")?;
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
                if ty == SDL_EVENT_PEN_UP
                    && let Some(pen) = pen_touch_event(&event)
                {
                    // The normal target already consumed its release endpoint.
                    // Commit orphaned contacts without changing other pages'
                    // pen proximity or invalidating their touch gestures.
                    app.for_each_page(|page| page.release_pen(pen.which.0, true));
                }
            }
            #[cfg(target_os = "macos")]
            if let Some(monitor) = &pencil_mode {
                let taps = monitor.take();
                // AppKit callbacks do not mutate App inside the Cocoa pump.
                // Apply gestures only after SDL's event batch, outside modal
                // pickers and native dialogs.
                if taps > 0
                    && !file_dialog.pending()
                    && !picker.active()
                    && !toolbar_hidden
                    && !app.suspended
                    && !app.picking
                    && !app.screenshot.active()
                {
                    for _ in 0..taps.min(16) {
                        if app.switch_pen_eraser() {
                            mode_changed = true;
                            toolbar_dirty = true;
                        }
                    }
                    if mode_changed {
                        for canvas in &mut canvases {
                            canvas.dirty = true;
                        }
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
                return Ok(());
            }

            if let Some((kind, result)) = file_dialog.poll() {
                toolbar_dirty = true;
                restore_toolbar_focus = !app.screenshot.active();
                match kind {
                    DialogKind::ScreenshotFolder => match result {
                        Ok(Some(path)) => {
                            let previous = app.preferences.screenshot_folder.replace(path);
                            if let Err(error) = app.preferences.save(&state_dir().join("ui.json")) {
                                app.preferences.screenshot_folder = previous;
                                app.status = text("Settings save failed").detail(error);
                            }
                        }
                        Ok(None) => (),
                        Err(error) => {
                            app.status = text("Could not choose a screenshot folder").detail(error);
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
                                    if let Err(error) = app.screenshot.save(Destination::File(path))
                                    {
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
                        app.status = text("Display query failed").detail(error);
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
                    app.status = text("Toolbar layout failed").detail(error);
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
                    if let Err(error) =
                        canvas::reconcile(&mut canvases, &mut app, &toolbar, &options)
                    {
                        app.status = canvas::failure(error);
                        toolbar_dirty = true;
                    }
                }
                if wayland && requested.is_some_and(|id| Some(id) != reported) {
                    app.status =
                        text("The Wayland compositor controls window order and placement.").into();
                    toolbar_dirty = true;
                }
                if removed
                    && app.preferences.restore_session
                    && app.pages.needs_save(&app.history)
                    && let Err(error) = app.save_session()
                {
                    app.status = text("Save after display removal failed").detail(error);
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
                            app.status = text("Screenshot failed").detail(error);
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
                        app.status = text("Screenshot saved").detail(path.display());
                        toolbar_dirty = true;
                        restore_toolbar_focus = true;
                        mode_changed = true;
                    }
                    Err(SaveError::Exists(path)) if app.screenshot.ready() => {
                        app.screenshot.confirm_replace(path);
                    }
                    Err(SaveError::Exists(path)) => {
                        app.status = text("Screenshot not saved; file already exists")
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
                        app.status = text("Screenshot save failed").detail(error);
                        toolbar_dirty = true;
                    }
                }
            }
            let hide = picker.hides_windows()
                || canvases.iter().any(|c| c.background.pending())
                || app.screenshot.portal_fallback_requested();
            toolbar_dirty |= capture_visibility(
                &mut canvases,
                &toolbar,
                &mut tooltips,
                &mut toolbar_hidden,
                hide,
                app.screenshot.active(),
            )?;
            app.screenshot.acknowledge_portal_fallback_hidden();
            let mut restore_text_focus = false;
            let pick_display = canvases[active]
                .output
                .and_then(|id| app.displays.iter().find(|display| display.id == id));
            if let Some(result) = picker.poll(&canvases[active].window, pick_display, &app.displays)
            {
                app.picking = false;
                let origin = text_pick.take();
                let canvas = &canvases[active];
                let on_text = origin.as_ref().is_some_and(|(window, output, target)| {
                    *window == canvas.window.id
                        && *output == canvas.output
                        && app.matches_text_pick_target(target)
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
                            text("Picked").detail(color.hex())
                        } else {
                            text("Color selection cancelled").into()
                        }
                    }
                    Ok(None) => text("Color selection cancelled").into(),
                    Err(error) => text("Screen color picker failed").detail(error),
                };
            }
            for canvas in &mut canvases {
                if let Some(result) = canvas.background.poll(&canvas.window) {
                    canvas.dirty = true;
                    toolbar_dirty = true;
                    app.status = match result {
                        Ok(()) => {
                            text("Desktop frozen. Select Live desktop to resume the background.")
                                .into()
                        }
                        Err(error) => text("Showing the live desktop").detail(error),
                    };
                }
            }
            let hide = picker.hides_windows()
                || canvases.iter().any(|c| c.background.pending())
                || app.screenshot.portal_fallback_requested();
            toolbar_dirty |= capture_visibility(
                &mut canvases,
                &toolbar,
                &mut tooltips,
                &mut toolbar_hidden,
                hide,
                app.screenshot.active(),
            )?;
            app.screenshot.acknowledge_portal_fallback_hidden();
            if hide {
                wait_for_events(Duration::from_millis(8));
                return Ok(());
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
            let expanded = (app.panel.is_some() || app.panel_retiring.is_some()) && !app.picking;
            if expanded != toolbar_expanded || controller_scale_changed {
                toolbar_dirty = true;
                controller_bounds_changed = true;
                toolbar_expanded = expanded;
                if let Err(error) = toolbar.arrange_toolbar(expanded, None) {
                    app.status = text("Toolbar layout failed").detail(error);
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
            // Capturing the whole preferences object every event-loop tick
            // copies the optional screenshot path even when the toolbar is
            // idle. Only UI processing can mutate settings this iteration.
            #[cfg(target_os = "linux")]
            let previous_wayland = app.preferences.wayland;
            let mut previous_preferences = None;
            let was_board = app.board;
            app.frozen = canvases[active].background.frozen;
            let mut cmd = Commands::default();
            let mut output = None;
            if !toolbar_hidden
                && (toolbar_dirty
                    || !gui.raw.events.is_empty()
                    || toolbar_repaint_at.is_some_and(|at| Instant::now() >= at))
            {
                previous_preferences = Some(app.preferences.clone());
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
                let ui_started = performance.as_ref().map(|_| Instant::now());
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
                if let (Some(start), Some(monitor)) = (ui_started, &mut performance) {
                    monitor.counts.toolbar_ui_time += start.elapsed();
                }
                toolbar_text_input = frame.platform_output.ime.is_some();
                toolbar_repaint_at = gui::repaint_at(&frame);
                output = Some(frame);
            }
            if cmd.quit {
                running = false;
                return Ok(());
            }
            mode_changed |= cmd.mode;
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
            if cmd.screenshot
                && !file_dialog.pending()
                && !picker.active()
                && !app.screenshot.saving()
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
            if let Some((from, to)) = cmd.font_reorder {
                match app.text_fonts.move_font(from, to, &annotation_font_path) {
                    Ok(()) => cmd.redraw_canvas = true,
                    Err(error) => {
                        app.status = text("Could not reorder fonts").detail(error);
                    }
                }
                toolbar_dirty = true;
            }
            if let Some(id) = cmd.font_remove.take() {
                match app.text_fonts.remove_font(&id, &annotation_font_path) {
                    Ok(()) => cmd.redraw_canvas = true,
                    Err(error) => {
                        app.status = text("Could not remove font").detail(error);
                    }
                }
                toolbar_dirty = true;
            }
            if cmd.reset_font {
                match app.text_fonts.reset(&annotation_font_path) {
                    Ok(()) => {
                        app.status = text("Using the included annotation font").into();
                        cmd.redraw_canvas = true;
                    }
                    Err(error) => {
                        app.status = text("Could not reset the annotation font").detail(error)
                    }
                }
                toolbar_dirty = true;
            }
            #[cfg(target_os = "linux")]
            let relogin = app.update_gnome_guide(&mut cmd, previous_wayland, options.smoke);
            if cmd.preferences {
                toolbar_dirty = true;
                if let Some(previous_preferences) = previous_preferences
                    && app.preferences != previous_preferences
                {
                    topology_dirty |= update_preferences(
                        &mut app,
                        previous_preferences,
                        !wayland && !options.smoke,
                    );
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
                    app.status = text("Frame-rate setting save failed").detail(error);
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
            let hide = picker.hides_windows()
                || canvases.iter().any(|c| c.background.pending())
                || app.screenshot.portal_fallback_requested();
            toolbar_dirty |= capture_visibility(
                &mut canvases,
                &toolbar,
                &mut tooltips,
                &mut toolbar_hidden,
                hide,
                screenshot_active,
            )?;
            app.screenshot.acknowledge_portal_fallback_hidden();
            if toolbar_hidden && let Some(mut output) = output.take() {
                output.shapes.clear();
                output.platform_output.ime = None;
                gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
            }
            if hide {
                wait_for_events(Duration::from_millis(8));
                return Ok(());
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
                // A separately hosted on-screen keyboard may own the OS
                // focus while still typing into this canvas. The ordinary
                // toolbar-to-canvas 150ms timeout must not finish its editor.
                // An actual toolbar text field still takes precedence.
                if app.input.text.screen_keyboard_session() && !toolbar_text_input {
                    text_focus.deadline = None;
                } else {
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
                let toolbar_started = performance.as_ref().map(|_| Instant::now());
                presented = true;
                if !toolbar_ime_active {
                    output.platform_output.ime = None;
                }
                if let Err(error) = toolbar.update_rounded_shape(&mut toolbar_shape_size) {
                    // Some compositors lack native shape masks. Keep a visible
                    // rounded, alpha-composited UI rather than terminating the
                    // entire application; retry after a real size/DPI change.
                    eprintln!("OverInk: native toolbar shape unavailable: {error}");
                    toolbar_shape_size = Some(toolbar.pixel_size());
                }
                toolbar_render.begin(&toolbar, [0.; 4])?;
                gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
                toolbar_render.present()?;
                if let Some(monitor) = &mut performance {
                    monitor.counts.toolbar_frames += 1;
                    if let Some(start) = toolbar_started {
                        monitor.counts.toolbar_present_time += start.elapsed();
                    }
                }
                tooltips.update(&toolbar, &gui, app.language, &toolbar_color);
            }
            if !follow_controller {
                toolbar.sync_and_pump_events();
                controller_display = toolbar.display_id();
                follow_controller = true;
            }
            let keep_editor_open = restore_text_focus
                || toolbar.input_focused()
                || text_focus.deadline.is_some()
                || app.input.text.screen_keyboard_session();
            let mouse = !picker.choosing() && app.mouse_ready();
            let mut unsafe_layer = false;
            let mut extended_ink_bytes: usize = canvases
                .iter()
                .map(canvas::Canvas::extended_ink_bytes)
                .sum();
            for (index, canvas) in canvases.iter_mut().enumerate() {
                let screenshot = app.screenshot.targets(canvas.window.id);
                let mouse = mouse && !screenshot;
                // The native chooser owns interaction while pending. Even a
                // screenshot canvas kept as a Wayland transient parent would
                // remain fullscreen and always-on-top behind the chooser.
                let visible = if file_dialog.pending() || picker.hides_windows() {
                    // Native macOS and Wayland portal samplers must see the
                    // desktop beneath OverInk, not its transparent ink canvas.
                    // Keep only the movable Cancel/Quit controller visible.
                    false
                } else {
                    screenshot
                        || (options.smoke
                            || !app.hidden
                                && !app.suspended
                                && canvas.output.is_some_and(|id| connected.contains(&id)))
                            && (app.multi || index == active)
                            && (!picker.choosing() || index == active)
                };
                let raise_for_screenshot =
                    visible && screenshot && (cmd.screenshot || !canvas.visible);
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
                    let canvas_started = performance.as_ref().map(|_| Instant::now());
                    let painted = canvas.paint(
                        &mut app,
                        &options,
                        picker.choosing(),
                        keep_editor_open,
                        theme,
                        ink::MAX_EXTENDED_INK_TEXTURE_BYTES.saturating_sub(extended_ink_bytes),
                    )?;
                    presented |= painted;
                    if painted && let Some(monitor) = &mut performance {
                        monitor.counts.canvas_frames += 1;
                        if let Some(start) = canvas_started {
                            monitor.counts.canvas_present_time += start.elapsed();
                        }
                    }
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
            } else if mode_changed || controller_bounds_changed || picker.choosing() {
                let toolbar_display = toolbar.display_id();
                if let Some(canvas) = canvases
                    .iter()
                    .find(|canvas| canvas.visible && (wayland || canvas.output == toolbar_display))
                {
                    canvas.keep_toolbar_above(&toolbar);
                }
            }
            // Only a direct-finger re-tap or the annotation keyboard button
            // requests the OSK. Mouse/pen focus still allows normal hardware
            // typing and IME composition without opening the keyboard.
            if app.input.text.take_keyboard_request()
                && !options.smoke
                && !app.picking
                && !app.screenshot.active()
                && !file_dialog.pending()
                && app.tool.is_text()
                && !app.pass
                && app.input.text.editing()
            {
                let canvas = &mut canvases[active];
                gui.sync_text_input(&toolbar, false);
                canvas.focus_text_editor();
                canvas.keep_toolbar_above(&toolbar);
                text_focus.deadline = None;
                if app.preferences.keyboard_source == KeyboardSource::System
                    && crate::keyboard::system_supported(&video)
                {
                    // Respect an explicit Windows system-keyboard preference:
                    // never leave an old OverInk overlay open behind TabTip.
                    app.input.text.close_osk();
                    match crate::keyboard::show(&canvas.window, &video) {
                        Ok(()) => app.input.text.mark_screen_keyboard_open(),
                        Err(error) => {
                            #[cfg(not(target_os = "windows"))]
                            app.input.text.open_osk();
                            // Windows already tried InputPane, TabTip and
                            // osk.exe. Report failure rather than silently
                            // substituting the keyboard the user opted out of.
                            #[cfg(target_os = "windows")]
                            let message = text("Windows system keyboard unavailable");
                            #[cfg(not(target_os = "windows"))]
                            let message = text("Using OverInk keyboard");
                            app.status = message.detail(error);
                        }
                    }
                } else {
                    // macOS and X11 have no reliable universal OSK request.
                    // This egui overlay cannot steal the native editor focus.
                    app.input.text.open_osk();
                }
                canvas.dirty = true;
                toolbar_dirty = true;
            }
            if app.input.text.take_input_source_request() {
                if let Err(error) = crate::keyboard::switch_input_method() {
                    app.status = text("Could not switch input method").detail(error);
                }
                toolbar_dirty = true;
            }
            // Consume requests even when the picker cannot start, so an old
            // click does not unexpectedly launch a picker after switching modes.
            if app.input.text.take_screen_pick()
                && !app.picking
                && !app.screenshot.active()
                && !file_dialog.pending()
                && let Some(target) = app.text_pick_target()
            {
                app.input.text.refocus();
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
                            if let Err(error) =
                                app.screenshot.save(Destination::Folder(path.into_owned()))
                            {
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
                // Hide the fullscreen selection *before* opening the native
                // chooser, even on Linux/FreeBSD. Wayland transient parents
                // cannot be hidden without also affecting a modal child, so
                // screenshot-save choosers are deliberately unparented.
                for canvas in &mut canvases {
                    canvas.show(&toolbar, false)?;
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
                    app.status = text("Autosave failed").detail(error);
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
                if app.has_contacts() || !app.input.laser.is_empty() {
                    // Preserve precise pacing only during active drawing or
                    // laser animation, where its latency can be perceptible.
                    if let Some(monitor) = &mut performance {
                        monitor.counts.precise_waits += 1;
                    }
                    pacer.wait();
                } else {
                    // The event queue can wake SDL_WaitEventTimeout on every
                    // mouse-motion packet. That bypasses frame pacing and
                    // can present hundreds of full-canvas frames per second.
                    // A scheduler sleep caps ordinary UI refresh to the
                    // selected frame rate without SDL_DelayPrecise spinning.
                    if let Some(monitor) = &mut performance {
                        monitor.counts.frame_sleeps += 1;
                    }
                    std::thread::sleep(pacer.delay(Instant::now()));
                }
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
                if let Some(monitor) = &mut performance {
                    monitor.counts.event_waits += 1;
                }
                wait_for_events(timeout);
            }
            if let Some(monitor) = &mut performance {
                monitor.report();
            }
            Ok(())
        })?;
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
            pen_double: stylus::PenDoubleTap::default(),
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
            font_catalog: None,
            font_search: String::new(),
            adding_font: false,
            screenshot: Screenshot::default(),
            #[cfg(target_os = "linux")]
            gnome_guide: None,
            language: Language::English,
            suspended: false,
            frozen: false,
            panel: None,
            panel_retiring: None,
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
        assert_eq!(strokes[2].points.last().map(|point| point.x), Some(700.));
        for (left, right, x) in [
            (&strokes[0], &strokes[1], 80.),
            (&strokes[1], &strokes[2], 480.),
        ] {
            assert!(left.points.last().is_some_and(|point| point.x < x) && right.points[0].x > x);
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
                    pen_state: SDL_PEN_INPUT_DOWN,
                    ..Default::default()
                },
            },
            id,
            size,
        );
        let Some(active) = a.history.active.as_ref() else {
            panic!("Expected an active stroke after the pen-axis event");
        };
        let points = &active.points;
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
            assert_eq!(
                a.history.strokes()[0].points.last().map(|point| point.x),
                Some(65.)
            );

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
        let Some(trail) = a.input.laser.values().next() else {
            panic!("Expected the fading laser trail");
        };
        assert_eq!(trail.len(), 4);
        assert!(trail[0].starts_segment && !trail[1].starts_segment);
        assert!(trail[2].starts_segment && !trail[3].starts_segment);
        assert!(trail[1].point.x < 100. && trail[2].point.x > 300.);
    }
    #[test]
    fn startup_uses_mouse_mode_and_space_restores_the_pen() -> Result<(), Box<dyn std::error::Error>>
    {
        let state = tempfile::tempdir()?;
        let mut app = App::new(
            false,
            0,
            vec![screen(10, 0, 60.)],
            state.path(),
            Preferences {
                restore_session: false,
                ..Preferences::default()
            },
        );
        assert!(app.pass);
        assert!(!app.wait_for_mouse_up);
        assert!(app.mouse_ready());
        assert_eq!(app.tool, Tool::Pen);
        assert!(!app.hidden);

        let space = SDL_Event {
            key: SDL_KeyboardEvent {
                r#type: SDL_EVENT_KEY_DOWN,
                key: SDLK_SPACE,
                ..Default::default()
            },
        };
        assert!(app.keyboard(&space, false).mode);
        assert!(!app.pass);
        assert_eq!(app.tool, Tool::Pen);
        assert!(app.keyboard(&space, false).mode);
        assert!(app.pass);
        Ok(())
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
    fn disabling_session_recovery_discards_the_cache_and_keeps_manual_saves()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = std::env::temp_dir().join(format!(
            "overink-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
        ));
        let displays = vec![screen(10, 0, 60.0), screen(20, 1920, 120.0)];
        let mut a = App::new(true, 0, displays.clone(), &state, Preferences::default());
        a.use_tool(Tool::Pen);
        add_ink(&mut a, &[120.]);
        a.select_page(20);
        add_ink(&mut a, &[440.]);
        a.save();
        a.finish_session()?;
        assert!(a.save_path.is_file());
        assert!(Path::new(&a.path).is_file());

        let resumed = App::new(true, 1, displays.clone(), &state, Preferences::default());
        assert_eq!(resumed.history.strokes()[0].points[0].x, 440.);
        Preferences {
            restore_session: false,
            ..Preferences::default()
        }
        .save(&state.join("ui.json"))?;
        let mut private = App::new(
            true,
            1,
            displays,
            &state,
            Preferences::load(&state.join("ui.json")),
        );
        assert!(private.history.strokes().is_empty());
        private.use_tool(Tool::Pen);
        add_ink(&mut private, &[640.]);
        private.finish_session()?;
        assert!(!private.save_path.exists());
        assert!(Path::new(&private.path).is_file());
        private.load();
        assert_eq!(private.history.strokes()[0].points[0].x, 440.);
        std::fs::remove_dir_all(state)?;
        Ok(())
    }
    #[test]
    fn svg_export_creates_the_default_directory_on_first_launch()
    -> Result<(), Box<dyn std::error::Error>> {
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
        app.use_tool(Tool::Pen);
        add_ink(&mut app, &[20., 40.]);
        app.export();
        let svg = std::fs::read_to_string(Path::new(&app.path).with_extension("svg"))?;
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("<path"));
        std::fs::remove_dir_all(state)?;
        Ok(())
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
