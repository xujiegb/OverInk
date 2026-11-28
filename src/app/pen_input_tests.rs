//! Regression tests for pen/touch contact boundaries and Cocoa event ordering.
use super::*;
const CANVAS: SDL_WindowID = SDL_WindowID(1);
const OTHER_WINDOW: SDL_WindowID = SDL_WindowID(2);
const PEN: SDL_PenID = SDL_PenID(73);
const SIZE: [f32; 2] = [1000., 700.];

fn contact(app: &mut App, window: SDL_WindowID, down: bool, x: f32) {
    app.event(
        &SDL_Event {
            ptouch: SDL_PenTouchEvent {
                r#type: if down {
                    SDL_EVENT_PEN_DOWN
                } else {
                    SDL_EVENT_PEN_UP
                },
                windowID: window,
                which: PEN,
                pen_state: if down {
                    SDL_PEN_INPUT_DOWN
                } else {
                    Default::default()
                },
                x,
                y: 140.,
                down,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
}

fn motion(app: &mut App, window: SDL_WindowID, down: bool, x: f32) {
    app.event(
        &SDL_Event {
            pmotion: SDL_PenMotionEvent {
                r#type: SDL_EVENT_PEN_MOTION,
                windowID: window,
                which: PEN,
                pen_state: if down {
                    SDL_PEN_INPUT_DOWN
                } else {
                    Default::default()
                },
                x,
                y: 140.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
}

#[test]
fn pen_up_routed_to_another_window_must_commit_active_canvas() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 10.);
    motion(&mut app, CANVAS, true, 100.);

    // The desktop/window transition sends PEN_UP with a different windowID.
    contact(&mut app, OTHER_WINDOW, false, 700.);
    assert!(app.history.active.is_none());
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(
        app.history.strokes()[0].points.last().map(|p| p.x),
        Some(100.)
    );

    motion(&mut app, CANVAS, false, 800.);
    contact(&mut app, CANVAS, true, 820.);
    contact(&mut app, CANVAS, false, 840.);
    assert_eq!(app.history.strokes().len(), 2);
    assert!(app.history.strokes()[1].points.iter().all(|p| p.x >= 820.));
}

#[test]
fn normal_pen_up_keeps_last_coordinate_sample() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 10.);
    contact(&mut app, CANVAS, false, 40.);
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(app.history.strokes()[0].points.len(), 2);
    assert_eq!(app.history.strokes()[0].points[1].x, 40.);
}

#[test]
fn invalid_pen_coordinates_never_create_an_ink_sample_at_the_origin() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, f32::NAN);
    assert!(app.history.active.is_none());
    motion(&mut app, CANVAS, true, 150.);
    motion(&mut app, CANVAS, true, f32::INFINITY);
    contact(&mut app, CANVAS, false, f32::NAN);
    let [stroke] = app.history.strokes() else {
        panic!("expected one completed stroke");
    };
    assert_eq!(stroke.points.len(), 1);
    assert_eq!(stroke.points[0].x, 150.);
}

fn finger_contact(app: &mut App, ty: SDL_EventType, x: f32) {
    app.event(&finger_packet(ty, CANVAS, 3, x), CANVAS, SIZE);
}

fn finger_packet(ty: SDL_EventType, window: SDL_WindowID, finger: u64, x: f32) -> SDL_Event {
    SDL_Event {
        tfinger: SDL_TouchFingerEvent {
            r#type: ty,
            touchID: SDL_TouchID(9),
            fingerID: SDL_FingerID(finger),
            windowID: window,
            x,
            y: 0.2,
            pressure: if ty == SDL_EVENT_FINGER_UP { 0. } else { 1. },
            ..Default::default()
        },
    }
}

#[test]
fn release_sent_to_another_window_commits_only_the_matching_finger() {
    let mut app = super::touch_tests::app();
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.05);
    finger_contact(&mut app, SDL_EVENT_FINGER_MOTION, 0.1);
    app.event(
        &finger_packet(SDL_EVENT_FINGER_DOWN, CANVAS, 4, 0.3),
        CANVAS,
        SIZE,
    );
    app.event(
        &finger_packet(SDL_EVENT_FINGER_UP, OTHER_WINDOW, 3, 0.9),
        CANVAS,
        SIZE,
    );

    assert_eq!(app.input.touches.len(), 1);
    let [stroke] = app.history.strokes() else {
        panic!("expected one completed finger stroke");
    };
    assert_eq!(stroke.points.last().map(|p| p.x), Some(100.));
}

