use super::{History, MAX_POINTS, Point, Stroke, Tool, segment_distance};
use std::{borrow::Cow, collections::BTreeMap, mem, slice, sync::Arc};

#[derive(Default)]
pub struct Erasure {
    last: Option<Point>,
    pending: Option<(Point, f32)>,
    sweeps: Vec<Sweep>,
    edits: BTreeMap<usize, Edit>,
}

struct Edit {
    sweeps: Vec<usize>,
    fragments: Vec<Arc<Stroke>>,
}

impl Erasure {
    pub fn has_edits(&self) -> bool {
        !self.edits.is_empty()
    }

    pub fn sample(&mut self, strokes: &[Arc<Stroke>], point: Point, radius: f32) -> bool {
        if let Some(last) = self.last {
            let distance = last.dist(point);
            if distance <= (radius * 0.1).clamp(1.0, 3.0) {
                self.pending = (distance > 0.).then_some((point, radius));
                return false;
            }
        }
        self.pending = None;
        self.apply_sample(strokes, point, radius)
    }

    pub fn break_path(&mut self, strokes: &[Arc<Stroke>]) -> bool {
        let changed = self.finish_sample(strokes);
        self.last = None;
        changed
    }

    fn finish_sample(&mut self, strokes: &[Arc<Stroke>]) -> bool {
        let Some((point, radius)) = self.pending.take() else {
            return false;
        };
        self.apply_sample(strokes, point, radius)
    }

    fn apply_sample(&mut self, strokes: &[Arc<Stroke>], point: Point, radius: f32) -> bool {
        let from = self.last.replace(point).unwrap_or(point);
        let sweep = Sweep {
            from,
            to: point,
            radius,
        };
        let sweep_index = self.sweeps.len();
        let mut changed = false;
        for (index, stroke) in strokes.iter().enumerate() {
            let remaining = self
                .edits
                .get(&index)
                .map_or(slice::from_ref(stroke), |edit| edit.fragments.as_slice());
            if let Some(fragments) = sweep.cut_strokes(remaining) {
                changed = true;
                let edit = self.edits.entry(index).or_insert_with(|| Edit {
                    sweeps: Vec::new(),
                    fragments: Vec::new(),
                });
                edit.sweeps.push(sweep_index);
                edit.fragments = fragments;
            }
        }
        if changed {
            self.sweeps.push(sweep);
        }
        changed
    }

