#![forbid(unsafe_code)]

// SPDX-License-Identifier: BSD-3-Clause
mod dialog;

use crate::{
    appearance::{Language, Preferences, Status, Text, text},
    sdl::{Runtime, driver},
};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use zbus::{OwnedVariant, Proxy, connection::Builder};

const UUID: &str = "overink-overlay@overink";
const EXTENSIONS: &str = "gnome-shell/extensions";
const METADATA: &str = include_str!("../assets/gnome-shell/metadata.json");
const EXTENSION_JS: &str = include_str!("../assets/gnome-shell/extension.js");
const SHELL_BUS: &str = "org.gnome.Shell";
const SHELL_PATH: &str = "/org/gnome/Shell";
const SHELL_EXTENSIONS: &str = "org.gnome.Shell.Extensions";

// GNOME Shell exports these states as D-Bus doubles.
const ACTIVE: f64 = 1.;
const INACTIVE: f64 = 2.;
const ERROR: f64 = 3.;
const OUT_OF_DATE: f64 = 4.;
const INITIALIZED: f64 = 6.;

pub fn in_wayland_session() -> bool {
    crate::sdl::is_wayland_session()
        && env::var("XDG_SESSION_TYPE").as_deref() != Ok("x11")
        && ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"]
            .into_iter()
            .find_map(|key| env::var(key).ok().filter(|value| !value.trim().is_empty()))
            .is_some_and(|desktop| is_gnome(&desktop))
}

