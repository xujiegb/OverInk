#![forbid(unsafe_code)]

//! Request the *system* touch keyboard without an additional dependency.
//! SDL handles IME/text-input state; desktop-specific launchers are used only
//! where the compositor does not provide an OSK request protocol.
use crate::sdl::Window;

pub fn show(window: &Window, _video: &str) -> Result<(), String> {
    // Keep Unicode composition on the text canvas regardless of keyboard UI.
    window.request_screen_keyboard()?;

    #[cfg(target_os = "windows")]
    {
        use std::{env, path::PathBuf, process::Command};
        // TabTip is the modern Windows touch/handwriting panel, not the
        // legacy osk.exe accessibility keyboard. Language and handwriting
        // layout remain under the user's Windows input-method settings.
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
        // On Windows 11, TabTip.exe is often already running. Spawning it
        // again does not guarantee the keyboard is displayed. Ask Windows
        // to show the input pane for the actual SDL canvas HWND first.
        // Windows may refuse (e.g. with a physical keyboard connected), so
        // retain TabTip as a best-effort fallback.
        match crate::native::windows_keyboard::try_show(window) {
            Ok(true) => return Ok(()),
            Ok(false) => (),
            Err(error) => eprintln!("OverInk: InputPane TryShow unavailable: {error}"),
        }
        Command::new(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open Windows touch keyboard: {error}"))
    }

    #[cfg(target_os = "macos")]
    {
        use std::{path::Path, process::Command};
        // Keyboard Viewer is the supported system-provided keyboard UI.
        // -g keeps the native annotation editor in the foreground for IME.
        let app = Path::new("/System/Library/Input Methods/KeyboardViewer.app");
        if !app.is_dir() {
            return Err("macOS Keyboard Viewer is unavailable".into());
        }
        Command::new("/usr/bin/open")
            .arg("-g")
            .arg("-a")
            .arg(app)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open macOS Keyboard Viewer: {error}"))
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        if _video == "wayland" {
            // SDL's Wayland text-input-v3 request above activates the
            // compositor-managed OSK, if the session has one configured.
            // There is no universal GNOME/KDE API to force its visibility.
            return Ok(());
        }
        // GNOME/KDE on Xorg (and FreeBSD X11) have no compositor-managed
        // text-input-v3 panel. Onboard is an optional user-installed OSK.
        std::process::Command::new("onboard")
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                format!("Could not start Onboard (install the X11 on-screen keyboard): {error}")
            })
    }
}
