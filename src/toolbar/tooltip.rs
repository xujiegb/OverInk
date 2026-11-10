use crate::{appearance::Language, gui::Gui, icc::ColorManager, sdl::*};
use egui::{Color32, FontId, Id, Response, Theme, Tooltip, Vec2, pos2, vec2};

const WRAP_WIDTH: f32 = 280.;
const PAD_X: f32 = 8.;
const PAD_Y: f32 = 5.;

#[derive(Clone, Default, PartialEq)]
struct Hint {
    text: String,
    y: f32,
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
                    y: response.rect.center().y,
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
    hint: Hint,
    theme: Theme,
    language: Language,
    color_revision: u64,
}

impl Tooltips {
    pub fn clear(&mut self) {
        self.0 = None;
    }

    pub fn update(&mut self, parent: &Window, gui: &Gui, language: Language, color: &ColorManager) {
        let Some(hint) = gui.ctx.data_mut(|data| data.remove_temp::<Hint>(hint_id())) else {
            self.clear();
            return;
        };
        let theme = gui.ctx.theme();
        if self.0.as_ref().is_some_and(|popup| {
            popup.hint == hint
                && popup.theme == theme
                && popup.language == language
                && popup.color_revision == color.revision
        }) {
            return;
        }
        self.clear();
        self.0 = Popup::new(parent, gui.units, hint, theme, language, color).ok();
    }

    pub fn expose(&mut self, id: SDL_WindowID, color: &ColorManager) {
        if let Some(popup) = &mut self.0
            && popup.window.id == id
            && popup.paint(color).is_err()
        {
            self.clear();
        }
    }
}

impl Popup {
    fn new(
        parent: &Window,
        units: f32,
        hint: Hint,
        theme: Theme,
        language: Language,
        color: &ColorManager,
    ) -> Result<Self, String> {
        let [w, h] = [hint.size.x, hint.size.y].map(|size| (size * units).ceil() as i32 + 1);
        let mut x = parent.size()[0].ceil() as i32 + 5;
        let y = (hint.y * units).round() as i32 - h / 2;
        unsafe {
            let mut bounds = SDL_Rect::default();
            let mut parent_x = 0;
            if driver() != "wayland"
                && parent
                    .display_id()
                    .is_some_and(|id| SDL_GetDisplayUsableBounds(SDL_DisplayID(id), &mut bounds))
                && SDL_GetWindowPosition(parent.raw, &mut parent_x, std::ptr::null_mut())
                && parent_x + x + w > bounds.x + bounds.w
                && parent_x - 5 - w >= bounds.x
            {
                x = -5 - w;
            }
        }
        let window = Window::tooltip(parent, x, y, w, h)?;
        let render = Renderer::software(&window)?;
        let mut popup = Self {
            gui: Gui::new(),
            render,
            window,
            hint,
            theme,
            language,
            color_revision: color.revision,
        };
        popup.gui.set_language(language);
        popup.gui.ctx.set_theme(theme);
        popup.window.show(true)?;
        popup.paint(color)?;
        Ok(popup)
    }

    fn paint(&mut self, color: &ColorManager) -> Result<(), String> {
        let input = self.gui.input(&self.window);
        let frame = self.gui.ctx.run_ui(input, |ui| {
            let painter = ui.painter();
            let galley = painter.layout(
                self.hint.text.clone(),
                FontId::proportional(13.),
                ui.visuals().text_color(),
                WRAP_WIDTH,
            );
            painter.galley(pos2(PAD_X, PAD_Y), galley, ui.visuals().text_color());
        });
        let bg = super::colors(self.theme == Theme::Dark).panel.to_array();
        let [r, g, b] = color.map([bg[0], bg[1], bg[2]].map(|v| f32::from(v) / 255.));
        self.render.begin(&self.window, [r, g, b, 1.])?;
        self.gui.paint(&self.render, &self.window, color, frame)?;
        self.render.present()
    }
}
