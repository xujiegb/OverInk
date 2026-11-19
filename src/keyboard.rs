#![forbid(unsafe_code)]

//! On-screen keyboard requests without changing desktop accessibility settings.
//! GNOME and KDE Wayland keyboards belong to the compositor, not an X11 process.
use crate::sdl::Window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyboardRequest {
    /// Refocus text input (including Cocoa touch-to-mouse emulation).
    Focus,
    /// A direct SDL finger gesture requested keyboard entry.
    Touch,
    /// The annotation's explicit keyboard button was pressed.
    Button,
}

pub(crate) fn request(window: &Window, kind: KeyboardRequest) -> Result<(), String> {
    // An active IME can coexist with a dismissed OSK. Reissue SDL input
    // only for a deliberate edit gesture, not every frame. An actual touch
    // or explicit button forces the keyboard hint for this one request;
    // SDL's "auto" otherwise suppresses it when hardware keys are attached.
    match kind {
        KeyboardRequest::Focus => {
            // macOS 27's touch compatibility can arrive as an ordinary
            // mouse click. Without new native touch hooks, treat a second
            // click into a selected annotation as an explicit OSK request.
            if cfg!(target_os = "macos") {
                window.request_screen_keyboard()
            } else {
                window.sync_text_input(false);
                window.sync_text_input(true);
                Ok(())
            }
        }
        KeyboardRequest::Touch | KeyboardRequest::Button => {
            window.request_screen_keyboard()?;
            launch_os_keyboard(kind)
        }
    }
}

#[cfg(target_os = "windows")]
fn launch_os_keyboard(_kind: KeyboardRequest) -> Result<(), String> {
    // Prefer Windows Touch Keyboard and fall back to the built-in accessible
    // OSK. No undocumented COM interfaces or registry changes.
    let path = std::env::var_os("CommonProgramFiles")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("Microsoft Shared").join("Ink").join("TabTip.exe"));
    if let Some(path) = path
        && path.is_file()
        && std::process::Command::new(path).spawn().is_ok()
    {
        return Ok(());
    }
    std::process::Command::new("osk.exe")
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Cannot open Windows screen keyboard: {error}"))
}

#[cfg(target_os = "macos")]
fn launch_os_keyboard(kind: KeyboardRequest) -> Result<(), String> {
    if kind != KeyboardRequest::Button {
        // On touch-capable macOS 27, leave keyboard presentation to the
        // native text-input session rather than launch an accessibility app.
        return Ok(());
    }
    // Manual fallback also works for users of macOS 11–26; don't change the
    // user's Accessibility Keyboard preference behind their back.
    std::process::Command::new("/usr/bin/open")
        .args(["-b", "com.apple.inputmethod.AssistiveControl"])
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Cannot launch Accessibility Keyboard: {error}"))
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn launch_os_keyboard(_kind: KeyboardRequest) -> Result<(), String> {
    if crate::sdl::driver() == "wayland" {
        // GNOME Mutter and KDE KWin own the Wayland input-method-v3 OSK.
        // SDL_StartTextInput above requests it; launching an X11 keyboard
        // or altering GNOME/KDE preferences cannot do the same thing.
        return Ok(());
    }
    for program in ["onboard", "matchbox-keyboard", "florence"] {
        if std::process::Command::new(program).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("Install an on-screen keyboard (Onboard, Matchbox or Florence) for X11".into())
}

#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd"
)))]
fn launch_os_keyboard(_kind: KeyboardRequest) -> Result<(), String> {
    Ok(())
}
