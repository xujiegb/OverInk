//! Language detection and UI translations; all locales share one catalog.
#![forbid(unsafe_code)]

use crate::sdl::preferred_locales;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

mod catalog;
mod de;
mod en;
mod es;
mod fr;
mod it;
mod ja;
mod ko;
mod nl;
mod pt;
mod ru;
mod tr;
mod zh_hans;
mod zh_hant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-Hans")]
    Simplified,
    #[serde(rename = "zh-Hant")]
    Traditional,
    #[serde(rename = "fr")]
    French,
    #[serde(rename = "es")]
    Spanish,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "de")]
    German,
    #[serde(rename = "ru")]
    Russian,
    #[serde(rename = "nl")]
    Dutch,
    #[serde(rename = "tr")]
    Turkish,
    #[serde(rename = "ko")]
    Korean,
    #[serde(rename = "pt")]
    Portuguese,
    #[serde(rename = "it")]
    Italian,
}

impl Language {
    pub const ALL: [Self; 13] = [
        Self::English,
        Self::Simplified,
        Self::Traditional,
        Self::French,
        Self::Spanish,
        Self::Japanese,
        Self::German,
        Self::Russian,
        Self::Dutch,
        Self::Turkish,
        Self::Korean,
        Self::Portuguese,
        Self::Italian,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Simplified => "简体中文",
            Self::Traditional => "繁體中文",
            Self::French => "Français",
            Self::Spanish => "Español",
            Self::Japanese => "日本語",
            Self::German => "Deutsch",
            Self::Russian => "Русский",
            Self::Dutch => "Nederlands",
            Self::Turkish => "Türkçe",
            Self::Korean => "한국어",
            Self::Portuguese => "Português",
            Self::Italian => "Italiano",
        }
    }

    pub fn text(self, english: &'static str) -> &'static str {
        text(english).get(self)
    }

    pub(crate) fn from_locale(language: &str, country: &str) -> Option<Self> {
        let language = language.split(['.', '@']).next().unwrap_or_default();
        let mut parts = language.split(['-', '_']);
        let code = parts.next()?;
        if ["en", "c", "posix"]
            .iter()
            .any(|tag| code.eq_ignore_ascii_case(tag))
        {
            return Some(Self::English);
        }
        if code.eq_ignore_ascii_case("fr") {
            return Some(Self::French);
        }
        for (tag, language) in [
            ("es", Self::Spanish),
            ("ja", Self::Japanese),
            ("de", Self::German),
            ("ru", Self::Russian),
            ("nl", Self::Dutch),
            ("tr", Self::Turkish),
            ("ko", Self::Korean),
            ("pt", Self::Portuguese),
            ("it", Self::Italian),
        ] {
            if code.eq_ignore_ascii_case(tag) {
                return Some(language);
            }
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
        preferred_locales()
            .into_iter()
            .find_map(|(language, country)| Self::from_locale(&language, &country))
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Text(&'static str);

pub const fn text(english: &'static str) -> Text {
    Text(english)
}

impl Text {
    pub fn get(self, language: Language) -> &'static str {
        let Some(index) = catalog::index(self.0) else {
            return self.0;
        };
        match language {
            Language::English => en::STRINGS[index],
            Language::Simplified => zh_hans::STRINGS[index],
            Language::Traditional => zh_hant::STRINGS[index],
            Language::French => fr::STRINGS[index],
            Language::Spanish => es::STRINGS[index],
            Language::Japanese => ja::STRINGS[index],
            Language::German => de::STRINGS[index],
            Language::Russian => ru::STRINGS[index],
            Language::Dutch => nl::STRINGS[index],
            Language::Turkish => tr::STRINGS[index],
            Language::Korean => ko::STRINGS[index],
            Language::Portuguese => pt::STRINGS[index],
            Language::Italian => it::STRINGS[index],
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_locales_share_complete_catalog() {
        for (index, english) in en::STRINGS.iter().enumerate() {
            assert_eq!(catalog::index(english), Some(index));
        }
        for entries in [
            &en::STRINGS,
            &zh_hans::STRINGS,
            &zh_hant::STRINGS,
            &fr::STRINGS,
            &es::STRINGS,
            &ja::STRINGS,
            &de::STRINGS,
            &ru::STRINGS,
            &nl::STRINGS,
            &tr::STRINGS,
            &ko::STRINGS,
            &pt::STRINGS,
            &it::STRINGS,
        ] {
            assert_eq!(entries.len(), 184);
            assert!(entries.iter().all(|value| !value.is_empty()));
        }
    }
    #[test]
    fn original_language_preferences_and_unknown_keys() {
        assert_eq!(Language::English.text("Settings"), "Settings");
        assert_eq!(Language::Simplified.text("Settings"), "设置");
        assert_eq!(Language::Traditional.text("Settings"), "設定");
        assert_eq!(Language::French.text("Settings"), "Réglages");
        assert_eq!(
            Language::French.text("Untranslated UI key"),
            "Untranslated UI key"
        );
    }
}
