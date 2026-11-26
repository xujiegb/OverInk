//! SDL3 file dialog ABI boundary. SDL owns callback path storage and borrows
//! the initial location for the duration of the native show call.
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
use super::{
    Window,
    dialog::{DialogKind, use_window_parent},
};
#[cfg(any(test, not(any(target_os = "linux", target_os = "freebsd"))))]
use super::{dialog::complete, error};
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
use sdl3_sys::dialog::*;
use std::{ffi::CStr, path::PathBuf};
#[cfg(any(test, not(any(target_os = "linux", target_os = "freebsd"))))]
use std::ffi::{c_char, c_int, c_void};
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
use std::{ffi::CString, path::Path, ptr};

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
const PNG_FILTER: &SDL_DialogFileFilter = &SDL_DialogFileFilter {
    name: c"PNG".as_ptr(),
    pattern: c"png".as_ptr(),
};

/// Called from SDL, potentially on a different thread or before show returns.
/// Userdata is an opaque numeric ID, never a pointer to Rust-owned storage.
#[cfg(any(test, not(any(target_os = "linux", target_os = "freebsd"))))]
pub(super) extern "C" fn chosen(user: *mut c_void, files: *const *const c_char, _filter: c_int) {
    complete(user.addr(), || {
        if files.is_null() {
            return Err(error());
        }
        // SAFETY: SDL guarantees that a non-null result list points to at
        // least one C pointer, terminated by a null pointer, for this callback.
        let file = unsafe { *files };
        if file.is_null() {
            return Ok(None);
        }
        // SAFETY: The selected path is NUL-terminated and borrowed from SDL
        // for the duration of this callback. path_from_c copies it immediately.
        path_from_c(unsafe { CStr::from_ptr(file) }).map(Some)
    });
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
pub(super) fn show(id: usize, kind: DialogKind, window: &Window, location: Option<&Path>) {
    // No pointer to owned Rust state survives the SDL callback. An ID is
    // encoded as pointer metadata but is only converted back to an integer.
    let user = ptr::without_provenance_mut(id);
    let location = location.and_then(location_to_c);
    let initial = location.as_ref().map_or(ptr::null(), |path| path.as_ptr());
    let parent = if use_window_parent(kind) {
        window.raw.as_ptr()
    } else {
        ptr::null_mut()
    };
    // SAFETY: SDL borrows the initial C string only during this call.
    // The filter uses static storage valid through callback completion.
    // The parent is a live SDL Window pointer or null. Screenshot-save
    // dialogs are unparented so the fullscreen canvas can be hidden while
    // the chooser is open, including GNOME Wayland and XWayland.
    unsafe {
        match kind {
            DialogKind::ScreenshotFolder => {
                SDL_ShowOpenFolderDialog(Some(chosen), user, parent, initial, false)
            }
            DialogKind::ScreenshotFile => {
                SDL_ShowSaveFileDialog(Some(chosen), user, parent, PNG_FILTER, 1, initial)
            }
        }
    }
}

pub(super) fn downloads() -> Option<PathBuf> {
    use sdl3_sys::filesystem::{SDL_FOLDER_DOWNLOADS, SDL_GetUserFolder};

    // SAFETY: SDL returns a borrowed, optional NUL-terminated string owned by
    // SDL itself. The returned path is copied; SDL_GetUserFolder is not freed.
    unsafe {
        let path = SDL_GetUserFolder(SDL_FOLDER_DOWNLOADS);
        if path.is_null() {
            return None;
        }
        path_from_c(CStr::from_ptr(path)).ok()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn location_to_c(path: &Path) -> Option<CString> {
    #[cfg(any(windows, target_os = "macos"))]
    let bytes = path.to_str()?.as_bytes();
    #[cfg(not(any(windows, target_os = "macos")))]
    let bytes = path.as_os_str().as_encoded_bytes();
    CString::new(bytes).ok()
}

pub(super) fn path_from_c(path: &CStr) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
    }
    #[cfg(not(unix))]
    {
        path.to_str()
            .map(PathBuf::from)
            .map_err(|error| format!("Invalid UTF-8 file dialog path: {error}"))
    }
}