#[test]
fn xinput_registration_and_hover_leave_touch_available_after_pen_up() {
    let mut app = super::touch_tests::app();
    app.pen_hover_reliable = false;
    app.event(
        &SDL_Event {
            pproximity: SDL_PenProximityEvent {
                r#type: SDL_EVENT_PEN_PROXIMITY_IN,
                which: PEN,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
    motion(&mut app, CANVAS, false, 250.);
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.05);
    assert_eq!(app.input.touches.len(), 1);

    contact(&mut app, CANVAS, true, 250.);
    assert!(app.input.touches.is_empty());
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.1);
    assert!(app.input.touches.is_empty());
    contact(&mut app, OTHER_WINDOW, false, 600.);
    motion(&mut app, CANVAS, false, 260.);
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.2);
    assert_eq!(app.input.touches.len(), 1);
    finger_contact(&mut app, SDL_EVENT_FINGER_UP, 0.3);
    assert_eq!(app.history.strokes().len(), 2);
}

#[test]
#[ignore = "synthetic input profiling workload"]
fn profile_pen_touch_input() {
    const GESTURES: u32 = 20_000;
    const SAMPLES: u32 = 96;
    let mut app = super::touch_tests::app();
    app.pen_hover_reliable = false;

    let start = Instant::now();
    for _ in 0..GESTURES {
        contact(&mut app, CANVAS, true, 100.);
        for sample in 0..SAMPLES {
            let x = 102. + sample as f32 * 4.;
            motion(&mut app, CANVAS, true, x);
            app.event(
                std::hint::black_box(&SDL_Event {
                    paxis: SDL_PenAxisEvent {
                        r#type: SDL_EVENT_PEN_AXIS,
                        windowID: CANVAS,
                        which: PEN,
                        pen_state: SDL_PEN_INPUT_DOWN,
                        axis: SDL_PEN_AXIS_PRESSURE,
                        value: sample as f32 / SAMPLES as f32,
                        x,
                        y: 140.,
                        ..Default::default()
                    },
                }),
                CANVAS,
                SIZE,
            );
        }
        contact(&mut app, CANVAS, false, 490.);
        assert_eq!(app.history.strokes().len(), 1);
        app.history = History::default();
    }
    println!(
        "INPUT_PROFILE,pen,{},{:.3}",
        GESTURES * (2 * SAMPLES + 2),
        start.elapsed().as_secs_f64() * 1000.
    );

    let mut app = super::touch_tests::app();
    let start = Instant::now();
    for _ in 0..GESTURES {
        for (ty, count) in [
            (SDL_EVENT_FINGER_DOWN, 1),
            (SDL_EVENT_FINGER_MOTION, SAMPLES),
            (SDL_EVENT_FINGER_UP, 1),
        ] {
            for sample in 0..count {
                let x = 0.15
                    + 0.005
                        * (sample
                            + if ty == SDL_EVENT_FINGER_UP {
                                SAMPLES
                            } else {
                                0
                            }) as f32;
                for finger in [1, 2] {
                    let packet = finger_packet(ty, CANVAS, finger, x);
                    app.event(std::hint::black_box(&packet), CANVAS, SIZE);
                }
            }
        }
        assert_eq!(app.history.strokes().len(), 2);
        app.history = History::default();
    }
    println!(
        "INPUT_PROFILE,touch,{},{:.3}",
        GESTURES * 2 * (SAMPLES + 2),
        start.elapsed().as_secs_f64() * 1000.
    );
}

