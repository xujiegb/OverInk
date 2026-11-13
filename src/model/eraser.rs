use super::{
    Bounds, History, MAX_POINTS, Point, PointSpan, Stroke, StrokeBounds, Tool, segment_distance,
};
use std::{borrow::Cow, collections::BTreeMap, mem, ops::Range, sync::Arc};

pub const MAX_ERASURE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum EraseError {
    Samples,
    Preview,
}

#[derive(Default)]
pub struct Erasure {
    last: Option<Point>,
    pending: Option<(Point, f32)>,
    sweeps: Vec<Sweep>,
    edits: BTreeMap<usize, Edit>,
    edit_bytes: usize,
    delta: i64,
    limited: Option<EraseError>,
    damage: Option<Bounds>,
    affected: Option<Bounds>,
    affected_strokes: Option<Bounds>,
}

struct Edit {
    sweeps: Vec<u32>,
    paths: Paths,
}

impl Edit {
    fn bytes(&self) -> usize {
        // Charge the map entry as well as the owned buffers.
        size_of::<(usize, Self)>()
            + 2 * size_of::<usize>()
            + self.sweeps.capacity() * size_of::<u32>()
            + self.paths.buffer_bytes()
    }
}

#[derive(Clone)]
struct Paths {
    outline: Option<Box<[Point]>>,
    head: Option<[Point; 3]>,
    fragments: Vec<Fragment>,
    count: usize,
}

#[derive(Clone)]
struct Fragment {
    // The range addresses the immutable stroke or a converted shape.
    // Only the two interpolated endpoints belong to this fragment.
    range: Range<usize>,
    start: Option<Point>,
    end: Option<Point>,
    head: bool,
    bounds: StrokeBounds,
}

impl Fragment {
    fn points<'a>(&self, source: &'a [Point]) -> PointSpan<'a> {
        PointSpan {
            points: &source[self.range.clone()],
            start: self.start,
            end: self.end,
        }
    }

    fn len(&self) -> usize {
        self.range.len() + usize::from(self.start.is_some()) + usize::from(self.end.is_some())
    }

    fn keep(
        &self,
        source: &[Point],
        kept: Range<usize>,
        start: Option<Point>,
        end: Option<Point>,
    ) -> Option<Self> {
        let point_span = self.points(source);
        let first = usize::from(point_span.start.is_some());
        let range = self.range.start + kept.start.saturating_sub(first).min(self.range.len())
            ..self.range.start + kept.end.saturating_sub(first).min(self.range.len());
        let span = PointSpan {
            points: &source[range.clone()],
            start: start.or_else(|| point_span.start.filter(|_| kept.contains(&0))),
            end: end.or_else(|| {
                point_span
                    .end
                    .filter(|_| kept.contains(&(point_span.len() - 1)))
            }),
        };
        let mut length = 0.;
        let mut previous = None::<Point>;
        let points = span.iter().inspect(|&point| {
            if length < 0.5
                && let Some(last) = previous
            {
                length += last.dist(point);
            }
            previous = Some(point);
        });
        let bounds = StrokeBounds::measure(points)?;
        (length >= 0.5).then_some(Self {
            range,
            start: span.start,
            end: span.end,
            head: self.head,
            bounds,
        })
    }
}

impl Paths {
    fn buffer_bytes(&self) -> usize {
        self.fragments.capacity() * size_of::<Fragment>()
            + self
                .outline
                .as_ref()
                .map_or(0, |points| size_of_val(&**points))
    }

    fn new(stroke: &Stroke, bounds: StrokeBounds) -> Self {
        let polyline = stroke.polyline();
        let count = polyline.len();
        let outline = match polyline {
            Cow::Owned(points) => Some(points.into_boxed_slice()),
            Cow::Borrowed(_) => None,
        };
        let head = stroke.arrow_head();
        let mut fragments = Vec::with_capacity(1 + usize::from(head.is_some()));
        fragments.push(Fragment {
            range: 0..count,
            start: None,
            end: None,
            head: false,
            bounds,
        });
        if let Some(points) = head {
            fragments.push(Fragment {
                range: 0..points.len(),
                start: None,
                end: None,
                head: true,
                bounds: StrokeBounds::measure(points.into_iter()).unwrap(),
            });
        }
        Self {
            outline,
            head,
            fragments,
            count: count + head.map_or(0, |head| head.len()),
        }
    }

