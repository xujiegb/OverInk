#![forbid(unsafe_code)]

use super::{App, LASER_LIFETIME};
use crate::{
    appearance::text,
    background::Background,
    color::InkColor,
    controller::Rect,
    geometry::{self, Mesh},
    icc::ColorManager,
    model::{Bounds, Erasure, History, Point, PointSpan, Stroke, Tool},
    sdl::*,
};
use std::time::{Duration, Instant};

const PAPER_SIZE: i32 = 64;
const MARKER_TILE: i32 = 1024;
const MESH_SEGMENTS: usize = 1024;
// SDL expands indexed geometry before upload. Bound each upload, including
// those for a large document rebuild or one unusually long stroke.
const GEOMETRY_BATCH: usize = 24_576;
// Extra FP16 ink targets share this allowance across displays.
pub(super) const MAX_EXTENDED_INK_TEXTURE_BYTES: usize = 64 * 1024 * 1024;
// Cheap documents should not reserve a large render target for a few strokes.
// Small SDR targets still benefit from incremental painting immediately.
const MAX_EAGER_SDR_BYTES: usize = 4 * 1024 * 1024;
const CACHE_STROKES: usize = 64;
const CACHE_POINTS: usize = 8192;
const CACHE_ALIGNMENT: i32 = 128;
const REPLAY_WINDOW: Duration = Duration::from_millis(900);
const EXPENSIVE_REPLAY: Duration = Duration::from_micros(500);
const CACHE_IDLE_EXPIRY: Duration = Duration::from_secs(20);

use crate::sdl::InkTexture as Texture;

struct Frame<'a> {
    renderer: &'a Renderer,
    color: &'a ColorManager,
    pixels: [i32; 2],
    scale: [f32; 2],
    aa: f32,
    extended_budget: usize,
}

#[derive(PartialEq)]
struct LayerRevision {
    scale: [f32; 2],
    color: u64,
    history: u64,
    white: f32,
}

struct CommittedLayer {
    texture: Texture,
    region: SDL_Rect,
    surface: [i32; 2],
    format: SDL_PixelFormat,
    revision: Option<LayerRevision>,
    count: usize,
    append: u64,
    erase: u64,
}

#[derive(Default)]
struct InkBounds {
    revision: [u64; 2],
    count: usize,
    bounds: Option<Bounds>,
}

impl InkBounds {
    fn get(&mut self, history: &History) -> Option<Bounds> {
        let revision = [history.ink_revision(), history.erase_revision()];
        if self.revision != revision || self.count > history.strokes().len() {
            *self = Self {
                revision,
                ..Self::default()
            };
        }
        if self.count != history.strokes().len() {
            let paths = history.stroke_bounds();
            for (stroke, bounds) in history.strokes()[self.count..]
                .iter()
                .zip(&paths[self.count..])
            {
                if let Some(bounds) = bounds {
                    Bounds::include(
                        &mut self.bounds,
                        bounds.path.padded(stroke.max_radius() * 2.),
                    );
                }
            }
            self.count = history.strokes().len();
        }
        self.bounds
    }
}

#[derive(Clone, Copy)]
struct InkClip {
    bounds: Bounds,
    origin: [i32; 2],
}

impl InkClip {
    fn new(region: SDL_Rect, target: SDL_Rect, scale: [f32; 2]) -> Self {
        Self {
            bounds: Bounds {
                min: [region.x as f32 / scale[0], region.y as f32 / scale[1]],
                max: [
                    (region.x + region.w) as f32 / scale[0],
                    (region.y + region.h) as f32 / scale[1],
                ],
            },
            origin: [target.x, target.y],
        }
    }

    fn intersects(self, bounds: Bounds) -> bool {
        self.bounds.intersects(bounds)
    }
}

#[derive(Default)]
struct ReplayActivity {
    previous: [Option<Instant>; 2],
    last_use: Option<Instant>,
}

impl ReplayActivity {
    fn observe(&mut self, now: Instant, cost: Duration) {
        if cost < EXPENSIVE_REPLAY {
            *self = Self::default();
            return;
        }
        let [first, second] = self.previous;
        self.previous = [second, Some(now)];
        if first.is_some_and(|first| now.duration_since(first) <= REPLAY_WINDOW) {
            self.last_use = Some(now);
        }
    }
}

#[derive(Default)]
pub(super) struct Ink {
    layer: Option<CommittedLayer>,
    unavailable_at: Option<([i32; 2], SDL_PixelFormat)>,
    painter: StrokePainter,
    activity: ReplayActivity,
    bounds: InkBounds,
}

impl Ink {
    pub fn invalidate_layer(&mut self) {
        self.layer = None;
        self.unavailable_at = None;
        self.activity = ReplayActivity::default();
        self.bounds = InkBounds::default();
    }

    pub fn release_idle_layer(&mut self, now: Instant) {
        if self
            .activity
            .last_use
            .is_some_and(|last| now.duration_since(last) >= CACHE_IDLE_EXPIRY)
        {
            self.invalidate_layer();
        }
    }

    pub fn reset_textures(&mut self) {
        // A render-target reset loses contents, not ownership of SDL_Texture.
        // Destroy each live object before rebuilding its contents.
        self.invalidate_layer();
        self.painter.paper = None;
        self.painter.marker_mask = None;
        self.painter.paper_unavailable = false;
        self.painter.marker_unavailable = false;
    }

    pub fn extended_bytes(&self) -> usize {
        self.layer.as_ref().map_or(0, |layer| {
            if layer.format == SDL_PIXELFORMAT_RGBA64_FLOAT {
                layer_bytes([layer.region.w, layer.region.h], layer.format)
            } else {
                0
            }
        })
    }

