use crate::color::InkColor;
use egui::{Pos2, Rect, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

pub const MAX_TEXT_BOXES: usize = 256;
pub const MAX_TEXT_CHARS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextBox {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub font_size: f32,
    pub color: InkColor,
    pub text: String,
    #[serde(default)]
    pub scroll_offset: f32,
}

impl TextBox {
    pub const FONT_SIZE: std::ops::RangeInclusive<f32> = 8. ..=144.;
    pub const PADDING: f32 = 6.;
    const MIN_SIZE: [f32; 2] = [48., 28.];
    const MAX_SIZE: f32 = 16384.;

    pub fn new(position: Pos2, canvas: Rect, color: InkColor) -> Self {
        let mut item = Self {
            origin: [position.x, position.y],
            size: [
                (canvas.right() - position.x).clamp(144., 320.),
                (canvas.bottom() - position.y).clamp(64., 112.),
            ],
            font_size: 28.,
            color,
            text: String::new(),
            scroll_offset: 0.,
        };
        item.move_to(position, canvas);
        item
    }

    pub fn rect(&self) -> Rect {
        Rect::from_min_size(pos2(self.origin[0], self.origin[1]), Vec2::from(self.size))
    }

    pub fn move_to(&mut self, position: Pos2, canvas: Rect) {
        self.origin = [
            position.x.clamp(
                canvas.left(),
                (canvas.right() - self.size[0]).max(canvas.left()),
            ),
            position.y.clamp(
                canvas.top(),
                (canvas.bottom() - self.size[1]).max(canvas.top()),
            ),
        ];
    }

    pub fn resize(&mut self, size: Vec2, canvas: Rect) {
        let available = canvas.max - self.rect().min;
        self.size = std::array::from_fn(|i| {
            size[i].clamp(
                Self::MIN_SIZE[i],
                available[i].clamp(Self::MIN_SIZE[i], Self::MAX_SIZE),
            )
        });
    }

    pub fn valid(&self) -> bool {
        !self.text.trim().is_empty()
            && self.text.chars().take(MAX_TEXT_CHARS + 1).count() <= MAX_TEXT_CHARS
            && self
                .origin
                .iter()
                .all(|v| v.is_finite() && v.abs() <= 100000.)
            && self
                .size
                .into_iter()
                .zip(Self::MIN_SIZE)
                .all(|(value, min)| (min..=Self::MAX_SIZE).contains(&value))
            && Self::FONT_SIZE.contains(&self.font_size)
            && self.scroll_offset.is_finite()
            && self.scroll_offset >= 0.
            && self.color.rgb.iter().all(|v| (0. ..=1.).contains(v))
    }

    pub fn text_rect(&self) -> Rect {
        self.rect().shrink2(vec2(Self::PADDING, Self::PADDING))
    }

    pub fn visible_scroll_offset(&self, content_height: f32) -> f32 {
        self.scroll_offset
            .min((content_height - self.text_rect().height()).max(0.))
    }
}
