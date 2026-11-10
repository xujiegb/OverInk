use super::{Image, Snapshot};
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
    receiver: mpsc::Receiver<Result<Option<Snapshot>, String>>,
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
                let result = capture(&flag).and_then(|image| {
                    image
                        .filter(|_| !flag.load(Ordering::Relaxed))
                        .map(Snapshot::compress)
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
    pub fn poll(&self) -> Option<Result<Option<Snapshot>, String>> {
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