    pub fn paint(
        &mut self,
        app: &mut App,
        renderer: &Renderer,
        window: &Window,
        color: &ColorManager,
        background: &mut Background,
        extended_budget: usize,
    ) -> Result<[f32; 2], String> {
        let bg = match (app.board, app.pass) {
            (Some(super::Board::White), false) => [0.97, 0.97, 0.96, 1.],
            (Some(super::Board::Black), false) => [0., 0., 0., 1.],
            (_, pass) => [0., 0., 0., if pass { 0. } else { 1. / 255. }],
        };
        let scale = renderer.begin(window, bg)?;
        let frame = Frame {
            renderer,
            color,
            pixels: window.pixel_size(),
            scale,
            aa: 0.8 / scale[0].max(scale[1]),
            extended_budget,
        };
        self.painter.queued = 0;
        if background.frozen
            && !app.pass
            && let Err(error) = background.draw(renderer, window, color)
        {
            background.live();
            app.status = text("Frozen desktop unavailable").detail(error);
        }

        self.committed(&frame, app)?;
        if let Some(stroke) = &app.history.active {
            self.painter.stroke(&frame, stroke)?;
        }
        for finger in app.input.touches.values() {
            for stroke in finger.fragments.iter().chain(finger.stroke.iter()) {
                self.painter.stroke(&frame, stroke)?;
            }
        }
        if !app.has_contacts() && self.painter.samples.capacity() > 128_000 {
            self.painter.samples = Vec::new();
        }
        Ok(scale)
    }

    fn paint_laser(&mut self, frame: &Frame<'_>, app: &mut App) -> Result<(), String> {
        let now = Instant::now();
        app.input.laser.retain(|_, trail| {
            while trail
                .front()
                .is_some_and(|sample| now.duration_since(sample.at) >= LASER_LIFETIME)
            {
                trail.pop_front();
            }
            !trail.is_empty()
        });
        let mut laser = Stroke {
            tool: Tool::Laser,
            color: InkColor::from_rgb8([246, 72, 99]),
            width: 4. * app.input.canvas_units,
            opacity: 1.,
            pressure_curve: 1.,
            pressure_enabled: false,
            points: Vec::new(),
        };
        for trail in app.input.laser.values() {
            laser.points.clear();
            for sample in trail {
                if sample.starts_segment && !laser.points.is_empty() {
                    self.painter.stroke(frame, &laser)?;
                    laser.points.clear();
                }
                laser.points.push(Point {
                    pressure: 1.
                        - now.duration_since(sample.at).as_secs_f32()
                            / LASER_LIFETIME.as_secs_f32(),
                    ..sample.point
                });
            }
            self.painter.stroke(frame, &laser)?;
        }
        Ok(())
    }

    pub fn present(
        &mut self,
        app: &mut App,
        renderer: &Renderer,
        window: &Window,
        color: &ColorManager,
        scale: [f32; 2],
    ) -> Result<(), String> {
        if !app.input.laser.is_empty() {
            let frame = Frame {
                renderer,
                color,
                pixels: window.pixel_size(),
                scale,
                aa: 0.8 / scale[0].max(scale[1]),
                extended_budget: 0,
            };
            self.paint_laser(&frame, app)?;
        }
        if let Some(hole) = app.input.protected {
            clear_hole(renderer, hole)?;
        }
        renderer.present()?;
        // Keep the final eraser frame until SDL has submitted its composition.
        self.release_unused_layer(app);
        Ok(())
    }

    fn release_unused_layer(&mut self, app: &App) {
        if self.activity.last_use.is_none()
            && self
                .layer
                .as_ref()
                .is_some_and(|layer| uses_adaptive_cache(layer.surface, layer.format))
            && !document_needs_layer(app)
            && !app.input.pending_erasures.values().any(Erasure::has_edits)
        {
            self.invalidate_layer();
        }
    }

    fn committed(&mut self, frame: &Frame<'_>, app: &mut App) -> Result<(), String> {
        let damage = app.input.ink_damage.take();
        let count = app.history.strokes().len();
        if count == 0 {
            self.invalidate_layer();
            return Ok(());
        }
        let renderer = frame.renderer;
        let format = if renderer.extended {
            SDL_PIXELFORMAT_RGBA64_FLOAT
        } else {
            SDL_PIXELFORMAT_RGBA32
        };
        if renderer.extended && renderer.name != "metal" {
            self.invalidate_layer();
            return self.painter.history(frame, app, 0, None, 1.);
        }
        if self
            .layer
            .as_ref()
            .is_some_and(|layer| layer.surface != frame.pixels || layer.format != format)
        {
            self.invalidate_layer();
        }
        if uses_adaptive_cache(frame.pixels, format) && !document_needs_layer(app) {
            if let Some(last_use) = &mut self.activity.last_use {
                // A paint during a long drawing gesture is activity too.
                *last_use = Instant::now();
            } else if self.layer.is_none() {
                let started = (!app.input.has_contacts() && app.input.pending_erasures.is_empty())
                    .then(Instant::now);
                self.painter.history(frame, app, 0, None, 1.)?;
                if let Some(started) = started {
                    let now = Instant::now();
                    self.activity.observe(now, now.duration_since(started));
                }
                return Ok(());
            }
        } else {
            // A busy document keeps its layer independent of hover activity.
            self.activity = ReplayActivity::default();
        }
        let Some(region) = self
            .bounds
            .get(&app.history)
            .and_then(|bounds| cache_region(frame, bounds))
        else {
            self.invalidate_layer();
            return Ok(());
        };
        let size = [region.w, region.h];
        let target = (size, format);
        if renderer.extended && layer_bytes(size, format) > frame.extended_budget {
            self.layer = None;
            self.activity = ReplayActivity::default();
            return self.painter.history(frame, app, 0, None, 1.);
        }
        if self
            .layer
            .as_ref()
            .is_some_and(|layer| layer.region != region)
        {
            self.layer = None;
        }
        if self.unavailable_at == Some(target) {
            return self.painter.history(frame, app, 0, None, 1.);
        }
        if self.layer.is_none() {
            match Texture::with_format(
                renderer,
                size,
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_BLEND_PREMULTIPLIED,
                format,
            ) {
                Ok(texture) => {
                    self.layer = Some(CommittedLayer {
                        texture,
                        region,
                        surface: frame.pixels,
                        format,
                        revision: None,
                        count: 0,
                        append: 0,
                        erase: 0,
                    });
                }
                Err(_) => {
                    self.activity = ReplayActivity::default();
                    self.unavailable_at = Some(target);
                }
            }
        }
        if let Some(layer) = &mut self.layer {
            let revision = LayerRevision {
                scale: frame.scale,
                color: frame.color.revision,
                history: app.history.ink_revision(),
                white: if renderer.extended {
                    renderer.sdr_white_point().max(f32::MIN_POSITIVE)
                } else {
                    0.
                },
            };
            let erase = app.history.erase_revision();
            let append = app.history.append_revision();
            let replaced = layer.erase != erase;
            let rebuild = layer.revision.as_ref() != Some(&revision)
                || replaced && layer.append != append
                || !replaced && layer.count > count;
            let first = if replaced { count } else { layer.count };
            layer.count = count;
            layer.append = append;
            layer.erase = erase;
            if rebuild || damage.is_some() || first < count {
                let result = renderer.with_target(&layer.texture, || {
                    self.painter.queued = 0;
                    // Match the SDR white point used when drawing directly to
                    // the window. A highlighter builds its mask at scale 1.
                    let white = if renderer.extended {
                        revision.white
                    } else {
                        1.
                    };
                    self.painter.repaint_layer(
                        frame,
                        app,
                        region,
                        (!rebuild).then_some(first),
                        damage,
                        white,
                    )
                });
                self.painter.queued = 0;
                if result.is_err() {
                    self.invalidate_layer();
                    self.unavailable_at = Some(target);
                    return self.painter.history(frame, app, 0, None, 1.);
                }
                layer.revision = Some(revision);
            }

            // The history was painted with source-over into a transparent
            // texture, so its stored RGB is already premultiplied by alpha.
            let destination = SDL_FRect {
                x: region.x as f32,
                y: region.y as f32,
                w: region.w as f32,
                h: region.h as f32,
            };
            return renderer.with_pixel_scale(frame.scale, || {
                renderer.with_color_scale(revision_white_scale(layer), || {
                    layer.texture.draw(renderer, None, Some(&destination))
                })
            });
        }
        self.painter.history(frame, app, 0, None, 1.)
    }
}

