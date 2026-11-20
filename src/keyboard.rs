#![forbid(unsafe_code)]

//! Optional system on-screen keyboard and input-source actions.
//! OverInk's built-in egui keyboard is the default and never depends on these
//! helpers. The native OSK is opt-in only on Windows and Linux Wayland/XWayland.
use crate::sdl::Window;

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
        let path = ["CommonProgramW6432", "CommonProgramFiles"]
            .into_iter()
            .filter_map(env::var_os)
            .map(|root| {
                PathBuf::from(root)
                    .join("microsoft shared")
                    .join("ink")
                    .join("TabTip.exe")
            })
            .find(|path| path.is_file())
            .ok_or("Windows touch keyboard (TabTip.exe) is unavailable")?;
        Command::new(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not launch Windows touch keyboard: {error}"))
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
        // Fcitx5's toggle changes the active system input method without
        // synthesizing application keyboard events or touching IME preedit.
        if std::process::Command::new("fcitx5-remote")
            .arg("-t")
            .output()
            .is_ok_and(|result| result.status.success())
        {
            return Ok(());
        }
        // IBus lists installed engine IDs; cycle using its own daemon. It
        // owns the candidate UI, and OverInk only requests an engine change.
        let current = std::process::Command::new("ibus")
            .arg("engine")
            .output()
            .map_err(|error| format!("No Fcitx5 or IBus input source: {error}"))?;
        let list = std::process::Command::new("ibus")
            .arg("list-engine")
            .output()
            .map_err(|error| format!("Could not enumerate IBus engines: {error}"))?;
        if !current.status.success() || !list.status.success() {
            return Err("Could not query running IBus input sources".into());
        }
        let active = String::from_utf8_lossy(&current.stdout);
        let engines: Vec<&str> = std::str::from_utf8(&list.stdout)
            .map_err(|e| e.to_string())?
            .lines()
            .filter_map(|line| line.trim().split_once(" - ").map(|(name, _)| name.trim()))
            .filter(|name| !name.is_empty())
            .collect();
        if engines.len() < 2 {
            return Err("Configure at least two system input methods".into());
        }
        let index = engines
            .iter()
            .position(|engine| *engine == active.trim())
            .unwrap_or(0);
        let next = engines[(index + 1) % engines.len()];
        let status = std::process::Command::new("ibus")
            .args(["engine", next])
            .status()
            .map_err(|error| error.to_string())?;
        return status
            .success()
            .then_some(())
            .ok_or("IBus could not switch input source".into());
    }
    #[cfg(target_os = "macos")]
    {
        // macOS does not expose a public cross-process input-source picker to
        // ordinary applications. Respect the user's configured Control+Space
        // shortcut through System Events, which requires Accessibility grant.
        let output = std::process::Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "tell application \"System Events\" to key code 49 using control down",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        return output.status.success().then_some(()).ok_or_else(|| {
            format!(
                "Enable Accessibility for input-source switching: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        });
    }
    #[cfg(target_os = "windows")]
    {
        Err("Use Win+Space to change the Windows input language; the OverInk keyboard does not impersonate a hardware input method".into())
    }
}
