use super::{App, LASER_LIFETIME};
use crate::{
    appearance::text,
    background::Background,
    color::InkColor,
    controller::Rect,
    geometry::{self, Mesh},
    icc::ColorManager,
    model::{Bounds, Erasure, Point, PointSpan, Stroke, Tool},
    sdl::*,
};
use std::{ptr, time::Instant};

const PAPER_SIZE: i32 = 64;
const MARKER_TILE: i32 = 1024;
const MESH_SEGMENTS: usize = 1024;
// SDL expands indexed geometry before upload. Bound each upload, including
// those for a large document rebuild or one unusually long stroke.
const GEOMETRY_BATCH: usize = 24_576;
// Extra FP16 ink targets share this allowance across displays.
pub(super) const MAX_EXTENDED_INK_TEXTURE_BYTES: usize = 64 * 1024 * 1024;
const EXTENDED_CACHE_STROKES: usize = 64;
const EXTENDED_CACHE_POINTS: usize = 8192;

struct Texture(*mut SDL_Texture);
impl Texture {
    fn new(
        renderer: &Renderer,
        [width, height]: [i32; 2],
        access: SDL_TextureAccess,
        blend: SDL_BlendMode,
    ) -> Result<Self, String> {
        Self::with_format(
            renderer,
            [width, height],
            access,
            blend,
            SDL_PIXELFORMAT_RGBA32,
        )
    }

    fn with_format(
        renderer: &Renderer,
        [width, height]: [i32; 2],
        access: SDL_TextureAccess,
        blend: SDL_BlendMode,
        format: SDL_PixelFormat,
    ) -> Result<Self, String> {
        unsafe {
            let raw = SDL_CreateTexture(renderer.raw, format, access, width, height);
            if raw.is_null() {
                return Err(error());
            }
            let texture = Self(raw);
            check(SDL_SetTextureBlendMode(raw, blend))?;
            check(SDL_SetTextureScaleMode(raw, SDL_SCALEMODE_NEAREST))?;
            Ok(texture)
        }
    }
}
impl Drop for Texture {
    fn drop(&mut self) {
        unsafe { SDL_DestroyTexture(self.0) };
    }
}

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
    size: [i32; 2],
    format: SDL_PixelFormat,
    revision: Option<LayerRevision>,
    count: usize,
    append: u64,
    erase: u64,
}

#[derive(Default)]
pub(super) struct Ink {
    layer: Option<CommittedLayer>,
    unavailable_at: Option<([i32; 2], SDL_PixelFormat)>,
    painter: StrokePainter,
}

impl Ink {
    pub fn invalidate_layer(&mut self) {
        self.layer = None;
        self.unavailable_at = None;
    }

    pub fn reset_textures(&mut self) {
        self.invalidate_layer();
        self.painter.paper = None;
        self.painter.marker_mask = None;
        self.painter.paper_unavailable = false;
        self.painter.marker_unavailable = false;
    }

