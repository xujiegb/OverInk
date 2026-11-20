#![forbid(unsafe_code)]

//! Switch between the *user's configured* input methods on Unix desktops.
//! Native candidates and preedit remain owned by Fcitx5 / IBus. No global
//! key injection, shell invocation, FFI, or changes to the text buffer.
//! Both helpers work on Linux (X11/XWayland/Wayland) and FreeBSD when an
//! input-method daemon is running in the current desktop session.

use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Output},
};

fn success_stdout(program: &str, args: &[&str]) -> Result<Option<String>, String> {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{program}: {error}")),
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8(output.stdout)
            .map_err(|error| format!("{program} returned non-UTF-8 text: {error}"))?
            .trim()
            .to_owned(),
    ))
}

fn checked_command(program: &str, args: &[&str]) -> Result<(), String> {
    let output: Output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{program} failed ({}): {}",
            output.status,
            stderr.trim()
        ))
    }
}

/// Profile section names use [Groups/N] and [Groups/N/Items/M].
fn group_section(header: &str) -> Option<(&str, Option<usize>)> {
    let path = header.strip_prefix("[Groups/")?.strip_suffix(']')?;
    if let Some((id, item)) = path.split_once("/Items/") {
        id.parse::<usize>().ok()?;
        Some((id, Some(item.parse::<usize>().ok()?)))
    } else {
        path.parse::<usize>().ok()?;
        Some((path, None))
    }
}

/// Only the current group is eligible. The ordering here comes from the
/// user's Fcitx5 profile, NOT all engines installed on the machine.
fn configured_fcitx_methods<'a>(profile: &'a str, group: &str) -> Vec<&'a str> {
    let mut selected_group = None;
    let mut group_section_id = None;
    for line in profile.lines().map(str::trim) {
        if line.starts_with('[') {
            group_section_id =
                group_section(line).and_then(|(id, item)| item.is_none().then_some(id));
        } else if line
            .strip_prefix("Name=")
            .is_some_and(|name| name.trim() == group)
            && let Some(id) = group_section_id
        {
            selected_group = Some(id);
            break;
        }
    }
    let Some(selected_group) = selected_group else {
        return Vec::new();
    };

    let mut current_item = None;
    let mut items = Vec::new();
    for line in profile.lines().map(str::trim) {
        if line.starts_with('[') {
            current_item = group_section(line)
                .and_then(|(id, item)| (id == selected_group).then_some(item).flatten());
        } else if let Some(name) = line.strip_prefix("Name=")
            && let Some(index) = current_item
        {
            let name = name.trim();
            if !name.is_empty() && !items.iter().any(|(_, item)| *item == name) {
                items.push((index, name));
            }
        }
    }
    items.sort_unstable_by_key(|(index, _)| *index);
    items.into_iter().map(|(_, name)| name).collect()
}

fn fcitx_profile_path() -> Option<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("fcitx5").join("profile"))
}

/// Rotate from the current engine, wrapping around without inventing defaults.
/// A single configured method should report a useful error, not a fake switch.
fn next_method<'a>(current: &str, methods: &'a [&'a str]) -> Result<&'a str, &'static str> {
    if methods.len() < 2 {
        return Err("Configure at least two input methods in this group");
    }
    let Some(index) = methods.iter().position(|name| *name == current) else {
        return Err("Current input method is not in the configured group");
    };
    Ok(methods[(index + 1) % methods.len()])
}

/// Returns true only when the running Fcitx5 daemon handled the operation.
/// When Fcitx5 is unavailable, the caller can try IBus.
fn try_fcitx5() -> Result<bool, String> {
    let Some(current) = success_stdout("fcitx5-remote", &["-n"])? else {
        return Ok(false);
    };
    if current.is_empty() {
        return Ok(false);
    }
    let group = success_stdout("fcitx5-remote", &["-q"])?;
    let configured = group
        .as_deref()
        .filter(|name| !name.is_empty())
        .and_then(|name| {
            let profile = fcitx_profile_path().and_then(|path| fs::read_to_string(path).ok())?;
            Some((name.to_owned(), profile))
        });

    if let Some((group, profile)) = configured {
        let methods = configured_fcitx_methods(&profile, &group);
        if methods.len() >= 2 {
            let next = next_method(&current, &methods).map_err(str::to_owned)?;
            checked_command("fcitx5-remote", &["-s", next])?;
            return Ok(true);
        }
    }
    // Old Fcitx5 versions and profiles that have not yet been written to disk
    // still support switching between the keyboard and active IME. Unlike
    // the configured-group path this is a toggle, not a multi-engine cycle.
    checked_command("fcitx5-remote", &["-t"])?;
    Ok(true)
}

