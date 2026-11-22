use super::{Canvas, CanvasPaintTimings, PaintScope};
use crate::{
    app::{App, Board, screenshot::outside_selection},
    controller,
    model::{Bounds, Point, Stroke},
    sdl::*,
};
use egui::{Pos2, Rect, Vec2};
use std::time::Instant;

const MAX_COMPOSITION_BYTES: usize = 32 * 1024 * 1024;
const MIN_INK_COMPOSITION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
struct CompositionKey {
    pixels: [i32; 2],
    scale: [f32; 2],
    ink: [u64; 3],
    color: u64,
    units: f32,
    board: u8,
    pass: bool,
    protected: Option<controller::Rect>,
}

#[derive(Clone, Copy)]
enum Footprint {
    Selection(Option<[Rect; 2]>),
    Ink(Rect),
}

pub(super) struct DamageLayer {
    texture: InkTexture,
    key: CompositionKey,
    footprint: Option<Footprint>,
}

struct DamageRects {
    rects: [SDL_Rect; 64],
    count: usize,
}

impl Default for DamageRects {
    fn default() -> Self {
        Self {
            rects: [SDL_Rect::default(); 64],
            count: 0,
        }
    }
}

fn selection_bands(selection: Option<[Rect; 2]>, screen: Rect) -> [Rect; 8] {
    let Some([outer, inner]) = selection else {
        let mut all = [Rect::NOTHING; 8];
        all[0] = screen;
        return all;
    };
    let [a, b, c, d] = outside_selection(screen, outer);
    let [e, f, g, h] = outside_selection(outer, inner);
    [a, b, c, d, e, f, g, h]
}

impl DamageRects {
    fn push(&mut self, rect: Rect, size: [f32; 2]) {
        if !rect.is_positive() {
            return;
        }
        // Include adjacent edge pixels so different SDL rasterizers agree
        // even when the logical selection falls between device pixels.
        let left = (rect.left().floor() - 1.).clamp(0., size[0]) as i32;
        let top = (rect.top().floor() - 1.).clamp(0., size[1]) as i32;
        let right = (rect.right().ceil() + 1.).clamp(0., size[0]) as i32;
        let bottom = (rect.bottom().ceil() + 1.).clamp(0., size[1]) as i32;
        if right > left && bottom > top {
            self.rects[self.count] = SDL_Rect {
                x: left,
                y: top,
                w: right - left,
                h: bottom - top,
            };
            self.count += 1;
        }
    }

    fn difference(&mut self, a: Rect, b: Rect, size: [f32; 2]) {
        let overlap = a.intersect(b);
        if overlap.is_positive() {
            for rect in outside_selection(a, overlap) {
                self.push(rect, size);
            }
        } else {
            self.push(a, size);
        }
    }

    fn between(previous: Option<Footprint>, next: Footprint, screen: Rect) -> Self {
        let mut dirty = Self::default();
        let size = [screen.width(), screen.height()];
        match (previous, next) {
            (Some(Footprint::Selection(a)), Footprint::Selection(b)) => {
                // Compare the submitted quads themselves. SDL can round
                // each rectangle independently at fractional coordinates.
                for (a, b) in selection_bands(a, screen)
                    .into_iter()
                    .zip(selection_bands(b, screen))
                {
                    if a != b {
                        dirty.difference(a, b, size);
                        dirty.difference(b, a, size);
                    }
                }
            }
            (Some(Footprint::Ink(a)), Footprint::Ink(b)) => dirty.push(a.union(b), size),
            _ => dirty.push(screen, size),
        }
        dirty
    }

    fn rects(&self) -> &[SDL_Rect] {
        &self.rects[..self.count]
    }
}

