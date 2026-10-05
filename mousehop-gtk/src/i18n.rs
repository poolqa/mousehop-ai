//! Bundled UI translations. English messages are stable catalog keys.
use std::{path::PathBuf, sync::OnceLock};

const CATALOG: &str = include_str!("../translations/zh-TW.tsv");
static LANGUAGE: OnceLock<Language> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    System,
    English,
    TraditionalChinese,
}

impl Language {
    pub(crate) fn parse(value: &str) -> Self {
        match value.trim() {
            "en" => Self::English,
            "zh-TW" => Self::TraditionalChinese,
            _ => Self::System,
        }
    }
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::TraditionalChinese => "zh-TW",
        }
    }
    pub(crate) fn index(self) -> u32 {
        match self {
            Self::System => 0,
            Self::English => 1,
            Self::TraditionalChinese => 2,
        }
    }
}

fn preference_path() -> PathBuf {
    gtk::glib::user_config_dir()
        .join("mousehop")
        .join("language")
}

pub(crate) fn preference() -> Language {
    std::fs::read_to_string(preference_path())
        .map(|s| Language::parse(&s))
        .unwrap_or(Language::System)
}

pub(crate) fn save(language: Language) -> std::io::Result<()> {
    let path = preference_path();
    std::fs::create_dir_all(path.parent().expect("language directory"))?;
    std::fs::write(path, language.code())
}

pub(crate) fn is_traditional_locale(locale: &str) -> bool {
    let normalized = locale.to_ascii_lowercase().replace('_', "-");
    let locale = normalized.split(['.', '@']).next().unwrap_or("");
    if locale.split('-').any(|part| part == "hans") {
        return false;
    }
    locale.starts_with("zh-")
        && locale
            .split('-')
            .any(|part| matches!(part, "hant" | "tw" | "hk" | "mo"))
}

fn system_language() -> Language {
    // GLib uses the platform language preferences, including Windows UI locale.
    // macOS GUI launches often have no LANG; AppleLanguages supplies that case.
    #[cfg(target_os = "macos")]
    if std::env::var_os("LANGUAGE").is_none()
        && std::env::var_os("LC_ALL").is_none()
        && std::env::var_os("LC_MESSAGES").is_none()
        && std::env::var_os("LANG").is_none()
    {
        if let Ok(output) = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
        {
            if let Some(locale) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|s| s.trim().trim_matches(['"', ',', ' ']))
                .find(|s| !s.is_empty() && *s != "(" && *s != ")")
            {
                return if is_traditional_locale(locale) {
                    Language::TraditionalChinese
                } else {
                    Language::English
                };
            }
        }
    }
    let locale = gtk::glib::language_names()
        .into_iter()
        .find(|s| s.as_str() != "C");
    if locale.is_some_and(|s| is_traditional_locale(&s)) {
        Language::TraditionalChinese
    } else {
        Language::English
    }
}

pub(crate) fn language() -> Language {
    *LANGUAGE.get_or_init(|| match preference() {
        Language::System => system_language(),
        language => language,
    })
}

pub(crate) fn translate(message: &str, language: Language) -> &str {
    if language == Language::TraditionalChinese {
        for line in CATALOG.lines() {
            if let Some((key, value)) = line.split_once('\t') {
                if key == message {
                    return value;
                }
            }
        }
    }
    message
}

pub(crate) fn tr(message: &str) -> &str {
    translate(message, language())
}

/// Named placeholders keep addresses, counts, and markup out of translations.
pub(crate) fn text(message: &str, values: &[(&str, String)]) -> String {
    let mut output = tr(message).to_owned();
    for (key, value) in values {
        output = output.replace(&format!("{{{key}}}"), value);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translated_resource_contains_long_descriptions_markup_and_language_options() {
        let bytes = gtk::glib::Bytes::from_static(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/mousehop-zh-TW.gresource"
        )));
        let resource = gtk::gio::Resource::from_data(&bytes).unwrap();
        let data = resource
            .lookup_data(
                "/com/mousehop/Mousehop/window.ui",
                gtk::gio::ResourceLookupFlags::NONE,
            )
            .unwrap();
        let xml = std::str::from_utf8(&data).unwrap();
        assert!(xml.contains("語言"));
        assert!(xml.contains("跟隨系統"));
        assert!(xml.contains("English"));
        assert!(xml.contains("繁體中文"));
        assert!(xml.contains("當對端無法回報游標"));
        assert!(xml.contains("無法取得主機名稱"));
        assert!(!xml.contains("can't tell us"));
        let data = resource
            .lookup_data(
                "/com/mousehop/Mousehop/authorization_window.ui",
                gtk::gio::ResourceLookupFlags::NONE,
            )
            .unwrap();
        assert!(std::str::from_utf8(&data).unwrap().contains("未授權的裝置"));
    }

    #[test]
    fn locales_distinguish_traditional_and_simplified_chinese() {
        for locale in ["zh_TW.UTF-8", "zh-Hant", "zh-Hant-TW", "zh_HK", "zh-MO"] {
            assert!(is_traditional_locale(locale), "{locale}");
        }
        for locale in ["en_US", "zh_CN", "zh-Hans-CN", "zh-Hans-TW", "C"] {
            assert!(!is_traditional_locale(locale), "{locale}");
        }
    }
    #[test]
    fn catalog_keys_are_unique_and_nonempty() {
        let mut keys = std::collections::HashSet::new();
        for line in CATALOG.lines() {
            let (key, value) = line.split_once('\t').expect("key and translation");
            assert!(keys.insert(key), "duplicate {key}");
            assert!(!value.is_empty());
            let placeholders = |s: &str| {
                s.split('{')
                    .skip(1)
                    .map(|s| s.split('}').next().unwrap().to_owned())
                    .collect::<std::collections::HashSet<_>>()
            };
            assert_eq!(placeholders(key), placeholders(value), "{key}");
        }
    }
    #[test]
    fn translations_fall_back_to_english() {
        assert_eq!(translate("Language", Language::TraditionalChinese), "語言");
        assert_eq!(translate("Language", Language::English), "Language");
        assert_eq!(
            translate("new message", Language::TraditionalChinese),
            "new message"
        );
        assert_eq!(Language::parse("invalid"), Language::System);
    }
}
