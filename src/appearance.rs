use crate::sdl::*;
use egui::Theme;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, ffi::CStr, path::Path};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod portal;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-Hans")]
    Simplified,
    #[serde(rename = "zh-Hant")]
    Traditional,
}

impl Language {
    pub const ALL: [Self; 3] = [Self::English, Self::Simplified, Self::Traditional];

    pub fn name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Simplified => "简体中文",
            Self::Traditional => "繁體中文",
        }
    }

    pub fn text(
        self,
        english: &'static str,
        hans: &'static str,
        hant: &'static str,
    ) -> &'static str {
        text(english, hans, hant).get(self)
    }

    fn from_locale(language: &str, country: &str) -> Option<Self> {
        let language = language.split(['.', '@']).next().unwrap_or_default();
        let mut parts = language.split(['-', '_']);
        let code = parts.next()?;
        if ["en", "c", "posix"]
            .iter()
            .any(|tag| code.eq_ignore_ascii_case(tag))
        {
            return Some(Self::English);
        }
        if !code.eq_ignore_ascii_case("zh") {
            return None;
        }
        let mut traditional_region = false;
        for part in parts.chain(country.split(['-', '_'])) {
            if part.eq_ignore_ascii_case("Hant") {
                return Some(Self::Traditional);
            }
            if part.eq_ignore_ascii_case("Hans") {
                return Some(Self::Simplified);
            }
            traditional_region |= ["TW", "HK", "MO"]
                .iter()
                .any(|tag| part.eq_ignore_ascii_case(tag));
        }
        Some(if traditional_region {
            Self::Traditional
        } else {
            Self::Simplified
        })
    }

    pub fn system() -> Self {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(language) = ["LC_ALL", "LC_MESSAGES"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
            .and_then(|locale| Self::from_locale(&locale, ""))
        {
            return language;
        }
        unsafe {
            let mut count = 0;
            let locales = SDL_GetPreferredLocales(&mut count);
            if locales.is_null() {
                return Self::English;
            }
            let name = |ptr: *const std::ffi::c_char| {
                if ptr.is_null() {
                    ""
                } else {
                    CStr::from_ptr(ptr).to_str().unwrap_or_default()
                }
            };
            let language = (0..count as usize).find_map(|i| {
                let locale = (*locales.add(i)).as_ref()?;
                Self::from_locale(name(locale.language), name(locale.country))
            });
            SDL_free(locales.cast());
            language.unwrap_or_default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Text([&'static str; 3]);

pub const fn text(english: &'static str, hans: &'static str, hant: &'static str) -> Text {
    Text([english, hans, hant])
}

impl Text {
    pub fn get(self, language: Language) -> &'static str {
        self.0[language as usize]
    }

    pub fn detail(self, detail: impl ToString) -> Status {
        Status {
            label: self,
            detail: detail.to_string(),
        }
    }
}

#[derive(Default)]
pub struct Status {
    label: Text,
    detail: String,
}

impl From<Text> for Status {
    fn from(label: Text) -> Self {
        Self {
            label,
            detail: String::new(),
        }
    }
}

impl Status {
    pub fn get(&self, language: Language) -> Cow<'_, str> {
        let label = self.label.get(language);
        if self.detail.is_empty() {
            Cow::Borrowed(label)
        } else {
            Cow::Owned(format!("{label} · {}", self.detail))
        }
    }
}

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
            Self::System => language.text("Follow system", "跟随系统", "跟隨系統"),
            Self::Light => language.text("Light", "浅色", "淺色"),
            Self::Dark => language.text("Dark", "深色", "深色"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: Option<Language>,
    pub theme: ThemeMode,
    pub restore_session: bool,
    pub per_display_windows: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: None,
            theme: ThemeMode::System,
            restore_session: true,
            per_display_windows: false,
        }
    }
}

impl Preferences {
    pub fn load(path: &Path) -> Self {
        if std::fs::metadata(path).is_ok_and(|m| m.len() <= 4096) {
            std::fs::read(path)
                .ok()
                .and_then(|data| serde_json::from_slice(&data).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let data = serde_json::to_vec(&self).map_err(|e| e.to_string())?;
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
            ThemeMode::System => match unsafe { SDL_GetSystemTheme() } {
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
        ] {
            assert_eq!(Language::from_locale(tag, region), Some(language), "{tag}");
        }
        assert_eq!(Language::from_locale("ja_JP", ""), None);
    }

    #[test]
    fn ui_preferences_default_to_system_and_roundtrip() {
        let default: Preferences = serde_json::from_str("{}").unwrap();
        assert_eq!(default, Preferences::default());
        assert_eq!(default.language, None);
        assert_eq!(default.theme, ThemeMode::System);
        assert!(default.restore_session);
        assert!(!default.per_display_windows);
        let manual = Preferences {
            language: Some(Language::Traditional),
            theme: ThemeMode::Dark,
            restore_session: false,
            per_display_windows: true,
        };
        assert_eq!(
            serde_json::from_str::<Preferences>(&serde_json::to_string(&manual).unwrap()).unwrap(),
            manual,
        );
    }
}
