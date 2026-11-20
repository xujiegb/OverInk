#![forbid(unsafe_code)]

//! OverInk's own egui on-screen keyboard.
//! Interaction and page hierarchy are independently implemented in safe Rust,
//! informed by the GNOME Shell OSK's public UX (GPL-2.0-or-later). No GNOME
//! JavaScript, layouts, stylesheets or data files are incorporated here.
//! The IME and candidate window always belong to the operating system.
use egui::{Color32, CornerRadius, Rect, RichText, Stroke, Ui, pos2, vec2};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    Insert(String),
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

// Curated Unicode sequences, not imported GNOME emoji data. Color emoji are
// rendered by OverInk's existing OpenMoji fallback; no candidate strip.
const FACES: &[&str] = &[
    "😀", "😁", "😂", "🤣", "😃", "😄", "😅", "😆", "😉", "😊", "😋", "😎", "😍", "😘", "😗", "😙",
    "😚", "🙂", "🤗", "🤩", "🤔", "🤨", "😐", "😑", "😶", "🙄", "😏", "😣", "😥", "😮", "😯", "😪",
    "😫", "🥱", "😴", "😌", "😛", "😜", "😝", "🤤", "😒", "😓", "😔", "😕", "🙃", "🫠", "🤑", "😲",
    "☹️", "🙁", "😖", "😞", "😟", "😤", "😢", "😭", "😦", "😧", "😨", "😩", "🤯", "😬", "😰", "😱",
    "🥺", "😳", "🤪", "😵", "🥴", "😠", "😡", "🤬", "🤢", "🤮", "🤧", "😷", "🤒", "🤕", "🤠", "👻",
    "💀", "🤖",
];
const PEOPLE: &[&str] = &[
    "👍", "👎", "👏", "🙌", "👐", "🤲", "🤝", "🙏", "✍️", "💪", "👋", "🤚", "🖐️", "✋", "🖖", "👌",
    "🤌", "🤏", "✌️", "🤞", "🤟", "🤘", "🤙", "👈", "👉", "👆", "👇", "☝️", "✊", "👊", "🤛", "🤜",
    "❤️", "🫶", "👀", "👂", "👃", "👄",
];
const NATURE: &[&str] = &[
    "🌷", "🌹", "🌺", "🌸", "🌼", "🌻", "💐", "🍀", "🌿", "🌱", "🌳", "🌲", "🌵", "🍁", "🍂", "🌾",
    "🌴", "🍄", "🌞", "🌝", "🌚", "⭐", "🌟", "🌈", "🌊", "🔥", "❄️", "☀️", "🌙", "🌎", "🐈", "🐕",
    "🦋", "🐝", "🐢",
];
const FOOD: &[&str] = &[
    "🍎", "🍐", "🍊", "🍋", "🍌", "🍉", "🍇", "🍓", "🫐", "🍒", "🍑", "🥭", "🍍", "🥝", "🍅", "🥑",
    "🥦", "🥕", "🌽", "🥐", "🍞", "🧀", "🍳", "🍔", "🍟", "🍕", "🌭", "🥪", "🌮", "🍜", "🍣", "🍩",
    "🍪", "🍰", "☕", "🍵",
];
const TRAVEL: &[&str] = &[
    "✈️", "🚀", "🚗", "🚕", "🚌", "🚎", "🏎️", "🚓", "🚑", "🚒", "🚚", "🚲", "🛴", "🚄", "🚆", "🚇",
    "🚢", "⛵", "🛳️", "🛫", "🛬", "🛎️", "🏠", "🏫", "🏢", "🏖️", "🏕️", "⛰️", "🌋", "🗻", "🏝️", "🌆",
    "🌉",
];
const ACTIVITIES: &[&str] = &[
    "🏃", "🚶", "🚴", "🏊", "⚽", "🏀", "🏈", "⚾", "🎾", "🏐", "🏓", "🏸", "🥊", "🎳", "🎯", "🎲",
    "🎮", "🎨", "🎭", "🎬", "🎤", "🎧", "🎹", "🎸", "🥁", "📸", "🏆", "🥇", "🥈", "🥉",
];
const OBJECTS: &[&str] = &[
    "🔔", "🔕", "📱", "💻", "⌨️", "🖥️", "🖨️", "📷", "📹", "🎥", "🔋", "💡", "🔦", "🕯️", "📚", "📝",
    "✏️", "🖊️", "🖌️", "✂️", "🔒", "🔑", "🔧", "⚙️", "🧲", "🔬", "🧪", "📎", "📌", "📍",
];
const HEARTS: &[&str] = &[
    "❤️",
    "🧡",
    "💛",
    "💚",
    "💙",
    "💜",
    "🖤",
    "🤍",
    "🤎",
    "💖",
    "💗",
    "💓",
    "💕",
    "💞",
    "💘",
    "💝",
    "💟",
    "❣️",
    "💔",
    "❤️‍🔥",
    "❤️‍🩹",
    "♥️",
    "💯",
    "✨",
    "💫",
    "⭐",
    "🌟",
    "💥",
    "🔥",
    "✅",
    "❌",
];
const SYMBOLS_EMOJI: &[&str] = &[
    "🏳️",
    "🏴",
    "🏁",
    "🚩",
    "🏳️‍🌈",
    "🏳️‍⚧️",
    "⚠️",
    "☮️",
    "☯️",
    "♻️",
    "✔️",
    "☑️",
    "❎",
    "🔴",
    "🟠",
    "🟡",
    "🟢",
    "🔵",
    "🟣",
    "⚫",
    "⚪",
    "🔶",
    "🔷",
    "🔺",
    "🔻",
    "↗️",
    "↘️",
    "⬆️",
    "⬇️",
    "➡️",
    "⬅️",
    "🔁",
    "🔀",
];
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

