#![forbid(unsafe_code)]

//! Optional system on-screen keyboard and input-source actions.
//! OverInk's built-in egui keyboard is the default and never depends on these
//! helpers. The native OSK is opt-in only on Windows and Linux Wayland/XWayland.
use crate::sdl::Window;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod unix_ime;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native_keys;
#[cfg(all(not(test), any(target_os = "windows", target_os = "macos")))]
pub use native_keys::PhysicalKey;

#[cfg(all(not(test), any(target_os = "windows", target_os = "macos")))]
pub fn send_physical_key(key: PhysicalKey) -> Result<(), String> {
    native_keys::send_key(key)
}

#[cfg(all(not(test), any(target_os = "windows", target_os = "macos")))]
pub fn supports_character(ch: char) -> bool {
    native_keys::supports_character(ch)
}

pub fn system_supported(_video: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        _video == "wayland"
            || std::env::var("XDG_SESSION_TYPE")
                .is_ok_and(|session| session.eq_ignore_ascii_case("wayland"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

pub fn show(window: &Window, _video: &str) -> Result<(), String> {
    if !system_supported(_video) {
        return Err("System keyboard is unavailable here; use the OverInk keyboard".into());
    }
    #[cfg(target_os = "windows")]
    {
        use std::{env, path::PathBuf, process::Command};
        // InputPane is associated with the live canvas HWND. Never restart
        // SDL_StartTextInput/StopTextInput: that collapses the active IME pane.
        match crate::native::windows_keyboard::try_show(window) {
            Ok(true) => return Ok(()),
            Ok(false) => (),
            Err(error) => eprintln!("OverInk: system InputPane unavailable: {error}"),
        }
        let tabtip = ["CommonProgramW6432", "CommonProgramFiles"]
            .into_iter()
            .filter_map(env::var_os)
            .map(|root| {
                PathBuf::from(root)
                    .join("microsoft shared")
                    .join("ink")
                    .join("TabTip.exe")
            })
            .find(|path| path.is_file());
        if let Some(path) = tabtip
            && Command::new(path).spawn().is_ok()
        {
            return Ok(());
        }
        // Tablet-mode InputPane can refuse to appear with a hardware
        // keyboard attached. The built-in Windows Accessibility OSK is still
        // a native system keyboard and does not require unsafe API calls.
        Command::new("osk.exe")
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Windows InputPane, TabTip and osk.exe unavailable: {error}"))
    }
    #[cfg(target_os = "linux")]
    {
        if _video == "wayland" {
            window.request_screen_keyboard()?;
            return Ok(());
        }
        // XWayland cannot control GNOME's Wayland OSK through X11. Try
        // optional Onboard; return a real error so callers fall back to
        // OverInk instead of silently claiming that a keyboard appeared.
        std::process::Command::new("onboard")
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("No XWayland system keyboard available: {error}"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = window;
        Err("Use the built-in OverInk keyboard".into())
    }
}

/// The globe selects a *system input method*, not an OverInk keyboard page.
/// The built-in keyboard never implements candidate or preedit windows.
/// Switch through existing IME tools where the platform exposes them.
pub fn switch_input_method() -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        // Fcitx5 and IBus own candidates and native input-source state.
        // No X11 key injection or Wayland-specific permission is needed.
        unix_ime::switch_input_method()
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        // Prefer actual OS input-source shortcuts over committing a synthetic
        // string; only the operating system changes the selected IME.
        // Requires Accessibility permission on macOS, and at least two
        // configured sources. macOS shortcuts can be changed in Settings.
        native_keys::switch_source()
    }
}
