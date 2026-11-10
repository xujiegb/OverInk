use crate::color::InkColor;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::VecDeque, sync::Arc};

mod eraser;
pub use eraser::Erasure;

pub const MAX_POINTS: usize = 1_000_000;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    #[default]
    Pen,
    Pencil,
    Marker,
    Line,
    Arrow,
    Rectangle,
    Ellipse,
    Eraser,
    Laser,
}
impl Tool {
    pub const ALL: [Self; 9] = [
        Self::Pen,
        Self::Pencil,
        Self::Marker,
        Self::Line,
        Self::Arrow,
        Self::Rectangle,
        Self::Ellipse,
        Self::Eraser,
        Self::Laser,
    ];
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt: [f32; 2],
}
impl Point {
    pub fn new(x: f32, y: f32, pressure: f32, tilt: [f32; 2]) -> Self {
        Self {
            x: if x.is_finite() { x } else { 0.0 },
            y: if y.is_finite() { y } else { 0.0 },
            pressure: if pressure.is_finite() {
                pressure.clamp(0., 1.)
            } else {
                0.
            },
            tilt: tilt.map(|v| {
                if v.is_finite() {
                    v.clamp(-90., 90.)
                } else {
                    0.
                }
            }),
        }
    }
    pub fn dist(self, b: Self) -> f32 {
        (self.x - b.x).hypot(self.y - b.y)
    }
    pub fn interpolate(self, b: Self, t: f32) -> Self {
        Self::new(
            self.x + (b.x - self.x) * t,
            self.y + (b.y - self.y) * t,
            self.pressure + (b.pressure - self.pressure) * t,
            [
                self.tilt[0] + (b.tilt[0] - self.tilt[0]) * t,
                self.tilt[1] + (b.tilt[1] - self.tilt[1]) * t,
            ],
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stroke {
    pub tool: Tool,
    pub color: InkColor,
    pub width: f32,
    pub opacity: f32,
    pub pressure_curve: f32,
    pub pressure_enabled: bool,
    pub points: Vec<Point>,
}
impl Stroke {
    pub fn radius(&self, p: Point) -> f32 {
        let scale = if self.pressure_enabled {
            0.08 + 0.92 * p.pressure.powf(self.pressure_curve)
        } else {
            1.
        };
        let tilt = if self.tool == Tool::Pencil {
            1.0 + p.tilt[0].hypot(p.tilt[1]).min(90.0) / 60.0
        } else {
            1.0
        };
        self.width * scale * tilt * 0.5
    }
    pub fn polyline(&self) -> Cow<'_, [Point]> {
        let Some(a) = self.points.first() else {
            return Cow::Borrowed(&self.points);
        };
        let b = self.points.last().unwrap();
        let point = |x, y| Point::new(x, y, 1., [0.; 2]);
        Cow::Owned(match self.tool {
            Tool::Line | Tool::Arrow => vec![*a, *b],
            Tool::Rectangle => vec![
                point(a.x, a.y),
                point(b.x, a.y),
                point(b.x, b.y),
                point(a.x, b.y),
                point(a.x, a.y),
            ],
            Tool::Ellipse => {
                let cx = (a.x + b.x) * 0.5;
                let cy = (a.y + b.y) * 0.5;
                let rx = (b.x - a.x).abs() * 0.5;
                let ry = (b.y - a.y).abs() * 0.5;
                (0..=96)
                    .map(|i| {
                        let t = i as f32 / 96. * std::f32::consts::TAU;
                        point(cx + rx * t.cos(), cy + ry * t.sin())
                    })
                    .collect()
            }
            _ => return Cow::Borrowed(&self.points),
        })
    }
    pub fn arrow_head(&self) -> Option<[Point; 3]> {
        let [a, .., b] = self.points.as_slice() else {
            return None;
        };
        if self.tool != Tool::Arrow {
            return None;
        }
        let angle = (b.y - a.y).atan2(b.x - a.x);
        let length = (self.width * 3.5).max(14.).min(a.dist(*b) * 0.65);
        let tip = |offset: f32| {
            Point::new(
                b.x - length * (angle + offset).cos(),
                b.y - length * (angle + offset).sin(),
                1.,
                [0.; 2],
            )
        };
        Some([tip(0.5), *b, tip(-0.5)])
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    pub size: [f32; 2],
    pub strokes: Vec<Arc<Stroke>>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            size: [1920., 1080.],
            strokes: vec![],
        }
    }
}
#[derive(Default)]
pub struct History {
    pub doc: Document,
    pub active: Option<Stroke>,
    undo: VecDeque<Vec<Arc<Stroke>>>,
    redo: Vec<Vec<Arc<Stroke>>>,
    pub changed: bool,
}
impl History {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn snapshot(&mut self) {
        // Committed strokes are immutable, so snapshots share their samples.
        self.undo.push_back(self.doc.strokes.clone());
        while self.undo.len() > 64
            || self
                .undo
                .iter()
                .flat_map(|v| v.iter())
                .map(|s| s.points.len() * std::mem::size_of::<Point>())
                .sum::<usize>()
                > 64 * 1024 * 1024
        {
            self.undo.pop_front();
        }
        self.redo.clear();
        self.changed = true;
    }
    pub fn commit(&mut self) {
        if let Some(s) = self.active.take()
            && !s.points.is_empty()
        {
            self.snapshot();
            self.doc.strokes.push(Arc::new(s))
        }
    }
    pub fn undo(&mut self) {
        self.active = None;
        if let Some(s) = self.undo.pop_back() {
            self.redo.push(std::mem::replace(&mut self.doc.strokes, s));
            self.changed = true
        }
    }
    pub fn redo(&mut self) {
        self.active = None;
        if let Some(s) = self.redo.pop() {
            self.undo
                .push_back(std::mem::replace(&mut self.doc.strokes, s));
            self.changed = true
        }
    }
    pub fn clear(&mut self) {
        self.active = None;
        if !self.doc.strokes.is_empty() {
            self.snapshot();
            self.doc.strokes.clear()
        }
    }
    pub fn set_document(&mut self, doc: Document, restore: bool) {
        if restore {
            *self = Self {
                doc,
                ..Self::default()
            };
        } else {
            self.snapshot();
            self.doc = doc;
            self.active = None;
        }
    }
}
impl Document {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("Unsupported session version".into());
        }
        if self
            .size
            .iter()
            .any(|v| !v.is_finite() || *v <= 0. || *v > 100000.)
        {
            return Err("Invalid canvas dimensions".into());
        }
        if self.strokes.iter().map(|s| s.points.len()).sum::<usize>() > MAX_POINTS {
            return Err("Session exceeds one million points".into());
        }
        for s in &self.strokes {
            if !s.width.is_finite()
                || !(0.1..=500.).contains(&s.width)
                || !s.opacity.is_finite()
                || !(0.0..=1.).contains(&s.opacity)
                || !s.pressure_curve.is_finite()
                || !(0.1..=5.).contains(&s.pressure_curve)
            {
                return Err("Invalid brush values".into());
            }
            if s.color
                .rgb
                .iter()
                .any(|v| !v.is_finite() || !(0.0..=1.).contains(v))
            {
                return Err("Invalid color".into());
            }
            for p in &s.points {
                if !p.x.is_finite()
                    || !p.y.is_finite()
                    || p.x.abs() > 100000.
                    || p.y.abs() > 100000.
                    || !p.pressure.is_finite()
                    || !(0.0..=1.).contains(&p.pressure)
                    || p.tilt.iter().any(|v| !v.is_finite() || v.abs() > 90.)
                {
                    return Err("Invalid pen sample".into());
                }
            }
        }
        Ok(())
    }
}
pub fn segment_distance(p: Point, a: Point, b: Point) -> f32 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / (dx * dx + dy * dy).max(1e-12)).clamp(0., 1.);
    (p.x - a.x - t * dx).hypot(p.y - a.y - t * dy)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stroke() -> Stroke {
        Stroke {
            tool: Tool::Pen,
            color: InkColor::default(),
            width: 5.,
            opacity: 1.,
            pressure_curve: 1.,
            pressure_enabled: true,
            points: vec![
                Point::new(0., 0., 0., [0.; 2]),
                Point::new(100., 0., 1., [0.; 2]),
            ],
        }
    }
    #[test]
    fn all_8192_pressure_steps_remain_distinct() {
        let s = stroke();
        let mut prev = 0.;
        for i in 0..8192 {
            let v = s.radius(Point::new(0., 0., i as f32 / 8191., [0.; 2]));
            assert!(v > prev);
            prev = v
        }
    }
    #[test]
    fn undo_keeps_the_same_committed_stroke_allocation() {
        let mut history = History {
            active: Some(stroke()),
            ..History::default()
        };
        history.commit();
        let original = Arc::clone(&history.doc.strokes[0]);
        history.active = Some(stroke());
        history.commit();
        history.undo();
        assert!(Arc::ptr_eq(&original, &history.doc.strokes[0]));
        history.redo();
        assert!(Arc::ptr_eq(&original, &history.doc.strokes[0]));
    }

    #[test]
    fn rejects_nonfinite_and_unknown_version() {
        let mut d = Document::default();
        d.strokes.push(Arc::new(Stroke {
            width: f32::NAN,
            ..stroke()
        }));
        assert!(d.validate().is_err());
        d.strokes.clear();
        d.version = 2;
        assert!(d.validate().is_err())
    }
}