    pub fn preview<'a>(
        stroke: &'a Arc<Stroke>,
        index: usize,
        drafts: impl Iterator<Item = &'a Self>,
    ) -> Cow<'a, [Arc<Stroke>]> {
        let mut edits =
            drafts.filter_map(|draft| draft.edits.get(&index).map(|edit| (draft, edit)));
        let Some((_, first)) = edits.next() else {
            return Cow::Borrowed(slice::from_ref(stroke));
        };
        let mut remaining = Cow::Borrowed(first.fragments.as_slice());
        for (draft, edit) in edits {
            for &sweep in &edit.sweeps {
                if let Some(fragments) = draft.sweeps[sweep].cut_strokes(&remaining) {
                    remaining = Cow::Owned(fragments);
                }
            }
        }
        remaining
    }

    pub fn commit(
        mut self,
        history: &mut History,
        other_drafts: &mut BTreeMap<u64, Self>,
        point_count: usize,
    ) -> Result<(), &'static str> {
        self.finish_sample(&history.doc.strokes);
        if self.edits.is_empty() {
            return Ok(());
        }
        let removed: usize = self
            .edits
            .keys()
            .map(|i| history.doc.strokes[*i].points.len())
            .sum();
        let added: usize = self
            .edits
            .values()
            .flat_map(|edit| &edit.fragments)
            .map(|stroke| stroke.points.len())
            .sum();
        if point_count - removed + added > MAX_POINTS {
            return Err("Erase would exceed the one million sample limit.");
        }
        history.snapshot_replacement();
        let mut remap = Vec::new();
        for (index, stroke) in mem::take(&mut history.doc.strokes).into_iter().enumerate() {
            let start = history.doc.strokes.len();
            let edit = self.edits.remove(&index);
            let replaced = edit.is_some();
            if let Some(edit) = edit {
                history.doc.strokes.extend(edit.fragments);
            } else {
                history.doc.strokes.push(stroke);
            }
            if !other_drafts.is_empty() {
                remap.push((start..history.doc.strokes.len(), replaced));
            }
        }
        // Keep the scope of overlapping contacts by replaying their cuts only
        // over the surviving pieces of the strokes they originally touched.
        for draft in other_drafts.values_mut() {
            for (old_index, edit) in mem::take(&mut draft.edits) {
                let (range, replaced) = &remap[old_index];
                if !replaced {
                    draft.edits.insert(range.start, edit);
                    continue;
                }
                for index in range.clone() {
                    let mut remaining = Cow::Borrowed(slice::from_ref(&history.doc.strokes[index]));
                    let mut sweeps = Vec::new();
                    for &sweep in &edit.sweeps {
                        if let Some(fragments) = draft.sweeps[sweep].cut_strokes(&remaining) {
                            sweeps.push(sweep);
                            remaining = Cow::Owned(fragments);
                        }
                    }
                    if let Cow::Owned(fragments) = remaining {
                        draft.edits.insert(index, Edit { sweeps, fragments });
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Sweep {
    from: Point,
    to: Point,
    radius: f32,
}

impl Sweep {
    fn cut_strokes(self, strokes: &[Arc<Stroke>]) -> Option<Vec<Arc<Stroke>>> {
        let mut remaining: Option<Vec<Arc<Stroke>>> = None;
        for (index, stroke) in strokes.iter().enumerate() {
            if let Some(fragments) = self.cut_stroke(stroke) {
                remaining
                    .get_or_insert_with(|| strokes[..index].to_vec())
                    .extend(fragments.into_iter().map(Arc::new));
            } else if let Some(remaining) = &mut remaining {
                remaining.push(stroke.clone());
            }
        }
        remaining
    }

    fn cut_stroke(self, stroke: &Stroke) -> Option<Vec<Stroke>> {
        let points = stroke.polyline();
        let shaft = self.cut_path(&points, |point| stroke.radius(point));
        let head = stroke.arrow_head();
        let head_cut = head
            .as_ref()
            .and_then(|head| self.cut_path(head, |_| stroke.width * 0.5));
        if shaft.is_none() && head_cut.is_none() {
            return None;
        }
        let tool = match stroke.tool {
            Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::Ellipse => Tool::Pen,
            tool => tool,
        };
        let fragment = |points, pressure_enabled| Stroke {
            tool,
            color: stroke.color,
            width: stroke.width,
            opacity: stroke.opacity,
            pressure_curve: stroke.pressure_curve,
            pressure_enabled,
            points,
        };
        let mut fragments: Vec<_> = shaft
            .unwrap_or_else(|| vec![points.into_owned()])
            .into_iter()
            .map(|points| fragment(points, stroke.pressure_enabled))
            .collect();
        if let Some(head) = head {
            fragments.extend(
                head_cut
                    .unwrap_or_else(|| vec![head.to_vec()])
                    .into_iter()
                    .map(|points| fragment(points, false)),
            );
        }
        Some(fragments)
    }

    fn cut_path(
        self,
        points: &[Point],
        ink_radius: impl Fn(Point) -> f32,
    ) -> Option<Vec<Vec<Point>>> {
        if let [dot] = points {
            return (segment_distance(*dot, self.from, self.to) < self.radius + ink_radius(*dot))
                .then(Vec::new);
        }
        let mut start_radius = points.first().copied().map_or(0., &ink_radius);
        let mut cuts = points.windows(2).enumerate().filter_map(|(index, pair)| {
            // Allow room for the round caps on the remaining fragments.
            let end_radius = ink_radius(pair[1]);
            let radii = [mem::replace(&mut start_radius, end_radius), end_radius];
            self.interval(pair[0], pair[1], radii)
                .map(|cut| (index, cut))
        });
        let first = cuts.next()?;
        let mut fragments = Vec::new();
        let mut next = 0;
        let mut tail = None;
        for (index, [enter, exit]) in std::iter::once(first).chain(cuts) {
            let a = points[index];
            let b = points[index + 1];
            let mut run: Vec<_> = tail.take().into_iter().collect();
            run.extend_from_slice(&points[next..index]);
            if enter > 0. || !run.is_empty() {
                run.push(a);
                if enter > 0. {
                    run.push(a.interpolate(b, enter));
                }
                push_fragment(&mut fragments, run);
            }
            tail = (exit < 1.).then(|| a.interpolate(b, exit));
            next = index + 1;
        }
        let mut run: Vec<_> = tail.into_iter().collect();
        run.extend_from_slice(&points[next..]);
        // The last endpoint is erased if the last segment was cut to its end.
        if !run.is_empty() && (tail.is_some() || next + 1 < points.len()) {
            push_fragment(&mut fragments, run);
        }
        Some(fragments)
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

fn push_fragment(fragments: &mut Vec<Vec<Point>>, run: Vec<Point>) {
    let mut length = 0.;
    if run.windows(2).any(|pair| {
        length += pair[0].dist(pair[1]);
        length >= 0.5
    }) {
        fragments.push(run);
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
        let fragments = sweep.cut_stroke(&stroke).unwrap();
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
        let fragments = sweep
            .cut_path(&[Point::new(0., 0., 1., [0.; 2]), corner, end], |_| 0.)
            .unwrap();
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
            Point::new(4., -4., 0.1, [0.; 2]),
            Point::new(4., 0., 0.1, [0.; 2]),
            Point::new(4., 4., 1., [0.; 2]),
        ];
        let fragments = sweep
            .cut_path(&points, |point| 5. * point.pressure)
            .unwrap();
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
        let fragments = sweep.cut_path(&points, |_| 0.).unwrap();
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0][0].x, 53.);
        assert_eq!(fragments[0][1].x, 70.);
    }

    #[test]
    fn skipped_samples_still_reach_the_release_point() {
        let mut history = History::default();
        history.doc.strokes.push(Arc::new(Stroke {
            width: 0.2,
            pressure_enabled: false,
            points: vec![Point::new(1.3, 0., 1., [0.; 2])],
            ..pencil()
        }));
        let mut erase = Erasure::default();
        for i in 0..=10 {
            let p = Point::new(i as f32 * 0.05, 0., 1., [0.; 2]);
            assert!(!erase.sample(&history.doc.strokes, p, 1.));
        }
        assert_eq!(erase.last.unwrap().x, 0.);
        erase.commit(&mut history, &mut BTreeMap::new(), 1).unwrap();
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
        let fragments = sweep.cut_stroke(&stroke).unwrap();
        assert_eq!(fragments.len(), 2);
        let shaft = &fragments[0];
        assert_eq!(shaft.tool, Tool::Pen);
        assert_eq!(shaft.points[0].x, 0.);
        assert_eq!(shaft.points[1].x, 100.);
        assert!(fragments[1].points[0].x > point.x);
    }
}
