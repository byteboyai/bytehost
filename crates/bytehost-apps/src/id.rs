//! 应用 id 与版本号:两个经过校验的小类型。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 应用 id:稳定、小写、`[a-z0-9-]`,1–63 个字符,不以 `-` 开头或结尾。它同时是应用 origin 的主机名
/// (`<id>.localhost`)与磁盘目录名,所以字符集必须保守——这也让它天然不可能含路径分隔符。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AppId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    Empty,
    TooLong,
    BadChar(char),
    EdgeHyphen,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "应用 id 不能为空"),
            Self::TooLong => write!(f, "应用 id 最长 63 个字符"),
            Self::BadChar(c) => write!(f, "应用 id 只能含小写字母、数字和 '-',发现 {c:?}"),
            Self::EdgeHyphen => write!(f, "应用 id 不能以 '-' 开头或结尾"),
        }
    }
}

impl std::error::Error for IdError {}

impl AppId {
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(IdError::Empty);
        }
        if s.len() > 63 {
            return Err(IdError::TooLong);
        }
        if let Some(c) = s
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(IdError::BadChar(c));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(IdError::EdgeHyphen);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AppId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl From<AppId> for String {
    fn from(id: AppId) -> String {
        id.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `主.次.修订` 三段数字版本号(不支持预发布后缀:应用版本与宿主版本都只用这一种形式)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionError(pub String);

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "版本号必须是 主.次.修订 三段数字,收到 {:?}", self.0)
    }
}

impl std::error::Error for VersionError {}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub fn parse(s: &str) -> Result<Self, VersionError> {
        let bad = || VersionError(s.to_string());
        let mut it = s.split('.');
        let mut next = || -> Result<u32, VersionError> {
            let part = it.next().ok_or_else(bad)?;
            // 不允许前导零(`01.2.3` 与 `1.2.3` 会被当成同一个版本,目录名却不同)
            if part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let v = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        if it.next().is_some() {
            return Err(bad());
        }
        Ok(v)
    }
}

impl TryFrom<String> for Version {
    type Error = VersionError;
    fn try_from(s: String) -> Result<Self, VersionError> {
        Self::parse(&s)
    }
}

impl From<Version> for String {
    fn from(v: Version) -> String {
        v.to_string()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_accepts_lowercase_digits_and_inner_hyphens() {
        for ok in ["excalidraw", "a", "my-app-2", "0day", &"a".repeat(63)] {
            assert!(AppId::new(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn app_id_rejects_everything_that_could_escape_a_hostname_or_a_directory() {
        assert_eq!(AppId::new(""), Err(IdError::Empty));
        assert_eq!(AppId::new("a".repeat(64)), Err(IdError::TooLong));
        assert_eq!(AppId::new("-a"), Err(IdError::EdgeHyphen));
        assert_eq!(AppId::new("a-"), Err(IdError::EdgeHyphen));
        for bad in ["Excal", "a_b", "a.b", "a/b", "../x", "a b", "中文", "a:80"] {
            assert!(matches!(AppId::new(bad), Err(IdError::BadChar(_))), "{bad}");
        }
    }

    #[test]
    fn app_id_serde_round_trips_as_a_plain_string_and_validates_on_read() {
        let id = AppId::new("excalidraw").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"excalidraw\"");
        assert_eq!(serde_json::from_str::<AppId>("\"excalidraw\"").unwrap(), id);
        assert!(serde_json::from_str::<AppId>("\"../etc\"").is_err());
    }

    #[test]
    fn version_parses_three_numeric_parts_only() {
        assert_eq!(Version::parse("0.17.3").unwrap(), Version::new(0, 17, 3));
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1..3",
            "v1.2.3",
            "1.2.3-rc1",
            "-1.2.3",
            "1.2.99999999999",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn version_orders_numerically_not_lexically() {
        assert!(Version::new(0, 10, 0) > Version::new(0, 9, 0));
        assert!(Version::new(1, 0, 0) > Version::new(0, 99, 99));
        assert_eq!(Version::new(1, 2, 3).to_string(), "1.2.3");
    }

    #[test]
    fn version_serde_round_trips_as_a_string() {
        let v = Version::new(0, 17, 0);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"0.17.0\"");
        assert_eq!(serde_json::from_str::<Version>("\"0.17.0\"").unwrap(), v);
        assert!(serde_json::from_str::<Version>("\"0.17\"").is_err());
    }

    #[test]
    fn version_parts_cannot_have_leading_zeros() {
        for bad in ["01.2.3", "1.02.3", "1.2.03", "00.0.0"] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Version::parse("0.0.0").unwrap(), Version::new(0, 0, 0));
        assert_eq!(
            Version::parse("10.20.30").unwrap(),
            Version::new(10, 20, 30)
        );
    }
}
