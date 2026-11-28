use super::dialog::{DialogKind, complete};
use sdl3::{
    dialog::{
        DialogCallback, DialogError, DialogFileFilter, show_open_folder_dialog,
        show_save_file_dialog,
    },
    video::Window,
};
use std::{
    ffi::CStr,
    path::{Path, PathBuf},
};

pub(super) fn show(id: usize, kind: DialogKind, location: Option<&Path>) {
    // SDL accepts UTF-8 paths. Treat an unrepresentable suggestion as absent.
    let location = location.filter(|path| path.to_str().is_some_and(|path| !path.contains('\0')));
    let callback: DialogCallback =
        Box::new(move |result, _| complete(id, || selected_path(result)));
    // The fullscreen selection is hidden before opening the unparented chooser.
    let parent = None::<&Window>;
    match kind {
        DialogKind::ScreenshotFolder => show_open_folder_dialog(location, false, parent, callback),
        DialogKind::ScreenshotFile => {
            let filters = [DialogFileFilter {
                name: "PNG",
                pattern: "png",
            }];
            if let Err(error) = show_save_file_dialog(&filters, location, parent, callback) {
                complete(id, || Err(error.to_string()));
            }
        }
    }
}

fn selected_path(result: Result<Vec<PathBuf>, DialogError>) -> Result<Option<PathBuf>, String> {
    match result {
        Ok(paths) => Ok(paths.into_iter().next()),
        Err(DialogError::Canceled) => Ok(None),
        Err(error) => Err(error.to_string()),
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
        let path = CStr::from_ptr(path);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
        }
        #[cfg(not(unix))]
        path.to_str().ok().map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_native_dialog_selection_cancellation_and_error() -> Result<(), &'static str> {
        assert_eq!(
            selected_path(Ok(vec![PathBuf::from("截图.png")])),
            Ok(Some(PathBuf::from("截图.png")))
        );
        assert_eq!(selected_path(Err(DialogError::Canceled)), Ok(None));
        let error = std::ffi::CString::new([0])
            .err()
            .ok_or("Expected an invalid filter string")?;
        assert!(selected_path(Err(DialogError::FilterError(error))).is_err());
        Ok(())
    }
}
