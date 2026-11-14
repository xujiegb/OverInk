use crate::color::InkColor;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    cell::{Ref, RefCell},
    collections::VecDeque,
    sync::Arc,
};

mod eraser;
pub use eraser::{EraseError, Erasure};

pub const MAX_POINTS: usize = 1_000_000;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
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

    pub fn is_shape(self) -> bool {
        matches!(
            self,
            Self::Line | Self::Arrow | Self::Rectangle | Self::Ellipse
        )
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
    pub const MERGE_DISTANCE: f32 = 0.05;

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
    pub fn merge_sample(&mut self, other: Self, pressure_enabled: bool, pencil: bool) {
        if pressure_enabled {
            self.pressure = self.pressure.max(other.pressure);
        }
        if pencil {
            self.tilt = other.tilt;
        }
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

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub min: [f32; 2],
    pub max: [f32; 2],
}
impl Bounds {
    pub fn between(a: Point, b: Point) -> Self {
        Self {
            min: [a.x.min(b.x), a.y.min(b.y)],
            max: [a.x.max(b.x), a.y.max(b.y)],
        }
    }

    pub fn points(points: impl Iterator<Item = Point>) -> Option<Self> {
        points
            .map(|point| Self::between(point, point))
            .reduce(Self::union)
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: [self.min[0].min(other.min[0]), self.min[1].min(other.min[1])],
            max: [self.max[0].max(other.max[0]), self.max[1].max(other.max[1])],
        }
    }

    pub fn padded(self, margin: f32) -> Self {
        Self {
            min: self.min.map(|value| value - margin),
            max: self.max.map(|value| value + margin),
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        self.max[0] >= other.min[0]
            && self.min[0] <= other.max[0]
            && self.max[1] >= other.min[1]
            && self.min[1] <= other.max[1]
    }

    pub fn include(into: &mut Option<Self>, bounds: Self) {
        *into = Some(into.map_or(bounds, |current| current.union(bounds)));
    }
}

#[derive(Clone, Copy)]
pub struct PointSpan<'a> {
    points: &'a [Point],
    start: Option<Point>,
    end: Option<Point>,
}
impl<'a> PointSpan<'a> {
    pub fn new(points: &'a [Point]) -> Self {
        Self {
            points,
            start: None,
            end: None,
        }
    }

    pub fn len(self) -> usize {
        self.points.len() + usize::from(self.start.is_some()) + usize::from(self.end.is_some())
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Point> + 'a {
        self.start
            .into_iter()
            .chain(self.points.iter().copied())
            .chain(self.end)
    }

