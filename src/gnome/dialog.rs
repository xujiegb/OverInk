// SPDX-License-Identifier: BSD-3-Clause
use super::{Decision, Guide};
use crate::{
    appearance::{Language, Preferences, ThemeMonitor},
    gui::Gui,
    icc::ColorManager,
    sdl::*,
};
use std::ptr;

const WIDTH: i32 = 480;
const HEIGHT: i32 = 180;

pub(super) fn run(
    mut guide: Guide,
    preferences: Preferences,
) -> Result<(Decision, Runtime), String> {
    // The guide is a normal toplevel and can open even without XWayland.
    let runtime = Runtime::new(false, true, false).or_else(|wayland| {
        Runtime::new(true, false, false).map_err(|x11| format!("{wayland} · {x11}"))
    })?;
    let window = Window::new("OverInk — GNOME Wayland", WIDTH, HEIGHT, false)?;
    // SDL's X11 window coordinates are pixels, while egui uses points.
    let units = window.ui_scale();
    if units > 1. && driver() == "x11" {
        unsafe {
            check(SDL_SetWindowSize(
                window.raw,
                (WIDTH as f32 * units).round() as i32,
                (HEIGHT as f32 * units).round() as i32,
            ))?;
        }
    }
    let renderer = Renderer::new(&window, true).or_else(|_| Renderer::software(&window))?;
    let mut gui = Gui::new();
    let language = preferences.language.unwrap_or_else(Language::system);
    gui.set_language(language);
    let mut appearance = ThemeMonitor::default();
    let mut color = ColorManager::default();
    color.refresh(&window, renderer.extended);
    window.show(true)?;
    unsafe { SDL_RaiseWindow(window.raw) };
    let decision = 'dialog: loop {
        let mut event = SDL_Event::default();
        while unsafe { SDL_PollEvent(&mut event) } {
            match unsafe { SDL_EventType(event.r#type) } {
                SDL_EVENT_WINDOW_CLOSE_REQUESTED => {
                    // SDL also queues a quit for the last window. The main
                    // app can reuse this runtime when the guide ran on X11.
                    unsafe { SDL_FlushEvent(SDL_EVENT_QUIT.0) };
                    break 'dialog close_decision(&guide);
                }
                SDL_EVENT_QUIT => break 'dialog close_decision(&guide),
                SDL_EVENT_KEY_DOWN
                    if unsafe { event.key.key == SDLK_ESCAPE && !event.key.repeat } =>
                {
                    break 'dialog close_decision(&guide);
                }
                _ => gui.event(&event, window.id, window.size()),
            }
        }
        let theme = appearance.theme(preferences.theme);
        gui.ctx.set_theme(theme);
        let mut action = None;
        let input = gui.input(&window);
        let output = gui.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::central_panel(ui.style()).inner_margin(18.))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.heading("GNOME Wayland");
                        ui.add_space(4.);
                        action = guide.ui(ui, language, true);
                    });
                });
        });
        let delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(1000, |viewport| {
                viewport.repaint_delay.as_millis().min(1000)
            });
        let background = gui.ctx.style_of(theme).visuals.panel_fill.to_array();
        renderer.begin(&window, background.map(|value| f32::from(value) / 255.))?;
        gui.paint(&renderer, &window, &color, output)?;
        renderer.present()?;
        if let Some(action) = action {
            if let Some(decision) = guide.act(action) {
                break decision;
            }
        } else if delay != 0 {
            unsafe { SDL_WaitEventTimeout(ptr::null_mut(), delay as i32) };
        }
    };
    Ok((decision, runtime))
}

fn close_decision(guide: &Guide) -> Decision {
    if matches!(guide, Guide::Relogin) {
        Decision::Relogin
    } else {
        Decision::XWayland
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gnome::Action;

    #[test]
    fn closing_after_installation_keeps_native_wayland_enabled() {
        let mut guide = Guide::Relogin;
        assert_eq!(Some(close_decision(&guide)), guide.act(Action::Proceed));
        assert_eq!(guide.act(Action::Cancel), Some(Decision::XWayland));
        assert_eq!(close_decision(&Guide::Install), Decision::XWayland);
        assert_eq!(close_decision(&Guide::Update), Decision::XWayland);
    }
}
