#![forbid(unsafe_code)]

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

mod color_wheel;
use color_wheel::ColorWheel;

const MENU_BUTTON: Vec2 = vec2(toolbar::RAIL_BUTTON_SIZE.x, 35.);
const KEYBOARD_BUTTON: Vec2 = vec2(52., 35.);
const KEYBOARD_COLUMN: f32 = 7.;
const MENU_GAP: f32 = 4.;
const MENU_PADDING: f32 = 5.;
// Palette width is derived from the text action bar, not an independent
// fixed pixel width. Reserve only the control rows; the wheel fills the
// available vertical space without stretching beyond its natural circle.
const PALETTE_MARGIN: f32 = 10.;
const PALETTE_GAP: f32 = 7.;
const PALETTE_SLIDER_HEIGHT: f32 = 32.;
const MENU_SIZE: Vec2 = vec2(
    8. * MENU_BUTTON.x + KEYBOARD_BUTTON.x + 8. * MENU_GAP + 2. * MENU_PADDING,
    MENU_BUTTON.y + 2. * MENU_PADDING,
);

#[derive(Clone, Copy)]
struct TextMenus {
    actions: Rect,
    scale: f32,
    palette: Option<(Rect, f32)>,
}

// A screen pick must not recolor a different annotation when the toolbar is
// still interactive while the native picker is running.
#[derive(Debug, PartialEq)]
pub(super) struct TextPickTarget {
    selected: Option<usize>,
    editing: bool,
    item: TextBox,
    committed: Option<String>,
}

impl TextMenus {
    fn contains(self, pos: Pos2) -> bool {
        self.actions.contains(pos) || self.palette.is_some_and(|(rect, _)| rect.contains(pos))
    }
}

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
    touch_press: bool,
    touch_origin: Option<Pos2>,
    menu_pointer: MenuPointer,
    context_menu: Option<Pos2>,
    color_wheel: Option<ColorWheel>,
    screen_pick_requested: bool,
    keyboard_requested: bool,
    keyboard_press: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextContextAction {
    SelectAll,
    Copy,
    Paste,
    Cut,
}

#[derive(Default)]
struct MenuPointer {
    hovered: bool,
    pressed: bool,
}

