#![forbid(unsafe_code)]

use crate::sdl::*;
use egui::Theme;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use crate::language::{Language, Status, Text, text};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod portal;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self, language: Language) -> &'static str {
        match self {
            Self::System => language.text("Follow system"),
            Self::Light => language.text("Light"),
            Self::Dark => language.text("Dark"),
        }
    }
}

/// Which on-screen keyboard opens when touching the text annotation's
/// keyboard icon. Native OSK is an explicit opt-in: GNOME-style OverInk keys
/// are the cross-platform default and never require accessibility permissions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardSource {
    #[default]
    OverInk,
    System,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: Option<Language>,
    pub theme: ThemeMode,
    pub restore_session: bool,
    pub per_display_windows: bool,
    pub wayland: bool,
    pub keyboard_source: KeyboardSource,
    pub screenshot_hdr: bool,
    pub screenshot_folder: Option<PathBuf>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: None,
            theme: ThemeMode::System,
            restore_session: true,
            per_display_windows: false,
            wayland: false,
            keyboard_source: KeyboardSource::OverInk,
            screenshot_hdr: false,
            screenshot_folder: None,
        }
    }
}

impl Preferences {
    /// Snapshot the panel controls without copying the owned screenshot path.
    pub fn panel_values(&self) -> Self {
        Self {
            screenshot_folder: None,
            ..*self
        }
    }

    pub fn load(path: &Path) -> Self {
        if std::fs::metadata(path).is_ok_and(|m| m.len() <= 256 * 1024) {
            std::fs::read(path)
                .ok()
                .and_then(|data| serde_json::from_slice(&data).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, data)
            .and_then(|()| std::fs::rename(&tmp, path))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
            .map_err(|e| e.to_string())
    }
}

#[derive(Default)]
pub struct ThemeMonitor {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    portal: Option<portal::Monitor>,
}

impl ThemeMonitor {
    pub fn theme(&mut self, mode: ThemeMode) -> Theme {
        let theme = match mode {
            ThemeMode::Light => Some(Theme::Light),
            ThemeMode::Dark => Some(Theme::Dark),
            ThemeMode::System => match system_theme() {
                SDL_SYSTEM_THEME_DARK => Some(Theme::Dark),
                SDL_SYSTEM_THEME_LIGHT => Some(Theme::Light),
                _ => None,
            },
        };
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if mode == ThemeMode::System {
            // The desktop portal supplies the user's preference even when
            // vendored SDL was built without libdbus headers.
            return self
                .portal
                .get_or_insert_with(portal::Monitor::start)
                .theme()
                .or(theme)
                .unwrap_or(Theme::Light);
        } else {
            self.portal = None;
        }
        theme.unwrap_or(Theme::Light)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_script_takes_precedence_over_region() {
        for (tag, region, language) in [
            ("zh_CN.UTF-8", "", Language::Simplified),
            ("zh", "HK", Language::Traditional),
            ("zh_MO", "", Language::Traditional),
            ("zh-Hant", "CN", Language::Traditional),
            ("zh-Hans", "TW", Language::Simplified),
            ("zh", "SG", Language::Simplified),
            ("en-GB", "", Language::English),
            ("fr-FR", "", Language::French),
            ("fr_CA.UTF-8", "", Language::French),
            ("fr", "CH", Language::French),
            ("es-MX", "", Language::Spanish),
            ("ja-JP", "", Language::Japanese),
            ("de-AT", "", Language::German),
            ("ru_RU.UTF-8", "", Language::Russian),
            ("nl-BE", "", Language::Dutch),
            ("tr-TR", "", Language::Turkish),
            ("ko_KR.UTF-8", "", Language::Korean),
            ("pt-PT", "", Language::Portuguese),
            ("pt-BR", "", Language::Portuguese),
            ("it-CH", "", Language::Italian),
        ] {
            assert_eq!(Language::from_locale(tag, region), Some(language), "{tag}");
        }
        assert_eq!(Language::from_locale("sv_SE", ""), None);
    }

    #[test]
    fn french_selection_is_backward_compatible_and_localized() -> serde_json::Result<()> {
        assert_eq!(serde_json::to_string(&Language::French)?, r#""fr""#);
        assert_eq!(
            serde_json::from_str::<Language>(r#""fr""#)?,
            Language::French
        );
        assert_eq!(
            serde_json::from_str::<Language>(r#""zh-Hant""#)?,
            Language::Traditional
        );
        assert_eq!(Language::French.name(), "Français");
        assert_eq!(Language::French.text("Settings"), "Réglages");
        assert_eq!(Language::French.text("Pick screen"), "Pipette");
        assert_eq!(ThemeMode::System.label(Language::French), "Système");
        assert_eq!(
            text("Save failed").get(Language::French),
            "Échec d’enregistrement"
        );
        Ok(())
    }

    #[test]
    fn nine_added_languages_roundtrip_and_translate() -> serde_json::Result<()> {
        for language in [
            Language::Spanish,
            Language::Japanese,
            Language::German,
            Language::Russian,
            Language::Dutch,
            Language::Turkish,
            Language::Korean,
            Language::Portuguese,
            Language::Italian,
        ] {
            let encoded = serde_json::to_string(&language)?;
            assert_eq!(serde_json::from_str::<Language>(&encoded)?, language);
            assert_ne!(
                language.text("Settings"),
                "Settings",
                "{} needs a localized label",
                language.name()
            );
            assert_ne!(
                language.text("Save"),
                "Save",
                "{} needs a localized button",
                language.name()
            );
        }
        // Old preferences must deserialize unchanged.
        for legacy in [r#""en""#, r#""zh-Hans""#, r#""zh-Hant""#, r#""fr""#] {
            assert!(serde_json::from_str::<Language>(legacy).is_ok());
        }
        Ok(())
    }

    #[test]
    fn ui_preferences_default_to_system_and_roundtrip() -> serde_json::Result<()> {
        let default: Preferences = serde_json::from_str("{}")?;
        assert_eq!(default, Preferences::default());
        assert_eq!(default.language, None);
        assert_eq!(default.theme, ThemeMode::System);
        assert!(default.restore_session);
        assert!(!default.per_display_windows);
        assert!(!default.wayland);
        assert_eq!(default.keyboard_source, KeyboardSource::OverInk);
        let manual = Preferences {
            language: Some(Language::Traditional),
            theme: ThemeMode::Dark,
            restore_session: false,
            per_display_windows: true,
            wayland: true,
            keyboard_source: KeyboardSource::System,
            screenshot_hdr: true,
            screenshot_folder: Some(PathBuf::from("OverInk Captures")),
        };
        let encoded = serde_json::to_string(&manual)?;
        assert_eq!(serde_json::from_str::<Preferences>(&encoded)?, manual);
        let legacy: Preferences = serde_json::from_str(r#"{"theme":"dark"}"#)?;
        assert_eq!(legacy.keyboard_source, KeyboardSource::OverInk);
        Ok(())
    }
}
