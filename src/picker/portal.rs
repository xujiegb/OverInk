use super::PickResult;
use crate::color::{InkColor, Space};
use futures_lite::{StreamExt, future};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Structure, Variant};

pub struct SystemJob {
    receiver: mpsc::Receiver<PickResult>,
    cancel: async_channel::Sender<()>,
}
impl SystemJob {
    pub fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let (cancel, cancelled) = async_channel::bounded(1);
        thread::Builder::new()
            .name("overink-color-portal".into())
            .spawn(move || {
                let result = zbus::block_on(pick(&cancelled)).map_err(|e| {
                    format!(
                        "Screen picker: {e}. A Screenshot/PickColor portal backend is required."
                    )
                });
                let _ = sender.send(result);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { receiver, cancel })
    }
    pub fn poll(&mut self) -> Option<PickResult> {
        match self.receiver.try_recv() {
            Ok(v) => Some(v),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("Screen picker disconnected".into())),
        }
    }
    pub fn cancel(&mut self) {
        self.cancel.close();
    }
}
impl Drop for SystemJob {
    fn drop(&mut self) {
        self.cancel();
    }
}
async fn pick(
    cancelled: &async_channel::Receiver<()>,
) -> Result<Option<InkColor>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(mut values) = request(cancelled).await? else {
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
async fn request(
    cancelled: &async_channel::Receiver<()>,
) -> Result<Option<HashMap<String, OwnedVariant>>, Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::session().await?;
    let screenshot = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Screenshot",
    )
    .await?;
    let _: u32 = screenshot.get_property("version").await?;
    if cancelled.is_closed() {
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
    )
    .await?;
    // Install the response subscription before invoking PickColor.
    let mut responses = request.receive_signal("Response").await?;
    if cancelled.is_closed() {
        return Ok(None);
    }
    let options = HashMap::from([("handle_token", Variant::from(token))]);
    let handle: OwnedObjectPath = screenshot.call("PickColor", &("", options)).await?;
    if handle.as_str() != path {
        return Err("Portal returned an unexpected request handle".into());
    }
    let response = future::or(
        async {
            let _ = cancelled.recv().await;
            None
        },
        responses.next(),
    )
    .await;
    if cancelled.is_closed() {
        let _ = request.call_noreply("Close", &()).await;
        return Ok(None);
    }
    let message = response.ok_or("Portal closed without a response")?;
    let (code, values): (u32, HashMap<String, OwnedVariant>) = message.body().deserialize()?;
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
    use std::{sync::atomic::AtomicBool, time::Duration};

    struct MockScreenshot {
        await_close: AtomicBool,
        pending: mpsc::Sender<()>,
        closed: mpsc::Sender<()>,
    }

    struct MockRequest(mpsc::Sender<()>);

    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl MockRequest {
        fn close(&self) {
            self.0.send(()).unwrap();
        }
    }

    #[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
    impl MockScreenshot {
        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            2
        }
        async fn pick_color(
            &self,
            _parent: &str,
            options: HashMap<String, OwnedVariant>,
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
            if self.await_close.swap(true, Ordering::Relaxed) {
                connection
                    .object_server()
                    .at(path.as_str(), MockRequest(self.closed.clone()))
                    .await?;
                self.pending.send(()).unwrap();
                return Ok(OwnedObjectPath::try_from(path).unwrap());
            }
            let color =
                OwnedVariant::try_from(Structure::from((18. / 255., 171. / 255., 239. / 255.)))
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
    fn portal_response_before_method_reply_and_cancellation() {
        let (pending, requested) = mpsc::channel();
        let (closed, acknowledged) = mpsc::channel();
        let _service = zbus::block_on(async {
            zbus::connection::Builder::session()
                .name("org.freedesktop.portal.Desktop")
                .serve_at(
                    "/org/freedesktop/portal/desktop",
                    MockScreenshot {
                        await_close: AtomicBool::new(false),
                        pending,
                        closed,
                    },
                )
                .build()
                .await
                .unwrap()
        });
        let timeout = Duration::from_secs(5);
        let job = SystemJob::start().unwrap();
        let color = job
            .receiver
            .recv_timeout(timeout)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(color.srgb8(), [0x12, 0xab, 0xef]);
        assert_eq!(color.space, Space::Srgb);

        let mut job = SystemJob::start().unwrap();
        requested.recv_timeout(timeout).unwrap();
        job.cancel();
        acknowledged.recv_timeout(timeout).unwrap();
        assert!(
            job.receiver
                .recv_timeout(timeout)
                .unwrap()
                .unwrap()
                .is_none()
        );
    }
}
