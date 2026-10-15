//! 语言标签子集校验与规范化、locale 协商。

use std::fmt;

/// 语言标签非法的原因(用于字段级错误)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocaleError {
    Empty,
    InvalidChar,
    TooManySubtags,
    EmptySegment,
    BadLanguage,
    BadScript,
    BadRegion,
    MisplacedSubtag,
}

impl fmt::Display for LocaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LocaleError::Empty => "语言标签不能为空",
            LocaleError::InvalidChar => "语言标签只能含 [A-Za-z0-9-],不接受下划线/点/路径字符",
            LocaleError::TooManySubtags => "语言标签子标签过多(不接受扩展/private-use)",
            LocaleError::EmptySegment => "语言标签不能有空段(如连续连字符)",
            LocaleError::BadLanguage => "language 必须是 2–8 个字母",
            LocaleError::BadScript => "script 必须是 4 个字母",
            LocaleError::BadRegion => "region 必须是 2 个字母或 3 个数字",
            LocaleError::MisplacedSubtag => "子标签位置/形状不符(language[-script[-region]])",
        };
        f.write_str(s)
    }
}

impl std::error::Error for LocaleError {}

/// 规范化一个语言标签:language 小写、script 首字母大写其余小写、region 大写。
///
/// 接受形式:`lang` | `lang-script` | `lang-region` | `lang-script-region`,
/// 其中 language=2–8 字母,script=4 字母,region=2 字母或 3 数字。拒绝下划线、点、路径字符、
/// 空段、扩展/private-use。规范化后用于比较与作为资源文件名。
pub fn normalize_locale(raw: &str) -> Result<String, LocaleError> {
    if raw.is_empty() {
        return Err(LocaleError::Empty);
    }
    if !raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(LocaleError::InvalidChar);
    }
    let parts: Vec<&str> = raw.split('-').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(LocaleError::EmptySegment);
    }
    if parts.len() > 3 {
        return Err(LocaleError::TooManySubtags);
    }
    let language = parts[0];
    if !(2..=8).contains(&language.len()) || !language.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(LocaleError::BadLanguage);
    }

    let mut out = language.to_ascii_lowercase();
    if parts.len() >= 2 {
        let second = parts[1];
        if second.len() == 4 && second.bytes().all(|b| b.is_ascii_alphabetic()) {
            out.push('-');
            out.push_str(&title_case(second));
            if parts.len() == 3 {
                let third = parts[2];
                if is_region(third) {
                    out.push('-');
                    out.push_str(&third.to_ascii_uppercase());
                } else {
                    return Err(LocaleError::BadRegion);
                }
            }
        } else if is_region(second) {
            out.push('-');
            out.push_str(&second.to_ascii_uppercase());
            if parts.len() == 3 {
                return Err(LocaleError::MisplacedSubtag);
            }
        } else {
            return Err(LocaleError::MisplacedSubtag);
        }
    }
    Ok(out)
}

/// 协商应用语言:ui_locale 规范化后在 `supported_locales` 中精确匹配则采用,否则回退 default。
/// 不做语言族/简繁隐式转换;未知宿主语言直接采用 default。
pub fn negotiate_locale(ui_locale: &str, supported: &[String], default_locale: &str) -> String {
    if let Ok(ui) = normalize_locale(ui_locale)
        && supported
            .iter()
            .any(|s| normalize_locale(s).ok().as_deref() == Some(ui.as_str()))
    {
        return ui;
    }
    normalize_locale(default_locale).unwrap_or_else(|_| default_locale.to_string())
}

fn is_region(s: &str) -> bool {
    (s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic()))
        || (s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}