#[test]
fn repeated_finger_down_after_missing_up_starts_an_independent_stroke() {
    let mut app = super::touch_tests::app();
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.05);
    finger_contact(&mut app, SDL_EVENT_FINGER_MOTION, 0.1);
    assert_eq!(app.input.touches.len(), 1);

    // Another DOWN with the very same finger ID must commit the old stroke.
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.7);
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(app.input.touches.len(), 1);
    finger_contact(&mut app, SDL_EVENT_FINGER_MOTION, 0.73);
    finger_contact(&mut app, SDL_EVENT_FINGER_UP, 0.75);

    assert_eq!(app.history.strokes().len(), 2);
    assert!(
        app.history.strokes()[0]
            .points
            .iter()
            .all(|point| point.x <= 100.)
    );
    assert!(
        app.history.strokes()[1]
            .points
            .iter()
            .all(|point| point.x >= 700.)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn long_valid_finger_release_without_pressure_preserves_endpoint() {
    let mut app = super::touch_tests::app();
    app.event(
        &SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: SDL_EVENT_FINGER_DOWN,
                touchID: SDL_TouchID(5),
                fingerID: SDL_FingerID(5),
                windowID: CANVAS,
                x: 0.1,
                y: 0.1,
                pressure: 0.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
    app.event(
        &SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: SDL_EVENT_FINGER_UP,
                touchID: SDL_TouchID(5),
                fingerID: SDL_FingerID(5),
                windowID: CANVAS,
                x: 0.1,
                y: 0.9,
                pressure: 0.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
    let [stroke] = app.history.strokes() else {
        panic!("expected a complete touch stroke");
    };
    assert_eq!(stroke.points.len(), 2);
    assert_eq!(stroke.points[1].y, 630.);
}

#[cfg(target_os = "macos")]
#[test]
fn long_valid_release_with_positive_pressure_preserves_shape_endpoint() {
    let mut app = super::touch_tests::app();
    app.use_tool(Tool::Line);
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.05);
    finger_contact(&mut app, SDL_EVENT_FINGER_MOTION, 0.1);
    // Positive contact pressure at release is not evidence of a stale
    // coordinate, even if it is far away from the last motion sample.
    app.event(
        &SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: SDL_EVENT_FINGER_UP,
                touchID: SDL_TouchID(9),
                fingerID: SDL_FingerID(3),
                windowID: CANVAS,
                x: 0.8,
                y: 0.2,
                pressure: 1.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
    let [stroke] = app.history.strokes() else {
        panic!("expected a completed line shape");
    };
    assert_eq!(stroke.points.last().map(|point| point.x), Some(800.));
}

#[cfg(target_os = "macos")]
#[test]
fn sparse_pen_without_pressure_retains_valid_distant_up_position() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 10.);
    contact(&mut app, CANVAS, false, 300.);
    let [stroke] = app.history.strokes() else {
        panic!("expected completed stroke");
    };
    assert_eq!(stroke.points.len(), 2);
    assert_eq!(stroke.points[1].x, 300.);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_long_pen_release_with_measured_pressure_keeps_endpoint() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 10.);
    app.event(
        &SDL_Event {
            paxis: SDL_PenAxisEvent {
                r#type: SDL_EVENT_PEN_AXIS,
                windowID: CANVAS,
                which: PEN,
                pen_state: SDL_PEN_INPUT_DOWN,
                axis: SDL_PEN_AXIS_PRESSURE,
                value: 0.4,
                x: 10.,
                y: 140.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
    contact(&mut app, CANVAS, false, 700.);
    let [stroke] = app.history.strokes() else {
        panic!("expected completed pen stroke");
    };
    assert_eq!(stroke.points.last().map(|p| p.x), Some(700.));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_long_finger_release_after_positive_pressure_keeps_endpoint() {
    let mut app = super::touch_tests::app();
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.05);
    finger_contact(&mut app, SDL_EVENT_FINGER_UP, 0.75);
    let [stroke] = app.history.strokes() else {
        panic!("expected completed finger stroke");
    };
    assert_eq!(stroke.points.last().map(|p| p.x), Some(750.));
}

#[cfg(target_os = "macos")]
fn cocoa_touch(app: &mut App, timestamp: u64, down: bool, x: f32) {
    app.event(
        &SDL_Event {
            ptouch: SDL_PenTouchEvent {
                r#type: if down {
                    SDL_EVENT_PEN_DOWN
                } else {
                    SDL_EVENT_PEN_UP
                },
                timestamp,
                windowID: CANVAS,
                which: PEN,
                pen_state: if down {
                    SDL_PEN_INPUT_DOWN
                } else {
                    Default::default()
                },
                down,
                x,
                y: 140.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
}

#[cfg(target_os = "macos")]
fn cocoa_motion(app: &mut App, timestamp: u64, x: f32) {
    app.event(
        &SDL_Event {
            pmotion: SDL_PenMotionEvent {
                r#type: SDL_EVENT_PEN_MOTION,
                timestamp,
                windowID: CANVAS,
                which: PEN,
                pen_state: SDL_PEN_INPUT_DOWN,
                x,
                y: 140.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
}

#[cfg(target_os = "macos")]
#[test]
fn cocoa_pen_down_uses_same_nsevent_motion_instead_of_previous_cached_position() {
    let mut app = super::touch_tests::app();

    // SDL 3.4.20 calls SendPenTouch BEFORE SendPenMotion in one
    // Cocoa_HandlePenPointEvent. Its DOWN coordinates therefore refer
    // to the previous pen sample, not the actual new contact position.
    cocoa_touch(&mut app, 100_000, true, 0.);
    assert!(app.history.active.is_none());
    cocoa_motion(&mut app, 100_000, 30.);
    cocoa_motion(&mut app, 101_000, 85.);
    cocoa_touch(&mut app, 102_000, false, 85.);
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(app.history.strokes()[0].points[0].x, 30.);
    assert_eq!(
        app.history.strokes()[0].points.last().map(|p| p.x),
        Some(85.)
    );

    // The next PEN_DOWN reports 85: that was the previous stroke's end.
    // A following PEN_MOTION with the very same timestamp carries the
    // true fresh contact position at 690. It must START there, not draw
    // a line from 85 to 690.
    cocoa_touch(&mut app, 200_000, true, 85.);
    assert!(app.history.active.is_none());
    cocoa_motion(&mut app, 200_000, 690.);
    let Some(active) = app.history.active.as_ref() else {
        panic!("pen motion must begin the next stroke");
    };
    assert_eq!(active.points.len(), 1);
    assert_eq!(active.points[0].x, 690.);
    cocoa_motion(&mut app, 201_000, 740.);
    cocoa_touch(&mut app, 202_000, false, 740.);

    assert_eq!(app.history.strokes().len(), 2);
    assert!(app.history.strokes()[0].points.iter().all(|p| p.x <= 85.));
    assert!(app.history.strokes()[1].points.iter().all(|p| p.x >= 690.));
}

#[cfg(target_os = "macos")]
#[test]
fn cocoa_pen_axis_can_start_stationary_contact_with_fresh_position() {
    let mut app = super::touch_tests::app();
    cocoa_touch(&mut app, 300_000, true, 50.);
    app.event(
        &SDL_Event {
            paxis: SDL_PenAxisEvent {
                r#type: SDL_EVENT_PEN_AXIS,
                timestamp: 300_000,
                windowID: CANVAS,
                which: PEN,
                pen_state: SDL_PEN_INPUT_DOWN,
                axis: SDL_PEN_AXIS_PRESSURE,
                value: 0.4,
                x: 510.,
                y: 140.,
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
    );
    let Some(active) = app.history.active.as_ref() else {
        panic!("same-timestamp pen axis should start an active stroke");
    };
    assert_eq!(active.points[0].x, 510.);
    assert_eq!(active.points.len(), 1);
    cocoa_touch(&mut app, 301_000, false, 510.);
    assert_eq!(app.history.strokes().len(), 1);
}

#[cfg(target_os = "macos")]
#[test]
fn cocoa_stationary_dot_without_motion_falls_back_to_touch_position() {
    let mut app = super::touch_tests::app();
    cocoa_touch(&mut app, 400_000, true, 44.);
    assert!(app.history.active.is_none());
    cocoa_touch(&mut app, 401_000, false, 44.);
    let [stroke] = app.history.strokes() else {
        panic!("a stationary pen tap must leave one dot");
    };
    assert_eq!(stroke.points.len(), 1);
    assert_eq!(stroke.points[0].x, 44.);
}
