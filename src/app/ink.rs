use super::{App, LASER_LIFETIME};
use crate::{
    appearance::text,
    background::Background,
    color::InkColor,
    controller::Rect,
    geometry::{self, Mesh},
    icc::ColorManager,
    model::{Erasure, Point, Stroke, Tool},
    sdl::*,
};
use std::{ops::RangeInclusive, ptr, time::Instant};

const PAPER_SIZE: i32 = 64;
const MARKER_TILE: i32 = 1024;
const MESH_SEGMENTS: usize = 1024;
// SDL expands indexed geometry before upload. Bound each upload, including
// those for a large document rebuild or one unusually long stroke.
const GEOMETRY_BATCH: usize = 24_576;

struct Texture(*mut SDL_Texture);
impl Texture {
    fn new(
        renderer: &Renderer,
        [width, height]: [i32; 2],
        access: SDL_TextureAccess,
        blend: SDL_BlendMode,
    ) -> Result<Self, String> {
        unsafe {
            let raw =
                SDL_CreateTexture(renderer.raw, SDL_PIXELFORMAT_RGBA32, access, width, height);
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
}

#[derive(PartialEq)]
struct LayerRevision {
    scale: [f32; 2],
    color: u64,
    history: u64,
    eraser: u64,
}

struct CommittedLayer {
    texture: Texture,
    size: [i32; 2],
    revision: Option<LayerRevision>,
    count: usize,
}

#[derive(Default)]
pub(super) struct Ink {
    layer: Option<CommittedLayer>,
    unavailable_at: Option<[i32; 2]>,
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

    pub fn paint(
        &mut self,
        app: &mut App,
        renderer: &Renderer,
        window: &Window,
        color: &ColorManager,
        background: &mut Background,
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
        renderer.present()
    }

    fn committed(&mut self, frame: &Frame<'_>, app: &App) -> Result<(), String> {
        let count = app.history.doc.strokes.len();
        if count == 0 {
            self.layer = None;
            return Ok(());
        }
        let renderer = frame.renderer;
        // Avoid a second full-size FP16 surface and preserve extended gamut
        // and reference white on the HDR renderer.
        if renderer.extended {
            return self.painter.history(frame, app, 0);
        }

        if self
            .layer
            .as_ref()
            .is_some_and(|layer| layer.size != frame.pixels)
        {
            self.invalidate_layer();
        }
        if self.layer.is_none() && self.unavailable_at != Some(frame.pixels) {
            match Texture::new(
                renderer,
                frame.pixels,
                SDL_TEXTUREACCESS_TARGET,
                SDL_BLENDMODE_BLEND_PREMULTIPLIED,
            ) {
                Ok(texture) => {
                    self.layer = Some(CommittedLayer {
                        texture,
                        size: frame.pixels,
                        revision: None,
                        count: 0,
                    });
                }
                Err(_) => self.unavailable_at = Some(frame.pixels),
            }
        }
        if let Some(layer) = &mut self.layer {
            let revision = LayerRevision {
                scale: frame.scale,
                color: frame.color.revision,
                history: app.history.ink_revision(),
                eraser: app.input.eraser_revision,
            };
            let rebuild = layer.revision.as_ref() != Some(&revision) || layer.count > count;
            if rebuild || layer.count < count {
                self.painter.queued = 0;
                let result = renderer.with_target(layer.texture.0, || {
                    unsafe {
                        check(SDL_SetRenderScale(
                            renderer.raw,
                            frame.scale[0],
                            frame.scale[1],
                        ))?;
                    }
                    if rebuild {
                        renderer.clear([0.; 4])?;
                    }
                    self.painter
                        .history(frame, app, if rebuild { 0 } else { layer.count })
                });
                self.painter.queued = 0;
                if result.is_err() {
                    self.layer = None;
                    self.unavailable_at = Some(frame.pixels);
                    return self.painter.history(frame, app, 0);
                }
                layer.revision = Some(revision);
                layer.count = count;
            }

            // The history was painted with source-over into a transparent
            // texture, so its stored RGB is already premultiplied by alpha.
            return unsafe {
                check(SDL_RenderTexture(
                    renderer.raw,
                    layer.texture.0,
                    ptr::null(),
                    ptr::null(),
                ))
            };
        }
        self.painter.history(frame, app, 0)
    }
}

fn clear_hole(renderer: &Renderer, rect: Rect) -> Result<(), String> {
    let rect = SDL_FRect {
        x: rect.x as f32,
        y: rect.y as f32,
        w: rect.w as f32,
        h: rect.h as f32,
    };
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
}

impl StrokePainter {
    fn history(&mut self, frame: &Frame<'_>, app: &App, first: usize) -> Result<(), String> {
        for (index, stroke) in app.history.doc.strokes.iter().enumerate().skip(first) {
            let preview = Erasure::preview(stroke, index, app.input.pending_erasures.values());
            for fragment in preview.iter() {
                self.stroke(frame, fragment)?;
            }
        }
        Ok(())
    }

    fn stroke(&mut self, frame: &Frame<'_>, stroke: &Stroke) -> Result<(), String> {
        if stroke.points.is_empty() || stroke.opacity <= 0. {
            return Ok(());
        }
        let renderer = frame.renderer;
        let paper = if stroke.tool == Tool::Pencil {
            self.paper_texture(renderer)
        } else {
            ptr::null_mut()
        };
        let vertex_grain = paper.is_null();
        geometry::prepare_samples(stroke, vertex_grain, &mut self.samples);
        let rgb = frame.color.map(stroke.color.output_rgb(renderer.extended));
        if stroke.tool == Tool::Marker
            && let Some(mask) = self.marker_texture(renderer)
        {
            return self.marker(frame, mask, rgb, stroke);
        }
        for range in mesh_ranges(self.samples.len()) {
            self.mesh
                .update(stroke, &self.samples, range, frame.aa, vertex_grain);
            self.set_vertices(rgb, stroke.opacity, [1.; 2], [0; 2], !vertex_grain);
            submit(
                renderer,
                paper,
                &self.vertices,
                &self.mesh.indices,
                &mut self.queued,
            )?;
        }
        Ok(())
    }

    fn set_vertices(
        &mut self,
        rgb: [f32; 3],
        opacity: f32,
        scale: [f32; 2],
        origin: [i32; 2],
        paper: bool,
    ) {
        self.vertices.clear();
        self.vertices.extend(self.mesh.vertices.iter().map(|v| {
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
                // The mask accumulates coverage at full opacity. Tint it and
                // apply the marker's opacity only once, after intersections.
                self.queued = 0;
                renderer.with_target(mask, || {
                    unsafe { check(SDL_SetRenderScale(renderer.raw, 1., 1.))? };
                    renderer.clear([1., 1., 1., 0.])?;
                    for range in mesh_ranges(self.samples.len()) {
                        self.mesh
                            .update(stroke, &self.samples, range, frame.aa, false);
                        self.set_vertices([1.; 3], 1., frame.scale, [x, y], false);
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
                let dst = SDL_FRect {
                    x: x as f32 / frame.scale[0],
                    y: y as f32 / frame.scale[1],
                    w: width / frame.scale[0],
                    h: height / frame.scale[1],
                };
                unsafe { check(SDL_RenderTexture(renderer.raw, mask, &src, &dst))? };
            }
        }
        Ok(())
    }
}

fn mesh_ranges(sample_count: usize) -> impl Iterator<Item = RangeInclusive<usize>> {
    let end = sample_count.saturating_sub(1);
    (0..end.max(1))
        .step_by(MESH_SEGMENTS)
        .map(move |start| start..=(start + MESH_SEGMENTS).min(end))
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
        fn history(&self, ink: &mut Ink, app: &App) {
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
        canvas.history(&mut ink, &app);
        let red = canvas.pixel(50, 20);
        assert!((125..=129).contains(&red[0]), "{red:?}");
        assert_eq!(red[0], red[3]);
        assert_eq!(ink.layer.as_ref().unwrap().count, 1);

        ink.painter.vertices.clear();
        canvas.history(&mut ink, &app);
        assert!(ink.painter.vertices.is_empty());
        assert_eq!(canvas.pixel(50, 20), red);

        commit(&mut app, Tool::Pen, [0, 255, 0], &[[10., 70.], [100., 70.]]);
        canvas.history(&mut ink, &app);
        assert_eq!(ink.layer.as_ref().unwrap().count, 2);
        assert_eq!(canvas.pixel(50, 20), red);
        let green = canvas.pixel(50, 70);
        assert!(green[1] > 120);
        app.history.undo();
        canvas.history(&mut ink, &app);
        assert_eq!(canvas.pixel(50, 70), [0; 4]);
        app.history.redo();
        canvas.history(&mut ink, &app);
        assert_eq!(canvas.pixel(50, 70), green);

        app.erase_sample(1, Point::new(50., 20., 1., [0.; 2]));
        canvas.history(&mut ink, &app);
        assert_eq!(canvas.pixel(50, 20), [0; 4]);
        app.reset_contacts();
        canvas.history(&mut ink, &app);
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
        canvas.history(&mut ink, &app);
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
        canvas.history(&mut ink, &app);
        assert!(ink.painter.marker_mask.is_some());
        let straight = canvas.pixel(65, 25);
        assert!((125..=129).contains(&straight[3]), "{straight:?}");
        assert_eq!(canvas.pixel(45, 25), straight);
        assert_eq!(canvas.pixel(MARKER_TILE - 1, 25), straight);
        assert_eq!(canvas.pixel(MARKER_TILE, 25), straight);
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
        ink.committed(&frame, &app).unwrap();
        assert!(canvas.pixel(100, 32)[3] > 120);
        assert_eq!(canvas.pixel(35, 16), [0; 4]);
    }
}
