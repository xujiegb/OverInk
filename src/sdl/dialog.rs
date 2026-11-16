use super::{Window, error};
use sdl3_sys::dialog::*;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    ptr,
    sync::mpsc,
};

#[derive(Clone, Copy)]
pub enum DialogKind {
    Font,
    ScreenshotFolder,
    ScreenshotFile,
}

type ResultPath = Result<Option<PathBuf>, String>;

#[derive(Default)]
pub struct FileDialog(Option<(DialogKind, mpsc::Receiver<ResultPath>)>);

struct Callback {
    sender: mpsc::Sender<ResultPath>,
    location: Option<CString>,
    filter: SDL_DialogFileFilter,
}

impl FileDialog {
    pub fn pending(&self) -> bool {
        self.0.is_some()
    }

    pub fn open(&mut self, kind: DialogKind, window: &Window, location: Option<&Path>) {
        if self.pending() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.0 = Some((kind, receiver));
        let callback = Box::into_raw(Box::new(Callback {
            sender,
            location: location
                .and_then(|path| CString::new(path.as_os_str().as_encoded_bytes()).ok()),
            filter: SDL_DialogFileFilter {
                name: c"PNG".as_ptr(),
                pattern: c"png".as_ptr(),
            },
        }));
        // SDL may call the callback before the function returns. No references
        // into its state are kept alive across the call.
        unsafe {
            let data = callback.cast();
            let initial = (*callback)
                .location
                .as_ref()
                .map_or(ptr::null(), |path| path.as_ptr());
            match kind {
                DialogKind::Font => SDL_ShowOpenFileDialog(
                    Some(chosen),
                    data,
                    window.raw,
                    ptr::null(),
                    0,
                    initial,
                    false,
                ),
                DialogKind::ScreenshotFolder => {
                    SDL_ShowOpenFolderDialog(Some(chosen), data, window.raw, initial, false)
                }
                DialogKind::ScreenshotFile => SDL_ShowSaveFileDialog(
                    Some(chosen),
                    data,
                    window.raw,
                    ptr::addr_of!((*callback).filter),
                    1,
                    initial,
                ),
            }
        }
    }

    pub fn poll(&mut self) -> Option<(DialogKind, ResultPath)> {
        let (kind, receiver) = self.0.as_ref()?;
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("File dialog closed".into()),
        };
        let kind = *kind;
        self.0 = None;
        Some((kind, result))
    }
}

unsafe extern "C" fn chosen(user: *mut c_void, files: *const *const c_char, _filter: c_int) {
    unsafe {
        let callback = Box::from_raw(user.cast::<Callback>());
        let result = if files.is_null() {
            Err(error())
        } else if (*files).is_null() {
            Ok(None)
        } else {
            Ok(Some(path_from_c(CStr::from_ptr(*files))))
        };
        let _ = callback.sender.send(result);
    }
}

pub fn downloads() -> Option<PathBuf> {
    use sdl3_sys::filesystem::{SDL_FOLDER_DOWNLOADS, SDL_GetUserFolder};
    unsafe {
        let path = SDL_GetUserFolder(SDL_FOLDER_DOWNLOADS);
        (!path.is_null()).then(|| path_from_c(CStr::from_ptr(path)))
    }
}

fn path_from_c(path: &CStr) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()))
    }
    #[cfg(not(unix))]
    PathBuf::from(path.to_string_lossy().into_owned())
}