fn is_gnome(desktop: &str) -> bool {
    desktop.split(':').any(|name| {
        let name = name.trim();
        [
            "GNOME",
            "GNOME-Classic",
            "GNOME-Wayland",
            "GNOME-Classic-Wayland",
        ]
        .iter()
        .any(|tag| name.eq_ignore_ascii_case(tag))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Native,
    Relogin,
    XWayland,
}

#[derive(Clone, Copy)]
pub enum Action {
    Proceed,
    Cancel,
}

pub enum Guide {
    Install,
    Update,
    Enable,
    Relogin,
    Unavailable(Status),
}

const RELOGIN: Text = text("Sign out of GNOME and sign back in. Then open OverInk to enable the extension.");

impl Guide {
    pub fn check() -> Option<Self> {
        zbus::block_on(async {
            let proxy = shell().await?;
            inspect(&proxy).await
        })
        .unwrap_or_else(|error| {
            Some(Self::Unavailable(
                text("Could not check the GNOME extension")
                .detail(error),
            ))
        })
    }

    pub fn ui(&self, ui: &mut egui::Ui, tr: Language, startup: bool) -> Option<Action> {
        let (message, proceed) = match self {
            Self::Install => (
                tr.text("Install OverInk Overlay to keep the canvas on top and position the toolbar in GNOME Wayland."),
                tr.text("Install extension"),
            ),
            Self::Update => (
                tr.text("Update OverInk Overlay to use native Wayland with this version of OverInk."),
                tr.text("Update extension"),
            ),
            Self::Enable => (
                tr.text("Enable the installed OverInk Overlay extension to use native Wayland."),
                tr.text("Enable extension"),
            ),
            Self::Relogin => (
                RELOGIN.get(tr),
                tr.text("Use XWayland for now"),
            ),
            Self::Unavailable(status) => {
                ui.label(status.get(tr));
                ("", tr.text("Check again"))
            }
        };
        if !message.is_empty() {
            ui.label(message);
        }
        let cancel = if startup {
            tr.text("Turn off native Wayland")
        } else {
            tr.text("Cancel")
        };
        ui.horizontal_wrapped(|ui| {
            if ui.button(proceed).clicked() {
                Some(Action::Proceed)
            } else if ui.button(cancel).clicked() {
                Some(Action::Cancel)
            } else {
                None
            }
        })
        .inner
    }

    pub fn act(&mut self, action: Action) -> Option<Decision> {
        if matches!(action, Action::Cancel) {
            return Some(Decision::XWayland);
        }
        match self {
            Self::Install | Self::Update => {
                *self = match Self::check() {
                    None => return Some(Decision::Native),
                    Some(Self::Install | Self::Update) => match install_files() {
                        Ok(_) => Self::Relogin,
                        Err(error) => Self::Unavailable(
                            text("Could not write the GNOME extension")
                            .detail(error),
                        ),
                    },
                    Some(guide) => guide,
                };
                None
            }
            Self::Enable => match enable() {
                Ok(()) => Some(Decision::Native),
                Err(error) => {
                    *self = Self::Unavailable(
                        text("Could not activate the GNOME extension")
                        .detail(error),
                    );
                    None
                }
            },
            Self::Relogin => Some(Decision::Relogin),
            Self::Unavailable(_) => match Self::check() {
                None => Some(Decision::Native),
                Some(guide) => {
                    *self = guide;
                    None
                }
            },
        }
    }
}

pub fn relogin_status() -> Status {
    RELOGIN.into()
}

pub fn start_video(
    mut preferences: Preferences,
    path: &Path,
    x11: bool,
    smoke: bool,
) -> Result<(Runtime, Preferences, Option<Status>), String> {
    let guide = if preferences.wayland && !x11 && !smoke && in_wayland_session() {
        Guide::check()
    } else {
        None
    };
    let Some(guide) = guide else {
        return Ok((
            Runtime::new(x11, preferences.wayland, smoke)?,
            preferences,
            None,
        ));
    };
    let status = match dialog::run(guide, &preferences) {
        Ok((Decision::Native, runtime)) if driver() == "wayland" => {
            // Keep the display connection when the compositor supplied WAYLAND_SOCKET.
            return Ok((runtime, preferences, None));
        }
        Ok((Decision::Native, runtime)) => {
            let status = text("Extension enabled. Restart OverInk to use native Wayland.")
            .into();
            return Ok((runtime, preferences, Some(status)));
        }
        Ok((Decision::Relogin, runtime)) => {
            if driver() == "x11" {
                return Ok((runtime, preferences, Some(relogin_status())));
            }
            relogin_status()
        }
        Ok((Decision::XWayland, runtime)) => {
            let status = turn_off_wayland(&mut preferences, path);
            if driver() == "x11" {
                return Ok((runtime, preferences, Some(status)));
            }
            status
        }
        Err(error) => {
            eprintln!("OverInk: opening the GNOME extension guide failed ({error})");
            text("Using XWayland because the extension guide could not open")
            .detail(error)
        }
    };
    Ok((Runtime::new(true, false, false)?, preferences, Some(status)))
}

fn turn_off_wayland(preferences: &mut Preferences, path: &Path) -> Status {
    // Read again in case settings changed while the startup guide was open.
    *preferences = Preferences::load(path);
    preferences.wayland = false;
    match preferences.save(path) {
        Ok(()) => text("Using XWayland.")
        .into(),
        Err(error) => {
            eprintln!("OverInk: saving the XWayland preference failed ({error})");
            text("Settings save failed").detail(error)
        }
    }
}

async fn shell() -> zbus::Result<Proxy<'static>> {
    let connection = Builder::session()
        .method_timeout(Duration::from_secs(1))
        .build()
        .await?;
    Proxy::new_owned(connection, SHELL_BUS, SHELL_PATH, SHELL_EXTENSIONS).await
}

type ExtensionInfo = HashMap<String, OwnedVariant>;

async fn extension_info(shell: &Proxy<'_>) -> zbus::Result<ExtensionInfo> {
    shell.call("GetExtensionInfo", &(UUID,)).await
}

fn extension_state(info: &ExtensionInfo) -> Option<f64> {
    info.get("state")
        .and_then(|state| f64::try_from(state).ok())
}

async fn inspect(shell: &Proxy<'_>) -> zbus::Result<Option<Guide>> {
    let info = extension_info(shell).await?;
    let state = extension_state(&info);
    let bundled_revision = bundled_metadata()["version"].as_f64().unwrap_or(0.);
    let old_bundle = matches!(state, Some(ACTIVE | INACTIVE | INITIALIZED))
        && extension_version(&info) < bundled_revision;
    if state == Some(OUT_OF_DATE) || old_bundle {
        let version: String = shell.get_property("ShellVersion").await?;
        if bundled_supports(&version) {
            return Ok(Some(if installed_bundle(&info) {
                Guide::Relogin
            } else {
                Guide::Update
            }));
        }
        if state == Some(OUT_OF_DATE) {
            return Ok(Some(incompatible(version)));
        }
    }
    let guide = match state {
        Some(ACTIVE) => return Ok(None),
        Some(ERROR) => Guide::Unavailable(
            text("GNOME reported an extension error")
            .detail(extension_error(&info).unwrap_or_default()),
        ),
        Some(INACTIVE | INITIALIZED) => {
            if shell.get_property::<bool>("UserExtensionsEnabled").await? {
                Guide::Enable
            } else {
                Guide::Unavailable(
                    text("Open GNOME Extensions and turn on its main switch. Then check again.")
                    .into(),
                )
            }
        }
        None if info.is_empty() => {
            let version: String = shell.get_property("ShellVersion").await?;
            if !bundled_supports(&version) {
                incompatible(version)
            } else if overlay_on_disk() {
                Guide::Relogin
            } else {
                Guide::Install
            }
        }
        _ => Guide::Unavailable(
            text("OverInk Overlay is inactive. Check its status in GNOME Extensions.")
            .into(),
        ),
    };
    Ok(Some(guide))
}

fn incompatible(version: String) -> Guide {
    Guide::Unavailable(
        text("OverInk Overlay is incompatible with this GNOME Shell version")
        .detail(version),
    )
}

fn extension_error(info: &ExtensionInfo) -> Option<&str> {
    info.get("error")
        .and_then(|value| <&str>::try_from(value).ok())
        .filter(|error| !error.is_empty())
}

fn extension_version(info: &ExtensionInfo) -> f64 {
    info.get("version")
        .and_then(|value| f64::try_from(value).ok())
        .unwrap_or(0.)
}

fn enable() -> Result<(), String> {
    let shell = zbus::block_on(shell()).map_err(|error| error.to_string())?;
    enable_extension(&shell)
}

fn enable_extension(shell: &Proxy<'_>) -> Result<(), String> {
    if !zbus::block_on(shell.call::<_, _, bool>("EnableExtension", &(UUID,)))
        .map_err(|error| error.to_string())?
    {
        return Err("GNOME Shell rejected EnableExtension".into());
    }
    // EnableExtension sets the preference before Shell loads the JavaScript.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let info = zbus::block_on(extension_info(shell)).map_err(|error| error.to_string())?;
        match extension_state(&info) {
            Some(ACTIVE) => return Ok(()),
            Some(ERROR | OUT_OF_DATE) => {
                return Err(extension_error(&info)
                    .unwrap_or("GNOME Shell reported an extension error")
                    .into());
            }
            _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => break,
        }
    }
    Err("GNOME Shell did not report an active extension".into())
}

