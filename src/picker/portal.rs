#![forbid(unsafe_code)]

use crate::{
    background::Image,
    color::{InkColor, Space},
};
use futures_lite::{StreamExt, future};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};
#[cfg(target_os = "linux")]
use std::sync::{Arc, atomic::AtomicU8};
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Structure, Variant};

type PortalResult<T> = Result<Option<T>, String>;

/// Owned full-screen Portal image conversion, transferred to the capture worker.
#[cfg(target_os = "linux")]
pub(crate) type ScreenshotFallback = Box<dyn FnOnce(Image) -> Result<Image, String> + Send>;

pub struct SystemJob<T = InkColor> {
    receiver: mpsc::Receiver<PortalResult<T>>,
    cancel: async_channel::Sender<()>,
    #[cfg(target_os = "linux")]
    portal_visibility: Option<Arc<AtomicU8>>,
}
impl SystemJob<InkColor> {
    pub fn start() -> Result<Self, String> {
        Self::spawn("overink-color-portal", |cancelled| {
            zbus::block_on(pick(cancelled)).map_err(|error| {
                format!(
                    "Screen picker: {error}. A Screenshot/PickColor portal backend is required."
                )
            })
        })
    }
}
impl SystemJob<Image> {
    pub fn screenshot(
        require_screen_target: bool,
        prepare: impl FnOnce(Image) -> Result<Image, String> + Send + 'static,
    ) -> Result<Self, String> {
        Self::spawn("overink-screenshot-portal", move |cancelled| {
            let values = zbus::block_on(request("Screenshot", require_screen_target, cancelled))
                .map_err(|error| format!("Screen capture: {error}"))?;
            let Some(mut values) = values else {
                return Ok(None);
            };
            let uri = values
                .remove("uri")
                .ok_or("Portal returned no screenshot")?;
            let uri = <&str>::try_from(&*uri).map_err(|error| error.to_string())?;
            let path = local_file(uri)?;
            prepare(Image::load(&path)?).map(Some)
        })
    }

