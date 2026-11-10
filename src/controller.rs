use crate::sdl::*;
#[cfg(not(target_os = "windows"))]
use std::ptr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < (self.x + self.w) as f32
            && y < (self.y + self.h) as f32
    }
    pub fn crosses(self, from: crate::model::Point, to: crate::model::Point) -> bool {
        let (mut enter, mut exit) = (0.0_f32, 1.0_f32);
        for (start, end, min, max) in [
            (from.x, to.x, self.x, self.x + self.w),
            (from.y, to.y, self.y, self.y + self.h),
        ] {
            let delta = end - start;
            let (min, max) = (min as f32, max as f32);
            if delta == 0. {
                if start < min || start > max {
                    return false;
                }
            } else {
                let near = (min - start) / delta;
                let far = (max - start) / delta;
                enter = enter.max(near.min(far));
                exit = exit.min(near.max(far));
                if enter > exit {
                    return false;
                }
            }
        }
        true
    }
    pub fn clipped(self, size: [i32; 2]) -> Option<Self> {
        let x = self.x.max(0);
        let y = self.y.max(0);
        let right = (self.x + self.w).min(size[0]);
        let bottom = (self.y + self.h).min(size[1]);
        (right > x && bottom > y).then_some(Self {
            x,
            y,
            w: right - x,
            h: bottom - y,
        })
    }
    pub fn pixels(self, window: &Window) -> Self {
        let size = window.size();
        let pixels = window.pixel_size();
        let scale = [pixels[0] as f32 / size[0], pixels[1] as f32 / size[1]];
        let x = (self.x as f32 * scale[0]).floor() as i32;
        let y = (self.y as f32 * scale[1]).floor() as i32;
        Self {
            x,
            y,
            w: ((self.x + self.w) as f32 * scale[0]).ceil() as i32 - x,
            h: ((self.y + self.h) as f32 * scale[1]).ceil() as i32 - y,
        }
    }
    #[cfg(not(target_os = "windows"))]
    pub fn sdl(self) -> SDL_Rect {
        SDL_Rect {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }
}

// Four disjoint rectangles cover the canvas except the protected controller.
#[cfg(any(not(target_os = "windows"), test))]
pub fn outside(size: [i32; 2], hole: Option<Rect>) -> Vec<Rect> {
    let full = Rect {
        x: 0,
        y: 0,
        w: size[0],
        h: size[1],
    };
    let Some(hole) = hole.and_then(|r| r.clipped(size)) else {
        return vec![full];
    };
    [
        Rect {
            x: 0,
            y: 0,
            w: size[0],
            h: hole.y,
        },
        Rect {
            x: 0,
            y: hole.y + hole.h,
            w: size[0],
            h: size[1] - hole.y - hole.h,
        },
        Rect {
            x: 0,
            y: hole.y,
            w: hole.x,
            h: hole.h,
        },
        Rect {
            x: hole.x + hole.w,
            y: hole.y,
            w: size[0] - hole.x - hole.w,
            h: hole.h,
        },
    ]
    .into_iter()
    .filter(|r| r.w > 0 && r.h > 0)
    .collect()
}

fn rectangle(canvas: &Window, toolbar: &Window) -> Option<Rect> {
    // Wayland intentionally has no global toplevel coordinates. Its controller
    // uses a transient parent instead of inventing a screen position.
    if crate::sdl::driver() == "wayland" {
        return None;
    }
    if unsafe { SDL_GetWindowFlags(toolbar.raw) } & SDL_WINDOW_HIDDEN != 0 {
        return None;
    }
    let bounds = crate::native::controller_bounds(canvas, toolbar)?;
    let x = bounds[0].floor() as i32 - 2;
    let y = bounds[1].floor() as i32 - 2;
    Rect {
        x,
        y,
        w: (bounds[0] + bounds[2]).ceil() as i32 - x + 2,
        h: (bounds[1] + bounds[3]).ceil() as i32 - y + 2,
    }
    .clipped(canvas.size().map(|v| v.ceil() as i32))
}

#[derive(Default)]
pub struct Guard {
    last: Option<([i32; 2], Option<Rect>)>,
}
impl Guard {
    pub fn update(
        &mut self,
        canvas: &Window,
        toolbar: &Window,
        mouse: bool,
        shape_rendering: bool,
    ) -> Result<(Option<Rect>, bool), String> {
        let hole = rectangle(canvas, toolbar);
        // Cocoa uses a higher native toolbar level. X11 uses an input region.
        // Both clip ink without a screen-sized SDL surface and texture.
        if cfg!(target_os = "macos") || crate::sdl::driver() == "x11" {
            return Ok((hole, false));
        }
        if unsafe { SDL_GetWindowFlags(canvas.raw) } & SDL_WINDOW_TRANSPARENT == 0 {
            return Ok((None, false));
        }
        let size = canvas.pixel_size();
        // Mouse mode uses a separate native pass-through setting.
        let pixels = if mouse || (!shape_rendering && !cfg!(target_os = "windows")) {
            None
        } else {
            hole.map(|r| r.pixels(canvas))
        };
        if self.last == Some((size, pixels)) {
            return Ok((hole, false));
        }
        #[cfg(target_os = "windows")]
        crate::native::set_controller_region(canvas, size, pixels)?;
        #[cfg(not(target_os = "windows"))]
        unsafe {
            if let Some(hole) = pixels {
                let surface = SDL_CreateSurface(size[0], size[1], SDL_PIXELFORMAT_RGBA32);
                if surface.is_null() {
                    return Err(crate::sdl::error());
                }
                let result = (|| {
                    let opaque = SDL_MapSurfaceRGBA(surface, 255, 255, 255, 255);
                    let transparent = SDL_MapSurfaceRGBA(surface, 0, 0, 0, 0);
                    check(SDL_FillSurfaceRect(surface, ptr::null(), transparent))?;
                    for area in outside(size, Some(hole)) {
                        check(SDL_FillSurfaceRect(surface, &area.sdl(), opaque))?;
                    }
                    // SDL applies the mask to both rendering and native hit testing.
                    check(SDL_SetWindowShape(canvas.raw, surface))
                })();
                SDL_DestroySurface(surface);
                result?;
            } else {
                check(SDL_SetWindowShape(canvas.raw, ptr::null_mut()))?;
            }
        }
        self.last = Some((size, pixels));
        Ok((hole, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excluded_area_never_belongs_to_the_canvas_regions() {
        for hole in [
            Rect {
                x: 20,
                y: 30,
                w: 40,
                h: 50,
            },
            Rect {
                x: -10,
                y: -20,
                w: 40,
                h: 50,
            },
            Rect {
                x: 90,
                y: 90,
                w: 40,
                h: 50,
            },
            Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
        ] {
            let areas = outside([100, 100], Some(hole));
            for y in 0..100 {
                for x in 0..100 {
                    let coverage = areas
                        .iter()
                        .filter(|r| r.contains(x as f32, y as f32))
                        .count();
                    assert_eq!(coverage, usize::from(!hole.contains(x as f32, y as f32)));
                }
            }
        }
    }
}
