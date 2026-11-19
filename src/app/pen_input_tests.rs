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
