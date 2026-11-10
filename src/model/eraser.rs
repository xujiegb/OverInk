use super::{History, MAX_POINTS, Point, Stroke, Tool, segment_distance};
use std::{borrow::Cow, collections::BTreeMap, mem, slice, sync::Arc};

#[derive(Default)]
pub struct Erasure {
    pub last: Option<Point>,
    edits: BTreeMap<usize, Edit>,
}

struct Edit {
    sweeps: Vec<Sweep>,
    fragments: Vec<Arc<Stroke>>,
}

impl Erasure {
    pub fn sample(&mut self, strokes: &[Arc<Stroke>], point: Point, radius: f32) {
        if self.last.is_some_and(|last| last.dist(point) <= 0.05) {
            return;
        }
        let from = self.last.replace(point).unwrap_or(point);
        let sweep = Sweep {
            from,
            to: point,
            radius,
        };
        for (index, stroke) in strokes.iter().enumerate() {
            let remaining = self
                .edits
                .get(&index)
                .map_or(slice::from_ref(stroke), |edit| edit.fragments.as_slice());
            if let Some(fragments) = sweep.cut_strokes(remaining) {
                let edit = self.edits.entry(index).or_insert_with(|| Edit {
                    sweeps: Vec::new(),
                    fragments: Vec::new(),
                });
                edit.sweeps.push(sweep);
                edit.fragments = fragments;
            }
        }
    }

    pub fn preview<'a>(
        stroke: &'a Arc<Stroke>,
        index: usize,
        drafts: impl Iterator<Item = &'a Self>,
    ) -> Cow<'a, [Arc<Stroke>]> {
        let mut edits = drafts.filter_map(|draft| draft.edits.get(&index));
        let Some(first) = edits.next() else {
            return Cow::Borrowed(slice::from_ref(stroke));
        };
        let mut remaining = Cow::Borrowed(first.fragments.as_slice());
        for sweep in edits.flat_map(|edit| &edit.sweeps) {
            if let Some(fragments) = sweep.cut_strokes(&remaining) {
                remaining = Cow::Owned(fragments);
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
        history.snapshot();
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
                    for sweep in &edit.sweeps {
                        if let Some(fragments) = sweep.cut_strokes(&remaining) {
                            remaining = Cow::Owned(fragments);
                        }
                    }
                    if let Cow::Owned(fragments) = remaining {
                        draft.edits.insert(
                            index,
                            Edit {
                                sweeps: edit.sweeps.clone(),
                                fragments,
                            },
                        );
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
        let mut cuts = points.windows(2).enumerate().filter_map(|(index, pair)| {
            // Allow room for the round caps on the remaining fragments.
            let radius = self.radius + ink_radius(pair[0]).max(ink_radius(pair[1]));
            self.interval(pair[0], pair[1], radius)
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
                fragments.push(run);
            }
            tail = (exit < 1.).then(|| a.interpolate(b, exit));
            next = index + 1;
        }
        let mut run: Vec<_> = tail.into_iter().collect();
        run.extend_from_slice(&points[next..]);
        // The last endpoint is erased if the last segment was cut to its end.
        if !run.is_empty() && (tail.is_some() || next + 1 < points.len()) {
            fragments.push(run);
        }
        Some(fragments)
    }

    // A sweep is a rectangle with two round caps. Its intersection with a
    // stroke segment is a single interval, including between sparse samples.
    fn interval(self, a: Point, b: Point, radius: f32) -> Option<[f32; 2]> {
        if a.x.min(b.x) > self.from.x.max(self.to.x) + radius
            || a.x.max(b.x) < self.from.x.min(self.to.x) - radius
            || a.y.min(b.y) > self.from.y.max(self.to.y) + radius
            || a.y.max(b.y) < self.from.y.min(self.to.y) - radius
        {
            return None;
        }
        if a.x == b.x && a.y == b.y {
            return (segment_distance(a, self.from, self.to) < radius).then_some([0., 1.]);
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
        let radius = f64::from(radius);
        let len_sq = dx * dx + dy * dy;
        let cap = |center: f64| {
            let x = x - center;
            let cross = x * dy - y * dx;
            let disc = radius * radius * len_sq - cross * cross;
            if disc <= 0. {
                return None;
            }
            let middle = -(x * dx + y * dy) / len_sq;
            let half = disc.sqrt() / len_sq;
            intersection([middle - half, middle + half], [0., 1.])
        };
        let body = band(x, dx, 0., length)
            .and_then(|along| intersection(along, band(y, dy, -radius, radius)?));
        [body, cap(0.), cap(length)]
            .into_iter()
            .flatten()
            .reduce(|a, b| [a[0].min(b[0]), a[1].max(b[1])])
            .map(|cut| cut.map(|v| v as f32))
            .filter(|[enter, exit]| enter < exit)
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