    pub fn extended_bytes(&self) -> usize {
        self.layer.as_ref().map_or(0, |layer| {
            if layer.format == SDL_PIXELFORMAT_RGBA64_FLOAT {
                extended_bytes(layer.size)
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
    ) -> Result<(), String> {
        let bg = if app.board && !app.pass {
            [0.97, 0.97, 0.96, 1.]
        } else {
            [0., 0., 0., if app.pass { 0. } else { 1. / 255. }]
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
            app.status =
                text("Frozen desktop unavailable", "无法冻结桌面", "無法凍結桌面").detail(error);
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
                    self.painter.stroke(&frame, &laser)?;
                    laser.points.clear();
                }
                laser.points.push(Point {
                    pressure: 1.
                        - now.duration_since(sample.at).as_secs_f32()
                            / LASER_LIFETIME.as_secs_f32(),
                    ..sample.point
                });
            }
            self.painter.stroke(&frame, &laser)?;
        }
        if let Some(hole) = app.input.protected {
            clear_hole(renderer, hole)?;
        }
        if !app.has_contacts() && self.painter.samples.capacity() > 128_000 {
            self.painter.samples = Vec::new();
        }
        renderer.present()?;
        // A small canvas can use its FP16 layer for the final erase frame.
        // Release it after SDL has submitted the composition.
        self.release_unused_extended_layer(app);
        Ok(())
    }

    fn release_unused_extended_layer(&mut self, app: &App) {
        if self.extended_bytes() != 0 && !extended_cache_needed(app) {
            self.invalidate_layer();
        }
    }

    fn committed(&mut self, frame: &Frame<'_>, app: &mut App) -> Result<(), String> {
        let damage = app.input.ink_damage.take();
        let count = app.history.strokes().len();
        if count == 0 {
            self.layer = None;
            return Ok(());
        }
        let renderer = frame.renderer;
        let format = if renderer.extended {
            SDL_PIXELFORMAT_RGBA64_FLOAT
        } else {
            SDL_PIXELFORMAT_RGBA32
        };
        if renderer.extended
            && (renderer.name != "metal"
                || extended_bytes(frame.pixels) > frame.extended_budget
                || (self.layer.is_none() && !extended_cache_needed(app)))
        {
            self.invalidate_layer();
            return self.painter.history(frame, app, 0, None, 1.);
        }

        if self
            .layer
            .as_ref()
            .is_some_and(|layer| layer.size != frame.pixels || layer.format != format)
        {
            self.invalidate_layer();
        }
        let target = (frame.pixels, format);
        if self.layer.is_none() && self.unavailable_at != Some(target) {
            match Texture::with_format(
                renderer,
                frame.pixels,
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_BLEND_PREMULTIPLIED,
                format,
            ) {
                Ok(texture) => {
                    self.layer = Some(CommittedLayer {
                        texture,
                        size: frame.pixels,
                        format,
                        revision: None,
                        count: 0,
                        append: 0,
                        erase: 0,
                    });
                }
                Err(_) => self.unavailable_at = Some(target),
            }
        }
        if let Some(layer) = &mut self.layer {
            let revision = LayerRevision {
                scale: frame.scale,
                color: frame.color.revision,
                history: app.history.ink_revision(),
                white: if renderer.extended {
                    unsafe {
                        SDL_GetFloatProperty(
                            SDL_GetRendererProperties(renderer.raw),
                            SDL_PROP_RENDERER_SDR_WHITE_POINT_FLOAT,
                            1.,
                        )
                        .max(f32::MIN_POSITIVE)
                    }
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
                let result = renderer.with_target(layer.texture.0, || {
                    self.painter.queued = 0;
                    // Match the SDR white point used when drawing directly to
                    // the window. A highlighter builds its mask at scale 1.
                    let white = if renderer.extended {
                        revision.white
                    } else {
                        1.
                    };
                    self.painter
                        .repaint_layer(frame, app, rebuild, first, damage, white)
                });
                self.painter.queued = 0;
                if result.is_err() {
                    self.layer = None;
                    self.unavailable_at = Some(target);
                    return self.painter.history(frame, app, 0, None, 1.);
                }
                layer.revision = Some(revision);
            }

            // The history was painted with source-over into a transparent
            // texture, so its stored RGB is already premultiplied by alpha.
            return with_color_scale(renderer, revision_white_scale(layer), || unsafe {
                check(SDL_RenderTexture(
                    renderer.raw,
                    layer.texture.0,
                    ptr::null(),
                    ptr::null(),
                ))
            });
        }
        self.painter.history(frame, app, 0, None, 1.)
    }
}

fn extended_bytes([width, height]: [i32; 2]) -> usize {
    (width.max(0) as usize)
        .saturating_mul(height.max(0) as usize)
        .saturating_mul(8)
}

fn extended_cache_needed(app: &App) -> bool {
    app.history.strokes().len() >= EXTENDED_CACHE_STROKES
        || app.history.point_count() >= EXTENDED_CACHE_POINTS
        || app.input.pending_erasures.values().any(Erasure::has_edits)
}

fn revision_white_scale(layer: &CommittedLayer) -> f32 {
    let white = layer
        .revision
        .as_ref()
        .map_or(0., |revision| revision.white);
    if white > 0. { white.recip() } else { 1. }
}

fn with_color_scale(
    renderer: &Renderer,
    scale: f32,
    draw: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if scale == 1. {
        return draw();
    }
    unsafe {
        let mut previous = 1.;
        check(SDL_GetRenderColorScale(renderer.raw, &mut previous))?;
        check(SDL_SetRenderColorScale(renderer.raw, previous * scale))?;
        let result = draw();
        let restore = check(SDL_SetRenderColorScale(renderer.raw, previous));
        result.and(restore)
    }
}

fn set_scale(renderer: &Renderer, [x, y]: [f32; 2]) -> Result<(), String> {
    unsafe { check(SDL_SetRenderScale(renderer.raw, x, y)) }
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
    unsafe {
        check(SDL_SetRenderDrawColorFloat(renderer.raw, 0., 0., 0., 0.))?;
        check(SDL_SetRenderDrawBlendMode(renderer.raw, SDL_BLENDMODE_NONE))?;
        let draw = check(SDL_RenderFillRect(renderer.raw, &rect));
        let restore = check(SDL_SetRenderDrawBlendMode(
            renderer.raw,
            SDL_BLENDMODE_BLEND,
        ));
        draw.and(restore)
    }
}

#[derive(Default)]
struct StrokePainter {
    mesh: Mesh,
    samples: Vec<Point>,
    vertices: Vec<SDL_Vertex>,
    tile_indices: Vec<u32>,
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
        rebuild: bool,
        first: usize,
        damage: Option<Bounds>,
        white: f32,
    ) -> Result<(), String> {
        let renderer = frame.renderer;
        if rebuild {
            set_scale(renderer, frame.scale)?;
            renderer.clear([0.; 4])?;
            return self.history(frame, app, 0, None, white);
        }
        if first < app.history.strokes().len() {
            set_scale(renderer, frame.scale)?;
            self.history(frame, app, first, None, white)?;
        }
        if let Some(region) = damage.and_then(|bounds| dirty_rect(frame, bounds)) {
            // Set the clip in device pixels, including at fractional scale.
            set_scale(renderer, [1.; 2])?;
            unsafe { check(SDL_SetRenderClipRect(renderer.raw, &region))? };
            let rect = SDL_FRect {
                x: region.x as f32,
                y: region.y as f32,
                w: region.w as f32,
                h: region.h as f32,
            };
            let bounds = Bounds {
                min: [rect.x / frame.scale[0], rect.y / frame.scale[1]],
                max: [
                    (rect.x + rect.w) / frame.scale[0],
                    (rect.y + rect.h) / frame.scale[1],
                ],
            };
            let result = clear_rect(renderer, rect)
                .and_then(|()| self.history(frame, app, 0, Some(bounds), white));
            let reset_clip = unsafe { check(SDL_SetRenderClipRect(renderer.raw, ptr::null())) };
            result.and(reset_clip)?;
        }
        Ok(())
    }

    fn history(
        &mut self,
        frame: &Frame<'_>,
        app: &App,
        first: usize,
        region: Option<Bounds>,
        white: f32,
    ) -> Result<(), String> {
        let bounds = region.map(|_| app.history.stroke_bounds());
        for (index, stroke) in app.history.strokes().iter().enumerate().skip(first) {
            if let Some(region) = region
                && bounds.as_ref().unwrap()[index].is_none_or(|bounds| {
                    !region.intersects(bounds.path.padded(stroke.max_radius() * 2. + frame.aa))
                })
            {
                continue;
            }
            let mut result = Ok(());
            if !Erasure::preview(
                stroke,
                index,
                app.input.pending_erasures.values(),
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
        region: Option<Bounds>,
        white: f32,
    ) -> Result<(), String> {
        if points.is_empty() || stroke.opacity <= 0. {
            return Ok(());
        }
        let renderer = frame.renderer;
        let paper = if stroke.tool == Tool::Pencil {
            self.paper_texture(renderer)
        } else {
            ptr::null_mut()
        };
        let vertex_grain = paper.is_null();
        geometry::prepare_samples(stroke, points, vertex_grain, &mut self.samples);
        let rgb = frame.color.map(stroke.color.output_rgb(renderer.extended));
        if stroke.tool == Tool::Marker
            && let Some(mask) = self.marker_texture(renderer)
        {
            return self.marker(frame, mask, rgb, stroke, region, white);
        }
        with_color_scale(renderer, white, || {
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
                    [0; 2],
                    !vertex_grain,
                );
                #[cfg(test)]
                {
                    self.submitted_indices += self.mesh.indices.len();
                }
                submit(
                    renderer,
                    paper,
                    &self.vertices,
                    &self.mesh.indices,
                    &mut self.queued,
                )?;
            }
            Ok(())
        })
    }

    fn paper_texture(&mut self, renderer: &Renderer) -> *mut SDL_Texture {
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
                unsafe {
                    check(SDL_UpdateTexture(
                        texture.0,
                        ptr::null(),
                        pixels.as_ptr().cast(),
                        PAPER_SIZE * 4,
                    ))?;
                    // Only the pencil uses textured geometry on the canvas.
                    check(SDL_SetRenderTextureAddressMode(
                        renderer.raw,
                        SDL_TEXTURE_ADDRESS_WRAP,
                        SDL_TEXTURE_ADDRESS_WRAP,
                    ))?;
                }
                Ok(texture)
            };
            self.paper = create().ok();
            self.paper_unavailable = self.paper.is_none();
        }
        self.paper
            .as_ref()
            .map_or(ptr::null_mut(), |texture| texture.0)
    }

    fn marker_texture(&mut self, renderer: &Renderer) -> Option<*mut SDL_Texture> {
        if self.marker_mask.is_none() && !self.marker_unavailable {
            self.marker_mask = Texture::new(
                renderer,
                [MARKER_TILE; 2],
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_BLEND,
            )
            .ok();
            self.marker_unavailable = self.marker_mask.is_none();
        }
        self.marker_mask.as_ref().map(|texture| texture.0)
    }

    fn marker(
        &mut self,
        frame: &Frame<'_>,
        mask: *mut SDL_Texture,
        rgb: [f32; 3],
        stroke: &Stroke,
        region: Option<Bounds>,
        white: f32,
    ) -> Result<(), String> {
        let renderer = frame.renderer;
        // The ribbon can extend up to twice the largest radius at a join.
        let margin = stroke.width + frame.aa;
        let bounds = self.samples.iter().fold(
            [
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ],
            |bounds, p| {
                [
                    bounds[0].min(p.x - margin),
                    bounds[1].min(p.y - margin),
                    bounds[2].max(p.x + margin),
                    bounds[3].max(p.y + margin),
                ]
            },
        );
        let left = ((bounds[0] * frame.scale[0]).floor() as i32).clamp(0, frame.pixels[0]);
        let top = ((bounds[1] * frame.scale[1]).floor() as i32).clamp(0, frame.pixels[1]);
        let right = ((bounds[2] * frame.scale[0]).ceil() as i32).clamp(0, frame.pixels[0]);
        let bottom = ((bounds[3] * frame.scale[1]).ceil() as i32).clamp(0, frame.pixels[1]);
        if left >= right || top >= bottom {
            return Ok(());
        }
        unsafe {
            check(SDL_SetTextureColorModFloat(mask, rgb[0], rgb[1], rgb[2]))?;
            check(SDL_SetTextureAlphaModFloat(mask, stroke.opacity))?;
        }
        let tiled = right - left > MARKER_TILE || bottom - top > MARKER_TILE;
        for y in (top..bottom).step_by(MARKER_TILE as usize) {
            for x in (left..right).step_by(MARKER_TILE as usize) {
                let width = (right - x).min(MARKER_TILE) as f32;
                let height = (bottom - y).min(MARKER_TILE) as f32;
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
                    unsafe { check(SDL_SetRenderScale(renderer.raw, 1., 1.))? };
                    renderer.clear([1., 1., 1., 0.])?;
                    let visible = |a, b| {
                        region.is_none_or(|region| {
                            region.intersects(Bounds::between(a, b).padded(margin))
                        })
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
                        let indices = if tiled {
                            self.tile_indices.clear();
                            for triangle in self.mesh.indices.as_chunks::<3>().0 {
                                let p = triangle.map(|i| self.vertices[i as usize].position);
                                if p.iter().any(|p| p.x >= 0.)
                                    && p.iter().any(|p| p.x < width)
                                    && p.iter().any(|p| p.y >= 0.)
                                    && p.iter().any(|p| p.y < height)
                                {
                                    self.tile_indices.extend(triangle);
                                }
                            }
                            self.tile_indices.as_slice()
                        } else {
                            self.mesh.indices.as_slice()
                        };
                        #[cfg(test)]
                        {
                            self.submitted_indices += indices.len();
                        }
                        submit(
                            renderer,
                            ptr::null_mut(),
                            &self.vertices,
                            indices,
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
                let dst = if region.is_some() {
                    SDL_FRect {
                        x: x as f32,
                        y: y as f32,
                        w: width,
                        h: height,
                    }
                } else {
                    SDL_FRect {
                        x: x as f32 / frame.scale[0],
                        y: y as f32 / frame.scale[1],
                        w: width / frame.scale[0],
                        h: height / frame.scale[1],
                    }
                };
                with_color_scale(renderer, white, || unsafe {
                    check(SDL_RenderTexture(renderer.raw, mask, &src, &dst))
                })?;
            }
        }
        Ok(())
    }
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
    texture: *mut SDL_Texture,
    vertices: &[SDL_Vertex],
    indices: &[u32],
    queued: &mut usize,
) -> Result<(), String> {
    for batch in indices.chunks(GEOMETRY_BATCH) {
        if *queued + batch.len() > GEOMETRY_BATCH {
            unsafe { check(SDL_FlushRenderer(renderer.raw))? };
            *queued = 0;
        }
        renderer.geometry(texture, vertices, batch)?;
        *queued += batch.len();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::touch_tests::app;

    struct Surface(*mut SDL_Surface);
    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe { SDL_DestroySurface(self.0) };
        }
    }

    struct TestCanvas {
        renderer: Renderer,
        _surface: Surface,
        color: ColorManager,
    }
    impl TestCanvas {
        fn new([width, height]: [i32; 2]) -> Self {
            unsafe {
                let surface = Surface(SDL_CreateSurface(width, height, SDL_PIXELFORMAT_RGBA32));
                assert!(!surface.0.is_null());
                let raw = SDL_CreateSoftwareRenderer(surface.0);
                assert!(!raw.is_null(), "{}", error());
                assert!(SDL_SetRenderDrawBlendMode(raw, SDL_BLENDMODE_BLEND));
                Self {
                    renderer: Renderer {
                        raw,
                        extended: false,
                        name: "software".into(),
                    },
                    _surface: surface,
                    color: ColorManager::default(),
                }
            }
        }
        fn frame(&self) -> Frame<'_> {
            Frame {
                renderer: &self.renderer,
                color: &self.color,
                pixels: unsafe { [(*self._surface.0).w, (*self._surface.0).h] },
                scale: [1.; 2],
                aa: 0.8,
                extended_budget: MAX_EXTENDED_INK_TEXTURE_BYTES,
            }
        }
        fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
            unsafe {
                let image = Surface(SDL_RenderReadPixels(self.renderer.raw, ptr::null()));
                assert!(!image.0.is_null(), "{}", error());
                let [mut r, mut g, mut b, mut a] = [0; 4];
                assert!(SDL_ReadSurfacePixel(
                    image.0, x, y, &mut r, &mut g, &mut b, &mut a
                ));
                [r, g, b, a]
            }
        }
        fn pixels(&self) -> Vec<[u8; 4]> {
            unsafe {
                let image = Surface(SDL_RenderReadPixels(self.renderer.raw, ptr::null()));
                assert!(!image.0.is_null(), "{}", error());
                let mut pixels = Vec::new();
                for y in 0..(*image.0).h {
                    for x in 0..(*image.0).w {
                        let [mut r, mut g, mut b, mut a] = [0; 4];
                        assert!(SDL_ReadSurfacePixel(
                            image.0, x, y, &mut r, &mut g, &mut b, &mut a
                        ));
                        pixels.push([r, g, b, a]);
                    }
                }
                pixels
            }
        }
        fn history(&self, ink: &mut Ink, app: &mut App) {
            self.renderer.clear([0.; 4]).unwrap();
            ink.committed(&self.frame(), app).unwrap();
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
    fn cached_ink_keeps_alpha_and_tracks_history_and_eraser_edits() {
        let canvas = TestCanvas::new([128, 96]);
        let mut app = app();
        let mut ink = Ink::default();
        commit(&mut app, Tool::Pen, [255, 0, 0], &[[10., 20.], [100., 20.]]);
        canvas.history(&mut ink, &mut app);
        let red = canvas.pixel(50, 20);
        assert!((125..=129).contains(&red[0]), "{red:?}");
        assert_eq!(red[0], red[3]);
        assert_eq!(ink.layer.as_ref().unwrap().count, 1);

        ink.painter.vertices.clear();
        canvas.history(&mut ink, &mut app);
        assert!(ink.painter.vertices.is_empty());
        assert_eq!(canvas.pixel(50, 20), red);

        commit(&mut app, Tool::Pen, [0, 255, 0], &[[10., 70.], [100., 70.]]);
        canvas.history(&mut ink, &mut app);
        assert_eq!(ink.layer.as_ref().unwrap().count, 2);
        assert_eq!(canvas.pixel(50, 20), red);
        let green = canvas.pixel(50, 70);
        assert!(green[1] > 120);
        app.history.undo();
        canvas.history(&mut ink, &mut app);
        assert_eq!(canvas.pixel(50, 70), [0; 4]);
        app.history.redo();
        canvas.history(&mut ink, &mut app);
        assert_eq!(canvas.pixel(50, 70), green);

        app.erase_sample(1, Point::new(50., 20., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app);
        assert_eq!(canvas.pixel(50, 20), [0; 4]);
        app.reset_contacts();
        canvas.history(&mut ink, &mut app);
        assert_eq!(canvas.pixel(50, 20), red);

        let outside_hole = canvas.pixel(60, 20);
        clear_hole(
            &canvas.renderer,
            Rect {
                x: 47,
                y: 16,
                w: 8,
                h: 8,
            },
        )
        .unwrap();
        assert_eq!(canvas.pixel(50, 20), [0; 4]);
        assert_eq!(canvas.pixel(60, 20), outside_hole);
        app.history.clear();
        canvas.history(&mut ink, &mut app);
        assert!(ink.layer.is_none());
    }

    #[test]
    fn marker_crossing_has_the_same_opacity_as_the_rest_of_the_stroke() {
        let canvas = TestCanvas::new([1280, 96]);
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
        canvas.history(&mut ink, &mut app);
        assert!(ink.painter.marker_mask.is_some());
        let straight = canvas.pixel(65, 25);
        assert!((125..=129).contains(&straight[3]), "{straight:?}");
        assert_eq!(canvas.pixel(45, 25), straight);
        assert_eq!(canvas.pixel(MARKER_TILE - 1, 25), straight);
        assert_eq!(canvas.pixel(MARKER_TILE, 25), straight);
    }

    #[test]
    fn marker_split_updates_a_crossing_away_from_the_cut() {
        let canvas = TestCanvas::new([600, 160]);
        let full = TestCanvas::new([600, 160]);
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
        canvas.history(&mut ink, &mut app);
        let before = crossing(&canvas);
        assert_eq!(before, canvas.pixel(160, 60));

        app.erase_sample(1, Point::new(420., 60., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app);
        full.history(&mut Ink::default(), &mut app);
        let split = crossing(&full);
        assert!(split[3] > before[3]);
        assert_eq!(crossing(&canvas), split);

        app.reset_contacts();
        canvas.history(&mut ink, &mut app);
        assert_eq!(crossing(&canvas), before);

        app.erase_sample(2, Point::new(420., 60., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app);
        app.commit_erase(2);
        canvas.history(&mut ink, &mut app);
        assert_eq!(app.history.strokes().len(), 2);
        assert_eq!(crossing(&canvas), split);
        app.history.undo();
        canvas.history(&mut ink, &mut app);
        assert_eq!(crossing(&canvas), before);
        app.history.redo();
        canvas.history(&mut ink, &mut app);
        assert_eq!(crossing(&canvas), split);
    }

    #[test]
    fn small_extended_cache_releases_its_layer_after_erase() {
        let canvas = TestCanvas::new([320, 160]);
        let mut app = app();
        let mut ink = Ink::default();
        commit(
            &mut app,
            Tool::Pen,
            [255, 200, 0],
            &[[10., 60.], [300., 60.]],
        );
        assert!(!extended_cache_needed(&app));
        for (id, x) in [(1, 100.), (2, 180.)] {
            app.erase_sample(id, Point::new(x, 60., 1., [0.; 2]));
            canvas.history(&mut ink, &mut app);
            // Exercise the lifetime policy with a software-backed target.
            ink.layer.as_mut().unwrap().format = SDL_PIXELFORMAT_RGBA64_FLOAT;
            ink.release_unused_extended_layer(&app);
            assert_eq!(ink.extended_bytes(), extended_bytes([320, 160]));

            if id == 1 {
                app.commit_erase(id);
            } else {
                app.reset_contacts();
            }
            assert!(!extended_cache_needed(&app));
            ink.release_unused_extended_layer(&app);
            assert!(ink.layer.is_none());
            assert_eq!(ink.extended_bytes(), 0);
        }

        let stroke = (*app.history.strokes()[0]).clone();
        app.history
            .append(std::iter::repeat_n(stroke, EXTENDED_CACHE_STROKES));
        assert!(extended_cache_needed(&app));
    }

    #[test]
    fn local_erase_matches_a_full_rebuild_at_fractional_scale() {
        let canvas = TestCanvas::new([840, 240]);
        let full = TestCanvas::new([840, 240]);
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
            set_scale(renderer, frame.scale).unwrap();
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
        let mut compare = |app: &mut App| {
            canvas.renderer.clear([0.; 4]).unwrap();
            full.renderer.clear([0.; 4]).unwrap();
            ink.committed(&frame, app).unwrap();
            reference.invalidate_layer();
            reference.committed(&reference_frame, app).unwrap();
            let actual = canvas.pixels();
            let expected = full.pixels();
            let difference = actual.iter().zip(&expected).position(|(a, b)| a != b);
            assert!(
                difference.is_none(),
                "{:?}",
                difference.map(|i| (i % 840, i / 840, actual[i], expected[i]))
            );
        };
        let point = |x, y| Point::new(x, y, 1., [0.; 2]);
        compare(&mut app);
        app.erase_sample(1, point(240., 70.));
        compare(&mut app);
        app.erase_sample(1, point(260., 78.));
        compare(&mut app);
        app.erase_sample(2, point(300., 70.));
        compare(&mut app);
        app.commit_erase(2);
        compare(&mut app);
        app.reset_contacts();
        compare(&mut app);
        app.history.undo();
        compare(&mut app);
        app.history.redo();
        compare(&mut app);
    }

    #[test]
    fn local_erase_skips_distant_geometry_on_dense_canvas() {
        let canvas = TestCanvas::new([1024, 768]);
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
        canvas.history(&mut ink, &mut app);
        let full_indices = std::mem::take(&mut ink.painter.submitted_indices);
        app.erase_sample(1, Point::new(500., 350., 1., [0.; 2]));
        canvas.history(&mut ink, &mut app);
        let preview_indices = std::mem::take(&mut ink.painter.submitted_indices);
        eprintln!("Full canvas: {full_indices} indices. Erase preview: {preview_indices} indices.");
        assert!(preview_indices > 0 && preview_indices < full_indices / 100);
        app.commit_erase(1);
        canvas.history(&mut ink, &mut app);
        assert_eq!(ink.painter.submitted_indices, 0);
    }

    #[test]
    fn committed_layer_maps_logical_coordinates_at_high_pixel_density() {
        let canvas = TestCanvas::new([128, 96]);
        let mut app = app();
        let mut ink = Ink::default();
        commit(&mut app, Tool::Pen, [255, 0, 0], &[[30., 16.], [58., 16.]]);
        let mut frame = canvas.frame();
        frame.scale = [2.; 2];
        frame.aa = 0.4;
        unsafe {
            assert!(SDL_SetRenderScale(canvas.renderer.raw, 2., 2.));
        }
        canvas.renderer.clear([0.; 4]).unwrap();
        ink.committed(&frame, &mut app).unwrap();
        assert!(canvas.pixel(100, 32)[3] > 120);
        assert_eq!(canvas.pixel(35, 16), [0; 4]);
    }
}
