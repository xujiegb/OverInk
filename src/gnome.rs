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
use zbus::{
    blocking::{Proxy, connection::Builder},
    zvariant::OwnedValue,
};

const UUID: &str = "overink-overlay@overink";
const EXTENSIONS: &str = "gnome-shell/extensions";
const METADATA: &str = include_str!("../assets/gnome-shell/metadata.json");
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
    Enable,
    Relogin,
    Unavailable(Status),
}

const RELOGIN: Text = text(
    "Sign out of GNOME and sign back in. Then open OverInk to enable the extension.",
    "请注销并重新登录 GNOME，然后打开 OverInk 启用扩展。",
    "請登出並重新登入 GNOME，然後開啟 OverInk 啟用擴充套件。",
);

impl Guide {
    pub fn check() -> Option<Self> {
        shell()
            .and_then(|proxy| inspect(&proxy))
            .unwrap_or_else(|error| {
                Some(Self::Unavailable(
                    text(
                        "Could not check the GNOME extension",
                        "无法检查 GNOME 扩展",
                        "無法檢查 GNOME 擴充套件",
                    )
                    .detail(error),
                ))
            })
    }

    pub fn ui(&self, ui: &mut egui::Ui, tr: Language, startup: bool) -> Option<Action> {
        let (message, proceed) = match self {
            Self::Install => (
                tr.text(
                    "Install OverInk Overlay to keep the canvas on top and position the toolbar in GNOME Wayland.",
                    "安装 OverInk Overlay 扩展后，GNOME Wayland 才能将画布置顶并定位工具栏。",
                    "安裝 OverInk Overlay 擴充套件後，GNOME Wayland 才能將畫布置頂並定位工具列。",
                ),
                tr.text("Install extension", "安装扩展", "安裝擴充套件"),
            ),
            Self::Enable => (
                tr.text(
                    "Enable the installed OverInk Overlay extension to use native Wayland.",
                    "启用已安装的 OverInk Overlay 扩展，即可使用原生 Wayland。",
                    "啟用已安裝的 OverInk Overlay 擴充套件，即可使用原生 Wayland。",
                ),
                tr.text("Enable extension", "启用扩展", "啟用擴充套件"),
            ),
            Self::Relogin => (
                RELOGIN.get(tr),
                tr.text(
                    "Use XWayland for now",
                    "暂用 XWayland",
                    "暫用 XWayland",
                ),
            ),
            Self::Unavailable(status) => {
                ui.label(status.get(tr));
                ("", tr.text("Check again", "重新检查", "重新檢查"))
            }
        };
        if !message.is_empty() {
            ui.label(message);
        }
        let cancel = if startup {
            tr.text(
                "Turn off native Wayland",
                "关闭原生 Wayland",
                "關閉原生 Wayland",
            )
        } else {
            tr.text("Cancel", "取消", "取消")
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
            Self::Install => {
                *self = match Self::check() {
                    None => return Some(Decision::Native),
                    Some(Self::Install) => match install_files() {
                        Ok(_) => Self::Relogin,
                        Err(error) => Self::Unavailable(
                            text(
                                "Extension installation failed",
                                "安装扩展失败",
                                "安裝擴充套件失敗",
                            )
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
                        text(
                            "Could not activate the GNOME extension",
                            "无法启用 GNOME 扩展",
                            "無法啟用 GNOME 擴充套件",
                        )
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
    let status = match dialog::run(guide, preferences) {
        Ok((Decision::Native, runtime)) if driver() == "wayland" => {
            // Keep the display connection when the compositor supplied WAYLAND_SOCKET.
            return Ok((runtime, preferences, None));
        }
        Ok((Decision::Native, runtime)) => {
            let status = text(
                "Extension enabled. Restart OverInk to use native Wayland.",
                "扩展已启用。重启 OverInk 后使用原生 Wayland。",
                "擴充套件已啟用。重新啟動 OverInk 後使用原生 Wayland。",
            )
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
            text(
                "Using XWayland because the extension guide could not open",
                "扩展引导无法打开，正在使用 XWayland",
                "擴充套件引導無法開啟，正在使用 XWayland",
            )
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
        Ok(()) => text(
            "Using XWayland.",
            "正在使用 XWayland。",
            "正在使用 XWayland。",
        )
        .into(),
        Err(error) => {
            eprintln!("OverInk: saving the XWayland preference failed ({error})");
            text("Settings save failed", "保存设置失败", "儲存設定失敗").detail(error)
        }
    }
}

fn shell() -> zbus::Result<Proxy<'static>> {
    let connection = Builder::session()?
        .method_timeout(Duration::from_secs(1))
        .build()?;
    Proxy::new_owned(connection, SHELL_BUS, SHELL_PATH, SHELL_EXTENSIONS)
}

type ExtensionInfo = HashMap<String, OwnedValue>;

fn extension_info(shell: &Proxy<'_>) -> zbus::Result<ExtensionInfo> {
    shell.call("GetExtensionInfo", &(UUID,))
}

fn extension_state(info: &ExtensionInfo) -> Option<f64> {
    info.get("state")
        .and_then(|state| f64::try_from(state).ok())
}

fn inspect(shell: &Proxy<'_>) -> zbus::Result<Option<Guide>> {
    let info = extension_info(shell)?;
    let state = extension_state(&info);
    if state == Some(ACTIVE) {
        return Ok(None);
    }
    let unsupported = || {
        text(
            "OverInk Overlay is incompatible with this GNOME Shell version",
            "OverInk Overlay 与此 GNOME Shell 版本不兼容",
            "OverInk Overlay 與此 GNOME Shell 版本不相容",
        )
    };
    let guide = match state {
        Some(ERROR) => Guide::Unavailable(
            text(
                "GNOME reported an extension error",
                "GNOME 报告扩展错误",
                "GNOME 回報擴充套件錯誤",
            )
            .detail(extension_error(&info).unwrap_or_default()),
        ),
        Some(OUT_OF_DATE) => {
            Guide::Unavailable(unsupported().detail(shell.get_property::<String>("ShellVersion")?))
        }
        Some(INACTIVE | INITIALIZED) => {
            if shell.get_property::<bool>("UserExtensionsEnabled")? {
                Guide::Enable
            } else {
                Guide::Unavailable(
                    text(
                        "Open GNOME Extensions and turn on its main switch. Then check again.",
                        "请打开 GNOME 扩展应用，开启总开关后重新检查。",
                        "請開啟 GNOME 擴充套件應用程式，開啟總開關後重新檢查。",
                    )
                    .into(),
                )
            }
        }
        None if info.is_empty() => {
            let version: String = shell.get_property("ShellVersion")?;
            if !bundled_supports(&version) {
                Guide::Unavailable(unsupported().detail(version))
            } else if overlay_on_disk() {
                Guide::Relogin
            } else {
                Guide::Install
            }
        }
        _ => Guide::Unavailable(
            text(
                "OverInk Overlay is inactive. Check its status in GNOME Extensions.",
                "扩展尚未生效，请在 GNOME 扩展应用中检查状态。",
                "擴充套件尚未生效，請在 GNOME 擴充套件應用程式中檢查狀態。",
            )
            .into(),
        ),
    };
    Ok(Some(guide))
}

fn extension_error(info: &ExtensionInfo) -> Option<&str> {
    info.get("error")
        .and_then(|value| <&str>::try_from(value).ok())
        .filter(|error| !error.is_empty())
}

fn enable() -> Result<(), String> {
    let shell = shell().map_err(|error| error.to_string())?;
    enable_extension(&shell)
}

fn enable_extension(shell: &Proxy<'_>) -> Result<(), String> {
    if !shell
        .call::<_, _, bool>("EnableExtension", &(UUID,))
        .map_err(|error| error.to_string())?
    {
        return Err("GNOME Shell rejected EnableExtension".into());
    }
    // EnableExtension sets the preference before Shell loads the JavaScript.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let info = extension_info(shell).map_err(|error| error.to_string())?;
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
    let metadata: serde_json::Value =
        serde_json::from_str(METADATA).expect("bundled GNOME metadata");
    metadata["shell-version"]
        .as_array()
        .is_some_and(|versions| versions.iter().any(|version| version == major))
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

fn install_files() -> Result<PathBuf, String> {
    let data = user_data().ok_or("No absolute XDG_DATA_HOME or HOME directory")?;
    let extension = data.join(EXTENSIONS).join(UUID);
    fs::create_dir_all(&extension)
        .map_err(|error| format!("Creating {} failed ({error})", extension.display()))?;
    for (name, contents) in [
        (
            "extension.js",
            include_str!("../assets/gnome-shell/extension.js"),
        ),
        ("metadata.json", METADATA),
    ] {
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
    fn declining_the_startup_guide_persists_wayland_off() {
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
            ..preferences
        };
        saved.save(&path).unwrap();
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
        fs::remove_dir_all(dir).unwrap();
    }

    struct MockShell(AtomicUsize);

    #[zbus::interface(name = "org.gnome.Shell.Extensions")]
    impl MockShell {
        #[zbus(property)]
        fn shell_version(&self) -> &str {
            "48.7"
        }

        #[zbus(property)]
        fn user_extensions_enabled(&self) -> bool {
            true
        }

        fn get_extension_info(&self, uuid: &str) -> ExtensionInfo {
            assert_eq!(uuid, UUID);
            let state = match self.0.fetch_add(1, Ordering::Relaxed) {
                0 => return HashMap::new(),
                1 => INITIALIZED,
                2 => 8.,
                _ => ACTIVE,
            };
            HashMap::from([("state".into(), state.into())])
        }

        fn enable_extension(&self, uuid: &str) -> bool {
            uuid == UUID
        }
    }

    #[test]
    #[ignore = "requires an isolated session bus: dbus-run-session cargo test shell_activation -- --ignored"]
    fn shell_activation_waits_for_the_extension_to_be_active() {
        let _service = Builder::session()
            .unwrap()
            .name(SHELL_BUS)
            .unwrap()
            .serve_at(SHELL_PATH, MockShell(AtomicUsize::new(0)))
            .unwrap()
            .build()
            .unwrap();
        let proxy = shell().unwrap();
        assert!(matches!(
            inspect(&proxy).unwrap(),
            Some(Guide::Install | Guide::Relogin)
        ));
        assert!(matches!(inspect(&proxy).unwrap(), Some(Guide::Enable)));
        enable_extension(&proxy).unwrap();
        assert!(inspect(&proxy).unwrap().is_none());
    }
}
