use crate::{
    color::{InkColor, PRESETS, Space},
    export, geometry, gui,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Mouse,
    Pen(u32),
}
struct FingerStroke {
    tool: Tool,
    stroke: Option<Stroke>,
}
#[derive(Default)]
struct Commands {
    mode: bool,
    display: bool,
    quit: bool,
    pick: bool,
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
    hidden: bool,
    board: bool,
    display: usize,
    display_names: Vec<String>,
    pens: HashMap<u32, Pen>,
    pen_proximity: BTreeSet<u32>,
    touches: BTreeMap<TouchKey, FingerStroke>,
    touch_enabled: bool,
    source: Option<Source>,
    pointer: Option<Point>,
    laser: VecDeque<(Point, Instant)>,
    status: String,
    path: String,
    save_path: PathBuf,
    canvas_units: f32,
    last_save: Instant,
    last_ink: Instant,
    pen_seen: bool,
}
impl App {
    fn new(board: bool, display: usize) -> Self {
        let base = state_dir();
        let save_path = base.join("session.overink.json");
        let mut history = History::default();
        let mut status = "Ready. Draw on the canvas. Space switches to passthrough.".to_string();
        if save_path.exists()
            && let Err(e) = history.load(&save_path)
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
            hidden: false,
            board,
            display,
            display_names: crate::sdl::displays(),
            pens: HashMap::new(),
            pen_proximity: BTreeSet::new(),
            touches: BTreeMap::new(),
            touch_enabled: true,
            source: None,
            pointer: None,
            laser: VecDeque::new(),
            status,
            path: save_path.to_string_lossy().into_owned(),
            save_path,
            canvas_units: 1.,
            last_save: Instant::now(),
            last_ink: Instant::now(),
            pen_seen: false,
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
            } else {
                self.opacity
            },
            pressure_curve: self.curve,
            pressure_enabled: self.pressure && matches!(tool, Tool::Pen | Tool::Marker),
            points: vec![],
        }
    }
    fn begin(&mut self, source: Source, p: Point, eraser: bool) {
        if self.pass || self.hidden {
            return;
        }
        self.history.commit();
        self.source = Some(source);
        self.pointer = Some(p);
        self.last_ink = Instant::now();
        let tool = if eraser { Tool::Eraser } else { self.tool };
        if tool == Tool::Eraser {
            self.history
                .erase(p, self.width.max(12.) * self.canvas_units);
            return;
        }
        if tool == Tool::Laser {
            self.laser.push_back((p, Instant::now()));
            return;
        }
        if self
            .history
            .doc
            .strokes
            .iter()
            .map(|s| s.points.len())
            .sum::<usize>()
            >= MAX_POINTS
        {
            self.status = "One million sample limit reached. Export and clear the canvas.".into();
            self.source = None;
            return;
        }
        let mut s = self.stroke(tool);
        s.points.push(p);
        self.history.active = Some(s);
    }
    fn motion(&mut self, source: Source, p: Point, eraser: bool) {
        self.pointer = Some(p);
        if self.source != Some(source) || self.pass || self.hidden {
            return;
        }
        if eraser || self.tool == Tool::Eraser {
            self.history
                .erase(p, self.width.max(12.) * self.canvas_units);
            return;
        }
        if self.tool == Tool::Laser {
            self.laser.push_back((p, Instant::now()));
            return;
        }
        let committed_points = self
            .history
            .doc
            .strokes
            .iter()
            .map(|s| s.points.len())
            .sum::<usize>();
        if let Some(ref mut s) = self.history.active {
            if matches!(
                s.tool,
                Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::Ellipse
            ) {
                if s.points.len() == 1 {
                    s.points.push(p)
                } else {
                    s.points[1] = p
                }
            } else if s.points.len() < 100000
                && committed_points + s.points.len() < MAX_POINTS
                && s.points.last().is_none_or(|last| {
                    last.dist(p) > 0.15 || (last.pressure - p.pressure).abs() > 0.00001
                })
            {
                s.points.push(p)
            }
        }
    }
    fn end(&mut self, source: Source) {
        if self.source == Some(source) {
            self.history.commit();
            self.source = None
        }
    }
    fn stop(&mut self) {
        self.history.commit();
        self.commit_touches();
        self.source = None;
        for p in self.pens.values_mut() {
            p.down = false
        }
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
        let can_append = self.draft_point_count() < MAX_POINTS;
        let Some(finger) = self.touches.get_mut(&key) else {
            return;
        };
        match finger.tool {
            Tool::Eraser => self
                .history
                .erase(point, self.width.max(12.) * self.canvas_units),
            Tool::Laser => self.laser.push_back((point, Instant::now())),
            _ => {
                let Some(stroke) = finger.stroke.as_mut() else {
                    return;
                };
                if matches!(
                    stroke.tool,
                    Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::Ellipse
                ) {
                    if stroke.points.len() == 1 {
                        stroke.points.push(point);
                    } else {
                        stroke.points[1] = point;
                    }
                } else if can_append
                    && stroke.points.len() < 100_000
                    && stroke.points.last().is_none_or(|p| p.dist(point) > 0.15)
                {
                    stroke.points.push(point);
                }
            }
        }
    }
    fn touch_event(&mut self, event: SDL_TouchFingerEvent, size: [f32; 2]) {
        let key = TouchKey::from(event);
        if event.r#type == SDL_EVENT_FINGER_CANCELED {
            self.touches.remove(&key);
            return;
        }
        if event.r#type == SDL_EVENT_FINGER_UP {
            if let Some(point) = touch::point(event, size) {
                self.touch_motion(key, point);
            }
            if let Some(finger) = self.touches.remove(&key) {
                self.commit_touch(finger);
            }
            return;
        }
        if !self.touch_enabled || self.pass || self.hidden || !self.pen_proximity.is_empty() {
            return;
        }
        let Some(point) = touch::point(event, size) else {
            return;
        };
        if event.r#type == SDL_EVENT_FINGER_DOWN {
            if self.touches.contains_key(&key)
                || self.touches.len() >= 16
                || touch::is_indirect(event.touchID)
                || !(0.0..=1.0).contains(&event.x)
                || !(0.0..=1.0).contains(&event.y)
                || self.draft_point_count() >= MAX_POINTS
            {
                return;
            }
            let stroke = if matches!(self.tool, Tool::Eraser | Tool::Laser) {
                None
            } else {
                let mut stroke = self.stroke(self.tool);
                // Capacitive contact pressure is often absent or contact area, not pen force.
                stroke.pressure_enabled = false;
                stroke.points.push(point);
                Some(stroke)
            };
            self.touches.insert(
                key,
                FingerStroke {
                    tool: self.tool,
                    stroke,
                },
            );
        }
        self.touch_motion(key, point);
    }
    fn event(&mut self, e: &SDL_Event, overlay: SDL_WindowID, size: [f32; 2]) {
        unsafe {
            let ty = SDL_EventType(e.r#type);
            match ty {
                SDL_EVENT_MOUSE_BUTTON_DOWN if e.button.windowID == overlay => {
                    let b = e.button;
                    if b.which == SDL_PEN_MOUSEID || b.which == SDL_TOUCH_MOUSEID {
                        return;
                    }
                    if b.button == 1 || b.button == 3 {
                        self.begin(
                            Source::Mouse,
                            Point::new(b.x, b.y, 1., [0.; 2]),
                            b.button == 3,
                        )
                    }
                }
                SDL_EVENT_MOUSE_BUTTON_UP if e.button.windowID == overlay => {
                    if e.button.which != SDL_PEN_MOUSEID && e.button.which != SDL_TOUCH_MOUSEID {
                        self.end(Source::Mouse)
                    }
                }
                SDL_EVENT_MOUSE_MOTION if e.motion.windowID == overlay => {
                    let m = e.motion;
                    if m.which != SDL_PEN_MOUSEID && m.which != SDL_TOUCH_MOUSEID {
                        self.motion(
                            Source::Mouse,
                            Point::new(m.x, m.y, 1., [0.; 2]),
                            m.state & SDL_BUTTON_RMASK != 0,
                        )
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
                SDL_EVENT_PEN_PROXIMITY_IN => {
                    self.pen_proximity.insert(e.pproximity.which.0);
                    self.touches.clear();
                }
                SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP if e.ptouch.windowID == overlay => {
                    let t = e.ptouch;
                    self.pen_seen = true;
                    self.pen_proximity.insert(t.which.0);
                    self.touches.clear();
                    let pen = self.pens.entry(t.which.0).or_default();
                    pen.down = t.down;
                    let p = Point::new(
                        t.x,
                        t.y,
                        if pen.has_pressure { pen.pressure } else { 0.5 },
                        pen.tilt,
                    );
                    if t.down {
                        self.begin(Source::Pen(t.which.0), p, t.eraser)
                    } else {
                        self.motion(Source::Pen(t.which.0), p, t.eraser);
                        self.end(Source::Pen(t.which.0));
                        if let Some(p) = self.pens.get_mut(&t.which.0) {
                            p.pressure = 0.;
                            p.has_pressure = false;
                        }
                    }
                }
                SDL_EVENT_PEN_AXIS if e.paxis.windowID == overlay => {
                    let a = e.paxis;
                    self.pen_seen = true;
                    self.pen_proximity.insert(a.which.0);
                    self.touches.clear();
                    let pen = self.pens.entry(a.which.0).or_default();
                    match a.axis {
                        SDL_PEN_AXIS_PRESSURE => {
                            pen.pressure = if a.value.is_finite() {
                                a.value.clamp(0., 1.)
                            } else {
                                0.
                            };
                            pen.has_pressure = true
                        }
                        SDL_PEN_AXIS_XTILT => pen.tilt[0] = a.value.clamp(-90., 90.),
                        SDL_PEN_AXIS_YTILT => pen.tilt[1] = a.value.clamp(-90., 90.),
                        _ => {}
                    }
                    let p = Point::new(a.x, a.y, pen.pressure, pen.tilt);
                    let eraser = a.pen_state & SDL_PEN_INPUT_ERASER_TIP != 0;
                    self.motion(Source::Pen(a.which.0), p, eraser);
                }
                SDL_EVENT_PEN_MOTION if e.pmotion.windowID == overlay => {
                    let m = e.pmotion;
                    self.pen_seen = true;
                    self.pen_proximity.insert(m.which.0);
                    self.touches.clear();
                    let pen = self.pens.entry(m.which.0).or_default();
                    let p = Point::new(
                        m.x,
                        m.y,
                        if pen.has_pressure { pen.pressure } else { 0.5 },
                        pen.tilt,
                    );
                    self.motion(
                        Source::Pen(m.which.0),
                        p,
                        m.pen_state & SDL_PEN_INPUT_ERASER_TIP != 0,
                    );
                }
                SDL_EVENT_PEN_PROXIMITY_OUT => {
                    let id = e.pproximity.which.0;
                    self.end(Source::Pen(id));
                    self.pens.remove(&id);
                    self.pen_proximity.remove(&id);
                }
                SDL_EVENT_WINDOW_FOCUS_LOST if e.window.windowID == overlay => {
                    self.touches.clear();
                    self.pen_proximity.clear();
                    self.stop();
                }
                SDL_EVENT_WINDOW_MOUSE_LEAVE if e.window.windowID == overlay => {
                    if self.source == Some(Source::Mouse) {
                        self.end(Source::Mouse)
                    }
                    self.pointer = None
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
                if self.history.active.is_some() || !self.touches.is_empty() {
                    self.history.active = None;
                    self.touches.clear();
                    self.source = None
                } else {
                    self.pass = true;
                    c.mode = true
                }
                return c;
            }
            if m.command {
                match k.key {
                    SDLK_Z if m.shift => {
                        self.touches.clear();
                        self.history.redo()
                    }
                    SDLK_Z => {
                        self.touches.clear();
                        self.history.undo()
                    }
                    SDLK_Y => {
                        self.touches.clear();
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
                    self.stop();
                    self.pass = !self.pass;
                    c.mode = true
                }
                SDLK_H => {
                    self.stop();
                    self.hidden = !self.hidden;
                    c.mode = true
                }
                SDLK_P => self.tool = Tool::Pen,
                SDLK_M => self.tool = Tool::Marker,
                SDLK_L => self.tool = Tool::Line,
                SDLK_A => self.tool = Tool::Arrow,
                SDLK_R => self.tool = Tool::Rectangle,
                SDLK_O => self.tool = Tool::Ellipse,
                SDLK_E => self.tool = Tool::Eraser,
                SDLK_K => self.tool = Tool::Laser,
                SDLK_1 => self.select_color(InkColor::from_rgb8(PRESETS[0])),
                SDLK_2 => self.select_color(InkColor::from_rgb8(PRESETS[1])),
                SDLK_3 => self.select_color(InkColor::from_rgb8(PRESETS[2])),
                SDLK_4 => self.select_color(InkColor::from_rgb8(PRESETS[3])),
                _ => {}
            }
        }
        c
    }
    fn save(&mut self) {
        self.stop();
        let p = PathBuf::from(&self.path);
        match self.history.save(&p) {
            Ok(()) => self.status = format!("Saved {}", p.display()),
            Err(e) => self.status = format!("Save: {e}"),
        }
    }
    fn load(&mut self) {
        self.stop();
        let p = PathBuf::from(&self.path);
        match self.history.load(&p) {
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
    fn select_color(&mut self, color: InkColor) {
        self.color = color;
        self.hex_entry = crate::color::HexEntry::new(color);
    }
    fn color_controls(&mut self, ui: &mut egui::Ui, commands: &mut Commands) {
        ui.horizontal_wrapped(|ui| {
            for (i, rgb) in PRESETS.iter().enumerate() {
                let color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                let selected = self.color.srgb8() == *rgb;
                let button = egui::Button::new(
                    egui::RichText::new((i + 1).to_string()).color(egui::Color32::WHITE),
                )
                .fill(color)
                .stroke(egui::Stroke::new(
                    if selected { 2. } else { 0. },
                    egui::Color32::WHITE,
                ))
                .min_size(egui::vec2(32., 30.));
                if ui
                    .add(button)
                    .on_hover_text(format!("{}  (sRGB)", InkColor::from_rgb8(*rgb).css()))
                    .clicked()
                {
                    self.select_color(InkColor::from_rgb8(*rgb));
                }
            }
            ui.separator();
            ui.label("Palette");
            let mut rgb = self.color.srgb8();
            if ui
                .color_edit_button_srgb(&mut rgb)
                .on_hover_text("Choose a color from the palette")
                .changed()
            {
                self.select_color(InkColor::from_rgb8(rgb));
            }
            if ui
                .button("Pick screen")
                .on_hover_text("Choose a desktop pixel; Esc cancels")
                .clicked()
            {
                commands.pick = true;
            }
            self.hex_entry.sync(self.color);
            ui.label("HEX");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.hex_entry.text)
                    .desired_width(90.)
                    .char_limit(16)
                    .hint_text("#FFFFFF"),
            );
            let accept = ui.button("Apply").clicked()
                || (response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if (response.has_focus() || response.lost_focus())
                && ui.input(|input| input.key_pressed(egui::Key::Escape))
            {
                self.hex_entry.revert();
            } else {
                if (response.changed() || accept)
                    && let Some(color) = self.hex_entry.apply()
                {
                    self.color = color;
                }
                if response.lost_focus() || accept {
                    self.hex_entry.normalize();
                }
            }
            if self.hex_entry.invalid {
                ui.colored_label(
                    egui::Color32::from_rgb(236, 113, 125),
                    "Use #RRGGBB or #RGB",
                );
            }
        });
    }
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        render: &Renderer,
        color: &ColorManager,
        video: &str,
    ) -> Commands {
        let mut c = Commands::default();
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(24, 28, 38))
            .inner_margin(16)
            .show(ui, |ui| {
                if self.picking {
                    ui.heading("OverInk - pick a screen color");
                    ui.label("Click a desktop pixel on this display. Esc or right-click cancels.");
                    if ui.button("Cancel picker").clicked() { c.pick = true; }
                    return;
                }
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("OverInk").color(egui::Color32::from_rgb(224, 227, 240)));
                    ui.label(egui::RichText::new("DRAW OVER YOUR IDEAS").size(10.).color(egui::Color32::GRAY));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Quit").clicked() { c.quit = true; }
                        if ui.button(if self.hidden { "Show" } else { "Hide" }).clicked() {
                            self.stop(); self.hidden = !self.hidden; c.mode = true;
                        }
                        let label = if self.pass { "Resume drawing" } else { "Click through" };
                        let button = egui::Button::new(label).fill(egui::Color32::from_rgb(73, 86, 128));
                        if ui.add(button).clicked() { self.stop(); self.pass = !self.pass; c.mode = true; }
                    });
                });
                ui.add_space(3.);
                ui.horizontal_wrapped(|ui| {
                    for tool in Tool::ALL {
                        if ui.selectable_value(&mut self.tool, tool, tool.label()).changed() { self.stop(); }
                    }
                });
                self.color_controls(ui, &mut c);
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("source-gamut")
                        .selected_text(match self.color.space { Space::Srgb => "sRGB color", Space::DisplayP3 => "Display P3 color" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.color.space, Space::Srgb, "sRGB color");
                            ui.selectable_value(&mut self.color.space, Space::DisplayP3, "Display P3 color");
                        });
                    ui.add(egui::Slider::new(&mut self.width, 1.0..=48.0).text("Size"));
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.pressure, "Pen pressure");
                    if ui.checkbox(&mut self.touch_enabled, "Touch draw").changed() { self.touches.clear(); }
                    ui.add(egui::Slider::new(&mut self.curve, 0.2..=3.).text("Curve"));
                    ui.add(egui::Slider::new(&mut self.opacity, 0.1..=1.).text("Opacity"));
                });
                ui.horizontal(|ui| {
                    if ui.button("Undo").clicked() { self.touches.clear(); self.history.undo(); }
                    if ui.button("Redo").clicked() { self.touches.clear(); self.history.redo(); }
                    if ui.button("Clear").clicked() { self.touches.clear(); self.history.clear(); }
                    ui.separator();
                    if ui.checkbox(&mut self.board, "Whiteboard").changed() { c.mode = true; }
                    egui::ComboBox::from_id_salt("display")
                        .selected_text(self.display_names.get(self.display).cloned().unwrap_or_else(|| "Display".into()))
                        .show_ui(ui, |ui| {
                            for (i, name) in self.display_names.iter().enumerate() {
                                if ui.selectable_value(&mut self.display, i, name).changed() { c.display = true; }
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("File");
                    ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(410.));
                    if ui.button("Save").clicked() { self.save(); }
                    if ui.button("Open").clicked() { self.load(); }
                    if ui.button("SVG").clicked() { self.export(); }
                });
                ui.separator();
                let p = self.pens.values().find(|p| p.down).or_else(|| self.pens.values().next()).copied().unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label(if self.pen_seen { "Native pen" } else if !self.touches.is_empty() { "Touchscreen" } else { "Mouse / waiting for pen" });
                    ui.add(egui::ProgressBar::new(p.pressure).desired_width(100.).text(format!("{:.1}%", p.pressure * 100.)));
                    ui.label(format!("tilt {:.0} / {:.0}   |   {} strokes", p.tilt[0], p.tilt[1], self.history.doc.strokes.len()));
                });
                let (hdr, headroom) = render.hdr();
                let encoding = if hdr { "HDR: SDR-white mapped" } else if render.extended { "linear sRGB" } else { "SDR sRGB" };
                ui.small(format!("{video} / {} / {encoding} / headroom {headroom:.2}x", render.name));
                ui.small(&color.status);
                if video == "wayland" {
                    ui.colored_label(egui::Color32::from_rgb(236, 173, 69),
                        "Wayland: compositor controls overlay position and stacking. Use --x11 for desktop overlay if needed.");
                }
                ui.small("Space: click through   Esc: cancel / pass   P: pen   E: erase   K: laser   Ctrl/Cmd+Z: undo");
                ui.label(egui::RichText::new(&self.status).size(11.).color(egui::Color32::from_rgb(165, 178, 211)));
            });
        c
    }
    fn paint(&mut self, r: &Renderer, w: &Window, color: &ColorManager) -> Result<(), String> {
        let bg = if self.board {
            [0.97, 0.97, 0.96, 1.]
        } else {
            [0., 0., 0., if self.pass { 0. } else { 1. / 255. }]
        };
        let scale = r.begin(w, bg)?;
        let aa = 0.8 / scale[0].max(scale[1]);
        for s in &self.history.doc.strokes {
            paint_stroke(r, s, color, aa, 1.)?
        }
        if let Some(ref s) = self.history.active {
            paint_stroke(r, s, color, aa, 1.)?
        }
        for finger in self.touches.values() {
            if let Some(ref stroke) = finger.stroke {
                paint_stroke(r, stroke, color, aa, 1.)?;
            }
        }
        let now = Instant::now();
        while self
            .laser
            .front()
            .is_some_and(|(_, t)| now.duration_since(*t) > Duration::from_millis(900))
        {
            self.laser.pop_front();
        }
        let laser_template = self.stroke(Tool::Pen);
        for pair in self.laser.make_contiguous().windows(2) {
            let mut s = laser_template.clone();
            s.pressure_enabled = false;
            s.width = 4.;
            s.color = InkColor::from_rgb8([246, 72, 99]);
            s.points = vec![pair[0].0, pair[1].0];
            paint_stroke(
                r,
                &s,
                color,
                aa,
                1. - now.duration_since(pair[0].1).as_secs_f32() / 0.9,
            )?
        }
        r.present()
    }
}
fn paint_stroke(
    r: &Renderer,
    s: &Stroke,
    color: &ColorManager,
    aa: f32,
    fade: f32,
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
                a: s.opacity * v.coverage * fade.clamp(0., 1.),
            },
            tex_coord: SDL_FPoint { x: 0., y: 0. },
        })
        .collect();
    r.geometry(ptr::null_mut(), &vertices, &m.indices)
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
    if a.hidden {
        return w.show(false);
    }
    w.show(true)?;
    if let Err(e) = pass.set(w, a.pass) {
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
    Ok(())
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
    let mut app = App::new(options.board || options.smoke, options.display);
    let canvas = Window::new("OverInk - canvas", 1100, 720, !options.smoke)?;
    if !options.smoke
        && let Err(e) = canvas.place_on_display(options.display, true)
        && video != "wayland"
    {
        return Err(e);
    }
    app.canvas_units = canvas.scale() / unsafe { SDL_GetWindowPixelDensity(canvas.raw) }.max(0.1);
    let canvas_render = Renderer::new(&canvas, options.sdr)?;
    // Declare renderers before GUI texture owners, so textures are destroyed first.
    let toolbar = Window::new("OverInk", 820, 430, false)?;
    let toolbar_render = Renderer::new(&toolbar, options.sdr)?;
    let units = toolbar.scale() / unsafe { SDL_GetWindowPixelDensity(toolbar.raw) }.max(0.1);
    unsafe {
        SDL_SetWindowSize(
            toolbar.raw,
            (820. * units).round() as i32,
            (450. * units).round() as i32,
        );
    }
    unsafe {
        SDL_SetWindowParent(toolbar.raw, canvas.raw);
    }
    let mut gui = gui::Gui::new();
    let mut native = Passthrough::new(&canvas);
    let mut canvas_color = ColorManager::default();
    canvas_color.refresh(&canvas, canvas_render.extended);
    let mut toolbar_color = ColorManager::default();
    toolbar_color.refresh(&toolbar, toolbar_render.extended);
    canvas.show(true)?;
    if !options.smoke {
        let _ = canvas.place_on_display(options.display, true);
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
    let mut picker_windows_hidden = false;
    let mut running = true;
    let mut frame = 0;
    let mut last_color = Instant::now();
    while running {
        let mut event = SDL_Event::default();
        while unsafe { SDL_PollEvent(&mut event) } {
            let ty = unsafe { SDL_EventType(event.r#type) };
            if ty == SDL_EVENT_QUIT {
                running = false;
                break;
            }
            if ty == SDL_EVENT_WINDOW_CLOSE_REQUESTED {
                let id = unsafe { event.window.windowID };
                if id == toolbar.id {
                    running = false;
                    break;
                } else if id == canvas.id {
                    app.hidden = true;
                    canvas.show(false)?;
                }
            }
            let was_picking = picker.active();
            picker.event(&event, &canvas);
            gui.event(&event, toolbar.id, toolbar.size());
            if !was_picking {
                app.event(&event, canvas.id, canvas.size());
            }
            let canvas_activated = unsafe {
                (ty == SDL_EVENT_WINDOW_FOCUS_GAINED && event.window.windowID == canvas.id)
                    || (ty == SDL_EVENT_MOUSE_BUTTON_DOWN && event.button.windowID == canvas.id)
                    || (ty == SDL_EVENT_PEN_DOWN && event.ptouch.windowID == canvas.id)
                    || (ty == SDL_EVENT_FINGER_DOWN && event.tfinger.windowID == canvas.id)
            };
            if canvas_activated {
                crate::native::keep_controller_above(&toolbar);
            }
            if ty == SDL_EVENT_KEY_DOWN && !was_picking {
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
        if picker.hides_windows() && !picker_windows_hidden {
            canvas.show(false)?;
            toolbar.show(false)?;
            picker_windows_hidden = true;
        }
        if let Some(result) = picker.poll(&canvas) {
            app.picking = false;
            app.pen_proximity.clear();
            match result {
                Ok(Some(color)) => {
                    app.select_color(color);
                    app.status = format!("Picked {}", color.css());
                }
                Ok(None) => app.status = "Color selection cancelled".into(),
                Err(error) => app.status = error,
            }
            toolbar.show(true)?;
            apply_mode(&mut app, &canvas, &toolbar, &mut native)?;
            picker_windows_hidden = false;
        }
        if picker.hides_windows() {
            unsafe {
                SDL_Delay(8);
            }
            continue;
        }
        // The OS/SDL updates HDR white point dynamically. ICC changes are polled to
        // also catch profile replacement without a display move notification.
        if last_color.elapsed() > Duration::from_secs(3) {
            canvas_color.refresh(&canvas, canvas_render.extended);
            toolbar_color.refresh(&toolbar, toolbar_render.extended);
            last_color = Instant::now();
        }
        let input = gui.input(&toolbar);
        let ctx = gui.ctx.clone();
        let mut cmd = Commands::default();
        let output = ctx.run_ui(input, |ui| {
            cmd = app.ui(ui, &canvas_render, &canvas_color, &video)
        });
        if cmd.quit {
            running = false
        }
        if cmd.pick {
            if picker.active() {
                picker.cancel();
            } else {
                app.stop();
                app.pen_proximity.clear();
                app.picking = true;
                picker.start(&video);
                if picker.choosing() {
                    canvas.show(true)?;
                    native.set(&canvas, false)?;
                    unsafe {
                        SDL_RaiseWindow(canvas.raw);
                        SDL_RaiseWindow(toolbar.raw);
                    }
                }
                if picker.hides_windows() {
                    canvas.show(false)?;
                    toolbar.show(false)?;
                    picker_windows_hidden = true;
                }
            }
        }
        if cmd.display {
            app.stop();
            canvas
                .place_on_display(app.display, true)
                .unwrap_or_else(|e| app.status = e);
            canvas_color.refresh(&canvas, canvas_render.extended);
        }
        if cmd.mode || cmd.display {
            apply_mode(&mut app, &canvas, &toolbar, &mut native)?;
        }
        if picker.hides_windows() {
            // Consume texture deltas from the frame that launched the sampler,
            // without presenting an application window over the desktop sample.
            gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
            unsafe {
                SDL_Delay(8);
            }
            continue;
        }
        toolbar_render.begin(&toolbar, [0.094, 0.11, 0.149, 1.])?;
        gui.paint(&toolbar_render, &toolbar, &toolbar_color, output)?;
        toolbar_render.present()?;
        if picker.choosing() {
            canvas_render.begin(&canvas, [0., 0., 0., 1. / 255.])?;
            canvas_render.present()?;
        } else if !app.hidden {
            app.history.doc.size = canvas.size();
            app.paint(&canvas_render, &canvas, &canvas_color)?;
            // Software transparent-window updates may replace the input shape.
            // Reapply the empty region after presentation so visible ink stays click-through.
            if app.pass
                && canvas_render.name == "software"
                && let Err(e) = native.set(&canvas, true)
            {
                app.pass = false;
                app.status = e;
            }
        }
        if !options.smoke
            && app.last_save.elapsed() > Duration::from_secs(10)
            && app.history.active.is_none()
            && app.touches.is_empty()
        {
            if app.history.changed {
                match app.history.save(&app.save_path) {
                    Ok(()) => {}
                    Err(e) => app.status = format!("Autosave: {e}"),
                }
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
        // Two windows must not busy-loop if a renderer cannot provide vsync.
        unsafe {
            SDL_Delay(8);
        }
    }
    if !options.smoke {
        app.stop();
        if let Err(e) = app.history.save(&app.save_path) {
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
            Source::Mouse,
            Point::new(100. + i as f32 * 200., 300., 1., [0.; 2]),
            false,
        );
        app.motion(
            Source::Mouse,
            Point::new(250. + i as f32 * 200., 500., 1., [0.; 2]),
            false,
        );
        app.end(Source::Mouse);
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
            hidden: false,
            board: true,
            display: 0,
            display_names: vec![],
            pens: HashMap::new(),
            pen_proximity: BTreeSet::new(),
            touches: BTreeMap::new(),
            touch_enabled: true,
            source: None,
            pointer: None,
            laser: VecDeque::new(),
            status: String::new(),
            path: String::new(),
            save_path: PathBuf::new(),
            canvas_units: 1.,
            last_save: Instant::now(),
            last_ink: Instant::now(),
            pen_seen: false,
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
