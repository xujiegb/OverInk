use super::App;
use crate::{
    appearance::text,
    color::{InkColor, PRESETS},
    fonts::TextFonts,
    gui::Gui,
    model::{History, MAX_TEXT_BOXES, MAX_TEXT_CHARS, TextBox, Tool},
    sdl::{SDL_Event, SDL_WindowID},
    toolbar,
};
use egui::{
    Color32, Event, ImeEvent, Key, PointerButton, Pos2, Rect, Stroke, StrokeKind, TextBuffer, Ui,
    UiBuilder, Vec2, pos2,
    text::{CCursor, CCursorRange},
    vec2,
};
use std::time::{Duration, Instant};

const MENU_BUTTON: Vec2 = vec2(30., 28.);
const MENU_GAP: f32 = 2.;
const MENU_PADDING: f32 = 4.;
const MENU_SIZE: Vec2 = vec2(
    7. * MENU_BUTTON.x + 6. * MENU_GAP + 2. * MENU_PADDING,
    MENU_BUTTON.y + 2. * MENU_PADDING,
);

#[derive(Default)]
pub(super) struct ToolbarTextFocus {
    pub deadline: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum FocusAction {
    Keep,
    ReturnToCanvas,
    Finish,
}

impl ToolbarTextFocus {
    pub fn update(
        &mut self,
        now: Instant,
        canvas_focused: bool,
        toolbar_focused: bool,
        toolbar_text: bool,
        toolbar_busy: bool,
    ) -> FocusAction {
        if canvas_focused {
            self.deadline = None;
            return FocusAction::Keep;
        }
        if toolbar_focused && toolbar_text {
            self.deadline = None;
            return FocusAction::Finish;
        }
        if toolbar_focused && toolbar_busy {
            self.deadline = None;
            return FocusAction::Keep;
        }
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                FocusAction::Finish
            }
            None if toolbar_focused => {
                // Native window activation may complete asynchronously.
                self.deadline = Some(now + Duration::from_millis(150));
                FocusAction::ReturnToCanvas
            }
            _ => FocusAction::Keep,
        }
    }
}

#[derive(Default)]
pub(super) struct TextInput {
    pub selected: Option<usize>,
    draft: Option<Draft>,
    press: Option<Press>,
    click: Option<(usize, Pos2, Instant)>,
}

struct Draft {
    item: TextBox,
    focus: bool,
    had_input_focus: bool,
    cursor: Option<CCursorRange>,
    scroll_offset: f32,
    // The widget's buffer may contain uncommitted IME preedit text.
    committed: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    Move,
    Resize,
    FontSize,
}

struct Press {
    origin: Pos2,
    target: Option<(usize, Handle)>,
    preview: Option<TextBox>,
    moved: bool,
}

impl TextInput {
    pub fn editing(&self) -> bool {
        self.draft.is_some()
    }

    pub fn has_edit(&self, history: &History) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|draft| match self.selected {
                Some(index) => history.text_boxes().get(index).is_some_and(|original| {
                    original.text != draft.committed
                        || original.color != draft.item.color
                        || original.font_size != draft.item.font_size
                }),
                None => !draft.committed.trim().is_empty(),
            })
    }

    pub fn saw_input_focus(&mut self) {
        if let Some(draft) = &mut self.draft {
            draft.had_input_focus = true;
        }
    }

    pub fn refocus(&mut self) {
        self.cancel_press();
        if let Some(draft) = &mut self.draft {
            draft.item.text.clone_from(&draft.committed);
            draft.focus = true;
        }
    }

    pub fn take_draft(&mut self, history: &History) -> Option<(Option<usize>, TextBox)> {
        let draft = self.draft.take()?;
        let mut item = draft.item;
        item.text = draft.committed;
        let original = self
            .selected
            .and_then(|index| history.text_boxes().get(index));
        if original.is_none_or(|original| original.text != item.text) {
            item.scroll_offset = draft.scroll_offset;
        }
        Some((self.selected, item))
    }

    pub fn finish(&mut self, history: &mut History) {
        if let Some((index, item)) = self.take_draft(history) {
            self.selected = history.set_text_box(index, item);
        }
    }

    pub fn deselect(&mut self, history: &mut History) {
        self.finish(history);
        *self = Self::default();
    }

    pub fn cancel_press(&mut self) {
        self.press = None;
        self.click = None;
    }

    fn edit(&mut self, index: Option<usize>, item: TextBox) {
        self.cancel_press();
        self.selected = index;
        self.draft = Some(Draft {
            committed: item.text.clone(),
            cursor: Some(CCursorRange::one(CCursor::new(item.text.chars().count()))),
            scroll_offset: item.scroll_offset,
            item,
            focus: true,
            had_input_focus: false,
        });
    }
}

impl App {
    pub(super) fn cancel_text_touches(&mut self, gui: &mut Gui) -> bool {
        if gui.primary_touch().is_none() {
            return false;
        }
        self.input.text.cancel_press();
        gui.cancel_touches();
        true
    }

