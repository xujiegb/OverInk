#![forbid(unsafe_code)]

use crate::sdl::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        // Native input coordinates can be close to i32::MAX. Widen the
        // endpoints before comparing against the floating-point input.
        let right = i64::from(self.x) + i64::from(self.w);
        let bottom = i64::from(self.y) + i64::from(self.h);
        x >= self.x as f32 && y >= self.y as f32 && x < right as f32 && y < bottom as f32
    }
    pub fn crosses(self, from: crate::model::Point, to: crate::model::Point) -> bool {
        let (mut enter, mut exit) = (0.0_f32, 1.0_f32);
        for (start, end, min, max) in [
            (
                from.x,
                to.x,
                i64::from(self.x),
                i64::from(self.x) + i64::from(self.w),
            ),
            (
                from.y,
                to.y,
                i64::from(self.y),
                i64::from(self.y) + i64::from(self.h),
            ),
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
        // Native window coordinates may approach the i32 limits. Perform
        // endpoint arithmetic in i64 before passing any region to SDL, X11
        // or Wayland, rather than overflowing x + width / y + height.
        if size[0] <= 0 || size[1] <= 0 || self.w <= 0 || self.h <= 0 {
            return None;
        }
        let [right_limit, bottom_limit] = size.map(i64::from);
        let x = i64::from(self.x).clamp(0, right_limit);
        let y = i64::from(self.y).clamp(0, bottom_limit);
        let right = (i64::from(self.x) + i64::from(self.w)).clamp(0, right_limit);
        let bottom = (i64::from(self.y) + i64::from(self.h)).clamp(0, bottom_limit);
        (right > x && bottom > y).then_some(Self {
            x: x as i32,
            y: y as i32,
            w: (right - x) as i32,
            h: (bottom - y) as i32,
        })
    }
    pub fn pixels(self, window: &Window) -> Self {
        let size = window.size();
        let pixels = window.pixel_size();
        let scale = [pixels[0] as f32 / size[0], pixels[1] as f32 / size[1]];
        self.scaled(scale)
    }

    fn scaled(self, scale: [f32; 2]) -> Self {
        // Window placement may approach the i32 limits. Calculate both
        // endpoints without overflowing before narrowing to native pixels.
        let [sx, sy] = scale.map(f64::from);
        let right_edge = i64::from(self.x) + i64::from(self.w);
        let bottom_edge = i64::from(self.y) + i64::from(self.h);
        let x = (f64::from(self.x) * sx).floor() as i32;
        let y = (f64::from(self.y) * sy).floor() as i32;
        let right = (right_edge as f64 * sx).ceil() as i32;
        let bottom = (bottom_edge as f64 * sy).ceil() as i32;
        Self {
            x,
            y,
            w: right.saturating_sub(x),
            h: bottom.saturating_sub(y),
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
// Return a stack-backed iterator so native input routing never needs to
// allocate a temporary Vec just to iterate over at most four regions.
#[cfg(any(not(target_os = "windows"), test))]
pub fn outside(size: [i32; 2], hole: Option<Rect>) -> impl Iterator<Item = Rect> {
    let full = Rect {
        x: 0,
        y: 0,
        w: size[0],
        h: size[1],
    };
    let areas = if let Some(hole) = hole.and_then(|r| r.clipped(size)) {
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
    } else {
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        [full, empty, empty, empty]
    };
    areas.into_iter().filter(|r| r.w > 0 && r.h > 0)
}

fn rectangle(canvas: &Window, toolbar: &Window) -> Option<Rect> {
    // Wayland intentionally has no global toplevel coordinates. Its controller
    // uses a transient parent instead of inventing a screen position.
    if crate::sdl::driver() == "wayland" {
        return None;
    }
    if toolbar.hidden() {
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
        if !canvas.transparent() {
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
        {
            // SDL applies this mask to both rendering and native hit testing.
            let opaque_regions = pixels.map(|hole| {
                outside(size, Some(hole))
                    .map(Rect::sdl)
                    .collect::<Vec<_>>()
            });
            canvas.set_opaque_shape_regions(size, opaque_regions.as_deref())?;
        }
        self.last = Some((size, pixels));
        Ok((hole, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipping_extreme_native_coordinates_never_overflows() {
        assert_eq!(
            Rect {
                x: i32::MAX - 10,
                y: i32::MAX - 10,
                w: i32::MAX,
                h: i32::MAX,
            }
            .clipped([i32::MAX, i32::MAX]),
            Some(Rect {
                x: i32::MAX - 10,
                y: i32::MAX - 10,
                w: 10,
                h: 10,
            })
        );
        assert_eq!(
            Rect {
                x: i32::MIN,
                y: i32::MIN,
                w: i32::MAX,
                h: i32::MAX,
            }
            .clipped([100, 100]),
            None
        );
        assert_eq!(
            Rect {
                x: -10,
                y: -20,
                w: 30,
                h: 40,
            }
            .clipped([100, 100]),
            Some(Rect {
                x: 0,
                y: 0,
                w: 20,
                h: 20,
            })
        );
        assert_eq!(
            Rect {
                x: 1,
                y: 1,
                w: 5,
                h: 5,
            }
            .clipped([0, 100]),
            None
        );
    }

    #[test]
    fn containment_and_crossings_do_not_overflow_native_endpoints() {
        let rect = Rect {
            x: i32::MAX - 1_048_576,
            y: i32::MAX - 1_048_576,
            w: i32::MAX,
            h: i32::MAX,
        };
        let inside = 2_147_483_648.0;
        assert!(rect.contains(inside, inside));
        assert!(!rect.contains(-1.0, inside));
        let outside = crate::model::Point::new(inside - 2_097_152.0, inside, 0.5, [0.; 2]);
        let within = crate::model::Point::new(inside, inside, 0.5, [0.; 2]);
        assert!(rect.crosses(outside, within));
        assert!(!rect.crosses(outside, outside));
    }

    #[test]
    fn extreme_pixel_rectangles_do_not_overflow_native_bounds() {
        let rect = Rect {
            x: i32::MAX - 8,
            y: i32::MAX - 10,
            w: 100,
            h: 100,
        };
        let scaled = rect.scaled([1., 1.]);
        assert_eq!(scaled.x, i32::MAX - 8);
        assert_eq!(scaled.y, i32::MAX - 10);
        assert_eq!(scaled.w, 8);
        assert_eq!(scaled.h, 10);
    }

    #[test]
    fn uncovered_canvas_regions_are_bounded_without_a_protected_hole() {
        let whole = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        assert_eq!(outside([100, 100], None).collect::<Vec<_>>(), vec![whole]);
        let outside_canvas = Rect {
            x: 200,
            y: 200,
            w: 20,
            h: 20,
        };
        assert_eq!(
            outside([100, 100], Some(outside_canvas)).collect::<Vec<_>>(),
            vec![whole]
        );
        assert!(outside([0, 100], None).next().is_none());
    }

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
            let areas = outside([100, 100], Some(hole)).collect::<Vec<_>>();
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
