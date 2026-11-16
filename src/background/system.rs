use super::Image;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

pub type CaptureResult = Result<Option<Image>, String>;

pub struct Job<T> {
    receiver: mpsc::Receiver<Result<Option<T>, String>>,
    cancelled: Arc<AtomicBool>,
}
impl<T: Send + 'static> Job<T> {
    pub fn start(
        capture: impl FnOnce(&AtomicBool) -> CaptureResult + Send + 'static,
        prepare: impl FnOnce(Image) -> Result<T, String> + Send + 'static,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        thread::Builder::new()
            .name("overink-desktop-capture".into())
            .spawn(move || {
                let result = capture(&flag).and_then(|image| {
                    image
                        .filter(|_| !flag.load(Ordering::Relaxed))
                        .map(prepare)
                        .transpose()
                });
                let _ = sender.send(result);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            receiver,
            cancelled,
        })
    }
    pub fn poll(&self) -> Option<Result<Option<T>, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("Desktop capture disconnected".into())),
        }
    }
}
impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