    pub(super) fn text_event(
        &mut self,
        gui: &mut Gui,
        event: &SDL_Event,
        window: SDL_WindowID,
        size: [f32; 2],
    ) -> bool {
        use crate::sdl::*;

        let ty = unsafe { SDL_EventType(event.r#type) };
        let editing = self.input.text.editing();
        if !editing && self.input.text.selected.is_none() {
            gui.clear_events();
        }
        let first = gui.raw.events.len();
        let mut changed = false;
        if ty == SDL_EVENT_FINGER_CANCELED
            && gui.primary_touch() == Some(unsafe { crate::touch::TouchKey::from(event.tfinger) })
        {
            self.input.text.cancel_press();
            changed = true;
        }
        if matches!(ty, SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_DOWN) {
            changed |= self.cancel_text_touches(gui);
        }
        gui.event(event, window, size);
        if editing {
            return changed || gui.raw.events.len() != first;
        }
        let events: Vec<_> = gui.raw.events.drain(first..).collect();
        for event in events {
            let had_selection = self.input.text.selected.is_some();
            changed |= self.text_pointer(&event, size);
            if had_selection || self.input.text.selected.is_some() {
                gui.raw.events.push(event);
                changed = true;
            }
        }
        changed
    }

    pub(super) fn text_pointer(&mut self, event: &Event, size: [f32; 2]) -> bool {
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        let state = &mut self.input.text;
        match *event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                ..
            } => {
                if state.press.is_some()
                    || !canvas.contains(pos)
                    || self
                        .input
                        .protected
                        .is_some_and(|hole| hole.contains(pos.x, pos.y))
                {
                    return false;
                }
                if self
                    .text_menu_rect(canvas)
                    .is_some_and(|rect| rect.contains(pos))
                {
                    return true;
                }
                let state = &mut self.input.text;
                state.press = Some(Press {
                    origin: pos,
                    target: text_hit(&self.history, state.selected, pos, self.input.canvas_units),
                    preview: None,
                    moved: false,
                });
                true
            }
            Event::PointerMoved(pos) if state.press.is_some() => {
                self.move_text_pointer(pos, canvas);
                true
            }
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                ..
            } if state.press.is_some() => {
                self.move_text_pointer(pos, canvas);
                let state = &mut self.input.text;
                let press = state.press.take().unwrap();
                if let Some((index, handle)) = press.target
                    && let Some(item) = self.history.text_boxes().get(index)
                {
                    if let Some(item) = press.preview {
                        self.history.set_text_box(Some(index), item);
                        state.click = None;
                    } else if !press.moved && handle == Handle::Move {
                        let now = Instant::now();
                        let double = state.click.is_some_and(|(last, at, when)| {
                            last == index
                                && now.duration_since(when) < Duration::from_millis(400)
                                && at.distance(pos) <= 8. * self.input.canvas_units
                        });
                        if self.tool == Tool::Text || double {
                            state.edit(Some(index), (**item).clone());
                        } else {
                            state.selected = Some(index);
                            state.click = Some((index, pos, now));
                        }
                    }
                } else if press.target.is_none() && !press.moved && canvas.contains(pos) {
                    state.selected = None;
                    state.click = None;
                    if self.tool == Tool::Text {
                        if self.history.text_boxes().len() >= MAX_TEXT_BOXES {
                            self.status = text(
                                "Text box limit reached",
                                "已达到文字框数量上限",
                                "已達到文字框數量上限",
                            )
                            .into();
                        } else {
                            state.edit(None, TextBox::new(press.origin, canvas, self.color));
                        }
                    }
                }
                true
            }
            Event::PointerGone | Event::WindowFocused(false) if state.press.is_some() => {
                state.cancel_press();
                true
            }
            _ => false,
        }
    }

    fn move_text_pointer(&mut self, point: Pos2, canvas: Rect) {
        let state = &mut self.input.text;
        let press = state.press.as_mut().unwrap();
        let delta = point - press.origin;
        press.moved |= delta.length() >= 3. * self.input.canvas_units;
        if let Some((index, handle)) = press.target
            && press.moved
            && let Some(item) = self.history.text_boxes().get(index)
        {
            let mut item = (**item).clone();
            match handle {
                Handle::Move => item.move_to(item.rect().min + delta, canvas),
                Handle::Resize => item.resize(Vec2::from(item.size) + delta, canvas),
                Handle::FontSize => {
                    item.font_size = (item.font_size - delta.y * 0.5)
                        .clamp(*TextBox::FONT_SIZE.start(), *TextBox::FONT_SIZE.end());
                }
            }
            state.selected = Some(index);
            press.preview = Some(item);
        }
    }

    pub(super) fn delete_text_box(&mut self) -> bool {
        let state = &mut self.input.text;
        let Some(index) = state.selected else {
            return false;
        };
        *state = TextInput::default();
        self.history.remove_text_box(index)
    }

    pub(super) fn recolor_text(&mut self, color: InkColor) -> bool {
        if !self.tool.is_text() || self.pass || self.hidden {
            return false;
        }
        let state = &mut self.input.text;
        if let Some(draft) = &mut state.draft {
            return std::mem::replace(&mut draft.item.color, color) != color;
        }
        let Some(index) = state.selected else {
            return false;
        };
        let Some(original) = self
            .history
            .text_boxes()
            .get(index)
            .filter(|item| item.color != color)
        else {
            return false;
        };
        let mut item = (**original).clone();
        item.color = color;
        self.history.set_text_box(Some(index), item);
        if let Some(preview) = state
            .press
            .as_mut()
            .and_then(|press| press.preview.as_mut())
        {
            preview.color = color;
        }
        true
    }

    fn text_menu_rect(&self, canvas: Rect) -> Option<Rect> {
        let state = &self.input.text;
        let item = self.history.text_boxes().get(state.selected?)?;
        let item = state
            .press
            .as_ref()
            .and_then(|press| press.preview.as_ref())
            .unwrap_or(item);
        let protected = self.input.protected.map(|hole| {
            Rect::from_min_size(
                pos2(hole.x as f32, hole.y as f32),
                vec2(hole.w as f32, hole.h as f32),
            )
        });
        Some(toolbar::context_menu_rect(
            item.rect(),
            canvas,
            MENU_SIZE,
            protected,
        ))
    }

    fn resize_text_font(&mut self, step: f32) {
        let resize = |item: &mut TextBox| {
            item.font_size = (item.font_size + step)
                .clamp(*TextBox::FONT_SIZE.start(), *TextBox::FONT_SIZE.end());
        };
        if let Some(draft) = &mut self.input.text.draft {
            resize(&mut draft.item);
        } else if let Some(index) = self.input.text.selected
            && let Some(item) = self.history.text_boxes().get(index)
        {
            let mut item = (**item).clone();
            resize(&mut item);
            self.history.set_text_box(Some(index), item);
        }
    }

    fn text_menu(&mut self, ui: &mut Ui, rect: Rect) {
        let Some(item) = self
            .input
            .text
            .draft
            .as_ref()
            .map(|draft| &draft.item)
            .or_else(|| {
                self.input
                    .text
                    .selected
                    .and_then(|index| self.history.text_boxes().get(index))
                    .map(|item| &**item)
            })
        else {
            return;
        };
        let color = item.color;
        let font_size = item.font_size;
        let tr = self.language;
        ui.painter().rect(
            rect,
            6.,
            ui.visuals().window_fill(),
            ui.visuals().window_stroke(),
            StrokeKind::Inside,
        );
        let mut menu = ui.new_child(
            UiBuilder::new()
                .id_salt("annotation-actions")
                .max_rect(rect.shrink(MENU_PADDING))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        menu.spacing_mut().item_spacing.x = MENU_GAP;
        menu.spacing_mut().button_padding.x = 4.;
        let mut acted = false;
        if menu
            .add_sized(MENU_BUTTON, egui::Button::new("×"))
            .on_hover_text(tr.text("Delete text box", "删除文本框", "刪除文字框"))
            .clicked()
        {
            acted = self.delete_text_box();
        }
        for (label, hint, step, enabled) in [
            (
                "A+",
                tr.text("Increase font size", "加大字号", "加大字級"),
                2.,
                font_size < *TextBox::FONT_SIZE.end(),
            ),
            (
                "A−",
                tr.text("Decrease font size", "缩小字号", "縮小字級"),
                -2.,
                font_size > *TextBox::FONT_SIZE.start(),
            ),
        ] {
            if menu
                .add_enabled_ui(enabled, |ui| {
                    ui.add_sized(MENU_BUTTON, egui::Button::new(label))
                })
                .inner
                .on_hover_text(hint)
                .clicked()
            {
                self.resize_text_font(step);
                acted = true;
            }
        }
        for rgb in PRESETS {
            let preset = InkColor::from_rgb8(rgb);
            let label = format!(
                "{} {}",
                tr.text("Text color", "文字颜色", "文字顏色"),
                preset.hex()
            );
            if toolbar::swatch(&mut menu, MENU_BUTTON, rgb, &label, color.srgb8() == rgb)
                .on_hover_text(label)
                .clicked()
            {
                acted |= self.recolor_text(preset.converted(color.space));
            }
        }
        if acted {
            ui.ctx().request_repaint();
        }
        if self.input.text.editing()
            && ui.input(|input| {
                input
                    .pointer
                    .interact_pos()
                    .is_some_and(|p| rect.contains(p))
                    && input.pointer.primary_clicked()
            })
        {
            let id = ui.make_persistent_id("annotation-editor");
            ui.memory_mut(|mem| mem.request_focus(id));
        }
    }

    pub(super) fn text_key(&mut self, key: crate::sdl::SDL_Keycode) -> bool {
        use crate::sdl::*;
        let state = &mut self.input.text;
        let Some(index) = state.selected else {
            return false;
        };
        match key {
            SDLK_BACKSPACE | SDLK_DELETE => self.delete_text_box(),
            SDLK_RETURN | SDLK_KP_ENTER => {
                if let Some(item) = self.history.text_boxes().get(index) {
                    state.edit(Some(index), (**item).clone());
                }
                true
            }
            SDLK_ESCAPE => {
                state.deselect(&mut self.history);
                true
            }
            _ => false,
        }
    }

    pub(super) fn text_ui(
        &mut self,
        ui: &mut Ui,
        interactive: bool,
        composing: bool,
        keep_editor_open: bool,
    ) {
        let selected = self.input.text.selected;
        let draft = self.input.text.draft.as_ref();
        let preview = self
            .input
            .text
            .press
            .as_ref()
            .and_then(|press| press.preview.as_ref());
        for (index, item) in self.history.text_boxes().iter().enumerate() {
            let item = if interactive && selected == Some(index) {
                if draft.is_some() {
                    continue;
                }
                preview.unwrap_or(item)
            } else {
                item
            };
            draw_text_box(ui, &self.text_fonts, item);
        }
        if interactive {
            let menu = self.text_menu_rect(ui.clip_rect());
            if self.input.text.editing() {
                self.text_editor(ui, composing, keep_editor_open, menu);
            } else if let Some(item) =
                selected.and_then(|index| self.history.text_boxes().get(index))
            {
                let rect = preview.unwrap_or(item).rect();
                let [resize, font] = handles(rect, self.input.canvas_units);
                let color = crate::toolbar::ACTIVE;
                let painter = ui.painter();
                painter.rect_stroke(rect, 3., Stroke::new(1., color), StrokeKind::Inside);
                painter.rect_filled(resize.shrink(2.), 2., color);
                painter.circle_filled(font.center(), (font.width() / 2. - 2.).max(1.), color);
            }
            if let Some(rect) = menu {
                self.text_menu(ui, rect);
            }
        }
    }

    fn text_editor(
        &mut self,
        ui: &mut Ui,
        composing: bool,
        keep_editor_open: bool,
        menu: Option<Rect>,
    ) {
        let draft = self.input.text.draft.as_mut().unwrap();
        let rect = draft.item.rect();
        let text_rect = draft.item.text_rect();
        let id = ui.make_persistent_id("annotation-editor");
        let started = std::mem::take(&mut draft.focus);
        if started {
            ui.memory_mut(|mem| mem.request_focus(id));
            let mut state = egui::widgets::text_edit::TextEditState::default();
            state.cursor.set_char_range(draft.cursor);
            egui::TextEdit::store_state(ui.ctx(), id, state);
        }
        ui.painter().rect_stroke(
            rect,
            3.,
            Stroke::new(1., crate::toolbar::ACTIVE),
            StrokeKind::Inside,
        );
        let (focused, finish, committed_input, menu_clicked) = ui.input(|input| {
            // A pointer can start editing before SDL grants keyboard focus.
            // Keep the draft available to toolbar actions such as Redo or Clear.
            let lost_window_focus = !keep_editor_open && !input.focused
                && (draft.had_input_focus
                    || input.events.iter().any(|event| matches!(event, Event::WindowFocused(false))));
            let clicked = input.pointer.primary_clicked().then(|| input.pointer.interact_pos()).flatten();
            let menu_clicked = clicked.is_some_and(|point| menu.is_some_and(|menu| menu.contains(point)));
            let finish = lost_window_focus
                || (!composing
                    && (input.key_pressed(Key::Escape)
                        || input.events.iter().any(|event| {
                            matches!(event, Event::Key {
                    key: Key::Enter, pressed: true, modifiers, ..
                } if modifiers.command)
                        })))
                || (!menu_clicked && clicked.is_some_and(|point| !rect.contains(point)));
            let committed_input = input.events.iter().any(|event| {
                matches!(event, Event::Text(value) | Event::Paste(value) | Event::Ime(ImeEvent::Commit(value)) if !value.is_empty())
            });
            (input.focused, finish, committed_input, menu_clicked)
        });
        draft.had_input_focus |= focused;
        let font_size = draft.item.font_size;
        let color = ink(&draft.item);
        let mut child = ui.new_child(UiBuilder::new().max_rect(text_rect));
        child.set_clip_rect(text_rect.intersect(ui.clip_rect()));
        child.spacing_mut().item_spacing = Vec2::ZERO;
        let mut layouter = |ui: &Ui, value: &dyn egui::TextBuffer, wrap: f32| {
            self.text_fonts
                .layout(ui, value.as_str(), font_size, color, wrap)
        };
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("annotation-scroll")
            .max_height(text_rect.height())
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
        if started {
            scroll = scroll.vertical_scroll_offset(draft.scroll_offset);
        }
        let output = scroll.show(&mut child, |ui| {
            egui::TextEdit::multiline(&mut draft.item.text)
                .id(id)
                .font(TextFonts::font_id(font_size))
                .event_filter(egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                    ..Default::default()
                })
                .text_color(color)
                .desired_width(text_rect.width())
                .desired_rows(1)
                .margin(egui::Margin::ZERO)
                .frame(egui::Frame::NONE)
                .char_limit(MAX_TEXT_CHARS)
                .layouter(&mut layouter)
                .show(ui)
        });
        draft.scroll_offset = output.state.clamped_offset().y;
        let output = output.inner;
        if !composing {
            draft.cursor = output.cursor_range;
        }
        if output.response.changed() {
            if !composing {
                draft.committed.clone_from(&draft.item.text);
            } else if committed_input && let Some(preedit) = output.cursor_range {
                // SDL can commit one composition and start another in the same frame.
                draft.committed.clone_from(&draft.item.text);
                draft
                    .committed
                    .delete_char_range(preedit.as_sorted_char_range());
                draft.cursor = Some(CCursorRange::one(preedit.sorted_cursors()[0]));
            }
        }
        if finish
            || (focused && !started && !composing && !menu_clicked && output.response.lost_focus())
        {
            self.input.text.finish(&mut self.history);
            ui.memory_mut(|mem| mem.surrender_focus(id));
            ui.ctx().request_repaint();
        }
    }
}