fn ibus_engine_names(output: &str) -> Vec<&str> {
    output
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| !name.chars().any(char::is_whitespace))
        .collect()
}

/// Parse the exact quoted string array printed by GSettings, without using a
/// shell or accepting any arbitrary engine name from untrusted command output.
fn quoted_gvariant_strings(value: &str) -> Vec<&str> {
    let mut rest = value;
    let mut values = Vec::new();
    while let Some(start) = rest.find('\'') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('\'') else {
            break;
        };
        let name = &rest[..end];
        if !name.is_empty() {
            values.push(name);
        }
        rest = &rest[end + 1..];
    }
    values
}

fn try_ibus() -> Result<(), String> {
    let Some(current) = success_stdout("ibus", &["engine"])? else {
        return Err("Neither Fcitx5 nor IBus is running in this session".into());
    };
    if current.is_empty() {
        return Err("IBus has no active engine".into());
    }

    // IBus supports --name-only (not the localized "name - description"
    // format). Read the user's preloaded engine order if available; filter
    // it against registered engines, never select a missing engine.
    let listing = success_stdout("ibus", &["list-engine", "--name-only"])?
        .ok_or("IBus cannot enumerate its engines")?;
    let available = ibus_engine_names(&listing);
    let preference = success_stdout(
        "gsettings",
        &["get", "org.freedesktop.ibus.general", "preload-engines"],
    )?;
    let preferred = preference
        .as_deref()
        .map(quoted_gvariant_strings)
        .unwrap_or_default();

    let has_configured_order = !preferred.is_empty();
    let configured: Vec<&str> = preferred
        .into_iter()
        .filter(|engine| available.contains(engine))
        .collect();
    // Never silently activate an unconfigured engine when the user has
    // explicitly selected fewer than two in IBus preferences.
    let methods: &[&str] = if has_configured_order {
        &configured
    } else {
        &available
    };
    let next = next_method(&current, methods).map_err(|error| format!("IBus: {error}"))?;
    checked_command("ibus", &["engine", next])
}

/// The system IME keeps its current composition/candidate UI. Only request a
/// source change; OverInk never fabricates its own candidate strings.
pub(super) fn switch_input_method() -> Result<(), String> {
    if try_fcitx5()? { Ok(()) } else { try_ibus() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"[Groups/0]
Name=Default
DefaultIM=rime

[Groups/0/Items/2]
Name=mozc

[Groups/0/Items/0]
Name=keyboard-us

[Groups/0/Items/1]
Name=rime

[Groups/1]
Name=Japanese

[Groups/1/Items/0]
Name=keyboard-jp

[Groups/1/Items/1]
Name=anthy
"#;

    #[test]
    fn profile_uses_only_selected_group_and_numeric_order() {
        assert_eq!(
            configured_fcitx_methods(PROFILE, "Default"),
            ["keyboard-us", "rime", "mozc"]
        );
        assert_eq!(
            configured_fcitx_methods(PROFILE, "Japanese"),
            ["keyboard-jp", "anthy"]
        );
        assert!(configured_fcitx_methods(PROFILE, "Unknown").is_empty());
    }

    #[test]
    fn next_method_wraps_and_rejects_unknown_entries() {
        let methods = ["keyboard-us", "rime", "mozc"];
        assert_eq!(next_method("rime", &methods), Ok("mozc"));
        assert_eq!(next_method("mozc", &methods), Ok("keyboard-us"));
        assert!(next_method("other", &methods).is_err());
        assert!(next_method("rime", &["rime"]).is_err());
    }

    #[test]
    fn ibus_engines_use_name_only_and_configured_order() {
        assert_eq!(
            ibus_engine_names("mozc-jp\nm17n:zh:pinyin\nxkb:us::eng\n"),
            ["mozc-jp", "m17n:zh:pinyin", "xkb:us::eng"]
        );
        assert_eq!(
            quoted_gvariant_strings("@as ['xkb:us::eng', 'm17n:zh:pinyin']"),
            ["xkb:us::eng", "m17n:zh:pinyin"]
        );
        assert!(quoted_gvariant_strings("@as []").is_empty());
    }
}