fn active_ink_bounds(app: &App) -> Option<Rect> {
    let mut bounds = None;
    if let Some(stroke) = app.history.active.as_ref() {
        if !stroke.tool.is_shape() {
            return None;
        }
        let mut outline = [Point::EMPTY; Stroke::OUTLINE_POINTS];
        for &point in stroke
            .polyline(&mut outline)
            .iter()
            .chain(stroke.arrow_head().iter().flatten())
        {
            Bounds::include(&mut bounds, Bounds::between(point, point));
        }
        bounds = bounds.map(|bounds| bounds.padded(stroke.max_radius() * 2. + 1.));
    }
    let mut laser = None;
    for sample in app.input.laser.values().flatten() {
        Bounds::include(&mut laser, Bounds::between(sample.point, sample.point));
    }
    if let Some(laser) = laser {
        Bounds::include(&mut bounds, laser.padded(4. * app.input.canvas_units + 1.));
    }
    bounds.map(|bounds| Rect::from_min_max(Pos2::from(bounds.min), Pos2::from(bounds.max)))
}

impl Canvas {
    pub(super) fn paint_damaged_overlay(
        &mut self,
        app: &mut App,
        mut timings: Option<&mut CanvasPaintTimings>,
    ) -> Result<bool, String> {
        let selecting = app.screenshot.targets(self.window.id) && app.screenshot.selecting();
        let cached = self.ink.cached_bytes();
        if self.render.name != "software"
            || self.render.extended
            || self.background.frozen
            || app.multi
            || app.hidden
            || app.picking
            || app.tool.is_text()
            || self.text.is_some()
            || !app.history.text_boxes().is_empty()
            || !app.input.pending_erasures.is_empty()
            || !app.input.touches.is_empty()
            || app.screenshot.active()
                && (!selecting || app.history.active.is_some() || !app.input.laser.is_empty())
            || !selecting && app.history.active.is_none() && app.input.laser.is_empty()
            || !selecting && cached < MIN_INK_COMPOSITION_BYTES
        {
            self.damage = None;
            return Ok(false);
        }
        let size = self.window.size();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        let footprint = if selecting {
            Footprint::Selection(
                app.screenshot
                    .selection_rects(screen, app.input.canvas_units),
            )
        } else if let Some(area) = active_ink_bounds(app) {
            Footprint::Ink(area)
        } else {
            self.damage = None;
            return Ok(false);
        };
        let pixels = self.window.pixel_size();
        let scale = std::array::from_fn(|axis| pixels[axis] as f32 / size[axis]);
        let bytes = crate::app::ink::layer_bytes(pixels, SDL_PIXELFORMAT_RGBA32);
        if scale.iter().any(|&value| !matches!(value, 1. | 2.))
            || !app.history.strokes().is_empty() && (scale != [1.; 2] || cached == 0)
            || bytes.saturating_add(cached) > MAX_COMPOSITION_BYTES
        {
            self.damage = None;
            return Ok(false);
        }
        if let Footprint::Ink(area) = footprint {
            let previous = self.damage.as_ref().and_then(|layer| layer.footprint);
            let dirty = if let Some(Footprint::Ink(previous)) = previous {
                area.union(previous).intersect(screen)
            } else {
                area.intersect(screen)
            };
            // A full-size texture copy costs more than a small direct draw.
            // Use retained composition for a local change over a large ink layer.
            if dirty.area() > screen.area() / 4. {
                self.damage = None;
                return Ok(false);
            }
        }
        let key = CompositionKey {
            pixels,
            scale,
            ink: [
                app.history.ink_revision(),
                app.history.append_revision(),
                app.history.erase_revision(),
            ],
            color: self.color.revision,
            units: app.input.canvas_units,
            board: match app.board {
                None => 0,
                Some(Board::White) => 1,
                Some(Board::Black) => 2,
            },
            pass: app.pass,
            protected: app.input.protected,
        };
        let mut layer = if let Some(layer) = self
            .damage
            .take()
            .filter(|layer| layer.key.pixels == pixels)
        {
            layer
        } else {
            let Ok(texture) = InkTexture::with_format(
                &self.render,
                pixels,
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_NONE,
                SDL_PIXELFORMAT_RGBA32,
            ) else {
                return Ok(false);
            };
            DamageLayer {
                texture,
                key,
                footprint: None,
            }
        };
        let previous = (layer.key == key).then_some(layer.footprint).flatten();
        let dirty = DamageRects::between(previous, footprint, screen);
        let ink_start = timings.as_ref().map(|_| Instant::now());
        if !dirty.rects().is_empty() {
            let result = self.render.with_target(&layer.texture, || {
                let result = dirty.rects().iter().try_for_each(|&rect| {
                    let scope = PaintScope {
                        extended_budget: 0,
                        damage: Some(rect),
                    };
                    let scale = self.ink.paint(
                        app,
                        &self.render,
                        &self.window,
                        &self.color,
                        &mut self.background,
                        scope,
                    )?;
                    if selecting {
                        let [r, g, b, _] = crate::toolbar::ACTIVE.to_array();
                        app.screenshot.paint_selection(
                            &self.render,
                            size,
                            app.input.canvas_units,
                            self.color.map([r, g, b].map(|v| f32::from(v) / 255.)),
                        )?;
                    }
                    self.ink
                        .paint_overlays(app, &self.render, &self.window, &self.color, scale)
                });
                result.and(self.render.try_set_clip_rect(None))
            });
            if result.is_err() {
                self.ink.invalidate_layer();
                return Ok(false);
            }
        }
        if let (Some(start), Some(metrics)) = (ink_start, timings.as_deref_mut()) {
            metrics.ink += start.elapsed();
        }
        let swap_started = timings.as_ref().map(|_| Instant::now());
        self.render.set_render_scale([1.; 2])?;
        self.render.try_set_clip_rect(None)?;
        layer.texture.draw(&self.render, None, None)?;
        self.render.present()?;
        if let (Some(start), Some(metrics)) = (swap_started, timings) {
            metrics.present += start.elapsed();
        }
        self.ink.release_unused_layer(app);
        self.animation = !app.input.laser.is_empty();
        layer.key = key;
        layer.footprint = Some(footprint);
        self.damage = Some(layer);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::screenshot::Screenshot;
    use egui::pos2;

    #[test]
    fn fractional_selection_damage_matches_a_full_frame() -> Result<(), String> {
        let size = [100., 80.];
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        let background = [0.08, 0.17, 0.24, 0.45];
        for scale in [1, 2] {
            let pixels = [100 * scale, 80 * scale];
            let retained = TestSurface::new(pixels)?.software_renderer()?;
            let direct = TestSurface::new(pixels)?.software_renderer()?;
            let texture = InkTexture::with_format(
                &retained,
                pixels,
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_NONE,
                SDL_PIXELFORMAT_RGBA32,
            )?;
            let mut previous = None;
            for end in [
                pos2(77.3, 67.7),
                pos2(71.8, 65.2),
                pos2(12.7, 33.3),
                pos2(20.4, 18.9),
            ] {
                let selection = Screenshot::test_drag(pos2(20.4, 18.9), end);
                let next = Footprint::Selection(selection.selection_rects(screen, 1.));
                let dirty = DamageRects::between(previous, next, screen);
                retained.with_target(&texture, || {
                    retained.set_render_scale([scale as f32; 2])?;
                    for &rect in dirty.rects() {
                        retained.try_set_clip_rect(Some(&rect))?;
                        retained.replace_rect(
                            &SDL_FRect {
                                x: rect.x as f32,
                                y: rect.y as f32,
                                w: rect.w as f32,
                                h: rect.h as f32,
                            },
                            background,
                        )?;
                        selection.paint_selection(&retained, size, 1., [0., 1., 0.])?;
                    }
                    retained.try_set_clip_rect(None)
                })?;
                texture.draw(&retained, None, None)?;
                direct.set_render_scale([scale as f32; 2])?;
                direct.clear(background)?;
                selection.paint_selection(&direct, size, 1., [0., 1., 0.])?;
                let retained_pixels = TestSurface::pixels(&retained)?;
                let direct_pixels = TestSurface::pixels(&direct)?;
                let difference = retained_pixels
                    .iter()
                    .zip(&direct_pixels)
                    .position(|(a, b)| a != b);
                assert!(
                    difference.is_none(),
                    "Selection differs at scale {scale}, index {difference:?}"
                );
                previous = Some(next);
            }
        }
        Ok(())
    }
}
