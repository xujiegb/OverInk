//! Regression tests for Apple Pencil / Sidecar's pen-contact transitions.
//! SDL sends motion and axis packets during hover; their full pen_state
//! must end any stale contact before samples can enter an active stroke.
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

fn pressure(app: &mut App, window: SDL_WindowID, down: bool, x: f32, value: f32) {
    app.event(
        &SDL_Event {
            paxis: SDL_PenAxisEvent {
                r#type: SDL_EVENT_PEN_AXIS,
                windowID: window,
                which: PEN,
                pen_state: if down {
                    SDL_PEN_INPUT_DOWN
                } else {
                    Default::default()
                },
                axis: SDL_PEN_AXIS_PRESSURE,
                value,
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
fn sidecar_lost_pen_up_axis_hover_cannot_join_two_strokes() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 20.);
    motion(&mut app, CANVAS, true, 80.);
    assert!(app.history.active.is_some());

    // A release packet was lost. Sidecar delivers a zero-pressure hover
    // update at the position of the next annotation before another PEN_DOWN.
    pressure(&mut app, CANVAS, false, 600., 0.);
    assert!(app.history.active.is_none());
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(app.history.strokes()[0].points.len(), 2);
    motion(&mut app, CANVAS, false, 650.);

    contact(&mut app, CANVAS, true, 680.);
    motion(&mut app, CANVAS, true, 720.);
    contact(&mut app, CANVAS, false, 720.);

    assert_eq!(app.history.strokes().len(), 2);
    assert!(app.history.strokes()[0].points.iter().all(|p| p.x <= 80.));
    assert!(app.history.strokes()[1].points.iter().all(|p| p.x >= 680.));
}

#[test]
fn sidecar_hover_motion_ends_stroke_without_pen_up() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 30.);
    motion(&mut app, CANVAS, true, 90.);

    motion(&mut app, CANVAS, false, 550.);
    assert!(app.history.active.is_none());
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(
        app.history.strokes()[0].points.last().map(|p| p.x),
        Some(90.)
    );
    contact(&mut app, CANVAS, true, 570.);
    contact(&mut app, CANVAS, false, 580.);
    assert_eq!(app.history.strokes().len(), 2);
    assert_eq!(
        app.history.strokes()[1].points.first().map(|p| p.x),
        Some(570.)
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

fn mouse_contact(app: &mut App, window: SDL_WindowID, down: bool, x: f32, button: u8) {
    app.event(
        &SDL_Event {
            button: SDL_MouseButtonEvent {
                r#type: if down {
                    SDL_EVENT_MOUSE_BUTTON_DOWN
                } else {
                    SDL_EVENT_MOUSE_BUTTON_UP
                },
                windowID: window,
                button,
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

fn mouse_motion(app: &mut App, window: SDL_WindowID, state: SDL_MouseButtonFlags, x: f32) {
    app.event(
        &SDL_Event {
            motion: SDL_MouseMotionEvent {
                r#type: SDL_EVENT_MOUSE_MOTION,
                windowID: window,
                state,
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
fn sidecar_mouse_compatibility_hover_never_connects_independent_strokes() {
    let mut app = super::touch_tests::app();
    mouse_contact(&mut app, CANVAS, true, 20., LEFT_BUTTON);
    mouse_motion(&mut app, CANVAS, SDL_BUTTON_LMASK, 90.);
    assert!(app.history.active.is_some());

    // AppKit/SDL dropped the old mouse-up; the pointer moves as a hover,
    // without the left-button state. This event must commit the old stroke
    // rather than paint a line to the new touch-down position.
    mouse_motion(&mut app, CANVAS, SDL_MouseButtonFlags::default(), 690.);
    assert!(app.history.active.is_none());
    assert_eq!(app.history.strokes().len(), 1);
    assert!(
        app.history.strokes()[0]
            .points
            .iter()
            .all(|point| point.x <= 90.)
    );

    mouse_contact(&mut app, CANVAS, true, 720., LEFT_BUTTON);
    mouse_motion(&mut app, CANVAS, SDL_BUTTON_LMASK, 760.);
    mouse_contact(&mut app, CANVAS, false, 760., LEFT_BUTTON);
    assert_eq!(app.history.strokes().len(), 2);
    assert!(
        app.history.strokes()[1]
            .points
            .iter()
            .all(|point| point.x >= 720.)
    );
}

#[test]
fn mouse_right_eraser_continues_only_while_right_button_is_pressed() {
    let mut app = super::touch_tests::app();
    mouse_contact(&mut app, CANVAS, true, 30., RIGHT_BUTTON);
    assert!(app.input.contact.is_some());
    mouse_motion(&mut app, CANVAS, SDL_BUTTON_RMASK, 60.);
    assert!(app.input.contact.is_some());
    mouse_motion(&mut app, CANVAS, SDL_BUTTON_LMASK, 80.);
    assert!(app.input.contact.is_none());
}

#[test]
fn mouse_release_from_another_window_clears_orphaned_contact() {
    let mut app = super::touch_tests::app();
    mouse_contact(&mut app, CANVAS, true, 40., LEFT_BUTTON);
    mouse_motion(&mut app, CANVAS, SDL_BUTTON_LMASK, 90.);
    // The event arrived on the controller, not the drawing canvas. The
    // event-loop invokes this same safe helper on all pages before routing.
    app.release_mouse_if_button_up(SDL_MouseButtonFlags::default());
    assert!(app.history.active.is_none());
    assert_eq!(app.history.strokes().len(), 1);
    assert_eq!(
        app.history.strokes()[0].points.last().map(|point| point.x),
        Some(90.)
    );
    mouse_contact(&mut app, CANVAS, true, 700., LEFT_BUTTON);
    mouse_contact(&mut app, CANVAS, false, 700., LEFT_BUTTON);
    assert_eq!(app.history.strokes().len(), 2);
}

#[cfg(target_os = "macos")]
#[test]
fn sidecar_stale_down_bit_zero_pressure_ends_contact() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 40.);
    pressure(&mut app, CANVAS, true, 70., 0.2);
    motion(&mut app, CANVAS, true, 90.);
    assert!(app.history.active.is_some());

    // Sidecar may retain SDL's DOWN bit even after Cocoa reports pressure 0.
    pressure(&mut app, CANVAS, true, 700., 0.);
    assert!(app.history.active.is_none());
    assert_eq!(app.history.strokes().len(), 1);
    assert!(
        app.history.strokes()[0]
            .points
            .iter()
            .all(|point| point.x <= 90.)
    );
    motion(&mut app, CANVAS, true, 730.);
    assert!(app.history.active.is_none());

    contact(&mut app, CANVAS, true, 750.);
    motion(&mut app, CANVAS, true, 780.);
    contact(&mut app, CANVAS, false, 780.);
    assert_eq!(app.history.strokes().len(), 2);
    assert!(
        app.history.strokes()[1]
            .points
            .iter()
            .all(|point| point.x >= 750.)
    );
}

fn finger_contact(app: &mut App, ty: SDL_EventType, x: f32) {
    app.event(
        &SDL_Event {
            tfinger: SDL_TouchFingerEvent {
                r#type: ty,
                touchID: SDL_TouchID(9),
                fingerID: SDL_FingerID(3),
                windowID: CANVAS,
                x,
                y: 0.2,
                pressure: if ty == SDL_EVENT_FINGER_UP { 0. } else { 1. },
                ..Default::default()
            },
        },
        CANVAS,
        SIZE,
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
fn macos_pen_up_at_distant_stale_position_does_not_draw_connector() {
    let mut app = super::touch_tests::app();
    contact(&mut app, CANVAS, true, 40.);
    motion(&mut app, CANVAS, true, 100.);
    // Positive pen pressure provides additional evidence that a distant
    // release sample with no intervening motion may be stale.
    pressure(&mut app, CANVAS, true, 100., 0.4);
    // SDL receives the release near the next touchdown rather than the old tip.
    contact(&mut app, CANVAS, false, 700.);
    assert_eq!(app.history.strokes().len(), 1);
    assert!(
        app.history.strokes()[0]
            .points
            .iter()
            .all(|point| point.x <= 100.)
    );
    contact(&mut app, CANVAS, true, 750.);
    contact(&mut app, CANVAS, false, 760.);
    assert_eq!(app.history.strokes().len(), 2);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_finger_up_at_distant_stale_position_does_not_draw_connector() {
    let mut app = super::touch_tests::app();
    finger_contact(&mut app, SDL_EVENT_FINGER_DOWN, 0.04);
    finger_contact(&mut app, SDL_EVENT_FINGER_MOTION, 0.10);
    finger_contact(&mut app, SDL_EVENT_FINGER_UP, 0.70);
    assert_eq!(app.history.strokes().len(), 1);
    assert!(
        app.history.strokes()[0]
            .points
            .iter()
            .all(|point| point.x <= 100.)
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
fn cocoa_touch(app: &mut App, timestamp: u64, down: bool, x: f32) {
    app.event(
        &SDL_Event {
            ptouch: SDL_PenTouchEvent {
                r#type: if down { SDL_EVENT_PEN_DOWN } else { SDL_EVENT_PEN_UP },
                timestamp,
                windowID: CANVAS,
                which: PEN,
                pen_state: if down { SDL_PEN_INPUT_DOWN } else { Default::default() },
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
    assert_eq!(app.history.strokes()[0].points.last().map(|p| p.x), Some(85.));

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
