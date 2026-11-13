use super::{App, Options, ink::Ink};
use crate::{
    appearance::{Text, text},
    background::{self, Background, SnapshotMode},
    controller::{Guard, Rect},
    display::Display,
    icc::ColorManager,
    native::{self, Passthrough},
    sdl::*,
};
use std::collections::BTreeSet;

pub(super) struct Canvas {
    // SDL destroys textures with their renderer, and renderers with their window.
    pub background: Background,
    ink: Ink,
    pub render: Renderer,
    pub window: Window,
    native: Passthrough,
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
            render,
            window,
            native,
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
            if visible {
                if let Some(id) = self.output
                    && let Some([x, y, w, h]) = self.bounds
                    && driver() != "wayland"
                {
                    let (mut actual_x, mut actual_y) = (0, 0);
                    let positioned = unsafe {
                        SDL_GetWindowPosition(self.window.raw, &mut actual_x, &mut actual_y)
                    };
                    if !positioned
                        || [actual_x, actual_y] != [x, y]
                        || self.window.size() != [w as f32, h as f32]
                    {
                        self.window.place_on_display(id, true).map_err(|error| {
                            format!("Positioning the drawing canvas failed ({error})")
                        })?;
                    }
                }
                unsafe { SDL_SetWindowAlwaysOnTop(self.window.raw, true) };
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
        app.input.canvas_units =
            self.window.scale() / unsafe { SDL_GetWindowPixelDensity(self.window.raw) }.max(0.1);
    }

    pub fn keep_toolbar_above(&self, toolbar: &Window) {
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
        self.ink.reset_textures();
        self.background.invalidate_texture();
        self.dirty = true;
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
        wayland: bool,
        extended_ink_budget: usize,
    ) -> Result<bool, String> {
        if !self.visible || !(options.smoke || self.dirty || self.animation) {
            return Ok(false);
        }
        if picker_choosing || wayland && !self.on_output(options.smoke) {
            self.render.begin(
                &self.window,
                [0., 0., 0., if picker_choosing { 1. / 255. } else { 0. }],
            )?;
            self.render.present()?;
        } else if self.on_output(options.smoke) {
            app.on_page(self.output, |page| -> Result<(), String> {
                self.set_page_geometry(page);
                #[cfg(target_os = "macos")]
                if !options.sdr && !self.extended_unavailable {
                    let extended = (self.background.frozen && self.window.hdr_enabled())
                        || page.has_wide_ink();
                    if extended != self.render.extended {
                        self.renderer_reset();
                        self.render.change_color_output(&self.window, extended)?;
                        self.extended_unavailable = extended && !self.render.extended;
                        self.color.refresh(&self.window, self.render.extended);
                        self.guard_dirty = true;
                        self.native.invalidate();
                    }
                }
                page.history.doc.size = self.window.size();
                self.ink.paint(
                    page,
                    &self.render,
                    &self.window,
                    &self.color,
                    &mut self.background,
                    extended_ink_budget,
                )?;
                self.animation = !page.input.laser.is_empty();
                Ok(())
            })?;
        } else {
            return Ok(false);
        }
        self.dirty = false;
        Ok(true)
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
    unsafe {
        let window = match ty {
            SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => event.button.windowID,
            SDL_EVENT_MOUSE_MOTION => event.motion.windowID,
            SDL_EVENT_FINGER_DOWN
            | SDL_EVENT_FINGER_MOTION
            | SDL_EVENT_FINGER_UP
            | SDL_EVENT_FINGER_CANCELED => event.tfinger.windowID,
            SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_PROXIMITY_OUT => event.pproximity.windowID,
            SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => event.ptouch.windowID,
            SDL_EVENT_PEN_MOTION => event.pmotion.windowID,
            SDL_EVENT_PEN_AXIS => event.paxis.windowID,
            SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP => event.key.windowID,
            SDL_EVENT_WINDOW_FOCUS_LOST | SDL_EVENT_WINDOW_MOUSE_LEAVE => event.window.windowID,
            _ => return None,
        };
        (window.0 != 0).then_some(window)
    }
}

pub(super) fn failure(error: impl ToString) -> crate::appearance::Status {
    text(
        "Drawing layer update failed",
        "更新书写图层失败",
        "更新書寫圖層失敗",
    )
    .detail(error)
}
