use super::PickResult;
use crate::color::{InkColor, Space};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue, Structure, Value},
};
pub(crate) type CloseHandle = (Connection, String);
pub struct SystemJob {
    receiver: mpsc::Receiver<PickResult>,
    cancelled: Arc<AtomicBool>,
    close: Arc<Mutex<Option<CloseHandle>>>,
}
impl SystemJob {
    pub fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let close = Arc::new(Mutex::new(None));
        let cancel_flag = cancelled.clone();
        let close_handle = close.clone();
        thread::Builder::new()
            .name("overink-color-portal".into())
            .spawn(move || {
                let result = pick(&cancel_flag, &close_handle).map_err(|e| {
                    format!(
                        "Screen picker: {e}. A Screenshot/PickColor portal backend is required."
                    )
                });
                let _ = sender.send(result);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            receiver,
            cancelled,
            close,
        })
    }
    pub fn poll(&mut self) -> Option<PickResult> {
        match self.receiver.try_recv() {
            Ok(v) => Some(v),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("Screen picker disconnected".into())),
        }
    }
    pub fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some((connection, path)) =
            self.close.lock().ok().and_then(|mut handle| handle.take())
        {
            let _ = thread::Builder::new()
                .name("overink-close-color-portal".into())
                .spawn(move || {
                    if let Ok(proxy) = Proxy::new(
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
impl Drop for SystemJob {
    fn drop(&mut self) {
        self.cancel();
    }
}
fn pick(
    cancelled: &AtomicBool,
    close: &Mutex<Option<CloseHandle>>,
) -> Result<Option<InkColor>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(mut values) = request("PickColor", HashMap::new(), cancelled, close)? else {
        return Ok(None);
    };
    let value = values.remove("color").ok_or("Portal returned no color")?;
    let structure = Structure::try_from(value)?;
    let fields = structure.fields();
    if fields.len() != 3 {
        return Err("Portal returned an invalid RGB tuple".into());
    }
    let mut rgb = [0.; 3];
    for (i, field) in fields.iter().enumerate() {
        let value = f64::try_from(field)?;
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err("Portal returned an invalid RGB component".into());
        }
        rgb[i] = value as f32;
    }
    Ok(Some(InkColor {
        rgb,
        space: Space::Srgb,
    }))
}
pub(crate) fn request(
    method: &str,
    mut options: HashMap<&str, Value<'_>>,
    cancelled: &AtomicBool,
    close: &Mutex<Option<CloseHandle>>,
) -> Result<Option<HashMap<String, OwnedValue>>, Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::session()?;
    let screenshot = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Screenshot",
    )?;
    let _: u32 = screenshot.get_property("version")?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let token = format!(
        "overink_color_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let sender = connection
        .unique_name()
        .ok_or("No D-Bus sender name")?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        path.as_str(),
        "org.freedesktop.portal.Request",
    )?;
    // Install the response subscription before invoking PickColor.
    let mut responses = request.receive_signal("Response")?;
    options.insert("handle_token", Value::from(token));
    let handle: OwnedObjectPath = screenshot.call(method, &("", options))?;
    if handle.as_str() != path {
        return Err("Portal returned an unexpected request handle".into());
    }
    if let Ok(mut state) = close.lock() {
        *state = Some((connection.clone(), path.clone()));
    }
    if cancelled.load(Ordering::Relaxed) {
        let _: Result<(), _> = request.call("Close", &());
        return Ok(None);
    }
    let Some(message) = responses.next() else {
        return if cancelled.load(Ordering::Relaxed) {
            Ok(None)
        } else {
            Err("Portal closed without a response".into())
        };
    };
    let (code, values): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    if code == 1 {
        return Ok(None);
    }
    if code != 0 {
        return Err("Screen capture or color selection was denied or failed".into());
    }
    Ok(Some(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct MockScreenshot {
        screenshot: std::path::PathBuf,
    }
    #[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
    impl MockScreenshot {
        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            2
        }
        async fn screenshot(
            &self,
            _parent: &str,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            assert!(bool::try_from(options.get("interactive").unwrap()).unwrap());
            image::RgbaImage::from_pixel(3, 2, image::Rgba([18, 171, 239, 255]))
                .save(&self.screenshot)
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
            let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
            let sender = header.sender().unwrap();
            let path = format!(
                "/org/freedesktop/portal/desktop/request/{}/{}",
                sender.as_str().trim_start_matches(':').replace('.', "_"),
                token
            );
            let uri =
                OwnedValue::try_from(Value::from(format!("file://{}", self.screenshot.display())))
                    .unwrap();
            connection
                .emit_signal(
                    Some(sender.as_str()),
                    path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0u32, HashMap::from([("uri", uri)])),
                )
                .await?;
            Ok(OwnedObjectPath::try_from(path).unwrap())
        }
        async fn pick_color(
            &self,
            _parent: &str,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
            let sender = header.sender().unwrap();
            let path = format!(
                "/org/freedesktop/portal/desktop/request/{}/{}",
                sender.as_str().trim_start_matches(':').replace('.', "_"),
                token
            );
            let color =
                OwnedValue::try_from(Structure::from((18. / 255., 171. / 255., 239. / 255.)))
                    .unwrap();
            let values = HashMap::from([("color", color)]);
            // Intentionally respond before the method reply to exercise the subscription race.
            connection
                .emit_signal(
                    Some(sender.as_str()),
                    path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0u32, values),
                )
                .await?;
            Ok(OwnedObjectPath::try_from(path).unwrap())
        }
    }
    #[test]
    #[ignore = "requires an isolated session bus: dbus-run-session cargo test portal_response -- --ignored"]
    fn portal_response_before_method_reply_is_received() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "overink-portal-test-{}-{unique}.png",
            std::process::id()
        ));
        let _service = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                MockScreenshot {
                    screenshot: path.clone(),
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let mut job = SystemJob::start().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = job.poll() {
                let color = result.unwrap().unwrap();
                assert_eq!(color.srgb8(), [0x12, 0xab, 0xef]);
                assert_eq!(color.space, Space::Srgb);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Portal response was lost"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut screenshot = crate::background::system::Job::start([0, 0, 3, 2]).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = screenshot.poll() {
                let image = result.unwrap().unwrap();
                assert_eq!((image.width, image.height), (3, 2));
                assert!(
                    image
                        .rgba
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|v| *v == [18, 171, 239, 255])
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Screenshot response was lost"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_file(path).unwrap();
    }
}
