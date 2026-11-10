// SPDX-License-Identifier: BSD-3-Clause
use std::{fs, path::PathBuf};

const UUID: &str = "overink-overlay@overink";

pub fn install_overlay() -> Result<(), String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })
        .ok_or("No absolute XDG_DATA_HOME or HOME directory")?;
    let extension = data.join("gnome-shell/extensions").join(UUID);
    fs::create_dir_all(&extension)
        .map_err(|error| format!("Creating {} failed ({error})", extension.display()))?;
    for (name, contents) in [
        (
            "extension.js",
            include_str!("../assets/gnome-shell/extension.js"),
        ),
        (
            "metadata.json",
            include_str!("../assets/gnome-shell/metadata.json"),
        ),
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
    println!("Installed {UUID} in {}", extension.display());
    println!("After logging back in to GNOME, run gnome-extensions enable {UUID}");
    Ok(())
}