    /// GNOME Shell captures OverInk's selected rectangle, not a Shell picker.
    /// The same D-Bus connection sends CancelCapture when the UI is cancelled.
    #[cfg(target_os = "linux")]
    pub fn shell_area(
        window: &crate::sdl::Window,
        region: egui::Rect,
        canvas: [f32; 2],
        portal_fallback: Option<ScreenshotFallback>,
    ) -> Result<Self, String> {
        static NEXT_SHELL_REQUEST: AtomicU64 = AtomicU64::new(1);
        let request_id = format!(
            "overink_{}_{}",
            std::process::id(),
            NEXT_SHELL_REQUEST.fetch_add(1, Ordering::Relaxed)
        );
        let position = if crate::sdl::driver() == "wayland" {
            None
        } else {
            window.position().ok()
        };
        let [x, y] = position.unwrap_or([0, 0]);
        let positioned = position.is_some();
        // State: 0 = Shell capture, 1 = hide requested, 2 = canvas hidden.
        // The worker must not request a compositor screenshot until the
        // main SDL thread has removed OverInk windows from the desktop.
        let portal_visibility = Arc::new(AtomicU8::new(0));
        let worker_visibility = Arc::clone(&portal_visibility);
        let mut job = Self::spawn("overink-gnome-shell-capture", move |cancelled| {
            let shell_request = (
                request_id.as_str(),
                region.left() as f64,
                region.top() as f64,
                region.right() as f64,
                region.bottom() as f64,
                canvas[0] as f64,
                canvas[1] as f64,
                positioned,
                f64::from(x),
                f64::from(y),
            );
            let shell_result: zbus::Result<Option<String>> = zbus::block_on(async {
                if cancelled.is_closed() {
                    return Ok(None);
                }
                let connection = Connection::session().await?;
                let bridge = Proxy::new(
                    &connection,
                    "org.overink.Overlay",
                    "/org/overink/Overlay",
                    "org.overink.Overlay",
                )
                .await?;
                if cancelled.is_closed() {
                    return Ok(None);
                }

                let mut capture =
                    std::pin::pin!(bridge.call::<_, _, String>("CaptureArea", &shell_request));
                let first = future::or(async { Some(capture.as_mut().await) }, async {
                    let _ = cancelled.recv().await;
                    None
                })
                .await;
                match first {
                    Some(response) => response.map(Some),
                    None => {
                        // Cancellation is scoped by the D-Bus unique sender and
                        // request ID: another app cannot cancel a different job.
                        let _ = bridge
                            .call::<_, _, ()>("CancelCapture", &(request_id.as_str(),))
                            .await;
                        // Keep the original call alive to consume a late path
                        // and unlink its PNG in case capture already completed.
                        Ok(capture.await.ok())
                    }
                }
            });
            let path = match shell_result {
                Ok(path) => path,
                Err(error)
                    if portal_fallback.is_some()
                        && (error.to_string().contains("ServiceUnknown")
                            || error.to_string().contains("NameHasNoOwner")) =>
                {
                    // The Shell extension is absent. Only the main SDL thread
                    // can hide the annotation canvas and toolbar; wait for
                    // that acknowledgement before requesting portal pixels.
                    worker_visibility.store(1, Ordering::Release);
                    while !cancelled.is_closed()
                        && worker_visibility.load(Ordering::Acquire) != 2
                    {
                        thread::sleep(std::time::Duration::from_millis(8));
                    }
                    if cancelled.is_closed() {
                        return Ok(None);
                    }
                    // SDL_HideWindow is synchronous, but compositor visibility
                    // is applied at the next frame. Give it time to settle.
                    thread::sleep(std::time::Duration::from_millis(100));
                    if cancelled.is_closed() {
                        return Ok(None);
                    }
                    // Portal v2 has no explicit full-screen target. Accept
                    // only an image matching the known single-display desktop.
                    // Never fall back to the incomplete X11 framebuffer.
                    let values = zbus::block_on(request("Screenshot", false, cancelled)).map_err(
                        |error| {
                            format!(
                                "GNOME Overlay is unavailable; screenshot portal failed: {error}. Enable OverInk Overlay (revision 6) in GNOME Wayland."
                            )
                        },
                    )?;
                    let Some(mut values) = values else {
                        return Ok(None);
                    };
                    let uri = values
                        .remove("uri")
                        .ok_or("Screenshot portal returned no image")?;
                    let uri = <&str>::try_from(&*uri).map_err(|error| error.to_string())?;
                    let file = local_file(uri)?;
                    if cancelled.is_closed() {
                        return Ok(None);
                    }
                    // The returned URI may be a document-portal FUSE entry or
                    // a user-owned file, so it is not ours to unlink.
                    let image = Image::load(&file)?;
                    if cancelled.is_closed() {
                        return Ok(None);
                    }
                    let fallback =
                        portal_fallback.ok_or("GNOME capture fallback is unavailable")?;
                    let image = fallback(image).map_err(|error| {
                        format!(
                            "{error}. Enable OverInk Overlay (revision 6) in GNOME Wayland for reliable compositor capture."
                        )
                    })?;
                    if cancelled.is_closed() {
                        return Ok(None);
                    }
                    return Ok(Some(image));
                }
                Err(error) => {
                    return Err(format!(
                        "GNOME capture failed: {error}. Enable OverInk Overlay (revision 6) in GNOME Wayland. XWayland alone cannot capture native Wayland windows."
                    ));
                }
            };
            let Some(path) = path else {
                return Ok(None);
            };
            let file = PathBuf::from(path);
            let expected = std::env::temp_dir()
                .canonicalize()
                .map_err(|error| error.to_string())?;
            let parent = file
                .parent()
                .ok_or("GNOME capture returned an invalid file path")?
                .canonicalize()
                .map_err(|error| error.to_string())?;
            if parent != expected
                || !file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("overink-capture-"))
            {
                return Err("GNOME capture returned an unexpected temporary file".into());
            }
            // A cancelled request can race with a completed D-Bus response.
            // Always remove the compositor's PNG, even if no longer needed.
            let result = if cancelled.is_closed() {
                Ok(None)
            } else {
                Image::load(&file).map(Some)
            };
            let _ = std::fs::remove_file(file);
            result
        })?;
        job.portal_visibility = Some(portal_visibility);
        Ok(job)
    }
}
impl<T: Send + 'static> SystemJob<T> {
    fn spawn(
        thread_name: &str,
        request: impl FnOnce(&async_channel::Receiver<()>) -> PortalResult<T> + Send + 'static,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let (cancel, cancelled) = async_channel::bounded(1);
        thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let _ = sender.send(request(&cancelled));
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            receiver,
            cancel,
            #[cfg(target_os = "linux")]
            portal_visibility: None,
        })
    }
    pub fn poll(&self) -> Option<PortalResult<T>> {
        match self.receiver.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("Screenshot portal disconnected".into())),
        }
    }
}
impl<T> SystemJob<T> {
    #[cfg(target_os = "linux")]
    pub fn portal_fallback_pending(&self) -> bool {
        self.portal_visibility
            .as_ref()
            .is_some_and(|state| state.load(Ordering::Acquire) != 0)
    }

