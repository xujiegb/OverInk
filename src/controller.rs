use crate::sdl::*;
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
    ) -> Result<Option<Rect>, String> {
        let hole = rectangle(canvas, toolbar);
        if unsafe { SDL_GetWindowFlags(canvas.raw) } & SDL_WINDOW_TRANSPARENT == 0 {
            return Ok(None);
        }
        let size = canvas.pixel_size();
        // Cocoa derives ignoresMouseEvents from an active shape. Remove that
        // per-pixel shape in Mouse mode so whole-window passthrough stays set.
        let pixels = if mouse || !shape_rendering {
            None
        } else {
            hole.map(|r| r.pixels(canvas))
        };
        if self.last == Some((size, pixels)) {
            return Ok(hole);
        }
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
        Ok(hole)
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
