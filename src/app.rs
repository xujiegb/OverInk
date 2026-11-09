mod panel;

use crate::{
    color::{InkColor, PRESETS},
    display::{Display, Pages},
    export,
    frame_rate::{FramePacer, FrameRate},
    geometry, gui,
    icc::ColorManager,
    model::{History, MAX_POINTS, Point, Stroke, Tool},
    native::Passthrough,
    sdl::*,
    touch::{self, TouchKey},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::PathBuf,
    ptr,
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
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Mouse(u8),
    Pen(u32),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Ink,
    Erase,
    Laser(u64),
}
#[derive(Clone, Copy)]
struct Contact {
    source: Source,
    gesture: Gesture,
}
struct FingerStroke {
    gesture: Gesture,
    stroke: Option<Stroke>,
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
}
struct App {
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
    board: bool,
    display: usize,
    displays: Vec<Display>,
    pages: Pages,
    frame_rate: FrameRate,
    suspended: bool,
    frozen: bool,
    protected: Option<crate::controller::Rect>,
    panel: Option<crate::toolbar::Panel>,
    pens: HashMap<u32, Pen>,
    pen_proximity: BTreeSet<u32>,
    touches: BTreeMap<TouchKey, FingerStroke>,
    touch_enabled: bool,
    contact: Option<Contact>,
    laser: BTreeMap<u64, VecDeque<(Point, Instant)>>,
    next_laser: u64,
    status: String,
    path: String,
    save_path: PathBuf,
    canvas_units: f32,
    last_save: Instant,
}
impl App {
    fn new(board: bool, display: usize, displays: Vec<Display>) -> Self {
        let base = state_dir();
        let save_path = base.join("session.overink.json");
        let mut history = History::default();
        let display = display.min(displays.len().saturating_sub(1));
        let suspended = displays.is_empty();
        let mut pages = Pages::new(displays.get(display).map(|d| d.id));
        pages.sync(&mut history, &displays);
        let mut status = "Ready. Draw on the canvas. Space switches to passthrough.".to_string();
        if save_path.exists()
            && let Err(e) = pages.load(&mut history, &save_path, true)
        {
            status = format!("Restore: {e}")
        }
        Self {
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
            board,
            display,
            displays,
            pages,
            frame_rate: FrameRate::load(&base.join("frame-rate.json")),
            suspended,
            frozen: false,
            protected: None,
            panel: None,
            pens: HashMap::new(),
            pen_proximity: BTreeSet::new(),
            touches: BTreeMap::new(),
            touch_enabled: true,
            contact: None,
            laser: BTreeMap::new(),
            next_laser: 0,
            status,
            path: save_path.to_string_lossy().into_owned(),
            save_path,
            canvas_units: 1.,
            last_save: Instant::now(),
        }
    }
    fn stroke(&self, tool: Tool) -> Stroke {
        Stroke {
            tool,
            color: self.color,
            width: if tool == Tool::Marker {
                self.width * 3. * self.canvas_units
            } else {
                self.width * self.canvas_units
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
            Tool::Eraser => Gesture::Erase,
            Tool::Laser => {
                self.next_laser += 1;
                Gesture::Laser(self.next_laser)
            }
            _ => Gesture::Ink,
        }
    }
    fn laser_sample(&mut self, id: u64, point: Point) {
        let trail = self.laser.entry(id).or_default();
        let now = Instant::now();
        if let Some((last, time)) = trail.back_mut()
            && last.dist(point) <= 0.05
        {
            *time = now;
            return;
        }
        if trail.len() >= MAX_LASER_SAMPLES {
            trail.pop_front();
        }
        trail.push_back((point, now));
    }
    fn begin(&mut self, source: Source, p: Point, eraser: bool) {
        if self.pass
            || self.hidden
            || self.suspended
            || self.protected.is_some_and(|r| r.contains(p.x, p.y))
        {
            return;
        }
        self.finish_contact();
        let tool = if eraser { Tool::Eraser } else { self.tool };
        let gesture = self.gesture(tool);
        match gesture {
            Gesture::Erase => self
                .history
                .erase(p, self.width.max(12.) * self.canvas_units),
            Gesture::Laser(id) => self.laser_sample(id, p),
            Gesture::Ink => {
                if self.draft_point_count() >= MAX_POINTS {
                    self.status =
                        "One million sample limit reached. Export and clear the canvas.".into();
                    return;
                }
                let mut stroke = self.stroke(tool);
                stroke.points.push(p);
                self.history.active = Some(stroke);
            }
        }
        self.contact = Some(Contact { source, gesture });
    }
    fn motion(&mut self, source: Source, p: Point) {
        let Some(contact) = self.contact.filter(|contact| contact.source == source) else {
            return;
        };
        if self.protected.is_some_and(|r| r.contains(p.x, p.y)) {
            self.finish_contact();
            return;
        }
        if self.pass || self.hidden || self.suspended {
            return;
        }
        match contact.gesture {
            Gesture::Erase => self
                .history
                .erase(p, self.width.max(12.) * self.canvas_units),
            Gesture::Laser(id) => self.laser_sample(id, p),
            Gesture::Ink => {
                let can_append = self.draft_point_count() < MAX_POINTS;
                if let Some(stroke) = &mut self.history.active {
                    Self::append(stroke, p, can_append);
                }
            }
        }
    }
    fn append(stroke: &mut Stroke, p: Point, can_append: bool) {
        if matches!(
            stroke.tool,
            Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::Ellipse
        ) {
            if stroke.points.len() == 1 && can_append {
                stroke.points.push(p);
            } else if let Some(end) = stroke.points.get_mut(1) {
                *end = p;
            }
        } else if can_append
            && stroke.points.len() < 100_000
            && stroke.points.last().is_none_or(|last| {
                last.dist(p) > 0.15
                    || (stroke.pressure_enabled && (last.pressure - p.pressure).abs() > 0.00001)
            })
        {
            stroke.points.push(p);
        }
    }
    fn end(&mut self, source: Source) {
        if self.contact.is_some_and(|contact| contact.source == source) {
            self.finish_contact();
        }
    }
    fn finish_contact(&mut self) {
        self.contact = None;
        self.history.commit();
        self.end_erase_if_idle();
    }
    fn end_erase_if_idle(&mut self) {
        if self
            .contact
            .is_none_or(|contact| contact.gesture != Gesture::Erase)
            && self
                .touches
                .values()
                .all(|finger| finger.gesture != Gesture::Erase)
        {
            self.history.end_erase();
        }
    }
    fn discard_touches(&mut self) {
        self.touches.clear();
        self.end_erase_if_idle();
    }
    fn stop(&mut self) {
        self.finish_contact();
        self.commit_touches();
        self.history.end_erase();
        for pen in self.pens.values_mut() {
            pen.down = false;
        }
    }
    fn use_mouse(&mut self) {
        self.stop();
        self.reset_contacts();
        self.pass = true;
        self.wait_for_mouse_up = true;
        self.panel = None;
        self.status = "Mouse mode: click the desktop. Select any drawing tool to resume.".into();
    }
    fn use_tool(&mut self, tool: Tool) {
        self.stop();
        self.reset_contacts();
        self.tool = tool;
        self.pass = false;
        self.wait_for_mouse_up = false;
        self.hidden = false;
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
        self.history.active = None;
        self.contact = None;
        self.discard_touches();
        for pen in self.pens.values_mut() {
            pen.down = false;
        }
        self.laser.clear();
    }
    fn refresh_displays(&mut self, displays: Vec<Display>, preferred: Option<u32>) -> (bool, bool) {
        let old = self
            .displays
            .iter()
            .find(|d| Some(d.id) == self.pages.active)
            .cloned();
        let removed = self.pages.sync(&mut self.history, &displays);
        if removed {
            self.reset_contacts();
        }
        let target = preferred
            .filter(|id| displays.iter().any(|d| d.id == *id))
            .or(self.pages.active)
            .or_else(|| displays.first().map(|d| d.id));
        if let Some(id) = target {
            if self.pages.active != Some(id) {
                self.stop();
                self.reset_contacts();
                self.pages.switch(&mut self.history, id);
            }
        } else {
            self.reset_contacts();
        }
        self.suspended = target.is_none();
        self.display = displays
            .iter()
            .position(|d| Some(d.id) == target)
            .unwrap_or(0);
        let new = displays.get(self.display);
        let layout = old.as_ref().map(|d| (d.id, d.bounds, d.scale))
            != new.map(|d| (d.id, d.bounds, d.scale));
        self.displays = displays;
        if removed {
            self.status = "Display disconnected: its ink and undo history were cleared".into();
        }
        (layout, removed)
    }
    fn refresh_hz(&self) -> f64 {
        self.displays.get(self.display).map_or(0.0, |d| d.refresh)
    }
    fn draft_point_count(&self) -> usize {
        self.history
            .doc
            .strokes
            .iter()
            .map(|s| s.points.len())
            .sum::<usize>()
            + self.history.active.as_ref().map_or(0, |s| s.points.len())
            + self
                .touches
                .values()
                .filter_map(|f| f.stroke.as_ref())
                .map(|s| s.points.len())
                .sum::<usize>()
    }
    fn commit_touch(&mut self, finger: FingerStroke) {
        if let Some(stroke) = finger.stroke
            && !stroke.points.is_empty()
        {
            self.history.snapshot();
            self.history.doc.strokes.push(stroke);
        }
    }
    fn commit_touches(&mut self) {
        for (_, finger) in std::mem::take(&mut self.touches) {
            self.commit_touch(finger);
        }
    }
    fn touch_motion(&mut self, key: TouchKey, point: Point) {
        if self.protected.is_some_and(|r| r.contains(point.x, point.y)) {
            return;
        }
        let can_append = self.draft_point_count() < MAX_POINTS;
        let Some(finger) = self.touches.get_mut(&key) else {
            return;
        };
        match finger.gesture {
            Gesture::Erase => self
                .history
                .erase(point, self.width.max(12.) * self.canvas_units),
            Gesture::Laser(id) => self.laser_sample(id, point),
            Gesture::Ink => {
                if let Some(stroke) = &mut finger.stroke {
                    Self::append(stroke, point, can_append);
                }
            }
        }
    }
    fn touch_event(&mut self, event: SDL_TouchFingerEvent, size: [f32; 2]) {
        let key = TouchKey::from(event);
        if event.r#type == SDL_EVENT_FINGER_CANCELED {
            if let Some(finger) = self.touches.remove(&key)
                && let Gesture::Laser(id) = finger.gesture
            {
                self.laser.remove(&id);
            }
            self.end_erase_if_idle();
            return;
        }
        if event.r#type == SDL_EVENT_FINGER_UP {
            if let Some(point) = touch::point(event, size) {
                self.touch_motion(key, point);
            }
            if let Some(finger) = self.touches.remove(&key) {
                self.commit_touch(finger);
            }
            self.end_erase_if_idle();
            return;
        }
        if !self.touch_enabled
            || self.pass
            || self.hidden
            || self.suspended
            || !self.pen_proximity.is_empty()
        {
            return;
        }
        let Some(point) = touch::point(event, size) else {
            return;
        };
        if event.r#type == SDL_EVENT_FINGER_DOWN {
            if self.protected.is_some_and(|r| r.contains(point.x, point.y)) {
                return;
            }
            if self.touches.contains_key(&key)
                || self.touches.len() >= 16
                || touch::is_indirect(event.touchID)
                || !(0.0..=1.0).contains(&event.x)
                || !(0.0..=1.0).contains(&event.y)
                || self.draft_point_count() >= MAX_POINTS
            {
                return;
            }
            let gesture = self.gesture(self.tool);
            let stroke = if gesture == Gesture::Ink {
                let mut stroke = self.stroke(self.tool);
                // Capacitive contact pressure is often absent or contact area, not pen force.
                stroke.pressure_enabled = false;
                stroke.points.push(point);
                Some(stroke)
            } else {
                None
            };
            self.touches.insert(key, FingerStroke { gesture, stroke });
        }
        self.touch_motion(key, point);
    }
    // Device proximity is independent of the current tool or modal picker.
    fn track_pen(&mut self, event: &SDL_Event) {
        let id = unsafe {
            match SDL_EventType(event.r#type) {
                SDL_EVENT_PEN_PROXIMITY_IN => event.pproximity.which.0,
                SDL_EVENT_PEN_PROXIMITY_OUT => {
                    let id = event.pproximity.which.0;
                    self.end(Source::Pen(id));
                    self.pens.remove(&id);
                    self.pen_proximity.remove(&id);
                    return;
                }
                SDL_EVENT_PEN_AXIS => event.paxis.which.0,
                SDL_EVENT_PEN_MOTION => event.pmotion.which.0,
                SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => event.ptouch.which.0,
                _ => return,
            }
        };
        if self.pen_proximity.insert(id) {
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
                SDL_EVENT_MOUSE_BUTTON_UP if e.button.windowID == overlay => {
                    if e.button.which != SDL_PEN_MOUSEID && e.button.which != SDL_TOUCH_MOUSEID {
                        self.end(Source::Mouse(e.button.button));
                    }
                }
                SDL_EVENT_MOUSE_MOTION if e.motion.windowID == overlay => {
                    let m = e.motion;
                    if m.which != SDL_PEN_MOUSEID
                        && m.which != SDL_TOUCH_MOUSEID
                        && let Some(contact) = self.contact
                        && matches!(contact.source, Source::Mouse(_))
                    {
                        self.motion(contact.source, Point::new(m.x, m.y, 1., [0.; 2]));
                    }
                }
                SDL_EVENT_FINGER_DOWN
                | SDL_EVENT_FINGER_MOTION
                | SDL_EVENT_FINGER_UP
                | SDL_EVENT_FINGER_CANCELED
                    if e.tfinger.windowID == overlay =>
                {
                    self.touch_event(e.tfinger, size);
                }
                SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP if e.ptouch.windowID == overlay => {
                    let t = e.ptouch;
                    let pen = self.pens.entry(t.which.0).or_default();
                    pen.down = t.down;
                    let p = pen.point(t.x, t.y);
                    if t.down {
                        self.begin(Source::Pen(t.which.0), p, t.eraser);
                    } else {
                        self.motion(Source::Pen(t.which.0), p);
                        self.end(Source::Pen(t.which.0));
                        if let Some(pen) = self.pens.get_mut(&t.which.0) {
                            pen.pressure = 0.;
                            pen.has_pressure = false;
                        }
                    }
                }
                SDL_EVENT_PEN_AXIS if e.paxis.windowID == overlay => {
                    let a = e.paxis;
                    let pen = self.pens.entry(a.which.0).or_default();
                    match a.axis {
                        SDL_PEN_AXIS_PRESSURE if a.value.is_finite() => {
                            pen.pressure = a.value.clamp(0., 1.);
                            pen.has_pressure = true;
                        }
                        SDL_PEN_AXIS_XTILT => pen.tilt[0] = a.value.clamp(-90., 90.),
                        SDL_PEN_AXIS_YTILT => pen.tilt[1] = a.value.clamp(-90., 90.),
                        _ => {}
                    }
                    let point = pen.point(a.x, a.y);
                    self.motion(Source::Pen(a.which.0), point);
                }
                SDL_EVENT_PEN_MOTION if e.pmotion.windowID == overlay => {
                    let m = e.pmotion;
                    let point = self.pens.entry(m.which.0).or_default().point(m.x, m.y);
                    self.motion(Source::Pen(m.which.0), point);
                }
                SDL_EVENT_WINDOW_FOCUS_LOST if e.window.windowID == overlay => {
                    self.discard_touches();
                    self.stop();
                }
                SDL_EVENT_WINDOW_MOUSE_LEAVE
                    if e.window.windowID == overlay
                        && self
                            .contact
                            .is_some_and(|contact| matches!(contact.source, Source::Mouse(_))) =>
                {
                    self.finish_contact();
                }
                _ => {}
            }
        }
    }
    fn keyboard(&mut self, e: &SDL_Event, editing_text: bool) -> Commands {
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
            if editing_text {
                return c;
            }
            if k.key == SDLK_ESCAPE {
                if self.contact.is_some() || !self.touches.is_empty() {
                    self.reset_contacts();
                } else {
                    self.use_mouse();
                    c.mode = true
                }
                return c;
            }
            if m.command {
                match k.key {
                    SDLK_Z if m.shift => {
                        self.reset_contacts();
                        self.history.redo()
                    }
                    SDLK_Z => {
                        self.reset_contacts();
                        self.history.undo()
                    }
                    SDLK_Y => {
                        self.reset_contacts();
                        self.history.redo()
                    }
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
                    self.stop();
                    self.hidden = !self.hidden;
                    c.mode = true
                }
                SDLK_P | SDLK_B | SDLK_M | SDLK_L | SDLK_A | SDLK_R | SDLK_O | SDLK_E | SDLK_K => {
                    let tool = match k.key {
                        SDLK_B => Tool::Pencil,
                        SDLK_M => Tool::Marker,
                        SDLK_L => Tool::Line,
                        SDLK_A => Tool::Arrow,
                        SDLK_R => Tool::Rectangle,
                        SDLK_O => Tool::Ellipse,
                        SDLK_E => Tool::Eraser,
                        SDLK_K => Tool::Laser,
                        _ => Tool::Pen,
                    };
                    self.use_tool(tool);
                    c.mode = true;
                }
                SDLK_1 => self.select_preset(PRESETS[0]),
                SDLK_2 => self.select_preset(PRESETS[1]),
                SDLK_3 => self.select_preset(PRESETS[2]),
                SDLK_4 => self.select_preset(PRESETS[3]),
                _ => {}
            }
        }
        c
    }
    fn save(&mut self) {
        self.stop();
        let p = PathBuf::from(&self.path);
        let result = if p == self.save_path {
            self.save_session()
        } else {
            self.pages.save(&self.history, &p)
        };
        match result {
            Ok(()) => self.status = format!("Saved {}", p.display()),
            Err(e) => self.status = format!("Save: {e}"),
        }
    }
    fn save_session(&mut self) -> Result<(), String> {
        self.pages.save(&self.history, &self.save_path)?;
        self.pages.mark_saved(&mut self.history);
        Ok(())
    }
    fn load(&mut self) {
        self.stop();
        let p = PathBuf::from(&self.path);
        match self.pages.load(&mut self.history, &p, false) {
            Ok(()) => self.status = "Session loaded".into(),
            Err(e) => self.status = format!("Open: {e}"),
        }
    }
    fn export(&mut self) {
        self.stop();
        let p = PathBuf::from(&self.path).with_extension("svg");
        let data = export::svg(&self.history.doc);
        match std::fs::write(&p, data) {
            Ok(()) => self.status = format!("Exported {}", p.display()),
            Err(e) => self.status = format!("Export: {e}"),
        }
    }
    fn select_preset(&mut self, rgb: [u8; 3]) {
        self.select_color(InkColor::from_rgb8(rgb).converted(self.color.space));
    }
    fn select_color(&mut self, color: InkColor) {
        self.color = color;
        self.hex_entry = crate::color::HexEntry::new(color);
    }
    fn paint(
        &mut self,
        r: &Renderer,
        w: &Window,
        color: &ColorManager,
        background: &mut crate::background::Background,
    ) -> Result<(), String> {
        let bg = if self.board && !self.pass {
            [0.97, 0.97, 0.96, 1.]
        } else {
            [0., 0., 0., if self.pass { 0. } else { 1. / 255. }]
        };
        // Clear the protected area to transparent even on software renderers,
        // which cannot implement SDL's custom alpha-mask texture blend mode.
        let scale = r.begin(w, [0.; 4])?;
        let clips = crate::controller::outside(w.size().map(|v| v.ceil() as i32), self.protected);
        for area in &clips {
            unsafe {
                check(SDL_SetRenderClipRect(r.raw, &area.sdl()))?;
                check(SDL_SetRenderDrawColorFloat(
                    r.raw, bg[0], bg[1], bg[2], bg[3],
                ))?;
                check(SDL_RenderFillRect(r.raw, ptr::null()))?;
            }
            if !self.pass
                && let Err(error) = background.draw(r, w, color)
            {
                background.live();
                self.frozen = false;
                self.status = format!("Frozen background unavailable: {error}");
            }
        }
        unsafe {
            SDL_SetRenderClipRect(r.raw, ptr::null());
        }
        let aa = 0.8 / scale[0].max(scale[1]);
        for s in &self.history.doc.strokes {
            paint_stroke(r, s, color, aa, &clips)?
        }
        if let Some(ref s) = self.history.active {
            paint_stroke(r, s, color, aa, &clips)?
        }
        for finger in self.touches.values() {
            if let Some(ref stroke) = finger.stroke {
                paint_stroke(r, stroke, color, aa, &clips)?;
            }
        }
        let now = Instant::now();
        self.laser.retain(|_, trail| {
            while trail
                .front()
                .is_some_and(|(_, at)| now.duration_since(*at) >= LASER_LIFETIME)
            {
                trail.pop_front();
            }
            !trail.is_empty()
        });
        let mut laser = Stroke {
            tool: Tool::Laser,
            color: InkColor::from_rgb8([246, 72, 99]),
            width: 4. * self.canvas_units,
            opacity: 1.,
            pressure_curve: 1.,
            pressure_enabled: false,
            points: Vec::new(),
        };
        for trail in self.laser.values() {
            laser.points.clear();
            laser.points.extend(trail.iter().map(|(point, at)| Point {
                // The laser ribbon uses pressure as its per-sample fade.
                pressure: 1. - now.duration_since(*at).as_secs_f32() / LASER_LIFETIME.as_secs_f32(),
                ..*point
            }));
            paint_stroke(r, &laser, color, aa, &clips)?;
        }
        r.present()
    }
}
fn paint_stroke(
    r: &Renderer,
    s: &Stroke,
    color: &ColorManager,
    aa: f32,
    clips: &[crate::controller::Rect],
) -> Result<(), String> {
    let m = geometry::mesh(s, aa);
    let rgb = color.map(s.color.output_rgb(r.extended));
    let vertices: Vec<SDL_Vertex> = m
        .vertices
        .iter()
        .map(|v| SDL_Vertex {
            position: SDL_FPoint {
                x: v.xy[0],
                y: v.xy[1],
            },
            color: SDL_FColor {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
                a: s.opacity * v.coverage,
            },
            tex_coord: SDL_FPoint { x: 0., y: 0. },
        })
        .collect();
    let bounds = vertices.iter().fold(
        [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ],
        |b, v| {
            [
                b[0].min(v.position.x),
                b[1].min(v.position.y),
                b[2].max(v.position.x),
                b[3].max(v.position.y),
            ]
        },
    );
    let result = (|| {
        for area in clips {
            if bounds[2] < area.x as f32
                || bounds[3] < area.y as f32
                || bounds[0] >= (area.x + area.w) as f32
                || bounds[1] >= (area.y + area.h) as f32
            {
                continue;
            }
            unsafe {
                check(SDL_SetRenderClipRect(r.raw, &area.sdl()))?;
            }
            r.geometry(ptr::null_mut(), &vertices, &m.indices)?;
        }
        Ok(())
    })();
    unsafe {
        SDL_SetRenderClipRect(r.raw, ptr::null());
    }
    result
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
fn apply_mode(
    a: &mut App,
    w: &Window,
    toolbar: &Window,
    pass: &mut Passthrough,
) -> Result<(), String> {
    if a.hidden || a.suspended {
        return crate::native::show_canvas(w, toolbar, false);
    }
    crate::native::show_canvas(w, toolbar, true)?;
    let mouse = a.mouse_ready();
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let result = pass.set_guarded(w, mouse, a.protected);
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let result = pass.set(w, mouse);
    if let Err(e) = result {
        a.pass = false;
        a.status = format!("Passthrough unavailable: {e}. Use Hide.");
    }
    unsafe {
        SDL_SetWindowAlwaysOnTop(w.raw, true);
        if !a.pass {
            SDL_RaiseWindow(w.raw);
        }
        // Keep the controller above the drawing input region after resuming.
        SDL_RaiseWindow(toolbar.raw);
    }
    crate::native::keep_controller_above(w, toolbar, pass);
    Ok(())
}

fn mouse_released() -> bool {
    // Keep the native input region until SDL receives the matching button-up.
    // Removing it during a drag can lose that release and make SDL discard the
    // next controller button-down as a duplicate of the unfinished drag.
    unsafe { SDL_GetMouseState(ptr::null_mut(), ptr::null_mut()).0 == 0 }
}

pub struct Options {
    pub board: bool,
    pub x11: bool,
    pub sdr: bool,
    pub display: usize,
    pub smoke: bool,
}
pub fn run(options: Options) -> Result<(), String> {
    if options.smoke {
        unsafe {
            SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"dummy".as_ptr());
        }
    }
    let _runtime = Runtime::new(options.x11)?;
    let video = crate::sdl::driver();
    let canvas = Window::new("OverInk - canvas", 1100, 720, !options.smoke)?;
    let topology = crate::display::Topology::new(&video);
    let displays = topology.connected(&canvas).unwrap_or_default();
    let mut app = App::new(options.board || options.smoke, options.display, displays);
    if !options.smoke
        && let Some(id) = app.pages.active
        && let Err(e) = canvas.place_on_display(id, true)
        && video != "wayland"
    {
        return Err(e);
    }
    app.canvas_units = canvas.scale() / unsafe { SDL_GetWindowPixelDensity(canvas.raw) }.max(0.1);
    let canvas_render = Renderer::new(&canvas, options.sdr)?;
    let xwayland = crate::background::is_xwayland(&canvas, &video);
    // Declare renderers before GUI texture owners, so textures are destroyed first.
    let toolbar = Window::new(
        "OverInk",
        crate::toolbar::WIDTH as i32,
        crate::toolbar::HEIGHT as i32,
        false,
    )?;
    toolbar.setup_toolbar();
    if !options.smoke {
        let _ = toolbar.arrange_toolbar(false, app.pages.active);
    }
    let toolbar_render = Renderer::new(&toolbar, options.sdr)?;
    let mut gui = gui::Gui::new();
    let mark = crate::toolbar::brand(&gui.ctx);
    let mut toolbar_expanded = false;
    let mut native = Passthrough::new(&canvas);
    let mut canvas_color = ColorManager::default();
    canvas_color.refresh(&canvas, canvas_render.extended);
    let mut toolbar_color = ColorManager::default();
    toolbar_color.refresh(&toolbar, toolbar_render.extended);
    crate::native::show_canvas(&canvas, &toolbar, true)?;
    if !options.smoke
        && let Some(id) = app.pages.active
    {
        let _ = canvas.place_on_display(id, true);
    }
    toolbar.show(true)?;
    unsafe {
        SDL_StartTextInput(toolbar.raw);
        SDL_RaiseWindow(toolbar.raw);
    }
    if !options.smoke {
        apply_mode(&mut app, &canvas, &toolbar, &mut native)?;
        unsafe {
            SDL_RaiseWindow(toolbar.raw);
        }
    }
    if options.smoke {
        app.history = History::default();
        smoke_events(&mut app, canvas.id)?;
    }
    let mut picker = crate::picker::Picker::default();
    let mut background = crate::background::Background::default();
    let mut controller_guard = crate::controller::Guard::default();
    let mut picker_windows_hidden = false;
    let mut running = true;
    let mut frame = 0;
    let mut last_color = Instant::now();
    let mut last_displays = Instant::now();
    let mut controller_display = None;
    let mut follow_controller = false;
    let mut requested_display = None;
    let mut topology_dirty = !options.smoke;
    let mut pacer = FramePacer::new(app.frame_rate, app.refresh_hz(), Instant::now());
    while running {
        let mut disconnected = BTreeSet::new();
        let mut controller_scale_changed = false;
        let mut controller_moved = false;
        let mut canvas_surface_changed = false;
        let mut event = SDL_Event::default();
        while unsafe { SDL_PollEvent(&mut event) } {
            let ty = unsafe { SDL_EventType(event.r#type) };
            if ty == SDL_EVENT_QUIT {
                running = false;
                break;
            }
            if ty >= SDL_EVENT_DISPLAY_FIRST && ty <= SDL_EVENT_DISPLAY_LAST {
                topology_dirty = !options.smoke;
                if ty == SDL_EVENT_DISPLAY_REMOVED {
                    disconnected.insert(unsafe { event.display.displayID.0 });
                }
            }
            if matches!(
                ty,
                SDL_EVENT_WINDOW_MOVED
                    | SDL_EVENT_WINDOW_DISPLAY_CHANGED
                    | SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED
            ) && unsafe { event.window.windowID } == toolbar.id
            {
                topology_dirty = !options.smoke;
                controller_scale_changed |= ty == SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED;
                controller_moved |= ty != SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED;
            }
            if matches!(
                ty,
                SDL_EVENT_WINDOW_DISPLAY_CHANGED
                    | SDL_EVENT_WINDOW_DISPLAY_SCALE_CHANGED
                    | SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED
                    | SDL_EVENT_WINDOW_HDR_STATE_CHANGED
            ) && unsafe { event.window.windowID } == canvas.id
            {
                canvas_surface_changed = true;
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if ty == SDL_EVENT_WINDOW_SHOWN && unsafe { event.window.windowID } == canvas.id {
                native.invalidate();
            }
            if ty == SDL_EVENT_WINDOW_CLOSE_REQUESTED {
                let id = unsafe { event.window.windowID };
                if id == toolbar.id {
                    running = false;
                    break;
                } else if id == canvas.id {
                    app.hidden = true;
                    crate::native::show_canvas(&canvas, &toolbar, false)?;
                }
            }
            let was_picking = picker.active();
            picker.event(&event, &canvas);
            gui.event(&event, toolbar.id, toolbar.size());
            if !was_picking
                && !background.pending()
                && !app.suspended
                && (options.smoke || canvas.display_id() == app.pages.active)
            {
                app.event(&event, canvas.id, canvas.size());
            } else {
                app.track_pen(&event);
            }
            let canvas_activated = unsafe {
                (ty == SDL_EVENT_WINDOW_FOCUS_GAINED && event.window.windowID == canvas.id)
                    || (ty == SDL_EVENT_MOUSE_BUTTON_DOWN && event.button.windowID == canvas.id)
                    || (ty == SDL_EVENT_PEN_DOWN && event.ptouch.windowID == canvas.id)
                    || (ty == SDL_EVENT_FINGER_DOWN && event.tfinger.windowID == canvas.id)
            };
            if canvas_activated {
                crate::native::keep_controller_above(&canvas, &toolbar, &native);
            }
            if ty == SDL_EVENT_KEY_DOWN && !was_picking && !background.pending() {
                let id = unsafe { event.key.windowID };
                if id == canvas.id || id == toolbar.id {
                    let c = app.keyboard(
                        &event,
                        id == toolbar.id
                            && (gui.ctx.egui_wants_keyboard_input()
                                || egui::Popup::is_any_open(&gui.ctx)),
                    );
                    if c.mode {
                        apply_mode(&mut app, &canvas, &toolbar, &mut native)?
                    }
                    if c.quit {
                        running = false
                    }
                }
            }
        }
        if !running {
            break;
        }
        if !options.smoke
            && (topology_dirty
                || (video == "x11" && last_displays.elapsed() > Duration::from_millis(500)))
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
                .connected(&canvas)
                .unwrap_or_else(|error| {
                    app.status = format!("Display query: {error}");
                    app.displays.clone()
                })
                .into_iter()
                .filter(|d| !disconnected.contains(&d.id))
                .collect::<Vec<_>>();
            let disconnected = app
                .displays
                .iter()
                .any(|old| !displays.iter().any(|d| d.id == old.id));
            let (layout, removed) = app.refresh_displays(displays, requested.or(moved_to));
            if requested.is_some()
                && video != "wayland"
                && let Err(error) = toolbar.arrange_toolbar(toolbar_expanded, app.pages.active)
            {
                app.status = error;
            }
            if layout || removed || canvas.display_id() != app.pages.active {
                if picker.active() {
                    picker.cancel();
                }
                crate::native::show_canvas(&canvas, &toolbar, false)?;
                if let Some(id) = app.pages.active {
                    if let Err(error) = canvas.place_on_display(id, true) {
                        app.status = format!("Cannot move the drawing surface: {error}");
                        app.suspended = true;
                    }
                    if removed {
                        let _ = toolbar.recover_on_display(id);
                    }
                    canvas_color.refresh(&canvas, canvas_render.extended);
                    toolbar_color.refresh(&toolbar, toolbar_render.extended);
                    background.invalidate_texture();
                    apply_mode(&mut app, &canvas, &toolbar, &mut native)?;
                }
            }
            if disconnected
                && app.pages.needs_save(&app.history)
                && let Err(error) = app.save_session()
            {
                app.status = format!("Save after display removal: {error}");
            }
            topology_dirty = false;
            last_displays = Instant::now();
        }
        if canvas_surface_changed {
            canvas_color.refresh(&canvas, canvas_render.extended);
            background.invalidate_texture();
        }
        app.canvas_units =
            canvas.scale() / unsafe { SDL_GetWindowPixelDensity(canvas.raw) }.max(0.1);
        pacer.configure(app.frame_rate, app.refresh_hz(), Instant::now());
        let snapshot_mode =
            crate::background::snapshot_mode(&canvas, &canvas_render, &video, xwayland);
        if let Err(reason) = background.display(
            app.displays.get(app.display).filter(|_| !app.suspended),
            &app.displays.iter().map(|d| d.id).collect(),
            snapshot_mode,
            !app.pass,
        ) {
            app.frozen = false;
            app.status = format!("{reason}. Showing the live desktop.");
        }
        if (picker.hides_windows() || background.pending()) && !picker_windows_hidden {
            crate::native::show_canvas(&canvas, &toolbar, false)?;
            toolbar.show(false)?;
            picker_windows_hidden = true;
        }
        if let Some(result) = picker.poll(&canvas) {
            app.picking = false;
            match result {
                Ok(Some(color)) => {
                    app.select_color(color);
                    app.status = format!("Picked {}", color.hex());
                }
                Ok(None) => app.status = "Color selection cancelled".into(),
                Err(error) => app.status = error,
            }
        }
        if let Some(result) = background.poll(&canvas) {
            app.frozen = background.frozen;
            app.status = match result {
                Ok(()) => "Desktop frozen. Select Live desktop to resume the background.".into(),
                Err(error) => format!("{error}. Showing the live desktop."),
            };
        }
        if picker_windows_hidden && !picker.hides_windows() && !background.pending() {
            toolbar.show(true)?;
            apply_mode(&mut app, &canvas, &toolbar, &mut native)?;
            picker_windows_hidden = false;
        }
        if picker.hides_windows() || background.pending() {
            unsafe {
                SDL_Delay(8);
            }
            continue;
        }
        // The OS/SDL updates HDR white point dynamically. ICC changes are polled to
        // also catch profile replacement without a display move notification.
        if last_color.elapsed() > Duration::from_secs(3) {
            let previous = canvas_color.revision;
            canvas_color.refresh(&canvas, canvas_render.extended);
            toolbar_color.refresh(&toolbar, toolbar_render.extended);
            if previous != canvas_color.revision {
                background.invalidate_texture();
            }
            last_color = Instant::now();
        }
        let expanded = app.panel.is_some() && !app.picking;
        if expanded != toolbar_expanded || controller_scale_changed {
            toolbar_expanded = expanded;
            if let Err(error) = toolbar.arrange_toolbar(expanded, None) {
                app.status = error;
            }
        }
        let mut input = gui.input(&toolbar);
        input.predicted_dt = pacer.period.as_secs_f32();
        let ctx = gui.ctx.clone();
        let mut cmd = Commands::default();
        let output = ctx.run_ui(input, |ui| {
            cmd = app.ui(
                ui,
                &canvas_render,
                &canvas_color,
                &video,
                snapshot_mode.unavailable(),
                mark.id(),
            )
        });
        if cmd.quit {
            running = false
        }
        if cmd.background {
            if picker.active() {
                picker.cancel();
            }
            app.stop();
            app.reset_contacts();
            if app.frozen {
                background.freeze();
                if app.pass {
                    background.pause_capture();
                }
            } else {
                background.live();
            }
        }
        if cmd.pick && !background.pending() {
            if picker.active() {
                picker.cancel();
            } else {
                app.stop();
                app.picking = true;
                picker.start(&video);
                if picker.choosing() {
                    crate::native::show_canvas(&canvas, &toolbar, true)?;
                    native.set(&canvas, false)?;
                    unsafe {
                        SDL_RaiseWindow(canvas.raw);
                        SDL_RaiseWindow(toolbar.raw);
                    }
                }
                if picker.hides_windows() {
                    crate::native::show_canvas(&canvas, &toolbar, false)?;
                    toolbar.show(false)?;
                    picker_windows_hidden = true;
                }
            }
        }
        if let Some(id) = cmd.display {
            requested_display = Some(id);
            topology_dirty = true;
        }
        if cmd.frame_rate {
            if let Err(error) = app.frame_rate.save(&state_dir().join("frame-rate.json")) {
                app.status = format!("Frame-rate setting: {error}");
            }
            pacer.configure(app.frame_rate, app.refresh_hz(), Instant::now());
        }
        if cmd.mode {
            apply_mode(&mut app, &canvas, &toolbar, &mut native)?;
        }
        if picker.hides_windows() || background.pending() {
            if !picker_windows_hidden {
                crate::native::show_canvas(&canvas, &toolbar, false)?;
                toolbar.show(false)?;
                picker_windows_hidden = true;
            }
            // Consume texture deltas from the frame that launched the sampler,
            // without presenting an application window over the desktop sample.
            gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
            unsafe {
                SDL_Delay(8);
            }
            continue;
        }
        toolbar_render.begin(&toolbar, [246. / 255., 250. / 255., 253. / 255., 1.])?;
        gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
        toolbar_render.present()?;
        if !follow_controller {
            // Establish the baseline after the first map/configure, without
            // letting the compositor's initial placement override --display.
            unsafe {
                SDL_SyncWindow(toolbar.raw);
                SDL_PumpEvents();
            }
            controller_display = toolbar.display_id();
            follow_controller = true;
        }
        let mouse = !picker.choosing() && app.mouse_ready();
        match controller_guard.update(&canvas, &toolbar, mouse, canvas_render.name != "software") {
            Ok(rect) => app.protected = rect,
            Err(error) => {
                app.hidden = true;
                crate::native::show_canvas(&canvas, &toolbar, false)?;
                app.status = format!("Drawing layer hidden to keep the controls usable: {error}");
            }
        }
        if picker.choosing() {
            canvas_render.begin(&canvas, [0., 0., 0., 1. / 255.])?;
            canvas_render.present()?;
        } else if !app.hidden
            && !app.suspended
            && (options.smoke || canvas.display_id() == app.pages.active)
        {
            app.history.doc.size = canvas.size();
            app.paint(&canvas_render, &canvas, &canvas_color, &mut background)?;
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if !options.smoke
            && !app.hidden
            && !app.suspended
            && let Err(error) = native.set_guarded(&canvas, mouse, app.protected)
        {
            app.pass = false;
            app.status = format!("Input region: {error}");
            if video == "x11" && app.protected.is_some() {
                app.hidden = true;
                crate::native::show_canvas(&canvas, &toolbar, false)?;
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        if mouse && let Err(error) = native.set(&canvas, true) {
            app.status = format!("Mouse mode: {error}");
        }
        if !options.smoke
            && app.last_save.elapsed() > Duration::from_secs(10)
            && app.history.active.is_none()
            && app.touches.is_empty()
        {
            if app.pages.needs_save(&app.history)
                && let Err(e) = app.save_session()
            {
                app.status = format!("Autosave: {e}");
            }
            app.last_save = Instant::now();
        }
        frame += 1;
        if options.smoke && frame >= 3 {
            let path = state_dir();
            std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            std::fs::write(path.join("smoke.svg"), export::svg(&app.history.doc))
                .map_err(|e| e.to_string())?;
            unsafe {
                let surface = SDL_RenderReadPixels(canvas_render.raw, ptr::null());
                if surface.is_null() {
                    return Err(crate::sdl::error());
                }
                let p = crate::sdl::cstring(&path.join("smoke.bmp").to_string_lossy());
                let ok = SDL_SaveBMP(surface, p.as_ptr());
                SDL_DestroySurface(surface);
                check(ok)?;
            }
            unsafe {
                let surface = SDL_RenderReadPixels(toolbar_render.raw, ptr::null());
                if surface.is_null() {
                    return Err(crate::sdl::error());
                }
                let p = crate::sdl::cstring(&path.join("toolbar.bmp").to_string_lossy());
                let ok = SDL_SaveBMP(surface, p.as_ptr());
                SDL_DestroySurface(surface);
                check(ok)?;
            }
            println!(
                "OverInk smoke OK: {} strokes, native pressure and multi-touch events, egui textures, SDL geometry, SVG, BMP",
                app.history.doc.strokes.len()
            );
            running = false;
        }
        pacer.wait();
    }
    if !options.smoke {
        app.stop();
        if app.pages.needs_save(&app.history)
            && let Err(e) = app.save_session()
        {
            return Err(format!("Autosave on exit: {e}"));
        }
    }
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
    if app.history.doc.strokes.len() != 1 || app.history.doc.strokes[0].points.len() < 128 {
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
    if app.touches.len() != 2 {
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
    if app.history.doc.strokes.len() != 7 || !app.touches.is_empty() {
        return Err("Multi-touch commit smoke failed".into());
    }
    Ok(())
}

#[cfg(test)]
mod touch_tests {
    use super::*;
    fn app() -> App {
        App {
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
            board: true,
            display: 0,
            displays: vec![],
            pages: Pages::default(),
            frame_rate: FrameRate::Auto,
            suspended: false,
            frozen: false,
            protected: None,
            panel: None,
            pens: HashMap::new(),
            pen_proximity: BTreeSet::new(),
            touches: BTreeMap::new(),
            touch_enabled: true,
            contact: None,
            laser: BTreeMap::new(),
            next_laser: 0,
            status: String::new(),
            path: String::new(),
            save_path: PathBuf::new(),
            canvas_units: 1.,
            last_save: Instant::now(),
        }
    }
    fn finger(app: &mut App, ty: SDL_EventType, device: u64, id: u64, x: f32, y: f32) {
        let event = SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: ty,
                touchID: SDL_TouchID(device),
                fingerID: SDL_FingerID(id),
                windowID: SDL_WindowID(1),
                x,
                y,
                ..Default::default()
            },
        };
        app.event(&event, SDL_WindowID(1), [800., 600.]);
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
        assert!(a.history.active.is_none() && a.history.doc.strokes.is_empty());
        a.history.undo();
        assert_eq!(a.history.doc.strokes.len(), 2);
        a.history.undo();
        assert_eq!(a.history.doc.strokes.len(), 1);
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
        assert!(a.touches.is_empty() && a.pen_proximity.contains(&4));
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
        assert_eq!(a.laser.len(), 4);
        assert!(
            a.laser.values().all(|trail| {
                trail.len() >= 2 && trail.iter().all(|(p, _)| p.x == trail[0].0.x)
            })
        );
        assert!(a.history.doc.strokes.is_empty());
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
        assert!(a.contact.is_none());
        assert_eq!(a.history.doc.strokes.len(), 1);
        assert_eq!(a.history.doc.strokes[0].points.len(), 2);
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
        assert_eq!(a.history.doc.strokes.len(), 1);
        assert_eq!(a.history.doc.strokes[0].points.len(), 2);
    }
    #[test]
    fn controller_area_rejects_pen_touch_and_eraser_edits() {
        let mut a = app();
        a.protected = Some(crate::controller::Rect {
            x: 100,
            y: 100,
            w: 300,
            h: 200,
        });
        a.begin(Source::Pen(1), Point::new(150., 150., 1., [0.; 2]), false);
        assert!(a.history.active.is_none());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.3);
        assert!(a.touches.is_empty());
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
        assert_eq!(a.history.doc.strokes.len(), 1);
        assert!(a.history.doc.strokes[0].points.iter().all(|p| p.x < 100.));
        a.use_tool(Tool::Eraser);
        let count = a.history.doc.strokes.len();
        a.begin(
            Source::Mouse(LEFT_BUTTON),
            Point::new(150., 150., 1., [0.; 2]),
            true,
        );
        assert_eq!(a.history.doc.strokes.len(), count);
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
        assert!(a.history.doc.strokes.is_empty());
        assert_eq!(a.frame_rate.hz(a.refresh_hz()), 120.0);
        a.refresh_displays(vec![screen(20, 1920, 90.0), screen(10, 0, 60.0)], Some(10));
        assert_eq!(a.history.doc.strokes.len(), 1);
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
        assert!(a.touches.is_empty());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        a.history.undo();
        a.history.redo();
        assert!(a.history.doc.strokes.is_empty());
        assert!(a.touches.is_empty());
        a.refresh_displays(vec![screen(30, 0, 60.0)], Some(30));
        assert!(!a.suspended);
        assert!(a.history.doc.strokes.is_empty());
    }
    #[test]
    fn finger_stroke_is_saved_in_logical_units_without_fake_pen_pressure() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 3, 0.1, 0.2);
        finger(&mut a, SDL_EVENT_FINGER_MOTION, 1, 3, 0.5, 0.5);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 3, 0.5, 0.5);
        assert!(a.touches.is_empty());
        let s = &a.history.doc.strokes[0];
        assert_eq!(s.points.len(), 2);
        assert_eq!([s.points[1].x, s.points[1].y], [400., 300.]);
        assert!(!s.pressure_enabled);
        a.history.undo();
        assert!(a.history.doc.strokes.is_empty());
        a.history.redo();
        assert_eq!(a.history.doc.strokes.len(), 1);
    }
    #[test]
    fn multiple_fingers_and_devices_do_not_join_strokes() {
        let mut a = app();
        for (d, f, x) in [(1, 1, 0.1), (1, 2, 0.4), (2, 1, 0.7)] {
            finger(&mut a, SDL_EVENT_FINGER_DOWN, d, f, x, 0.1);
        }
        assert_eq!(a.touches.len(), 3);
        for (d, f, x) in [(1, 1, 0.1), (1, 2, 0.4), (2, 1, 0.7)] {
            finger(&mut a, SDL_EVENT_FINGER_UP, d, f, x, 0.7);
        }
        assert_eq!(a.history.doc.strokes.len(), 3);
        for s in &a.history.doc.strokes {
            assert_eq!(s.points[0].x, s.points[1].x);
        }
    }
    #[test]
    fn canceled_and_focus_lost_contacts_are_discarded() {
        let mut a = app();
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        finger(&mut a, SDL_EVENT_FINGER_CANCELED, 1, 1, 0.2, 0.2);
        assert!(a.touches.is_empty() && a.history.doc.strokes.is_empty());
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
        assert!(a.touches.is_empty() && a.history.doc.strokes.is_empty());
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
        assert!(a.touches.is_empty());
        a.event(
            &pen(SDL_EVENT_PEN_PROXIMITY_OUT),
            SDL_WindowID(1),
            [800., 600.],
        );
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 1, 0.4, 0.4);
        assert!(a.history.doc.strokes.is_empty());
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 3, 0.2, 0.2);
        finger(&mut a, SDL_EVENT_FINGER_UP, 1, 3, 0.4, 0.4);
        assert_eq!(a.history.doc.strokes.len(), 1);
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
        assert_eq!(a.history.doc.strokes.len(), 1);
    }
    #[test]
    fn disabled_or_passthrough_touch_cannot_draw() {
        let mut a = app();
        a.touch_enabled = false;
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 1, 0.2, 0.2);
        a.touch_enabled = true;
        a.pass = true;
        finger(&mut a, SDL_EVENT_FINGER_DOWN, 1, 2, 0.2, 0.2);
        assert!(a.touches.is_empty() && a.history.doc.strokes.is_empty());
    }
}
