#![forbid(unsafe_code)]

use crate::color::InkColor;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    cell::{Ref, RefCell},
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

mod eraser;
mod text_box;
pub use eraser::{EraseError, Erasure};
pub use text_box::{MAX_TEXT_BOXES, MAX_TEXT_CHARS, TextBox};

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
    Text,
    Select,
}
impl Tool {
    pub const ALL: [Self; 11] = [
        Self::Pen,
        Self::Pencil,
        Self::Marker,
        Self::Line,
        Self::Arrow,
        Self::Rectangle,
        Self::Ellipse,
        Self::Eraser,
        Self::Laser,
        Self::Text,
        Self::Select,
    ];

    pub fn is_shape(self) -> bool {
        matches!(
            self,
            Self::Line | Self::Arrow | Self::Rectangle | Self::Ellipse
        )
    }

    pub fn is_text(self) -> bool {
        matches!(self, Self::Text | Self::Select)
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

    fn at(self, index: usize) -> Option<Point> {
        if index == 0
            && let Some(start) = self.start
        {
            return Some(start);
        }
        self.points
            .get(index.checked_sub(usize::from(self.start.is_some()))?)
            .copied()
            .or_else(|| {
                self.end
                    .filter(|_| self.len().checked_sub(1) == Some(index))
            })
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
    #[cfg(any(target_os = "macos", test))]
    pub fn has_wide_gamut_ink(&self) -> bool {
        !self.points.is_empty() && self.color.needs_wide_gamut()
    }

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
        let Some((a, b)) = self.points.first().zip(self.points.last()) else {
            return Cow::Borrowed(&self.points);
        };
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
        if let Some([left, tip, right]) = stroke.arrow_head() {
            bounds.path = bounds
                .path
                .union(Bounds::between(left, tip))
                .union(Bounds::between(right, right));
        }
        Some(bounds)
    }

    fn union(self, other: Self) -> Self {
        Self {
            path: self.path.union(other.path),
            well_spaced: self.well_spaced && other.well_spaced,
        }
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

const STROKE_BLOCK_SEGMENTS: usize = 128;

#[derive(Default)]
struct StrokeIndex {
    bounds: Vec<Option<StrokeBounds>>,
    blocks: BTreeMap<usize, Box<[StrokeBounds]>>,
}

impl StrokeIndex {
    fn push(&mut self, stroke: &Stroke) {
        let bounds = if !stroke.tool.is_shape() && stroke.points.len() > 2 * STROKE_BLOCK_SEGMENTS {
            // Adjacent blocks include their shared point so both the bounds
            // and spacing of every original segment are covered.
            let last = stroke.points.len() - 1;
            let blocks: Box<[_]> = (0..last)
                .step_by(STROKE_BLOCK_SEGMENTS)
                .filter_map(|first| {
                    let end = (first + STROKE_BLOCK_SEGMENTS).min(last);
                    StrokeBounds::measure(stroke.points[first..=end].iter().copied())
                })
                .collect();
            let combined = blocks.iter().copied().reduce(StrokeBounds::union);
            self.blocks.insert(self.bounds.len(), blocks);
            combined
        } else {
            StrokeBounds::new(stroke)
        };
        self.bounds.push(bounds);
    }

    fn blocks(&self, index: usize) -> &[StrokeBounds] {
        self.blocks
            .get(&index)
            .map_or(&[], |blocks| blocks.as_ref())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    pub size: [f32; 2],
    pub strokes: Vec<Arc<Stroke>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_boxes: Vec<Arc<TextBox>>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            size: [1920., 1080.],
            strokes: vec![],
            text_boxes: vec![],
        }
    }
}

struct HistoryStep {
    change: HistoryChange,
    bytes: usize,
}

enum HistoryChange {
    Page(Vec<Arc<Stroke>>, Vec<Arc<TextBox>>, usize),
    StrokeTail(usize, Vec<Arc<Stroke>>, usize),
    Erasure(Vec<StrokeRangeEdit>, Vec<Arc<Stroke>>, usize),
    Text(TextEdit),
    Group(Vec<HistoryStep>),
}

struct StrokeRangeEdit {
    first: usize,
    remove: usize,
    insert: usize,
}

enum TextEdit {
    Insert(usize, Arc<TextBox>),
    Remove(usize),
    Replace(usize, Arc<TextBox>),
}

impl HistoryStep {
    fn new(strokes: Vec<Arc<Stroke>>, text_boxes: Vec<Arc<TextBox>>, point_count: usize) -> Self {
        let bytes = text_boxes.iter().fold(
            stroke_history_bytes(&strokes)
                .saturating_add(text_boxes.capacity() * size_of::<Arc<TextBox>>()),
            |total, item| total.saturating_add(text_history_bytes(item)),
        );
        Self {
            change: HistoryChange::Page(strokes, text_boxes, point_count),
            bytes,
        }
    }

    fn tail(first: usize, strokes: Vec<Arc<Stroke>>, point_count: usize) -> Self {
        Self {
            bytes: stroke_history_bytes(&strokes),
            change: HistoryChange::StrokeTail(first, strokes, point_count),
        }
    }

    fn erasure(edits: Vec<StrokeRangeEdit>, strokes: Vec<Arc<Stroke>>, point_count: usize) -> Self {
        Self {
            bytes: stroke_history_bytes(&strokes)
                .saturating_add(edits.capacity() * size_of::<StrokeRangeEdit>()),
            change: HistoryChange::Erasure(edits, strokes, point_count),
        }
    }

    fn text(edit: TextEdit) -> Self {
        let bytes = match &edit {
            TextEdit::Insert(_, item) | TextEdit::Replace(_, item) => text_history_bytes(item),
            TextEdit::Remove(_) => 0,
        };
        Self {
            change: HistoryChange::Text(edit),
            bytes,
        }
    }

    fn group(edits: Vec<Self>) -> Self {
        let bytes = edits
            .iter()
            .fold(edits.capacity() * size_of::<Self>(), |bytes, edit| {
                bytes.saturating_add(edit.bytes)
            });
        Self {
            change: HistoryChange::Group(edits),
            bytes,
        }
    }
}

fn stroke_history_bytes(strokes: &Vec<Arc<Stroke>>) -> usize {
    strokes.iter().fold(
        strokes.capacity() * size_of::<Arc<Stroke>>(),
        |total, stroke| {
            total.saturating_add(
                size_of::<Stroke>()
                    + 2 * size_of::<usize>()
                    + stroke.points.capacity() * size_of::<Point>(),
            )
        },
    )
}

fn text_history_bytes(item: &TextBox) -> usize {
    size_of::<TextBox>() + 2 * size_of::<usize>() + item.text.capacity()
}

#[derive(Default)]
pub struct History {
    doc: Document,
    point_count: usize,
    pub active: Option<Stroke>,
    undo: VecDeque<HistoryStep>,
    redo: VecDeque<HistoryStep>,
    stroke_index: RefCell<StrokeIndex>,
    // Appends can be painted over the committed layer. Erasure supplies its
    // own damage, while replacing committed ink invalidates the whole layer.
    append_revision: u64,
    erase_revision: u64,
    ink_revision: u64,
    #[cfg(any(target_os = "macos", test))]
    wide_gamut_ink: std::cell::Cell<Option<([u64; 3], bool)>>,
    // Committed text is independent of ink and of a live editor draft.
    // A renderer can keep static text until this revision changes.
    text_revision: u64,
    pub changed: bool,
}
impl History {
    pub fn document(&self) -> &Document {
        &self.doc
    }

    pub fn strokes(&self) -> &[Arc<Stroke>] {
        &self.doc.strokes
    }

    pub fn text_boxes(&self) -> &[Arc<TextBox>] {
        &self.doc.text_boxes
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

    #[cfg(any(target_os = "macos", test))]
    pub fn has_wide_gamut_ink(&self) -> bool {
        let revision = [self.ink_revision, self.append_revision, self.erase_revision];
        if let Some((cached_revision, has_wide_gamut)) = self.wide_gamut_ink.get()
            && cached_revision == revision
        {
            return has_wide_gamut;
        }
        let has_wide_gamut = self
            .strokes()
            .iter()
            .any(|stroke| stroke.has_wide_gamut_ink());
        self.wide_gamut_ink.set(Some((revision, has_wide_gamut)));
        has_wide_gamut
    }

    pub fn text_revision(&self) -> u64 {
        self.text_revision
    }

    pub fn point_count(&self) -> usize {
        self.point_count
    }

    fn stroke_index(&self) -> Ref<'_, StrokeIndex> {
        let first = self.stroke_index.borrow().bounds.len();
        let next = &self.doc.strokes[first..];
        if !next.is_empty() {
            let mut index = self.stroke_index.borrow_mut();
            for stroke in next {
                index.push(stroke);
            }
        }
        self.stroke_index.borrow()
    }

    pub fn stroke_bounds(&self) -> Ref<'_, [Option<StrokeBounds>]> {
        Ref::map(self.stroke_index(), |index| index.bounds.as_slice())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn record_undo(&mut self, step: HistoryStep) {
        self.redo.clear();
        self.undo.push_back(step);
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

    fn replace_with_undo(&mut self, doc: Document, point_count: usize) {
        let old = std::mem::replace(&mut self.doc, doc);
        if !old.strokes.is_empty() || !self.doc.strokes.is_empty() {
            self.invalidate_stroke_bounds();
            self.ink_revision = self.ink_revision.wrapping_add(1);
        }
        if !old.text_boxes.is_empty() || !self.doc.text_boxes.is_empty() {
            self.text_revision = self.text_revision.wrapping_add(1);
        }
        let old_count = std::mem::replace(&mut self.point_count, point_count);
        self.record_undo(HistoryStep::new(old.strokes, old.text_boxes, old_count));
    }

    fn invalidate_stroke_bounds(&mut self) {
        let index = self.stroke_index.get_mut();
        index.bounds.clear();
        index.blocks.clear();
        if index.bounds.capacity() > 4096 {
            index.bounds = Vec::new();
        }
    }

    pub fn append(&mut self, strokes: impl Iterator<Item = Stroke>) {
        let first = self.doc.strokes.len();
        let previous_points = self.point_count;
        self.doc.strokes.extend(
            strokes
                .inspect(|stroke| self.point_count += stroke.points.len())
                .map(Arc::new),
        );
        if self.doc.strokes.len() != first {
            self.record_undo(HistoryStep::tail(first, Vec::new(), previous_points));
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

    pub fn set_text_box(&mut self, index: Option<usize>, value: TextBox) -> Option<usize> {
        if value.text.trim().is_empty() {
            if let Some(index) = index {
                self.remove_text_box(index);
            }
            return None;
        }
        self.put_text_box(index, value, false)
    }

    fn put_text_box(&mut self, index: Option<usize>, value: TextBox, merge: bool) -> Option<usize> {
        if let Some(index) = index {
            if **self.doc.text_boxes.get(index)? != value {
                let old = std::mem::replace(&mut self.doc.text_boxes[index], Arc::new(value));
                self.record_text_edit(TextEdit::Replace(index, old), merge);
                self.text_revision = self.text_revision.wrapping_add(1);
            }
            return Some(index);
        }
        let index = self.doc.text_boxes.len();
        if index >= MAX_TEXT_BOXES {
            return None;
        }
        self.record_text_edit(TextEdit::Remove(index), merge);
        self.doc.text_boxes.push(Arc::new(value));
        self.text_revision = self.text_revision.wrapping_add(1);
        Some(index)
    }

    fn record_text_edit(&mut self, edit: TextEdit, merge: bool) {
        let step = HistoryStep::text(edit);
        if !merge {
            self.record_undo(step);
        } else if let Some(previous) = self.undo.pop_back() {
            let mut edits = match previous.change {
                HistoryChange::Group(edits) => edits,
                _ => vec![previous],
            };
            edits.push(step);
            self.record_undo(HistoryStep::group(edits));
        }
    }

    pub fn remove_text_box(&mut self, index: usize) -> bool {
        if index >= self.doc.text_boxes.len() {
            return false;
        }
        let old = self.doc.text_boxes.remove(index);
        self.record_undo(HistoryStep::text(TextEdit::Insert(index, old)));
        self.text_revision = self.text_revision.wrapping_add(1);
        true
    }

    fn swap_page(
        &mut self,
        strokes: Vec<Arc<Stroke>>,
        text_boxes: Vec<Arc<TextBox>>,
        points: usize,
    ) -> HistoryStep {
        let strokes = std::mem::replace(&mut self.doc.strokes, strokes);
        let text_boxes = std::mem::replace(&mut self.doc.text_boxes, text_boxes);
        if text_boxes.len() != self.doc.text_boxes.len()
            || text_boxes
                .iter()
                .zip(&self.doc.text_boxes)
                .any(|(a, b)| !Arc::ptr_eq(a, b))
        {
            self.text_revision = self.text_revision.wrapping_add(1);
        }
        let point_count = std::mem::replace(&mut self.point_count, points);
        if strokes.len() != self.doc.strokes.len()
            || strokes
                .iter()
                .zip(&self.doc.strokes)
                .any(|(a, b)| !Arc::ptr_eq(a, b))
        {
            self.invalidate_stroke_bounds();
            self.ink_revision = self.ink_revision.wrapping_add(1);
        }
        HistoryStep::new(strokes, text_boxes, point_count)
    }

    fn apply_step(&mut self, step: HistoryStep) -> HistoryStep {
        self.changed = true;
        match step.change {
            HistoryChange::Page(strokes, text, points) => self.swap_page(strokes, text, points),
            HistoryChange::StrokeTail(first, strokes, points) => {
                let removed = self.doc.strokes.split_off(first);
                self.doc.strokes.extend(strokes);
                // The index for the unchanged prefix remains valid.
                let bounds = self.stroke_index.get_mut();
                bounds.bounds.truncate(first);
                bounds.blocks.retain(|&index, _| index < first);
                self.ink_revision = self.ink_revision.wrapping_add(1);
                let old_points = std::mem::replace(&mut self.point_count, points);
                HistoryStep::tail(first, removed, old_points)
            }
            HistoryChange::Erasure(mut edits, strokes, points) => {
                let mut old = std::mem::take(&mut self.doc.strokes).into_iter();
                let mut replacement = strokes.into_iter();
                let mut inverse = Vec::new();
                let mut consumed = 0;
                for edit in &mut edits {
                    self.doc
                        .strokes
                        .extend(old.by_ref().take(edit.first - consumed));
                    consumed = edit.first + edit.remove;
                    edit.first = self.doc.strokes.len();
                    inverse.extend(old.by_ref().take(edit.remove));
                    self.doc
                        .strokes
                        .extend(replacement.by_ref().take(edit.insert));
                    std::mem::swap(&mut edit.remove, &mut edit.insert);
                }
                self.doc.strokes.extend(old);
                self.invalidate_stroke_bounds();
                self.ink_revision = self.ink_revision.wrapping_add(1);
                let old_points = std::mem::replace(&mut self.point_count, points);
                HistoryStep::erasure(edits, inverse, old_points)
            }
            HistoryChange::Text(edit) => {
                let inverse = match edit {
                    TextEdit::Insert(index, item) => {
                        self.doc.text_boxes.insert(index, item);
                        TextEdit::Remove(index)
                    }
                    TextEdit::Remove(index) => {
                        TextEdit::Insert(index, self.doc.text_boxes.remove(index))
                    }
                    TextEdit::Replace(index, item) => TextEdit::Replace(
                        index,
                        std::mem::replace(&mut self.doc.text_boxes[index], item),
                    ),
                };
                self.text_revision = self.text_revision.wrapping_add(1);
                HistoryStep::text(inverse)
            }
            HistoryChange::Group(edits) => {
                let inverse = edits
                    .into_iter()
                    .rev()
                    .map(|edit| self.apply_step(edit))
                    .collect();
                HistoryStep::group(inverse)
            }
        }
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
    pub fn clear_with_text(&mut self, draft: Option<(Option<usize>, TextBox)>) {
        // Undoing clear restores the draft. Undoing the preceding action
        // also rolls back the draft, as with a full-page snapshot.
        if let Some((index, item)) = draft.filter(|(_, item)| !item.text.trim().is_empty()) {
            self.put_text_box(index, item, true);
        }
        self.clear();
    }

    pub fn clear(&mut self) {
        self.active = None;
        if !self.doc.is_empty() {
            let empty = Document {
                size: self.doc.size,
                version: self.doc.version,
                ..Document::default()
            };
            self.replace_with_undo(empty, 0);
        }
    }
    pub fn set_document(&mut self, doc: Document, restore: bool) {
        let point_count = doc.strokes.iter().map(|stroke| stroke.points.len()).sum();
        if restore {
            *self = Self {
                doc,
                point_count,
                ink_revision: self.ink_revision.wrapping_add(1),
                text_revision: self.text_revision.wrapping_add(1),
                ..Self::default()
            };
        } else {
            self.replace_with_undo(doc, point_count);
            self.active = None;
        }
    }
}
impl Document {
    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty() && self.text_boxes.is_empty()
    }

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
            if s.tool.is_text() {
                return Err("Text annotations must be stored in text boxes".into());
            }
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
        if self.text_boxes.len() > MAX_TEXT_BOXES
            || self.text_boxes.iter().any(|item| !item.valid())
        {
            return Err("Invalid text annotation".into());
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
    fn undo_append_and_text_edits_keep_only_the_change() -> Result<(), String> {
        let mut history = History::default();
        let strokes = std::iter::repeat_with(|| Arc::new(stroke()))
            .take(20_000)
            .collect();
        history.set_document(
            Document {
                strokes,
                ..Document::default()
            },
            true,
        );
        let original = Arc::clone(&history.strokes()[0]);
        history.append(std::iter::once(stroke()));
        let step = history.undo.back().ok_or("missing stroke undo")?;
        assert!(
            matches!(&step.change, HistoryChange::StrokeTail(20_000, strokes, 40_000) if strokes.is_empty())
        );
        assert_eq!(step.bytes, 0);

        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800., 600.));
        let mut draft = TextBox::new(egui::pos2(160., 100.), screen, InkColor::default(), 1.);
        draft.text = "Draft restored by clear".into();
        history.clear_with_text(Some((None, draft)));
        history.undo();
        assert_eq!(history.strokes().len(), 20_001);
        assert_eq!(history.text_boxes().len(), 1);
        history.undo();
        assert_eq!(history.strokes().len(), 20_000);
        assert!(history.text_boxes().is_empty());
        assert!(Arc::ptr_eq(&history.strokes()[0], &original));
        history.redo();
        assert_eq!(history.strokes().len(), 20_001);
        assert_eq!(history.text_boxes().len(), 1);
        history.redo();
        assert!(history.document().is_empty());
        Ok(())
    }

    #[test]
    fn point_span_rejects_out_of_bounds_without_reusing_end_point() {
        let point = |x| Point::new(x, 0., 1., [0.; 2]);
        let samples = [point(10.), point(20.)];
        let span = PointSpan {
            points: &samples,
            start: Some(point(0.)),
            end: Some(point(30.)),
        };
        assert_eq!(span.len(), 4);
        for (index, x) in [0., 10., 20., 30.].into_iter().enumerate() {
            assert_eq!(span.at(index).map(|point| point.x), Some(x));
        }
        assert!(span.at(4).is_none());
        assert!(span.at(usize::MAX).is_none());
        assert!(PointSpan::new(&[]).at(0).is_none());
        let only_end = PointSpan {
            points: &[],
            start: None,
            end: Some(point(5.)),
        };
        assert_eq!(only_end.at(0).map(|point| point.x), Some(5.));
        assert!(only_end.at(1).is_none());
    }

    #[test]
    fn history_budget_counts_fragment_capacity_and_both_stacks() {
        let mut fragment = stroke();
        fragment.points.reserve(4096);
        let point_count = fragment.points.len();
        let step = HistoryStep::new(vec![Arc::new(fragment)], Vec::new(), point_count);
        assert!(step.bytes > 4096 * size_of::<Point>());
        let mut history = History::default();
        for stack in [&mut history.undo, &mut history.redo] {
            stack.push_back(HistoryStep {
                change: HistoryChange::Page(Vec::new(), Vec::new(), 0),
                bytes: MAX_HISTORY_BYTES / 2,
            });
        }
        history.undo.push_back(step);
        history.trim_steps();
        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.redo.len(), 1);
        assert!(matches!(
            &history.undo[0].change,
            HistoryChange::Page(strokes, _, _) if strokes.len() == 1
        ));
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
    fn text_history_keeps_the_committed_ink_revision() {
        let mut history = History::default();
        history.append(std::iter::once(stroke()));
        let ink_revision = history.ink_revision();
        let item = TextBox {
            origin: [10., 10.],
            size: [150., 60.],
            font_size: 28.,
            color: InkColor::default(),
            text: "Annotation".into(),
            scroll_offset: 0.,
        };
        let text_revision = history.text_revision();
        history.set_text_box(None, item.clone());
        assert_ne!(history.text_revision(), text_revision);
        let inserted_revision = history.text_revision();
        history.undo();
        assert_ne!(history.text_revision(), inserted_revision);
        assert!(history.text_boxes().is_empty());
        assert_eq!(history.ink_revision(), ink_revision);
        history.redo();
        assert_eq!(*history.text_boxes()[0], item);
        assert_eq!(history.ink_revision(), ink_revision);
        history.undo();
        history.undo();
        assert!(history.strokes().is_empty());
        assert_ne!(history.ink_revision(), ink_revision);
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
