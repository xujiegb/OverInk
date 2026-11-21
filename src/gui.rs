//! Small egui/SDL3 integration. SDL_RenderGeometry makes the same UI usable on
//! s390x, LoongArch and FreeBSD without assuming a Vulkan driver.
#![forbid(unsafe_code)]
use crate::{
    icc::ColorManager,
    sdl::*,
    touch::{self, TouchKey},
};
use egui::{Context, Event, Key, Modifiers, RawInput, TextureId};
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};
/// The single source of application UI scaling, read from the owning SDL
/// window. Full-size panels use it as the egui coordinate conversion;
/// transparent annotation canvases keep SDL coordinates and pass this same
/// value to their overlay layout. Do not independently query DPI in widgets.
pub(crate) fn window_units(window: &Window) -> f32 {
    window.ui_scale().max(0.5)
}

pub struct Gui {
    pub ctx: Context,
    pub raw: RawInput,
    textures: HashMap<TextureId, GuiTexture>,
    // Reuse conversion buffers across egui meshes and atlas updates.
    vertices: Vec<SDL_Vertex>,
    pixels: Vec<u8>,
    // Do not coalesce a real mouse event with a touch-generated pointer move.
    last_mouse_motion: bool,
    started: Instant,
    pub units: f32,
    touch_primary: Option<TouchKey>,
    touch_positions: BTreeMap<TouchKey, egui::Pos2>,
    composing: bool,
    composition_ended: Option<u64>,
}
impl Gui {
    pub fn new() -> Self {
        let ctx = Context::default();
        // Apply input actions once per native frame, including mode toggles.
        ctx.options_mut(|options| options.max_passes = std::num::NonZeroUsize::MIN);
        ctx.set_theme(egui::Theme::Light);
        ctx.set_fonts(crate::fonts::ui_definitions());
        ctx.all_styles_mut(|s| {
            let colors = crate::toolbar::colors(s.visuals.dark_mode);
            s.visuals.panel_fill = colors.rail;
            s.visuals.window_fill = colors.panel;
            s.visuals.window_corner_radius = egui::CornerRadius::same(15);
            s.visuals.menu_corner_radius = egui::CornerRadius::same(11);
            if s.visuals.dark_mode {
                s.visuals.widgets.noninteractive.fg_stroke.color =
                    egui::Color32::from_rgb(192, 207, 223);
                s.visuals.weak_text_color = Some(egui::Color32::from_rgb(155, 174, 192));
            } else {
                s.visuals.selection.bg_fill = egui::Color32::from_rgb(208, 236, 249);
                s.visuals.widgets.active.bg_fill = s.visuals.selection.bg_fill;
            }
            s.spacing.item_spacing = egui::vec2(6., 5.);
            s.spacing.button_padding = egui::vec2(10., 7.);
        });
        Self {
            ctx,
            raw: RawInput::default(),
            textures: HashMap::new(),
            vertices: Vec::new(),
            pixels: Vec::new(),
            last_mouse_motion: false,
            started: Instant::now(),
            units: 1.,
            touch_primary: None,
            touch_positions: BTreeMap::new(),
            composing: false,
            composition_ended: None,
        }
    }
    pub fn input(&mut self, w: &Window) -> RawInput {
        self.input_with_units(w, window_units(w))
    }

    pub fn canvas_input(&mut self, w: &Window) -> RawInput {
        self.input_with_units(w, 1.)
    }

    pub fn suspend_input(&mut self, window: &Window) {
        self.sync_text_input(window, false);
        self.cancel_touches();
        self.raw.events.push(Event::PointerGone);
        self.raw.events.retain(|event| {
            matches!(
                event,
                Event::PointerGone
                    | Event::PointerMoved(_)
                    | Event::PointerButton { pressed: false, .. }
            )
        });
        self.ctx.stop_dragging();
    }

    pub fn has_touch(&self, key: TouchKey) -> bool {
        self.touch_positions.contains_key(&key)
    }

    pub fn primary_touch(&self) -> Option<TouchKey> {
        self.touch_primary
    }

