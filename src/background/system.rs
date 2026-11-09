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

pub struct Job {
    receiver: mpsc::Receiver<CaptureResult>,
    cancelled: Arc<AtomicBool>,
}
impl Job {
    pub fn start(
        capture: impl FnOnce(&AtomicBool) -> CaptureResult + Send + 'static,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        thread::Builder::new()
            .name("overink-freeze-desktop".into())
            .spawn(move || {
                let _ = sender.send(capture(&flag));
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            receiver,
            cancelled,
        })
    }
    pub fn poll(&self) -> Option<CaptureResult> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("Desktop capture disconnected".into())),
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