fn layer_bytes([width, height]: [i32; 2], format: SDL_PixelFormat) -> usize {
    let bytes_per_pixel = if format == SDL_PIXELFORMAT_RGBA64_FLOAT {
        8
    } else {
        4
    };
    (width.max(0) as usize)
        .saturating_mul(height.max(0) as usize)
        .saturating_mul(bytes_per_pixel)
}

fn uses_adaptive_cache(pixels: [i32; 2], format: SDL_PixelFormat) -> bool {
    format == SDL_PIXELFORMAT_RGBA64_FLOAT || layer_bytes(pixels, format) > MAX_EAGER_SDR_BYTES
}

fn document_needs_layer(app: &App) -> bool {
    app.history.strokes().len() >= CACHE_STROKES || app.history.point_count() >= CACHE_POINTS
}

fn cache_region(frame: &Frame<'_>, bounds: Bounds) -> Option<SDL_Rect> {
    let pixels = dirty_rect(frame, bounds)?;
    let left = pixels.x / CACHE_ALIGNMENT * CACHE_ALIGNMENT;
    let top = pixels.y / CACHE_ALIGNMENT * CACHE_ALIGNMENT;
    let align_end = |end: i32, limit: i32| {
        ((i64::from(end) + i64::from(CACHE_ALIGNMENT) - 1) / i64::from(CACHE_ALIGNMENT)
            * i64::from(CACHE_ALIGNMENT))
        .min(i64::from(limit)) as i32
    };
    Some(SDL_Rect {
        x: left,
        y: top,
        w: align_end(pixels.x + pixels.w, frame.pixels[0]) - left,
        h: align_end(pixels.y + pixels.h, frame.pixels[1]) - top,
    })
}

fn revision_white_scale(layer: &CommittedLayer) -> f32 {
    let white = layer
        .revision
        .as_ref()
        .map_or(0., |revision| revision.white);
    if white > 0. { white.recip() } else { 1. }
}

fn dirty_rect(frame: &Frame<'_>, bounds: Bounds) -> Option<SDL_Rect> {
    let bounds = bounds.padded(frame.aa + 2. / frame.scale[0].min(frame.scale[1]));
    let min: [i32; 2] = std::array::from_fn(|i| {
        ((bounds.min[i] * frame.scale[i]).floor() as i32).clamp(0, frame.pixels[i])
    });
    let max: [i32; 2] = std::array::from_fn(|i| {
        ((bounds.max[i] * frame.scale[i]).ceil() as i32).clamp(0, frame.pixels[i])
    });
    (min[0] < max[0] && min[1] < max[1]).then_some(SDL_Rect {
        x: min[0],
        y: min[1],
        w: max[0] - min[0],
        h: max[1] - min[1],
    })
}

fn clear_hole(renderer: &Renderer, rect: Rect) -> Result<(), String> {
    clear_rect(
        renderer,
        SDL_FRect {
            x: rect.x as f32,
            y: rect.y as f32,
            w: rect.w as f32,
            h: rect.h as f32,
        },
    )
}

fn clear_rect(renderer: &Renderer, rect: SDL_FRect) -> Result<(), String> {
    renderer.clear_rect_transparent(&rect)
}

#[derive(Default)]
struct StrokePainter {
    mesh: Mesh,
    samples: Vec<Point>,
    vertices: Vec<SDL_Vertex>,
    queued: usize,
    paper: Option<Texture>,
    paper_unavailable: bool,
    marker_mask: Option<Texture>,
    marker_unavailable: bool,
    #[cfg(test)]
    submitted_indices: usize,
}

