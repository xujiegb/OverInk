#![forbid(unsafe_code)]

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
    let [x, y] = [event.x * size[0], event.y * size[1]];
    if !x.is_finite() || !y.is_finite() || size.iter().any(|v| *v <= 0.) {
        return None;
    }
    // SDL coordinates are window-normalized; use logical window units, not pixels.
    // Do not clamp outside-window motion: doing so would draw along the edge.
    let pressure = if event.pressure.is_finite() && event.pressure > 0. {
        event.pressure.clamp(0., 1.)
    } else {
        1.
    };
    Some(Point::new(x, y, pressure, [0.; 2]))
}

pub fn starts_on_window(event: SDL_TouchFingerEvent) -> bool {
    (0.0..=1.0).contains(&event.x)
        && (0.0..=1.0).contains(&event.y)
        && !indirect_touch_device(event.touchID)
}

pub fn key_from_event(event: &SDL_Event) -> Option<TouchKey> {
    crate::sdl::finger_event(event).map(TouchKey::from)
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
        assert_eq!(
            point(event, [800., 600.]).map(|p| [p.x, p.y, p.pressure]),
            Some([200., 300., 1.])
        );
        assert_eq!(
            point(SDL_TouchFingerEvent { x: -0.1, ..event }, [800., 600.]).map(|p| p.x),
            Some(-80.)
        );
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
        assert!(
            point(
                SDL_TouchFingerEvent {
                    x: f32::MAX,
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
