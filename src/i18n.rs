use std::{collections::HashMap, sync::OnceLock};

use serde::Deserialize;

const EN_JSON: &str = include_str!("../locales/en.json");
const ZH_CN_JSON: &str = include_str!("../locales/zh-CN.json");

#[derive(Debug, Deserialize)]
struct Catalog {
    strings: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct I18n {
    language: String,
}

impl I18n {
    pub fn new(language: &str) -> Self {
        Self {
            language: normalize_language_code(language).to_owned(),
        }
    }

    pub fn set_language(&mut self, language: &str) {
        self.language = normalize_language_code(language).to_owned();
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn tr(&self, key: &str) -> String {
        catalogs()
            .get(self.language())
            .and_then(|catalog| catalog.get(key))
            .or_else(|| catalogs().get("en").and_then(|catalog| catalog.get(key)))
            .cloned()
            .unwrap_or_else(|| key.to_owned())
    }

    pub fn tr_args(&self, key: &str, args: &[(&str, String)]) -> String {
        let mut text = self.tr(key);
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    pub fn supported_languages() -> &'static [(&'static str, &'static str)] {
        &[("en", "English"), ("zh-CN", "简体中文")]
    }
}

pub fn detect_system_language() -> String {
    sys_locale::get_locale()
        .map(|locale| normalize_language_code(&locale).to_owned())
        .unwrap_or_else(|| "en".to_owned())
}

pub fn normalize_language_code(language: &str) -> &'static str {
    let lower = language.trim().to_ascii_lowercase();
    if lower.starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

fn catalogs() -> &'static HashMap<String, HashMap<String, String>> {
    static CATALOGS: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();
    CATALOGS.get_or_init(|| {
        HashMap::from([
            ("en".to_owned(), parse_catalog(EN_JSON)),
            ("zh-CN".to_owned(), parse_catalog(ZH_CN_JSON)),
        ])
    })
}

fn parse_catalog(json: &str) -> HashMap<String, String> {
    serde_json::from_str::<Catalog>(json)
        .expect("invalid locale file")
        .strings
}

#[cfg(test)]
mod tests {
    use super::{I18n, normalize_language_code};

    #[test]
    fn normalize_language_falls_back_to_english() {
        assert_eq!(normalize_language_code("fr-FR"), "en");
        assert_eq!(normalize_language_code("en-US"), "en");
        assert_eq!(normalize_language_code("zh"), "zh-CN");
    }

    #[test]
    fn translation_uses_fallback_and_args() {
        let i18n = I18n::new("zh-CN");
        let text = i18n.tr_args("status.detected_adb", &[("path", "C:/adb.exe".to_owned())]);
        assert!(text.contains("C:/adb.exe"));
    }
}
