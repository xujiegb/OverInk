//! One pressure ribbon per stroke. No series of overlapping translucent stamps.
use crate::model::{Point, Stroke, Tool};
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
fn disk(m: &mut Mesh, p: Point, r: f32, aa: f32) {
    let c = add(m, [p.x, p.y], 1.);
    let n = 32;
    let base = m.vertices.len() as u32;
    for i in 0..=n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        for (rr, alpha) in [(r, 1.), (r + aa, 0.)] {
            add(m, [p.x + rr * a.cos(), p.y + rr * a.sin()], alpha);
        }
    }
    for i in 0..n {
        let a = base + i * 2;
        tri(m, c, a, a + 2);
        tri(m, a, a + 1, a + 3);
        tri(m, a, a + 3, a + 2);
    }
}
fn cap(m: &mut Mesh, p: Point, r: f32, angle: f32, aa: f32) {
    let c = add(m, [p.x, p.y], 1.);
    let base = m.vertices.len() as u32;
    let n = 16;
    for i in 0..=n {
        let a = angle + i as f32 / n as f32 * std::f32::consts::PI;
        for (rr, alpha) in [(r, 1.), (r + aa, 0.)] {
            add(m, [p.x + rr * a.cos(), p.y + rr * a.sin()], alpha);
        }
    }
    for i in 0..n {
        let a = base + i * 2;
        tri(m, c, a, a + 2);
        tri(m, a, a + 1, a + 3);
        tri(m, a, a + 3, a + 2);
    }
}
fn ribbon(m: &mut Mesh, points: &[Point], stroke: &Stroke, aa: f32) {
    let mut p = Vec::new();
    for &v in points {
        if p.last().is_none_or(|prev: &Point| prev.dist(v) > 0.05) {
            p.push(v)
        }
    }
    if p.len() == 1 {
        disk(m, p[0], stroke.radius(p[0]), aa);
        return;
    }
    if p.is_empty() {
        return;
    }
    let base = m.vertices.len() as u32;
    for (i, &q) in p.iter().enumerate() {
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
        for (distance, alpha) in [(r + aa, 0.), (r, 1.), (-r, 1.), (-r - aa, 0.)] {
            add(
                m,
                [q.x + normal[0] * distance, q.y + normal[1] * distance],
                alpha,
            );
        }
        if i > 0 {
            let a = base + (i as u32 - 1) * 4;
            let b = a + 4;
            for j in 0..3 {
                tri(m, a + j, b + j, a + j + 1);
                tri(m, a + j + 1, b + j, b + j + 1);
            }
        }
    }
    let a = p[0];
    let b = p[1];
    let end = *p.last().unwrap();
    let prev = p[p.len() - 2];
    cap(
        m,
        a,
        stroke.radius(a),
        (b.y - a.y).atan2(b.x - a.x) + std::f32::consts::FRAC_PI_2,
        aa,
    );
    cap(
        m,
        end,
        stroke.radius(end),
        (end.y - prev.y).atan2(end.x - prev.x) - std::f32::consts::FRAC_PI_2,
        aa,
    );
}
pub fn mesh(stroke: &Stroke, aa: f32) -> Mesh {
    let mut m = Mesh::default();
    let mut points = stroke.polyline();
    if stroke.tool == Tool::Pencil && points.len() > 1 {
        let mut samples = vec![points[0]];
        for pair in points.windows(2) {
            let steps = (pair[0].dist(pair[1]) / 2.0).ceil().clamp(1.0, 32.0) as usize;
            for i in 1..=steps {
                let t = i as f32 / steps as f32;
                let a = pair[0];
                let b = pair[1];
                samples.push(Point::new(
                    a.x + (b.x - a.x) * t,
                    a.y + (b.y - a.y) * t,
                    a.pressure + (b.pressure - a.pressure) * t,
                    [
                        a.tilt[0] + (b.tilt[0] - a.tilt[0]) * t,
                        a.tilt[1] + (b.tilt[1] - a.tilt[1]) * t,
                    ],
                ));
            }
            if samples.len() >= 100_000 {
                break;
            }
        }
        points = samples;
    }
    ribbon(&mut m, &points, stroke, aa);
    if stroke.tool == Tool::Pencil {
        // Stable paper-like density, so redrawing the same samples never flickers.
        for vertex in &mut m.vertices {
            let mut hash = (vertex.xy[0].floor() as i32 as u32).wrapping_mul(0x45d9f3b)
                ^ (vertex.xy[1].floor() as i32 as u32).wrapping_mul(0x119de1f3);
            hash ^= hash >> 16;
            vertex.coverage *= 0.32 + 0.68 * (hash & 255) as f32 / 255.0;
        }
    }
    if stroke.tool == Tool::Arrow && stroke.points.len() >= 2 {
        let a = stroke.points[0];
        let b = *stroke.points.last().unwrap();
        let angle = (b.y - a.y).atan2(b.x - a.x);
        let length = (stroke.width * 3.5).max(14.).min(a.dist(b) * 0.65);
        let p = |offset: f32| {
            Point::new(
                b.x - length * (angle + offset).cos(),
                b.y - length * (angle + offset).sin(),
                1.,
                [0.; 2],
            )
        };
        let mut head = stroke.clone();
        head.pressure_enabled = false;
        ribbon(&mut m, &[p(0.5), b, p(-0.5)], &head, aa);
    }
    m
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::InkColor;
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
