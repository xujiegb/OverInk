#![forbid(unsafe_code)]

use super::{App, Options, ink::Ink};
use crate::{
    appearance::{Text, text},
    background::{self, Background, SnapshotMode},
    controller::{Guard, Rect},
    display::Display,
    gui::{self, Gui},
    icc::ColorManager,
    native::{self, Passthrough},
    sdl::*,
};
use std::{collections::BTreeSet, time::Instant};

pub(super) struct Canvas {
    // SDL destroys textures with their renderer, and renderers with their window.
    pub background: Background,
    ink: Ink,
    text: Option<Gui>,
    text_style: Option<(u64, u64)>,
    pub text_repaint_at: Option<Instant>,
    pub render: Renderer,
    native: Passthrough,
    pub window: Window,
    pub color: ColorManager,
    guard: Guard,
    pub output: Option<u32>,
    bounds: Option<[i32; 4]>,
    pub protected: Option<Rect>,
    pub visible: bool,
    pub dirty: bool,
    pub guard_dirty: bool,
    animation: bool,
    mouse: bool,
    #[cfg(target_os = "macos")]
    extended_unavailable: bool,
}

impl Canvas {
    pub fn new(
        window: Window,
        display: Option<&Display>,
        options: &Options,
    ) -> Result<Self, String> {
        if let Some(display) = display.filter(|_| !options.smoke)
            && let Err(error) = window.place_on_display(display.id, true)
            && driver() != "wayland"
        {
            return Err(format!("Positioning the drawing canvas failed ({error})"));
        }
        let render =
            Renderer::new(&window, options.sdr || cfg!(target_os = "macos")).map_err(|error| {
                format!(
                    "Creating the canvas renderer on {} failed ({error})",
                    driver()
                )
            })?;
        let native = Passthrough::new(&window);
        let mut color = ColorManager::default();
        color.refresh(&window, render.extended);
        Ok(Self {
            background: Background::default(),
            ink: Ink::default(),
            text: None,
            text_style: None,
            text_repaint_at: None,
            render,
            native,
            window,
            color,
            guard: Guard::default(),
            output: display.map(|d| d.id),
            bounds: display.map(|d| d.bounds),
            protected: None,
            visible: false,
            dirty: true,
            guard_dirty: true,
            animation: false,
            mouse: false,
            #[cfg(target_os = "macos")]
            extended_unavailable: false,
        })
    }

    fn attach(&mut self, display: Option<&Display>, toolbar: &Window) -> Result<(), String> {
        let output = display.map(|d| d.id);
        let bounds = display.map(|d| d.bounds);
        if self.output == output
            && self.bounds == bounds
            && (output.is_none() || driver() == "wayland" || self.on_output(false))
        {
            return Ok(());
        }
        if let Some(display) = display {
            if driver() != "wayland" || self.output.is_none() {
                self.show(toolbar, false)?;
                self.window.place_on_display(display.id, true)?;
            }
        } else {
            self.show(toolbar, false)?;
            self.background.live();
        }
        self.output = output;
        self.bounds = bounds;
        self.suspend_text_input();
        self.ink.invalidate_layer();
        self.surface_changed();
        self.guard_dirty = true;
        Ok(())
    }

    pub fn show(&mut self, toolbar: &Window, visible: bool) -> Result<(), String> {
        if self.visible != visible {
            native::show_canvas(&self.window, toolbar, visible).map_err(|error| {
                format!(
                    "Updating canvas visibility on {} failed ({error})",
                    driver()
                )
            })?;
            self.visible = visible;
            self.guard_dirty = true;
            self.dirty = true;
            self.native.invalidate();
            if !visible && self.render.extended {
                self.ink.invalidate_layer();
            }
            if !visible {
                self.suspend_text_input();
            }
            if visible {
                if let Some(id) = self.output
                    && let Some([x, y, w, h]) = self.bounds
                    && driver() != "wayland"
                    && (self.window.position().ok() != Some([x, y])
                        || self.window.size() != [w as f32, h as f32])
                {
                    self.window.place_on_display(id, true).map_err(|error| {
                        format!("Positioning the drawing canvas failed ({error})")
                    })?;
                }
                self.window.set_always_on_top(true);
                self.keep_toolbar_above(toolbar);
            }
        }
        Ok(())
    }

    pub fn on_output(&self, smoke: bool) -> bool {
        smoke || self.output.is_some() && self.window.display_id() == self.output
    }