    fn visit(&self, stroke: &Stroke, mut visit: impl FnMut(&Stroke, PointSpan<'_>, StrokeBounds)) {
        let mut style = Stroke {
            tool: if stroke.tool.is_shape() {
                Tool::Pen
            } else {
                stroke.tool
            },
            points: Vec::new(),
            ..*stroke
        };
        for fragment in &self.fragments {
            style.pressure_enabled = stroke.pressure_enabled && !fragment.head;
            let source = path_source(&self.outline, &self.head, stroke, fragment.head);
            visit(&style, fragment.points(source), fragment.bounds);
        }
    }

    fn cut(
        &mut self,
        sweep: Sweep,
        stroke: &Stroke,
        replacement: &mut Vec<Fragment>,
    ) -> Option<Bounds> {
        let mut damage = None;
        let mut index = 0;
        while index < self.fragments.len() {
            let fragment = &self.fragments[index];
            let source = path_source(&self.outline, &self.head, stroke, fragment.head);
            if let Some(bounds) = sweep.cut_path(source, fragment, stroke, replacement) {
                Bounds::include(&mut damage, bounds);
                let count = replacement.len();
                self.count = self.count - fragment.len()
                    + replacement.iter().map(Fragment::len).sum::<usize>();
                self.fragments.splice(index..=index, replacement.drain(..));
                index += count;
            } else {
                index += 1;
            }
        }
        damage
    }
}

fn path_source<'a>(
    outline: &'a Option<Box<[Point]>>,
    head: &'a Option<[Point; 3]>,
    stroke: &'a Stroke,
    is_head: bool,
) -> &'a [Point] {
    if is_head {
        head.as_ref().unwrap()
    } else {
        outline.as_deref().unwrap_or(&stroke.points)
    }
}

impl Erasure {
    pub fn has_edits(&self) -> bool {
        !self.edits.is_empty()
    }

    pub fn take_damage(&mut self) -> Option<Bounds> {
        self.damage.take()
    }

    pub fn affected(&self, overlapping: bool) -> Option<Bounds> {
        if overlapping {
            self.affected_strokes
        } else {
            self.affected
        }
    }