impl MenuPointer {
    fn wants_event(&mut self, event: &Event, menu: Option<TextMenus>, popup: Option<Rect>) -> bool {
        let contains = |pos| {
            menu.is_some_and(|rect| rect.contains(pos))
                || popup.is_some_and(|rect| rect.contains(pos))
        };
        match *event {
            Event::PointerMoved(pos) => {
                let hovered = contains(pos);
                let was_hovered = std::mem::replace(&mut self.hovered, hovered);
                self.pressed || was_hovered || hovered
            }
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                ..
            } => {
                let was_pressed = self.pressed;
                let hovered = contains(pos);
                if !was_pressed && !hovered {
                    return false;
                }
                self.hovered = hovered;
                self.pressed = pressed;
                true
            }
            Event::PointerGone => std::mem::take(&mut self.hovered) || self.pressed,
            Event::WindowFocused(false) => {
                let previous = std::mem::take(self);
                previous.hovered || previous.pressed
            }
            _ => false,
        }
    }
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

    pub fn take_screen_pick(&mut self) -> bool {
        std::mem::take(&mut self.screen_pick_requested)
    }

    pub fn take_keyboard_request(&mut self) -> bool {
        std::mem::take(&mut self.keyboard_requested)
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
        self.color_wheel = None;
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
        self.menu_pointer = MenuPointer::default();
        self.context_menu = None;
        self.color_wheel = None;
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
        self.input.text.touch_press = false;
        self.input.text.touch_origin = None;
        gui.cancel_touches();
        true
    }

    fn keyboard_button_rect(&self, canvas: Rect) -> Option<Rect> {
        let menu = self.text_menus(canvas)?;
        let top_left = menu.actions.min
            + vec2(
                MENU_PADDING + KEYBOARD_COLUMN * (MENU_BUTTON.x + MENU_GAP),
                MENU_PADDING,
            ) * menu.scale;
        Some(Rect::from_min_size(top_left, KEYBOARD_BUTTON * menu.scale))
    }

    /// Handle the keyboard button in the same checked pointer path as text-box
    /// selection. A native SDL mouse, pen, or finger click must not depend on
    /// which egui overlay layer currently owns the menu's widget ID.
    fn keyboard_pointer(&mut self, event: &Event, canvas: Rect) -> bool {
        match *event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                ..
            } => {
                self.input.text.keyboard_press = self
                    .keyboard_button_rect(canvas)
                    .is_some_and(|rect| rect.contains(pos));
                self.input.text.keyboard_press
            }
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                ..
            } => {
                let down_on_button = std::mem::take(&mut self.input.text.keyboard_press);
                if down_on_button
                    && self
                        .keyboard_button_rect(canvas)
                        .is_some_and(|rect| rect.contains(pos))
                {
                    return self.request_text_keyboard();
                }
                false
            }
            Event::PointerGone | Event::WindowFocused(false) => {
                self.input.text.keyboard_press = false;
                false
            }
            _ => false,
        }
    }

    fn request_text_keyboard(&mut self) -> bool {
        if !self.input.text.editing()
            && let Some(index) = self.input.text.selected
            && let Some(item) = self.history.text_boxes().get(index)
        {
            self.input.text.edit(Some(index), (**item).clone());
        }
        if self.input.text.editing() {
            self.input.text.keyboard_requested = true;
            true
        } else {
            false
        }
    }

    pub(super) fn text_event(
        &mut self,
        gui: &mut Gui,
        event: &SDL_Event,
        window: SDL_WindowID,
        size: [f32; 2],
    ) -> bool {
        use crate::sdl::*;

        let ty = event_type(event);
        let editing = self.input.text.editing();
        if !editing && self.input.text.selected.is_none() {
            gui.clear_events();
        }
        let first = gui.raw.events.len();
        let mut changed = false;
        if ty == SDL_EVENT_FINGER_CANCELED
            && gui.primary_touch() == crate::touch::key_from_event(event)
        {
            self.input.text.cancel_press();
            changed = true;
        }
        if matches!(ty, SDL_EVENT_PEN_PROXIMITY_IN | SDL_EVENT_PEN_DOWN) {
            changed |= self.cancel_text_touches(gui);
        }
        let primary_touch = gui.primary_touch();
        gui.event(event, window, size);
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        // Resolve keyboard clicks before filtering raw egui input. On macOS
        // and Windows the native pointer sequence can cross an overlay
        // layer or focus boundary without registering an egui Button click.
        for pointer in gui.raw.events.iter().skip(first) {
            changed |= self.keyboard_pointer(pointer, canvas);
        }
        if editing
            && ty == SDL_EVENT_MOUSE_BUTTON_DOWN
            && let Some(button) = mouse_button_event(event)
            && button.button == SDL_BUTTON_RIGHT as u8
            && button.which != SDL_PEN_MOUSEID
            && button.which != SDL_TOUCH_MOUSEID
        {
            let point = pos2(button.x, button.y);
            if self
                .text_menu_item()
                .is_some_and(|item| item.rect().contains(point))
            {
                self.input.text.context_menu = Some(point);
                changed = true;
            }
        }
        if ty == SDL_EVENT_FINGER_DOWN && gui.primary_touch() == crate::touch::key_from_event(event)
        {
            self.input.text.touch_press = true;
            self.input.text.touch_origin = finger_event(event)
                .and_then(|finger| crate::touch::point(finger, size))
                .map(|point| pos2(point.x, point.y));
        } else if ty == SDL_EVENT_MOUSE_BUTTON_DOWN
            && mouse_button_event(event).is_some_and(|button| button.which != SDL_TOUCH_MOUSEID)
        {
            // On X11/Windows, the same finger may also synthesize SDL mouse
            // packets. Those must not erase the real touch provenance.
            self.input.text.touch_press = false;
            self.input.text.touch_origin = None;
        }
        let touch_ended = matches!(ty, SDL_EVENT_FINGER_UP | SDL_EVENT_FINGER_CANCELED);
        if editing {
            // The editor is already active, but the native keyboard may have
            // been dismissed. A *direct finger* tap in the text can reopen it.
            // Pen and mouse pointer events must never request it automatically.
            if ty == SDL_EVENT_FINGER_UP
                && self.input.text.touch_press
                && crate::touch::key_from_event(event) == primary_touch
                && finger_event(event)
                    .and_then(|finger| crate::touch::point(finger, size))
                    .is_some_and(|point| {
                        let pos = pos2(point.x, point.y);
                        self.input.text.touch_origin.is_some_and(|start| {
                            start.distance(pos) <= 8. * self.input.canvas_units
                        }) && self
                            .text_menu_item()
                            .is_some_and(|item| item.rect().contains(pos))
                    })
            {
                self.input.text.keyboard_requested = true;
            }
            if touch_ended {
                self.input.text.touch_press = false;
                self.input.text.touch_origin = None;
            }
            return changed || gui.raw.events.len() != first;
        }
        let mut queued = first;
        gui.raw.events.retain(|event| {
            if queued > 0 {
                queued -= 1;
                return true;
            }
            changed |= self.text_pointer(event, size);
            let menu = self.text_menus(canvas);
            let popup = self.text_context_rect(canvas);
            let forward = self.input.text.menu_pointer.wants_event(event, menu, popup);
            changed |= forward;
            forward
        });
        if touch_ended {
            self.input.text.touch_press = false;
            self.input.text.touch_origin = None;
        }
        changed
    }

    pub(super) fn text_pointer(&mut self, event: &Event, size: [f32; 2]) -> bool {
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        let state = &mut self.input.text;
        match *event {
            Event::PointerButton {
                pos,
                button: PointerButton::Secondary,
                pressed: true,
                ..
            } => {
                if !canvas.contains(pos) {
                    return false;
                }
                if let Some((index, _)) =
                    text_hit(&self.history, state.selected, pos, self.input.canvas_units)
                {
                    state.selected = Some(index);
                    state.context_menu = Some(pos);
                    state.click = None;
                    return true;
                }
                state.context_menu = None;
                false
            }
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
                    .text_menus(canvas)
                    .is_some_and(|menus| menus.contains(pos))
                    || self
                        .text_context_rect(canvas)
                        .is_some_and(|menu| menu.contains(pos))
                {
                    return true;
                }
                let state = &mut self.input.text;
                state.context_menu = None;
                state.color_wheel = None;
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
                let Some(press) = state.press.take() else {
                    return true;
                };
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
                        // Touch: first tap selects, subsequent tap enters the
                        // editor, even after the mouse double-click timeout.
                        let already_selected = state.selected == Some(index);
                        let edit = if state.touch_press {
                            already_selected
                        } else {
                            self.tool == Tool::Text || double
                        };
                        if edit {
                            let touch = state.touch_press;
                            state.edit(Some(index), (**item).clone());
                            state.keyboard_requested |= touch;
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
                            state.edit(
                                None,
                                TextBox::new(
                                    press.origin,
                                    canvas,
                                    self.color,
                                    self.input.canvas_units,
                                ),
                            );
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
        let Some(press) = state.press.as_mut() else {
            return;
        };
        let delta = point - press.origin;
        press.moved |= delta.length() >= 3. * self.input.canvas_units;
        if let Some((index, handle)) = press.target
            && press.moved
            && let Some(item) = self.history.text_boxes().get(index)
        {
            // Drag events can arrive at high refresh rates. Reuse the
            // preview's text allocation instead of cloning the annotation's
            // String for every pointer movement. Reset geometry from the
            // committed source so deltas remain absolute, not cumulative.
            let source = item.as_ref();
            let mut preview = press.preview.take().unwrap_or_else(|| (*source).clone());
            preview.origin = source.origin;
            preview.size = source.size;
            preview.font_size = source.font_size;
            preview.color = source.color;
            preview.scroll_offset = source.scroll_offset;
            if preview.text != source.text {
                preview.text.clone_from(&source.text);
            }
            match handle {
                Handle::Move => preview.move_to(preview.rect().min + delta, canvas),
                Handle::Resize => preview.resize(Vec2::from(preview.size) + delta, canvas),
                Handle::FontSize => {
                    preview.font_size = (preview.font_size - delta.y * 0.5)
                        .clamp(*TextBox::FONT_SIZE.start(), *TextBox::FONT_SIZE.end());
                }
            }
            state.selected = Some(index);
            press.preview = Some(preview);
        }
    }

    pub(super) fn delete_text_box(&mut self) -> bool {
        let state = std::mem::take(&mut self.input.text);
        match state.selected {
            Some(index) => self.history.remove_text_box(index),
            None => state.editing(),
        }
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

    fn text_menu_item(&self) -> Option<&TextBox> {
        let state = &self.input.text;
        state.draft.as_ref().map(|draft| &draft.item).or_else(|| {
            let item = self.history.text_boxes().get(state.selected?)?;
            Some(
                state
                    .press
                    .as_ref()
                    .and_then(|press| press.preview.as_ref())
                    .unwrap_or(item),
            )
        })
    }

    fn text_menus(&self, canvas: Rect) -> Option<TextMenus> {
        let item = self.text_menu_item()?;
        let protected = self.input.protected.map(|hole| {
            Rect::from_min_size(
                pos2(hole.x as f32, hole.y as f32),
                vec2(hole.w as f32, hole.h as f32),
            )
        });
        let units = self.input.canvas_units;
        let scale = toolbar::menu_scale(canvas, MENU_SIZE, units);
        let actions =
            toolbar::context_menu_rect(item.rect(), canvas, MENU_SIZE * scale, protected, scale);
        let palette = self.input.text.color_wheel.map(|_| {
            let (size, scale) = palette_layout(actions, canvas, scale);
            // At most two obstacles: avoid allocating a Vec every palette frame.
            let obstacles = [actions, protected.unwrap_or(actions)];
            let count = if protected.is_some() { 2 } else { 1 };
            let rect = toolbar::context_menu_rect_avoiding(
                actions,
                canvas,
                size,
                &obstacles[..count],
                scale,
            );
            (rect, scale)
        });
        Some(TextMenus {
            actions,
            scale,
            palette,
        })
    }

    fn text_context_rect(&self, canvas: Rect) -> Option<Rect> {
        let point = self.input.text.context_menu?;
        let lines = if self.text_menu_item()?.text.is_empty() {
            1.
        } else {
            4.
        };
        let size = vec2(170., lines * 32. + 12.) * self.input.canvas_units;
        let min = pos2(
            point
                .x
                .clamp(canvas.left(), (canvas.right() - size.x).max(canvas.left())),
            point
                .y
                .clamp(canvas.top(), (canvas.bottom() - size.y).max(canvas.top())),
        );
        Some(Rect::from_min_size(min, size))
    }

    fn text_context_action(&mut self, action: TextContextAction) -> bool {
        use crate::sdl::{clipboard_text, set_clipboard_text};
        if action == TextContextAction::Paste && clipboard_text().is_none() {
            return false;
        }
        if self.input.text.draft.is_none() {
            let Some(index) = self.input.text.selected else {
                return false;
            };
            let Some(item) = self.history.text_boxes().get(index) else {
                return false;
            };
            if action == TextContextAction::Copy {
                set_clipboard_text(&item.text);
                return true;
            }
            self.input.text.edit(Some(index), (**item).clone());
        }
        let Some(draft) = self.input.text.draft.as_mut() else {
            return false;
        };
        let total = draft.item.text.chars().count();
        let selected = draft
            .cursor
            .map(|range| range.as_sorted_char_range())
            .filter(|range| range.start != range.end)
            .map(|range| (usize::from(range.start))..usize::from(range.end));
        let range = selected
            .as_ref()
            .map_or(0..total, |range| range.start..range.end);
        match action {
            TextContextAction::SelectAll => {
                draft.cursor = Some(CCursorRange::two(CCursor::new(0), CCursor::new(total)));
                draft.focus = true;
            }
            TextContextAction::Copy | TextContextAction::Cut => {
                let start = draft
                    .item
                    .text
                    .char_indices()
                    .nth(range.start)
                    .map_or(draft.item.text.len(), |(i, _)| i);
                let end = draft
                    .item
                    .text
                    .char_indices()
                    .nth(range.end)
                    .map_or(draft.item.text.len(), |(i, _)| i);
                set_clipboard_text(&draft.item.text[start..end]);
                if action == TextContextAction::Cut {
                    draft.item.text.delete_char_range(
                        egui::text::CharIndex(range.start)..egui::text::CharIndex(range.end),
                    );
                    draft.committed.clone_from(&draft.item.text);
                    draft.cursor = Some(CCursorRange::one(CCursor::new(range.start)));
                    draft.focus = true;
                }
            }
            TextContextAction::Paste => {
                let Some(clipboard) = clipboard_text() else {
                    return false;
                };
                let caret = selected.as_ref().map_or(
                    draft
                        .cursor
                        .map_or(total, |range| range.primary.index.into()),
                    |range| range.start,
                );
                let to_replace = selected.unwrap_or(caret..caret);
                let max = MAX_TEXT_CHARS
                    .saturating_sub(total.saturating_sub(to_replace.end - to_replace.start));
                let insertion: String = clipboard.chars().take(max).collect();
                draft.item.text.delete_char_range(
                    egui::text::CharIndex(to_replace.start)..egui::text::CharIndex(to_replace.end),
                );
                let inserted = draft
                    .item
                    .text
                    .insert_text(&insertion, egui::text::CharIndex(caret));
                draft.committed.clone_from(&draft.item.text);
                draft.cursor = Some(CCursorRange::one(CCursor::new(caret + inserted)));
                draft.focus = true;
            }
        }
        true
    }

    fn text_context_popup(&mut self, ui: &mut Ui, rect: Rect) {
        let tr = self.language;
        let has_text = self
            .text_menu_item()
            .is_some_and(|item| !item.text.is_empty());
        let mut action = None;
        overlay(ui, "annotation-context-popup", rect, |ui| {
            egui::Frame::new()
                .fill(ui.visuals().window_fill())
                .stroke(ui.visuals().window_stroke())
                .corner_radius(egui::CornerRadius::same(12))
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    let entries = [
                        (
                            TextContextAction::SelectAll,
                            tr.text("Select all", "全选", "全選"),
                        ),
                        (TextContextAction::Copy, tr.text("Copy", "拷贝", "複製")),
                        (TextContextAction::Paste, tr.text("Paste", "粘贴", "貼上")),
                        (TextContextAction::Cut, tr.text("Cut", "剪切", "剪下")),
                    ];
                    for (choice, title) in entries {
                        if !has_text && choice != TextContextAction::Paste {
                            continue;
                        }
                        if ui
                            .add_sized(
                                vec2(rect.width() - 12., 32. * self.input.canvas_units),
                                egui::Button::new(title),
                            )
                            .clicked()
                        {
                            action = Some(choice);
                        }
                    }
                });
        });
        if let Some(action) = action {
            self.input.text.context_menu = None;
            self.text_context_action(action);
            ui.ctx().request_repaint();
        }
    }

    pub(super) fn text_pick_target(&self) -> Option<TextPickTarget> {
        if !self.tool.is_text() || self.pass || self.hidden {
            return None;
        }
        Some(TextPickTarget {
            selected: self.input.text.selected,
            editing: self.input.text.editing(),
            item: self.text_menu_item()?.clone(),
            committed: self
                .input
                .text
                .draft
                .as_ref()
                .map(|draft| draft.committed.clone()),
        })
    }

    /// Compare a delayed pick with the current text state without copying the
    /// annotation or its editable buffer again when the native picker closes.
    pub(super) fn matches_text_pick_target(&self, target: &TextPickTarget) -> bool {
        self.tool.is_text()
            && !self.pass
            && !self.hidden
            && self.input.text.selected == target.selected
            && self.input.text.editing() == target.editing
            && self
                .text_menu_item()
                .is_some_and(|item| item == &target.item)
            && self
                .input
                .text
                .draft
                .as_ref()
                .map(|draft| draft.committed.as_str())
                == target.committed.as_deref()
    }

    pub(super) fn pick_text_color(&mut self, color: InkColor) -> Option<InkColor> {
        if !self.tool.is_text() || self.pass || self.hidden {
            return None;
        }
        // An immediate color pick only needs the current color. Do not copy
        // the full text box and committed draft as a delayed screen pick does.
        let current = self.text_menu_item()?.color;
        let color = color.converted(current.space);
        // Picking an already selected color is still a successful pick.
        (current == color || self.recolor_text(color)).then_some(color)
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

    fn text_menu(&mut self, ui: &mut Ui, menus: TextMenus) {
        let Some(item) = self.text_menu_item() else {
            return;
        };
        let color = item.color;
        let font_size = item.font_size;
        let tr = self.language;
        let scale = menus.scale;
        let rect = menus.actions;
        let button = MENU_BUTTON * scale;
        let font_step = 2. * self.input.canvas_units;
        ui.painter().rect(
            rect,
            8. * scale,
            ui.visuals().window_fill(),
            ui.visuals().window_stroke(),
            StrokeKind::Inside,
        );
        let mut menu = ui.new_child(
            UiBuilder::new()
                .id_salt("annotation-actions")
                .max_rect(rect.shrink(MENU_PADDING * scale))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        menu.spacing_mut().item_spacing.x = MENU_GAP * scale;
        menu.spacing_mut().button_padding = vec2(5., 4.) * scale;
        let mut acted = false;
        if annotation_icon_button(
            &mut menu,
            button,
            ActionIcon::Delete,
            tr.text("Delete text box", "删除文本框", "刪除文字框"),
        )
        .on_hover_text(
            egui::RichText::new(tr.text("Delete text box", "删除文本框", "刪除文字框"))
                .size(toolbar::MENU_FONT_SIZE * scale),
        )
        .clicked()
        {
            acted = self.delete_text_box();
        }
        for (icon, hint, step, enabled) in [
            (
                ActionIcon::Increase,
                tr.text("Increase font size", "加大字号", "加大字級"),
                font_step,
                font_size < *TextBox::FONT_SIZE.end(),
            ),
            (
                ActionIcon::Decrease,
                tr.text("Decrease font size", "缩小字号", "縮小字級"),
                -font_step,
                font_size > *TextBox::FONT_SIZE.start(),
            ),
        ] {
            if menu
                .add_enabled_ui(enabled, |ui| annotation_icon_button(ui, button, icon, hint))
                .inner
                .on_hover_text(egui::RichText::new(hint).size(toolbar::MENU_FONT_SIZE * scale))
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
            if toolbar::swatch_button(&mut menu, button, rgb, &label, color.srgb8() == rgb)
                .on_hover_text(egui::RichText::new(label).size(toolbar::MENU_FONT_SIZE * scale))
                .clicked()
            {
                acted |= self.recolor_text(preset.converted(color.space));
                self.input.text.color_wheel = None;
            }
        }
        let keyboard_label = tr.text("Show on-screen keyboard", "呼出屏幕键盘", "開啟螢幕鍵盤");
        let (icon_rect, keyboard_response) =
            menu.allocate_exact_size(KEYBOARD_BUTTON * scale, egui::Sense::click());
        keyboard_response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Button, menu.is_enabled(), keyboard_label)
        });
        if keyboard_response.hovered() || keyboard_response.has_focus() {
            menu.painter().rect_filled(
                icon_rect,
                6. * scale,
                menu.visuals().widgets.hovered.bg_fill,
            );
        }
        // A vector keyboard is legible on every OS; the previous Unicode
        // "⌨" was rendered as a tiny fallback glyph on some installations.
        let keyboard_rect = Rect::from_center_size(icon_rect.center(), vec2(31., 22.) * scale);
        let stroke = Stroke::new(2.0 * scale, menu.visuals().text_color());
        menu.painter()
            .rect_stroke(keyboard_rect, 3. * scale, stroke, StrokeKind::Inside);
        for row in 0..2 {
            for col in 0..5 {
                let center =
                    keyboard_rect.min + vec2(5. + 5.25 * col as f32, 6. + 6. * row as f32) * scale;
                menu.painter()
                    .circle_filled(center, 1.25 * scale, stroke.color);
            }
        }
        let spacebar = keyboard_rect.min + vec2(8., 18.) * scale;
        menu.painter()
            .line_segment([spacebar, spacebar + vec2(15., 0.) * scale], stroke);
        if keyboard_response
            .on_hover_text(
                egui::RichText::new(keyboard_label).size(toolbar::MENU_FONT_SIZE * scale),
            )
            .clicked()
        {
            acted |= self.request_text_keyboard();
        }
        let wheel_open = self.input.text.color_wheel.is_some();
        let label = tr.text("Color wheel", "色轮", "色輪");
        let (icon, response) = menu.allocate_exact_size(button, egui::Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::Button, menu.is_enabled(), wheel_open, label)
        });
        if wheel_open || response.hovered() || response.has_focus() {
            menu.painter()
                .rect_filled(icon, 6. * scale, menu.visuals().widgets.hovered.bg_fill);
        }
        color_wheel::paint_disc(menu.painter(), icon.center(), 12. * scale, 1.);
        if response
            .on_hover_text(egui::RichText::new(label).size(toolbar::MENU_FONT_SIZE * scale))
            .clicked()
            && self.text_menu_item().is_some()
        {
            self.input.text.color_wheel = (!wheel_open).then(|| ColorWheel::new(color));
            acted = true;
        }
        if acted {
            ui.ctx().request_repaint();
        }
    }

    fn text_color_wheel(&mut self, ui: &mut Ui, rect: Rect, scale: f32) {
        let Some(mut wheel) = self.input.text.color_wheel else {
            return;
        };
        let tr = self.language;
        ui.painter().rect(
            rect,
            8. * scale,
            ui.visuals().window_fill(),
            ui.visuals().window_stroke(),
            StrokeKind::Inside,
        );
        let inner = rect.shrink(PALETTE_MARGIN * scale);
        let wheel_diameter = (inner.height()
            - (PALETTE_SLIDER_HEIGHT + MENU_BUTTON.y + 2. * PALETTE_GAP) * scale)
            .max(0.)
            .min(inner.width());
        let mut palette = ui.new_child(UiBuilder::new().id_salt("wheel-controls").max_rect(inner));
        palette.set_width(inner.width());
        palette.spacing_mut().item_spacing = vec2(PALETTE_GAP, PALETTE_GAP) * scale;
        palette.spacing_mut().button_padding = vec2(6., 4.) * scale;
        palette.vertical_centered(|ui| wheel.show(ui, wheel_diameter, scale));
        palette.horizontal(|ui| {
            ui.set_width(inner.width());
            ui.label(
                egui::RichText::new(tr.text("Brightness", "亮度", "亮度"))
                    .size(toolbar::MENU_FONT_SIZE * scale),
            );
            let remaining = ui.available_width().max(0.);
            ui.spacing_mut().slider_width = remaining;
            ui.spacing_mut().interact_size.y = PALETTE_SLIDER_HEIGHT * scale;
            ui.add_sized(
                vec2(remaining, PALETTE_SLIDER_HEIGHT * scale),
                egui::Slider::new(&mut wheel.value.v, 0. ..=1.).show_value(false),
            );
        });
        let (apply, pick) = palette
            .horizontal(|ui| {
                ui.set_width(inner.width());
                let gap = ui.spacing().item_spacing.x;
                let button_width = ((ui.available_width() - gap) / 2.).max(0.);
                let size = vec2(button_width, MENU_BUTTON.y * scale);
                let apply = ui
                    .add_sized(
                        size,
                        toolbar::menu_button(tr.text("Apply", "应用", "套用"), scale),
                    )
                    .clicked();
                let pick = ui
                    .add_sized(
                        size,
                        toolbar::menu_button(tr.text("Pick screen", "屏幕取色", "螢幕取色"), scale),
                    )
                    .clicked();
                (apply, pick)
            })
            .inner;
        if apply || pick {
            self.input.text.color_wheel = None;
            self.input.text.screen_pick_requested |= pick;
            if apply && let Some(color) = wheel.chosen_color() {
                self.pick_text_color(color);
            }
            ui.ctx().request_repaint();
        } else {
            self.input.text.color_wheel = Some(wheel);
        }
    }

    pub(super) fn text_key(&mut self, key: crate::sdl::SDL_Keycode) -> bool {
        use crate::sdl::*;
        let state = &mut self.input.text;
        if key == SDLK_ESCAPE && state.color_wheel.take().is_some() {
            return true;
        }
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
            let screen = ui.clip_rect();
            let mut menus = self.text_menus(screen);
            if self.input.text.color_wheel.is_some() {
                let escape = !composing
                    && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape));
                let outside = ui.input(|input| {
                    input.pointer.primary_clicked()
                        && input
                            .pointer
                            .interact_pos()
                            .is_some_and(|point| menus.is_none_or(|menus| !menus.contains(point)))
                });
                if escape || outside {
                    self.input.text.color_wheel = None;
                    menus = self.text_menus(screen);
                }
            }
            if self.input.text.editing() {
                self.text_editor(ui, composing, keep_editor_open, menus);
            } else if let Some(item) =
                selected.and_then(|index| self.history.text_boxes().get(index))
            {
                let rect = preview.unwrap_or(item).rect();
                let scale = self.input.canvas_units;
                let [resize, font] = handles(rect, scale);
                let color = crate::toolbar::ACTIVE;
                let painter = ui.painter();
                painter.rect_stroke(
                    rect,
                    3. * scale,
                    Stroke::new(1.25 * scale, color),
                    StrokeKind::Inside,
                );
                painter.rect_filled(resize.shrink(2. * scale), 2. * scale, color);
                painter.circle_filled(
                    font.center(),
                    (font.width() / 2. - 2. * scale).max(1.),
                    color,
                );
            }
            if let Some(menus) = menus {
                if self.input.text.editing() {
                    overlay(ui, "annotation-menu", menus.actions, |ui| {
                        self.text_menu(ui, menus)
                    });
                    if ui.input(|input| {
                        (input.pointer.any_pressed() || input.pointer.any_released())
                            && input
                                .pointer
                                .interact_pos()
                                .is_some_and(|point| menus.contains(point))
                    }) {
                        let id = ui.make_persistent_id("annotation-editor");
                        ui.memory_mut(|mem| mem.request_focus(id));
                    }
                } else {
                    self.text_menu(ui, menus);
                }
                if let Some((rect, scale)) = self.text_menus(screen).and_then(|menus| menus.palette)
                {
                    overlay(ui, "annotation-palette", rect, |ui| {
                        self.text_color_wheel(ui, rect, scale)
                    });
                }
            }
            if let Some(rect) = self.text_context_rect(screen) {
                self.text_context_popup(ui, rect);
            }
        }
    }

    fn text_editor(
        &mut self,
        ui: &mut Ui,
        composing: bool,
        keep_editor_open: bool,
        menus: Option<TextMenus>,
    ) {
        let wheel_open = self.input.text.color_wheel.is_some();
        let context_rect = self.text_context_rect(ui.clip_rect());
        let Some(draft) = self.input.text.draft.as_mut() else {
            return;
        };
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
            3. * self.input.canvas_units,
            Stroke::new(1.25 * self.input.canvas_units, crate::toolbar::ACTIVE),
            StrokeKind::Inside,
        );
        let (focused, finish, committed_input, menu_clicked) = ui.input(|input| {
            // A pointer can start editing before SDL grants keyboard focus.
            // Keep the draft available to toolbar actions such as Redo or Clear.
            let lost_window_focus = !keep_editor_open && !input.focused
                && (draft.had_input_focus
                    || input.events.iter().any(|event| matches!(event, Event::WindowFocused(false))));
            let clicked = input.pointer.primary_clicked().then(|| input.pointer.interact_pos()).flatten();
            let menu_pointer = input.pointer.interact_pos().is_some_and(|point| {
                menus.is_some_and(|menus| menus.contains(point))
                    || context_rect.is_some_and(|rect| rect.contains(point))
            });
            let finish = lost_window_focus
                || (!composing
                    && (input.key_pressed(Key::Escape)
                        || input.events.iter().any(|event| {
                            matches!(event, Event::Key {
                    key: Key::Enter, pressed: true, modifiers, ..
                } if modifiers.command)
                        })))
                || (!menu_pointer && clicked.is_some_and(|point| !rect.contains(point)));
            let committed_input = input.events.iter().any(|event| {
                matches!(event, Event::Text(value) | Event::Paste(value) | Event::Ime(ImeEvent::Commit(value)) if !value.is_empty())
            });
            (input.focused, finish, committed_input, menu_pointer)
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
            || (focused
                && !started
                && !composing
                && !wheel_open
                && !menu_clicked
                && output.response.lost_focus())
        {
            self.input.text.finish(&mut self.history);
            ui.memory_mut(|mem| mem.surrender_focus(id));
            ui.ctx().request_repaint();
        }
    }
}