    pub fn sync_text_input(&mut self, window: &Window, enabled: bool) {
        window.sync_text_input(enabled);
        if !enabled {
            self.composing = false;
            self.composition_ended = None;
        }
    }

    pub(crate) fn input_with_units(&mut self, w: &Window, units: f32) -> RawInput {
        let logical = w.size();
        self.units = units;
        self.raw.focused = w.input_focused();
        self.raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(logical[0] / self.units, logical[1] / self.units),
        ));
        self.raw.time = Some(self.started.elapsed().as_secs_f64());
        let current = modifiers(mod_state());
        // A synthetic input event would ask egui for another frame.
        if current != self.ctx.input(|input| input.modifiers) {
            self.raw.events.push(Event::ModifiersChanged(current));
        }
        self.raw
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(w.pixel_density() * units);
        self.raw.take()
    }

    /// The embedded keyboard is part of the active annotation editor, not
    /// a separate text field. When visible, clicks on its keys must not make
    /// egui surrender TextEdit focus and abort native IME composition.
    pub fn preserve_embedded_keyboard_focus(&self, enabled: bool) {
        self.ctx.options_mut(|options| {
            options.input_options.surrender_focus_on = if enabled {
                egui::SurrenderFocusOn::Never
            } else {
                egui::SurrenderFocusOn::Clicks
            };
        });
    }

    pub fn clear_events(&mut self) {
        self.composition_ended = None;
        self.raw.events.clear();
        self.last_mouse_motion = false;
    }

    pub fn composing(&self) -> bool {
        self.composing
    }

    pub fn clear_composition(&mut self, window: &Window) {
        window.clear_composition();
        self.cancel_composition(0);
    }

    fn cancel_composition(&mut self, timestamp: u64) {
        if self.end_composition(timestamp) {
            self.raw
                .events
                .push(Event::Ime(egui::ImeEvent::Commit(String::new())));
        }
    }

    fn end_composition(&mut self, timestamp: u64) -> bool {
        let was_composing = std::mem::take(&mut self.composing);
        if was_composing {
            self.composition_ended = (timestamp != 0).then_some(timestamp);
        }
        was_composing
    }
    // egui consumes the final hover position per UI frame. SDL can queue
    // hundreds of motion samples between paints; preserve clicks, wheel and
    // touch events, but replace an adjacent stale mouse position in place.
    pub fn can_merge_mouse_position(&self) -> bool {
        self.last_mouse_motion && matches!(self.raw.events.last(), Some(Event::PointerMoved(_)))
    }

    fn queue_mouse_position(&mut self, pos: egui::Pos2) {
        if self.can_merge_mouse_position()
            && let Some(Event::PointerMoved(last)) = self.raw.events.last_mut()
        {
            *last = pos;
        } else {
            self.raw.events.push(Event::PointerMoved(pos));
        }
        self.last_mouse_motion = true;
    }

    /// Decode tagged SDL events through the checked, owning SDL API.
    pub fn event(&mut self, e: &SDL_Event, id: SDL_WindowID, size: [f32; 2]) {
        let ty = event_type(e);
        let scale = self.units;
        if ty != SDL_EVENT_MOUSE_MOTION {
            self.last_mouse_motion = false;
        }
        match ty {
            SDL_EVENT_MOUSE_MOTION => {
                if let Some(m) = mouse_motion_event(e)
                    && m.windowID == id
                    && m.which != SDL_TOUCH_MOUSEID
                    && m.which != SDL_PEN_MOUSEID
                {
                    self.queue_mouse_position(egui::pos2(m.x / scale, m.y / scale));
                }
            }
            SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP => {
                if let Some(b) = mouse_button_event(e)
                    && b.windowID == id
                    && b.which != SDL_TOUCH_MOUSEID
                    && b.which != SDL_PEN_MOUSEID
                {
                    let button = match b.button {
                        1 => egui::PointerButton::Primary,
                        2 => egui::PointerButton::Middle,
                        3 => egui::PointerButton::Secondary,
                        _ => return,
                    };
                    self.raw.events.push(Event::PointerButton {
                        pos: egui::pos2(b.x / scale, b.y / scale),
                        button,
                        pressed: b.down,
                        modifiers: modifiers(mod_state()),
                    });
                }
            }
            SDL_EVENT_FINGER_DOWN | SDL_EVENT_FINGER_MOTION | SDL_EVENT_FINGER_UP => {
                if let Some(finger) = finger_event(e)
                    && finger.windowID == id
                {
                    self.touch_event(finger, size);
                }
            }
            SDL_EVENT_FINGER_CANCELED => {
                if let Some(finger) = finger_event(e)
                    && self.has_touch(TouchKey::from(finger))
                {
                    self.touch_event(finger, size);
                }
            }
            SDL_EVENT_PEN_MOTION => {
                if let Some(pen) = pen_motion_event(e)
                    && pen.windowID == id
                {
                    self.raw.events.push(Event::PointerMoved(egui::pos2(
                        pen.x / scale,
                        pen.y / scale,
                    )));
                }
            }
            SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP => {
                if let Some(p) = pen_touch_event(e)
                    && p.windowID == id
                    && !p.eraser
                {
                    self.raw.events.push(Event::PointerButton {
                        pos: egui::pos2(p.x / scale, p.y / scale),
                        button: egui::PointerButton::Primary,
                        pressed: p.down,
                        modifiers: modifiers(mod_state()),
                    });
                }
            }
            SDL_EVENT_MOUSE_WHEEL => {
                if let Some(w) = mouse_wheel_event(e)
                    && w.windowID == id
                {
                    // SDL already includes the OS natural-scrolling preference
                    // in x/y. FLIPPED describes that preference, not a second
                    // conversion to apply before forwarding to egui.
                    self.raw.events.push(Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(w.x, w.y),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        modifiers: modifiers(mod_state()),
                    });
                }
            }
            SDL_EVENT_WINDOW_FOCUS_LOST if window_event_id(e) == Some(id) => {
                self.cancel_touches();
                self.cancel_composition(0);
                self.composition_ended = None;
                self.raw.focused = false;
                self.raw.events.push(Event::WindowFocused(false));
            }
            SDL_EVENT_WINDOW_FOCUS_GAINED if window_event_id(e) == Some(id) => {
                self.raw.focused = true;
                self.raw.events.push(Event::WindowFocused(true));
            }
            SDL_EVENT_WINDOW_MOUSE_LEAVE if window_event_id(e) == Some(id) => {
                self.raw.events.push(Event::PointerGone);
            }
            SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP => {
                let Some(k) = key_event(e).filter(|key| key.windowID == id) else {
                    return;
                };
                let m = modifiers(k.r#mod);
                let same_ime_key = k.down
                    && self
                        .composition_ended
                        .take()
                        .is_some_and(|at| k.timestamp != 0 && k.timestamp.abs_diff(at) < 2_000_000);
                // Some backends deliver the composition and its initiating key together.
                if self.composing
                    || (same_ime_key
                        && !m.command
                        && !m.alt
                        && matches!(
                            k.key,
                            SDLK_BACKSPACE | SDLK_ESCAPE | SDLK_RETURN | SDLK_KP_ENTER
                        ))
                {
                    return;
                }
                if k.down && m.command && k.key == SDLK_V {
                    if let Some(text) = clipboard_text() {
                        self.raw.events.push(Event::Paste(text));
                    }
                    return;
                }
                if k.down && m.command && k.key == SDLK_C {
                    self.raw.events.push(Event::Copy);
                    return;
                }
                if k.down && m.command && k.key == SDLK_X {
                    self.raw.events.push(Event::Cut);
                    return;
                }
                if let Some(key) = key(k.key) {
                    self.raw.events.push(Event::Key {
                        key,
                        physical_key: None,
                        pressed: k.down,
                        repeat: k.repeat,
                        modifiers: m,
                    });
                }
            }
            SDL_EVENT_TEXT_INPUT => {
                if let Some((text, timestamp)) = text_input_data(e, id) {
                    let event = if self.end_composition(timestamp) {
                        Event::Ime(egui::ImeEvent::Commit(text))
                    } else {
                        Event::Text(text)
                    };
                    self.raw.events.push(event);
                }
            }
            SDL_EVENT_TEXT_EDITING => {
                let Some(edit) = text_editing_data(e, id) else {
                    return;
                };
                let text = edit.text;
                if text.is_empty() {
                    self.cancel_composition(edit.timestamp);
                    return;
                }
                self.composing = true;
                self.composition_ended = None;
                let active_range_chars = if edit.start >= 0 && edit.length >= 0 {
                    let count = text.chars().count();
                    let start = (edit.start as usize).min(count);
                    let end = (start + edit.length as usize).min(count);
                    Some(start..end)
                } else {
                    None
                };
                self.raw.events.push(Event::Ime(egui::ImeEvent::Preedit {
                    text,
                    active_range_chars,
                }));
            }
            _ => {}
        }
    }
    fn touch_event(&mut self, event: SDL_TouchFingerEvent, size: [f32; 2]) {
        let key = TouchKey::from(event);
        let phase = match event.r#type {
            SDL_EVENT_FINGER_DOWN => egui::TouchPhase::Start,
            SDL_EVENT_FINGER_MOTION => egui::TouchPhase::Move,
            SDL_EVENT_FINGER_UP => egui::TouchPhase::End,
            _ => egui::TouchPhase::Cancel,
        };
        if phase == egui::TouchPhase::Start {
            if touch::is_indirect(event.touchID) || self.touch_positions.contains_key(&key) {
                return;
            }
            let Some(point) = touch::point(event, size) else {
                return;
            };
            let pos = egui::pos2(point.x / self.units, point.y / self.units);
            self.touch_positions.insert(key, pos);
            if self.touch_primary.is_none() {
                self.touch_primary = Some(key);
            }
        }
        let Some(previous) = self.touch_positions.get_mut(&key) else {
            return;
        };
        let point = touch::point(event, size);
        let pos = point.map_or(*previous, |p| {
            egui::pos2(p.x / self.units, p.y / self.units)
        });
        *previous = pos;
        self.raw.events.push(Event::Touch {
            device_id: egui::TouchDeviceId(key.device),
            id: egui::TouchId(key.finger),
            phase,
            pos,
            force: if event.pressure > 0. && event.pressure.is_finite() {
                Some(event.pressure.clamp(0., 1.))
            } else {
                None
            },
        });
        if self.touch_primary == Some(key) {
            if phase == egui::TouchPhase::Cancel {
                // Move away before releasing; an OS cancellation must not click a button.
                self.ctx.stop_dragging();
                self.raw
                    .events
                    .push(Event::PointerMoved(egui::pos2(-10000., -10000.)));
                self.raw.events.push(Event::PointerButton {
                    pos: egui::pos2(-10000., -10000.),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                });
            } else {
                self.raw.events.push(Event::PointerMoved(pos));
                if phase == egui::TouchPhase::Start || phase == egui::TouchPhase::End {
                    self.raw.events.push(Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: phase == egui::TouchPhase::Start,
                        modifiers: egui::Modifiers::default(),
                    });
                }
            }
            if phase == egui::TouchPhase::End || phase == egui::TouchPhase::Cancel {
                self.raw.events.push(Event::PointerGone);
                self.touch_primary = None;
            }
        }
        if phase == egui::TouchPhase::End || phase == egui::TouchPhase::Cancel {
            self.touch_positions.remove(&key);
        }
    }
    pub fn cancel_touches(&mut self) {
        for (key, pos) in std::mem::take(&mut self.touch_positions) {
            self.raw.events.push(Event::Touch {
                device_id: egui::TouchDeviceId(key.device),
                id: egui::TouchId(key.finger),
                phase: egui::TouchPhase::Cancel,
                pos,
                force: None,
            });
        }
        if self.touch_primary.take().is_some() {
            self.ctx.stop_dragging();
            self.raw
                .events
                .push(Event::PointerMoved(egui::pos2(-10000., -10000.)));
            self.raw.events.push(Event::PointerButton {
                pos: egui::pos2(-10000., -10000.),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
            self.raw.events.push(Event::PointerGone);
        }
    }
    pub fn paint(
        &mut self,
        r: &Renderer,
        window: &Window,
        color: &ColorManager,
        mut output: egui::FullOutput,
    ) -> Result<(), String> {
        if let Some(ime) = output.platform_output.ime {
            let cursor = ime.cursor_rect;
            let area = SDL_Rect {
                x: (cursor.left() * self.units).round() as i32,
                y: (cursor.top() * self.units).round() as i32,
                w: 1,
                h: (cursor.height() * self.units).ceil() as i32,
            };
            window.set_text_input_area(&area);
            if ime.should_interrupt_composition {
                self.clear_composition(window);
            }
        }
        for (id, deltas) in output.textures_delta.set.drain() {
            for delta in deltas {
                let egui::ImageData::Color(image) = delta.image;
                let [w, h] = image.size;
                self.pixels.clear();
                self.pixels
                    .extend(image.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()));
                if id == TextureId::default() && !color.is_identity() {
                    // The font atlas carries the color of emoji, while
                    // monochrome glyphs are tinted with mapped vertices.
                    for (rgba, pixel) in self
                        .pixels
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(image.pixels.iter())
                    {
                        let [red, green, blue, _] = *rgba;
                        if pixel.a() != 0 && [red, green, blue] != [255; 3] {
                            let rgb = color
                                .map([red, green, blue].map(|channel| f32::from(channel) / 255.));
                            rgba[..3].copy_from_slice(
                                &rgb.map(|channel| (channel * 255.).round() as u8),
                            );
                        }
                    }
                }
                if delta.pos.is_none() {
                    self.textures.insert(id, GuiTexture::new(r, w, h)?);
                }
                let texture = self
                    .textures
                    .get(&id)
                    .ok_or("Missing partial egui texture")?;
                let rect = delta.pos.map(|pos| SDL_Rect {
                    x: pos[0] as i32,
                    y: pos[1] as i32,
                    w: w as i32,
                    h: h as i32,
                });
                texture.update([w, h], &self.pixels, rect.as_ref())?;
            }
        }
        let px = window.pixel_size();
        let logical = window.size();
        let scale = [px[0] as f32 / logical[0], px[1] as f32 / logical[1]];
        for p in self.ctx.tessellate(output.shapes, output.pixels_per_point) {
            let egui::epaint::Primitive::Mesh(mesh) = p.primitive else {
                continue;
            };
            let Some(texture) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let rect = p.clip_rect;
            let x = (rect.min.x * self.units * scale[0])
                .floor()
                .clamp(0., px[0] as f32) as i32;
            let y = (rect.min.y * self.units * scale[1])
                .floor()
                .clamp(0., px[1] as f32) as i32;
            let x2 = (rect.max.x * self.units * scale[0])
                .ceil()
                .clamp(0., px[0] as f32) as i32;
            let y2 = (rect.max.y * self.units * scale[1])
                .ceil()
                .clamp(0., px[1] as f32) as i32;
            if x2 <= x || y2 <= y {
                continue;
            }
            // SDL clip rectangles are in renderer logical units, before RenderScale.
            let clip = SDL_Rect {
                x: (x as f32 / scale[0]).floor() as i32,
                y: (y as f32 / scale[1]).floor() as i32,
                w: ((x2 - x) as f32 / scale[0]).ceil() as i32,
                h: ((y2 - y) as f32 / scale[1]).ceil() as i32,
            };
            r.set_clip_rect(Some(&clip));
            self.vertices.clear();
            self.vertices.extend(mesh.vertices.iter().map(|v| {
                let [red, green, blue, alpha] = v.color.to_srgba_unmultiplied();
                let c = color.map([red as f32 / 255., green as f32 / 255., blue as f32 / 255.]);
                SDL_Vertex {
                    position: SDL_FPoint {
                        x: v.pos.x * self.units,
                        y: v.pos.y * self.units,
                    },
                    color: SDL_FColor {
                        r: c[0],
                        g: c[1],
                        b: c[2],
                        a: alpha as f32 / 255.,
                    },
                    tex_coord: SDL_FPoint {
                        x: v.uv.x,
                        y: v.uv.y,
                    },
                }
            }));
            texture.geometry(r, &self.vertices, &mesh.indices)?;
        }
        r.set_clip_rect(None);
        for id in output.textures_delta.free.drain() {
            self.textures.remove(&id);
        }
        for command in output.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                set_clipboard_text(&text);
            }
        }
        Ok(())
    }
}