    pub fn available_budget<'a>(others: impl Iterator<Item = &'a Self>) -> usize {
        MAX_ERASURE_BYTES.saturating_sub(others.map(Self::bytes).sum())
    }

    fn bytes(&self) -> usize {
        self.edit_bytes + self.sweeps.capacity() * size_of::<Sweep>()
    }

    fn reject(&mut self, error: EraseError, overlapping: bool) {
        if let Some(bounds) = self.affected(overlapping) {
            Bounds::include(&mut self.damage, bounds);
        }
        let damage = self.damage;
        *self = Self {
            limited: Some(error),
            damage,
            ..Self::default()
        };
    }

    pub fn sample(
        &mut self,
        history: &History,
        point_count: usize,
        budget: usize,
        point: Point,
        radius: f32,
    ) {
        if let Some(last) = self.last {
            let distance = last.dist(point);
            if distance <= (radius * 0.1).clamp(1.0, 3.0) {
                self.pending = (distance > 0.).then_some((point, radius));
                return;
            }
        }
        self.pending = None;
        self.apply_sample(history, point_count, budget, point, radius);
    }

    pub fn break_path(&mut self, history: &History, point_count: usize, budget: usize) {
        self.finish_sample(history, point_count, budget);
        self.last = None;
    }

    fn finish_sample(&mut self, history: &History, point_count: usize, budget: usize) {
        if let Some((point, radius)) = self.pending.take() {
            self.apply_sample(history, point_count, budget, point, radius);
        }
    }

    fn apply_sample(
        &mut self,
        history: &History,
        point_count: usize,
        budget: usize,
        point: Point,
        radius: f32,
    ) {
        if self.limited.is_some() {
            return;
        }
        let from = self.last.replace(point).unwrap_or(point);
        let sweep = Sweep {
            from,
            to: point,
            radius,
        };
        let sweep_index = self.sweeps.len() as u32;
        let mut changed = false;
        let mut replacement = Vec::new();
        for (index, (stroke, bounds)) in history
            .doc
            .strokes
            .iter()
            .zip(history.stroke_bounds().iter())
            .enumerate()
        {
            let Some(bounds) = *bounds else {
                continue;
            };
            if !sweep.bounds(stroke).intersects(bounds.path) {
                continue;
            }
            let (before, after, damage) = if let Some(edit) = self.edits.get_mut(&index) {
                let before = edit.paths.count;
                let old_bytes = edit.bytes();
                let Some(damage) = edit.paths.cut(sweep, stroke, &mut replacement) else {
                    continue;
                };
                edit.sweeps.push(sweep_index);
                self.edit_bytes = self.edit_bytes - old_bytes + edit.bytes();
                (before, edit.paths.count, damage)
            } else {
                let mut paths = Paths::new(stroke, bounds);
                let Some(damage) = paths.cut(sweep, stroke, &mut replacement) else {
                    continue;
                };
                let after = paths.count;
                let edit = Edit {
                    sweeps: vec![sweep_index],
                    paths,
                };
                self.edit_bytes += edit.bytes();
                self.edits.insert(index, edit);
                (stroke.points.len(), after, damage)
            };
            changed = true;
            Bounds::include(&mut self.damage, damage);
            Bounds::include(&mut self.affected, damage);
            let full = bounds.path.padded(stroke.max_radius() * 2.);
            Bounds::include(&mut self.affected_strokes, full);
            if budget < MAX_ERASURE_BYTES {
                // Overlapping contacts replay their cuts against new endpoints.
                Bounds::include(&mut self.damage, full);
            }
            self.delta += after as i64 - before as i64;
            if self.bytes() > budget {
                self.reject(EraseError::Preview, budget < MAX_ERASURE_BYTES);
                return;
            }
        }
        if point_count as i64 + self.delta > MAX_POINTS as i64 {
            self.reject(EraseError::Samples, budget < MAX_ERASURE_BYTES);
        } else if changed {
            self.sweeps.push(sweep);
            if self.bytes() > budget {
                self.reject(EraseError::Preview, budget < MAX_ERASURE_BYTES);
            }
        }
    }

    pub fn preview<'a>(
        stroke: &'a Stroke,
        index: usize,
        drafts: impl Iterator<Item = &'a Self>,
        visit: impl FnMut(&Stroke, PointSpan<'_>, StrokeBounds),
    ) -> bool {
        let mut edits =
            drafts.filter_map(|draft| draft.edits.get(&index).map(|edit| (draft, edit)));
        let Some((_, first)) = edits.next() else {
            return false;
        };
        let mut remaining = Cow::Borrowed(&first.paths);
        let mut replacement = Vec::new();
        'replay: for (draft, edit) in edits {
            for &sweep in &edit.sweeps {
                remaining
                    .to_mut()
                    .cut(draft.sweeps[sweep as usize], stroke, &mut replacement);
                if remaining.buffer_bytes() > MAX_ERASURE_BYTES {
                    // Individual drafts still commit or cancel independently.
                    remaining = Cow::Borrowed(&first.paths);
                    break 'replay;
                }
            }
        }
        remaining.visit(stroke, visit);
        true
    }

    pub fn commit(
        mut self,
        history: &mut History,
        other_drafts: &mut BTreeMap<u64, Self>,
        point_count: usize,
        damage: &mut Option<Bounds>,
    ) -> Result<(), EraseError> {
        let mut other_bytes: usize = other_drafts.values().map(Self::bytes).sum();
        self.finish_sample(
            history,
            point_count,
            MAX_ERASURE_BYTES.saturating_sub(other_bytes),
        );
        if let Some(bounds) = self.take_damage() {
            Bounds::include(damage, bounds);
        }
        if let Some(error) = self.limited {
            return Err(error);
        }
        if point_count as i64 + self.delta > MAX_POINTS as i64 {
            if let Some(affected) = self.affected(other_bytes != 0) {
                Bounds::include(damage, affected);
            }
            return Err(EraseError::Samples);
        }
        if self.edits.is_empty() {
            return Ok(());
        }
        if other_bytes != 0
            && let Some(bounds) = self.affected_strokes
        {
            Bounds::include(damage, bounds);
        }
        history.snapshot();
        history.erase_revision = history.erase_revision.wrapping_add(1);
        let committed_points = history.point_count as i64 + self.delta;
        debug_assert!((0..=MAX_POINTS as i64).contains(&committed_points));
        history.point_count = committed_points as usize;
        let previous = mem::take(&mut history.doc.strokes);
        let mut bounds = mem::take(history.stroke_bounds.get_mut()).into_iter();
        let mut remap = Vec::new();
        for (index, stroke) in previous.into_iter().enumerate() {
            let old_bounds = bounds.next().unwrap_or_else(|| StrokeBounds::new(&stroke));
            let start = history.doc.strokes.len();
            let edit = self.edits.remove(&index);
            let replaced = edit.is_some();
            if let Some(edit) = edit {
                edit.paths.visit(&stroke, |style, points, bounds| {
                    let mut samples = Vec::with_capacity(points.len());
                    samples.extend(points.iter());
                    history.doc.strokes.push(Arc::new(Stroke {
                        points: samples,
                        ..*style
                    }));
                    history.stroke_bounds.get_mut().push(Some(bounds));
                });
            } else {
                history.doc.strokes.push(stroke);
                history.stroke_bounds.get_mut().push(old_bounds);
            }
            if other_bytes != 0 {
                remap.push((start..history.doc.strokes.len(), replaced));
            }
        }
        for draft in other_drafts
            .values_mut()
            .filter(|draft| !draft.edits.is_empty())
        {
            other_bytes -= draft.bytes();
            let budget = MAX_ERASURE_BYTES.saturating_sub(other_bytes);
            draft.remap(history, &remap, point_count as i64 + self.delta, budget);
            other_bytes += draft.bytes();
            if let Some(bounds) = draft.take_damage() {
                Bounds::include(damage, bounds);
            }
        }
        Ok(())
    }

    fn remap(
        &mut self,
        history: &History,
        remap: &[(Range<usize>, bool)],
        point_count: i64,
        budget: usize,
    ) {
        self.delta = 0;
        self.edit_bytes = 0;
        let bounds = history.stroke_bounds();
        let mut replacement = Vec::new();
        for (old_index, edit) in mem::take(&mut self.edits) {
            let (range, replaced) = &remap[old_index];
            if !replaced {
                self.insert_remapped(range.start, &history.doc.strokes[range.start], edit);
            } else {
                for index in range.clone() {
                    let stroke = &history.doc.strokes[index];
                    let mut remapped = Edit {
                        sweeps: Vec::new(),
                        paths: Paths::new(stroke, bounds[index].unwrap()),
                    };
                    for &sweep in &edit.sweeps {
                        if remapped
                            .paths
                            .cut(self.sweeps[sweep as usize], stroke, &mut replacement)
                            .is_some()
                        {
                            remapped.sweeps.push(sweep);
                            if self.bytes() + remapped.bytes() > budget {
                                self.reject(EraseError::Preview, true);
                                return;
                            }
                        }
                    }
                    if !remapped.sweeps.is_empty() {
                        self.insert_remapped(index, stroke, remapped);
                    }
                }
            }
            if self.bytes() > budget {
                self.reject(EraseError::Preview, true);
                return;
            }
        }
        if point_count + self.delta > MAX_POINTS as i64 {
            self.reject(EraseError::Samples, true);
        } else if self.edits.is_empty() {
            self.sweeps = Vec::new();
            self.affected = None;
            self.affected_strokes = None;
        }
    }

    fn insert_remapped(&mut self, index: usize, stroke: &Stroke, edit: Edit) {
        self.delta += edit.paths.count as i64 - stroke.points.len() as i64;
        self.edit_bytes += edit.bytes();
        self.edits.insert(index, edit);
    }
}

