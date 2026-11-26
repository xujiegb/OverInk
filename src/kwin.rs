//! Short-lived KWin 6 scripts for native Wayland geometry and initial placement.
#![forbid(unsafe_code)]

use egui::{Rect, pos2, vec2};
use std::{
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use zbus::{Connection, Proxy};

const DESTINATION: &str = "org.kde.KWin";
const SCRIPTING: &str = "org.kde.kwin.Scripting";
const REPLY_PATH: &str = "/org/overink/KWinGeometry";
const SCRIPT: &str = include_str!("../assets/kwin/geometry.js");

type BridgeError = Box<dyn std::error::Error + Send + Sync>;

pub fn active() -> bool {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .any(|desktop| is_kde(&desktop))
}

fn is_kde(desktop: &str) -> bool {
    desktop
        .split(':')
        .any(|name| name.trim().eq_ignore_ascii_case("KDE"))
}

struct Reply(mpsc::Sender<String>);

#[zbus::interface(name = "org.overink.KWinGeometry")]
impl Reply {
    fn report(&self, payload: String) {
        let _ = self.0.send(payload);
    }
}

/// Only run bundled code against this process's own windows. Unload after
/// each request rather than installing a persistent session-wide plugin.
fn request(place_toolbar: bool) -> Result<Option<[f32; 4]>, BridgeError> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let plugin = format!(
        "overink-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let (sender, receiver) = mpsc::channel();
    zbus::block_on(async {
        let connection = Connection::session().await?;
        connection
            .object_server()
            .at(REPLY_PATH, Reply(sender))
            .await?;
        let destination = connection.unique_name().ok_or("No D-Bus sender name")?;
        let mut file = tempfile::Builder::new()
            .prefix("overink-kwin-")
            .suffix(".js")
            .tempfile()?;
        writeln!(
            file,
            "const overinkPid = {}; const placeToolbar = {}; const replyDestination = {};",
            std::process::id(),
            place_toolbar,
            serde_json::to_string(destination.as_str())?
        )?;
        file.write_all(SCRIPT.as_bytes())?;
        file.flush()?;
        let path = file.path().to_str().ok_or("Invalid KWin script path")?;
        let scripting = Proxy::new(&connection, DESTINATION, "/Scripting", SCRIPTING).await?;
        let id: i32 = scripting
            .call("loadScript", &(path, plugin.as_str()))
            .await?;
        if id < 0 {
            return Err("KWin refused to load the geometry script".into());
        }
        let result = async {
            let script_path = format!("/Scripting/Script{id}");
            let script = Proxy::new(
                &connection,
                DESTINATION,
                script_path.as_str(),
                "org.kde.kwin.Script",
            )
            .await?;
            script.call::<_, _, ()>("run", &()).await?;
            // zbus dispatches the exported interface on its connection executor.
            // This bounded wait runs only on a worker, never on the SDL thread.
            let payload = receiver.recv_timeout(Duration::from_secs(2))?;
            Ok(serde_json::from_str::<Option<[f32; 4]>>(&payload)?)
        }
        .await;
        let unloaded: zbus::Result<bool> =
            scripting.call("unloadScript", &(plugin.as_str(),)).await;
        if !matches!(unloaded, Ok(true)) {
            eprintln!("OverInk: could not unload the one-shot KWin geometry script");
        }
        result
    })
}

fn rect_from_geometry([x, y, width, height]: [f32; 4]) -> Result<Rect, String> {
    let rect = Rect::from_min_size(pos2(x, y), vec2(width, height));
    if !rect.min.is_finite() || !rect.max.is_finite() || !rect.is_positive() {
        return Err("KWin returned invalid window geometry".into());
    }
    Ok(rect)
}

pub fn canvas_geometry() -> Result<Rect, String> {
    let geometry = request(false)
        .map_err(|error| format!("KWin canvas geometry: {error}"))?
        .ok_or("KWin could not find the OverInk canvas")?;
    rect_from_geometry(geometry)
}

pub fn place_toolbar() {
    let worker = thread::Builder::new()
        .name("overink-kwin-placement".into())
        .spawn(|| {
            // KWin may not have processed the first SDL buffer yet.
            for _ in 0..4 {
                match request(true) {
                    Ok(Some(_)) => return,
                    Ok(None) => thread::sleep(Duration::from_millis(100)),
                    Err(error) => {
                        eprintln!("OverInk: KWin toolbar placement unavailable: {error}");
                        return;
                    }
                }
            }
            eprintln!("OverInk: KWin could not find the toolbar after its first present");
        });
    if let Err(error) = worker {
        eprintln!("OverInk: KWin toolbar placement worker failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_detection_requires_a_desktop_token() {
        assert!(is_kde("KDE"));
        assert!(is_kde("plasma:kde"));
        assert!(!is_kde("GNOME"));
        assert!(!is_kde("NotKDE"));
    }

    #[test]
    fn geometry_preserves_panel_offsets_and_negative_origins() -> Result<(), String> {
        let rect = rect_from_geometry([-1920., 40., 1920., 1040.])?;
        assert_eq!(rect.min, pos2(-1920., 40.));
        assert_eq!(rect.max, pos2(0., 1080.));
        assert!(rect_from_geometry([0., 0., 0., 100.]).is_err());
        assert!(rect_from_geometry([f32::NAN, 0., 100., 100.]).is_err());
        Ok(())
    }
}