    #[cfg(target_os = "linux")]
    pub fn acknowledge_portal_fallback_hidden(&self) {
        if let Some(state) = &self.portal_visibility {
            let _ = state.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    pub fn cancel(&self) {
        self.cancel.close();
    }
}
impl<T> Drop for SystemJob<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn local_file(uri: &str) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt as _;
    let path = uri
        .strip_prefix("file://")
        .ok_or("Screenshot URI is not a local file")?;
    let path = path.strip_prefix("localhost").unwrap_or(path);
    if !path.starts_with('/') {
        return Err("Screenshot URI is not a local file".into());
    }
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let mut hex = || {
                bytes
                    .next()
                    .and_then(|value| char::from(value).to_digit(16))
            };
            let high = hex().ok_or("Invalid screenshot URI")?;
            let low = hex().ok_or("Invalid screenshot URI")?;
            (high * 16 + low) as u8
        } else {
            byte
        };
        if byte == 0 {
            return Err("Invalid screenshot URI".into());
        }
        decoded.push(byte);
    }
    Ok(std::ffi::OsString::from_vec(decoded).into())
}

async fn pick(
    cancelled: &async_channel::Receiver<()>,
) -> Result<Option<InkColor>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(mut values) = request("PickColor", false, cancelled).await? else {
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
    method: &str,
    require_screen_target: bool,
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
    let version: u32 = screenshot.get_property("version").await?;
    if cancelled.is_closed() {
        return Ok(None);
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let token = format!(
        "overink_capture_{}_{}",
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
    // Install the response subscription before invoking the portal method.
    let mut responses = request.receive_signal("Response").await?;
    if cancelled.is_closed() {
        return Ok(None);
    }
    let mut options = HashMap::from([("handle_token", Variant::from(token))]);
    if method == "Screenshot" {
        if version >= 2 {
            options.insert("interactive", Variant::from(false));
        }
        if version >= 3
            && let Ok(targets) = screenshot.get_property::<u32>("AvailableTargets").await
            && targets & 1 != 0
        {
            options.insert("target", Variant::from(1_u32));
        } else if require_screen_target {
            return Err("The portal cannot request the entire desktop explicitly".into());
        }
    }
    let handle: OwnedObjectPath = screenshot.call(method, &("", options)).await?;
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

    #[cfg(target_os = "linux")]
    #[test]
    fn fallback_waits_for_canvas_hide_before_portal_request() {
        let (_sender, receiver) = mpsc::channel();
        let (cancel, _cancelled) = async_channel::bounded(1);
        let state = Arc::new(AtomicU8::new(0));
        let job = SystemJob::<Image> {
            receiver,
            cancel,
            portal_visibility: Some(Arc::clone(&state)),
        };
        assert!(!job.portal_fallback_pending());
        job.acknowledge_portal_fallback_hidden();
        assert_eq!(state.load(Ordering::Acquire), 0);
        state.store(1, Ordering::Release);
        assert!(job.portal_fallback_pending());
        job.acknowledge_portal_fallback_hidden();
        assert_eq!(state.load(Ordering::Acquire), 2);
        assert!(job.portal_fallback_pending());
    }

    struct MockScreenshot {
        await_close: AtomicBool,
        pending: mpsc::Sender<()>,
        closed: mpsc::Sender<()>,
    }

    struct MockRequest(mpsc::Sender<()>);

    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl MockRequest {
        fn close(&self) {
            let _ = self.0.send(());
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
            let token = options
                .get("handle_token")
                .and_then(|value| <&str>::try_from(value).ok())
                .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Missing handle token".into()))?;
            let sender = header
                .sender()
                .ok_or_else(|| zbus::fdo::Error::Failed("Missing D-Bus sender".into()))?;
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
                self.pending
                    .send(())
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                return OwnedObjectPath::try_from(path)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()));
            }
            let color =
                OwnedVariant::try_from(Structure::from((18. / 255., 171. / 255., 239. / 255.)))
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
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
            OwnedObjectPath::try_from(path)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }
    }
    #[test]
    #[ignore = "requires an isolated session bus: dbus-run-session cargo test portal_response -- --ignored"]
    fn portal_response_before_method_reply_and_cancellation()
    -> Result<(), Box<dyn std::error::Error>> {
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
        })?;
        let timeout = Duration::from_secs(5);
        let job = SystemJob::start()?;
        let color = job
            .receiver
            .recv_timeout(timeout)??
            .ok_or("Expected a portal color")?;
        assert_eq!(color.srgb8(), [0x12, 0xab, 0xef]);
        assert_eq!(color.space, Space::Srgb);

        let screenshot = SystemJob::screenshot(true, Ok)?;
        let error = screenshot
            .receiver
            .recv_timeout(timeout)?
            .err()
            .ok_or("Expected rejection of unavailable screen targeting")?;
        assert!(error.contains("cannot request the entire desktop explicitly"));

        let job = SystemJob::start()?;
        requested.recv_timeout(timeout)?;
        job.cancel();
        acknowledged.recv_timeout(timeout)?;
        assert!(job.receiver.recv_timeout(timeout)??.is_none());
        Ok(())
    }
}