#[derive(Clone, Copy)]
struct Sweep {
    from: Point,
    to: Point,
    radius: f32,
}

impl Sweep {
    fn bounds(self, stroke: &Stroke) -> Bounds {
        Bounds::between(self.from, self.to).padded(self.radius + stroke.max_radius())
    }

    fn cut_path(
        self,
        source: &[Point],
        fragment: &Fragment,
        stroke: &Stroke,
        kept: &mut Vec<Fragment>,
    ) -> Option<Bounds> {
        let bounds = self.bounds(stroke);
        if !bounds.intersects(fragment.bounds.path) {
            return None;
        }
        let points = fragment.points(source);
        let radius = |point| {
            if fragment.head {
                stroke.width * 0.5
            } else {
                stroke.radius(point)
            }
        };
        let margin = stroke.max_radius() * 2. + 0.1;
        if points.len() == 1 {
            let dot = points.at(0);
            return (segment_distance(dot, self.from, self.to) < self.radius + radius(dot))
                .then(|| fragment.bounds.path.padded(margin));
        }
        let mut previous_radius = None;
        let mut cuts = (0..points.len().saturating_sub(1)).filter_map(|index| {
            let a = points.at(index);
            let b = points.at(index + 1);
            if !bounds.intersects(Bounds::between(a, b)) {
                previous_radius = None;
                return None;
            }
            let start_radius = previous_radius.unwrap_or_else(|| radius(a));
            let end_radius = radius(b);
            previous_radius = Some(end_radius);
            self.interval(a, b, [start_radius, end_radius])
                .map(|cut| (index, cut))
        });
        let first = cuts.next()?;
        let mut damage = None;
        let mut next = 0;
        let mut tail = None;
        let mut run_end = |next, end, tail, head, damage: &mut Option<Bounds>| {
            if let Some(part) = fragment.keep(source, next..end, tail, head) {
                kept.push(part);
            } else {
                let discarded = tail
                    .into_iter()
                    .chain((next..end).map(|i| points.at(i)))
                    .chain(head);
                if let Some(bounds) = Bounds::points(discarded) {
                    Bounds::include(damage, bounds);
                }
            }
        };
        for (index, [enter, exit]) in std::iter::once(first).chain(cuts) {
            let a = points.at(index);
            let b = points.at(index + 1);
            let head = (enter > 0.).then(|| a.interpolate(b, enter));
            let start = head.unwrap_or(a);
            // Replacing a segment changes its adjacent joins and round caps.
            Bounds::include(&mut damage, Bounds::between(a, b));
            if index > 0 {
                Bounds::include(&mut damage, Bounds::between(points.at(index - 1), start));
            }
            if index + 2 < points.len() {
                Bounds::include(&mut damage, Bounds::between(b, points.at(index + 2)));
            }
            if enter > 0. || tail.is_some() || next < index {
                run_end(next, index + 1, tail, head, &mut damage);
            }
            tail = (exit < 1.).then(|| a.interpolate(b, exit));
            next = index + 1;
        }
        if tail.is_some() || next + 1 < points.len() {
            run_end(next, points.len(), tail, None, &mut damage);
        }
        let marker_split = stroke.tool == Tool::Marker && kept.len() > 1;
        let damage = if !matches!(stroke.tool, Tool::Pen | Tool::Marker)
            || marker_split
            || !fragment.bounds.well_spaced
            || kept.iter().any(|part| !part.bounds.well_spaced)
        {
            // Separate marker masks can change opacity at remote crossings.
            // Shape conversion and sample processing can also change distant pixels.
            fragment.bounds.path
        } else {
            damage.unwrap()
        };
        Some(damage.padded(margin))
    }

