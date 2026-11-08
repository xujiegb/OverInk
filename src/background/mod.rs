mod desktop;
#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
pub(crate) mod system;
use crate::{icc::ColorManager, sdl::*};
use std::{
    collections::{BTreeMap, BTreeSet},
    ptr,
    time::{Duration, Instant},
};

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
impl Image {
    #[cfg(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "freebsd",
        test
    ))]
    pub fn buffer(width: u32, height: u32) -> Result<Self, String> {
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or("Capture is too large")?;
        if pixels == 0 || pixels > 64 * 1024 * 1024 {
            return Err("Capture exceeds the 64 megapixel limit".into());
        }
        Ok(Self {
            width,
            height,
            rgba: vec![0; pixels * 4],
        })
    }
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd", test))]
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

struct Texture(*mut SDL_Texture);
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
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
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
    driver: String,
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
            driver: String::new(),
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
        driver: &str,
        drawing: bool,
    ) {
        self.images.retain(|id, _| connected.contains(id));
        let id = display.map(|d| d.id);
        let bounds = display.map_or([0; 4], |d| d.bounds);
        if id != self.display || bounds != self.bounds {
            let resized = id == self.display && bounds != self.bounds;
            if resized && let Some(id) = id {
                self.images.remove(&id);
            }
            self.capture = Capture::Idle;
            self.texture = None;
            self.display = id;
            self.bounds = bounds;
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
        self.driver = driver.into();
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
            Capture::Wait(due) if Instant::now() >= *due => {
                if self.display.is_none() {
                    Some(Err("No display to freeze".into()))
                } else if cfg!(target_os = "macos") || self.driver == "wayland" {
                    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
                    match system::Job::start(self.bounds) {
                        Ok(job) => {
                            self.capture = Capture::System(job);
                            None
                        }
                        Err(error) => Some(Err(error)),
                    }
                    #[cfg(not(any(
                        target_os = "linux",
                        target_os = "freebsd",
                        target_os = "macos"
                    )))]
                    {
                        Some(Err("No system capture backend".into()))
                    }
                } else {
                    Some(desktop::capture(window).map(Some))
                }
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
            Capture::System(job) => job.poll(),
            _ => None,
        };
        let result = result?;
        self.capture = Capture::Idle;
        match result {
            Ok(Some(image)) => {
                if let Some(id) = self.display {
                    // Bound retained screenshots to 256 MiB across displays.
                    while self.images.values().map(|v| v.rgba.len()).sum::<usize>()
                        + image.rgba.len()
                        > 256 * 1024 * 1024
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
        unsafe {
            if self.texture.is_none() {
                let texture = Texture(SDL_CreateTexture(
                    render.raw,
                    SDL_PIXELFORMAT_RGBA32,
                    SDL_TEXTUREACCESS_STATIC,
                    image.width as i32,
                    image.height as i32,
                ));
                if texture.0.is_null() {
                    return Err(crate::sdl::error());
                }
                let mut bytes = image.rgba.clone();
                color.map_rgba8(&mut bytes);
                check(SDL_UpdateTexture(
                    texture.0,
                    ptr::null(),
                    bytes.as_ptr().cast(),
                    (image.width * 4) as i32,
                ))?;
                SDL_SetTextureBlendMode(texture.0, SDL_BLENDMODE_NONE);
                SDL_SetTextureScaleMode(texture.0, SDL_SCALEMODE_LINEAR);
                self.texture = Some(texture);
            }
            let [w, h] = window.size();
            let vertices: Vec<_> = [
                (0., 0., 0., 0.),
                (w, 0., 1., 0.),
                (w, h, 1., 1.),
                (0., h, 0., 1.),
            ]
            .into_iter()
            .map(|(x, y, u, v)| SDL_Vertex {
                position: SDL_FPoint { x, y },
                color: SDL_FColor {
                    r: 1.,
                    g: 1.,
                    b: 1.,
                    a: 1.,
                },
                tex_coord: SDL_FPoint { x: u, y: v },
            })
            .collect();
            render.geometry(
                self.texture.as_ref().unwrap().0,
                &vertices,
                &[0, 1, 2, 0, 2, 3],
            )
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
        bg.display(None, &BTreeSet::new(), "x11", true);
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
        bg.display(Some(&screen), &BTreeSet::from([7]), "x11", true);
        bg.freeze();
        bg.display(Some(&screen), &BTreeSet::from([7]), "x11", false);
        assert!(!bg.pending());
        assert!(bg.frozen);
        bg.images.insert(7, Image::buffer(2, 2).unwrap());
        bg.display(Some(&screen), &BTreeSet::from([7]), "x11", true);
        assert!(!bg.pending());
        assert!(bg.images.contains_key(&7));
    }
}