impl StrokePainter {
    fn repaint_layer(
        &mut self,
        frame: &Frame<'_>,
        app: &App,
        target: SDL_Rect,
        first: Option<usize>,
        damage: Option<Bounds>,
        white: f32,
    ) -> Result<(), String> {
        let renderer = frame.renderer;
        renderer.set_render_scale([1.; 2])?;
        let clip = Some(InkClip::new(target, target, frame.scale));
        let Some(first) = first else {
            renderer.clear([0.; 4])?;
            return self.history(frame, app, 0, clip, white);
        };
        if first < app.history.strokes().len() {
            self.history(frame, app, first, clip, white)?;
        }
        if let Some(region) = damage.and_then(|bounds| dirty_rect(frame, bounds)) {
            let left = region.x.max(target.x);
            let top = region.y.max(target.y);
            let width = (region.x + region.w).min(target.x + target.w) - left;
            let height = (region.y + region.h).min(target.y + target.h) - top;
            if width <= 0 || height <= 0 {
                return Ok(());
            }
            let local = SDL_Rect {
                x: left - target.x,
                y: top - target.y,
                w: width,
                h: height,
            };
            renderer.try_set_clip_rect(Some(&local))?;
            let rect = SDL_FRect {
                x: local.x as f32,
                y: local.y as f32,
                w: local.w as f32,
                h: local.h as f32,
            };
            let clip = InkClip::new(
                SDL_Rect {
                    x: left,
                    y: top,
                    w: width,
                    h: height,
                },
                target,
                frame.scale,
            );
            let result = clear_rect(renderer, rect)
                .and_then(|()| self.history(frame, app, 0, Some(clip), white));
            let reset_clip = renderer.try_set_clip_rect(None);
            result.and(reset_clip)?;
        }
        Ok(())
    }

    fn history(
        &mut self,
        frame: &Frame<'_>,
        app: &App,
        first: usize,
        region: Option<InkClip>,
        white: f32,
    ) -> Result<(), String> {
        let bounds = region.map(|_| app.history.stroke_bounds());
        for (index, stroke) in app.history.strokes().iter().enumerate().skip(first) {
            if let Some(region) = region
                && bounds
                    .as_ref()
                    .and_then(|bounds| bounds.get(index))
                    .is_some_and(|entry| {
                        entry.as_ref().is_none_or(|bounds| {
                            !region
                                .intersects(bounds.path.padded(stroke.max_radius() * 2. + frame.aa))
                        })
                    })
            {
                continue;
            }
            let mut result = Ok(());
            if !Erasure::preview(
                stroke,
                index,
                app.input.pending_erasures.values(),
                &app.history,
                |style, points, bounds| {
                    if result.is_ok()
                        && region.is_none_or(|region| {
                            region
                                .intersects(bounds.path.padded(style.max_radius() * 2. + frame.aa))
                        })
                    {
                        result = self.path(frame, style, points, region, white);
                    }
                },
            ) {
                self.path(
                    frame,
                    stroke,
                    PointSpan::new(&stroke.polyline()),
                    region,
                    white,
                )?;
            }
            result?;
        }
        Ok(())
    }

    fn stroke(&mut self, frame: &Frame<'_>, stroke: &Stroke) -> Result<(), String> {
        self.path(frame, stroke, PointSpan::new(&stroke.polyline()), None, 1.)
    }

    fn path(
        &mut self,
        frame: &Frame<'_>,
        stroke: &Stroke,
        points: PointSpan<'_>,
        region: Option<InkClip>,
        white: f32,
    ) -> Result<(), String> {
        if points.is_empty() || stroke.opacity <= 0. {
            return Ok(());
        }
        let renderer = frame.renderer;
        let use_paper = stroke.tool == Tool::Pencil && self.ensure_paper_texture(renderer);
        let vertex_grain = !use_paper;
        geometry::prepare_samples(stroke, points, vertex_grain, &mut self.samples);
        let rgb = frame.color.map(stroke.color.output_rgb(renderer.extended));
        if stroke.tool == Tool::Marker && (!self.marker_unavailable || self.marker_mask.is_some()) {
            let Some(rect) = marker_rect(frame, &self.samples, stroke.width + frame.aa) else {
                return Ok(());
            };
            if self.ensure_marker_texture(renderer, [rect.w, rect.h]) {
                return self.marker(frame, rgb, stroke, region, white, rect);
            }
        }
        renderer.with_color_scale(white, || {
            let margin = stroke.max_radius() * 2. + frame.aa;
            let visible = |a, b| {
                stroke.tool == Tool::Arrow
                    || region.is_none_or(|region| {
                        region.intersects(Bounds::between(a, b).padded(margin))
                    })
            };
            let scale = if region.is_some() {
                frame.scale
            } else {
                [1.; 2]
            };
            for range in geometry::mesh_ranges(&self.samples, MESH_SEGMENTS, visible) {
                self.mesh
                    .update(stroke, &self.samples, range, frame.aa, vertex_grain);
                set_vertices(
                    &self.mesh,
                    &mut self.vertices,
                    rgb,
                    stroke.opacity,
                    scale,
                    region.map_or([0; 2], |region| region.origin),
                    !vertex_grain,
                );
                #[cfg(test)]
                {
                    self.submitted_indices += self.mesh.indices.len();
                }
                submit(
                    renderer,
                    if use_paper { self.paper.as_ref() } else { None },
                    &self.vertices,
                    &self.mesh.indices,
                    &mut self.queued,
                )?;
            }
            Ok(())
        })
    }

    fn ensure_paper_texture(&mut self, renderer: &Renderer) -> bool {
        if self.paper.is_none() && !self.paper_unavailable && renderer.name != "software" {
            let create = || -> Result<Texture, String> {
                let texture = Texture::new(
                    renderer,
                    [PAPER_SIZE; 2],
                    SDL_TEXTUREACCESS_STATIC,
                    SDL_BLENDMODE_BLEND,
                )?;
                let mut pixels = Vec::with_capacity((PAPER_SIZE * PAPER_SIZE * 4) as usize);
                for y in 0..PAPER_SIZE {
                    for x in 0..PAPER_SIZE {
                        pixels.extend([
                            255,
                            255,
                            255,
                            (255. * geometry::paper_density(x, y)).round() as u8,
                        ]);
                    }
                }
                texture.update_rgba8(&pixels)?;
                Ok(texture)
            };
            self.paper = create().ok();
            self.paper_unavailable = self.paper.is_none();
        }
        self.paper.is_some()
    }