    // A sweep is a rectangle with two round caps. Its intersection with a
    // stroke segment is a single interval, including between sparse samples.
    fn interval(self, a: Point, b: Point, [start, end]: [f32; 2]) -> Option<[f32; 2]> {
        let radius = self.radius + start.max(end);
        if a.x.min(b.x) > self.from.x.max(self.to.x) + radius
            || a.x.max(b.x) < self.from.x.min(self.to.x) - radius
            || a.y.min(b.y) > self.from.y.max(self.to.y) + radius
            || a.y.max(b.y) < self.from.y.min(self.to.y) - radius
        {
            return None;
        }
        let sx = f64::from(self.to.x) - f64::from(self.from.x);
        let sy = f64::from(self.to.y) - f64::from(self.from.y);
        let length = sx.hypot(sy);
        let local = |p: Point| {
            let x = f64::from(p.x) - f64::from(self.from.x);
            let y = f64::from(p.y) - f64::from(self.from.y);
            if length > 0. {
                [(x * sx + y * sy) / length, (y * sx - x * sy) / length]
            } else {
                [x, y]
            }
        };
        let [x, y] = local(a);
        let [bx, by] = local(b);
        let (dx, dy) = (bx - x, by - y);
        let radius = f64::from(self.radius) + f64::from(start);
        let dr = f64::from(end) - f64::from(start);
        let quadratic = dx * dx + dy * dy - dr * dr;
        let cap = |center: f64| {
            let x = x - center;
            quadratic_below(
                quadratic,
                x * dx + y * dy - radius * dr,
                x * x + y * y - radius * radius,
            )
        };
        let cut = if length == 0. {
            cap(0.)
        } else {
            let body = band(x, dx, 0., length)
                .and_then(|along| intersection(along, linear_below(y - radius, dy - dr)?))
                .and_then(|above| intersection(above, linear_below(-y - radius, -dy - dr)?));
            [body, cap(0.), cap(length)]
                .into_iter()
                .flatten()
                .reduce(|a, b| [a[0].min(b[0]), a[1].max(b[1])])
        }?;
        let [enter, exit] = cut.map(|v| v as f32);
        (enter < exit).then_some([enter, exit])
    }
}

