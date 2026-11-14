//! Small egui/SDL3 integration. SDL_RenderGeometry makes the same UI usable on
//! s390x, LoongArch and FreeBSD without assuming a Vulkan driver.
use crate::{
    appearance::Language,
    icc::ColorManager,
    sdl::*,
    touch::{self, TouchKey},
};
use egui::{Context, Event, Key, Modifiers, RawInput, TextureId};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::CStr,
    ptr,
    time::Instant,
};
pub struct Gui {
    pub ctx: Context,
    pub raw: RawInput,
    textures: HashMap<TextureId, *mut SDL_Texture>,
    started: Instant,
    pub units: f32,
    touch_primary: Option<TouchKey>,
    touch_positions: BTreeMap<TouchKey, egui::Pos2>,
    font_language: Option<Language>,
}
impl Gui {
    pub fn new() -> Self {
        let ctx = Context::default();
        // Apply input actions once per native frame, including mode toggles.
        ctx.options_mut(|options| options.max_passes = std::num::NonZeroUsize::new(1).unwrap());
        ctx.set_theme(egui::Theme::Light);
        ctx.all_styles_mut(|s| {
            let colors = crate::toolbar::colors(s.visuals.dark_mode);
            s.visuals.panel_fill = colors.rail;
            s.visuals.window_fill = colors.panel;
            if s.visuals.dark_mode {
                s.visuals.widgets.noninteractive.fg_stroke.color =
                    egui::Color32::from_rgb(192, 207, 223);
                s.visuals.weak_text_color = Some(egui::Color32::from_rgb(155, 174, 192));
            } else {
                s.visuals.selection.bg_fill = egui::Color32::from_rgb(208, 236, 249);
                s.visuals.widgets.active.bg_fill = s.visuals.selection.bg_fill;
            }
            s.spacing.item_spacing = egui::vec2(8., 9.);
            s.spacing.button_padding = egui::vec2(10., 7.);
        });
        Self {
            ctx,
            raw: RawInput::default(),
            textures: HashMap::new(),
            started: Instant::now(),
            units: 1.,
            touch_primary: None,
            touch_positions: BTreeMap::new(),
            font_language: None,
        }
    }
    pub fn set_language(&mut self, language: Language) {
        let font_language = if language == Language::Traditional {
            Language::Traditional
        } else {
            Language::Simplified
        };
        if self.font_language == Some(font_language) {
            return;
        }
        self.font_language = Some(font_language);
        let mut fonts = egui::FontDefinitions::default();
        let sc = "OverInk UI SC";
        let tc = "OverInk UI TC";
        for (name, data) in [
            (
                sc,
                include_bytes!("../assets/fonts/OverInk-UI-SC.otf").as_slice(),
            ),
            (
                tc,
                include_bytes!("../assets/fonts/OverInk-UI-TC.otf").as_slice(),
            ),
        ] {
            fonts
                .font_data
                .insert(name.into(), egui::FontData::from_static(data).into());
        }
        let order = if font_language == Language::Traditional {
            [tc, sc]
        } else {
            [sc, tc]
        };
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .extend(order.map(str::to_owned));
        }
        self.ctx.set_fonts(fonts);
    }
    pub fn input(&mut self, w: &Window) -> RawInput {
        let logical = w.size();
        self.units = w.ui_scale().max(0.5);
        self.raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(logical[0] / self.units, logical[1] / self.units),
        ));
        self.raw.time = Some(self.started.elapsed().as_secs_f64());
        let current = modifiers(unsafe { SDL_GetModState() });
        // A synthetic input event would ask egui for another frame.
        if current != self.ctx.input(|input| input.modifiers) {
            self.raw.events.push(Event::ModifiersChanged(current));
        }
        self.raw
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(w.scale());
        self.raw.take()
    }
    /// SDL_Event is a tagged C union. Only the field selected by `type` is read.
    pub fn event(&mut self, e: &SDL_Event, id: SDL_WindowID, size: [f32; 2]) {
        unsafe {
            let ty = SDL_EventType(e.r#type);
            let scale = self.units;
            match ty {
                SDL_EVENT_MOUSE_MOTION
                    if e.motion.windowID == id
                        && e.motion.which != SDL_TOUCH_MOUSEID
                        && e.motion.which != SDL_PEN_MOUSEID =>
                {
                    self.raw.events.push(Event::PointerMoved(egui::pos2(
                        e.motion.x / scale,
                        e.motion.y / scale,
                    )))
                }
                SDL_EVENT_MOUSE_BUTTON_DOWN | SDL_EVENT_MOUSE_BUTTON_UP
                    if e.button.windowID == id
                        && e.button.which != SDL_TOUCH_MOUSEID
                        && e.button.which != SDL_PEN_MOUSEID =>
                {
                    let b = e.button;
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
                        modifiers: modifiers(SDL_GetModState()),
                    });
                }
                SDL_EVENT_FINGER_DOWN
                | SDL_EVENT_FINGER_MOTION
                | SDL_EVENT_FINGER_UP
                | SDL_EVENT_FINGER_CANCELED
                    if e.tfinger.windowID == id =>
                {
                    self.touch_event(e.tfinger, size);
                }
                SDL_EVENT_PEN_MOTION if e.pmotion.windowID == id => self.raw.events.push(
                    Event::PointerMoved(egui::pos2(e.pmotion.x / scale, e.pmotion.y / scale)),
                ),
                SDL_EVENT_PEN_DOWN | SDL_EVENT_PEN_UP if e.ptouch.windowID == id => {
                    let p = e.ptouch;
                    self.raw.events.push(Event::PointerButton {
                        pos: egui::pos2(p.x / scale, p.y / scale),
                        button: egui::PointerButton::Primary,
                        pressed: p.down,
                        modifiers: modifiers(SDL_GetModState()),
                    });
                }
                SDL_EVENT_MOUSE_WHEEL if e.wheel.windowID == id => {
                    let w = e.wheel;
                    let sign = if w.direction == SDL_MOUSEWHEEL_FLIPPED {
                        -1.
                    } else {
                        1.
                    };
                    self.raw.events.push(Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(w.x * sign, w.y * sign),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        modifiers: modifiers(SDL_GetModState()),
                    });
                }
                SDL_EVENT_WINDOW_FOCUS_LOST if e.window.windowID == id => {
                    self.cancel_touches();
                    self.raw.focused = false;
                    self.raw.events.push(Event::WindowFocused(false));
                }
                SDL_EVENT_WINDOW_FOCUS_GAINED if e.window.windowID == id => {
                    self.raw.focused = true;
                    self.raw.events.push(Event::WindowFocused(true));
                }
                SDL_EVENT_WINDOW_MOUSE_LEAVE if e.window.windowID == id => {
                    self.raw.events.push(Event::PointerGone)
                }
                SDL_EVENT_KEY_DOWN | SDL_EVENT_KEY_UP if e.key.windowID == id => {
                    let k = e.key;
                    let m = modifiers(k.r#mod);
                    if k.down && m.command && k.key == SDLK_V {
                        let p = SDL_GetClipboardText();
                        if !p.is_null() {
                            let text = CStr::from_ptr(p).to_string_lossy().into_owned();
                            SDL_free(p.cast());
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
                SDL_EVENT_TEXT_INPUT if e.text.windowID == id => {
                    let p = e.text.text;
                    if !p.is_null() {
                        self.raw.events.push(Event::Text(
                            CStr::from_ptr(p).to_string_lossy().into_owned(),
                        ));
                    }
                }
                _ => {}
            }
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
    fn cancel_touches(&mut self) {
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
        unsafe {
            for (id, deltas) in output.textures_delta.set.drain() {
                for delta in deltas {
                    let egui::ImageData::Color(image) = delta.image;
                    let [w, h] = image.size;
                    let bytes: Vec<u8> = image
                        .pixels
                        .iter()
                        .flat_map(|p| p.to_srgba_unmultiplied())
                        .collect();
                    let texture = if delta.pos.is_none() {
                        if let Some(old) = self.textures.remove(&id) {
                            SDL_DestroyTexture(old)
                        }
                        let p = SDL_CreateTexture(
                            r.raw,
                            SDL_PIXELFORMAT_RGBA32,
                            SDL_TEXTUREACCESS_STATIC,
                            w as i32,
                            h as i32,
                        );
                        if p.is_null() {
                            return Err(crate::sdl::error());
                        }
                        check(SDL_SetTextureBlendMode(p, SDL_BLENDMODE_BLEND))?;
                        SDL_SetTextureScaleMode(p, SDL_SCALEMODE_LINEAR);
                        self.textures.insert(id, p);
                        p
                    } else {
                        *self
                            .textures
                            .get(&id)
                            .ok_or("Missing partial egui texture")?
                    };
                    let rect = delta.pos.map(|pos| SDL_Rect {
                        x: pos[0] as i32,
                        y: pos[1] as i32,
                        w: w as i32,
                        h: h as i32,
                    });
                    check(SDL_UpdateTexture(
                        texture,
                        rect.as_ref().map_or(ptr::null(), |r| r),
                        bytes.as_ptr().cast(),
                        (w * 4) as i32,
                    ))?;
                }
            }
            let px = window.pixel_size();
            let logical = window.size();
            let scale = [px[0] as f32 / logical[0], px[1] as f32 / logical[1]];
            for p in self.ctx.tessellate(output.shapes, output.pixels_per_point) {
                let egui::epaint::Primitive::Mesh(mesh) = p.primitive else {
                    continue;
                };
                let Some(&texture) = self.textures.get(&mesh.texture_id) else {
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
                SDL_SetRenderClipRect(r.raw, &clip);
                let vertices: Vec<SDL_Vertex> = mesh
                    .vertices
                    .iter()
                    .map(|v| {
                        let [red, green, blue, alpha] = v.color.to_srgba_unmultiplied();
                        let c =
                            color.map([red as f32 / 255., green as f32 / 255., blue as f32 / 255.]);
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
                    })
                    .collect();
                r.geometry(texture, &vertices, &mesh.indices)?;
            }
            SDL_SetRenderClipRect(r.raw, ptr::null());
            for id in output.textures_delta.free.drain() {
                if let Some(p) = self.textures.remove(&id) {
                    SDL_DestroyTexture(p)
                }
            }
            for command in output.platform_output.commands {
                if let egui::OutputCommand::CopyText(text) = command {
                    let text = crate::sdl::cstring(&text);
                    SDL_SetClipboardText(text.as_ptr());
                }
            }
            Ok(())
        }
    }
}
impl Drop for Gui {
    fn drop(&mut self) {
        for (_, p) in self.textures.drain() {
            unsafe { SDL_DestroyTexture(p) }
        }
    }
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
        SDLK_RETURN => Key::Enter,
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
    fn changing_ui_language_loads_chinese_glyphs() {
        let mut gui = Gui::new();
        for (language, sample) in [
            (
                Language::Simplified,
                "简体中文 屏幕取色 退出 安装扩展 重新登录",
            ),
            (
                Language::Traditional,
                "繁體中文 螢幕取色 結束 安裝擴充套件 重新登入",
            ),
        ] {
            gui.set_language(language);
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