    pub fn set_page_geometry(&self, app: &mut App) {
        app.input.protected = self.protected;
        app.input.canvas_units = self.window.ui_scale().max(0.5);
    }

    pub fn keep_toolbar_above(&self, toolbar: &Window) {
        if toolbar.hidden() {
            return;
        }
        if self.output == toolbar.display_id() || driver() == "wayland" {
            native::keep_controller_above(&self.window, toolbar, &self.native);
        }
    }

    pub fn surface_changed(&mut self) {
        self.color.refresh(&self.window, self.render.extended);
        self.background.invalidate_texture();
        self.dirty = true;
    }

    pub fn renderer_reset(&mut self) {
        // Release live SDL texture objects before rebuilding their contents
        // or destroying the old renderer.
        self.ink.reset_textures();
        self.suspend_text_input();
        // Dropping the old text GUI releases its SDL texture handles.
        self.text = None;
        self.text_style = None;
        self.background.invalidate_texture();
        self.dirty = true;
    }

    pub fn suspend_text_input(&mut self) {
        if let Some(gui) = &mut self.text {
            gui.suspend_input(&self.window);
        }
        self.text_repaint_at = None;
    }

    pub fn has_text_touch(&self, key: crate::touch::TouchKey) -> bool {
        self.text.as_ref().is_some_and(|gui| gui.has_touch(key))
    }

    pub fn cancel_text_touches(&mut self, app: &mut App) -> bool {
        self.text
            .as_mut()
            .is_some_and(|gui| app.cancel_text_touches(gui))
    }

    pub fn text_event(&mut self, app: &mut App, event: &SDL_Event) -> bool {
        let ty = event_type(event);
        let editing = app.input.text.editing();
        if !editing
            && matches!(
                ty,
                SDL_EVENT_KEY_DOWN
                    | SDL_EVENT_KEY_UP
                    | SDL_EVENT_TEXT_INPUT
                    | SDL_EVENT_TEXT_EDITING
            )
        {
            return false;
        }
        if ty == SDL_EVENT_FINGER_DOWN
            && (!app.touch_enabled || !app.input.pen_proximity.is_empty())
        {
            return false;
        }
        let gui = self.text.get_or_insert_with(Gui::new);
        let changed = app.text_event(gui, event, self.window.id, self.window.size());
        if !editing && app.input.text.editing() {
            self.activate_text_editor();
        } else if editing
            && matches!(
                ty,
                SDL_EVENT_WINDOW_FOCUS_GAINED | SDL_EVENT_WINDOW_FOCUS_LOST
            )
        {
            gui.sync_text_input(&self.window, self.window.input_focused());
        }
        changed
    }

    pub fn screenshot_event(&mut self, event: &SDL_Event) -> bool {
        let gui = self.text.get_or_insert_with(Gui::new);
        let before = gui.raw.events.len();
        gui.event(event, self.window.id, self.window.size());
        gui.raw.events.len() != before
    }

    pub fn activate_text_editor(&mut self) {
        self.text.get_or_insert_with(Gui::new).clear_events();
        self.focus_text_editor();
    }

    pub fn focus_text_editor(&mut self) {
        if !self.window.input_focused() {
            self.window.raise();
        }
        self.text
            .get_or_insert_with(Gui::new)
            .sync_text_input(&self.window, true);
        self.dirty = true;
    }

    pub fn flush_text_events(&mut self, app: &mut App, theme: egui::Theme) -> Result<(), String> {
        if app.input.text.editing()
            && self
                .text
                .as_ref()
                .is_some_and(|gui| !gui.raw.events.is_empty())
            && let Some(mut output) = self.text_frame(app, theme, true)
        {
            // Apply input and upload texture deltas before a toolbar action changes the draft.
            // The next canvas frame will paint the resulting document.
            output.shapes.clear();
            if let Some(gui) = self.text.as_mut() {
                gui.paint(&self.render, &self.window, &self.color, output)?;
                self.dirty = true;
            }
        }
        Ok(())
    }

    pub fn finish_text_editor(&mut self, app: &mut App, theme: egui::Theme) -> Result<(), String> {
        self.flush_text_events(app, theme)?;
        app.input.text.finish(&mut app.history);
        self.suspend_text_input();
        self.dirty = true;
        Ok(())
    }

    pub fn extended_ink_bytes(&self) -> usize {
        self.ink.extended_bytes()
    }

    pub fn refresh_color(&mut self) -> bool {
        let revision = self.color.revision;
        self.color.refresh(&self.window, self.render.extended);
        if self.color.revision == revision {
            return false;
        }
        self.background.invalidate_texture();
        self.dirty = true;
        true
    }

