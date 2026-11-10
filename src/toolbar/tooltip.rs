use crate::{appearance::Language, gui::Gui, icc::ColorManager, sdl::*};
use egui::{Color32, FontId, Id, Response, Theme, Tooltip, Vec2, pos2, vec2};

const WRAP_WIDTH: f32 = 280.;
const PAD_X: f32 = 8.;
const PAD_Y: f32 = 5.;

#[derive(Clone, Default, PartialEq)]
struct Hint {
    text: String,
    left: f32,
    right: f32,
    center_y: f32,
    size: Vec2,
}

fn hint_id() -> Id {
    Id::unique("overink-native-tooltip")
}

pub fn hint(response: Response, text: &str) -> Response {
    if Tooltip::should_show_tooltip(&response, false) {
        let size = response.ctx.fonts_mut(|fonts| {
            fonts
                .layout(
                    text.to_owned(),
                    FontId::proportional(13.),
                    Color32::WHITE,
                    WRAP_WIDTH,
                )
                .size()
        }) + vec2(2. * PAD_X, 2. * PAD_Y);
        response.ctx.data_mut(|data| {
            data.insert_temp(
                hint_id(),
                Hint {
                    text: text.to_owned(),
                    left: response.rect.left(),
                    right: response.rect.right(),
                    center_y: response.rect.center().y,
                    size,
                },
            );
        });
    }
    response
}

#[derive(Default)]
pub struct Tooltips(Option<Popup>);

struct Popup {
    gui: Gui,
    render: Renderer,
    window: Window,
    bounds: [i32; 4],
    content: Content,
}

#[derive(PartialEq)]
struct Content {
    hint: Hint,
    units: f32,
    theme: Theme,
    language: Language,
    color_revision: u64,
}

impl Tooltips {
    pub fn clear(&mut self) {
        if let Some(popup) = &self.0
            && popup.visible()
            && popup.window.show(false).is_err()
        {
            self.0 = None;
        }
    }

    pub fn update(&mut self, parent: &Window, gui: &Gui, language: Language, color: &ColorManager) {
        let Some(hint) = gui.ctx.data_mut(|data| data.remove_temp::<Hint>(hint_id())) else {
            self.clear();
            return;
        };
        let content = Content {
            hint,
            units: gui.units,
            theme: gui.ctx.theme(),
            language,
            color_revision: color.revision,
        };
        if self
            .0
            .as_ref()
            .is_some_and(|popup| popup.visible() && popup.content == content)
        {
            return;
        }
        if self.show(parent, content, color).is_err() {
            self.0 = None;
        }
    }

    fn show(
        &mut self,
        parent: &Window,
        content: Content,
        color: &ColorManager,
    ) -> Result<(), String> {
        let bounds = content.bounds(parent);
        let popup = if let Some(popup) = &mut self.0 {
            popup.configure(bounds)?;
            popup.content = content;
            popup
        } else {
            let [x, y, w, h] = bounds;
            let window = Window::tooltip(parent, x, y, w, h)?;
            let render = Renderer::software(&window)?;
            self.0.insert(Popup {
                gui: Gui::new(),
                render,
                window,
                bounds,
                content,
            })
        };
        popup.gui.set_language(popup.content.language);
        popup.gui.ctx.set_theme(popup.content.theme);
        popup.window.show(true)?;
        popup.paint(color)
    }

    pub fn expose(&mut self, id: SDL_WindowID, color: &ColorManager) {
        if let Some(popup) = &mut self.0
            && popup.window.id == id
            && popup.visible()
            && popup.paint(color).is_err()
        {
            self.0 = None;
        }
    }
}

impl Content {
    fn bounds(&self, parent: &Window) -> [i32; 4] {
        let [w, h] =
            [self.hint.size.x, self.hint.size.y].map(|size| (size * self.units).ceil() as i32 + 1);
        let mut x = (self.hint.right * self.units).ceil() as i32 + 5;
        let left = (self.hint.left * self.units).floor() as i32 - 5 - w;
        let y = (self.hint.center_y * self.units).round() as i32 - h / 2;
        if driver() == "wayland" {
            // An xdg-popup must overlap or touch its parent's surface.
            x = x.min(parent.size()[0] as i32);
            return [x, y, w, h];
        }
        unsafe {
            let mut bounds = SDL_Rect::default();
            let mut parent_x = 0;
            if parent
                .display_id()
                .is_some_and(|id| SDL_GetDisplayUsableBounds(SDL_DisplayID(id), &mut bounds))
                && SDL_GetWindowPosition(parent.raw, &mut parent_x, std::ptr::null_mut())
                && parent_x + x + w > bounds.x + bounds.w
                && parent_x + left >= bounds.x
            {
                x = left;
            }
        }
        [x, y, w, h]
    }
}

impl Popup {
    fn visible(&self) -> bool {
        (unsafe { SDL_GetWindowFlags(self.window.raw) } & SDL_WINDOW_HIDDEN) == 0
    }

    fn configure(&mut self, bounds: [i32; 4]) -> Result<(), String> {
        if bounds != self.bounds {
            // Re-map the popup at its new geometry, including on Wayland
            // compositors without support for repositioning mapped popups.
            self.window.show(false)?;
            let [x, y, w, h] = bounds;
            unsafe {
                check(SDL_SetWindowSize(self.window.raw, w, h))?;
                check(SDL_SetWindowPosition(self.window.raw, x, y))?;
            }
            self.bounds = bounds;
        }
        Ok(())
    }

    fn paint(&mut self, color: &ColorManager) -> Result<(), String> {
        let input = self.gui.input(&self.window);
        let frame = self.gui.ctx.run_ui(input, |ui| {
            let painter = ui.painter();
            let galley = painter.layout(
                self.content.hint.text.clone(),
                FontId::proportional(13.),
                ui.visuals().text_color(),
                WRAP_WIDTH,
            );
            painter.galley(pos2(PAD_X, PAD_Y), galley, ui.visuals().text_color());
        });
        let bg = super::colors(self.content.theme == Theme::Dark)
            .panel
            .to_array();
        let [r, g, b] = color.map([bg[0], bg[1], bg[2]].map(|v| f32::from(v) / 255.));
        self.render.begin(&self.window, [r, g, b, 1.])?;
        self.gui.paint(&self.render, &self.window, color, frame)?;
        self.render.present()
    }
}
