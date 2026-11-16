use super::{Window, error};
use sdl3_sys::dialog::*;
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    ptr,
    sync::{Mutex, MutexGuard},
};

#[derive(Clone, Copy)]
pub enum DialogKind {
    Font,
    ScreenshotFolder,
    ScreenshotFile,
}

type ResultPath = Result<Option<PathBuf>, String>;

// Cocoa can access the SDL parent from the sheet completion handler, and
// Windows can look up its native handle from a worker thread.
pub const USE_WINDOW_PARENT: bool = !cfg!(any(windows, target_os = "macos"));

const PNG_FILTER: &SDL_DialogFileFilter = &SDL_DialogFileFilter {
    name: c"PNG".as_ptr(),
    pattern: c"png".as_ptr(),
};

struct Callbacks {
    last_id: usize,
    results: BTreeMap<usize, Option<ResultPath>>,
}

static CALLBACKS: Mutex<Callbacks> = Mutex::new(Callbacks {
    last_id: 0,
    results: BTreeMap::new(),
});

fn callbacks() -> MutexGuard<'static, Callbacks> {
    CALLBACKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
pub struct FileDialog(Option<(usize, DialogKind)>);

impl FileDialog {
    pub fn pending(&self) -> bool {
        self.0.is_some()
    }

    fn begin(&mut self, kind: DialogKind) -> Option<usize> {
        if self.pending() {
            return None;
        }
        let mut callbacks = callbacks();
        let id = callbacks
            .last_id
            .checked_add(1)
            .expect("File dialog IDs exhausted");
        callbacks.last_id = id;
        callbacks.results.insert(id, None);
        self.0 = Some((id, kind));
        Some(id)
    }

    pub fn open(&mut self, kind: DialogKind, window: &Window, location: Option<&Path>) -> bool {
        let Some(id) = self.begin(kind) else {
            return false;
        };
        // SDL can call back during this call or from another thread. Userdata
        // is an opaque ID with no borrowed Rust storage.
        let user = ptr::without_provenance_mut(id);
        let location = location.and_then(location_to_c);
        let initial = location.as_ref().map_or(ptr::null(), |path| path.as_ptr());
        let parent = if USE_WINDOW_PARENT {
            window.raw
        } else {
            ptr::null_mut()
        };
        // SDL copies the initial location during the call. The filter must
        // remain valid until the callback and uses static storage.
        unsafe {
            match kind {
                DialogKind::Font => SDL_ShowOpenFileDialog(
                    Some(chosen),
                    user,
                    parent,
                    ptr::null(),
                    0,
                    initial,
                    false,
                ),
                DialogKind::ScreenshotFolder => {
                    SDL_ShowOpenFolderDialog(Some(chosen), user, parent, initial, false)
                }
                DialogKind::ScreenshotFile => {
                    SDL_ShowSaveFileDialog(Some(chosen), user, parent, PNG_FILTER, 1, initial)
                }
            }
        }
        true
    }

    pub fn poll(&mut self) -> Option<(DialogKind, ResultPath)> {
        let (id, kind) = self.0?;
        let mut callbacks = callbacks();
        let result = callbacks.results.get_mut(&id)?.take()?;
        callbacks.results.remove(&id);
        self.0 = None;
        Some((kind, result))
    }
}

impl Drop for FileDialog {
    fn drop(&mut self) {
        if let Some((id, _)) = self.0 {
            callbacks().results.remove(&id);
        }
    }
}

unsafe extern "C" fn chosen(user: *mut c_void, files: *const *const c_char, _filter: c_int) {
    if user.is_null() {
        return;
    }
    let mut callbacks = callbacks();
    let Some(result) = callbacks.results.get_mut(&user.addr()) else {
        return;
    };
    if result.is_some() {
        return;
    }
    // Copy the selected path while SDL still owns the result list.
    *result = Some(unsafe {
        if files.is_null() {
            Err(error())
        } else if (*files).is_null() {
            Ok(None)
        } else {
            path_from_c(CStr::from_ptr(*files)).map(Some)
        }
    });
}

pub fn downloads() -> Option<PathBuf> {
    use sdl3_sys::filesystem::{SDL_FOLDER_DOWNLOADS, SDL_GetUserFolder};
    unsafe {
        let path = SDL_GetUserFolder(SDL_FOLDER_DOWNLOADS);
        if path.is_null() {
            return None;
        }
        path_from_c(CStr::from_ptr(path)).ok()
    }
}

fn location_to_c(path: &Path) -> Option<CString> {
    #[cfg(any(windows, target_os = "macos"))]
    let bytes = path.to_str()?.as_bytes();
    #[cfg(not(any(windows, target_os = "macos")))]
    let bytes = path.as_os_str().as_encoded_bytes();
    CString::new(bytes).ok()
}

fn path_from_c(path: &CStr) -> Result<PathBuf, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_a_callback_from_another_thread_once() {
        let mut dialog = FileDialog::default();
        let id = dialog.begin(DialogKind::ScreenshotFile).unwrap();
        assert!(dialog.begin(DialogKind::Font).is_none());
        assert!(dialog.poll().is_none());

        std::thread::spawn(move || {
            let path = CString::new("截图.png").unwrap();
            let files = [path.as_ptr(), ptr::null()];
            let user = ptr::without_provenance_mut(id);
            unsafe {
                chosen(user, files.as_ptr(), -1);
                chosen(user, [ptr::null()].as_ptr(), -1);
            }
        })
        .join()
        .unwrap();

        let (kind, result) = dialog.poll().unwrap();
        assert!(matches!(kind, DialogKind::ScreenshotFile));
        assert_eq!(result, Ok(Some(PathBuf::from("截图.png"))));
        assert!(!dialog.pending());
        assert!(!callbacks().results.contains_key(&id));
        assert!(dialog.poll().is_none());
    }

    #[test]
    fn drop_unregisters_a_request_without_a_callback() {
        let mut dialog = FileDialog::default();
        let id = dialog.begin(DialogKind::Font).unwrap();
        drop(dialog);
        assert!(!callbacks().results.contains_key(&id));

        let mut next = FileDialog::default();
        let next_id = next.begin(DialogKind::ScreenshotFolder).unwrap();
        assert_ne!(id, next_id);
        let files = [ptr::null()];
        unsafe {
            chosen(ptr::null_mut(), files.as_ptr(), -1);
            chosen(ptr::without_provenance_mut(id), files.as_ptr(), -1);
        }
        assert!(next.poll().is_none());
        unsafe {
            chosen(ptr::without_provenance_mut(next_id), files.as_ptr(), -1);
        }
        assert!(matches!(
            next.poll(),
            Some((DialogKind::ScreenshotFolder, Ok(None)))
        ));
    }

    #[test]
    fn handles_non_utf8_paths_with_platform_encoding() {
        let path = c"\xff";
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let native = path_from_c(path).unwrap();
            assert_eq!(native.as_os_str().as_bytes(), path.to_bytes());
        }
        #[cfg(not(unix))]
        assert!(path_from_c(path).is_err());
    }
}
