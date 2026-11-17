#![forbid(unsafe_code)]

use super::{Window, dialog_native as native};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
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
        native::show(id, kind, window, location);
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

pub(super) fn complete(id: usize, value: impl FnOnce() -> ResultPath) {
    let mut callbacks = callbacks();
    let Some(result) = callbacks.results.get_mut(&id) else {
        return;
    };
    if result.is_none() {
        *result = Some(value());
    }
}

pub fn downloads() -> Option<PathBuf> {
    native::downloads()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn delivers_a_callback_from_another_thread_once() {
        let mut dialog = FileDialog::default();
        let id = dialog.begin(DialogKind::ScreenshotFile).unwrap();
        assert!(dialog.begin(DialogKind::Font).is_none());
        assert!(dialog.poll().is_none());

        std::thread::spawn(move || {
            complete(id, || native::path_from_c(c"截图.png").map(Some));
            complete(id, || panic!("A duplicate callback must be ignored"));
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
        complete(0, || panic!("Request ID zero is reserved"));
        complete(id, || panic!("A dropped request must be ignored"));
        assert!(next.poll().is_none());
        complete(next_id, || Ok(None));
        assert!(matches!(
            next.poll(),
            Some((DialogKind::ScreenshotFolder, Ok(None)))
        ));
    }

    #[test]
    fn native_callback_copies_selected_path() {
        let mut dialog = FileDialog::default();
        let id = dialog.begin(DialogKind::ScreenshotFile).unwrap();
        let files = [c"sample.png".as_ptr(), ptr::null()];
        native::chosen(ptr::without_provenance_mut(id), files.as_ptr(), -1);
        let (kind, result) = dialog.poll().unwrap();
        assert!(matches!(kind, DialogKind::ScreenshotFile));
        assert_eq!(result, Ok(Some(PathBuf::from("sample.png"))));
    }

    #[test]
    fn native_callback_cancellation_is_owned_and_consumed_once() {
        let mut dialog = FileDialog::default();
        let id = dialog.begin(DialogKind::Font).unwrap();
        let list = [ptr::null()];
        native::chosen(ptr::without_provenance_mut(id), list.as_ptr(), -1);
        assert!(matches!(dialog.poll(), Some((DialogKind::Font, Ok(None)))));
        assert!(dialog.poll().is_none());
    }

    #[test]
    fn native_callback_error_returns_a_result_not_a_pending_dialog() {
        let mut dialog = FileDialog::default();
        let id = dialog.begin(DialogKind::ScreenshotFolder).unwrap();
        native::chosen(ptr::without_provenance_mut(id), ptr::null(), -1);
        assert!(matches!(
            dialog.poll(),
            Some((DialogKind::ScreenshotFolder, Err(_)))
        ));
        assert!(!dialog.pending());
    }

    #[test]
    fn handles_non_utf8_paths_with_platform_encoding() {
        let path = c"\xff";
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let native = native::path_from_c(path).unwrap();
            assert_eq!(native.as_os_str().as_bytes(), path.to_bytes());
        }
        #[cfg(not(unix))]
        assert!(native::path_from_c(path).is_err());
    }
}