#[derive(Clone, Copy)]
enum ActionIcon {
    Delete,
    Increase,
    Decrease,
}

/// Native vector paths replace font-specific ×, A+ and A− glyphs. Geometry,
/// stroke and contrast follow the actual allocated button size/DPI.
fn annotation_icon_button(
    ui: &mut Ui,
    size: Vec2,
    icon: ActionIcon,
    label: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(
            rect,
            6. * (size.y / MENU_BUTTON.y),
            ui.visuals().widgets.hovered.bg_fill,
        );
    }
    let scale = size.y / MENU_BUTTON.y;
    let color = if ui.is_enabled() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let stroke = Stroke::new(2. * scale, color);
    let at = |x: f32, y: f32| rect.center() + vec2(x, y) * scale;
    let draw = |a: (f32, f32), b: (f32, f32)| {
        ui.painter()
            .line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
    };
    match icon {
        ActionIcon::Delete => {
            // Trash bin: handle, lid, body and dividers.
            draw((-8., -7.), (8., -7.));
            draw((-3., -10.), (3., -10.));
            draw((-3., -10.), (-3., -7.));
            draw((3., -10.), (3., -7.));
            draw((-6., -5.), (-5., 9.));
            draw((-5., 9.), (5., 9.));
            draw((5., 9.), (6., -5.));
            draw((-2., -3.), (-2., 6.));
            draw((2., -3.), (2., 6.));
        }
        ActionIcon::Increase | ActionIcon::Decrease => {
            // A is drawn with line segments, never with an OS font fallback.
            draw((-11., 9.), (-5., -9.));
            draw((-5., -9.), (1., 9.));
            draw((-9., 3.), (-1., 3.));
            draw((7., 0.), (15., 0.));
            if matches!(icon, ActionIcon::Increase) {
                draw((11., -4.), (11., 4.));
            }
        }
    }
    response
}

