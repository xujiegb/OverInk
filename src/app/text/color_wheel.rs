use crate::color::InkColor;
use egui::{Color32, Mesh, Painter, Pos2, Sense, Shape, Stroke, Ui, Vec2, ecolor::HsvaGamma, vec2};
use std::f32::consts::TAU;

#[derive(Clone, Copy)]
pub(super) struct ColorWheel {
    initial: HsvaGamma,
    pub value: HsvaGamma,
}

impl ColorWheel {
    pub fn new(color: InkColor) -> Self {
        let [r, g, b] = color.srgb8();
        let initial = Color32::from_rgb(r, g, b).into();
        Self {
            initial,
            value: initial,
        }
    }

    pub fn chosen_color(self) -> Option<InkColor> {
        (self.value != self.initial).then(|| {
            let [r, g, b, _] = Color32::from(self.value).to_array();
            InkColor::from_rgb8([r, g, b])
        })
    }

    pub fn show(&mut self, ui: &mut Ui, size: f32, scale: f32) {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
        let center = rect.center();
        let radius = (size * 0.5 - 6. * scale).max(0.);
        // A short window can leave no room for the disc after the brightness
        // and action rows. Do not divide by zero or paint an inverted mesh.
        if radius == 0. {
            return;
        }
        if let Some(pos) = response.interact_pointer_pos() {
            self.select(pos - center, radius);
        }

        paint_disc(ui.painter(), center, radius, 60, 4);
        let selected = center + direction(self.value.h) * (radius * self.value.s);
        ui.painter().circle_filled(selected, 5. * scale, self.value);
        ui.painter().circle_stroke(
            selected,
            6. * scale,
            Stroke::new(3. * scale, Color32::BLACK),
        );
        ui.painter().circle_stroke(
            selected,
            6. * scale,
            Stroke::new(1.5 * scale, Color32::WHITE),
        );
    }

    fn select(&mut self, offset: Vec2, radius: f32) {
        // The wheel displays bright hues. Picking one from pure black must
        // produce a visible color even if the brightness slider is untouched.
        if self.value.v == 0. {
            self.value.v = 1.;
        }
        self.value.s = (offset.length() / radius).min(1.);
        if self.value.s > 0. {
            self.value.h = (offset.y.atan2(offset.x) / TAU).rem_euclid(1.);
        }
    }
}

fn direction(hue: f32) -> Vec2 {
    let (sin, cos) = (hue * TAU).sin_cos();
    vec2(cos, sin)
}

pub(super) fn paint_disc(painter: &Painter, center: Pos2, radius: f32, segments: u32, rings: u32) {
    let color = |h, s| HsvaGamma { h, s, v: 1., a: 1. }.into();
    let mut mesh = Mesh::default();
    mesh.vertices.reserve_exact((1 + rings * segments) as usize);
    mesh.indices
        .reserve_exact((3 * (2 * rings - 1) * segments) as usize);
    mesh.colored_vertex(center, color(0., 0.));
    for ring in 1..=rings {
        let sat = ring as f32 / rings as f32;
        let start = 1 + (ring - 1) * segments;
        for segment in 0..segments {
            let hue = segment as f32 / segments as f32;
            mesh.colored_vertex(center + direction(hue) * (radius * sat), color(hue, sat));
            let here = start + segment;
            let next = start + (segment + 1) % segments;
            if ring == 1 {
                mesh.add_triangle(0, here, next);
            } else {
                mesh.add_triangle(here - segments, here, next);
                mesh.add_triangle(here - segments, next, next - segments);
            }
        }
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choosing_hue_from_black_produces_visible_color_without_brightness_drag() {
        let mut wheel = ColorWheel::new(InkColor::from_rgb8([0, 0, 0]));
        assert!(wheel.chosen_color().is_none());
        wheel.select(vec2(100., 0.), 100.);
        assert_eq!(
            wheel.chosen_color().map(|color| color.srgb8()),
            Some([255, 0, 0])
        );
        assert_eq!(wheel.value.v, 1.);
    }

    #[test]
    fn the_wheel_selects_hue_and_saturation_without_changing_brightness() {
        let mut wheel = ColorWheel::new(InkColor::from_rgb8([128; 3]));
        assert!(wheel.chosen_color().is_none());
        wheel.select(vec2(100., 0.), 100.);
        assert_eq!(
            wheel.chosen_color().map(|color| color.srgb8()),
            Some([128, 0, 0])
        );
        wheel.select(vec2(0., 150.), 100.);
        assert_eq!([wheel.value.h, wheel.value.s], [0.25, 1.]);
        wheel.select(Vec2::ZERO, 100.);
        assert_eq!(
            wheel.chosen_color().map(|color| color.srgb8()),
            Some([128; 3])
        );
    }
}