impl Keyboard {
    pub fn open(&mut self) {
        self.visible = true;
    }
    pub fn close(&mut self) {
        self.visible = false;
    }
    pub fn rect(&self, screen: Rect, scale: f32) -> Option<Rect> {
        if !self.visible || screen.width() < 120. || screen.height() < 130. {
            return None;
        }
        let margin = 10. * scale;
        let width = (screen.width() - 2. * margin).max(0.).min(1020. * scale);
        let height = (width * 0.29)
            .clamp(142. * scale, 320. * scale)
            .min((screen.height() - 2. * margin).max(0.) * 0.56);
        Some(Rect::from_min_size(
            pos2(
                screen.center().x - width * 0.5,
                screen.bottom() - height - margin,
            ),
            vec2(width, height),
        ))
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
                    if let Some(action) = self.row(ui, row, inside, i, row_h, gap, scale) {
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
                    if let Some(action) = self.row(ui, row, inside, i, row_h, gap, scale) {
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
                    if let Some(action) = self.row(ui, row, inside, i, row_h, gap, scale) {
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
        height: f32,
        gap: f32,
        scale: f32,
    ) -> Option<Action> {
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
        let fill = if special {
            Color32::from_rgb(70, 70, 73)
        } else {
            Color32::from_rgb(94, 94, 96)
        };
        let font_size = (17. * scale).min(rect.height() * 0.53).max(10.);
        ui.put(
            rect,
            egui::Button::new(RichText::new(label).size(font_size).color(Color32::WHITE))
                .fill(fill)
                .corner_radius(CornerRadius::same(9))
                .stroke(Stroke::NONE),
        )
        .clicked()
    }

    fn act(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Text(value) => {
                let value = if self.shift && self.page == Page::Letters {
                    value.to_uppercase()
                } else {
                    value.to_owned()
                };
                self.shift = false;
                Some(Action::Insert(value))
            }
            Key::Space => Some(Action::Insert(" ".into())),
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
        let Some((_, items)) = EMOJI_CATEGORIES.get(self.emoji_category).copied() else {
            return None;
        };
        let cell = (bounds.width() - (EMOJI_COLUMNS - 1) as f32 * gap) / EMOJI_COLUMNS as f32;
        let start = self.emoji_page * EMOJI_PAGE_SIZE;
        let mut result = None;
        for (i, emoji) in items.iter().skip(start).take(EMOJI_PAGE_SIZE).enumerate() {
            let col = (i % EMOJI_COLUMNS) as f32;
            let row = (i / EMOJI_COLUMNS) as f32;
            let rect = Rect::from_min_size(
                pos2(
                    bounds.left() + col * (cell + gap),
                    bounds.top() + row * (row_h + gap),
                ),
                vec2(cell, row_h),
            );
            if self.key(ui, rect, Key::Text(emoji), scale) {
                result = Some(Action::Insert((*emoji).to_owned()));
            }
        }
        let bottom = Rect::from_min_size(
            pos2(bounds.left(), bounds.top() + 3. * (row_h + gap)),
            vec2(bounds.width(), row_h),
        );
        let categories_width = (bottom.width() - 120. * scale).max(0.);
        let selector = Rect::from_min_size(bottom.min, vec2(categories_width, bottom.height()));
        let unit = selector.width() / (EMOJI_CATEGORIES.len() + 1) as f32;
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
            }
        }
        let nav_width = 28. * scale;
        let nav_x = selector.right();
        let max_page = items.len().saturating_sub(1) / EMOJI_PAGE_SIZE;
        if ui
            .put(
                Rect::from_min_size(pos2(nav_x, bottom.top()), vec2(nav_width, row_h)),
                egui::Button::new("‹"),
            )
            .clicked()
        {
            self.emoji_page = self.emoji_page.saturating_sub(1);
        }
        if ui
            .put(
                Rect::from_min_size(
                    pos2(nav_x + nav_width, bottom.top()),
                    vec2(nav_width, row_h),
                ),
                egui::Button::new("›"),
            )
            .clicked()
        {
            self.emoji_page = (self.emoji_page + 1).min(max_page);
        }
        let input_rect = Rect::from_min_size(
            pos2(nav_x + 2. * nav_width, bottom.top()),
            vec2(nav_width, row_h),
        );
        if self.key(ui, input_rect, Key::InputMethod, scale) {
            result = Some(Action::SwitchInputMethod);
        }
        let hide_rect = Rect::from_min_size(
            pos2(nav_x + 3. * nav_width, bottom.top()),
            vec2((bottom.right() - nav_x - 3. * nav_width).max(0.), row_h),
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
