#![forbid(unsafe_code)]

//! Request an on-screen keyboard without changing a desktop user's settings.
//! Wayland OSKs are compositor-owned; native SDL text input is the supported
//! path on both GNOME and KDE. Desktop fallbacks are opt-in on a tap/button.
use crate::sdl::Window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyboardRequest {
    /// Refocus text input (including Cocoa's touch-to-mouse emulation).
    Focus,
    /// A real SDL finger gesture requested keyboard entry.
    Touch,
    /// The user pressed the annotation's keyboard button.
    Button,
}

pub(crate) fn request(window: &Window, kind: KeyboardRequest) -> Result<(), String> {
    // SDL_StartTextInput is intentionally reissued: the editor may already
    // have an active IME but its OSK was dismissed while retaining focus.
    // This is invoked only on an intentional keyboard request, never per frame.
    window.sync_text_input(false);
    window.sync_text_input(true);

    if kind == KeyboardRequest::Focus {
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        // Prefer the touch keyboard, falling back to the built-in accessible
        // OSK. No undocumented COM interfaces or registry modifications.
        let path = std::env::var_os("CommonProgramFiles")
            .map(std::path::PathBuf::from)
            .map(|root| root.join("Microsoft Shared").join("Ink").join("TabTip.exe"));
        if let Some(path) = path
            && path.is_file()
            && std::process::Command::new(path).spawn().is_ok()
        {
            return Ok(());
        }
        return std::process::Command::new("osk.exe")
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Cannot open Windows screen keyboard: {error}"));
    }

    #[cfg(target_os = "macos")]
    {
        if kind == KeyboardRequest::Button {
            // macOS 27 touch keyboards should first be requested through the
            // focused native text-input session above. The accessibility
            // keyboard provides a manual option on older macOS releases.
            return std::process::Command::new("/usr/bin/open")
                .args(["-b", "com.apple.inputmethod.AssistiveControl"])
                .spawn()
                .map(|_| ())
                .map_err(|error| format!("Cannot launch Accessibility Keyboard: {error}"));
        }
        return Ok(());
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        if crate::sdl::driver() == "wayland" {
            // Mutter (GNOME) and KWin (KDE) manage their own input-method-v3
            // keyboard. Do not spawn an X11 keyboard into a Wayland session or
            // modify the user's desktop settings.
            return Ok(());
        }
        for program in ["onboard", "matchbox-keyboard", "florence"] {
            if std::process::Command::new(program).spawn().is_ok() {
                return Ok(());
            }
        }
        return Err("Install an on-screen keyboard (Onboard, Matchbox or Florence) for X11".into());
    }

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd"
    )))]
    {
        Ok(())
    }
}