    fn at(self, index: usize) -> Point {
        if index == 0
            && let Some(start) = self.start
        {
            return start;
        }
        self.points
            .get(index - usize::from(self.start.is_some()))
            .copied()
            .or(self.end)
            .unwrap()
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
    pub fn max_radius(&self) -> f32 {
        self.width * if self.tool == Tool::Pencil { 1.25 } else { 0.5 }
    }

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

#[derive(Clone, Copy)]
pub struct StrokeBounds {
    pub path: Bounds,
    well_spaced: bool,
}
impl StrokeBounds {
    fn new(stroke: &Stroke) -> Option<Self> {
        let points = stroke.polyline();
        let mut bounds = Self::measure(points.iter().copied())?;
        if let Some(head) = stroke.arrow_head() {
            bounds.path = bounds.path.union(Bounds::points(head.into_iter()).unwrap());
        }
        Some(bounds)
    }

    fn measure(mut points: impl Iterator<Item = Point>) -> Option<Self> {
        let mut previous = points.next()?;
        let mut path = Bounds::between(previous, previous);
        let mut well_spaced = true;
        for point in points {
            path = path.union(Bounds::between(point, point));
            // The renderer merges points at 0.05 units. Leave room for
            // a cut endpoint to change that decision at either neighbor.
            let x = point.x - previous.x;
            let y = point.y - previous.y;
            well_spaced &= x * x + y * y > 0.01;
            previous = point;
        }
        Some(Self { path, well_spaced })
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

struct HistoryStep {
    strokes: Vec<Arc<Stroke>>,
    bytes: usize,
    point_count: usize,
}

impl HistoryStep {
    fn new(strokes: Vec<Arc<Stroke>>, point_count: usize) -> Self {
        let entries = strokes.capacity() * size_of::<Arc<Stroke>>();
        let bytes = strokes.iter().fold(entries, |total, stroke| {
            total.saturating_add(
                size_of::<Stroke>()
                    + 2 * size_of::<usize>()
                    + stroke.points.capacity() * size_of::<Point>(),
            )
        });
        Self {
            strokes,
            bytes,
            point_count,
        }
    }
}

#[derive(Default)]
pub struct History {
    doc: Document,
    point_count: usize,
    pub active: Option<Stroke>,
    undo: VecDeque<HistoryStep>,
    redo: VecDeque<HistoryStep>,
    stroke_bounds: RefCell<Vec<Option<StrokeBounds>>>,
    // Appends can be painted over the committed layer. Erasure supplies its
    // own damage, while undo, redo, clear and load invalidate the whole layer.
    append_revision: u64,
    erase_revision: u64,
    ink_revision: u64,
    pub changed: bool,
}
impl History {
    pub fn document(&self) -> &Document {
        &self.doc
    }

    pub fn strokes(&self) -> &[Arc<Stroke>] {
        &self.doc.strokes
    }

    pub fn set_size(&mut self, size: [f32; 2]) {
        self.doc.size = size;
    }

    pub fn append_revision(&self) -> u64 {
        self.append_revision
    }

    pub fn erase_revision(&self) -> u64 {
        self.erase_revision
    }

    pub fn ink_revision(&self) -> u64 {
        self.ink_revision
    }

    pub fn point_count(&self) -> usize {
        self.point_count
    }

    pub fn stroke_bounds(&self) -> Ref<'_, [Option<StrokeBounds>]> {
        let end = self.doc.strokes.len();
        if self.stroke_bounds.borrow().len() < end {
            let mut bounds = self.stroke_bounds.borrow_mut();
            let first = bounds.len();
            bounds.extend(
                self.doc.strokes[first..end]
                    .iter()
                    .map(|stroke| StrokeBounds::new(stroke)),
            );
        }
        Ref::map(self.stroke_bounds.borrow(), Vec::as_slice)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn snapshot(&mut self) {
        // Committed strokes are immutable, so snapshots share their samples.
        self.redo.clear();
        self.undo
            .push_back(HistoryStep::new(self.doc.strokes.clone(), self.point_count));
        self.trim_steps();
        self.changed = true;
    }

    fn trim_steps(&mut self) {
        // Charge shared samples to each step to keep a conservative bound.
        while self.undo.len() + self.redo.len() > 64
            || self
                .undo
                .iter()
                .chain(&self.redo)
                .fold(0usize, |bytes, step| bytes.saturating_add(step.bytes))
                > MAX_HISTORY_BYTES
        {
            let older = if self.undo.len() >= self.redo.len() {
                &mut self.undo
            } else {
                &mut self.redo
            };
            older.pop_front();
        }
    }

    fn snapshot_replacement(&mut self) {
        self.snapshot();
        self.invalidate_stroke_bounds();
        self.ink_revision = self.ink_revision.wrapping_add(1);
    }

    fn invalidate_stroke_bounds(&mut self) {
        let bounds = self.stroke_bounds.get_mut();
        bounds.clear();
        if bounds.capacity() > 4096 {
            *bounds = Vec::new();
        }
    }

    pub fn append(&mut self, strokes: impl Iterator<Item = Stroke>) {
        let mut strokes = strokes.peekable();
        if strokes.peek().is_some() {
            self.snapshot();
            self.doc.strokes.extend(
                strokes
                    .inspect(|stroke| self.point_count += stroke.points.len())
                    .map(Arc::new),
            );
            self.append_revision = self.append_revision.wrapping_add(1);
        }
    }

    pub fn commit(&mut self) {
        let active = self
            .active
            .take()
            .filter(|stroke| !stroke.points.is_empty());
        self.append(active.into_iter());
    }
    fn apply_step(&mut self, step: HistoryStep) -> HistoryStep {
        let strokes = std::mem::replace(&mut self.doc.strokes, step.strokes);
        let point_count = std::mem::replace(&mut self.point_count, step.point_count);
        self.invalidate_stroke_bounds();
        self.changed = true;
        self.ink_revision = self.ink_revision.wrapping_add(1);
        HistoryStep::new(strokes, point_count)
    }

    pub fn undo(&mut self) {
        self.active = None;
        if let Some(step) = self.undo.pop_back() {
            let previous = self.apply_step(step);
            self.redo.push_back(previous);
            self.trim_steps();
        }
    }

    pub fn redo(&mut self) {
        self.active = None;
        if let Some(step) = self.redo.pop_back() {
            let previous = self.apply_step(step);
            self.undo.push_back(previous);
            self.trim_steps();
        }
    }
    pub fn clear(&mut self) {
        self.active = None;
        if !self.doc.strokes.is_empty() {
            self.snapshot_replacement();
            self.doc.strokes = Vec::new();
            self.point_count = 0;
        }
    }
    pub fn set_document(&mut self, doc: Document, restore: bool) {
        let point_count = doc.strokes.iter().map(|stroke| stroke.points.len()).sum();
        if restore {
            *self = Self {
                doc,
                point_count,
                ink_revision: self.ink_revision.wrapping_add(1),
                ..Self::default()
            };
        } else {
            self.snapshot_replacement();
            self.doc = doc;
            self.point_count = point_count;
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
    fn history_budget_counts_fragment_capacity_and_both_stacks() {
        let mut fragment = stroke();
        fragment.points.reserve(4096);
        let point_count = fragment.points.len();
        let step = HistoryStep::new(vec![Arc::new(fragment)], point_count);
        assert!(step.bytes > 4096 * size_of::<Point>());
        let mut history = History::default();
        for stack in [&mut history.undo, &mut history.redo] {
            stack.push_back(HistoryStep {
                strokes: Vec::new(),
                bytes: MAX_HISTORY_BYTES / 2,
                point_count: 0,
            });
        }
        history.undo.push_back(step);
        history.trim_steps();
        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.redo.len(), 1);
        assert_eq!(history.undo[0].strokes.len(), 1);
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
    fn point_count_follows_document_replacement_and_history() {
        let document = || Document {
            strokes: vec![Arc::new(stroke())],
            ..Document::default()
        };
        let mut history = History::default();
        history.set_document(document(), true);
        assert_eq!(history.point_count(), 2);
        history.append([stroke(), stroke()].into_iter());
        assert_eq!(history.point_count(), 6);
        history.set_document(document(), false);
        assert_eq!(history.point_count(), 2);
        history.undo();
        assert_eq!(history.point_count(), 6);
        history.redo();
        assert_eq!(history.point_count(), 2);
        history.clear();
        assert_eq!(history.point_count(), 0);
        history.undo();
        assert_eq!(history.point_count(), 2);
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
