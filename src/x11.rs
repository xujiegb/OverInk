#![forbid(unsafe_code)]

use crate::controller::{Rect, outside};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
use x11rb::{
    connection::Connection,
    protocol::{
        shape::{ConnectionExt as _, SK, SO},
        xproto::{
            AtomEnum, ClipOrdering, ConfigureWindowAux, ConnectionExt as _, PropMode, Rectangle,
            StackMode,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

thread_local! {
    static CONNECTION: RefCell<Weak<RustConnection>> = const { RefCell::new(Weak::new()) };
}

/// Share one X11 protocol connection between the SDL thread's RandR, Shape
/// and capture operations. Let the last user close it with the SDL session.
pub fn connection() -> Result<Rc<RustConnection>, String> {
    CONNECTION.with_borrow_mut(|cached| {
        if let Some(connection) = cached.upgrade() {
            return Ok(connection);
        }
        let (connection, _) = RustConnection::connect(None)
            .map_err(|error| format!("Cannot connect to the X11 display: {error}"))?;
        let connection = Rc::new(connection);
        *cached = Rc::downgrade(&connection);
        Ok(connection)
    })
}

pub fn forget_connection(connection: &Rc<RustConnection>) {
    CONNECTION.with_borrow_mut(|cached| {
        if cached.ptr_eq(&Rc::downgrade(connection)) {
            *cached = Weak::new();
        }
    });
}

pub fn cached_connection() -> Option<Rc<RustConnection>> {
    CONNECTION.with_borrow(Weak::upgrade)
}

pub fn connection_is_cached(connection: &Rc<RustConnection>) -> bool {
    CONNECTION.with_borrow(|cached| cached.ptr_eq(&Rc::downgrade(connection)))
}

/// Request compositing for an alpha overlay before the window is mapped.
pub fn keep_composited(id: u32) -> Result<(), String> {
    let connection = connection()?;
    let atom = connection
        .intern_atom(false, b"_NET_WM_BYPASS_COMPOSITOR")
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?
        .atom;
    connection
        .change_property32(PropMode::REPLACE, id, atom, AtomEnum::CARDINAL, &[2])
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| format!("Cannot request X11 overlay compositing: {error}"))
}

pub fn shape_connection() -> Result<Rc<RustConnection>, String> {
    let connection = connection()?;
    let version = connection
        .shape_query_version()
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
    if (version.major_version, version.minor_version) < (1, 1) {
        return Err("X11 input shapes require Shape 1.1".into());
    }
    Ok(connection)
}

pub fn set_input(
    connection: &RustConnection,
    id: u32,
    size: [i32; 2],
    pass: bool,
    hole: Option<Rect>,
) -> Result<(), String> {
    let request = if pass || hole.is_some() {
        let mut rectangles = [Rectangle::default(); 4];
        let mut count = 0;
        if let Some(hole) = hole {
            for area in outside(size, Some(hole)) {
                rectangles[count] = Rectangle {
                    x: area.x.try_into().map_err(|_| "Input region is too large")?,
                    y: area.y.try_into().map_err(|_| "Input region is too large")?,
                    width: area.w.try_into().map_err(|_| "Input region is too large")?,
                    height: area.h.try_into().map_err(|_| "Input region is too large")?,
                };
                count += 1;
            }
        }
        connection.shape_rectangles(
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            id,
            0,
            0,
            &rectangles[..count],
        )
    } else {
        connection.shape_mask(SO::SET, SK::INPUT, id, 0, 0, x11rb::NONE)
    };
    request
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| format!("Cannot set the X11 input shape: {error}"))
}

pub fn raise(connection: &RustConnection, id: u32) {
    let order = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
    if let Ok(request) = connection.configure_window(id, &order) {
        // Keep focus and avoid a round trip on each drawing gesture.
        request.ignore_error();
        let _ = connection.flush();
    }
}

pub fn set_rounded_input(id: u32, [width, height]: [i32; 2], radius: i32) -> Result<(), String> {
    if width <= 0 || height <= 0 || radius <= 0 {
        return Err("Invalid X11 toolbar geometry".into());
    }
    let radius = radius.min(width / 2).min(height / 2);
    let mut spans = Vec::new();
    let mut row = |x, y, width, height| -> Result<(), String> {
        spans.push(Rectangle {
            x: i16::try_from(x).map_err(|_| "Toolbar shape is too large")?,
            y: i16::try_from(y).map_err(|_| "Toolbar shape is too large")?,
            width: u16::try_from(width).map_err(|_| "Toolbar shape is too large")?,
            height: u16::try_from(height).map_err(|_| "Toolbar shape is too large")?,
        });
        Ok(())
    };
    let r = f64::from(radius);
    for y in 0..radius {
        let vertical = r - (f64::from(y) + 0.5);
        let edge = r - (r * r - vertical * vertical).sqrt();
        // The previous mask included any edge pixel with nonzero alpha.
        let x = edge.floor() as i32;
        row(x, y, width - x * 2, 1)?;
        row(x, height - y - 1, width - x * 2, 1)?;
    }
    if height > 2 * radius {
        row(0, radius, width, height - 2 * radius)?;
    }
    // The egui toolbar already paints its rounded outline. Set just the
    // native hit region, avoiding SDL's pixmap and per-present shape texture.
    connection()?
        .shape_rectangles(SO::SET, SK::INPUT, ClipOrdering::UNSORTED, id, 0, 0, &spans)
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| format!("Cannot set the X11 toolbar shape: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{CreateWindowAux, WindowClass};

    #[test]
    #[ignore = "requires an isolated X11 display with the Shape extension"]
    fn x11_live_input_shape_handles_drawing_and_passthrough()
    -> Result<(), Box<dyn std::error::Error>> {
        let connection = shape_connection()?;
        let shared = super::connection()?;
        assert!(Rc::ptr_eq(&connection, &shared));
        let root = connection.setup().roots.first().ok_or("No X11 screen")?;
        let id = connection.generate_id()?;
        connection
            .create_window(
                root.root_depth,
                id,
                root.root,
                0,
                0,
                800,
                600,
                0,
                WindowClass::INPUT_OUTPUT,
                root.root_visual,
                &CreateWindowAux::new(),
            )?
            .check()?;
        keep_composited(id)?;
        let atom = connection
            .intern_atom(false, b"_NET_WM_BYPASS_COMPOSITOR")?
            .reply()?
            .atom;
        let property = connection
            .get_property(false, id, atom, AtomEnum::CARDINAL, 0, 1)?
            .reply()?;
        assert_eq!(
            property.value32().and_then(|mut values| values.next()),
            Some(2)
        );

        let bounds = [800, 600];
        let hole = Rect {
            x: 4,
            y: 5,
            w: 100,
            h: 120,
        };
        set_input(&connection, id, bounds, true, None)?;
        assert!(
            connection
                .shape_get_rectangles(id, SK::INPUT)?
                .reply()?
                .rectangles
                .is_empty()
        );
        set_input(&connection, id, bounds, false, Some(hole))?;
        let rects = connection
            .shape_get_rectangles(id, SK::INPUT)?
            .reply()?
            .rectangles;
        let area: i32 = rects
            .iter()
            .map(|rect| i32::from(rect.width) * i32::from(rect.height))
            .sum();
        assert_eq!(area, bounds[0] * bounds[1] - hole.w * hole.h);
        set_input(&connection, id, bounds, false, None)?;
        let rects = connection
            .shape_get_rectangles(id, SK::INPUT)?
            .reply()?
            .rectangles;
        assert_eq!(rects.len(), 1);
        assert_eq!([rects[0].width, rects[0].height], [800, 600]);
        set_rounded_input(id, bounds, 12)?;
        let rects = connection
            .shape_get_rectangles(id, SK::INPUT)?
            .reply()?
            .rectangles;
        assert!(rects.iter().any(|rect| rect.y == 0 && rect.x > 0));
        assert!(rects.iter().any(|rect| rect.x == 0 && rect.width == 800));
        connection.destroy_window(id)?.check()?;
        assert!(set_input(&connection, id, bounds, true, None).is_err());
        Ok(())
    }
}
