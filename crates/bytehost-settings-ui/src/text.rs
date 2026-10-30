//! 共享文案解析接缝。
//!
//! 共享层不导入任何产品的 i18n crate。调用方注入 `(key, english_fallback) -> String`
//! 的解析函数;没有翻译时必须返回英文 fallback。key 属于 `bytehost.settings` 文本空间。

/// 文本空间 owner。产品适配层可据此前缀查自己的词表。
pub const OWNER: &str = "bytehost.settings";

/// 英文回退表:共享键的默认文案。
pub fn english_fallback(key: &str) -> Option<&'static str> {
    Some(match key {
        "settings.title" => "Settings",
        "settings.close" => "Close",
        "settings.empty" => "No settings pages are available.",
        "settings.theme.title" => "Theme",
        "settings.theme.dark" => "Dark · ByteBoy2077",
        "settings.theme.light" => "Light · ByteBoy2077-Light",
        "settings.language.title" => "Language",
        "settings.language.follow_system" => "Follow System",
        "settings.language.english" => "English",
        "settings.language.simplified_chinese" => "Simplified Chinese",
        "settings.language.saving" => "Saving…",
        "settings.language.save_failed" => "Couldn't save. Try again.",
        "settings.language.conflict" => "Language changed elsewhere — review your choice.",
        "settings.language.unsupported" => "This service doesn't support language settings.",
        _ => return None,
    })
}

/// 注入的文本解析器。
#[derive(Clone, Copy)]
pub struct Text<'a> {
    resolve: &'a dyn Fn(&str, &str) -> String,
}

impl<'a> Text<'a> {
    pub fn new(resolve: &'a dyn Fn(&str, &str) -> String) -> Self {
        Self { resolve }
    }

    /// 内置表的解析器:优先表内英文,其次调用方给的 fallback。
    pub fn builtin() -> Self {
        Self::new(&|key: &str, fallback: &str| english_fallback(key).unwrap_or(fallback).to_owned())
    }

    pub fn get(&self, key: &str, fallback: &str) -> String {
        (self.resolve)(key, fallback)
    }
}

fn identity(_: &str, fallback: &str) -> String {
    fallback.to_owned()
}

/// 一个永远返回 fallback 的解析器,便于测试与最小示例。
pub fn identity_resolver() -> &'static dyn Fn(&str, &str) -> String {
    &identity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_resolver_prefers_the_shared_table_then_the_fallback() {
        let text = Text::builtin();
        assert_eq!(text.get("settings.title", "ignored"), "Settings");
        assert_eq!(
            text.get("unknown.key", "English fallback"),
            "English fallback"
        );
    }

    #[test]
    fn injected_resolver_receives_the_english_fallback() {
        let resolver = |_key: &str, fallback: &str| format!("zh:{fallback}");
        let text = Text::new(&resolver);
        assert_eq!(text.get("settings.close", "Close"), "zh:Close");
    }
}
