use crate::color::InkColor;
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, path::Path};

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
    pub fn label(self) -> &'static str {
        match self {
            Self::Pen => "Pen",
            Self::Pencil => "Pencil",
            Self::Marker => "Highlighter",
            Self::Line => "Line",
            Self::Arrow => "Arrow",
            Self::Rectangle => "Box",
            Self::Ellipse => "Ellipse",
            Self::Eraser => "Eraser",
            Self::Laser => "Laser",
        }
    }
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
    pub fn polyline(&self) -> Vec<Point> {
        if self.points.is_empty() {
            return vec![];
        }
        let a = self.points[0];
        let b = *self.points.last().unwrap();
        let point = |x, y| Point::new(x, y, 1., [0.; 2]);
        match self.tool {
            Tool::Line | Tool::Arrow => vec![a, b],
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
            _ => self.points.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    pub size: [f32; 2],
    pub strokes: Vec<Stroke>,
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
    undo: VecDeque<Vec<Stroke>>,
    redo: Vec<Vec<Stroke>>,
    pub changed: bool,
}
impl History {
    pub fn snapshot(&mut self) {
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
            self.doc.strokes.push(s)
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
    pub fn erase(&mut self, p: Point, radius: f32) {
        let keep: Vec<bool> = self
            .doc
            .strokes
            .iter()
            .map(|s| {
                let points = s.polyline();
                !points.iter().any(|a| a.dist(p) <= radius + s.width * 0.5)
                    && !points
                        .windows(2)
                        .any(|ab| segment_distance(p, ab[0], ab[1]) <= radius + s.width * 0.5)
            })
            .collect();
        if keep.iter().any(|k| !*k) {
            self.snapshot();
            let mut i = 0;
            self.doc.strokes.retain(|_| {
                let k = keep[i];
                i += 1;
                k
            })
        }
    }
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?
        }
        let data = serde_json::to_vec(&self.doc).map_err(|e| e.to_string())?;
        // Write a sibling first. A failed write never truncates the previous session.
        let tmp = path.with_extension("overink.tmp");
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        self.changed = false;
        Ok(())
    }
    pub fn load(&mut self, path: &Path) -> Result<(), String> {
        if std::fs::metadata(path).map_err(|e| e.to_string())?.len() > MAX_FILE_BYTES {
            return Err("Session exceeds 64 MiB".into());
        }
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        let doc: Document = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        doc.validate()?;
        self.snapshot();
        self.doc = doc;
        self.active = None;
        Ok(())
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
    fn erase_middle_and_undo() {
        let mut h = History {
            active: Some(stroke()),
            ..History::default()
        };
        h.commit();
        h.erase(Point::new(50., 0., 1., [0.; 2]), 3.);
        assert!(h.doc.strokes.is_empty());
        h.undo();
        assert_eq!(h.doc.strokes.len(), 1);
        h.redo();
        assert!(h.doc.strokes.is_empty())
    }
    #[test]
    fn rejects_nonfinite_and_unknown_version() {
        let mut d = Document::default();
        d.strokes.push(stroke());
        d.strokes[0].width = f32::NAN;
        assert!(d.validate().is_err());
        d.strokes.clear();
        d.version = 2;
        assert!(d.validate().is_err())
    }
}
