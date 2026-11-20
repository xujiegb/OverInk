#[cfg(target_os = "macos")]
mod macos;
mod png;
pub mod screenshot;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod system;
#[cfg(target_os = "windows")]
mod windows;
use crate::{icc::ColorManager, sdl::*};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    io::Read,
    time::{Duration, Instant},
};

const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Srgb8,
    LinearRgba16,
}

impl Encoding {
    fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Srgb8 => 4,
            Self::LinearRgba16 => 8,
        }
    }
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    encoding: Encoding,
}
impl Image {
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "windows",
        test
    ))]
    pub fn buffer(width: u32, height: u32) -> Result<Self, String> {
        Self::allocate(width, height, Encoding::Srgb8)
    }
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    fn hdr_buffer(width: u32, height: u32) -> Result<Self, String> {
        Self::allocate(width, height, Encoding::LinearRgba16)
    }
    fn allocate(width: u32, height: u32, encoding: Encoding) -> Result<Self, String> {
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(encoding.bytes_per_pixel()))
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
    pub fn crop(mut self, [left, top, right, bottom]: [u32; 4]) -> Result<Self, String> {
        if left >= right || top >= bottom || right > self.width || bottom > self.height {
            return Err("The selection is outside the screenshot".into());
        }
        let [width, height] = [right - left, bottom - top];
        if width != self.width || height != self.height {
            let bytes = self.encoding.bytes_per_pixel();
            let row_bytes = width as usize * bytes;
            for (row, y) in (top..bottom).enumerate() {
                let offset = (y as usize * self.width as usize + left as usize) * bytes;
                self.rgba
                    .copy_within(offset..offset + row_bytes, row * row_bytes);
            }
            self.rgba.truncate(height as usize * row_bytes);
            self.rgba.shrink_to_fit();
            self.width = width;
            self.height = height;
        }
        Ok(self)
    }

    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    fn copy_hdr_rows(
        &mut self,
        len: usize,
        stride: usize,
        mut copy: impl FnMut(usize, &mut [u8]),
    ) -> Result<(), String> {
        let row_bytes = (self.width as usize)
            .checked_mul(8)
            .ok_or("Invalid HDR capture row width")?;
        let height = self.height as usize;
        let needed = height
            .checked_sub(1)
            .and_then(|last| last.checked_mul(stride))
            .and_then(|offset| offset.checked_add(row_bytes));
        if row_bytes == 0
            || self.encoding != Encoding::LinearRgba16
            || row_bytes.checked_mul(height) != Some(self.rgba.len())
            || stride < row_bytes
            || needed.is_none_or(|size| size > len)
        {
            return Err("Invalid HDR capture row pitch".into());
        }
        for (y, row) in self.rgba.chunks_exact_mut(row_bytes).enumerate() {
            copy(y * stride, row);
            for pixel in row.as_chunks_mut::<8>().0 {
                // A display snapshot is opaque. Leave the three FP16 color
                // channels intact, including negative values and values above 1.
                pixel[6..].copy_from_slice(&0x3c00_u16.to_ne_bytes());
            }
        }
        Ok(())
    }
    // WGC supplies BGRA8 for SDR monitors. Validate the mapped D3D11 pitch
    // before copying; the snapshot format consumed by SDL remains opaque RGBA8.
    #[cfg(any(target_os = "windows", test))]
    fn copy_sdr_bgra_rows(
        &mut self,
        len: usize,
        stride: usize,
        mut copy: impl FnMut(usize, &mut [u8]),
    ) -> Result<(), String> {
        let row_bytes = (self.width as usize)
            .checked_mul(4)
            .ok_or("Invalid SDR capture row width")?;
        let needed = (self.height as usize)
            .checked_sub(1)
            .and_then(|last| last.checked_mul(stride))
            .and_then(|offset| offset.checked_add(row_bytes));
        if row_bytes == 0
            || self.encoding != Encoding::Srgb8
            || row_bytes.checked_mul(self.height as usize) != Some(self.rgba.len())
            || stride < row_bytes
            || needed.is_none_or(|size| size > len)
        {
            return Err("Invalid SDR capture row pitch".into());
        }
        for (y, row) in self.rgba.chunks_exact_mut(row_bytes).enumerate() {
            copy(y * stride, row);
            for pixel in row.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
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
            .into_rgba8();
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
    #[cfg(any(target_os = "linux", target_os = "freebsd", test))]
    pub fn convert_to_srgb(&mut self, profile: &[u8]) {
        crate::icc::convert_rgba8_to_srgb(&mut self.rgba, profile);
    }
}

// Keep the original channels available for a renderer reset without retaining
// a second uncompressed copy of native captures beside the active texture.
struct Snapshot {
    width: u32,
    height: u32,
    encoding: Encoding,
    bytes: Vec<u8>,
    compressed: bool,
}

impl From<Image> for Snapshot {
    fn from(image: Image) -> Self {
        Self {
            width: image.width,
            height: image.height,
            encoding: image.encoding,
            bytes: image.rgba,
            compressed: false,
        }
    }
}

impl Snapshot {
    #[cfg(any(target_os = "macos", target_os = "windows", test))]
    fn compress(image: Image) -> Result<Self, String> {
        use std::io::Write;
        let mut snapshot = Self::from(image);
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder
            .write_all(&snapshot.bytes)
            .map_err(|e| e.to_string())?;
        let compressed = encoder.finish().map_err(|e| e.to_string())?;
        if compressed.len() < snapshot.bytes.len() {
            snapshot.bytes = compressed;
            snapshot.compressed = true;
        }
        Ok(snapshot)
    }

    fn pixels(&self) -> Result<Cow<'_, [u8]>, String> {
        if !self.compressed {
            return Ok(Cow::Borrowed(&self.bytes));
        }
        let size = self.width as usize * self.height as usize * self.encoding.bytes_per_pixel();
        let mut pixels = Vec::with_capacity(size);
        flate2::read::ZlibDecoder::new(self.bytes.as_slice())
            .take(size as u64 + 1)
            .read_to_end(&mut pixels)
            .map_err(|e| e.to_string())?;
        if pixels.len() != size {
            return Err("Invalid frozen desktop snapshot size".into());
        }
        Ok(Cow::Owned(pixels))
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
    fn accepts(self, encoding: Encoding) -> bool {
        matches!(
            (self, encoding),
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
    hdr: bool,
    extended_available: bool,
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
    if hdr && (cfg!(target_os = "windows") || cfg!(target_os = "macos")) {
        if extended_available {
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

enum Capture {
    Idle,
    Wait(Instant),
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    System(system::Job<Snapshot>),
}
pub struct Background {
    pub frozen: bool,
    display: Option<u32>,
    bounds: [i32; 4],
    images: BTreeMap<u32, Snapshot>,
    texture: Option<SnapshotTexture>,
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
                .is_some_and(|image| !mode.accepts(image.encoding))
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
        Some(self.finish_capture(result?))
    }
    fn finish_capture(&mut self, result: Result<Option<Snapshot>, String>) -> Result<(), String> {
        self.capture = Capture::Idle;
        match result {
            Ok(Some(image)) if self.mode.accepts(image.encoding) => {
                if let Some(id) = self.display {
                    // Bound retained screenshots to 256 MiB across displays.
                    while self.images.values().map(|v| v.bytes.len()).sum::<usize>()
                        + image.bytes.len()
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
                Ok(())
            }
            Ok(Some(_)) => {
                self.live();
                Err("The screenshot did not preserve the desktop's color range".into())
            }
            Ok(None) => {
                self.live();
                Err("Desktop capture cancelled".into())
            }
            Err(error) => {
                self.live();
                Err(error)
            }
        }
    }
    fn start_capture(&mut self, window: &Window) -> Option<Result<Option<Snapshot>, String>> {
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
            let job = windows::start(id, self.mode == SnapshotMode::Linear, Snapshot::compress);
            #[cfg(target_os = "macos")]
            let job = macos::start(
                self.bounds,
                window.pixel_size(),
                self.mode,
                Snapshot::compress,
            );
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
            Some(crate::picker::desktop::capture(window).map(|image| Some(image.into())))
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
        let linear = image.encoding == Encoding::LinearRgba16;
        if self.texture.is_none() {
            let mut pixels = image.pixels()?;
            if !linear && !color.is_identity() {
                color.map_rgba8(pixels.to_mut());
            }
            self.texture = Some(SnapshotTexture::new(
                render,
                [image.width, image.height],
                linear,
                pixels.as_ref(),
            )?);
        }
        let Some(texture) = self.texture.as_ref() else {
            return Err("Background snapshot texture was not initialized".into());
        };
        let [w, h] = window.size();
        let dst = SDL_FRect { x: 0., y: 0., w, h };
        texture.draw(render, &dst, linear)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn sdr_capture_converts_bgra_with_stride_and_rejects_short_frames() -> TestResult {
        let mut image = Image::buffer(2, 2)?;
        let row0 = [3_u8, 2, 1, 45, 6, 5, 4, 12, 99, 99, 99, 99];
        let row1 = [9_u8, 8, 7, 32, 12, 11, 10, 0, 88, 88, 88, 88];
        let mut data = Vec::from(row0);
        data.extend_from_slice(&row1);
        assert!(image.copy_sdr_bgra_rows(18, 12, |_, _| {}).is_err());
        image.copy_sdr_bgra_rows(data.len(), 12, |offset, row| {
            row.copy_from_slice(&data[offset..offset + row.len()]);
        })?;
        assert_eq!(
            image.rgba,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        assert_eq!(image.encoding, Encoding::Srgb8);
        Ok(())
    }

    fn display_result(result: Result<(), crate::appearance::Text>) -> TestResult {
        result.map_err(|message| message.get(crate::appearance::Language::English).into())
    }
    #[test]
    fn disconnect_forgets_frozen_frames_and_live_mode_releases_images() -> TestResult {
        let mut bg = Background::default();
        bg.images.insert(7, Image::buffer(2, 2)?.into());
        bg.display = Some(7);
        bg.frozen = true;
        display_result(bg.display(None, &BTreeSet::new(), SnapshotMode::Sdr, true))?;
        assert!(bg.images.is_empty());
        assert!(!bg.pending());
        bg.freeze();
        assert!(bg.pending());
        bg.live();
        assert!(!bg.pending());
        assert!(!bg.frozen);
        assert!(Image::buffer(u32::MAX, u32::MAX).is_err());
        Ok(())
    }
    #[test]
    fn frozen_png_decodes_without_modifying_the_source_file() -> TestResult {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "overink-png-test-{}-{unique}.png",
            std::process::id()
        ));
        image::RgbaImage::from_pixel(3, 2, image::Rgba([18, 171, 239, 255])).save(&path)?;
        let image = Image::load(&path)?;
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
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn mouse_mode_defers_capture_without_discarding_frozen_pages() -> TestResult {
        let mut bg = Background::default();
        let screen = crate::display::Display {
            id: 7,
            name: "Display".into(),
            bounds: [0, 0, 100, 100],
            refresh: 60.,
            scale: 1.,
        };
        let connected = BTreeSet::from([7]);
        display_result(bg.display(Some(&screen), &connected, SnapshotMode::Sdr, true))?;
        bg.freeze();
        display_result(bg.display(Some(&screen), &connected, SnapshotMode::Sdr, false))?;
        assert!(!bg.pending());
        assert!(bg.frozen);
        bg.images.insert(7, Image::buffer(2, 2)?.into());
        display_result(bg.display(Some(&screen), &connected, SnapshotMode::Sdr, true))?;
        assert!(!bg.pending());
        assert!(bg.images.contains_key(&7));
        Ok(())
    }
    #[test]
    fn enabling_hdr_replaces_the_cached_sdr_capture() -> TestResult {
        let mut bg = Background::default();
        let screen = crate::display::Display {
            id: 7,
            name: "Display".into(),
            bounds: [0, 0, 100, 100],
            refresh: 60.,
            scale: 1.,
        };
        let connected = BTreeSet::from([7]);
        display_result(bg.display(Some(&screen), &connected, SnapshotMode::Sdr, true))?;
        bg.images.insert(7, Image::buffer(2, 2)?.into());
        bg.frozen = true;
        display_result(bg.display(Some(&screen), &connected, SnapshotMode::Linear, true))?;
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
        Ok(())
    }
    #[test]
    fn completed_capture_exits_pending_and_exposes_the_snapshot() -> TestResult {
        let mut bg = Background {
            display: Some(7),
            mode: SnapshotMode::Linear,
            ..Default::default()
        };
        bg.freeze();
        assert!(bg.pending());
        let snapshot = Snapshot::compress(Image::hdr_buffer(128, 128)?)?;
        assert!(bg.finish_capture(Ok(Some(snapshot))).is_ok());
        assert!(bg.frozen && !bg.pending());
        assert_eq!(bg.images[&7].encoding, Encoding::LinearRgba16);
        Ok(())
    }
    #[test]
    fn fp16_capture_keeps_highlights_and_negative_color_components() -> TestResult {
        let mut image = Image::hdr_buffer(1, 2)?;
        let mut padded = [0; 24];
        padded[..2].copy_from_slice(&0x4800_u16.to_ne_bytes()); // 8.0
        padded[2..4].copy_from_slice(&0xb800_u16.to_ne_bytes()); // -0.5
        padded[12..14].copy_from_slice(&0x4000_u16.to_ne_bytes()); // 2.0
        image.copy_hdr_rows(padded.len(), 12, |offset, row| {
            row.copy_from_slice(&padded[offset..offset + row.len()]);
        })?;
        assert_eq!(&image.rgba[..6], &padded[..6]);
        assert_eq!(&image.rgba[8..14], &padded[12..18]);
        assert_eq!(&image.rgba[6..8], &0x3c00_u16.to_ne_bytes());
        assert_eq!(&image.rgba[14..16], &0x3c00_u16.to_ne_bytes());
        Ok(())
    }
    #[test]
    fn compressed_hdr_snapshot_can_rebuild_the_texture_losslessly() -> TestResult {
        let mut image = Image::hdr_buffer(128, 64)?;
        let pixel = [0x4800_u16, 0xb800, 0x4000, 0x3c00].map(u16::to_ne_bytes);
        for bytes in image.rgba.as_chunks_mut::<8>().0 {
            bytes.copy_from_slice(pixel.as_flattened());
        }
        let snapshot = Snapshot::compress(image)?;
        assert!(snapshot.compressed);
        assert_eq!(snapshot.encoding, Encoding::LinearRgba16);
        assert!(snapshot.bytes.len() < 8192);
        for _ in 0..2 {
            let rgba = snapshot.pixels()?;
            assert_eq!(rgba.len(), 128 * 64 * 8);
            assert!(
                rgba.as_chunks::<8>()
                    .0
                    .iter()
                    .all(|rgba| rgba == pixel.as_flattened())
            );
        }
        Ok(())
    }
}
