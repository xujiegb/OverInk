//! One pressure ribbon per stroke. No series of overlapping translucent stamps.
use crate::model::{Point, Stroke, Tool};
use std::ops::RangeInclusive;
#[derive(Clone, Copy, Debug)]
pub struct Vertex {
    pub xy: [f32; 2],
    pub coverage: f32,
}
#[derive(Default, Debug)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}
fn add(m: &mut Mesh, p: [f32; 2], coverage: f32) -> u32 {
    let i = m.vertices.len() as u32;
    m.vertices.push(Vertex { xy: p, coverage });
    i
}
fn tri(m: &mut Mesh, a: u32, b: u32, c: u32) {
    m.indices.extend([a, b, c])
}
fn fan(m: &mut Mesh, p: Point, r: f32, arc: [f32; 2], n: u32, aa: f32, alpha: f32) {
    let center = add(m, [p.x, p.y], alpha);
    let base = m.vertices.len() as u32;
    let stride = if aa > 0. { 2 } else { 1 };
    for i in 0..=n {
        let angle = arc[0] + i as f32 / n as f32 * arc[1];
        let point = |radius| [p.x + radius * angle.cos(), p.y + radius * angle.sin()];
        add(m, point(r), alpha);
        if aa > 0. {
            add(m, point(r + aa), 0.);
        }
    }
    for i in 0..n {
        let a = base + i * stride;
        let b = a + stride;
        tri(m, center, a, b);
        if aa > 0. {
            tri(m, a, a + 1, b + 1);
            tri(m, a, b + 1, b);
        }
    }
}
fn ribbon(m: &mut Mesh, p: &[Point], range: RangeInclusive<usize>, stroke: &Stroke, aa: f32) {
    let coverage = |p: Point| {
        if stroke.tool == Tool::Laser {
            p.pressure
        } else {
            1.
        }
    };
    if p.len() == 1 {
        fan(
            m,
            p[0],
            stroke.radius(p[0]),
            [0., std::f32::consts::TAU],
            32,
            aa,
            coverage(p[0]),
        );
        return;
    }
    if p.is_empty() {
        return;
    }
    let base = m.vertices.len() as u32;
    let stride = if aa > 0. { 4 } else { 2 };
    let (first, last) = (*range.start(), *range.end());
    for i in range {
        let q = p[i];
        let direction = |a: Point, b: Point| {
            let l = a.dist(b).max(1e-6);
            [(b.x - a.x) / l, (b.y - a.y) / l]
        };
        let d0 = direction(p[i.saturating_sub(1)], q);
        let d1 = direction(q, p[(i + 1).min(p.len() - 1)]);
        let (normal, factor) = if i == 0 {
            ([-d1[1], d1[0]], 1.)
        } else if i == p.len() - 1 {
            ([-d0[1], d0[0]], 1.)
        } else {
            let n = [-d0[1] - d1[1], d0[0] + d1[0]];
            let l = n[0].hypot(n[1]);
            if l < 1e-5 {
                ([-d1[1], d1[0]], 1.)
            } else {
                let n = [n[0] / l, n[1] / l];
                let dot = (n[0] * -d1[1] + n[1] * d1[0]).abs().max(0.5);
                (n, 1. / dot)
            }
        };
        let r = stroke.radius(q) * factor;
        let alpha = coverage(q);
        let lanes = [(r + aa, 0.), (r, alpha), (-r, alpha), (-r - aa, 0.)];
        let lanes = if aa > 0. { &lanes[..] } else { &lanes[1..3] };
        for &(distance, alpha) in lanes {
            add(
                m,
                [q.x + normal[0] * distance, q.y + normal[1] * distance],
                alpha,
            );
        }
        if i > first {
            let a = base + ((i - first) as u32 - 1) * stride;
            let b = a + stride;
            for j in 0..stride - 1 {
                tri(m, a + j, b + j, a + j + 1);
                tri(m, a + j + 1, b + j, b + j + 1);
            }
        }
    }
    let a = p[0];
    let b = p[1];
    let end = *p.last().unwrap();
    let prev = p[p.len() - 2];
    if first == 0 {
        fan(
            m,
            a,
            stroke.radius(a),
            [
                (b.y - a.y).atan2(b.x - a.x) + std::f32::consts::FRAC_PI_2,
                std::f32::consts::PI,
            ],
            16,
            aa,
            coverage(a),
        );
    }
    if last + 1 == p.len() {
        fan(
            m,
            end,
            stroke.radius(end),
            [
                (end.y - prev.y).atan2(end.x - prev.x) - std::f32::consts::FRAC_PI_2,
                std::f32::consts::PI,
            ],
            16,
            aa,
            coverage(end),
        );
    }
}
pub fn mesh(stroke: &Stroke, aa: f32) -> Mesh {
    let mut m = Mesh::default();
    let mut samples = Vec::new();
    prepare_samples(stroke, true, &mut samples);
    m.update(
        stroke,
        &samples,
        0..=samples.len().saturating_sub(1),
        aa,
        true,
    );
    m
}

pub fn paper_density(x: i32, y: i32) -> f32 {
    let mut hash = (x as u32).wrapping_mul(0x45d9f3b) ^ (y as u32).wrapping_mul(0x119de1f3);
    hash ^= hash >> 16;
    0.32 + 0.68 * (hash & 255) as f32 / 255.0
}

fn append_distinct(points: &mut Vec<Point>, p: Point) {
    if points.last().is_none_or(|prev| prev.dist(p) > 0.05) {
        points.push(p);
    }
}

