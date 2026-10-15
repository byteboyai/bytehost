//! 标题引用与解析(纯逻辑,资源由 T2 注入)。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 一个标题声明:资源 key + 必填 fallback。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleRef {
    pub key: String,
    pub fallback: String,
}

/// 已装载的、按规范化 locale 归类的字符串表。
///
/// T1 仅用内存表测试解析顺序;T2 的包内资源装载器把 JSON 注入 `locales`
/// (键为 [`normalize_locale`](crate::i18n::normalize_locale) 后的规范标签)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct I18nBundle {
    /// locale(规范化后) -> (key -> value)
    pub locales: BTreeMap<String, BTreeMap<String, String>>,
}

impl I18nBundle {
    /// 标题解析顺序:当前语言 key → default 语言 key → fallback。
    /// 空值、或虽非空但不满足标题字符安全(>64 字符 / 含控制字符或双向控制字符)的值视为缺失,走下一步。
    pub fn resolve_title(&self, r: &TitleRef, current: &str, default: &str) -> String {
        for loc in [current, default] {
            if let Some(map) = self.locales.get(loc)
                && let Some(v) = map.get(&r.key)
                && is_title_safe(v)
            {
                return v.clone();
            }
        }
        r.fallback.clone()
    }
}

/// 用作外壳标题的值必须是非空、≤64 Unicode 字符、不含控制字符或双向控制字符。
/// 不满足的值在 [`I18nBundle::resolve_title`] 中按缺失处理,回退到 default 语言或 fallback。
fn is_title_safe(v: &str) -> bool {
    !v.is_empty() && v.chars().count() <= 64 && !v.chars().any(|c| c.is_control() || is_bidi(c))
}

/// 双向控制字符(U+200E/200F、U+202A–202E、U+2066–2069):能让文本在界面上"看起来"是另一个样子。
fn is_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}
