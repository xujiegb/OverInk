#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod system;
#[cfg(target_os = "windows")]
mod windows;
use crate::{icc::ColorManager, sdl::*};
use std::{
    collections::{BTreeMap, BTreeSet},
    ptr,
    time::{Duration, Instant},
};

const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Srgb8,
    LinearRgba16,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    encoding: Encoding,
}
impl Image {
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    pub fn buffer(width: u32, height: u32) -> Result<Self, String> {
        Self::allocate(width, height, Encoding::Srgb8)
    }
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    fn hdr_buffer(width: u32, height: u32) -> Result<Self, String> {
        Self::allocate(width, height, Encoding::LinearRgba16)
    }
    fn allocate(width: u32, height: u32, encoding: Encoding) -> Result<Self, String> {
        let bytes_per_pixel = if encoding == Encoding::LinearRgba16 {
            8
        } else {
            4
        };
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or("Capture is too large")?;
        if size == 0 || size > MAX_SNAPSHOT_BYTES {
            return Err("Capture exceeds the 256 MiB limit".into());
        }
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(size)
            .map_err(|error| error.to_string())?;
        rgba.resize(size, 0);
        Ok(Self {
            width,
            height,
            rgba,
            encoding,
        })
    }
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    /// The source must contain initialized pixels in each row. Padding is never read.
    unsafe fn copy_hdr_rows(
        &mut self,
        bytes: *const u8,
        len: usize,
        stride: usize,
    ) -> Result<(), String> {
        let row_bytes = self.width as usize * 8;
        let needed = stride
            .checked_mul(self.height as usize - 1)
            .and_then(|offset| offset.checked_add(row_bytes));
        if bytes.is_null() || stride < row_bytes || needed.is_none_or(|size| size > len) {
            return Err("Invalid HDR capture row pitch".into());
        }
        for (y, row) in self.rgba.chunks_exact_mut(row_bytes).enumerate() {
            unsafe { ptr::copy_nonoverlapping(bytes.add(y * stride), row.as_mut_ptr(), row_bytes) };
            for pixel in row.as_chunks_mut::<8>().0 {
                // A display snapshot is opaque. Leave the three FP16 color
                // channels intact, including negative values and values above 1.
                pixel[6..].copy_from_slice(&0x3c00_u16.to_ne_bytes());
            }
        }
        Ok(())
    }
    #[cfg(any(target_os = "macos", test))]
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        use image::ImageDecoder;
        let mut reader = image::ImageReader::open(path)
            .map_err(|e| e.to_string())?
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits);
        let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let profile = decoder.icc_profile().ok().flatten();
        let pixels = image::DynamicImage::from_decoder(decoder)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        let mut result = Self {
            width: pixels.width(),
            height: pixels.height(),
            rgba: pixels.into_raw(),
            encoding: Encoding::Srgb8,
        };
        if let Some(profile) = profile {
            result.convert_to_srgb(&profile);
        }
        Ok(result)
    }
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd", test))]
    pub fn convert_to_srgb(&mut self, profile: &[u8]) {
        use lcms2_sys as cms;
        if profile.is_empty() || profile.len() > 16 * 1024 * 1024 {
            return;
        }
        unsafe {
            let input = cms::cmsOpenProfileFromMem(profile.as_ptr().cast(), profile.len() as u32);
            if input.is_null() {
                return;
            }
            let srgb = cms::cmsCreate_sRGBProfile();
            let transform = cms::cmsCreateTransform(
                input,
                cms::PixelFormat::RGBA_8,
                srgb,
                cms::PixelFormat::RGBA_8,
                cms::Intent::RelativeColorimetric,
                cms::FLAGS_COPY_ALPHA | cms::FLAGS_BLACKPOINTCOMPENSATION,
            );
            cms::cmsCloseProfile(input);
            cms::cmsCloseProfile(srgb);
            if !transform.is_null() {
                cms::cmsDoTransform(
                    transform,
                    self.rgba.as_ptr().cast(),
                    self.rgba.as_mut_ptr().cast(),
                    self.width * self.height,
                );
                cms::cmsDeleteTransform(transform);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotMode {
    Sdr,
    Linear,
    LiveOnly(crate::appearance::Text),
}
impl SnapshotMode {
    pub fn unavailable(self) -> Option<crate::appearance::Text> {
        match self {
            Self::LiveOnly(reason) => Some(reason),
            _ => None,
        }
    }
    fn accepts(self, image: &Image) -> bool {
        matches!(
            (self, image.encoding),
            (Self::Sdr, Encoding::Srgb8) | (Self::Linear, Encoding::LinearRgba16)
        )
    }
}

pub fn is_xwayland(window: &Window, driver: &str) -> bool {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if driver == "x11" {
        return crate::picker::desktop::is_xwayland(window);
    }
    let _ = (window, driver);
    false
}

pub fn snapshot_mode(
    window: &Window,
    render: &Renderer,
    driver: &str,
    xwayland: bool,
) -> SnapshotMode {
    if driver == "wayland" || xwayland {
        return SnapshotMode::LiveOnly(crate::appearance::text(
            "This session cannot guarantee HDR in a desktop screenshot",
            "当前会话无法保证截屏保留 HDR",
            "目前的工作階段無法保證螢幕擷取保留 HDR",
        ));
    }
    let hdr = unsafe {
        SDL_GetBooleanProperty(
            SDL_GetWindowProperties(window.raw),
            SDL_PROP_WINDOW_HDR_ENABLED_BOOLEAN,
            false,
        ) || window.display_id().is_some_and(|id| {
            SDL_GetBooleanProperty(
                SDL_GetDisplayProperties(SDL_DisplayID(id)),
                SDL_PROP_DISPLAY_HDR_ENABLED_BOOLEAN,
                false,
            )
        })
    };
    if cfg!(target_os = "windows") || (cfg!(target_os = "macos") && hdr) {
        if render.extended {
            SnapshotMode::Linear
        } else {
            SnapshotMode::LiveOnly(crate::appearance::text(
                "Freezing this desktop requires an extended-linear-sRGB renderer",
                "冻结此桌面需要支持扩展线性 sRGB 的渲染器",
                "凍結此桌面需要支援延伸線性 sRGB 的繪圖引擎",
            ))
        }
    } else if hdr {
        SnapshotMode::LiveOnly(crate::appearance::text(
            "HDR desktop capture is unavailable on this backend",
            "当前显示后端不支持 HDR 截屏",
            "目前的顯示後端不支援 HDR 螢幕擷取",
        ))
    } else {
        SnapshotMode::Sdr
    }
}

struct Texture(*mut SDL_Texture);
impl Texture {
    fn new(render: &Renderer, image: &Image, color: &ColorManager) -> Result<Self, String> {
        unsafe {
            let linear = image.encoding == Encoding::LinearRgba16;
            let raw = if linear {
                if !render.extended {
                    return Err("The renderer cannot display the captured HDR range".into());
                }
                let props = SDL_CreateProperties();
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_TEXTURE_CREATE_FORMAT_NUMBER,
                    SDL_PIXELFORMAT_RGBA64_FLOAT.0 as i64,
                );
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_TEXTURE_CREATE_COLORSPACE_NUMBER,
                    SDL_COLORSPACE_SRGB_LINEAR.0 as i64,
                );
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_TEXTURE_CREATE_WIDTH_NUMBER,
                    image.width as i64,
                );
                SDL_SetNumberProperty(
                    props,
                    SDL_PROP_TEXTURE_CREATE_HEIGHT_NUMBER,
                    image.height as i64,
                );
                // A local-display capture is already mapped to this display.
                // A positive content headroom could ask SDL to tone map it again.
                SDL_SetFloatProperty(props, SDL_PROP_TEXTURE_CREATE_HDR_HEADROOM_FLOAT, 0.);
                let texture = SDL_CreateTextureWithProperties(render.raw, props);
                SDL_DestroyProperties(props);
                texture
            } else {
                SDL_CreateTexture(
                    render.raw,
                    SDL_PIXELFORMAT_RGBA32,
                    SDL_TEXTUREACCESS_STATIC,
                    image.width as i32,
                    image.height as i32,
                )
            };
            if raw.is_null() {
                return Err(crate::sdl::error());
            }
            let texture = Self(raw);
            let mut mapped;
            let (bytes, bytes_per_pixel) = if linear {
                (&image.rgba, 8)
            } else {
                mapped = image.rgba.clone();
                color.map_rgba8(&mut mapped);
                (&mapped, 4)
            };
            check(SDL_UpdateTexture(
                texture.0,
                ptr::null(),
                bytes.as_ptr().cast(),
                (image.width * bytes_per_pixel) as i32,
            ))?;
            check(SDL_SetTextureBlendMode(texture.0, SDL_BLENDMODE_NONE))?;
            check(SDL_SetTextureScaleMode(texture.0, SDL_SCALEMODE_LINEAR))?;
            Ok(texture)
        }
    }
}
impl Drop for Texture {
    fn drop(&mut self) {
        unsafe {
            SDL_DestroyTexture(self.0);
        }
    }
}
enum Capture {
    Idle,
    Wait(Instant),
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    System(system::Job),
}
pub struct Background {
    pub frozen: bool,
    display: Option<u32>,
    bounds: [i32; 4],
    images: BTreeMap<u32, Image>,
    texture: Option<Texture>,
    capture: Capture,
    started: Instant,
    mode: SnapshotMode,
}
impl Default for Background {
    fn default() -> Self {
        Self {
            frozen: false,
            display: None,
            bounds: [0; 4],
            images: BTreeMap::new(),
            texture: None,
            capture: Capture::Idle,
            started: Instant::now(),
            mode: SnapshotMode::Sdr,
        }
    }
}
impl Background {
    pub fn pending(&self) -> bool {
        !matches!(self.capture, Capture::Idle)
    }
    pub fn live(&mut self) {
        self.frozen = false;
        self.capture = Capture::Idle;
        self.images.clear();
        self.texture = None;
    }
    pub fn freeze(&mut self) {
        self.frozen = true;
        if let Some(id) = self.display {
            self.images.remove(&id);
        }
        self.texture = None;
        self.capture = Capture::Wait(Instant::now() + Duration::from_millis(120));
        self.started = Instant::now();
    }
    pub fn pause_capture(&mut self) {
        self.capture = Capture::Idle;
    }
    pub fn display(
        &mut self,
        display: Option<&crate::display::Display>,
        connected: &BTreeSet<u32>,
        mode: SnapshotMode,
        drawing: bool,
    ) -> Result<(), crate::appearance::Text> {
        self.images.retain(|id, _| connected.contains(id));
        let id = display.map(|d| d.id);
        let bounds = display.map_or([0; 4], |d| d.bounds);
        if id != self.display || bounds != self.bounds || self.mode != mode {
            let resized = id == self.display && bounds != self.bounds;
            if resized && let Some(id) = id {
                self.images.remove(&id);
            }
            self.capture = Capture::Idle;
            self.texture = None;
            self.display = id;
            self.bounds = bounds;
            self.mode = mode;
        }
        if let Some(id) = id
            && self
                .images
                .get(&id)
                .is_some_and(|image| !mode.accepts(image))
        {
            self.images.remove(&id);
            self.texture = None;
        }
        if self.frozen
            && let Some(reason) = mode.unavailable()
        {
            self.live();
            return Err(reason);
        }
        if !drawing {
            self.pause_capture();
        } else if !self.pending()
            && self.frozen
            && id.is_some_and(|id| !self.images.contains_key(&id))
        {
            self.capture = Capture::Wait(Instant::now() + Duration::from_millis(120));
            self.started = Instant::now();
        }
        Ok(())
    }
    pub fn invalidate_texture(&mut self) {
        self.texture = None;
    }
    pub fn poll(&mut self, window: &Window) -> Option<Result<(), String>> {
        if self.pending() && self.started.elapsed() > Duration::from_secs(120) {
            self.live();
            return Some(Err("Desktop capture timed out".into()));
        }
        let result = match &mut self.capture {
            Capture::Wait(due) if Instant::now() >= *due => self.start_capture(window),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            Capture::System(job) => job.poll(),
            _ => None,
        };
        let result = result?;
        self.capture = Capture::Idle;
        match result {
            Ok(Some(image)) if self.mode.accepts(&image) => {
                if let Some(id) = self.display {
                    // Bound retained screenshots to 256 MiB across displays.
                    while self.images.values().map(|v| v.rgba.len()).sum::<usize>()
                        + image.rgba.len()
                        > MAX_SNAPSHOT_BYTES
                    {
                        let Some(key) = self.images.keys().next().copied() else {
                            break;
                        };
                        self.images.remove(&key);
                    }
                    self.images.insert(id, image);
                    self.texture = None;
                }
                Some(Ok(()))
            }
            Ok(Some(_)) => {
                self.live();
                Some(Err(
                    "The screenshot did not preserve the desktop's color range".into(),
                ))
            }
            Ok(None) => {
                self.live();
                Some(Err("Desktop capture cancelled".into()))
            }
            Err(error) => {
                self.live();
                Some(Err(error))
            }
        }
    }
    fn start_capture(&mut self, window: &Window) -> Option<Result<Option<Image>, String>> {
        let Some(id) = self.display.filter(|id| window.display_id() == Some(*id)) else {
            return Some(Err(
                "The drawing surface is not on the selected display".into()
            ));
        };
        if let Some(reason) = self.mode.unavailable() {
            return Some(Err(reason.get(crate::appearance::Language::English).into()));
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            #[cfg(target_os = "windows")]
            let job = windows::start(id);
            #[cfg(target_os = "macos")]
            let job = macos::start(self.bounds, window.pixel_size(), self.mode);
            let _ = id;
            match job {
                Ok(job) => {
                    self.capture = Capture::System(job);
                    None
                }
                Err(error) => Some(Err(error)),
            }
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            let _ = id;
            if self.mode != SnapshotMode::Sdr {
                return Some(Err(
                    "HDR desktop capture is unavailable on this backend".into()
                ));
            }
            Some(crate::picker::desktop::capture(window).map(Some))
        }
    }
    pub fn draw(
        &mut self,
        render: &Renderer,
        window: &Window,
        color: &ColorManager,
    ) -> Result<(), String> {
        if !self.frozen {
            return Ok(());
        }
        let Some(image) = self.display.and_then(|id| self.images.get(&id)) else {
            return Ok(());
        };
        if self.texture.is_none() {
            self.texture = Some(Texture::new(render, image, color)?);
        }
        let texture = self.texture.as_ref().unwrap().0;
        let [w, h] = window.size();
        let dst = SDL_FRect { x: 0., y: 0., w, h };
        unsafe {
            if image.encoding == Encoding::Srgb8 {
                return check(SDL_RenderTexture(render.raw, texture, ptr::null(), &dst));
            }
            let mut previous = 1.;
            check(SDL_GetRenderColorScale(render.raw, &mut previous))?;
            let white = SDL_GetFloatProperty(
                SDL_GetRendererProperties(render.raw),
                SDL_PROP_RENDERER_SDR_WHITE_POINT_FLOAT,
                1.,
            );
            if !white.is_finite() || white <= 0. {
                return Err("Invalid SDR reference white level".into());
            }
            // SDL multiplies all draw calls by its current SDR white level.
            // Cancel that multiplier just for the captured linear pixels.
            check(SDL_SetRenderColorScale(render.raw, 1. / white))?;
            let draw = check(SDL_RenderTexture(render.raw, texture, ptr::null(), &dst));
            let restore = check(SDL_SetRenderColorScale(render.raw, previous));
            draw.and(restore)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnect_forgets_frozen_frames_and_live_mode_releases_images() {
        let mut bg = Background::default();
        bg.images.insert(7, Image::buffer(2, 2).unwrap());
        bg.display = Some(7);
        bg.frozen = true;
        bg.display(None, &BTreeSet::new(), SnapshotMode::Sdr, true)
            .unwrap();
        assert!(bg.images.is_empty());
        assert!(!bg.pending());
        bg.freeze();
        assert!(bg.pending());
        bg.live();
        assert!(!bg.pending());
        assert!(!bg.frozen);
        assert!(Image::buffer(u32::MAX, u32::MAX).is_err());
    }
    #[test]
    fn frozen_png_decodes_without_modifying_the_source_file() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "overink-png-test-{}-{unique}.png",
            std::process::id()
        ));
        image::RgbaImage::from_pixel(3, 2, image::Rgba([18, 171, 239, 255]))
            .save(&path)
            .unwrap();
        let image = Image::load(&path).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        assert!(
            image
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|v| *v == [18, 171, 239, 255])
        );
        assert!(path.exists());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn mouse_mode_defers_capture_without_discarding_frozen_pages() {
        let mut bg = Background::default();
        let screen = crate::display::Display {
            id: 7,
            name: "Display".into(),
            bounds: [0, 0, 100, 100],
            refresh: 60.,
            scale: 1.,
        };
        let connected = BTreeSet::from([7]);
        bg.display(Some(&screen), &connected, SnapshotMode::Sdr, true)
            .unwrap();
        bg.freeze();
        bg.display(Some(&screen), &connected, SnapshotMode::Sdr, false)
            .unwrap();
        assert!(!bg.pending());
        assert!(bg.frozen);
        bg.images.insert(7, Image::buffer(2, 2).unwrap());
        bg.display(Some(&screen), &connected, SnapshotMode::Sdr, true)
            .unwrap();
        assert!(!bg.pending());
        assert!(bg.images.contains_key(&7));
    }
    #[test]
    fn enabling_hdr_replaces_the_cached_sdr_capture() {
        let mut bg = Background::default();
        let screen = crate::display::Display {
            id: 7,
            name: "Display".into(),
            bounds: [0, 0, 100, 100],
            refresh: 60.,
            scale: 1.,
        };
        let connected = BTreeSet::from([7]);
        bg.display(Some(&screen), &connected, SnapshotMode::Sdr, true)
            .unwrap();
        bg.images.insert(7, Image::buffer(2, 2).unwrap());
        bg.frozen = true;
        bg.display(Some(&screen), &connected, SnapshotMode::Linear, true)
            .unwrap();
        assert!(bg.images.is_empty());
        assert!(bg.pending());
        let reason = crate::appearance::text(
            "No HDR screenshot backend",
            "没有 HDR 截屏后端",
            "沒有 HDR 螢幕擷取後端",
        );
        let unavailable = SnapshotMode::LiveOnly(reason);
        assert!(
            bg.display(Some(&screen), &connected, unavailable, true)
                .is_err()
        );
        assert!(!bg.pending());
        assert!(!bg.frozen);
    }
    #[test]
    fn fp16_capture_keeps_highlights_and_negative_color_components() {
        let mut image = Image::hdr_buffer(1, 2).unwrap();
        let mut padded = [0; 24];
        padded[..2].copy_from_slice(&0x4800_u16.to_ne_bytes()); // 8.0
        padded[2..4].copy_from_slice(&0xb800_u16.to_ne_bytes()); // -0.5
        padded[12..14].copy_from_slice(&0x4000_u16.to_ne_bytes()); // 2.0
        unsafe { image.copy_hdr_rows(padded.as_ptr(), padded.len(), 12) }.unwrap();
        assert_eq!(&image.rgba[..6], &padded[..6]);
        assert_eq!(&image.rgba[8..14], &padded[12..18]);
        assert_eq!(&image.rgba[6..8], &0x3c00_u16.to_ne_bytes());
        assert_eq!(&image.rgba[14..16], &0x3c00_u16.to_ne_bytes());
    }
}