fn bundled_supports(shell_version: &str) -> bool {
    let major = shell_version.split('.').next().unwrap_or_default();
    bundled_metadata()["shell-version"]
        .as_array()
        .is_some_and(|versions| versions.iter().any(|version| version == major))
}

fn bundled_metadata() -> &'static serde_json::Value {
    static BUNDLE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    BUNDLE.get_or_init(|| {
        serde_json::from_str(METADATA).unwrap_or_else(|error| {
            eprintln!("OverInk: invalid bundled GNOME extension metadata: {error}");
            serde_json::Value::Null
        })
    })
}

fn user_data() -> Option<PathBuf> {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })
}

fn overlay_on_disk() -> bool {
    let system_data = env::var_os("XDG_DATA_DIRS")
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    user_data()
        .into_iter()
        .chain(env::split_paths(&system_data))
        .filter(|path| path.is_absolute())
        .any(|path| {
            let extension = path.join(EXTENSIONS).join(UUID);
            extension.join("metadata.json").is_file() && extension.join("extension.js").is_file()
        })
}

fn installed_bundle(info: &ExtensionInfo) -> bool {
    let shell_path = info
        .get("path")
        .and_then(|value| <&str>::try_from(value).ok())
        .map(PathBuf::from);
    user_data()
        .map(|path| path.join(EXTENSIONS).join(UUID))
        .into_iter()
        .chain(shell_path)
        .filter(|path| path.is_absolute())
        .any(|extension| {
            // Shell can still report its old metadata until the next login.
            fs::read_to_string(extension.join("metadata.json")).is_ok_and(|data| data == METADATA)
                && fs::read_to_string(extension.join("extension.js"))
                    .is_ok_and(|data| data == EXTENSION_JS)
        })
}

fn install_files() -> Result<PathBuf, String> {
    let data = user_data().ok_or("No absolute XDG_DATA_HOME or HOME directory")?;
    let extension = data.join(EXTENSIONS).join(UUID);
    fs::create_dir_all(&extension)
        .map_err(|error| format!("Creating {} failed ({error})", extension.display()))?;
    for (name, contents) in [("extension.js", EXTENSION_JS), ("metadata.json", METADATA)] {
        let staged = extension.join(format!("{name}.{}.tmp", std::process::id()));
        let destination = extension.join(name);
        if let Err(error) =
            fs::write(&staged, contents).and_then(|()| fs::rename(&staged, &destination))
        {
            let _ = fs::remove_file(staged);
            return Err(format!(
                "Writing {} failed ({error})",
                destination.display()
            ));
        }
    }
    Ok(extension)
}