pub fn repaint_at(output: &egui::FullOutput) -> Option<Instant> {
    output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .and_then(|viewport| {
            // A zero delay is used for the one follow-up frame after input.
            Instant::now().checked_add(viewport.repaint_delay.max(Duration::from_millis(1)))
        })
}

pub fn modifiers(m: SDL_Keymod) -> Modifiers {
    let ctrl = m & SDL_KMOD_CTRL != 0;
    let mac_cmd = m & SDL_KMOD_GUI != 0;
    Modifiers {
        alt: m & SDL_KMOD_ALT != 0,
        ctrl,
        shift: m & SDL_KMOD_SHIFT != 0,
        mac_cmd,
        command: if cfg!(target_os = "macos") {
            mac_cmd
        } else {
            ctrl
        },
    }
}
pub fn key(k: SDL_Keycode) -> Option<Key> {
    Some(match k {
        SDLK_ESCAPE => Key::Escape,
        SDLK_TAB => Key::Tab,
        SDLK_BACKSPACE => Key::Backspace,
        SDLK_RETURN | SDLK_KP_ENTER => Key::Enter,
        SDLK_SPACE => Key::Space,
        SDLK_DELETE => Key::Delete,
        SDLK_LEFT => Key::ArrowLeft,
        SDLK_RIGHT => Key::ArrowRight,
        SDLK_UP => Key::ArrowUp,
        SDLK_DOWN => Key::ArrowDown,
        SDLK_HOME => Key::Home,
        SDLK_END => Key::End,
        SDLK_A => Key::A,
        SDLK_B => Key::B,
        SDLK_C => Key::C,
        SDLK_D => Key::D,
        SDLK_E => Key::E,
        SDLK_F => Key::F,
        SDLK_G => Key::G,
        SDLK_H => Key::H,
        SDLK_I => Key::I,
        SDLK_J => Key::J,
        SDLK_K => Key::K,
        SDLK_L => Key::L,
        SDLK_M => Key::M,
        SDLK_N => Key::N,
        SDLK_O => Key::O,
        SDLK_P => Key::P,
        SDLK_Q => Key::Q,
        SDLK_R => Key::R,
        SDLK_S => Key::S,
        SDLK_T => Key::T,
        SDLK_U => Key::U,
        SDLK_V => Key::V,
        SDLK_W => Key::W,
        SDLK_X => Key::X,
        SDLK_Y => Key::Y,
        SDLK_Z => Key::Z,
        SDLK_1 => Key::Num1,
        SDLK_2 => Key::Num2,
        SDLK_3 => Key::Num3,
        SDLK_4 => Key::Num4,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn included_ui_font_covers_both_chinese_scripts() {
        let gui = Gui::new();
        for sample in [
            "简体中文 屏幕取色 退出 安装扩展 重新登录",
            "繁體中文 螢幕取色 結束 安裝擴充套件 重新登入",
        ] {
            let mut output = gui.ctx.run_ui(RawInput::default(), |ui| {
                ui.label(sample);
                ui.ctx().fonts_mut(|fonts| {
                    let font = egui::FontId::proportional(14.);
                    assert!(
                        sample
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .all(|c| fonts.has_glyph(&font, c))
                    );
                });
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn adjacent_mouse_motion_coalesces_without_crossing_clicks() {
        let mut gui = Gui::new();
        let id = SDL_WindowID(1);
        let motion = |x| SDL_Event {
            motion: SDL_MouseMotionEvent {
                r#type: SDL_EVENT_MOUSE_MOTION,
                windowID: id,
                x,
                y: 40.,
                ..Default::default()
            },
        };
        for x in [10., 20., 30.] {
            gui.event(&motion(x), id, [800., 600.]);
        }
        assert!(matches!(
            gui.raw.events.as_slice(),
            [Event::PointerMoved(pos)] if *pos == egui::pos2(30., 40.)
        ));
        gui.event(
            &SDL_Event {
                button: SDL_MouseButtonEvent {
                    r#type: SDL_EVENT_MOUSE_BUTTON_DOWN,
                    windowID: id,
                    button: SDL_BUTTON_LEFT as u8,
                    x: 30.,
                    y: 40.,
                    down: true,
                    ..Default::default()
                },
            },
            id,
            [800., 600.],
        );
        gui.event(&motion(50.), id, [800., 600.]);
        assert!(matches!(
            gui.raw.events.as_slice(),
            [
                Event::PointerMoved(first),
                Event::PointerButton { pressed: true, .. },
                Event::PointerMoved(last)
            ]
                if *first == egui::pos2(30., 40.) && *last == egui::pos2(50., 40.)
        ));
    }

    #[test]
    fn mouse_wheel_respects_system_natural_scroll_direction() {
        let mut gui = Gui::new();
        let window = SDL_WindowID(1);
        for (direction, expected) in [
            (SDL_MOUSEWHEEL_NORMAL, egui::vec2(1.25, -2.5)),
            (SDL_MOUSEWHEEL_FLIPPED, egui::vec2(-3.5, 2.25)),
        ] {
            let wheel = SDL_Event {
                wheel: SDL_MouseWheelEvent {
                    r#type: SDL_EVENT_MOUSE_WHEEL,
                    windowID: window,
                    x: expected.x,
                    y: expected.y,
                    direction,
                    ..Default::default()
                },
            };
            gui.event(&wheel, window, [800., 600.]);
            assert!(
                matches!(gui.raw.events.as_slice(), [Event::MouseWheel { delta, .. }] if *delta == expected),
                "the SDL wheel delta must not be inverted again"
            );
            gui.raw.events.clear();
        }
    }

    fn finger(gui: &mut Gui, ty: SDL_EventType, id: u64, x: f32, y: f32) {
        gui.event(
            &SDL_Event {
                tfinger: SDL_TouchFingerEvent {
                    r#type: ty,
                    windowID: SDL_WindowID(1),
                    touchID: SDL_TouchID(2),
                    fingerID: SDL_FingerID(id),
                    x,
                    y,
                    ..Default::default()
                },
            },
            SDL_WindowID(1),
            [800., 600.],
        );
    }
    #[test]
    fn mouse_motion_does_not_replace_touch_pointer_motion() {
        let mut gui = Gui::new();
        finger(&mut gui, SDL_EVENT_FINGER_DOWN, 1, 0.1, 0.2);
        finger(&mut gui, SDL_EVENT_FINGER_MOTION, 1, 0.2, 0.2);
        gui.event(
            &SDL_Event {
                motion: SDL_MouseMotionEvent {
                    r#type: SDL_EVENT_MOUSE_MOTION,
                    windowID: SDL_WindowID(1),
                    x: 400.,
                    y: 150.,
                    ..Default::default()
                },
            },
            SDL_WindowID(1),
            [800., 600.],
        );
        assert!(matches!(
            gui.raw.events.as_slice(),
            [.., Event::PointerMoved(touch), Event::PointerMoved(mouse)]
                if *touch == egui::pos2(160., 120.) && *mouse == egui::pos2(400., 150.)
        ));
    }

    #[test]
    fn gui_touch_scales_coordinates_and_only_primary_finger_emulates_pointer() {
        let mut gui = Gui::new();
        gui.units = 2.;
        finger(&mut gui, SDL_EVENT_FINGER_DOWN, 1, 0.25, 0.5);
        finger(&mut gui, SDL_EVENT_FINGER_DOWN, 2, 0.75, 0.5);
        assert_eq!(
            gui.raw
                .events
                .iter()
                .filter(|e| matches!(e, Event::PointerButton { pressed: true, .. }))
                .count(),
            1
        );
        assert!(gui.raw.events.iter().any(|e|matches!(e, Event::PointerButton {pos,pressed:true,..} if *pos==egui::pos2(100.,150.))));
        gui.raw.events.clear();
        finger(&mut gui, SDL_EVENT_FINGER_UP, 2, 0.75, 0.5);
        assert!(
            !gui.raw
                .events
                .iter()
                .any(|e| matches!(e, Event::PointerButton { .. }))
        );
        finger(&mut gui, SDL_EVENT_FINGER_UP, 1, 0.25, 0.5);
        assert!(gui.touch_primary.is_none() && gui.touch_positions.is_empty());
    }
    #[test]
    fn canceled_finger_does_not_activate_a_button() {
        let mut gui = Gui::new();
        let mut position = egui::Pos2::ZERO;
        let frame = |gui: &mut Gui, position: &mut egui::Pos2| {
            let mut raw = gui.raw.take();
            raw.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800., 600.),
            ));
            let mut clicked = false;
            let output = gui.ctx.run_ui(raw, |ui| {
                let response = ui.button("Test");
                *position = response.rect.center();
                clicked = response.clicked();
            });
            output.drop_without_applying_deltas();
            clicked
        };
        frame(&mut gui, &mut position);
        finger(
            &mut gui,
            SDL_EVENT_FINGER_DOWN,
            1,
            position.x / 800.,
            position.y / 600.,
        );
        assert!(!frame(&mut gui, &mut position));
        finger(
            &mut gui,
            SDL_EVENT_FINGER_CANCELED,
            1,
            position.x / 800.,
            position.y / 600.,
        );
        assert!(!frame(&mut gui, &mut position));
        finger(
            &mut gui,
            SDL_EVENT_FINGER_DOWN,
            2,
            position.x / 800.,
            position.y / 600.,
        );
        frame(&mut gui, &mut position);
        finger(
            &mut gui,
            SDL_EVENT_FINGER_UP,
            2,
            position.x / 800.,
            position.y / 600.,
        );
        assert!(frame(&mut gui, &mut position));
    }
    #[test]
    fn primary_touch_drags_a_slider() {
        let mut gui = Gui::new();
        let mut value = 0.0_f32;
        let mut rect = egui::Rect::NOTHING;
        let frame = |gui: &mut Gui, value: &mut f32, rect: &mut egui::Rect| {
            let mut raw = gui.raw.take();
            raw.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800., 600.),
            ));
            let output = gui.ctx.run_ui(raw, |ui| {
                *rect = ui
                    .add_sized(
                        [240., 40.],
                        egui::Slider::new(value, 0.0..=1.0).show_value(false),
                    )
                    .rect;
            });
            output.drop_without_applying_deltas();
        };
        frame(&mut gui, &mut value, &mut rect);
        let y = rect.center().y / 600.;
        finger(
            &mut gui,
            SDL_EVENT_FINGER_DOWN,
            1,
            (rect.left() + 10.) / 800.,
            y,
        );
        frame(&mut gui, &mut value, &mut rect);
        let before = value;
        finger(
            &mut gui,
            SDL_EVENT_FINGER_MOTION,
            1,
            (rect.right() - 10.) / 800.,
            y,
        );
        frame(&mut gui, &mut value, &mut rect);
        assert!(value > before + 0.3);
        finger(
            &mut gui,
            SDL_EVENT_FINGER_UP,
            1,
            (rect.right() - 10.) / 800.,
            y,
        );
        frame(&mut gui, &mut value, &mut rect);
        assert!(gui.touch_positions.is_empty());
    }
}