    pub fn update_guard(&mut self, toolbar: &Window, mouse: bool) -> Result<(), String> {
        if self.guard_dirty || self.mouse != mouse {
            let (protected, shape_changed) =
                self.guard
                    .update(&self.window, toolbar, mouse, self.render.name != "software")?;
            if shape_changed {
                self.native.invalidate();
            }
            self.dirty |= self.protected != protected || shape_changed || self.mouse != mouse;
            self.protected = protected;
            self.guard_dirty = false;
            self.mouse = mouse;
        }
        Ok(())
    }

    pub fn apply_input(&mut self, mouse: bool) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let result = self.native.set_guarded(&self.window, mouse, self.protected);
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let result = self.native.set(&self.window, mouse);
        result
    }

    pub fn snapshot_mode(&self, video: &str, xwayland: bool, _force_sdr: bool) -> SnapshotMode {
        let extended = self.render.extended;
        #[cfg(target_os = "macos")]
        let extended = extended || (!_force_sdr && !self.extended_unavailable);
        background::snapshot_mode(self.window.hdr_enabled(), extended, video, xwayland)
    }

    pub fn update_background(
        &mut self,
        displays: &[Display],
        connected: &BTreeSet<u32>,
        mode: SnapshotMode,
        drawing: bool,
    ) -> Result<(), Text> {
        let display = displays.iter().find(|d| Some(d.id) == self.output);
        self.background.display(display, connected, mode, drawing)
    }

    pub fn paint(
        &mut self,
        app: &mut App,
        options: &Options,
        picker_choosing: bool,
        keep_editor_open: bool,
        theme: egui::Theme,
        extended_ink_budget: usize,
    ) -> Result<bool, String> {
        let text_repaint = self.text_repaint_at.is_some_and(|at| Instant::now() >= at);
        if !self.visible || !(options.smoke || self.dirty || self.animation || text_repaint) {
            return Ok(false);
        }
        let on_output = self.on_output(options.smoke);
        if picker_choosing || !on_output && driver() == "wayland" {
            self.render.begin(
                &self.window,
                [0., 0., 0., if picker_choosing { 1. / 255. } else { 0. }],
            )?;
            self.render.present()?;
        } else if on_output {
            app.on_page(self.output, |page| -> Result<(), String> {
                self.set_page_geometry(page);
                #[cfg(target_os = "macos")]
                if !options.sdr && !self.extended_unavailable {
                    let extended = (self.background.frozen && self.window.hdr_enabled())
                        || page.has_wide_ink();
                    if extended != self.render.extended {
                        page.input.text.refocus();
                        self.renderer_reset();
                        self.render.change_color_output(&self.window, extended)?;
                        self.extended_unavailable = extended && !self.render.extended;
                        self.color.refresh(&self.window, self.render.extended);
                        self.guard_dirty = true;
                        self.native.invalidate();
                    }
                }
                page.history.set_size(self.window.size());
                let scale = if page.hidden {
                    self.render.begin(&self.window, [0.; 4])?
                } else {
                    self.ink.paint(
                        page,
                        &self.render,
                        &self.window,
                        &self.color,
                        &mut self.background,
                        extended_ink_budget,
                    )?
                };
                if let Some(output) = self.text_frame(page, theme, keep_editor_open)
                    && let Some(gui) = self.text.as_mut()
                {
                    gui.paint(&self.render, &self.window, &self.color, output)?;
                }
                if page.hidden {
                    self.render.present()?;
                } else {
                    self.ink
                        .present(page, &self.render, &self.window, &self.color, scale)?;
                }
                self.animation = !page.input.laser.is_empty();
                Ok(())
            })?;
        } else {
            return Ok(false);
        }
        self.dirty = false;
        Ok(true)
    }

    fn text_frame(
        &mut self,
        app: &mut App,
        theme: egui::Theme,
        keep_editor_open: bool,
    ) -> Option<egui::FullOutput> {
        let screenshot = app.screenshot.targets(self.window.id);
        let interactive =
            !app.screenshot.active() && !app.picking && app.tool.is_text() && !app.pass;
        let editing = interactive && app.input.text.editing();
        if !app.touch_enabled {
            self.cancel_text_touches(app);
        }
        if app.history.text_boxes().is_empty() && !editing && !screenshot {
            self.stop_text_input(interactive);
            return None;
        }
        let style = (app.text_fonts.revision, self.color.revision);
        if self.text_style.is_some_and(|(_, color)| color != style.1) {
            self.suspend_text_input();
            self.text = None;
            app.input.text.refocus();
        }
        let gui = self.text.get_or_insert_with(Gui::new);
        if self.text_style != Some(style) {
            gui.ctx.set_fonts(app.text_fonts.definitions());
            self.text_style = Some(style);
        }
        if gui.ctx.theme() != theme {
            gui.ctx.set_theme(theme);
        }
        if editing {
            if self.window.input_focused() {
                gui.sync_text_input(&self.window, true);
            }
        } else if interactive || screenshot {
            gui.sync_text_input(&self.window, false);
        } else {
            gui.suspend_input(&self.window);
        }
        let input = gui.canvas_input(&self.window);
        let focused = input.focused;
        let composing = gui.composing();
        let mut output = gui.ctx.run_ui(input, |ui| {
            if !app.hidden {
                app.text_ui(ui, interactive, composing, keep_editor_open);
            }
            if screenshot {
                app.screenshot.ui(ui, app.language, app.input.canvas_units);
            }
        });
        self.text_repaint_at = gui::repaint_at(&output);
        if !focused {
            output.platform_output.ime = None;
        }
        if editing && !app.input.text.editing() {
            gui.sync_text_input(&self.window, false);
            output.platform_output.ime = None;
            self.text_repaint_at = Some(Instant::now());
        }
        if let Some(request) = app.input.text.take_keyboard_request()
            && app.input.text.editing()
        {
            self.focus_text_editor();
            if let Err(error) = crate::keyboard::request(&self.window, request) {
                app.status = text(
                    "Cannot open the screen keyboard",
                    "无法呼出屏幕键盘",
                    "無法開啟螢幕鍵盤",
                )
                .detail(error);
            }
        }
        Some(output)
    }

    fn stop_text_input(&mut self, interactive: bool) {
        if interactive {
            if let Some(gui) = &mut self.text {
                gui.sync_text_input(&self.window, false);
                gui.clear_events();
            }
            self.text_repaint_at = None;
        } else {
            self.suspend_text_input();
            self.text = None;
            self.text_style = None;
        }
    }
}