pub fn install_overlay() -> Result<(), String> {
    let extension = install_files()?;
    println!("Installed {UUID} in {}", extension.display());
    println!("After logging back in to GNOME, run gnome-extensions enable {UUID}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn matches_gnome_desktop_tokens_and_bundled_versions() {
        for desktop in [
            "GNOME",
            "ubuntu:GNOME",
            "GNOME-Classic:gnome",
            "gnome-wayland",
        ] {
            assert!(is_gnome(desktop), "{desktop}");
        }
        for desktop in ["", "KDE", "XFCE", "X-GNOME", "GNOME-Flashback:Unity"] {
            assert!(!is_gnome(desktop), "{desktop}");
        }
        assert!(bundled_supports("48.7"));
        assert!(bundled_supports("49"));
        assert!(!bundled_supports("99.0"));
    }

    #[test]
    fn declining_the_startup_guide_persists_wayland_off() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = env::temp_dir().join(format!("overink-gnome-preferences-{}", std::process::id()));
        let path = dir.join("ui.json");
        let mut preferences = Preferences {
            wayland: true,
            ..Preferences::default()
        };
        let saved = Preferences {
            language: Some(Language::Traditional),
            per_display_windows: true,
            restore_session: false,
            wayland: true,
            ..Preferences::default()
        };
        saved.save(&path)?;
        assert!(matches!(
            Guide::Install.act(Action::Cancel),
            Some(Decision::XWayland)
        ));
        turn_off_wayland(&mut preferences, &path);
        assert_eq!(
            preferences,
            Preferences {
                wayland: false,
                ..saved
            }
        );
        assert_eq!(Preferences::load(&path), preferences);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    struct MockShell {
        version: &'static str,
        states: &'static [Option<f64>],
        next: AtomicUsize,
        bundle_version: f64,
    }

    fn mock_shell(
        name: &str,
        version: &'static str,
        states: &'static [Option<f64>],
        bundle_version: f64,
    ) -> Result<Proxy<'static>, zbus::Error> {
        zbus::block_on(async {
            let service = Builder::session()
                .name(name)
                .serve_at(
                    SHELL_PATH,
                    MockShell {
                        version,
                        states,
                        next: AtomicUsize::new(0),
                        bundle_version,
                    },
                )
                .build()
                .await?;
            Proxy::new_owned(service, name.to_owned(), SHELL_PATH, SHELL_EXTENSIONS).await
        })
    }

    #[zbus::interface(name = "org.gnome.Shell.Extensions")]
    impl MockShell {
        #[zbus(property)]
        fn shell_version(&self) -> &str {
            self.version
        }

        #[zbus(property)]
        fn user_extensions_enabled(&self) -> bool {
            true
        }

        fn get_extension_info(&self, uuid: &str) -> ExtensionInfo {
            assert_eq!(uuid, UUID);
            let index = self.next.fetch_add(1, Ordering::Relaxed);
            self.states
                .get(index)
                .or_else(|| self.states.last())
                .copied()
                .flatten()
                .map_or_else(HashMap::new, |state| {
                    HashMap::from([
                        ("state".into(), state.into()),
                        ("version".into(), self.bundle_version.into()),
                    ])
                })
        }

        fn enable_extension(&self, uuid: &str) -> bool {
            uuid == UUID
        }
    }

    #[test]
    #[ignore = "requires an isolated session bus: dbus-run-session cargo test shell_activation -- --ignored"]
    fn shell_activation_waits_for_the_extension_to_be_active()
    -> Result<(), Box<dyn std::error::Error>> {
        let proxy = mock_shell(
            SHELL_BUS,
            "48.7",
            &[None, Some(INITIALIZED), Some(8.), Some(ACTIVE)],
            bundled_metadata()["version"]
                .as_f64()
                .ok_or("Missing bundled GNOME extension version")?,
        )?;
        assert!(matches!(
            zbus::block_on(inspect(&proxy))?,
            Some(Guide::Install | Guide::Relogin)
        ));
        assert!(matches!(
            zbus::block_on(inspect(&proxy))?,
            Some(Guide::Enable)
        ));
        enable_extension(&proxy)?;
        assert!(zbus::block_on(inspect(&proxy))?.is_none());
        Ok(())
    }

    #[test]
    #[ignore = "requires an isolated session bus: dbus-run-session cargo test outdated_extension -- --ignored"]
    fn outdated_extension_can_be_updated_when_the_bundle_supports_shell()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected = if installed_bundle(&HashMap::new()) {
            Guide::Relogin
        } else {
            Guide::Update
        };
        let supported = mock_shell(
            "org.gnome.Shell.TestSupported",
            "49.2",
            &[Some(OUT_OF_DATE)],
            0.,
        )?;
        let guide = zbus::block_on(inspect(&supported))?.ok_or("Expected a GNOME Shell guide")?;
        assert_eq!(
            std::mem::discriminant(&guide),
            std::mem::discriminant(&expected)
        );
        let unsupported = mock_shell(
            "org.gnome.Shell.TestUnsupported",
            "99.0",
            &[Some(OUT_OF_DATE)],
            0.,
        )?;
        assert!(matches!(
            zbus::block_on(inspect(&unsupported))?,
            Some(Guide::Unavailable(_))
        ));
        let active = mock_shell("org.gnome.Shell.TestOldActive", "49.2", &[Some(ACTIVE)], 0.)?;
        let guide = zbus::block_on(inspect(&active))?.ok_or("Expected a GNOME Shell guide")?;
        assert_eq!(
            std::mem::discriminant(&guide),
            std::mem::discriminant(&expected)
        );
        Ok(())
    }
}