fn draw_text_box(ui: &Ui, fonts: &TextFonts, item: &TextBox) {
    let rect = item.text_rect();
    if ui.is_rect_visible(rect) {
        let color = ink(item);
        let galley = fonts.layout(ui, &item.text, item.font_size, color, rect.width());
        let offset = item.visible_scroll_offset(galley.size().y);
        ui.painter()
            .with_clip_rect(rect.intersect(ui.clip_rect()))
            .galley(rect.min - vec2(0., offset), galley, color);
    }
}

fn ink(item: &TextBox) -> Color32 {
    let [red, green, blue] = item.color.srgb8();
    Color32::from_rgb(red, green, blue)
}

fn handles(rect: Rect, scale: f32) -> [Rect; 2] {
    let size = Vec2::splat((16. * scale).min(rect.height() / 2.));
    [
        Rect::from_min_size(rect.right_bottom() - size, size),
        Rect::from_min_size(pos2(rect.right() - size.x, rect.top()), size),
    ]
}

fn text_hit(
    history: &History,
    selected: Option<usize>,
    point: Pos2,
    scale: f32,
) -> Option<(usize, Handle)> {
    if let Some(index) = selected
        && let Some(item) = history.text_boxes().get(index)
    {
        let [resize, font] = handles(item.rect(), scale);
        if resize.contains(point) {
            return Some((index, Handle::Resize));
        }
        if font.contains(point) {
            return Some((index, Handle::FontSize));
        }
    }
    history
        .text_boxes()
        .iter()
        .rposition(|item| item.rect().contains(point))
        .map(|index| (index, Handle::Move))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{HistoryAction, touch_tests::app},
        gui::Gui,
        sdl::*,
    };
    use std::ffi::CString;

    const SIZE: [f32; 2] = [800., 600.];

    fn click(app: &mut App, point: Pos2) {
        for pressed in [true, false] {
            app.text_pointer(
                &Event::PointerButton {
                    pos: point,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
                SIZE,
            );
        }
    }

    fn render(app: &mut App, gui: &mut Gui) {
        render_with_toolbar_focus(app, gui, false);
    }

    fn render_with_toolbar_focus(app: &mut App, gui: &mut Gui, toolbar_focused: bool) {
        gui.raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE)));
        let composing = gui.composing();
        let input = gui.raw.take();
        let output = gui.ctx.run_ui(input, |ui| {
            app.text_ui(ui, true, composing, toolbar_focused)
        });
        output.drop_without_applying_deltas();
    }

    fn ime(gui: &mut Gui, event_type: SDL_EventType, value: &str) {
        ime_at(gui, event_type, value, 0);
    }

    fn ime_at(gui: &mut Gui, event_type: SDL_EventType, value: &str, timestamp: u64) {
        let value = CString::new(value).unwrap();
        let id = SDL_WindowID(1);
        let event = if event_type == SDL_EVENT_TEXT_EDITING {
            SDL_Event {
                edit: SDL_TextEditingEvent {
                    r#type: event_type,
                    timestamp,
                    windowID: id,
                    text: value.as_ptr(),
                    start: 0,
                    length: 0,
                    ..Default::default()
                },
            }
        } else {
            SDL_Event {
                text: SDL_TextInputEvent {
                    r#type: event_type,
                    timestamp,
                    windowID: id,
                    text: value.as_ptr(),
                    ..Default::default()
                },
            }
        };
        gui.event(&event, id, SIZE);
    }

    fn key_down(gui: &mut Gui, key: SDL_Keycode) {
        key_down_at(gui, key, 0);
    }

    fn key_down_at(gui: &mut Gui, key: SDL_Keycode, timestamp: u64) {
        let id = SDL_WindowID(1);
        gui.event(
            &SDL_Event {
                key: SDL_KeyboardEvent {
                    r#type: SDL_EVENT_KEY_DOWN,
                    timestamp,
                    windowID: id,
                    key,
                    down: true,
                    ..Default::default()
                },
            },
            id,
            SIZE,
        );
    }

    #[test]
    fn chinese_preedit_commits_once_and_unfinished_composition_is_discarded() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        render(&mut app, &mut gui);

        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "zhong");
        render(&mut app, &mut gui);
        assert_eq!(app.input.text.draft.as_ref().unwrap().item.text, "zhong");
        assert!(app.input.text.draft.as_ref().unwrap().committed.is_empty());

        key_down(&mut gui, SDLK_RETURN);
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "中");
        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "wen");
        render(&mut app, &mut gui);
        assert_eq!(app.input.text.draft.as_ref().unwrap().item.text, "中wen");
        assert_eq!(app.input.text.draft.as_ref().unwrap().committed, "中");
        assert!(!app.history.can_undo());
        app.stop();
        assert_eq!(app.history.text_boxes()[0].text, "中");
        app.change_history(HistoryAction::Undo);
        assert!(app.history.text_boxes().is_empty());
        assert!(!app.history.can_undo());
    }

    #[test]
    fn editor_waits_for_initial_window_focus_and_finishes_when_it_is_lost() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        gui.raw.focused = false;
        render(&mut app, &mut gui);
        render(&mut app, &mut gui);
        assert!(app.input.text.editing());

        gui.raw.focused = true;
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "文字");
        render(&mut app, &mut gui);
        gui.raw.focused = false;
        render(&mut app, &mut gui);
        assert!(!app.input.text.editing());
        assert_eq!(app.history.text_boxes()[0].text, "文字");
    }

    #[test]
    fn ime_end_before_control_keys_preserves_the_editor_and_confirmed_text() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "甲");
        render(&mut app, &mut gui);

        for key in [SDLK_BACKSPACE, SDLK_ESCAPE] {
            ime(&mut gui, SDL_EVENT_TEXT_EDITING, "z");
            render(&mut app, &mut gui);
            ime_at(&mut gui, SDL_EVENT_TEXT_EDITING, "", 10_000_000);
            key_down_at(&mut gui, key, 10_000_000);
            render(&mut app, &mut gui);
            assert_eq!(app.input.text.draft.as_ref().unwrap().committed, "甲");
        }

        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "yi");
        render(&mut app, &mut gui);
        ime_at(&mut gui, SDL_EVENT_TEXT_EDITING, "", 20_000_000);
        ime_at(&mut gui, SDL_EVENT_TEXT_INPUT, "乙", 20_150_000);
        key_down_at(&mut gui, SDLK_RETURN, 20_200_000);
        render(&mut app, &mut gui);
        assert_eq!(app.input.text.draft.as_ref().unwrap().committed, "甲乙");

        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "bing");
        render(&mut app, &mut gui);
        ime_at(&mut gui, SDL_EVENT_TEXT_INPUT, "丙", 30_000_000);
        key_down_at(&mut gui, SDLK_RETURN, 30_400_000);
        render(&mut app, &mut gui);
        assert_eq!(app.input.text.draft.as_ref().unwrap().committed, "甲乙丙");

        key_down(&mut gui, SDLK_RETURN);
        render(&mut app, &mut gui);
        assert_eq!(app.input.text.draft.as_ref().unwrap().committed, "甲乙丙\n");
    }

    #[test]
    fn independent_keys_work_in_the_same_batch_as_an_ime_commit() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "甲");
        render(&mut app, &mut gui);

        for (key, value, clock, expected) in [
            (SDLK_BACKSPACE, "乙", 10_000_000, "甲"),
            (SDLK_RETURN, "丙", 40_000_000, "甲丙\n"),
        ] {
            ime(&mut gui, SDL_EVENT_TEXT_EDITING, "preedit");
            render(&mut app, &mut gui);
            ime_at(&mut gui, SDL_EVENT_TEXT_INPUT, value, clock);
            key_down_at(&mut gui, key, clock + 15_000_000);
            render(&mut app, &mut gui);
            assert_eq!(app.input.text.draft.as_ref().unwrap().committed, expected);
        }
        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "ding");
        render(&mut app, &mut gui);
        ime_at(&mut gui, SDL_EVENT_TEXT_INPUT, "丁", 70_000_000);
        key_down_at(&mut gui, SDLK_ESCAPE, 85_000_000);
        render(&mut app, &mut gui);
        assert_eq!(app.history.text_boxes()[0].text, "甲丙\n丁");
    }

    #[test]
    fn clearing_the_first_draft_drops_stale_focus_before_the_next_editor() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        assert_eq!(app.history_buttons(), [false, false, true]);
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        let window = SDL_WindowID(1);
        let focus_lost = SDL_Event {
            window: SDL_WindowEvent {
                r#type: SDL_EVENT_WINDOW_FOCUS_LOST,
                windowID: window,
                ..Default::default()
            },
        };
        app.text_event(&mut gui, &focus_lost, window, SIZE);
        assert!(!gui.raw.events.is_empty());
        app.change_history(HistoryAction::Clear);
        assert_eq!(app.history_buttons(), [false; 3]);
        for (ty, down) in [
            (SDL_EVENT_MOUSE_BUTTON_DOWN, true),
            (SDL_EVENT_MOUSE_BUTTON_UP, false),
        ] {
            let event = SDL_Event {
                button: SDL_MouseButtonEvent {
                    r#type: ty,
                    windowID: window,
                    button: SDL_BUTTON_LEFT as u8,
                    down,
                    x: 200.,
                    y: 155.,
                    ..Default::default()
                },
            };
            app.text_event(&mut gui, &event, window, SIZE);
        }
        render(&mut app, &mut gui);
        assert!(app.input.text.editing());
    }

    #[test]
    fn toolbar_focus_protects_the_draft_and_redo_history() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        app.input.text.draft.as_mut().unwrap().committed = "原文".into();
        app.stop();
        let original = app.history.text_boxes()[0].clone();
        let mut edited = (*original).clone();
        edited.text = "可重做".into();
        app.history.set_text_box(Some(0), edited.clone());
        app.change_history(HistoryAction::Undo);
        click(&mut app, original.rect().center());
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        render(&mut app, &mut gui);
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "草稿");
        render(&mut app, &mut gui);
        gui.raw.focused = false;
        render_with_toolbar_focus(&mut app, &mut gui, true);
        assert!(app.input.text.editing());
        assert!(app.history.can_redo());

        let delete = SDL_Event {
            key: SDL_KeyboardEvent {
                r#type: SDL_EVENT_KEY_DOWN,
                key: SDLK_DELETE,
                down: true,
                ..Default::default()
            },
        };
        app.keyboard(&delete, false);
        assert!(app.input.text.editing());
        assert_eq!(app.history.text_boxes(), &[original]);

        app.change_history(HistoryAction::Redo);
        assert!(!app.input.text.editing());
        assert_eq!(*app.history.text_boxes()[0], edited);
    }

    #[test]
    fn toolbar_undo_preserves_prior_annotations_and_can_redo_the_draft() {
        for has_prior_box in [false, true] {
            let mut app = app();
            app.use_tool(Tool::Text);
            if has_prior_box {
                click(&mut app, pos2(20., 20.));
                app.input.text.draft.as_mut().unwrap().committed = "原批注".into();
                app.stop();
            }
            let previous = app.history.text_boxes().to_vec();
            click(&mut app, pos2(470., 280.));
            app.input.text.draft.as_mut().unwrap().committed = "重要批注".into();
            assert!(app.history_buttons()[0]);
            app.change_history(HistoryAction::Undo);
            assert!(!app.input.text.editing());
            assert_eq!(app.history.text_boxes(), previous);
            assert_eq!(app.history.can_undo(), has_prior_box);
            app.change_history(HistoryAction::Redo);
            assert_eq!(app.history.text_boxes().last().unwrap().text, "重要批注");
        }
    }

    #[test]
    fn clearing_the_first_written_draft_is_one_undoable_action() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        app.input.text.draft.as_mut().unwrap().committed = "首框".into();
        app.change_history(HistoryAction::Clear);
        assert!(app.history.document().is_empty());
        assert!(app.history.can_undo());
        app.change_history(HistoryAction::Undo);
        assert_eq!(app.history.text_boxes()[0].text, "首框");
        assert!(!app.history.can_undo());
        app.change_history(HistoryAction::Redo);
        assert!(app.history.document().is_empty());
    }

    #[test]
    fn toolbar_releases_focus_after_actions_and_finishes_if_activation_fails() {
        let mut focus = ToolbarTextFocus::default();
        let now = Instant::now();
        assert_eq!(
            focus.update(now, false, true, false, true),
            FocusAction::Keep
        );
        assert_eq!(
            focus.update(now, false, true, false, false),
            FocusAction::ReturnToCanvas
        );
        let expires = focus.deadline.unwrap();
        assert_eq!(
            focus.update(now, false, false, false, false),
            FocusAction::Keep
        );
        assert_eq!(
            focus.update(expires, true, false, false, false),
            FocusAction::Keep
        );
        assert!(focus.deadline.is_none());
        assert_eq!(
            focus.update(now, false, true, true, true),
            FocusAction::Finish
        );
        assert_eq!(
            focus.update(now, false, true, false, false),
            FocusAction::ReturnToCanvas
        );
        assert_eq!(
            focus.update(expires, false, true, false, false),
            FocusAction::Finish
        );
    }

    #[test]
    fn losing_toolbar_focus_finishes_an_editor_that_never_received_canvas_focus() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        gui.raw.focused = false;
        app.input.text.saw_input_focus();
        render_with_toolbar_focus(&mut app, &mut gui, true);
        assert!(app.input.text.editing());
        render(&mut app, &mut gui);
        assert!(!app.input.text.editing());
    }

    #[test]
    fn reopening_a_box_appends_at_the_end_and_commits_the_visible_scroll() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        app.input.text.draft.as_mut().unwrap().committed = "首行\n\n\n\n\n末行".into();
        app.stop();
        assert!(app.text_key(SDLK_RETURN));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        render(&mut app, &mut gui);
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "补充");
        for _ in 0..12 {
            render(&mut app, &mut gui);
        }
        app.stop();
        let item = &app.history.text_boxes()[0];
        assert!(item.text.ends_with("末行补充"));
        assert!(item.scroll_offset > 0.);
        let serialized = serde_json::to_string(item).unwrap();
        assert_eq!(
            &serde_json::from_str::<TextBox>(&serialized).unwrap(),
            item.as_ref()
        );
    }

    #[test]
    fn reset_preserves_the_editing_caret() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "甲丙");
        render(&mut app, &mut gui);
        key_down(&mut gui, SDLK_LEFT);
        render(&mut app, &mut gui);

        app.input.text.refocus();
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "乙");
        render(&mut app, &mut gui);
        assert_eq!(app.input.text.draft.as_ref().unwrap().committed, "甲乙丙");
    }

    #[test]
    fn switching_active_display_finishes_only_its_draft() {
        let mut app = app();
        app.multi = true;
        app.pages = crate::display::Pages::new(Some(1));
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        app.input.text.draft.as_mut().unwrap().committed = "显示器一".into();
        app.on_page(Some(2), |app| assert!(!app.input.text.editing()));
        assert!(app.input.text.editing());
        app.select_page(2);
        assert!(!app.input.text.editing());
        app.select_page(1);
        assert_eq!(app.history.text_boxes()[0].text, "显示器一");
        assert!(app.input.text.selected.is_none());
    }

    #[test]
    fn creation_at_the_limit_reports_the_reason() {
        let mut app = app();
        app.use_tool(Tool::Text);
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut item = TextBox::new(pos2(20., 20.), rect, app.color);
        item.text = "文字".into();
        let item = std::sync::Arc::new(item);
        app.history.set_document(
            crate::model::Document {
                size: SIZE,
                text_boxes: vec![item; MAX_TEXT_BOXES],
                ..Default::default()
            },
            true,
        );
        click(&mut app, pos2(520., 240.));
        assert!(!app.input.text.editing());
        assert_eq!(app.status.get(app.language), "Text box limit reached");
    }

    #[test]
    fn a_touch_can_create_a_box_or_cancel_a_drag_after_repaint() {
        let mut app = app();
        app.use_tool(Tool::Text);
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        let window_id = SDL_WindowID(1);
        let finger = |ty: SDL_EventType, x, y| SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: ty,
                windowID: if ty == SDL_EVENT_FINGER_CANCELED {
                    SDL_WindowID(0)
                } else {
                    window_id
                },
                touchID: SDL_TouchID(2),
                fingerID: SDL_FingerID(3),
                x,
                y,
                ..Default::default()
            },
        };
        let event = |gui: &mut Gui, app: &mut App, ty, x, y| {
            app.text_event(gui, &finger(ty, x, y), window_id, SIZE)
        };
        assert!(event(&mut gui, &mut app, SDL_EVENT_FINGER_DOWN, 0.25, 0.25));
        render(&mut app, &mut gui);
        assert!(event(&mut gui, &mut app, SDL_EVENT_FINGER_UP, 0.25, 0.25));
        app.input.text.draft.as_mut().unwrap().committed = "文字".into();
        app.use_tool(Tool::Select);
        let original = app.history.text_boxes()[0].clone();
        assert!(event(&mut gui, &mut app, SDL_EVENT_FINGER_DOWN, 0.3, 0.3));
        assert!(event(&mut gui, &mut app, SDL_EVENT_FINGER_MOTION, 0.4, 0.4));
        render(&mut app, &mut gui);
        assert!(event(
            &mut gui,
            &mut app,
            SDL_EVENT_FINGER_CANCELED,
            0.4,
            0.4
        ));
        assert!(app.input.text.press.is_none());
        assert_eq!(app.history.text_boxes(), &[original]);
    }

    #[test]
    fn moving_resizing_and_deleting_text_boxes_are_undoable() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        app.input.text.draft.as_mut().unwrap().committed = "中文 👩🏽‍🎨".into();
        app.stop();
        let original = app.history.text_boxes()[0].clone();
        app.use_tool(Tool::Select);
        click(&mut app, original.rect().center());
        for (from, to) in [
            (
                original.rect().center(),
                original.rect().center() + egui::vec2(80., 55.),
            ),
            (pos2(542., 279.), pos2(615., 315.)),
            (pos2(615., 181.), pos2(615., 151.)),
        ] {
            for (pos, pressed) in [(from, true), (to, false)] {
                assert!(app.text_pointer(
                    &Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                    SIZE,
                ));
            }
        }
        let edited = app.history.text_boxes()[0].clone();
        assert_eq!(edited.origin, [230., 175.]);
        assert_eq!(edited.size, [393., 148.]);
        assert_eq!(edited.font_size, 43.);
        assert!(app.text_key(SDLK_DELETE));
        assert!(app.history.document().is_empty());
        app.history.undo();
        assert_eq!(app.history.text_boxes(), &[edited]);
        for _ in 0..3 {
            app.history.undo();
        }
        assert_eq!(app.history.text_boxes(), &[original]);
    }

    #[test]
    fn context_menu_acts_on_the_selected_box_and_fits_at_the_bottom_edge() {
        let mut app = app();
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut item = TextBox::new(pos2(500., 300.), screen, InkColor::default());
        item.text = "Selected text".into();
        let index = app.history.set_text_box(None, item.clone()).unwrap();
        app.tool = Tool::Select;
        assert!(app.text_menu_rect(screen).is_none());
        click(&mut app, item.rect().center());
        let menu = app.text_menu_rect(screen).unwrap();
        assert!(menu.top() > item.rect().bottom());
        render(&mut app, &mut gui);
        let bigger = menu.min
            + vec2(
                MENU_PADDING + MENU_BUTTON.x * 1.5 + MENU_GAP,
                MENU_SIZE.y / 2.,
            );
        gui.raw.events.push(Event::PointerMoved(bigger));
        for pressed in [true, false] {
            let event = Event::PointerButton {
                pos: bigger,
                button: PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            app.text_pointer(&event, SIZE);
            gui.raw.events.push(event);
            render(&mut app, &mut gui);
        }
        assert_eq!(
            app.history.text_boxes()[index].font_size,
            item.font_size + 2.
        );
        assert!(app.input.text.press.is_none());
        app.history.undo();
        assert_eq!(app.history.text_boxes()[index].font_size, item.font_size);
        item.move_to(pos2(500., 580.), screen);
        app.history.set_text_box(Some(index), item.clone());
        let menu = app.text_menu_rect(screen).unwrap();
        assert!(item.rect().contains_rect(menu));
        click(&mut app, pos2(50., 50.));
        assert!(app.text_menu_rect(screen).is_none());
    }

    #[test]
    fn presets_recolor_drafts_and_undo_cancels_a_drag() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        assert!(app.select_preset(crate::color::PRESETS[1]));
        app.input.text.draft.as_mut().unwrap().committed = "文字".into();
        app.use_tool(Tool::Select);
        let original = app.history.text_boxes()[0].clone();
        assert_eq!(original.color.srgb8(), crate::color::PRESETS[1]);

        let pos = original.rect().center();
        click(&mut app, pos);
        assert!(app.select_preset(crate::color::PRESETS[2]));
        assert_eq!(
            app.history.text_boxes()[0].color.srgb8(),
            crate::color::PRESETS[2]
        );
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(app.text_pointer(&button(true), SIZE));
        assert!(app.text_pointer(&Event::PointerMoved(pos + Vec2::splat(20.)), SIZE));
        app.keyboard(
            &SDL_Event {
                key: SDL_KeyboardEvent {
                    r#type: SDL_EVENT_KEY_DOWN,
                    key: SDLK_Z,
                    r#mod: if cfg!(target_os = "macos") {
                        SDL_KMOD_GUI
                    } else {
                        SDL_KMOD_CTRL
                    },
                    down: true,
                    ..Default::default()
                },
            },
            false,
        );
        assert!(app.input.text.selected.is_none());
        assert!(!app.text_pointer(&button(false), SIZE));
        assert_eq!(app.history.text_boxes(), &[original]);
    }
}
