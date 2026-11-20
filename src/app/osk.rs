#![forbid(unsafe_code)]

//! OverInk's own egui on-screen keyboard.
//! Interaction and page hierarchy are independently implemented in safe Rust,
//! informed by the GNOME Shell OSK's public UX (GPL-2.0-or-later). No GNOME
//! JavaScript, layouts, stylesheets or data files are incorporated here.
//! The IME and candidate window always belong to the operating system.
use std::borrow::Cow;

use egui::{Color32, CornerRadius, Rect, RichText, Stroke, StrokeKind, Ui, pos2, vec2};

mod emoji_data;
use emoji_data::{ACTIVITIES, FACES, FOOD, HEARTS, NATURE, OBJECTS, PEOPLE, SYMBOLS_EMOJI, TRAVEL};

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    #[default]
    Letters,
    Symbols,
    ExtraSymbols,
    Emoji,
}

#[derive(Default)]
pub(super) struct Keyboard {
    pub visible: bool,
    page: Page,
    shift: bool,
    emoji_category: usize,
    emoji_page: usize,
    // The page at the start of a grid drag; changing the displayed page
    // must not also move the gesture origin.
    emoji_drag_start: usize,
    emoji_wheel_remainder: f32,
    // Fractional page position. A new gesture retargets the running animation
    // without snapping, including when the user immediately reverses direction.
    emoji_display: f32,
    // Actual egui Window geometry for pointer-focus protection and resizing.
    window_rect: Option<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    Insert(Cow<'static, str>),
    Backspace,
    Return,
    MoveLeft,
    MoveRight,
    SwitchInputMethod,
    Hide,
}

#[derive(Clone, Copy)]
enum Key {
    Text(&'static str),
    Delete,
    Return,
    Shift,
    LetterPage,
    SymbolsPage,
    ExtraPage,
    EmojiPage,
    Space,
    InputMethod,
    Hide,
    Left,
    Right,
}

#[derive(Clone, Copy)]
struct Cell {
    key: Key,
    weight: f32,
}
impl Cell {
    const fn text(value: &'static str) -> Self {
        Self {
            key: Key::Text(value),
            weight: 1.,
        }
    }
    const fn wide(key: Key, weight: f32) -> Self {
        Self { key, weight }
    }
}

const DELETE: Cell = Cell::wide(Key::Delete, 1.35);
const ENTER: Cell = Cell::wide(Key::Return, 1.7);
const SHIFT: Cell = Cell::wide(Key::Shift, 1.35);
const SYMBOLS: Cell = Cell::wide(Key::SymbolsPage, 1.7);
const LETTERS: Cell = Cell::wide(Key::LetterPage, 1.7);
const EXTRA: Cell = Cell::wide(Key::ExtraPage, 1.7);
const EMOJI: Cell = Cell::wide(Key::EmojiPage, 1.2);
const INPUT: Cell = Cell::wide(Key::InputMethod, 1.2);
const HIDE: Cell = Cell::wide(Key::Hide, 1.2);
const SPACE: Cell = Cell::wide(Key::Space, 4.5);
const LEFT: Cell = Cell::wide(Key::Left, 1.);
const RIGHT: Cell = Cell::wide(Key::Right, 1.);

const LETTER_ROW_1: [Cell; 11] = [
    Cell::text("q"),
    Cell::text("w"),
    Cell::text("e"),
    Cell::text("r"),
    Cell::text("t"),
    Cell::text("y"),
    Cell::text("u"),
    Cell::text("i"),
    Cell::text("o"),
    Cell::text("p"),
    DELETE,
];
const LETTER_ROW_2: [Cell; 10] = [
    Cell::text("a"),
    Cell::text("s"),
    Cell::text("d"),
    Cell::text("f"),
    Cell::text("g"),
    Cell::text("h"),
    Cell::text("j"),
    Cell::text("k"),
    Cell::text("l"),
    ENTER,
];
const LETTER_ROW_3: [Cell; 11] = [
    SHIFT,
    Cell::text("z"),
    Cell::text("x"),
    Cell::text("c"),
    Cell::text("v"),
    Cell::text("b"),
    Cell::text("n"),
    Cell::text("m"),
    Cell::text(","),
    Cell::text("."),
    SHIFT,
];
const LETTER_ROW_4: [Cell; 5] = [SYMBOLS, EMOJI, SPACE, INPUT, HIDE];

const NUM_ROW_1: [Cell; 11] = [
    Cell::text("1"),
    Cell::text("2"),
    Cell::text("3"),
    Cell::text("4"),
    Cell::text("5"),
    Cell::text("6"),
    Cell::text("7"),
    Cell::text("8"),
    Cell::text("9"),
    Cell::text("0"),
    DELETE,
];
const NUM_ROW_2: [Cell; 10] = [
    Cell::text("@"),
    Cell::text("#"),
    Cell::text("$"),
    Cell::text("%"),
    Cell::text("&"),
    Cell::text("-"),
    Cell::text("+"),
    Cell::text("("),
    Cell::text(")"),
    ENTER,
];
const NUM_ROW_3: [Cell; 9] = [
    EXTRA,
    Cell::text("*"),
    Cell::text("\""),
    Cell::text("'"),
    Cell::text(":"),
    Cell::text(";"),
    Cell::text("!"),
    Cell::text("?"),
    EXTRA,
];
const NUM_ROW_4: [Cell; 9] = [
    LETTERS,
    Cell::text("_"),
    Cell::text("/"),
    Cell::wide(Key::Space, 3.),
    Cell::text(","),
    Cell::text("."),
    EMOJI,
    INPUT,
    HIDE,
];

const EXTRA_ROW_1: [Cell; 11] = [
    Cell::text("~"),
    Cell::text("`"),
    Cell::text("|"),
    Cell::text("·"),
    Cell::text("√"),
    Cell::text("π"),
    Cell::text("÷"),
    Cell::text("×"),
    Cell::text("¶"),
    Cell::text("Δ"),
    DELETE,
];
const EXTRA_ROW_2: [Cell; 10] = [
    Cell::text("£"),
    Cell::text("¢"),
    Cell::text("€"),
    Cell::text("¥"),
    Cell::text("^"),
    Cell::text("°"),
    Cell::text("="),
    Cell::text("{"),
    Cell::text("}"),
    ENTER,
];
const EXTRA_ROW_3: [Cell; 9] = [
    SYMBOLS,
    Cell::text("\\"),
    Cell::text("©"),
    Cell::text("®"),
    Cell::text("™"),
    Cell::text("℅"),
    Cell::text("["),
    Cell::text("]"),
    SYMBOLS,
];
const EXTRA_ROW_4: [Cell; 9] = [
    LETTERS,
    LEFT,
    RIGHT,
    Cell::wide(Key::Space, 3.),
    Cell::text(","),
    Cell::text("."),
    EMOJI,
    INPUT,
    HIDE,
];

// Unicode 18 fully-qualified Emoji list lives in emoji_data.rs. Its nine
// independently generated sections match GNOME's category boundaries,
// including all Unicode regional and subdivision flag sequences.
const EMOJI_CATEGORIES: &[(&str, &[&str])] = &[
    ("😀", FACES),
    ("👍", PEOPLE),
    ("🌷", NATURE),
    ("🍴", FOOD),
    ("✈️", TRAVEL),
    ("🏃", ACTIVITIES),
    ("🔔", OBJECTS),
    ("❤️", HEARTS),
    ("🚩", SYMBOLS_EMOJI),
];
const EMOJI_COLUMNS: usize = 11;
const EMOJI_ROWS: usize = 3;
const EMOJI_PAGE_SIZE: usize = EMOJI_COLUMNS * EMOJI_ROWS;

// Both horizontal and vertical drags turn the fixed-size emoji grid into a
// pager. Direction follows content movement: drag left/up for later pages.
fn emoji_page_from_drag(origin: usize, last: usize, delta: egui::Vec2, step: f32) -> usize {
    let axis = if delta.x.abs() >= delta.y.abs() {
        delta.x
    } else {
        delta.y
    };
    if !axis.is_finite() {
        return origin.min(last);
    }
    let steps = (axis.abs() / step.max(1.)).floor() as usize;
    if axis < 0. {
        origin.saturating_add(steps).min(last)
    } else {
        origin.saturating_sub(steps)
    }
}

fn emoji_dot_at(x: f32, left: f32, spacing: f32, count: usize) -> usize {
    if count <= 1 || !x.is_finite() {
        return 0;
    }
    ((x - left) / spacing.max(1.))
        .round()
        .clamp(0., (count - 1) as f32) as usize
}

impl Keyboard {
    pub fn open(&mut self) {
        self.visible = true;
    }
    pub fn close(&mut self) {
        self.visible = false;
        self.window_rect = None;
    }
    pub fn rect(&self, screen: Rect, scale: f32) -> Option<Rect> {
        self.rect_near(screen, None, scale)
    }