fn linear_below(offset: f64, slope: f64) -> Option<[f64; 2]> {
    if slope == 0. {
        return (offset < 0.).then_some([0., 1.]);
    }
    let limit = -offset / slope;
    let bound = if slope > 0. {
        [f64::NEG_INFINITY, limit]
    } else {
        [limit, f64::INFINITY]
    };
    intersection(bound, [0., 1.])
}

// Intersect |p(t) - center| < r(t) after squaring a positive radius.
// `half` is half the linear coefficient.
fn quadratic_below(a: f64, half: f64, c: f64) -> Option<[f64; 2]> {
    if a == 0. {
        return linear_below(c, 2. * half);
    }
    let disc = half.mul_add(half, -a * c);
    if disc <= 0. {
        return (a < 0.).then_some([0., 1.]);
    }
    let q = -half - disc.sqrt().copysign(half);
    let roots = [q / a, c / q];
    let [low, high] = [roots[0].min(roots[1]), roots[0].max(roots[1])];
    if a > 0. {
        intersection([low, high], [0., 1.])
    } else {
        // Distance minus the interpolated radius is convex on [0, 1].
        intersection([f64::NEG_INFINITY, low], [0., 1.])
            .or_else(|| intersection([high, f64::INFINITY], [0., 1.]))
    }
}

fn intersection(a: [f64; 2], b: [f64; 2]) -> Option<[f64; 2]> {
    let interval = [a[0].max(b[0]), a[1].min(b[1])];
    (interval[0] < interval[1]).then_some(interval)
}

