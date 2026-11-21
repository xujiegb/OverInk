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
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

/// Only measured when OVERINK_PROFILE is set. Separates layout, geometry
/// upload and Metal present from the ink compositor without per-frame logging.
#[derive(Default)]
pub(super) struct CanvasPaintTimings {
    pub ink: Duration,
    pub text_ui: Duration,
    pub text_geometry: Duration,
    pub static_text: Duration,
    pub present: Duration,
}

pub(super) struct PaintFlags {
    pub picker_choosing: bool,
    pub keep_editor_open: bool,
}

/// A redraw is not necessarily a document change. Pointer hover, native
/// surface changes and committed ink/text invalidate different GPU resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DirtyReason {
    Overlay,
    Content,
    Surface,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DirtyReasons(u8);

impl DirtyReasons {
    const OVERLAY: u8 = 1;
    const CONTENT: u8 = 2;
    const SURFACE: u8 = 4;

    fn mark(&mut self, reason: DirtyReason) {
        self.0 |= match reason {
            DirtyReason::Overlay => Self::OVERLAY,
            DirtyReason::Content => Self::CONTENT,
            DirtyReason::Surface => Self::SURFACE,
        };
    }

    fn any(self) -> bool {
        self.0 != 0
    }

    fn clear(&mut self) {
        self.0 = 0;
    }
}

// The old ink layer is already retained by Ink. Committed text gets a
// separate transparent GPU texture; live editors, menus, IME and OSK remain
// immediate-mode and are composited on top. The GUI owns its glyph atlas.
// Full-screen text targets waste tens of MiB for a few labels. Keep only
// the text's clipped pixel bounds, capped to 8 MiB per visible canvas.
const MAX_STATIC_TEXT_TEXTURE_BYTES: usize = 8 * 1024 * 1024;

/// The union of committed label rectangles, expressed as pixel-aligned
/// [x, y, width, height] in the original canvas. Empty/outside labels
/// don't allocate a render target.
fn static_text_region(
    rects: impl Iterator<Item = egui::Rect>,
    logical: [f32; 2],
    pixels: [i32; 2],
) -> Option<[i32; 4]> {
    if logical.iter().any(|v| !v.is_finite() || *v <= 0.)
        || pixels.iter().any(|v| *v <= 0)
    {
        return None;
    }
    let frame = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::from(logical));
    let union = rects
        .map(|rect| rect.intersect(frame))
        .filter(|rect| rect.width() > 0. && rect.height() > 0.)
        .reduce(|a, b| a.union(b))?;
    let scale = [pixels[0] as f32 / logical[0], pixels[1] as f32 / logical[1]];
    let x = (union.min.x * scale[0]).floor().clamp(0., pixels[0] as f32) as i32;
    let y = (union.min.y * scale[1]).floor().clamp(0., pixels[1] as f32) as i32;
    let right = (union.max.x * scale[0]).ceil().clamp(0., pixels[0] as f32) as i32;
    let bottom = (union.max.y * scale[1]).ceil().clamp(0., pixels[1] as f32) as i32;
    (right > x && bottom > y).then_some([x, y, right - x, bottom - y])
}

struct StaticTextLayer {
    gui: Gui,
    texture: InkTexture,
    region: [i32; 4],
    full_pixels: [i32; 2],
    logical_size: [f32; 2],
    format: SDL_PixelFormat,
    text_revision: u64,
    selected: Option<usize>,
    style: Option<(u64, u64)>,
}