    fn ensure_marker_texture(&mut self, renderer: &Renderer, required: [i32; 2]) -> bool {
        let required =
            required.map(|size| (size.min(MARKER_TILE) as u32).next_power_of_two() as i32);
        let current = self
            .marker_mask
            .as_ref()
            .map_or([0; 2], Texture::dimensions);
        if !self.marker_unavailable && (current[0] < required[0] || current[1] < required[1]) {
            let size = std::array::from_fn(|axis| current[axis].max(required[axis]));
            match Texture::new(
                renderer,
                size,
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_BLEND,
            ) {
                Ok(mask) => self.marker_mask = Some(mask),
                Err(_) => self.marker_unavailable = true,
            }
        }
        self.marker_mask.is_some()
    }

    fn marker(
        &mut self,
        frame: &Frame<'_>,
        rgb: [f32; 3],
        stroke: &Stroke,
        region: Option<InkClip>,
        white: f32,
        rect: SDL_Rect,
    ) -> Result<(), String> {
        let renderer = frame.renderer;
        let mask = self.marker_mask.as_ref().ok_or("Missing marker mask")?;
        let [tile_width, tile_height] = mask.dimensions();
        let [left, top, right, bottom] = [rect.x, rect.y, rect.x + rect.w, rect.y + rect.h];
        let margin = stroke.width + frame.aa;
        mask.set_modulation(rgb, stroke.opacity)?;
        let origin = region.map_or([0; 2], |region| region.origin);
        let scale = if region.is_some() {
            [1.; 2]
        } else {
            frame.scale
        };
        renderer.with_pixel_scale(scale, || {
            for y in (top..bottom).step_by(tile_height as usize) {
                for x in (left..right).step_by(tile_width as usize) {
                    let width = (right - x).min(tile_width) as f32;
                    let height = (bottom - y).min(tile_height) as f32;
                    let tile = Bounds {
                        min: [x as f32 / frame.scale[0], y as f32 / frame.scale[1]],
                        max: [
                            (x as f32 + width) / frame.scale[0],
                            (y as f32 + height) / frame.scale[1],
                        ],
                    };
                    if region.is_some_and(|region| !region.intersects(tile)) {
                        continue;
                    }
                    // The mask accumulates coverage at full opacity. Tint it and
                    // apply the marker's opacity only once, after intersections.
                    self.queued = 0;
                    renderer.with_target(mask, || {
                        renderer.set_render_scale([1., 1.])?;
                        renderer.clear([1., 1., 1., 0.])?;
                        let visible = |a, b| {
                            // Include the ribbon's miter and antialias coverage.
                            // Only tessellate segments that can reach this tile.
                            let bounds = Bounds::between(a, b).padded(margin);
                            tile.intersects(bounds)
                                && region.is_none_or(|region| region.intersects(bounds))
                        };
                        for range in geometry::mesh_ranges(&self.samples, MESH_SEGMENTS, visible) {
                            self.mesh
                                .update(stroke, &self.samples, range, frame.aa, false);
                            set_vertices(
                                &self.mesh,
                                &mut self.vertices,
                                [1.; 3],
                                1.,
                                frame.scale,
                                [x, y],
                                false,
                            );
                            #[cfg(test)]
                            {
                                self.submitted_indices += self.mesh.indices.len();
                            }
                            submit(
                                renderer,
                                None,
                                &self.vertices,
                                &self.mesh.indices,
                                &mut self.queued,
                            )?;
                        }
                        Ok(())
                    })?;
                    self.queued = 0;
                    let src = SDL_FRect {
                        x: 0.,
                        y: 0.,
                        w: width,
                        h: height,
                    };
                    let dst = SDL_FRect {
                        x: (x - origin[0]) as f32,
                        y: (y - origin[1]) as f32,
                        w: width,
                        h: height,
                    };
                    renderer
                        .with_color_scale(white, || mask.draw(renderer, Some(&src), Some(&dst)))?;
                }
            }
            Ok(())
        })
    }
}

fn marker_rect(frame: &Frame<'_>, samples: &[Point], margin: f32) -> Option<SDL_Rect> {
    let bounds = Bounds::points(samples.iter().copied())?.padded(margin);
    let [left, top] = std::array::from_fn(|i| {
        ((bounds.min[i] * frame.scale[i]).floor() as i32).clamp(0, frame.pixels[i])
    });
    let [right, bottom] = std::array::from_fn(|i| {
        ((bounds.max[i] * frame.scale[i]).ceil() as i32).clamp(0, frame.pixels[i])
    });
    (left < right && top < bottom).then_some(SDL_Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    })
}

fn set_vertices(
    mesh: &Mesh,
    vertices: &mut Vec<SDL_Vertex>,
    rgb: [f32; 3],
    opacity: f32,
    scale: [f32; 2],
    origin: [i32; 2],
    paper: bool,
) {
    vertices.clear();
    vertices.extend(mesh.vertices.iter().map(|v| {
        let [x, y] = v.xy;
        let uv = if paper {
            [x / PAPER_SIZE as f32, y / PAPER_SIZE as f32]
        } else {
            [0.; 2]
        };
        SDL_Vertex {
            position: SDL_FPoint {
                x: x * scale[0] - origin[0] as f32,
                y: y * scale[1] - origin[1] as f32,
            },
            color: SDL_FColor {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
                a: opacity * v.coverage,
            },
            tex_coord: SDL_FPoint { x: uv[0], y: uv[1] },
        }
    }));
}