    /// Coordinates already use egui logical points; multiplying panel width by
    /// the DPI scale made a Retina keyboard twice as wide as intended.
    /// Pixel density is handled by the canvas egui context, not by geometry.
    pub fn rect_near(&self, screen: Rect, anchor: Option<Rect>, _scale: f32) -> Option<Rect> {
        if !self.visible || screen.width() < 280. || screen.height() < 200. {
            return None;
        }
        // Keep a bottom work-area inset so an always-visible macOS Dock does
        // not cover the initial keyboard position on a fullscreen SDL canvas.
        let inset_x = (screen.width() * 0.025).clamp(8., 28.);
        let inset_y = (screen.height() * 0.10).clamp(24., 120.);
        let safe = screen.shrink2(vec2(inset_x, inset_y));
        if safe.width() < 240. || safe.height() < 145. {
            return None;
        }
        let width = (safe.width() * 0.68).clamp(260., 960.).min(safe.width());
        let height = (width * 0.37).clamp(170., 350.).min(safe.height());
        let size = vec2(width, height);
        let gap = (height * 0.035).max(5.);
        let desired_x = anchor.map_or(safe.center().x, |r| r.center().x) - width * 0.5;
        let x = desired_x.clamp(safe.left(), safe.right() - width);
        let y = if let Some(text) = anchor {
            let below = text.bottom() + gap;
            let above = text.top() - gap - height;
            if below + height <= safe.bottom() {
                below
            } else if above >= safe.top() {
                above
            } else {
                // Neither side fits: choose the side with more available space
                // and clamp rather than covering the OS Dock or moving offscreen.
                let more_below = safe.bottom() - text.bottom() >= text.top() - safe.top();
                (if more_below { below } else { above }).clamp(safe.top(), safe.bottom() - height)
            }
        } else {
            safe.bottom() - height
        };
        Some(Rect::from_min_size(pos2(x, y), size))
    }

