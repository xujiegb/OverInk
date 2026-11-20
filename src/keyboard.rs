#![forbid(unsafe_code)]

//! Request the *system* touch keyboard without an additional dependency.
//! SDL handles IME/text-input state; desktop-specific launchers are used only
//! where the compositor does not provide an OSK request protocol.
use crate::sdl::Window;

pub fn show(window: &Window, _video: &str) -> Result<(), String> {
    // The canvas editor has already activated SDL text input. Only Wayland
    // needs SDL's on-screen-keyboard hint: restarting SDL text input on Win32
    // immediately tears down the same TSF/IME session the touch keyboard uses,
    // and also interferes with macOS's independently launched Keyboard Viewer.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    window.request_screen_keyboard()?;

    #[cfg(target_os = "windows")]
    {
        use std::{env, path::PathBuf, process::Command};
        // TabTip is the modern Windows touch/handwriting panel, not the
        // legacy osk.exe accessibility keyboard. Language and handwriting
        // layout remain under the user's Windows input-method settings.
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
            .map_err(|error| format!("Could not open Windows touch keyboard: {error}"))
    }

    #[cfg(target_os = "macos")]
    {
        show_macos_keyboard()
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

/// Run Apple's Keyboard Viewer in the foreground. Its UI is a separate
/// process: the SDL text editor must remain alive if the Viewer briefly owns
/// AppKit focus. Never use `open -g` (background-only), nor treat a successful
/// `spawn()` as evidence that Launch Services actually launched the viewer.
#[cfg(target_os = "macos")]
fn show_macos_keyboard() -> Result<(), String> {
    use std::{path::Path, process::Command};
    let app = Path::new("/System/Library/Input Methods/KeyboardViewer.app");
    if !app.is_dir() {
        return Err(
            "macOS Keyboard Viewer is not installed at its expected system path; enable Show Keyboard Viewer in System Settings > Keyboard > Text Input"
                .into(),
        );
    }
    // A closed Keyboard Viewer can leave a headless instance running; open
    // -n requests a fresh window rather than sending a no-op open to it.
    // A system policy may prohibit new instances, so fall back to activate.
    let mut failures = Vec::new();
    for fresh_instance in [true, false] {
        let mut command = Command::new("/usr/bin/open");
        if fresh_instance {
            command.arg("-n");
        }
        command.arg("-a").arg(app);
        match command.output() {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => failures.push(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ),
            Err(error) => failures.push(error.to_string()),
        }
    }
    Err(format!(
        "macOS could not activate Keyboard Viewer: {}",
        failures.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("; ")
    ))
}
