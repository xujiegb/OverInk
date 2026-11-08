use super::Image;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use {
    crate::picker::portal::{self, CloseHandle},
    std::sync::Mutex,
};
type CaptureResult = Result<Option<Image>, String>;
pub struct Job {
    receiver: mpsc::Receiver<CaptureResult>,
    cancelled: Arc<AtomicBool>,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    close: Arc<Mutex<Option<CloseHandle>>>,
}
impl Job {
    pub fn start(bounds: [i32; 4]) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let close = Arc::new(Mutex::new(None));
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let request = close.clone();
        thread::Builder::new()
            .name("overink-freeze-desktop".into())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                let result = macos_capture(bounds, &flag);
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                let result = {
                    let _ = bounds;
                    portal_capture(&flag, &request)
                };
                let _ = sender.send(result);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            receiver,
            cancelled,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            close,
        })
    }
    pub fn poll(&mut self) -> Option<CaptureResult> {
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
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some((connection, path)) =
            self.close.lock().ok().and_then(|mut handle| handle.take())
        {
            let _ = thread::Builder::new()
                .name("overink-close-screenshot".into())
                .spawn(move || {
                    if let Ok(proxy) = zbus::blocking::Proxy::new(
                        &connection,
                        "org.freedesktop.portal.Desktop",
                        path.as_str(),
                        "org.freedesktop.portal.Request",
                    ) {
                        let _: Result<(), _> = proxy.call("Close", &());
                    }
                    let _ = connection.close();
                });
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn portal_capture(cancelled: &AtomicBool, close: &Mutex<Option<CloseHandle>>) -> CaptureResult {
    let result = (|| -> Result<Option<Image>, Box<dyn std::error::Error + Send + Sync>> {
        let options = std::collections::HashMap::from([
            ("interactive", zbus::zvariant::Value::from(true)),
            ("modal", zbus::zvariant::Value::from(true)),
        ]);
        let Some(values) = portal::request("Screenshot", options, cancelled, close)? else {
            return Ok(None);
        };
        if cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let uri = <&str>::try_from(
            values
                .get("uri")
                .ok_or("Screenshot portal returned no image")?,
        )?;
        Ok(Some(Image::load(&file_uri(uri)?)?))
    })();
    result.map_err(|e| {
        format!("Cannot freeze the desktop: {e}. A Screenshot portal backend is required.")
    })
}
#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
fn file_uri(uri: &str) -> Result<std::path::PathBuf, String> {
    let value = uri
        .strip_prefix("file://")
        .ok_or("Screenshot must be a local file URI")?;
    let value = value.strip_prefix("localhost").unwrap_or(value);
    if !value.starts_with('/') {
        return Err("Remote screenshot URIs are not supported".into());
    }
    let mut bytes = Vec::new();
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let a = input.next().ok_or("Invalid screenshot URI")?;
            let b = input.next().ok_or("Invalid screenshot URI")?;
            let hex = |v: u8| (v as char).to_digit(16).ok_or("Invalid screenshot URI");
            bytes.push((hex(a)? * 16 + hex(b)?) as u8);
        } else {
            bytes.push(byte);
        }
    }
    if bytes.contains(&0) {
        return Err("Invalid screenshot URI".into());
    }
    Ok(String::from_utf8(bytes).map_err(|e| e.to_string())?.into())
}

#[cfg(target_os = "macos")]
fn macos_capture(bounds: [i32; 4], cancelled: &AtomicBool) -> CaptureResult {
    use std::{
        process::Command,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("overink-freeze-{}-{unique}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("desktop.png");
    let result = (|| {
        let [x, y, w, h] = bounds;
        let mut child = Command::new("/usr/sbin/screencapture")
            .args(["-x", "-t", "png", &format!("-R{x},{y},{w},{h}")])
            .arg(&file)
            .spawn()
            .map_err(|e| e.to_string())?;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if !status.success() {
                    return Err("Allow OverInk Screen Recording access in macOS System Settings, then retry freezing the desktop".into());
                }
                return Image::load(&file).map(Some);
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    let _ = std::fs::remove_file(file);
    let _ = std::fs::remove_dir(dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn screenshot_uri_is_local_and_percent_decoded() {
        assert_eq!(
            file_uri("file:///tmp/hello%20world.png").unwrap(),
            std::path::PathBuf::from("/tmp/hello world.png")
        );
        assert_eq!(
            file_uri("file://localhost/tmp/test.png").unwrap(),
            std::path::PathBuf::from("/tmp/test.png")
        );
        for invalid in [
            "https://example.com/a.png",
            "file://remote/tmp/a.png",
            "file:///tmp/%00",
            "file:///tmp/%GZ",
        ] {
            assert!(file_uri(invalid).is_err());
        }
    }
}