    pub fn current_rect(&self, fallback: Rect) -> Rect {
        self.window_rect.unwrap_or(fallback)
    }

    pub fn set_window_rect(&mut self, rect: Rect) {
        self.window_rect = Some(rect);
    }

    pub fn minimum_size(&self, available: egui::Vec2) -> egui::Vec2 {
        vec2(260., 170.).min(available)
    }

    pub fn ui(&mut self, ui: &mut Ui, rect: Rect, scale: f32) -> Option<Action> {
        let dark = Color32::from_rgb(43, 43, 47);
        ui.painter().rect_filled(rect, CornerRadius::same(12), dark);
        let margin = (8. * scale).max(3.);
        let gap = (5. * scale).max(2.);
        let inside = rect.shrink(margin);
        let row_h = ((inside.height() - 3. * gap) / 4.).max(0.);
        let mut result = None;
        match self.page {
            Page::Letters => {
                for (i, row) in [
                    LETTER_ROW_1.as_slice(),
                    LETTER_ROW_2.as_slice(),
                    LETTER_ROW_3.as_slice(),
                    LETTER_ROW_4.as_slice(),
                ]
                .into_iter()
                .enumerate()
                {
                    if let Some(action) = self.row(ui, row, inside, i, gap, scale) {
                        result = Some(action);
                    }
                }
            }
            Page::Symbols => {
                for (i, row) in [
                    NUM_ROW_1.as_slice(),
                    NUM_ROW_2.as_slice(),
                    NUM_ROW_3.as_slice(),
                    NUM_ROW_4.as_slice(),
                ]
                .into_iter()
                .enumerate()
                {
                    if let Some(action) = self.row(ui, row, inside, i, gap, scale) {
                        result = Some(action);
                    }
                }
            }
            Page::ExtraSymbols => {
                for (i, row) in [
                    EXTRA_ROW_1.as_slice(),
                    EXTRA_ROW_2.as_slice(),
                    EXTRA_ROW_3.as_slice(),
                    EXTRA_ROW_4.as_slice(),
                ]
                .into_iter()
                .enumerate()
                {
                    if let Some(action) = self.row(ui, row, inside, i, gap, scale) {
                        result = Some(action);
                    }
                }
            }
            Page::Emoji => {
                result = self.emoji_ui(ui, inside, row_h, gap, scale);
            }
        }
        result
    }

    fn row(
        &mut self,
        ui: &mut Ui,
        keys: &[Cell],
        inside: Rect,
        row: usize,
        gap: f32,
        scale: f32,
    ) -> Option<Action> {
        let height = ((inside.height() - 3. * gap) / 4.).max(0.);
        let total: f32 = keys.iter().map(|cell| cell.weight).sum();
        let unit = ((inside.width() - gap * (keys.len().saturating_sub(1)) as f32) / total).max(0.);
        let mut x = inside.left();
        let y = inside.top() + row as f32 * (height + gap);
        let mut pressed = None;
        for cell in keys {
            let width = cell.weight * unit;
            let rect = Rect::from_min_size(pos2(x, y), vec2(width, height));
            x += width + gap;
            if self.key(ui, rect, cell.key, scale) {
                pressed = self.act(cell.key);
            }
        }
        pressed
    }

