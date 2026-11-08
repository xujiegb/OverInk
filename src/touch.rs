use crate::{model::Point, sdl::*};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TouchKey {
    pub device: u64,
    pub finger: u64,
}
impl From<SDL_TouchFingerEvent> for TouchKey {
    fn from(event: SDL_TouchFingerEvent) -> Self {
        Self {
            device: event.touchID.0,
            finger: event.fingerID.0,
        }
    }
}

pub fn point(event: SDL_TouchFingerEvent, size: [f32; 2]) -> Option<Point> {
    if !event.x.is_finite()
        || !event.y.is_finite()
        || size.iter().any(|v| !v.is_finite() || *v <= 0.)
    {
        return None;
    }
    // SDL coordinates are window-normalized; use logical window units, not pixels.
    // Do not clamp outside-window motion: doing so would draw along the edge.
    let pressure = if event.pressure.is_finite() && event.pressure > 0. {
        event.pressure.clamp(0., 1.)
    } else {
        1.
    };
    Some(Point::new(
        event.x * size[0],
        event.y * size[1],
        pressure,
        [0.; 2],
    ))
}

pub fn is_indirect(device: SDL_TouchID) -> bool {
    if unsafe { SDL_WasInit(SDL_INIT_VIDEO) } == 0 {
        return false;
    }
    matches!(
        unsafe { SDL_GetTouchDeviceType(device) },
        SDL_TOUCH_DEVICE_INDIRECT_ABSOLUTE | SDL_TOUCH_DEVICE_INDIRECT_RELATIVE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_coordinates_use_logical_size_and_keep_outside_motion() {
        let event = SDL_TouchFingerEvent {
            x: 0.25,
            y: 0.5,
            ..Default::default()
        };
        let p = point(event, [800., 600.]).unwrap();
        assert_eq!([p.x, p.y, p.pressure], [200., 300., 1.]);
        let p = point(SDL_TouchFingerEvent { x: -0.1, ..event }, [800., 600.]).unwrap();
        assert_eq!(p.x, -80.);
    }
    #[test]
    fn invalid_coordinates_are_rejected_and_devices_keep_distinct_fingers() {
        assert!(
            point(
                SDL_TouchFingerEvent {
                    x: f32::NAN,
                    ..Default::default()
                },
                [800., 600.]
            )
            .is_none()
        );
        let a = TouchKey {
            device: 1,
            finger: 7,
        };
        let b = TouchKey {
            device: 2,
            finger: 7,
        };
        assert_ne!(a, b);
    }
}