pub fn prepare_samples(stroke: &Stroke, vertex_grain: bool, samples: &mut Vec<Point>) {
    samples.clear();
    let points = stroke.polyline();
    if vertex_grain && stroke.tool == Tool::Pencil && points.len() > 1 {
        // Preserve grain in SVG and on renderers without a repeating
        // texture. Limit subdivisions while retaining the whole path.
        let max_steps = (100_000 / points.len()).clamp(1, 32);
        samples.push(points[0]);
        for pair in points.windows(2) {
            let steps = ((pair[0].dist(pair[1]) / 2.0).ceil() as usize).clamp(1, max_steps);
            for i in 1..=steps {
                append_distinct(
                    samples,
                    pair[0].interpolate(pair[1], i as f32 / steps as f32),
                );
            }
        }
    } else {
        for &point in points.iter() {
            append_distinct(samples, point);
        }
    }
}

impl Mesh {
    pub fn update(
        &mut self,
        stroke: &Stroke,
        samples: &[Point],
        range: RangeInclusive<usize>,
        aa: f32,
        vertex_grain: bool,
    ) {
        self.vertices.clear();
        self.indices.clear();
        let end = *range.end() + 1 == samples.len();
        ribbon(self, samples, range, stroke, aa);
        if vertex_grain && stroke.tool == Tool::Pencil {
            for vertex in &mut self.vertices {
                vertex.coverage *=
                    paper_density(vertex.xy[0].floor() as i32, vertex.xy[1].floor() as i32);
            }
        }
        if end && let Some(points) = stroke.arrow_head() {
            let head = Stroke {
                pressure_enabled: false,
                points: Vec::new(),
                ..*stroke
            };
            let mut samples = points;
            let mut count = 1;
            for point in points.into_iter().skip(1) {
                if samples[count - 1].dist(point) > 0.05 {
                    samples[count] = point;
                    count += 1;
                }
            }
            ribbon(self, &samples[..count], 0..=count - 1, &head, aa);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::InkColor;
    #[test]
    fn batching_ribbons_preserves_the_join_and_emits_caps_once() {
        let stroke = Stroke {
            tool: Tool::Pen,
            color: InkColor::default(),
            width: 8.,
            opacity: 0.5,
            pressure_curve: 1.,
            pressure_enabled: true,
            points: vec![
                Point::new(0., 0., 0.3, [0.; 2]),
                Point::new(40., 40., 0.7, [0.; 2]),
                Point::new(80., 20., 1., [0.; 2]),
            ],
        };
        let mut a = Mesh::default();
        let mut b = Mesh::default();
        a.update(&stroke, &stroke.points, 0..=1, 0.8, false);
        b.update(&stroke, &stroke.points, 1..=2, 0.8, false);
        for (left, right) in a.vertices[4..8].iter().zip(&b.vertices[..4]) {
            assert_eq!(left.xy, right.xy);
            assert_eq!(left.coverage, right.coverage);
        }
        assert_eq!(
            a.indices.len() + b.indices.len(),
            mesh(&stroke, 0.8).indices.len()
        );
    }

    #[test]
    fn degenerate_and_pressure_meshes_are_finite() {
        let mut s = Stroke {
            tool: Tool::Pen,
            color: InkColor::default(),
            width: 10.,
            opacity: 0.3,
            pressure_curve: 1.,
            pressure_enabled: true,
            points: vec![],
        };
        for p in [
            Point::new(0., 0., 0., [0.; 2]),
            Point::new(0., 0., 1., [0.; 2]),
            Point::new(100., 1., 1., [0.; 2]),
            Point::new(0., 0., 0.5, [0.; 2]),
        ] {
            s.points.push(p);
            let m = mesh(&s, 0.75);
            assert!(
                m.vertices
                    .iter()
                    .all(|v| v.xy.iter().all(|v| v.is_finite()))
            );
            assert!(m.indices.iter().all(|i| (*i as usize) < m.vertices.len()));
        }
    }
    #[test]
    fn pencil_density_is_stable_and_tilt_changes_its_width() {
        let mut stroke = Stroke {
            tool: Tool::Pencil,
            color: InkColor::default(),
            width: 8.0,
            opacity: 0.68,
            pressure_curve: 1.0,
            pressure_enabled: true,
            points: vec![
                Point::new(10.0, 10.0, 0.2, [0.; 2]),
                Point::new(70.0, 40.0, 0.9, [60.0, 0.0]),
            ],
        };
        let a = mesh(&stroke, 0.5);
        let b = mesh(&stroke, 0.5);
        assert_eq!(a.indices, b.indices);
        assert!(
            a.vertices
                .iter()
                .zip(&b.vertices)
                .all(|(a, b)| a.coverage == b.coverage && a.xy == b.xy)
        );
        assert!(
            a.vertices
                .iter()
                .any(|v| v.coverage > 0.0 && v.coverage < 0.9)
        );
        let tilted = stroke.radius(stroke.points[1]);
        stroke.points[1].tilt = [0.; 2];
        assert!(tilted > stroke.radius(stroke.points[1]));
        assert!(
            a.vertices
                .iter()
                .all(|v| v.coverage.is_finite() && (0.0..=1.0).contains(&v.coverage))
        );
        assert_eq!(
            serde_json::from_str::<Stroke>(&serde_json::to_string(&stroke).unwrap())
                .unwrap()
                .tool,
            Tool::Pencil
        );
    }
}