pub(super) struct Canvas {
    // SDL destroys textures with their renderer, and renderers with their window.
    pub background: Background,
    ink: Ink,
    text: Option<Gui>,
    text_style: Option<(u64, u64)>,
    static_text: Option<StaticTextLayer>,
    static_text_unavailable: Option<([i32; 4], SDL_PixelFormat)>,
    pub text_repaint_at: Option<Instant>,
    pub render: Renderer,
    native: Passthrough,
    // Must drop before the SDL window; AppKit owns this view-attached monitor.
    #[cfg(target_os = "macos")]
    _direct_touch: Option<crate::native::macos::DirectTouchMonitor>,
    pub window: Window,
    pub color: ColorManager,
    guard: Guard,
    pub output: Option<u32>,
    bounds: Option<[i32; 4]>,
    pub protected: Option<Rect>,
    pub visible: bool,
    dirty: DirtyReasons,
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
        #[cfg(target_os = "macos")]
        let direct_touch = if options.smoke {
            None
        } else {
            match crate::native::macos::DirectTouchMonitor::install(&window) {
                Ok(monitor) => monitor,
                Err(error) => {
                    eprintln!("OverInk: {error}");
                    None
                }
            }
        };
        let mut color = ColorManager::default();
        color.refresh(&window, render.extended);
        Ok(Self {
            background: Background::default(),
            ink: Ink::default(),
            text: None,
            text_style: None,
            static_text: None,
            static_text_unavailable: None,
            text_repaint_at: None,
            render,
            native,
            #[cfg(target_os = "macos")]
            _direct_touch: direct_touch,
            window,
            color,
            guard: Guard::default(),
            output: display.map(|d| d.id),
            bounds: display.map(|d| d.bounds),
            protected: None,
            visible: false,
            dirty: DirtyReasons(DirtyReasons::SURFACE),
            guard_dirty: true,
            animation: false,
            mouse: false,
            #[cfg(target_os = "macos")]
            extended_unavailable: false,
        })
    }

    pub fn trim_idle_gpu_cache(&mut self) {
        self.ink.release_idle_extended_layer();
    }

    pub fn invalidate(&mut self, reason: DirtyReason) {
        self.dirty.mark(reason);
        if reason == DirtyReason::Surface {
            self.static_text = None;
            self.static_text_unavailable = None;
        }
    }

    pub fn invalidate_if(&mut self, changed: bool, reason: DirtyReason) {
        if changed {
            self.invalidate(reason);
        }
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
            self.invalidate(DirtyReason::Surface);
            self.native.invalidate();
            if !visible {
                self.ink.invalidate_layer();
                // Hidden canvases must not pin GPU targets or a second
                // egui glyph atlas until the window becomes visible again.
                self.static_text = None;
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
        app.input.canvas_units = gui::window_units(&self.window);
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
        self.invalidate(DirtyReason::Surface);
    }

    pub fn renderer_reset(&mut self) {
        // Release live SDL texture objects before rebuilding their contents
        // or destroying the old renderer.
        self.ink.reset_textures();
        self.suspend_text_input();
        // Dropping the old text GUI releases its SDL texture handles.
        self.text = None;
        self.text_style = None;
        self.static_text = None;
        self.static_text_unavailable = None;
        self.background.invalidate_texture();
        self.invalidate(DirtyReason::Surface);
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
            // Windows TabTip and macOS Keyboard Viewer are separate processes:
            // their activation can temporarily change SDL_WINDOW_INPUT_FOCUS.
            // Do not send SDL_StopTextInput on their FocusLost event. That
            // destroys the IME context and immediately collapses TabTip.
            if self.window.input_focused() || !app.input.text.screen_keyboard_session() {
                gui.sync_text_input(&self.window, self.window.input_focused());
            }
        }
        changed
    }

    /// Pending egui input must be replayed before any stable-hover short circuit.
    /// Otherwise the final cursor coordinate in a rapid motion batch can be lost.
    pub fn pending_screenshot_input(&self) -> bool {
        self.text
            .as_ref()
            .is_some_and(|gui| !gui.raw.events.is_empty())
    }

    pub fn screenshot_event(&mut self, event: &SDL_Event) -> bool {
        let gui = self.text.get_or_insert_with(Gui::new);
        // Adjacent mouse motions can replace the final egui event without
        // increasing the queue length. A new screenshot hover still requires
        // a frame even when the event count stays unchanged.
        let before = gui.raw.events.len();
        let previous_pos = gui.raw.events.last().and_then(|event| match event {
            egui::Event::PointerMoved(pos) => Some(*pos),
            _ => None,
        });
        gui.event(event, self.window.id, self.window.size());
        screenshot_input_changed(
            before,
            previous_pos,
            &gui.raw.events,
            event_type(event) == SDL_EVENT_MOUSE_MOTION,
        )
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
        self.invalidate(DirtyReason::Overlay);
    }

    pub fn flush_text_events(&mut self, app: &mut App, theme: egui::Theme) -> Result<(), String> {
        if app.input.text.editing()
            && self
                .text
                .as_ref()
                .is_some_and(|gui| !gui.raw.events.is_empty())
            && let Some(mut output) = self.text_frame(app, theme, true, false)
        {
            // Apply input and upload texture deltas before a toolbar action changes the draft.
            // The next canvas frame will paint the resulting document.
            output.shapes.clear();
            if let Some(gui) = self.text.as_mut() {
                gui.paint(&self.render, &self.window, &self.color, output)?;
                self.invalidate(DirtyReason::Overlay);
            }
        }
        Ok(())
    }

    pub fn finish_text_editor(&mut self, app: &mut App, theme: egui::Theme) -> Result<(), String> {
        self.flush_text_events(app, theme)?;
        app.input.text.finish(&mut app.history);
        self.suspend_text_input();
        self.invalidate(DirtyReason::Content);
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
        self.invalidate(DirtyReason::Surface);
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
            self.invalidate_if(
                self.protected != protected || shape_changed || self.mouse != mouse,
                DirtyReason::Overlay,
            );
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
        // An explicit --sdr request is also an opt-in to SDR desktop capture
        // on HDR Windows monitors, where a transparent SDL swap chain cannot
        // preserve the FP16 scRGB output. Do not silently discard HDR otherwise.
        let hdr = self.window.hdr_enabled() && !_force_sdr;
        background::snapshot_mode(hdr, extended, video, xwayland)
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

    /// Rebuild committed text only when its document, font, color or surface
    /// changes. Unlike the interactive egui context this one never receives
    /// pointer events, so hovering a keyboard does not re-tessellate labels.
    /// If an SDL backend cannot create a target, preserve the original path.
    fn paint_static_text(&mut self, app: &App, theme: egui::Theme) -> Result<bool, String> {
        let interactive =
            !app.screenshot.active() && !app.picking && app.tool.is_text() && !app.pass;
        let selected = if interactive {
            app.input.text.selected
        } else {
            None
        };
        if app.hidden || app.history.text_boxes().len() <= usize::from(selected.is_some()) {
            self.static_text = None;
            return Ok(false);
        }
        let full_pixels = self.window.pixel_size();
        let logical_size = self.window.size();
        let format = if self.render.extended {
            SDL_PIXELFORMAT_RGBA64_FLOAT
        } else {
            SDL_PIXELFORMAT_RGBA32
        };
        let style = (app.text_fonts.revision, self.color.revision);
        let revision = app.history.text_revision();
        let use_existing = self.static_text.as_ref().is_some_and(|layer| {
            layer.full_pixels == full_pixels
                && layer.logical_size == logical_size
                && layer.format == format
                && layer.style == Some(style)
                && layer.text_revision == revision
                && layer.selected == selected
                && layer.gui.ctx.theme() == theme
        });
        if use_existing {
            if let Some(layer) = self.static_text.as_ref() {
                let [x, y, w, h] = layer.region;
                let scale = [
                    full_pixels[0] as f32 / logical_size[0],
                    full_pixels[1] as f32 / logical_size[1],
                ];
                let destination = SDL_FRect {
                    x: x as f32 / scale[0],
                    y: y as f32 / scale[1],
                    w: w as f32 / scale[0],
                    h: h as f32 / scale[1],
                };
                layer.texture.draw(&self.render, None, Some(&destination))?;
                return Ok(true);
            }
        }
        let region = static_text_region(
            app.history
                .text_boxes()
                .iter()
                .enumerate()
                .filter(|(index, _)| Some(*index) != selected)
                .map(|(_, item)| item.text_rect()),
            logical_size,
            full_pixels,
        );
        let Some(region) = region else {
            self.static_text = None;
            return Ok(false);
        };
        let [x, y, width, height] = region;
        let bytes_per_pixel = if self.render.extended { 8 } else { 4 };
        let bytes = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .and_then(|(w, h)| w.checked_mul(h))
            .and_then(|area| area.checked_mul(bytes_per_pixel));
        if !bytes.is_some_and(|size| size <= MAX_STATIC_TEXT_TEXTURE_BYTES) {
            self.static_text = None;
            return Ok(false);
        }
        if self
            .static_text
            .as_ref()
            .is_some_and(|layer| {
                layer.region != region
                    || layer.full_pixels != full_pixels
                    || layer.logical_size != logical_size
                    || layer.format != format
            })
        {
            self.static_text = None;
        }
        let key = (region, format);
        if self.static_text.is_none() {
            if self.static_text_unavailable == Some(key) {
                return Ok(false);
            }
            let Ok(texture) = InkTexture::with_format(
                &self.render,
                [width, height],
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_BLEND_PREMULTIPLIED,
                format,
            ) else {
                self.static_text_unavailable = Some(key);
                return Ok(false);
            };
            self.static_text = Some(StaticTextLayer {
                gui: Gui::new(),
                texture,
                region,
                full_pixels,
                logical_size,
                format,
                text_revision: u64::MAX,
                selected: None,
                style: None,
            });
        }
        let Some(layer) = self.static_text.as_mut() else {
            return Ok(false);
        };
        if layer.style != Some(style) {
            layer.gui.ctx.set_fonts(app.text_fonts.definitions());
            layer.style = Some(style);
            layer.text_revision = u64::MAX;
        }
        if layer.gui.ctx.theme() != theme {
            layer.gui.ctx.set_theme(theme);
            layer.text_revision = u64::MAX;
        }
        if layer.text_revision != revision || layer.selected != selected {
            let input = layer.gui.canvas_input(&self.window);
            let output = layer
                .gui
                .ctx
                .run_ui(input, |ui| app.static_text_ui(ui, selected));
            let scale = [
                full_pixels[0] as f32 / logical_size[0],
                full_pixels[1] as f32 / logical_size[1],
            ];
            let origin = [x as f32 / scale[0], y as f32 / scale[1]];
            let paint = self.render.with_target(&layer.texture, || {
                self.render.set_render_scale(scale)?;
                self.render.try_set_clip_rect(None)?;
                self.render.clear([0.; 4])?;
                layer.gui.paint_region(
                    &self.render,
                    &self.window,
                    &self.color,
                    output,
                    origin,
                    [width, height],
                )
            });
            if paint.is_err() {
                self.static_text = None;
                self.static_text_unavailable = Some(key);
                return Ok(false);
            }
            layer.text_revision = revision;
            layer.selected = selected;
        }
        let scale = [
            full_pixels[0] as f32 / logical_size[0],
            full_pixels[1] as f32 / logical_size[1],
        ];
        let destination = SDL_FRect {
            x: x as f32 / scale[0],
            y: y as f32 / scale[1],
            w: width as f32 / scale[0],
            h: height as f32 / scale[1],
        };
        layer.texture.draw(&self.render, None, Some(&destination))?;
        Ok(true)
    }

    pub fn paint(
        &mut self,
        app: &mut App,
        options: &Options,
        flags: PaintFlags,
        theme: egui::Theme,
        extended_ink_budget: usize,
        mut timings: Option<&mut CanvasPaintTimings>,
    ) -> Result<bool, String> {
        let PaintFlags {
            picker_choosing,
            keep_editor_open,
        } = flags;
        let text_repaint = self.text_repaint_at.is_some_and(|at| Instant::now() >= at);
        if !self.visible || !(options.smoke || self.dirty.any() || self.animation || text_repaint) {
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
                let ink_started = timings.as_ref().map(|_| Instant::now());
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
                if let (Some(start), Some(metrics)) = (ink_started, timings.as_deref_mut()) {
                    metrics.ink += start.elapsed();
                }

                let static_started = timings.as_ref().map(|_| Instant::now());
                let cached_static_text = if page.hidden {
                    false
                } else {
                    self.paint_static_text(page, theme)?
                };
                if let (Some(start), Some(metrics)) = (static_started, timings.as_deref_mut()) {
                    metrics.static_text += start.elapsed();
                }
                let text_started = timings.as_ref().map(|_| Instant::now());
                let text_output =
                    self.text_frame(page, theme, keep_editor_open, cached_static_text);
                if let (Some(start), Some(metrics)) = (text_started, timings.as_deref_mut()) {
                    metrics.text_ui += start.elapsed();
                }
                let mesh_started = timings.as_ref().map(|_| Instant::now());
                if let Some(output) = text_output
                    && let Some(gui) = self.text.as_mut()
                {
                    gui.paint(&self.render, &self.window, &self.color, output)?;
                }
                if let (Some(start), Some(metrics)) = (mesh_started, timings.as_deref_mut()) {
                    metrics.text_geometry += start.elapsed();
                }
                let present_started = timings.as_ref().map(|_| Instant::now());
                if page.hidden {
                    self.render.present()?;
                } else {
                    self.ink
                        .present(page, &self.render, &self.window, &self.color, scale)?;
                }
                if let (Some(start), Some(metrics)) = (present_started, timings) {
                    metrics.present += start.elapsed();
                }
                self.animation = !page.input.laser.is_empty();
                Ok(())
            })?;
        } else {
            return Ok(false);
        }
        self.dirty.clear();
        Ok(true)
    }

    fn text_frame(
        &mut self,
        app: &mut App,
        theme: egui::Theme,
        keep_editor_open: bool,
        cached_static_text: bool,
    ) -> Option<egui::FullOutput> {
        let screenshot = app.screenshot.targets(self.window.id);
        let interactive =
            !app.screenshot.active() && !app.picking && app.tool.is_text() && !app.pass;
        let editing = interactive && app.input.text.editing();
        if !app.touch_enabled {
            self.cancel_text_touches(app);
        }
        if cached_static_text && !interactive && !screenshot {
            // Mouse/pass-through mode needs no interactive egui run at all:
            // the committed labels already live in the retained GPU layer.
            self.stop_text_input(false);
            return None;
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
        // egui normally surrenders TextEdit focus when the pointer clicks
        // outside its bounds. A virtual keyboard key is such a click, even
        // when the button itself is not focusable. Keep focus only while the
        // embedded keyboard is shown; text_ui already handles explicit
        // clicks outside the editor, and normal focus resumes when it closes.
        gui.preserve_embedded_keyboard_focus(editing && app.input.text.osk_visible());
        if editing {
            if self.window.input_focused() {
                gui.sync_text_input(&self.window, true);
            }
        } else if interactive || screenshot {
            gui.sync_text_input(&self.window, false);
        } else {
            gui.suspend_input(&self.window);
        }
        let mut input = gui.canvas_input(&self.window);
        if editing && app.input.text.screen_keyboard_session() {
            // The separate keyboard's window does not own the egui editor.
            // Keep its focus/IME state while native key/text events are
            // delivered to our SDL canvas. Real pointer clicks outside the
            // text still finish editing via text_ui's hit test.
            input.focused = true;
            input
                .events
                .retain(|event| !matches!(event, egui::Event::WindowFocused(false)));
        }
        let focused = input.focused;
        let composing = gui.composing();
        let mut output = gui.ctx.run_ui(input, |ui| {
            if !app.hidden {
                app.text_ui(
                    ui,
                    interactive,
                    composing,
                    keep_editor_open,
                    cached_static_text,
                );
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

// Replacing a queued SDL mouse move must still mark the screenshot UI dirty.
// A queue-length-only check would swallow rapid transitions across buttons.
fn screenshot_input_changed(
    before: usize,
    previous_pos: Option<egui::Pos2>,
    events: &[egui::Event],
    mouse_motion: bool,
) -> bool {
    events.len() != before
        || (mouse_motion
            && previous_pos.is_some_and(|pos| {
                matches!(events.last(), Some(egui::Event::PointerMoved(current)) if *current != pos)
            }))
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
        app.status =
            text("Using one canvas because a display window could not be opened").detail(error);
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
    text("Drawing layer update failed").detail(error)
}

#[cfg(test)]
mod screenshot_input_tests {
    use super::{DirtyReason, DirtyReasons, screenshot_input_changed, static_text_region};
    use egui::{Event, Rect, pos2, vec2};

    #[test]
    fn static_text_uses_pixel_aligned_bounds_instead_of_full_canvas() {
        let rect = Rect::from_min_size(pos2(11.2, 20.2), vec2(150.5, 70.5));
        assert_eq!(
            static_text_region(std::iter::once(rect), [1000., 600.], [2000, 1200]),
            Some([22, 40, 302, 142])
        );
        assert_eq!(
            static_text_region(
                [rect, Rect::from_min_size(pos2(400., 200.), vec2(30., 40.))].into_iter(),
                [1000., 600.],
                [2000, 1200],
            ),
            Some([22, 40, 838, 440])
        );
        assert!(static_text_region(
            std::iter::once(Rect::from_min_size(pos2(1100., 800.), vec2(10., 10.))),
            [1000., 600.],
            [2000, 1200],
        ).is_none());
    }

    #[test]
    fn overlay_dirt_does_not_implicitly_dirty_content_or_surface() {
        let mut flags = DirtyReasons::default();
        assert!(!flags.any());
        flags.mark(DirtyReason::Overlay);
        assert_eq!(flags.0, DirtyReasons::OVERLAY);
        flags.mark(DirtyReason::Content);
        assert_eq!(flags.0, DirtyReasons::OVERLAY | DirtyReasons::CONTENT);
        flags.mark(DirtyReason::Surface);
        assert_eq!(
            flags.0,
            DirtyReasons::OVERLAY | DirtyReasons::CONTENT | DirtyReasons::SURFACE
        );
        flags.clear();
        assert!(!flags.any());
    }

    #[test]
    fn coalesced_screenshot_mouse_move_still_repaints() {
        let events = [Event::PointerMoved(pos2(22., 30.))];
        assert!(screenshot_input_changed(
            1,
            Some(pos2(10., 30.)),
            &events,
            true
        ));
        assert!(!screenshot_input_changed(
            1,
            Some(pos2(22., 30.)),
            &events,
            true
        ));
        assert!(!screenshot_input_changed(
            1,
            Some(pos2(10., 30.)),
            &events,
            false
        ));
        assert!(screenshot_input_changed(0, None, &events, false));
    }
}