    fn key(&self, ui: &mut Ui, rect: Rect, key: Key, scale: f32) -> bool {
        if rect.width() < 2. || rect.height() < 2. {
            return false;
        }
        let (label, special) = match key {
            Key::Text(text) => (text, false),
            Key::Delete => ("⌫", true),
            Key::Return => ("↵", true),
            Key::Shift => ("⇧", true),
            Key::LetterPage => ("ABC", true),
            Key::SymbolsPage => ("?123", true),
            Key::ExtraPage => ("=/<", true),
            Key::EmojiPage => ("☺", true),
            Key::Space => ("", false),
            Key::InputMethod => ("🌐", true),
            Key::Hide => ("⌨⌄", true),
            Key::Left => ("‹", true),
            Key::Right => ("›", true),
        };
        let upper;
        let label = if self.shift && self.page == Page::Letters && !special {
            upper = label.to_uppercase();
            upper.as_str()
        } else {
            label
        };
        let description = match key {
            Key::Text(_) => label,
            Key::Delete => "Backspace",
            Key::Return => "Return",
            Key::Shift => "Shift",
            Key::LetterPage => "Letters",
            Key::SymbolsPage => "Numbers and symbols",
            Key::ExtraPage => "More symbols",
            Key::EmojiPage => "Emoji",
            Key::Space => "Space",
            Key::InputMethod => "Switch input method",
            Key::Hide => "Hide keyboard",
            Key::Left => "Move cursor left",
            Key::Right => "Move cursor right",
        };
        let fill = if special {
            Color32::from_rgb(70, 70, 73)
        } else {
            Color32::from_rgb(94, 94, 96)
        };
        // Fit lettering and Emoji glyphs to the actual key height rather than
        // applying one fixed 17pt value to every screen and DPI density.
        let font_size = (rect.height() * 0.46).min(rect.width() * 0.65).max(12.);
        let response = ui.put(
            rect,
            egui::Button::new(RichText::new(label).size(font_size).color(Color32::WHITE))
                .fill(fill)
                .corner_radius(CornerRadius::same(9))
                .stroke(Stroke::NONE),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), description)
        });
        let hovered = ui.ctx().animate_bool_with_time(
            response.id.with("osk-hover"),
            response.hovered(),
            0.11,
        );
        let down = ui.ctx().animate_bool_with_time(
            response.id.with("osk-pressed"),
            response.is_pointer_button_down_on(),
            0.075,
        );
        if hovered > 0.01 || down > 0.01 {
            ui.painter().rect_stroke(
                rect.shrink(1.),
                CornerRadius::same(9),
                Stroke::new(
                    (1.5 * hovered + 2. * down).max(0.5),
                    Color32::from_white_alpha((110. * hovered + 110. * down).min(220.) as u8),
                ),
                StrokeKind::Inside,
            );
            if down > 0.01 {
                ui.painter().rect_filled(
                    rect.shrink(2.),
                    CornerRadius::same(8),
                    Color32::from_white_alpha((45. * down) as u8),
                );
            }
        }
        response.clicked()
    }

    fn act(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Text(value) => {
                let value = if self.shift && self.page == Page::Letters {
                    Cow::Owned(value.to_uppercase())
                } else {
                    Cow::Borrowed(value)
                };
                self.shift = false;
                Some(Action::Insert(value))
            }
            Key::Space => Some(Action::Insert(Cow::Borrowed(" "))),
            Key::Delete => Some(Action::Backspace),
            Key::Return => Some(Action::Return),
            Key::Left => Some(Action::MoveLeft),
            Key::Right => Some(Action::MoveRight),
            Key::Shift => {
                self.shift = !self.shift;
                None
            }
            Key::LetterPage => {
                self.page = Page::Letters;
                self.shift = false;
                None
            }
            Key::SymbolsPage => {
                self.page = Page::Symbols;
                None
            }
            Key::ExtraPage => {
                self.page = Page::ExtraSymbols;
                None
            }
            Key::EmojiPage => {
                self.page = Page::Emoji;
                None
            }
            Key::InputMethod => Some(Action::SwitchInputMethod),
            Key::Hide => Some(Action::Hide),
        }
    }

    fn emoji_ui(
        &mut self,
        ui: &mut Ui,
        bounds: Rect,
        row_h: f32,
        gap: f32,
        scale: f32,
    ) -> Option<Action> {
        let (_, items) = EMOJI_CATEGORIES.get(self.emoji_category).copied()?;
        let cell = (bounds.width() - (EMOJI_COLUMNS - 1) as f32 * gap) / EMOJI_COLUMNS as f32;
        let max_page = items.len().saturating_sub(1) / EMOJI_PAGE_SIZE;
        self.emoji_page = self.emoji_page.min(max_page);

        // Keep the pager between the three emoji rows and the category bar.
        // The grid itself senses DRAG only, so existing accessible emoji
        // buttons still receive taps/clicks, but drags started on a button
        // pass through to this region instead of inserting that emoji.
        // Reserve enough height for a usable touch/pen scrub strip even on
        // short, compact keyboard windows.
        let dots_h = row_h * 0.38;
        let emoji_row_h = (row_h - dots_h / 3.).max(1.);
        let grid = Rect::from_min_size(
            bounds.min,
            vec2(bounds.width(), 3. * emoji_row_h + 2. * gap),
        );
        let drag = ui.interact(
            grid,
            ui.scope_id().with("emoji-grid-drag"),
            egui::Sense::DRAG,
        );
        if drag.drag_started() {
            self.emoji_drag_start = self.emoji_page;
            self.emoji_wheel_remainder = 0.;
        }
        if drag.dragged()
            && let Some(total_delta) = drag.total_drag_delta()
        {
            // Unlike drag_delta(), this is the total gesture distance:
            // slow touch/stylus motions must still advance the page.
            let step = (grid.width() * 0.18).clamp(28. * scale, 92. * scale);
            self.emoji_page =
                emoji_page_from_drag(self.emoji_drag_start, max_page, total_delta, step);
        }

        // Pointer-based hit test rather than Response::hovered: emoji buttons
        // sit above the drag surface and are supposed to receive hover.
        let over_grid = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .is_some_and(|pos| grid.contains(pos))
        });
        if over_grid && !drag.dragged() {
            let wheel = ui.input(|input| {
                if input.modifiers.ctrl || input.modifiers.command {
                    egui::Vec2::ZERO
                } else {
                    input.smooth_scroll_delta
                }
            });
            let axis = if wheel.x.abs() > wheel.y.abs() {
                wheel.x
            } else {
                wheel.y
            };
            if axis.is_finite() {
                self.emoji_wheel_remainder += axis;
                let threshold = 22. * scale;
                let steps = (self.emoji_wheel_remainder.abs() / threshold).floor() as usize;
                if steps > 0 {
                    let prior = self.emoji_page;
                    self.emoji_page = emoji_page_from_drag(
                        self.emoji_page,
                        max_page,
                        vec2(self.emoji_wheel_remainder.signum() * steps as f32, 0.),
                        1.,
                    );
                    // Once at an edge, discard excess momentum rather than
                    // saving up movement to jump on the next interaction.
                    self.emoji_wheel_remainder = if prior == self.emoji_page {
                        0.
                    } else {
                        self.emoji_wheel_remainder
                            - self.emoji_wheel_remainder.signum() * steps as f32 * threshold
                    };
                }
            }
        } else if !drag.dragged() {
            self.emoji_wheel_remainder = 0.;
        }

        // Smooth the fractional page position, not just its dots. Reversing
        // direction midway starts from the visible position rather than
        // restarting an animation at a previous page boundary.
        let dt = ui.input(|input| input.stable_dt).clamp(0., 0.06);
        let target = self.emoji_page as f32;
        let fraction = 1. - (-dt * 17.).exp();
        self.emoji_display += (target - self.emoji_display) * fraction;
        if (self.emoji_display - target).abs() < 0.002 {
            self.emoji_display = target;
        } else {
            ui.ctx().request_repaint();
        }
        let slide = self.emoji_display.clamp(0., max_page as f32);
        let from = slide.floor() as usize;
        let to = (from + 1).min(max_page);
        let mut result = None;
        let mut clip = ui.new_child(egui::UiBuilder::new().max_rect(grid));
        clip.set_clip_rect(grid.intersect(ui.clip_rect()));
        for page in from..=to {
            let offset_x = (page as f32 - slide) * (grid.width() + gap * 2.);
            let start = page * EMOJI_PAGE_SIZE;
            for (i, emoji) in items.iter().skip(start).take(EMOJI_PAGE_SIZE).enumerate() {
                let col = (i % EMOJI_COLUMNS) as f32;
                let row = (i / EMOJI_COLUMNS) as f32;
                let rect = Rect::from_min_size(
                    pos2(
                        grid.left() + col * (cell + gap) + offset_x,
                        grid.top() + row * (emoji_row_h + gap),
                    ),
                    vec2(cell, emoji_row_h),
                );
                if !grid.intersects(rect) {
                    continue;
                }
                if self.key(&mut clip, rect, Key::Text(emoji), scale)
                    && !drag.dragged()
                    && page == self.emoji_page
                {
                    result = Some(Action::Insert(Cow::Borrowed(*emoji)));
                }
            }
        }

        // Larger touch target than the visible dots. This interaction region
        // does not intersect the emoji grid or the category/input-mode bar.
        let dot_count = max_page + 1;
        let dot_spacing = (dots_h * 1.06).max(12.);
        let left = bounds.center().x - (dot_count.saturating_sub(1)) as f32 * dot_spacing * 0.5;
        let right = left + (dot_count.saturating_sub(1)) as f32 * dot_spacing;
        let dot_top = grid.bottom();
        let dot_bottom = bounds.top() + 3. * (row_h + gap);
        let dot_y = (dot_top + dot_bottom) * 0.5;
        let dots_area = Rect::from_min_max(
            pos2(left - dot_spacing * 0.5, dot_top),
            pos2(right + dot_spacing * 0.5, dot_bottom),
        );
        let dots = ui.interact(
            dots_area,
            ui.scope_id().with("emoji-page-dots"),
            egui::Sense::CLICK | egui::Sense::DRAG,
        );
        // Dock-like magnification on hover, with neighbouring dots also
        // gently growing. A finger or pen can press and scrub across them.
        let pointer = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .or_else(|| input.pointer.interact_pos())
        });
        let nearby = pointer.filter(|pos| dots_area.contains(*pos) || dots.dragged());
        if (dots.clicked() || dots.dragged())
            && let Some(pos) = dots.interact_pointer_pos()
        {
            let selected = emoji_dot_at(pos.x, left, dot_spacing, dot_count);
            if selected != self.emoji_page {
                self.emoji_page = selected;
                ui.ctx().request_repaint();
            }
            self.emoji_wheel_remainder = 0.;
        }
        for page in 0..dot_count {
            let center = pos2(left + page as f32 * dot_spacing, dot_y);
            let proximity = nearby.map_or(0., |pos| {
                (1. - ((pos.x - center.x).abs() / (dot_spacing * 1.8))).max(0.)
            });
            let active = self.emoji_page == page;
            let radius =
                (if active { 0.22 } else { 0.16 }) * dots_h + 0.32 * dots_h * proximity * proximity;
            if proximity > 0.1 {
                ui.painter().circle_filled(
                    center,
                    radius + 0.08 * dots_h,
                    Color32::from_white_alpha((28. * proximity) as u8),
                );
            }
            ui.painter().circle_filled(
                center,
                radius,
                if active {
                    Color32::WHITE
                } else {
                    Color32::from_gray(155)
                },
            );
        }
        let bottom = Rect::from_min_size(
            pos2(bounds.left(), bounds.top() + 3. * (row_h + gap)),
            vec2(bounds.width(), row_h),
        );
        // GNOME-like equally spaced bottom controls. Every normal key has
        // one proportional unit; only the two pager arrows are 2/3 width.
        let unit = bottom.width() / (EMOJI_CATEGORIES.len() as f32 + 3. + 4. / 3.);
        let selector = Rect::from_min_size(
            bottom.min,
            vec2(unit * (EMOJI_CATEGORIES.len() + 1) as f32, bottom.height()),
        );
        if ui
            .put(
                Rect::from_min_size(selector.min, vec2(unit, row_h)),
                egui::Button::new(RichText::new("ABC").color(Color32::WHITE).size(14. * scale))
                    .fill(Color32::from_rgb(70, 70, 73)),
            )
            .clicked()
        {
            self.page = Page::Letters;
        }
        for (i, (icon, _)) in EMOJI_CATEGORIES.iter().enumerate() {
            let rect = Rect::from_min_size(
                pos2(selector.min.x + (i + 1) as f32 * unit, bottom.top()),
                vec2(unit, row_h),
            );
            let fill = if self.emoji_category == i {
                Color32::from_rgb(90, 90, 96)
            } else {
                Color32::from_rgb(70, 70, 73)
            };
            if ui
                .put(
                    rect,
                    egui::Button::new(RichText::new(*icon).size(17. * scale))
                        .fill(fill)
                        .corner_radius(CornerRadius::same(8)),
                )
                .clicked()
            {
                self.emoji_category = i;
                self.emoji_page = 0;
                self.emoji_display = 0.;
                self.emoji_drag_start = 0;
                self.emoji_wheel_remainder = 0.;
            }
        }
        let nav_width = unit * (2. / 3.);
        let nav_x = selector.right();
        if ui
            .put(
                Rect::from_min_size(pos2(nav_x, bottom.top()), vec2(nav_width, row_h)),
                egui::Button::new(RichText::new("‹").size(row_h * 0.50)),
            )
            .clicked()
        {
            self.emoji_page = self.emoji_page.saturating_sub(1);
            ui.ctx().request_repaint();
        }
        if ui
            .put(
                Rect::from_min_size(
                    pos2(nav_x + nav_width, bottom.top()),
                    vec2(nav_width, row_h),
                ),
                egui::Button::new(RichText::new("›").size(row_h * 0.50)),
            )
            .clicked()
        {
            self.emoji_page = (self.emoji_page + 1).min(max_page);
            ui.ctx().request_repaint();
        }
        let input_rect = Rect::from_min_size(
            pos2(nav_x + 2. * nav_width, bottom.top()),
            vec2(unit, row_h),
        );
        if self.key(ui, input_rect, Key::InputMethod, scale) {
            result = Some(Action::SwitchInputMethod);
        }
        let hide_rect = Rect::from_min_size(
            pos2(input_rect.right(), bottom.top()),
            vec2((bottom.right() - input_rect.right()).max(0.), row_h),
        );
        if self.key(ui, hide_rect, Key::Hide, scale) {
            result = Some(Action::Hide);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn four_pages_and_emoji_classes_are_available() {
        let mut k = Keyboard::default();
        assert_eq!(k.page, Page::Letters);
        k.act(Key::SymbolsPage);
        assert_eq!(k.page, Page::Symbols);
        k.act(Key::ExtraPage);
        assert_eq!(k.page, Page::ExtraSymbols);
        k.act(Key::EmojiPage);
        assert_eq!(k.page, Page::Emoji);
        assert_eq!(EMOJI_CATEGORIES.len(), 9);
        assert!(EMOJI_CATEGORIES.iter().all(|(_, items)| !items.is_empty()));
        k.act(Key::LetterPage);
        assert_eq!(k.page, Page::Letters);
    }

    #[test]
    fn shift_is_one_shot_and_plain_unicode_keys_are_borrowed() {
        let mut keyboard = Keyboard::default();
        assert_eq!(
            keyboard.act(Key::Text("a")),
            Some(Action::Insert(Cow::Borrowed("a")))
        );
        keyboard.act(Key::Shift);
        assert_eq!(
            keyboard.act(Key::Text("a")),
            Some(Action::Insert(Cow::Owned("A".to_owned())))
        );
        assert!(!keyboard.shift);
        assert_eq!(
            keyboard.act(Key::Text("😊")),
            Some(Action::Insert(Cow::Borrowed("😊")))
        );
        assert_eq!(
            keyboard.act(Key::Space),
            Some(Action::Insert(Cow::Borrowed(" ")))
        );
    }

    #[test]
    fn emoji_pages_cover_every_category_without_empty_pages() {
        for (_, items) in EMOJI_CATEGORIES {
            let pages = items.len().div_ceil(EMOJI_PAGE_SIZE);
            assert!(pages > 0);
            for page in 0..pages {
                assert!(items.get(page * EMOJI_PAGE_SIZE).is_some());
            }
        }
    }

    #[test]
    fn emoji_grid_drag_navigates_in_both_axes_and_clamps() {
        let last = 3;
        assert_eq!(emoji_page_from_drag(1, last, vec2(-80., 5.), 40.), 3);
        assert_eq!(emoji_page_from_drag(2, last, vec2(0., 42.), 40.), 1);
        assert_eq!(emoji_page_from_drag(0, last, vec2(1000., 0.), 40.), 0);
        assert_eq!(emoji_page_from_drag(3, last, vec2(-1000., 0.), 40.), 3);
        assert_eq!(emoji_page_from_drag(1, last, vec2(14., 12.), 40.), 1);
    }

    #[test]
    fn emoji_dot_scrubbing_selects_nearest_page_and_clamps() {
        assert_eq!(emoji_dot_at(100., 100., 24., 4), 0);
        assert_eq!(emoji_dot_at(123., 100., 24., 4), 1);
        assert_eq!(emoji_dot_at(149., 100., 24., 4), 2);
        assert_eq!(emoji_dot_at(500., 100., 24., 4), 3);
        assert_eq!(emoji_dot_at(-10., 100., 24., 4), 0);
        assert_eq!(emoji_dot_at(123., 100., 24., 1), 0);
    }

    // Exercise the actual egui widgets, not only the page arithmetic:
    // mouse, direct touch and pen all reach the keyboard as pointer events.
    fn emoji_frame(
        keyboard: &mut Keyboard,
        ctx: &egui::Context,
        screen: Rect,
        rect: Rect,
        events: Vec<egui::Event>,
    ) -> Option<Action> {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut action = None;
        let output = ctx.run_ui(input, |ui| {
            action = keyboard.ui(ui, rect, 1.);
        });
        output.drop_without_applying_deltas();
        action
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn emoji_grid_drag_scrolls_without_inserting_a_character() {
        let mut keyboard = Keyboard::default();
        keyboard.open();
        keyboard.act(Key::EmojiPage);
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(800., 600.));
        let Some(rect) = keyboard.rect(screen, 1.) else {
            panic!("Expected emoji keyboard bounds");
        };
        let inside = rect.shrink(8.);
        // Start in the gap between Emoji buttons, never on a key.
        let cell = (inside.width() - 10. * 5.) / EMOJI_COLUMNS as f32;
        let start = pos2(inside.left() + 8. * cell + 7.5 * 5., inside.top() + 18.);
        emoji_frame(&mut keyboard, &ctx, screen, rect, vec![]);
        assert_eq!(
            emoji_frame(
                &mut keyboard,
                &ctx,
                screen,
                rect,
                vec![
                    egui::Event::PointerMoved(start),
                    pointer_button(start, true),
                ],
            ),
            None
        );
        let finish = start - vec2(240., 0.);
        assert_eq!(
            emoji_frame(
                &mut keyboard,
                &ctx,
                screen,
                rect,
                vec![egui::Event::PointerMoved(finish)],
            ),
            None,
            "Dragging the emoji grid must not insert its buttons"
        );
        assert!(
            keyboard.emoji_page > 0,
            "An emoji grid swipe should advance the page"
        );
        emoji_frame(
            &mut keyboard,
            &ctx,
            screen,
            rect,
            vec![pointer_button(finish, false)],
        );
    }

    #[test]
    fn emoji_page_dots_support_click_and_continuous_scrubbing() {
        let mut keyboard = Keyboard::default();
        keyboard.open();
        keyboard.act(Key::EmojiPage);
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(800., 600.));
        let Some(rect) = keyboard.rect(screen, 1.) else {
            panic!("Expected emoji keyboard bounds");
        };
        let inside = rect.shrink(8.);
        let gap = 5.;
        let row_h = (inside.height() - 3. * gap) / 4.;
        let dots_h = row_h * 0.38;
        let emoji_row_h = row_h - dots_h / 3.;
        let top = inside.top() + 3. * emoji_row_h + 2. * gap;
        let bottom = inside.top() + 3. * (row_h + gap);
        let y = (top + bottom) * 0.5;
        let last_page = FACES.len().div_ceil(EMOJI_PAGE_SIZE) - 1;
        let left = inside.center().x - last_page as f32 * (dots_h * 1.06) * 0.5;
        let page = |index: usize| pos2(left + index as f32 * dots_h * 1.06, y);
        emoji_frame(&mut keyboard, &ctx, screen, rect, vec![]);
        let second = page(1);
        emoji_frame(
            &mut keyboard,
            &ctx,
            screen,
            rect,
            vec![
                egui::Event::PointerMoved(second),
                pointer_button(second, true),
            ],
        );
        emoji_frame(
            &mut keyboard,
            &ctx,
            screen,
            rect,
            vec![pointer_button(second, false)],
        );
        assert_eq!(keyboard.emoji_page, 1, "A dot click should jump to page 2");
        let first = page(0);
        emoji_frame(
            &mut keyboard,
            &ctx,
            screen,
            rect,
            vec![
                egui::Event::PointerMoved(first),
                pointer_button(first, true),
            ],
        );
        let last = page(last_page);
        emoji_frame(
            &mut keyboard,
            &ctx,
            screen,
            rect,
            vec![egui::Event::PointerMoved(last)],
        );
        assert_eq!(
            keyboard.emoji_page, last_page,
            "Press-and-scrub should track the nearest dot before release"
        );
        emoji_frame(
            &mut keyboard,
            &ctx,
            screen,
            rect,
            vec![pointer_button(last, false)],
        );
    }

    #[test]
    fn responsive_overlay_stays_within_canvas() {
        let mut k = Keyboard::default();
        k.open();
        for (w, h) in [(800., 600.), (320., 240.), (1920., 1080.)] {
            let area = Rect::from_min_size(egui::Pos2::ZERO, vec2(w, h));
            let Some(r) = k.rect(area, 1.) else {
                panic!("keyboard should fit");
            };
            assert!(area.contains_rect(r));
        }
        k.close();
        assert!(
            k.rect(Rect::from_min_size(egui::Pos2::ZERO, vec2(800., 600.)), 1.)
                .is_none()
        );
    }
}