pub(super) fn active_index(canvases: &[Canvas], output: Option<u32>) -> usize {
    canvases
        .iter()
        .position(|canvas| canvas.output == output)
        .unwrap_or(0)
}

pub(super) fn reconcile(
    canvases: &mut Vec<Canvas>,
    app: &mut App,
    toolbar: &Window,
    options: &Options,
) -> Result<(), String> {
    if let Err(error) = reconcile_windows(canvases, app, toolbar, options) {
        if !app.multi {
            return Err(error);
        }
        app.finish_all_contacts();
        app.parked_input.clear();
        app.multi = false;
        app.status = text(
            "Using one canvas because a display window could not be opened",
            "有屏幕的画布无法打开，已切回单窗口",
            "有螢幕的畫布無法開啟，已切回單視窗",
        )
        .detail(error);
        reconcile_windows(canvases, app, toolbar, options)?;
    }
    Ok(())
}

fn reconcile_windows(
    canvases: &mut Vec<Canvas>,
    app: &App,
    toolbar: &Window,
    options: &Options,
) -> Result<(), String> {
    if app.multi && !app.displays.is_empty() {
        let mut added = Vec::new();
        for display in &app.displays {
            if !canvases
                .iter()
                .any(|canvas| canvas.output == Some(display.id))
            {
                let window = Window::new("OverInk - canvas", 1100, 720, true)?;
                added.push(Canvas::new(window, Some(display), options)?);
            }
        }
        canvases.retain(|canvas| app.displays.iter().any(|d| Some(d.id) == canvas.output));
        canvases.extend(added);
    } else {
        let index = active_index(canvases, app.pages.active);
        canvases.swap(0, index);
        canvases.truncate(1);
    }
    for canvas in canvases {
        let output = if app.multi {
            canvas.output
        } else {
            app.pages.active
        };
        canvas.attach(app.displays.iter().find(|d| Some(d.id) == output), toolbar)?;
    }
    Ok(())
}

pub(super) fn event_window(event: &SDL_Event, ty: SDL_EventType) -> Option<SDL_WindowID> {
    crate::sdl::event_window(event, ty)
}

pub(super) fn failure(error: impl ToString) -> crate::appearance::Status {
    text(
        "Drawing layer update failed",
        "更新书写图层失败",
        "更新書寫圖層失敗",
    )
    .detail(error)
}
