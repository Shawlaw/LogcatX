const EN_JSON: &str = include_str!("../locales/en.json");
const ZH_CN_JSON: &str = include_str!("../locales/zh-CN.json");

#[derive(Clone, Debug)]
pub struct I18n {
    inner: desktop_i18n::I18n,
}

impl I18n {
    pub fn new(language: &str) -> Self {
        Self {
            inner: desktop_i18n::I18n::from_json_catalogs(
                language,
                "en",
                &[("en", EN_JSON), ("zh-CN", ZH_CN_JSON)],
            )
            .expect("invalid locale file"),
        }
    }

    pub fn set_language(&mut self, language: &str) {
        self.inner.set_language(language);
    }

    pub fn tr(&self, key: &str) -> String {
        self.inner.tr(key)
    }

    pub fn tr_args(&self, key: &str, args: &[(&str, String)]) -> String {
        self.inner.tr_args(key, args)
    }

    pub fn supported_languages() -> &'static [(&'static str, &'static str)] {
        &[("en", "English"), ("zh-CN", "简体中文")]
    }
}

pub fn detect_system_language() -> String {
    normalize_language_code(&desktop_i18n::detect_system_language()).to_owned()
}

pub fn normalize_language_code(language: &str) -> &'static str {
    match desktop_i18n::normalize_language_code(language, &["en", "zh-CN"], "en").as_str() {
        "zh-CN" => "zh-CN",
        _ => "en",
    }
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
