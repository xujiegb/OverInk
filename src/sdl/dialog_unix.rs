//! Native Unix file choosers, independent of SDL's optional libdbus support.
#![forbid(unsafe_code)]

use super::dialog::{DialogKind, complete};
use futures_lite::{StreamExt, future};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

type Choice = Result<Option<PathBuf>, String>;

pub(super) fn show(
    id: usize,
    kind: DialogKind,
    location: Option<&Path>,
    cancelled: async_channel::Receiver<()>,
) {
    let location = location.map(Path::to_path_buf);
    let worker = thread::Builder::new()
        .name("overink-file-chooser".into())
        .spawn(move || {
            let result = choose(id, kind, location.as_deref(), &cancelled);
            complete(id, || result);
        });
    if let Err(error) = worker {
        complete(id, || Err(error.to_string()));
    }
}

fn choose(
    id: usize,
    kind: DialogKind,
    location: Option<&Path>,
    cancelled: &async_channel::Receiver<()>,
) -> Choice {
    let portal_error = match zbus::block_on(portal(id, kind, location, cancelled)) {
        Ok(result) => return Ok(result),
        Err(error) => error.to_string(),
    };
    if cancelled.is_closed() {
        return Ok(None);
    }
    // Prefer the desktop's own chooser. These are optional runtime fallbacks,
    // not package dependencies. Never treat an explicit cancellation as a
    // failure or launch a second chooser after it.
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|name| name.eq_ignore_ascii_case("KDE"));
    let backends = if kde {
        ["kdialog", "zenity"]
    } else {
        ["zenity", "kdialog"]
    };
    let mut errors = vec![format!("FileChooser portal: {portal_error}")];
    for backend in backends {
        if cancelled.is_closed() {
            return Ok(None);
        }
        match external(backend, kind, location, cancelled) {
            Ok(result) => return Ok(result),
            Err(error) => errors.push(format!("{backend}: {error}")),
        }
    }
    Err(errors.join("; "))
}

async fn portal(
    id: usize,
    kind: DialogKind,
    location: Option<&Path>,
    cancelled: &async_channel::Receiver<()>,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::session().await?;
    let chooser = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
    )
    .await?;
    let version: u32 = chooser.get_property("version").await?;
    if cancelled.is_closed() {
        return Ok(None);
    }
    let token = format!("overink_file_{}_{id}", std::process::id());
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
    let mut responses = request.receive_signal("Response").await?;
    if cancelled.is_closed() {
        return Ok(None);
    }

    let mut options = HashMap::from([("handle_token", Variant::from(token))]);
    let (method, title, folder) = match kind {
        DialogKind::ScreenshotFolder => {
            if version < 3 {
                return Err("FileChooser portal does not support directory selection".into());
            }
            options.insert("directory", Variant::from(true));
            options.insert("multiple", Variant::from(false));
            ("OpenFile", "Select screenshot folder", location)
        }
        DialogKind::ScreenshotFile => {
            options.insert(
                "filters",
                Variant::from(vec![("PNG", vec![(0_u32, "*.png")])]),
            );
            if let Some(name) = location
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
            {
                options.insert("current_name", Variant::from(name));
            }
            (
                "SaveFile",
                "Save screenshot as",
                location.and_then(Path::parent),
            )
        }
    };
    if let Some(folder) = folder.filter(|folder| folder.is_absolute()) {
        let mut bytes = folder.as_os_str().as_bytes().to_vec();
        if !bytes.contains(&0) {
            bytes.push(0);
            options.insert("current_folder", Variant::from(bytes));
        }
    }
    // The canvas is hidden while the chooser owns input. Do not parent the
    // native dialog to the transparent fullscreen overlay.
    let arguments = ("", title, options);
    let mut invoking = Box::pin(chooser.call::<_, _, OwnedObjectPath>(method, &arguments));
    let handle = future::or(async { Some(invoking.as_mut().await) }, async {
        let _ = cancelled.recv().await;
        None
    })
    .await;
    let Some(handle) = handle else {
        let _ = request.call_noreply("Close", &()).await;
        let _ = invoking.await;
        let _ = request.call::<_, _, ()>("Close", &()).await;
        return Ok(None);
    };
    let handle = handle?;
    if handle.as_str() != path {
        return Err("FileChooser returned an unexpected request handle".into());
    }
    if cancelled.is_closed() {
        let _ = request.call_noreply("Close", &()).await;
        return Ok(None);
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
    let message = response.ok_or("FileChooser closed without a response")?;
    let (code, mut values): (u32, HashMap<String, OwnedVariant>) = message.body().deserialize()?;
    match code {
        1 => Ok(None),
        0 => {
            let value = values.remove("uris").ok_or("FileChooser returned no URI")?;
            let uris = Vec::<String>::try_from(value)?;
            let uri = uris
                .first()
                .ok_or("FileChooser returned an empty URI list")?;
            if uris.len() != 1 {
                return Err("FileChooser returned more than one path".into());
            }
            crate::picker::portal::local_file(uri)
                .map(Some)
                .map_err(Into::into)
        }
        _ => Err("FileChooser was unavailable or failed".into()),
    }
}