/// Use exactly the text action bar's width. The palette's height (and its
/// round wheel) is fitted into the larger unobstructed vertical side of the
/// toolbar, then clipped by the canvas. No independent fixed palette size.
fn palette_layout(actions: Rect, canvas: Rect, scale: f32) -> (Vec2, f32) {
    let controls =
        (2. * PALETTE_MARGIN + 2. * PALETTE_GAP + PALETTE_SLIDER_HEIGHT + MENU_BUTTON.y) * scale;
    let below = (canvas.bottom() - actions.bottom() - MENU_GAP * scale).max(0.);
    let above = (actions.top() - canvas.top() - MENU_GAP * scale).max(0.);
    let max_height = below
        .max(above)
        .min(canvas.height() - 2. * MENU_GAP * scale);
    let wheel =
        (actions.width() - 2. * PALETTE_MARGIN * scale).min((max_height - controls).max(0.));
    (vec2(actions.width(), controls + wheel), scale)
}

fn overlay(ui: &Ui, id: &str, rect: Rect, content: impl FnOnce(&mut Ui)) {
    let clip = ui.clip_rect();
    egui::Area::new(ui.make_persistent_id(id))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .default_size(rect.size())
        .constrain(false)
        .fade_in(false)
        .show(ui.ctx(), |ui| {
            ui.set_clip_rect(clip);
            ui.set_min_size(rect.size());
            content(ui);
        });
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
    let size = Vec2::splat((20. * scale).min(rect.height() / 2.));
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

    fn draft_preview(app: &App) -> Option<&str> {
        app.input
            .text
            .draft
            .as_ref()
            .map(|draft| draft.item.text.as_str())
    }

    fn draft_committed(app: &App) -> Option<&str> {
        app.input
            .text
            .draft
            .as_ref()
            .map(|draft| draft.committed.as_str())
    }

    fn set_draft_committed(app: &mut App, text: &str) {
        let Some(draft) = app.input.text.draft.as_mut() else {
            panic!("Expected an active text draft");
        };
        draft.committed.clear();
        draft.committed.push_str(text);
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
        let Ok(value) = CString::new(value) else {
            panic!("IME test strings must not contain NUL bytes");
        };
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
        assert_eq!(draft_preview(&app), Some("zhong"));
        assert_eq!(draft_committed(&app), Some(""));

        key_down(&mut gui, SDLK_RETURN);
        ime(&mut gui, SDL_EVENT_TEXT_INPUT, "中");
        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "wen");
        render(&mut app, &mut gui);
        assert_eq!(draft_preview(&app), Some("中wen"));
        assert_eq!(draft_committed(&app), Some("中"));
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
            assert_eq!(draft_committed(&app), Some("甲"));
        }

        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "yi");
        render(&mut app, &mut gui);
        ime_at(&mut gui, SDL_EVENT_TEXT_EDITING, "", 20_000_000);
        ime_at(&mut gui, SDL_EVENT_TEXT_INPUT, "乙", 20_150_000);
        key_down_at(&mut gui, SDLK_RETURN, 20_200_000);
        render(&mut app, &mut gui);
        assert_eq!(draft_committed(&app), Some("甲乙"));

        ime(&mut gui, SDL_EVENT_TEXT_EDITING, "bing");
        render(&mut app, &mut gui);
        ime_at(&mut gui, SDL_EVENT_TEXT_INPUT, "丙", 30_000_000);
        key_down_at(&mut gui, SDLK_RETURN, 30_400_000);
        render(&mut app, &mut gui);
        assert_eq!(draft_committed(&app), Some("甲乙丙"));

        key_down(&mut gui, SDLK_RETURN);
        render(&mut app, &mut gui);
        assert_eq!(draft_committed(&app), Some("甲乙丙\n"));
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
            assert_eq!(draft_committed(&app), Some(expected));
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
        set_draft_committed(&mut app, "原文");
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
                set_draft_committed(&mut app, "原批注");
                app.stop();
            }
            let previous = app.history.text_boxes().to_vec();
            click(&mut app, pos2(470., 280.));
            set_draft_committed(&mut app, "重要批注");
            assert!(app.history_buttons()[0]);
            app.change_history(HistoryAction::Undo);
            assert!(!app.input.text.editing());
            assert_eq!(app.history.text_boxes(), previous);
            assert_eq!(app.history.can_undo(), has_prior_box);
            app.change_history(HistoryAction::Redo);
            assert_eq!(
                app.history
                    .text_boxes()
                    .last()
                    .map(|item| item.text.as_str()),
                Some("重要批注")
            );
        }
    }

    #[test]
    fn clearing_the_first_written_draft_is_one_undoable_action() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        set_draft_committed(&mut app, "首框");
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
        let Some(expires) = focus.deadline else {
            panic!("expected pending toolbar focus deadline");
        };
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
        set_draft_committed(&mut app, "首行\n\n\n\n\n末行");
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
        let Ok(serialized) = serde_json::to_string(item) else {
            panic!("failed to serialize the scrollable text box");
        };
        let Ok(restored) = serde_json::from_str::<TextBox>(&serialized) else {
            panic!("failed to deserialize the scrollable text box");
        };
        assert_eq!(&restored, item.as_ref());
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
        assert_eq!(draft_committed(&app), Some("甲乙丙"));
    }

    #[test]
    fn switching_active_display_finishes_only_its_draft() {
        let mut app = app();
        app.multi = true;
        app.pages = crate::display::Pages::new(Some(1));
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        set_draft_committed(&mut app, "显示器一");
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
        let mut item = TextBox::new(pos2(20., 20.), rect, app.color, 1.);
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
        set_draft_committed(&mut app, "文字");
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
    fn repeated_drag_motion_uses_absolute_offset_and_preserves_text() -> Result<(), &'static str> {
        let mut app = app();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut item = TextBox::new(pos2(150., 120.), screen, InkColor::default(), 1.);
        item.text = "中文 👩🏽‍🎨".into();
        let index = app
            .history
            .set_text_box(None, item)
            .ok_or("Cannot add text box")?;
        app.input.text.press = Some(Press {
            origin: pos2(150., 120.),
            target: Some((index, Handle::Move)),
            preview: None,
            moved: false,
        });

        app.move_text_pointer(pos2(180., 150.), screen);
        app.move_text_pointer(pos2(200., 160.), screen);
        let preview = app
            .input
            .text
            .press
            .as_ref()
            .and_then(|press| press.preview.as_ref())
            .ok_or("Missing drag preview")?;
        assert_eq!(preview.origin, [200., 160.]);
        assert_eq!(preview.text, "中文 👩🏽‍🎨");
        assert_eq!(app.history.text_boxes()[index].origin, [150., 120.]);
        Ok(())
    }

    #[test]
    fn moving_resizing_and_deleting_text_boxes_are_undoable() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        set_draft_committed(&mut app, "中文 👩🏽‍🎨");
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
        assert_eq!(edited.font_size, original.font_size + 15.);
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
        let mut item = TextBox::new(pos2(500., 300.), screen, InkColor::default(), 1.);
        item.text = "Selected text".into();
        let Some(index) = app.history.set_text_box(None, item.clone()) else {
            panic!("expected new text box index");
        };
        app.tool = Tool::Select;
        assert!(app.text_menus(screen).is_none());
        click(&mut app, item.rect().center());
        let Some(menu) = app.text_menus(screen).map(|menus| menus.actions) else {
            panic!("expected annotation menu actions");
        };
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
        let moved_rect = item.rect();
        app.history.set_text_box(Some(index), item);
        let Some(menu) = app.text_menus(screen).map(|menus| menus.actions) else {
            panic!("expected annotation menu actions");
        };
        assert!(moved_rect.intersects(menu));
        assert!(screen.contains_rect(menu));
        click(&mut app, pos2(50., 50.));
        assert!(app.text_menus(screen).is_none());
    }

    #[test]
    fn second_finger_tap_requests_keyboard_even_with_emulated_mouse_events() {
        let mut app = app();
        app.use_tool(Tool::Select);
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut item = TextBox::new(pos2(200., 150.), canvas, app.color, 1.);
        item.text = "中文输入".into();
        assert!(app.history.set_text_box(None, item).is_some());
        let window = SDL_WindowID(1);
        let mut gui = Gui::new();
        let finger = |kind| SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: kind,
                windowID: window,
                touchID: SDL_TouchID(7),
                fingerID: SDL_FingerID(12),
                x: 0.3,
                y: 0.3,
                ..Default::default()
            },
        };
        let simulated_mouse = |kind, down| SDL_Event {
            button: SDL_MouseButtonEvent {
                r#type: kind,
                windowID: window,
                which: SDL_TOUCH_MOUSEID,
                button: SDL_BUTTON_LEFT as u8,
                down,
                x: 240.,
                y: 180.,
                ..Default::default()
            },
        };

        for tap in 0..2 {
            app.text_event(&mut gui, &finger(SDL_EVENT_FINGER_DOWN), window, SIZE);
            app.text_event(
                &mut gui,
                &simulated_mouse(SDL_EVENT_MOUSE_BUTTON_DOWN, true),
                window,
                SIZE,
            );
            app.text_event(
                &mut gui,
                &simulated_mouse(SDL_EVENT_MOUSE_BUTTON_UP, false),
                window,
                SIZE,
            );
            app.text_event(&mut gui, &finger(SDL_EVENT_FINGER_UP), window, SIZE);
            assert_eq!(app.input.text.editing(), tap == 1);
            assert_eq!(app.input.text.take_keyboard_request(), tap == 1);
        }
        // Subsequent finger taps reopen an OSK dismissed while editing.
        app.text_event(&mut gui, &finger(SDL_EVENT_FINGER_DOWN), window, SIZE);
        app.text_event(&mut gui, &finger(SDL_EVENT_FINGER_UP), window, SIZE);
        assert!(app.input.text.take_keyboard_request());
    }

    #[test]
    fn native_mouse_pen_and_finger_clicks_open_the_selected_text_keyboard() {
        for device in ["mouse", "pen", "finger"] {
            let mut app = app();
            app.use_tool(Tool::Select);
            let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
            let mut item = TextBox::new(pos2(200., 150.), canvas, app.color, 1.);
            item.text = "Keyboard".into();
            assert!(app.history.set_text_box(None, item).is_some());
            app.input.text.selected = Some(0);
            let point = app
                .keyboard_button_rect(canvas)
                .expect("selected item has keyboard button")
                .center();
            let window = SDL_WindowID(1);
            let mut gui = Gui::new();
            for down in [true, false] {
                let event = match device {
                    "mouse" => SDL_Event {
                        button: SDL_MouseButtonEvent {
                            r#type: if down {
                                SDL_EVENT_MOUSE_BUTTON_DOWN
                            } else {
                                SDL_EVENT_MOUSE_BUTTON_UP
                            },
                            windowID: window,
                            which: SDL_MouseID(1),
                            button: SDL_BUTTON_LEFT as u8,
                            down,
                            x: point.x,
                            y: point.y,
                            ..Default::default()
                        },
                    },
                    "pen" => SDL_Event {
                        ptouch: SDL_PenTouchEvent {
                            r#type: if down {
                                SDL_EVENT_PEN_DOWN
                            } else {
                                SDL_EVENT_PEN_UP
                            },
                            windowID: window,
                            down,
                            x: point.x,
                            y: point.y,
                            ..Default::default()
                        },
                    },
                    _ => SDL_Event {
                        tfinger: SDL_TouchFingerEvent {
                            r#type: if down {
                                SDL_EVENT_FINGER_DOWN
                            } else {
                                SDL_EVENT_FINGER_UP
                            },
                            windowID: window,
                            touchID: SDL_TouchID(45),
                            fingerID: SDL_FingerID(9),
                            x: point.x / SIZE[0],
                            y: point.y / SIZE[1],
                            ..Default::default()
                        },
                    },
                };
                app.text_event(&mut gui, &event, window, SIZE);
                assert_eq!(app.input.text.take_keyboard_request(), !down, "{device}");
            }
            assert!(app.input.text.editing(), "{device}");
        }
    }

    #[test]
    fn keyboard_click_requires_release_inside_and_does_not_create_text_box() {
        let mut app = app();
        app.use_tool(Tool::Select);
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut item = TextBox::new(pos2(150., 120.), canvas, app.color, 1.);
        item.text = "Test".into();
        assert!(app.history.set_text_box(None, item).is_some());
        app.input.text.selected = Some(0);
        let area = app.keyboard_button_rect(canvas).expect("keyboard menu");
        let down = Event::PointerButton {
            pos: area.center(),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        };
        let release = |pos| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        };
        let outside_up = release(Pos2::ZERO);
        assert!(app.keyboard_pointer(&down, canvas));
        assert!(!app.keyboard_pointer(&outside_up, canvas));
        assert!(!app.input.text.editing());
        assert!(!app.input.text.take_keyboard_request());
        assert!(app.keyboard_pointer(&down, canvas));
        assert!(app.keyboard_pointer(&release(area.center()), canvas));
        assert_eq!(app.history.text_boxes().len(), 1);
        assert!(app.input.text.editing());
        assert!(app.input.text.take_keyboard_request());
    }

    #[test]
    fn mouse_editing_does_not_request_keyboard_automatically() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        assert!(app.input.text.editing());
        assert!(!app.input.text.take_keyboard_request());
    }

    #[test]
    fn a_new_draft_menu_works_over_the_editor_at_the_bottom_edge() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(520., 555.));
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let Some(rect) = app.text_menus(screen).map(|menus| menus.actions) else {
            panic!("expected draft menu actions");
        };
        let Some(draft) = app.input.text.draft.as_ref() else {
            panic!("expected active text draft");
        };
        assert!(draft.item.rect().intersects(rect));
        let font = draft.item.font_size;
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        render(&mut app, &mut gui);
        render(&mut app, &mut gui);
        let mut menu_click = |app: &mut App, column: f32| {
            let pos = rect.min
                + vec2(
                    MENU_PADDING + MENU_BUTTON.x * 0.5 + column * (MENU_BUTTON.x + MENU_GAP),
                    MENU_SIZE.y * 0.5,
                );
            gui.raw.events.push(Event::PointerMoved(pos));
            for pressed in [true, false] {
                gui.raw.events.push(Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
                render(app, &mut gui);
            }
        };
        menu_click(&mut app, 1.);
        assert_eq!(
            app.input
                .text
                .draft
                .as_ref()
                .map(|draft| draft.item.font_size),
            Some(font + 2.)
        );
        menu_click(&mut app, 7.);
        assert!(app.input.text.take_keyboard_request());
        menu_click(&mut app, 0.);
        assert!(!app.input.text.editing());
        assert!(app.history.text_boxes().is_empty());
    }

    #[test]
    fn selection_motion_only_repaints_when_it_interacts_with_the_menu() {
        let mut app = app();
        let mut gui = Gui::new();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut item = TextBox::new(pos2(350., 100.), screen, InkColor::default(), 1.);
        item.text = "Annotation".into();
        app.input.text.selected = app.history.set_text_box(None, item);
        let Some(menu) = app.text_menus(screen).map(|menus| menus.actions) else {
            panic!("expected annotation menu actions");
        };
        let window = SDL_WindowID(1);
        let motion = |p: Pos2| SDL_Event {
            motion: SDL_MouseMotionEvent {
                r#type: SDL_EVENT_MOUSE_MOTION,
                windowID: window,
                x: p.x,
                y: p.y,
                ..Default::default()
            },
        };
        assert!(!app.text_event(&mut gui, &motion(Pos2::ZERO), window, SIZE));
        assert!(gui.raw.events.is_empty());
        assert!(app.text_event(&mut gui, &motion(menu.center()), window, SIZE));
        assert!(app.text_event(&mut gui, &motion(Pos2::ZERO), window, SIZE));
        assert!(!app.text_event(&mut gui, &motion(pos2(1., 1.)), window, SIZE));
        assert_eq!(gui.raw.events.len(), 2);
    }

    #[test]
    fn new_annotations_and_controls_scale_with_the_canvas() {
        let mut app = app();
        app.use_tool(Tool::Text);
        app.input.canvas_units = 2.;
        let size = [1920., 1080.];
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        for pressed in [true, false] {
            assert!(app.text_pointer(
                &Event::PointerButton {
                    pos: pos2(100., 100.),
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
                size
            ));
        }
        let Some(draft) = app.input.text.draft.as_ref() else {
            panic!("expected scaled text draft");
        };
        let item = &draft.item;
        assert_eq!(item.size, [640., 224.]);
        assert_eq!(item.font_size, 2. * TextBox::DEFAULT_FONT_SIZE);
        let Some(menus) = app.text_menus(screen) else {
            panic!("expected scalable annotation menus");
        };
        assert_eq!(menus.actions.size(), MENU_SIZE * 2.);
        assert!(screen.contains_rect(menus.actions));
        let narrow = Rect::from_min_size(Pos2::ZERO, vec2(480., 650.));
        assert_eq!(
            app.text_menus(narrow)
                .map(|menus| narrow.contains_rect(menus.actions)),
            Some(true)
        );
    }

    #[test]
    fn palette_tracks_text_menu_width_and_available_window_height() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        app.input.text.color_wheel = Some(ColorWheel::new(app.color));
        for screen_size in [vec2(800., 600.), vec2(500., 420.), vec2(1100., 700.)] {
            let screen = Rect::from_min_size(Pos2::ZERO, screen_size);
            let menus = app.text_menus(screen).expect("text annotation menu");
            let (palette, scale) = menus.palette.expect("color palette");
            assert!((palette.width() - menus.actions.width()).abs() < 0.01);
            assert!(screen.contains_rect(menus.actions));
            assert!(palette.width() > 0.);
            assert!(palette.height() >= 0.);
            assert!(scale > 0.);
        }
    }

    #[test]
    fn annotation_palette_requests_a_screen_pick_without_finishing_a_draft() {
        let mut app = app();
        app.use_tool(Tool::Text);
        let original_brush = app.color;
        click(&mut app, pos2(500., 338.));
        set_draft_committed(&mut app, "Draft");
        app.input.text.refocus();
        let mut gui = Gui::new();
        gui.ctx.set_fonts(app.text_fonts.definitions());
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(SIZE));
        let mut tap = |app: &mut App, point: Pos2| {
            render(app, &mut gui);
            render(app, &mut gui);
            gui.raw.events.push(Event::PointerMoved(point));
            for pressed in [true, false] {
                gui.raw.events.push(Event::PointerButton {
                    pos: point,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
                render(app, &mut gui);
            }
        };

        let Some(row) = app.text_menus(screen).map(|menus| menus.actions) else {
            panic!("expected annotation palette row");
        };
        let wheel_button = row.right_center() - vec2(MENU_PADDING + MENU_BUTTON.x * 0.5, 0.);
        tap(&mut app, wheel_button);
        let Some((palette, scale)) = app.text_menus(screen).and_then(|menus| menus.palette) else {
            panic!("expected text color palette");
        };
        assert!(screen.contains_rect(palette));
        assert!(app.input.text.editing());
        assert!((palette.width() - row.width()).abs() < 0.01);
        let inner = palette.shrink(PALETTE_MARGIN * scale);
        let wheel_size = (inner.height()
            - (PALETTE_SLIDER_HEIGHT + MENU_BUTTON.y + 2. * PALETTE_GAP) * scale)
            .max(0.)
            .min(inner.width());
        let wheel_center = pos2(inner.center().x, inner.top() + wheel_size * 0.5);
        tap(&mut app, wheel_center + vec2(wheel_size * 0.3, 0.));
        let Some(color) = app
            .input
            .text
            .color_wheel
            .and_then(ColorWheel::chosen_color)
        else {
            panic!("expected color-wheel selection");
        };
        tap(
            &mut app,
            pos2(
                inner.left() + inner.width() * 0.25,
                inner.bottom() - MENU_BUTTON.y * scale * 0.5,
            ),
        );
        assert_eq!(
            app.input.text.draft.as_ref().map(|draft| draft.item.color),
            Some(color)
        );
        assert_eq!(app.color, original_brush);
        assert!(app.input.text.color_wheel.is_none());

        tap(&mut app, wheel_button);
        let (palette, scale) = app
            .text_menus(screen)
            .and_then(|menus| menus.palette)
            .expect("palette remains available after reopening");
        let inner = palette.shrink(PALETTE_MARGIN * scale);
        tap(
            &mut app,
            pos2(
                inner.left() + inner.width() * 0.75,
                inner.bottom() - MENU_BUTTON.y * scale * 0.5,
            ),
        );
        assert!(app.input.text.take_screen_pick());
        assert!(!app.input.text.take_screen_pick());
        assert!(app.input.text.color_wheel.is_none());
        let sampled = InkColor::from_rgb8([4, 171, 131]);
        assert_eq!(app.pick_text_color(sampled), Some(sampled));
        let Some(draft) = app.input.text.draft.as_ref() else {
            panic!("expected active text draft");
        };
        assert_eq!(draft.committed, "Draft");
        assert_eq!(draft.item.color, sampled);
        assert_eq!(app.color, original_brush);
        assert!(app.history.text_boxes().is_empty());
    }

    #[test]
    fn screen_pick_rejects_stale_targets_and_invalid_modes() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        let Some(target) = app.text_pick_target() else {
            panic!("Expected a draft text pick target");
        };
        assert!(app.matches_text_pick_target(&target));
        let color = InkColor::from_rgb8([4, 171, 131]);

        // A different text draft is not the target captured when picking began.
        set_draft_committed(&mut app, "changed");
        assert!(!app.matches_text_pick_target(&target));
        assert_ne!(app.text_pick_target().as_ref(), Some(&target));
        assert_eq!(app.pick_text_color(color), Some(color));
        assert_eq!(app.pick_text_color(color), Some(color));

        app.pass = true;
        assert!(!app.matches_text_pick_target(&target));
        assert!(app.text_pick_target().is_none());
        assert!(app.pick_text_color(color).is_none());
        app.pass = false;
        app.hidden = true;
        assert!(!app.matches_text_pick_target(&target));
        assert!(app.text_pick_target().is_none());
        assert!(app.pick_text_color(color).is_none());
        app.hidden = false;
        app.input.text = TextInput::default();
        assert!(!app.matches_text_pick_target(&target));
        assert!(app.text_pick_target().is_none());
        assert!(app.pick_text_color(color).is_none());
    }

    #[test]
    fn presets_recolor_drafts_and_undo_cancels_a_drag() {
        let mut app = app();
        app.use_tool(Tool::Text);
        click(&mut app, pos2(150., 120.));
        assert!(app.select_preset(crate::color::PRESETS[1]));
        set_draft_committed(&mut app, "文字");
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