fn submit(
    renderer: &Renderer,
    texture: Option<&Texture>,
    vertices: &[SDL_Vertex],
    indices: &[u32],
    queued: &mut usize,
) -> Result<(), String> {
    for batch in indices.chunks(GEOMETRY_BATCH) {
        if *queued + batch.len() > GEOMETRY_BATCH {
            renderer.flush()?;
            *queued = 0;
        }
        renderer.geometry(
            texture.map_or(GeometryTexture::Solid, GeometryTexture::Ink),
            vertices,
            batch,
        )?;
        *queued += batch.len();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::touch_tests::app;

    type TestResult = Result<(), String>;

    struct TestCanvas {
        renderer: TestRenderer,
        pixels: [i32; 2],
        color: ColorManager,
    }
    impl TestCanvas {
        fn new([width, height]: [i32; 2]) -> Result<Self, String> {
            let surface = TestSurface::new([width, height])?;
            let pixels = surface.size();
            let renderer = surface.software_renderer()?;
            Ok(Self {
                renderer,
                pixels,
                color: ColorManager::default(),
            })
        }
        fn frame(&self) -> Frame<'_> {
            Frame {
                renderer: &self.renderer,
                color: &self.color,
                pixels: self.pixels,
                scale: [1.; 2],
                aa: 0.8,
                extended_budget: MAX_EXTENDED_INK_TEXTURE_BYTES,
            }
        }
        fn pixel(&self, x: i32, y: i32) -> Result<[u8; 4], String> {
            TestSurface::pixel(&self.renderer, x, y)
        }
        fn pixels(&self) -> Result<Vec<[u8; 4]>, String> {
            TestSurface::pixels(&self.renderer)
        }
        fn history(&self, ink: &mut Ink, app: &mut App) -> TestResult {
            self.renderer.clear([0.; 4])?;
            ink.committed(&self.frame(), app)
        }
    }

    fn commit(app: &mut App, tool: Tool, rgb: [u8; 3], points: &[[f32; 2]]) {
        app.history.active = Some(Stroke {
            tool,
            color: InkColor::from_rgb8(rgb),
            width: 10.,
            opacity: 0.5,
            pressure_curve: 1.,
            pressure_enabled: false,
            points: points
                .iter()
                .map(|&[x, y]| Point::new(x, y, 1., [0.; 2]))
                .collect(),
        });
        app.history.commit();
    }

    #[test]
    fn cached_ink_keeps_alpha_and_tracks_history_and_eraser_edits() -> TestResult {
        let canvas = TestCanvas::new([128, 96])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(&mut app, Tool::Pen, [255, 0, 0], &[[10., 20.], [100., 20.]]);
        canvas.history(&mut ink, &mut app)?;
        let red = canvas.pixel(50, 20)?;
        assert!((125..=129).contains(&red[0]), "{red:?}");
        assert_eq!(red[0], red[3]);
        assert_eq!(ink.layer.as_ref().map(|layer| layer.count), Some(1));

        ink.painter.vertices.clear();
        canvas.history(&mut ink, &mut app)?;
        assert!(ink.painter.vertices.is_empty());
        assert_eq!(canvas.pixel(50, 20)?, red);

        commit(&mut app, Tool::Pen, [0, 255, 0], &[[10., 70.], [100., 70.]]);
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(ink.layer.as_ref().map(|layer| layer.count), Some(2));
        assert_eq!(canvas.pixel(50, 20)?, red);
        let green = canvas.pixel(50, 70)?;
        assert!(green[1] > 120);
        app.history.undo();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(50, 70)?, [0; 4]);
        app.history.redo();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(50, 70)?, green);

        app.erase_sample(1, Point::new(50., 20., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(50, 20)?, [0; 4]);
        app.reset_contacts();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(50, 20)?, red);

        let outside_hole = canvas.pixel(60, 20)?;
        clear_hole(
            &canvas.renderer,
            Rect {
                x: 47,
                y: 16,
                w: 8,
                h: 8,
            },
        )?;
        assert_eq!(canvas.pixel(50, 20)?, [0; 4]);
        assert_eq!(canvas.pixel(60, 20)?, outside_hole);
        app.history.clear();
        canvas.history(&mut ink, &mut app)?;
        assert!(ink.layer.is_none());
        Ok(())
    }

    #[test]
    fn small_marker_mask_grows_with_its_pixel_bounds() -> TestResult {
        let canvas = TestCanvas::new([1280, 160])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(
            &mut app,
            Tool::Marker,
            [255, 200, 0],
            &[[24., 35.], [110., 35.]],
        );
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(
            ink.painter.marker_mask.as_ref().map(Texture::dimensions),
            Some([128, 32])
        );
        let small = canvas.pixel(64, 35)?;
        assert!(small[3] > 0);

        commit(
            &mut app,
            Tool::Marker,
            [255, 200, 0],
            &[[16., 80.], [1250., 80.]],
        );
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(
            ink.painter.marker_mask.as_ref().map(Texture::dimensions),
            Some([1024, 32])
        );
        assert_eq!(canvas.pixel(64, 35)?, small);
        assert_eq!(canvas.pixel(1026, 80)?, canvas.pixel(1080, 80)?);
        Ok(())
    }

    #[test]
    fn marker_crossing_has_the_same_opacity_as_the_rest_of_the_stroke() -> TestResult {
        let canvas = TestCanvas::new([1280, 96])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(
            &mut app,
            Tool::Marker,
            [255, 200, 0],
            &[
                [10., 25.],
                [1240., 25.],
                [1240., 75.],
                [45., 75.],
                [45., 10.],
            ],
        );
        canvas.history(&mut ink, &mut app)?;
        assert!(ink.painter.marker_mask.is_some());
        let straight = canvas.pixel(65, 25)?;
        assert!((125..=129).contains(&straight[3]), "{straight:?}");
        assert_eq!(canvas.pixel(45, 25)?, straight);
        assert_eq!(canvas.pixel(MARKER_TILE - 1, 25)?, straight);
        assert_eq!(canvas.pixel(MARKER_TILE, 25)?, straight);
        Ok(())
    }

    #[test]
    fn marker_split_updates_a_crossing_away_from_the_cut() -> TestResult {
        let canvas = TestCanvas::new([600, 160])?;
        let full = TestCanvas::new([600, 160])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(
            &mut app,
            Tool::Marker,
            [255, 200, 0],
            &[
                [20., 60.],
                [180., 60.],
                [300., 60.],
                [380., 60.],
                [420., 60.],
                [460., 60.],
                [540., 60.],
                [540., 130.],
                [100., 130.],
                [100., 20.],
            ],
        );
        let crossing = |canvas: &TestCanvas| canvas.pixel(100, 60);
        canvas.history(&mut ink, &mut app)?;
        let before = crossing(&canvas)?;
        assert_eq!(before, canvas.pixel(160, 60)?);

        app.erase_sample(1, Point::new(420., 60., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app)?;
        full.history(&mut Ink::default(), &mut app)?;
        let split = crossing(&full)?;
        assert!(split[3] > before[3]);
        assert_eq!(crossing(&canvas)?, split);

        app.reset_contacts();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(crossing(&canvas)?, before);

        app.erase_sample(2, Point::new(420., 60., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app)?;
        app.commit_erase(2);
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(app.history.strokes().len(), 2);
        assert_eq!(crossing(&canvas)?, split);
        app.history.undo();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(crossing(&canvas)?, before);
        app.history.redo();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(crossing(&canvas)?, split);
        Ok(())
    }

    #[test]
    fn a_single_stroke_and_local_erase_on_4k_do_not_allocate_a_layer() -> TestResult {
        let canvas = TestCanvas::new([3840, 2160])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(&mut app, Tool::Pen, [255, 0, 0], &[[10., 20.], [100., 20.]]);
        canvas.history(&mut ink, &mut app)?;
        assert!(canvas.pixel(50, 20)?[0] > 0);
        assert!(ink.layer.is_none());
        app.erase_sample(1, Point::new(50., 20., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(50, 20)?[3], 0);
        assert!(ink.layer.is_none());
        app.reset_contacts();
        canvas.history(&mut ink, &mut app)?;
        assert!(canvas.pixel(50, 20)?[0] > 0);
        Ok(())
    }

    #[test]
    fn dense_local_ink_on_4k_keeps_a_small_offset_layer() -> TestResult {
        let canvas = TestCanvas::new([3840, 2160])?;
        let mut app = app();
        let mut ink = Ink::default();
        for i in 0..CACHE_STROKES {
            let y = 145. + i as f32;
            commit(
                &mut app,
                Tool::Marker,
                [255, 200, 0],
                &[[530., y], [610., y]],
            );
        }
        canvas.history(&mut ink, &mut app)?;
        let region = ink
            .layer
            .as_ref()
            .ok_or("expected cropped ink layer")?
            .region;
        assert_eq!(
            [region.x, region.y, region.w, region.h],
            [512, 128, 128, 128]
        );
        assert_eq!(
            layer_bytes([region.w, region.h], SDL_PIXELFORMAT_RGBA32),
            64 * 1024
        );
        let drawn = canvas.pixel(570, 172)?;
        assert!(drawn[3] > 0);
        assert_eq!(canvas.pixel(58, 44)?, [0; 4]);
        app.erase_sample(1, Point::new(570., 172., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(570, 172)?, [0; 4]);
        app.commit_erase(1);
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(570, 172)?, [0; 4]);
        app.history.undo();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(570, 172)?, drawn);
        app.history.redo();
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(canvas.pixel(570, 172)?, [0; 4]);
        Ok(())
    }

    #[test]
    fn small_extended_cache_releases_its_layer_after_erase() -> TestResult {
        let canvas = TestCanvas::new([320, 160])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(
            &mut app,
            Tool::Pen,
            [255, 200, 0],
            &[[10., 60.], [300., 60.]],
        );
        assert!(!document_needs_layer(&app));
        for (id, x) in [(1, 100.), (2, 180.)] {
            app.erase_sample(id, Point::new(x, 60., 1., [0.; 2]));
            canvas.history(&mut ink, &mut app)?;
            // Exercise the lifetime policy with a software-backed target.
            let Some(layer) = ink.layer.as_mut() else {
                panic!("expected a cached ink layer");
            };
            layer.format = SDL_PIXELFORMAT_RGBA64_FLOAT;
            ink.release_unused_layer(&app);
            assert_eq!(
                ink.extended_bytes(),
                layer_bytes([320, 128], SDL_PIXELFORMAT_RGBA64_FLOAT)
            );

            if id == 1 {
                app.commit_erase(id);
            } else {
                app.reset_contacts();
            }
            assert!(!document_needs_layer(&app));
            ink.release_unused_layer(&app);
            assert!(ink.layer.is_none());
            assert_eq!(ink.extended_bytes(), 0);
        }

        let stroke = (*app.history.strokes()[0]).clone();
        app.history
            .append(std::iter::repeat_n(stroke, CACHE_STROKES));
        assert!(document_needs_layer(&app));
        Ok(())
    }

    #[test]
    fn three_expensive_replays_must_fit_in_one_window_and_expire_after_last_use() {
        let mut ink = Ink::default();
        let start = Instant::now();
        ink.activity.observe(start, Duration::from_micros(20));
        for ms in [10, 710, 960] {
            ink.activity
                .observe(start + Duration::from_millis(ms), EXPENSIVE_REPLAY);
            assert!(ink.activity.last_use.is_none());
        }
        let last = start + Duration::from_secs(1);
        ink.activity.observe(last, EXPENSIVE_REPLAY);
        assert_eq!(ink.activity.last_use, Some(last));
        ink.release_idle_layer(last + CACHE_IDLE_EXPIRY - Duration::from_millis(1));
        assert_eq!(ink.activity.last_use, Some(last));
        ink.release_idle_layer(last + CACHE_IDLE_EXPIRY);
        assert!(ink.activity.last_use.is_none());
        ink.activity
            .observe(last + CACHE_IDLE_EXPIRY, EXPENSIVE_REPLAY);
        assert!(ink.activity.last_use.is_none());
    }

    #[test]
    fn local_erase_matches_a_full_rebuild_at_fractional_scale() -> TestResult {
        let canvas = TestCanvas::new([840, 240])?;
        let full = TestCanvas::new([840, 240])?;
        let mut app = app();
        let mut ink = Ink::default();
        let mut reference = Ink::default();
        let mut frame = canvas.frame();
        frame.scale = [1.5; 2];
        frame.aa = 0.8 / 1.5;
        let reference_frame = Frame {
            renderer: &full.renderer,
            color: &full.color,
            ..frame
        };
        for renderer in [&canvas.renderer, &full.renderer] {
            renderer.set_render_scale(frame.scale)?;
        }
        for (tool, color, points) in [
            (
                Tool::Pen,
                [255, 0, 0],
                [[8., 70.], [210., 70.], [340., 95.], [550., 95.]],
            ),
            (
                Tool::Marker,
                [10, 220, 0],
                [[10., 72.], [260., 70.], [480., 74.], [500., 90.]],
            ),
            (
                Tool::Pencil,
                [10, 0, 220],
                [[5., 112.], [180., 100.], [300., 70.], [550., 58.]],
            ),
            (
                Tool::Arrow,
                [30, 70, 220],
                [[40., 50.], [100., 70.], [210., 50.], [310., 30.]],
            ),
        ] {
            commit(&mut app, tool, color, &points);
        }
        // Add a distant stroke so the dirty update must keep pixels outside its clip.
        commit(
            &mut app,
            Tool::Pen,
            [0, 0, 255],
            &[[390., 122.], [540., 122.]],
        );
        let mut compare = |app: &mut App| -> TestResult {
            canvas.renderer.clear([0.; 4])?;
            full.renderer.clear([0.; 4])?;
            ink.committed(&frame, app)?;
            reference.invalidate_layer();
            reference.committed(&reference_frame, app)?;
            let actual = canvas.pixels()?;
            let expected = full.pixels()?;
            let difference = actual.iter().zip(&expected).position(|(a, b)| a != b);
            assert!(
                difference.is_none(),
                "{:?}",
                difference.map(|i| (i % 840, i / 840, actual[i], expected[i]))
            );
            Ok(())
        };
        let point = |x, y| Point::new(x, y, 1., [0.; 2]);
        compare(&mut app)?;
        app.erase_sample(1, point(240., 70.));
        compare(&mut app)?;
        app.erase_sample(1, point(260., 78.));
        compare(&mut app)?;
        app.erase_sample(2, point(300., 70.));
        compare(&mut app)?;
        app.commit_erase(2);
        compare(&mut app)?;
        app.reset_contacts();
        compare(&mut app)?;
        app.history.undo();
        compare(&mut app)?;
        app.history.redo();
        compare(&mut app)?;
        Ok(())
    }

    #[test]
    fn local_erase_skips_distant_geometry_on_dense_canvas() -> TestResult {
        let canvas = TestCanvas::new([1024, 768])?;
        let mut app = app();
        let mut ink = Ink::default();
        app.history.append((0..100).map(|row| {
            Stroke {
                tool: Tool::Pen,
                color: InkColor::default(),
                width: 1.,
                opacity: 0.8,
                pressure_curve: 1.,
                pressure_enabled: false,
                points: [0., 1023.]
                    .map(|x| Point::new(x, 10. + row as f32 * 7., 1., [0.; 2]))
                    .to_vec(),
            }
        }));
        // Chop 100 long strokes into 10,000 short pieces in one gesture.
        let points = app.draft_point_count();
        let mut draft = Erasure::default();
        for column in 1..100 {
            let x = column as f32 * 10.;
            for y in [0., 720.] {
                draft.sample(
                    &app.history,
                    points,
                    Erasure::available_budget(std::iter::empty()),
                    Point::new(x, y, 1., [0.; 2]),
                    2.,
                );
            }
            draft.break_path(
                &app.history,
                points,
                Erasure::available_budget(std::iter::empty()),
            );
        }
        app.input.pending_erasures.insert(0, draft);
        app.commit_erase(0);
        assert_eq!(app.history.strokes().len(), 10_000);
        canvas.history(&mut ink, &mut app)?;
        let full_indices = std::mem::take(&mut ink.painter.submitted_indices);
        app.erase_sample(1, Point::new(500., 350., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app)?;
        let preview_indices = std::mem::take(&mut ink.painter.submitted_indices);
        eprintln!("Full canvas: {full_indices} indices. Erase preview: {preview_indices} indices.");
        assert!(preview_indices > 0 && preview_indices < full_indices / 100);
        app.commit_erase(1);
        canvas.history(&mut ink, &mut app)?;
        assert_eq!(ink.painter.submitted_indices, 0);
        Ok(())
    }

    #[test]
    fn offset_ink_layer_matches_direct_rendering_at_fractional_scale() -> TestResult {
        let canvas = TestCanvas::new([640, 256])?;
        let direct = TestCanvas::new([640, 256])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(
            &mut app,
            Tool::Pen,
            [255, 0, 0],
            &[[352., 50.], [352., 150.]],
        );
        let frame = Frame {
            scale: [1.21; 2],
            aa: 0.8 / 1.21,
            ..canvas.frame()
        };
        let reference = Frame {
            renderer: &direct.renderer,
            color: &direct.color,
            ..frame
        };
        for renderer in [&canvas.renderer, &direct.renderer] {
            renderer.set_render_scale(frame.scale)?;
            renderer.clear([0.; 4])?;
        }
        ink.committed(&frame, &mut app)?;
        assert_eq!(ink.layer.as_ref().map(|layer| layer.region.x), Some(384));
        let mut painter = StrokePainter::default();
        painter.history(&reference, &app, 0, None, 1.)?;
        let pixels = canvas.pixels()?;
        let expected = direct.pixels()?;
        let difference = pixels.iter().zip(&expected).position(|(a, b)| a[3] != b[3]);
        assert_eq!(
            difference.map(|i| (i % 640, i / 640, pixels[i], expected[i])),
            None
        );
        Ok(())
    }

    #[test]
    fn committed_layer_maps_logical_coordinates_at_high_pixel_density() -> TestResult {
        let canvas = TestCanvas::new([128, 96])?;
        let mut app = app();
        let mut ink = Ink::default();
        commit(&mut app, Tool::Pen, [255, 0, 0], &[[30., 16.], [58., 16.]]);
        let mut frame = canvas.frame();
        frame.scale = [2.; 2];
        frame.aa = 0.4;
        canvas.renderer.set_render_scale([2., 2.])?;
        canvas.renderer.clear([0.; 4])?;
        ink.committed(&frame, &mut app)?;
        assert!(canvas.pixel(100, 32)?[3] > 120);
        assert_eq!(canvas.pixel(35, 16)?, [0; 4]);
        Ok(())
    }
}
