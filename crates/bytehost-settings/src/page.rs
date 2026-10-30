//! 页面身份:经过校验的稳定字符串,不使用显示标题作标识。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{APPS_ID, LANGUAGE_ID, THEME_ID};

/// `bytehost.*` 保留给共享模块;产品只能用其他命名空间。
pub const BUILTIN_NAMESPACE: &str = "bytehost.";

/// 内置页固定 ID。
pub const BUILTIN_IDS: [&str; 3] = [THEME_ID, LANGUAGE_ID, APPS_ID];

/// 页面 ID 校验失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageIdError {
    /// 空字符串。
    Empty,
    /// 含不允许的字符(只允许小写 ASCII 字母、数字、`.`、`_`、`-`)。
    InvalidCharacter(char),
    /// 含空段(`.a`、`a.`、`a..b`)。
    EmptySegment,
    /// 段首不是字母或数字。
    InvalidSegmentStart(char),
}

impl fmt::Display for PageIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageIdError::Empty => write!(f, "page id must not be empty"),
            PageIdError::InvalidCharacter(c) => {
                write!(f, "page id contains an invalid character: {c:?}")
            }
            PageIdError::EmptySegment => write!(f, "page id contains an empty segment"),
            PageIdError::InvalidSegmentStart(c) => {
                write!(
                    f,
                    "page id segment must start with a letter or digit: {c:?}"
                )
            }
        }
    }
}

impl std::error::Error for PageIdError {}

/// 经过校验的页面 ID。
///
/// 允许的字符:小写 ASCII 字母、数字、`.`、`_`、`-`;每个以 `.` 分隔的段都必须
/// 非空且以字母或数字开头。ID 在发布后必须保持稳定。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PageId(String);

impl PageId {
    /// 校验并构造。
    pub fn new(raw: impl Into<String>) -> Result<Self, PageIdError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(PageIdError::Empty);
        }
        for segment in raw.split('.') {
            if segment.is_empty() {
                return Err(PageIdError::EmptySegment);
            }
            for c in segment.chars() {
                let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
                if !ok {
                    return Err(PageIdError::InvalidCharacter(c));
                }
            }
            let first = segment.chars().next().expect("non-empty segment");
            if !first.is_ascii_alphanumeric() {
                return Err(PageIdError::InvalidSegmentStart(first));
            }
        }
        Ok(Self(raw))
    }

    /// 内置页 ID:必须位于保留命名空间且属于已知集合。
    pub fn builtin(raw: impl Into<String>) -> Result<Self, PageIdError> {
        let id = Self::new(raw)?;
        debug_assert!(
            BUILTIN_IDS.contains(&id.as_str()),
            "内置页 ID 必须来自 BUILTIN_IDS"
        );
        Ok(id)
    }

    /// 产品自定义页 ID:拒绝冒充 `bytehost.*`。
    pub fn custom(raw: impl Into<String>) -> Result<Self, PageIdError> {
        let id = Self::new(raw)?;
        debug_assert!(
            !id.as_str().starts_with(BUILTIN_NAMESPACE),
            "产品页面不能使用保留命名空间"
        );
        Ok(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_reserved(&self) -> bool {
        self.0.starts_with(BUILTIN_NAMESPACE)
    }

    pub fn is_known_builtin(&self) -> bool {
        BUILTIN_IDS.contains(&self.0.as_str())
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for PageId {
    type Err = PageIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for PageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// 页面种类。内置页由共享层提供视图,自定义页由产品渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    BuiltIn,
    Custom,
}

/// 注册一页所需的最小描述。注册顺序就是导航顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDescriptor {
    pub id: PageId,
    pub kind: PageKind,
    /// `bytehost.settings` 文本 key;`None` 表示只用 `fallback`。
    pub text_key: Option<String>,
    /// 英文回退标题(无翻译时必须显示)。
    pub fallback: String,
}

impl PageDescriptor {
    pub fn built_in(id: PageId, fallback: impl Into<String>) -> Self {
        Self {
            id,
            kind: PageKind::BuiltIn,
            text_key: None,
            fallback: fallback.into(),
        }
    }

    pub fn custom(id: PageId, text_key: impl Into<String>, fallback: impl Into<String>) -> Self {
        Self {
            id,
            kind: PageKind::Custom,
            text_key: Some(text_key.into()),
            fallback: fallback.into(),
        }
    }

    pub fn with_text_key(mut self, text_key: impl Into<String>) -> Self {
        self.text_key = Some(text_key.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_product_and_builtin_ids() {
        for raw in [
            "bytehost.theme",
            "dozer.git",
            "digger.editor",
            "a1",
            "x_y-z",
        ] {
            assert!(PageId::new(raw).is_ok(), "{raw} 应合法");
        }
    }

    #[test]
    fn rejects_empty_and_malformed_ids() {
        assert_eq!(PageId::new(""), Err(PageIdError::Empty));
        assert_eq!(PageId::new("a..b"), Err(PageIdError::EmptySegment));
        assert_eq!(PageId::new(".a"), Err(PageIdError::EmptySegment));
        assert_eq!(PageId::new("a."), Err(PageIdError::EmptySegment));
        assert_eq!(
            PageId::new("Dozer.Git"),
            Err(PageIdError::InvalidCharacter('D'))
        );
        assert_eq!(
            PageId::new("dozer git"),
            Err(PageIdError::InvalidCharacter(' '))
        );
        assert_eq!(
            PageId::new("dozer.-git"),
            Err(PageIdError::InvalidSegmentStart('-'))
        );
    }

    #[test]
    fn reserved_namespace_is_detected() {
        assert!(PageId::new("bytehost.apps").unwrap().is_reserved());
        assert!(!PageId::new("dozer.apps").unwrap().is_reserved());
        assert!(PageId::new("bytehost.apps").unwrap().is_known_builtin());
        assert!(!PageId::new("bytehost.custom").unwrap().is_known_builtin());
    }

    #[test]
    fn serde_round_trip_revalidates() {
        let id: PageId = serde_json::from_str(r#""dozer.git""#).unwrap();
        assert_eq!(id.as_str(), "dozer.git");
        assert!(serde_json::from_str::<PageId>(r#""Dozer Git""#).is_err());
    }
}