fn band(offset: f64, delta: f64, low: f64, high: f64) -> Option<[f64; 2]> {
    if delta == 0. {
        return (low..=high).contains(&offset).then_some([0., 1.]);
    }
    let a = (low - offset) / delta;
    let b = (high - offset) / delta;
    intersection([a.min(b), a.max(b)], [0., 1.])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::InkColor;

    fn pencil() -> Stroke {
        Stroke {
            tool: Tool::Pencil,
            color: InkColor::default(),
            width: 4.,
            opacity: 0.68,
            pressure_curve: 1.,
            pressure_enabled: true,
            points: vec![
                Point::new(0., 0., 0.2, [0.; 2]),
                Point::new(100., 0., 0.8, [60., 0.]),
            ],
        }
    }

    fn cut_stroke(sweep: Sweep, stroke: &Stroke) -> Option<Vec<Stroke>> {
        let mut paths = Paths::new(stroke, StrokeBounds::new(stroke)?);
        paths.cut(sweep, stroke, &mut Vec::new())?;
        let mut fragments = Vec::new();
        paths.visit(stroke, |style, points, _| {
            fragments.push(Stroke {
                points: points.iter().collect(),
                ..*style
            })
        });
        Some(fragments)
    }

    fn cut_points(sweep: Sweep, points: &[Point], width: f32, pressure: bool) -> Vec<Vec<Point>> {
        cut_stroke(
            sweep,
            &Stroke {
                tool: Tool::Pen,
                width,
                pressure_enabled: pressure,
                points: points.to_vec(),
                ..pencil()
            },
        )
        .unwrap()
        .into_iter()
        .map(|fragment| fragment.points)
        .collect()
    }

    #[test]
    fn preview_borrows_samples_and_commit_keeps_compact_fragments() {
        let point = |x, y| Point::new(x, y, 0.6, [30., 0.]);
        let mut history = History::default();
        history.append(std::iter::once(Stroke {
            points: (0..20_000).map(|x| point(x as f32, 0.)).collect(),
            ..pencil()
        }));
        let original = Arc::clone(&history.doc.strokes[0]);
        let mut erase = Erasure::default();
        for x in (200..20_000).step_by(200) {
            erase.sample(
                &history,
                original.points.len(),
                MAX_ERASURE_BYTES,
                point(x as f32, 0.),
                8.,
            );
            erase.break_path(&history, original.points.len(), MAX_ERASURE_BYTES);
        }
        let mut count = 0;
        assert!(Erasure::preview(
            &original,
            0,
            std::iter::once(&erase),
            |_, span, _| {
                count += 1;
                assert!(span.points.as_ptr_range().start >= original.points.as_ptr_range().start);
                assert!(span.points.as_ptr_range().end <= original.points.as_ptr_range().end);
            }
        ));
        assert_eq!(count, 100);
        assert!(erase.bytes() < original.points.len() * size_of::<Point>() / 10);
        erase
            .commit(
                &mut history,
                &mut BTreeMap::new(),
                original.points.len(),
                &mut None,
            )
            .unwrap();
        assert_eq!(history.doc.strokes.len(), count);
        assert!(
            history
                .doc
                .strokes
                .iter()
                .all(|stroke| stroke.points.len() == stroke.points.capacity())
        );
        history.undo();
        assert!(Arc::ptr_eq(&history.doc.strokes[0], &original));
        history.redo();
        assert_eq!(history.doc.strokes.len(), count);
    }

    #[test]
    fn preview_budget_discards_the_gesture_and_restores_its_damage() {
        let mut history = History::default();
        history.append(std::iter::once(pencil()));
        let mut erase = Erasure::default();
        let point = Point::new(50., 0., 1., [0.; 2]);
        erase.sample(&history, 2, 1, point, 5.);
        assert!(matches!(erase.limited, Some(EraseError::Preview)));
        assert_eq!(erase.bytes(), 0);
        assert!(!Erasure::preview(
            &history.doc.strokes[0],
            0,
            std::iter::once(&erase),
            |_, _, _| unreachable!()
        ));
        let mut damage = None;
        assert!(matches!(
            erase.commit(&mut history, &mut BTreeMap::new(), 2, &mut damage),
            Err(EraseError::Preview)
        ));
        assert!(damage.is_some());
        assert_eq!(history.doc.strokes.len(), 1);
    }

    #[test]
    fn sweep_cuts_between_samples_and_preserves_brush_values() {
        let stroke = pencil();
        let from = Point::new(50., -50., 1., [0.; 2]);
        let to = Point::new(50., 50., 1., [0.; 2]);
        let sweep = Sweep {
            from,
            to,
            radius: 5.,
        };
        let fragments = cut_stroke(sweep, &stroke).unwrap();
        assert_eq!(fragments.len(), 2);
        let left = fragments[0].points.last().unwrap();
        let right = fragments[1].points.first().unwrap();
        assert!(left.x < 45. && right.x > 55.);
        for point in [left, right] {
            assert!((point.pressure - (0.2 + 0.006 * point.x)).abs() < 1e-6);
            assert!((point.tilt[0] - 0.6 * point.x).abs() < 1e-5);
        }
        assert!(fragments.iter().all(|s| {
            s.tool == Tool::Pencil && s.opacity == stroke.opacity && s.pressure_enabled
        }));
    }

    #[test]
    fn cut_ending_at_a_corner_preserves_the_next_segment() {
        let corner = Point::new(10., 0., 0.7, [30., 0.]);
        let end = Point::new(10., 20., 0.9, [40., 0.]);
        let center = Point::new(6., 0., 1., [0.; 2]);
        let sweep = Sweep {
            from: center,
            to: center,
            radius: 4.,
        };
        let fragments = cut_points(
            sweep,
            &[Point::new(0., 0., 1., [0.; 2]), corner, end],
            0.,
            false,
        );
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[1].len(), 2);
        let [start, last] = fragments[1].as_slice() else {
            unreachable!();
        };
        assert_eq!(
            (start.x, start.y, start.pressure, start.tilt),
            (corner.x, corner.y, corner.pressure, corner.tilt)
        );
        assert_eq!((last.x, last.y), (end.x, end.y));
    }

    #[test]
    fn adjacent_segments_agree_at_a_pressure_corner() {
        let center = Point::new(0., 0., 1., [0.; 2]);
        let sweep = Sweep {
            from: center,
            to: center,
            radius: 1.,
        };
        let points = [
            Point::new(4., -4., 0., [0.; 2]),
            Point::new(4., 0., 0., [0.; 2]),
            Point::new(4., 4., 1., [0.; 2]),
        ];
        let fragments = cut_points(sweep, &points, 10., true);
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0][1].y, 0.);
        assert!(fragments[0].last().unwrap().y > 2.);
    }

    #[test]
    fn cut_discards_only_the_subpixel_remnant() {
        let center = Point::new(50., 0., 1., [0.; 2]);
        let sweep = Sweep {
            from: center,
            to: center,
            radius: 3.,
        };
        let points = [46.75, 70.].map(|x| Point::new(x, 0., 1., [0.; 2]));
        let fragments = cut_points(sweep, &points, 0., false);
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0][0].x, 53.);
        assert_eq!(fragments[0][1].x, 70.);
    }

    #[test]
    fn skipped_samples_still_reach_the_release_point() {
        let mut history = History::default();
        history.append(std::iter::once(Stroke {
            width: 0.2,
            pressure_enabled: false,
            points: vec![Point::new(1.3, 0., 1., [0.; 2])],
            ..pencil()
        }));
        let mut erase = Erasure::default();
        for i in 0..=10 {
            let p = Point::new(i as f32 * 0.05, 0., 1., [0.; 2]);
            erase.sample(&history, 1, MAX_ERASURE_BYTES, p, 1.);
            assert!(erase.take_damage().is_none());
        }
        assert_eq!(erase.last.unwrap().x, 0.);
        erase
            .commit(&mut history, &mut BTreeMap::new(), 1, &mut None)
            .unwrap();
        assert!(history.doc.strokes.is_empty());
        history.undo();
        assert_eq!(history.doc.strokes.len(), 1);
    }

    #[test]
    fn tapered_capsule_interval_agrees_with_distance_samples() {
        let mut random = 0x49213a_u32;
        let mut value = || {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            (random >> 16) as f32 / 2048. - 16.
        };
        for i in 0..512 {
            let mut point = || Point::new(value(), value(), 1., [0.; 2]);
            let from = point();
            let to = if i % 2 == 0 { from } else { point() };
            let a = point();
            let b = if i % 5 == 0 { a } else { point() };
            let [start, end] = [value().abs(), value().abs()];
            let sweep = Sweep {
                from,
                to,
                radius: 3.,
            };
            let interval = sweep.interval(a, b, [start, end]);
            for step in 0..=32 {
                let t = step as f32 / 32.;
                let distance = segment_distance(a.interpolate(b, t), from, to);
                let boundary = sweep.radius + start + (end - start) * t;
                if (distance - boundary).abs() > 0.001 {
                    assert_eq!(
                        interval.is_some_and(|[lo, hi]| lo <= t && t <= hi),
                        distance < boundary
                    );
                }
            }
        }
    }

    #[test]
    fn arrowhead_can_be_erased_without_losing_its_shaft() {
        let stroke = Stroke {
            tool: Tool::Arrow,
            width: 10.,
            pressure_enabled: false,
            ..pencil()
        };
        let point = stroke.arrow_head().unwrap()[0];
        let sweep = Sweep {
            from: point,
            to: point,
            radius: 2.,
        };
        let fragments = cut_stroke(sweep, &stroke).unwrap();
        assert_eq!(fragments.len(), 2);
        let shaft = &fragments[0];
        assert_eq!(shaft.tool, Tool::Pen);
        assert_eq!(shaft.points[0].x, 0.);
        assert_eq!(shaft.points[1].x, 100.);
        assert!(fragments[1].points[0].x > point.x);
    }
}
