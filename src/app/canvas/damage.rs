use super::{Canvas, CanvasPaintTimings, PaintScope};
use crate::{
    app::{App, Board},
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
    ink: [u64; 3],
    color: u64,
    units: f32,
    board: u8,
    pass: bool,
    protected: Option<controller::Rect>,
}

pub(super) struct DamageLayer {
    texture: InkTexture,
    key: CompositionKey,
    footprint: Option<Rect>,
}

fn dirty_rect(rect: Rect, screen: Rect) -> Option<SDL_Rect> {
    if !rect.is_positive() {
        return None;
    }
    // Expand to include adjacent edge pixels at fractional coordinates.
    let left = (rect.left().floor() - 1.).clamp(0., screen.width()) as i32;
    let top = (rect.top().floor() - 1.).clamp(0., screen.height()) as i32;
    let right = (rect.right().ceil() + 1.).clamp(0., screen.width()) as i32;
    let bottom = (rect.bottom().ceil() + 1.).clamp(0., screen.height()) as i32;
    (right > left && bottom > top).then_some(SDL_Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    })
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
            || app.history.active.is_none() && app.input.laser.is_empty()
        {
            self.damage = None;
            return Ok(false);
        }
        let cached = self.ink.cached_bytes();
        if cached < MIN_INK_COMPOSITION_BYTES {
            self.damage = None;
            return Ok(false);
        }
        let pixels = self.window.pixel_size();
        let bytes = crate::app::ink::layer_bytes(pixels, SDL_PIXELFORMAT_RGBA32);
        let size = self.window.size();
        if pixels.iter().any(|&value| value <= 0)
            || pixels.map(|value| value as f32) != size
            || bytes.saturating_add(cached) > MAX_COMPOSITION_BYTES
        {
            self.damage = None;
            return Ok(false);
        }
        let Some(footprint) = active_ink_bounds(app) else {
            self.damage = None;
            return Ok(false);
        };
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(size));
        let previous = self.damage.as_ref().and_then(|layer| layer.footprint);
        let dirty = previous.map_or(footprint, |previous| footprint.union(previous));
        // A full-size texture copy costs more than a small direct draw.
        // Use retained composition for a local change over a large ink layer.
        if dirty.intersect(screen).area() > screen.area() / 4. {
            self.damage = None;
            return Ok(false);
        }
        let key = CompositionKey {
            pixels,
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
        let region = if layer.key == key && layer.footprint.is_some() {
            dirty
        } else {
            screen
        };
        let ink_start = timings.as_ref().map(|_| Instant::now());
        if let Some(rect) = dirty_rect(region, screen) {
            let result = self.render.with_target(&layer.texture, || {
                let result = (|| {
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
                    self.ink
                        .paint_overlays(app, &self.render, &self.window, &self.color, scale)
                })();
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
        // The last fade frame clears the previous trail. It also stops the
        // animation timer, so release the composition here when drawing ends.
        if self.animation || app.history.active.is_some() {
            layer.key = key;
            layer.footprint = Some(footprint);
            self.damage = Some(layer);
        }
        Ok(true)
    }
}
