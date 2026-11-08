use crate::{model::History, sdl::*};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CStr,
};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod x11;

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub bounds: [i32; 4],
    pub refresh: f64,
    pub scale: f32,
}
pub fn connected() -> Result<Vec<Display>, String> {
    unsafe {
        let mut count = 0;
        let list = SDL_GetDisplays(&mut count);
        if list.is_null() {
            return Err(crate::sdl::error());
        }
        let result = (0..count.max(0) as usize)
            .map(|i| {
                let id = *list.add(i);
                let mut bounds = SDL_Rect::default();
                if !SDL_GetDisplayBounds(id, &mut bounds) {
                    return Err(crate::sdl::error());
                }
                let name = SDL_GetDisplayName(id);
                let mode = SDL_GetCurrentDisplayMode(id);
                let refresh = if mode.is_null() {
                    0.0
                } else if (*mode).refresh_rate_numerator > 0 && (*mode).refresh_rate_denominator > 0
                {
                    (*mode).refresh_rate_numerator as f64 / (*mode).refresh_rate_denominator as f64
                } else {
                    (*mode).refresh_rate as f64
                };
                Ok(Display {
                    id: id.0,
                    name: if name.is_null() {
                        format!("Display {}", i + 1)
                    } else {
                        CStr::from_ptr(name).to_string_lossy().into_owned()
                    },
                    bounds: [bounds.x, bounds.y, bounds.w, bounds.h],
                    refresh,
                    scale: SDL_GetDisplayContentScale(id),
                })
            })
            .collect::<Result<Vec<_>, String>>();
        SDL_free(list.cast());
        let result = result?;
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let result = {
            let mut result = result;
            if crate::sdl::driver() == "x11"
                && let Some(areas) = x11::active_areas()
            {
                result.retain(|d| areas.contains(&d.bounds));
            }
            result
        };
        Ok(result)
    }
}

// SDL instance IDs survive list reordering, and are never reused during a session.
#[derive(Default)]
pub struct Pages {
    pub active: Option<u32>,
    parked: BTreeMap<u32, History>,
}
impl Pages {
    pub fn new(active: Option<u32>) -> Self {
        Self {
            active,
            parked: BTreeMap::new(),
        }
    }
    pub fn switch(&mut self, history: &mut History, id: u32) -> bool {
        if self.active == Some(id) {
            return false;
        }
        if let Some(previous) = self.active {
            self.parked.insert(previous, std::mem::take(history));
        }
        *history = self.parked.remove(&id).unwrap_or_default();
        history.changed = true;
        self.active = Some(id);
        true
    }
    pub fn retain(&mut self, history: &mut History, connected: &BTreeSet<u32>) -> bool {
        self.parked.retain(|id, _| connected.contains(id));
        if self.active.is_some_and(|id| !connected.contains(&id)) {
            *history = History::default();
            history.changed = true;
            self.active = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        color::InkColor,
        model::{Point, Stroke, Tool},
    };
    fn draw(history: &mut History, x: f32) {
        history.active = Some(Stroke {
            tool: Tool::Pen,
            color: InkColor::default(),
            width: 5.0,
            opacity: 1.0,
            pressure_curve: 1.0,
            pressure_enabled: true,
            points: vec![Point::new(x, 10.0, 0.5, [0.0; 2])],
        });
        history.commit();
    }
    #[test]
    fn switching_screens_keeps_separate_ink_and_undo() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        assert!(pages.switch(&mut history, 20));
        assert!(history.doc.strokes.is_empty());
        draw(&mut history, 22.0);
        pages.switch(&mut history, 10);
        assert_eq!(history.doc.strokes[0].points[0].x, 11.0);
        history.undo();
        assert!(history.doc.strokes.is_empty());
        pages.switch(&mut history, 20);
        assert_eq!(history.doc.strokes[0].points[0].x, 22.0);
    }
    #[test]
    fn disconnect_drops_active_and_parked_ink_including_undo_redo() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        pages.switch(&mut history, 20);
        draw(&mut history, 22.0);
        assert!(pages.retain(&mut history, &BTreeSet::new()));
        assert!(history.doc.strokes.is_empty());
        history.undo();
        history.redo();
        assert!(history.doc.strokes.is_empty());
        pages.switch(&mut history, 10);
        history.undo();
        history.redo();
        assert!(history.doc.strokes.is_empty());
    }
    #[test]
    fn removing_an_inactive_screen_preserves_the_active_page() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        pages.switch(&mut history, 20);
        draw(&mut history, 22.0);
        assert!(!pages.retain(&mut history, &BTreeSet::from([20])));
        assert_eq!(history.doc.strokes[0].points[0].x, 22.0);
        pages.switch(&mut history, 10);
        assert!(history.doc.strokes.is_empty());
    }
}