fn external(
    backend: &str,
    kind: DialogKind,
    location: Option<&Path>,
    cancelled: &async_channel::Receiver<()>,
) -> Choice {
    let mut command = Command::new(backend);
    let location = location.filter(|path| path.is_absolute());
    if backend == "kdialog" {
        match kind {
            DialogKind::ScreenshotFolder => {
                command.arg("--getexistingdirectory");
            }
            DialogKind::ScreenshotFile => {
                command.arg("--getsavefilename");
            }
        }
        command.arg(location.unwrap_or_else(|| Path::new("/")));
        if matches!(kind, DialogKind::ScreenshotFile) {
            command.arg("*.png|PNG");
        }
    } else {
        command.arg("--file-selection");
        match kind {
            DialogKind::ScreenshotFolder => {
                command.arg("--directory");
            }
            DialogKind::ScreenshotFile => {
                command.args(["--save", "--file-filter=PNG | *.png"]);
            }
        }
        if let Some(location) = location {
            let mut filename = OsString::from("--filename=");
            filename.push(location);
            if matches!(kind, DialogKind::ScreenshotFolder) {
                filename.push("/");
            }
            command.arg(filename);
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    loop {
        if cancelled.is_closed() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
                if status.success() {
                    return selected_path(output.stdout).map(Some);
                }
                if status.code() == Some(1) {
                    return Ok(None);
                }
                return Err(format!("chooser exited with {status}"));
            }
            Ok(None) => thread::sleep(Duration::from_millis(30)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    }
}

/// Parse XDG user directories as data; never source the shell file.
pub(super) fn downloads() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !home.is_absolute() {
        return None;
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    Some(
        fs::read(config.join("user-dirs.dirs"))
            .ok()
            .and_then(|bytes| download_path(&bytes, &home))
            .unwrap_or_else(|| home.join("Downloads")),
    )
}

fn download_path(config: &[u8], home: &Path) -> Option<PathBuf> {
    let value = config
        .split(|&byte| byte == b'\n')
        .map(|line| line.trim_ascii())
        .find_map(|line| line.strip_prefix(b"XDG_DOWNLOAD_DIR="))?;
    let value = value.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let (prefix, value) = if let Some(value) = value.strip_prefix(b"$HOME/") {
        (home.as_os_str().as_bytes(), value)
    } else if value == b"$HOME" {
        return Some(home.to_path_buf());
    } else if value.starts_with(b"/") {
        (&[][..], value)
    } else {
        return None;
    };
    let mut path = Vec::with_capacity(prefix.len() + value.len() + 1);
    path.extend_from_slice(prefix);
    if !prefix.is_empty() && !prefix.ends_with(b"/") {
        path.push(b'/');
    }
    let mut bytes = value.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => {
                let escaped = bytes.next()?;
                if !matches!(escaped, b'\\' | b'"' | b'$' | b'`') {
                    path.push(b'\\');
                }
                path.push(escaped);
            }
            b'"' | b'$' | b'`' | 0 => return None,
            _ => path.push(byte),
        }
    }
    if path.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(path)))
}

fn selected_path(mut bytes: Vec<u8>) -> Result<PathBuf, String> {
    // The native chooser writes one newline, not arbitrary whitespace.
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.contains(&0) {
        return Err("File chooser returned an invalid path".into());
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    if !path.is_absolute() {
        return Err("File chooser did not return an absolute path".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooser_preserves_path_bytes_and_spaces() -> Result<(), String> {
        let path = selected_path(b"/tmp/a \xff.png\n".to_vec())?;
        assert_eq!(path.as_os_str().as_bytes(), b"/tmp/a \xff.png");
        assert_eq!(
            selected_path(b"/tmp/name\n\n".to_vec())?
                .as_os_str()
                .as_bytes(),
            b"/tmp/name\n"
        );
        assert!(selected_path(b"relative.png\n".to_vec()).is_err());
        assert!(selected_path(b"/tmp/a\0.png\n".to_vec()).is_err());
        assert!(selected_path(Vec::new()).is_err());
        Ok(())
    }
}
