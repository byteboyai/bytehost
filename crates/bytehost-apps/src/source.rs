//! 应用来源的策略与落地(feature `server`)。
//!
//! 一期来源有:本机目录(`LocalDir`)、本机压缩包(`Archive`)、https URL 压缩包(`Url`)。
//! **信任级别由服务端按来源推导**(见 [`policy_for`]),客户端在 `Plan` 请求里自报的 `provenance`/`trust`
//! 只能**更严**,不能更松(见 [`effective`])——否则客户端只要自报 `Trusted` 就能把 URL 来的应用当本机应用装。
//!
//! [`stage_source`] 把来源"落地"成一个含 `manifest.toml` 的目录(供 `read_package` 读取):
//! - `LocalDir`:整棵目录拷进 staging;
//! - `Archive`:进程内安全解压(见 [`crate::archive`]),绝不交给系统 `tar`/`unzip`;
//! - `Url`:先下载(见 [`crate::runtime::managed`] 的 `Fetcher`)、按期望 sha256 钉死,再走压缩包路径。

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::archive::{self, ArchiveError, ArchiveKind, ExtractReport, Limits};
use crate::digest::sha256_file;
use crate::plan::{Provenance, TrustLevel};
use crate::proto::{AppSource, SourceInfo};

/// 解压硬限制(来源用生产口径)。
pub fn default_limits() -> Limits {
    Limits::default()
}

/// 一个来源推导出的策略:来源类别、信任级别,以及是否只允许静态应用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePolicy {
    pub provenance: Provenance,
    pub trust: TrustLevel,
    /// 网络来源只允许静态应用(见计划用户裁决)。
    pub static_only: bool,
}

/// 按来源推导策略(服务端唯一真相,**不看客户端自报值**)。
pub fn policy_for(source: &AppSource) -> SourcePolicy {
    match source {
        AppSource::LocalDir { .. } | AppSource::Archive { .. } => SourcePolicy {
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
            static_only: false,
        },
        AppSource::Url { .. } => SourcePolicy {
            provenance: Provenance::ThirdParty,
            trust: TrustLevel::Untrusted,
            static_only: true,
        },
    }
}

fn trust_rank(t: TrustLevel) -> u8 {
    match t {
        TrustLevel::Trusted => 0,
        TrustLevel::Untrusted => 1,
    }
}

fn provenance_rank(p: Provenance) -> u8 {
    match p {
        Provenance::Local => 0,
        Provenance::AgentGenerated => 1,
        Provenance::ThirdParty => 2,
    }
}

/// 合并客户端自报的 `(provenance, trust)` 与服务端推导的策略,**每一项都取更严格者**。
/// 即:信任只能升到 `Untrusted` 不能降回 `Trusted`;来源只能变到更"外部"不能变回 `Local`。
pub fn effective(
    requested: (Provenance, TrustLevel),
    policy: &SourcePolicy,
) -> (Provenance, TrustLevel) {
    let provenance = if provenance_rank(requested.0) >= provenance_rank(policy.provenance) {
        requested.0
    } else {
        policy.provenance
    };
    let trust = if trust_rank(requested.1) >= trust_rank(policy.trust) {
        requested.1
    } else {
        policy.trust
    };
    (provenance, trust)
}

/// 校验通过的 URL:主机与"不含查询串的展示串"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    /// 小写主机(不含端口)。
    pub host: String,
    /// 展示用:scheme + 主机 + 路径,**不含**查询串与片段(**绝不入日志**)。
    pub display: String,
}

/// URL / sha256 的校验错误。
#[derive(Debug)]
pub enum SourceError {
    InvalidUrl(String),
    /// sha256 期望值格式非法(不是 64 位十六进制)。
    Sha256Format(String),
    /// 下载内容的 sha256 与期望值不符。
    Sha256Mismatch {
        expected: String,
        actual: String,
    },
    /// 来源尚未启用(如 Task 2 阶段的 URL;Task 3 起不再出现)。
    NotEnabled(String),
    Io(io::Error),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(why) => write!(f, "URL 不合法:{why}"),
            Self::Sha256Format(s) => write!(f, "sha256 必须是 64 位十六进制:{s}"),
            Self::Sha256Mismatch { expected, actual } => {
                write!(
                    f,
                    "下载内容的 sha256 与期望不符(期望 {expected},实际 {actual})"
                )
            }
            Self::NotEnabled(what) => write!(f, "{what}"),
            Self::Io(e) => write!(f, "读取来源出错:{e}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl From<io::Error> for SourceError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 校验并解析一个 https URL。规则:scheme 必须是 https(大小写不敏感)、有主机、无 userinfo
/// (拒凭据:`https://u:p@host` 一律拒绝)、不含控制字符/空白、整串 ≤ 2048。
/// **不做**本机/内网地址过滤:内网分发与本机测试是合法场景(见计划已知局限),审批卡展示主机即可。
pub fn validate_url(url: &str) -> Result<ParsedUrl, SourceError> {
    let invalid = |why: &str| SourceError::InvalidUrl(why.to_string());
    if url.len() > 2048 {
        return Err(invalid("过长(上限 2048 字符)"));
    }
    if url.chars().any(|c| c.is_control() || c == ' ') {
        return Err(invalid("含控制字符或空白"));
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("HTTPS://"))
        .or_else(|| {
            // scheme 大小写不敏感:`Https://` 等。
            let (scheme, rest) = url.split_once("://")?;
            if scheme.eq_ignore_ascii_case("https") {
                Some(rest)
            } else {
                None
            }
        })
        .ok_or_else(|| invalid("只支持 https"))?;
    // userinfo 与主机之间:第一个 '/' '?' '#' 之前是 authority。
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(invalid("缺少主机"));
    }
    if authority.contains('@') {
        return Err(invalid("不接受带凭据(user:pass@)的 URL"));
    }
    // 主机(去掉端口):IPv6 字面量 `[::1]:8443` 也允许。
    let host = if let Some(close) = authority.find(']') {
        authority[..=close].to_string()
    } else {
        authority.split(':').next().unwrap_or_default().to_string()
    };
    if host.is_empty() {
        return Err(invalid("缺少主机"));
    }
    let path = &rest[authority_end..];
    let path_no_query = path.split(['?', '#']).next().unwrap_or("");
    let display = format!("https://{}{}", authority, path_no_query);
    Ok(ParsedUrl {
        host: host.to_ascii_lowercase(),
        display,
    })
}

/// 规范化用户提供的期望 sha256:去首尾空白、转小写、必须是 64 位十六进制。
pub fn normalize_sha256(s: &str) -> Result<String, SourceError> {
    let t = s.trim();
    if t.len() != 64 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SourceError::Sha256Format(t.to_string()));
    }
    Ok(t.to_ascii_lowercase())
}

/// `stage_source` 的结果:落地信息 + 解压报告(目录来源没有解压报告)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedSource {
    pub info: SourceInfo,
}

/// 把一个来源落地到 `staging`(必须不存在或为空目录),返回计划要披露的来源信息。
///
/// `LocalDir` 走 [`copy_tree_dir`];`Archive` 走 [`crate::archive::extract`];`Url` 见 Task 3。
pub fn stage_source(
    source: &AppSource,
    staging: &Path,
    limits: &Limits,
) -> Result<StagedSource, StageError> {
    match source {
        AppSource::LocalDir { path } => {
            ensure_absolute(path).map_err(StageError::Source)?;
            copy_tree_dir(path, staging).map_err(StageError::Io)?;
            Ok(StagedSource {
                info: SourceInfo {
                    kind: "local_dir".into(),
                    display: path.to_string_lossy().into_owned(),
                    ..SourceInfo::default()
                },
            })
        }
        AppSource::Archive { path } => {
            ensure_absolute(path).map_err(StageError::Source)?;
            let kind = ArchiveKind::from_path(path).ok_or_else(|| {
                StageError::Source(SourceError::InvalidUrl(format!(
                    "不认识的压缩包扩展名:{}",
                    path.display()
                )))
            })?;
            let report =
                archive::extract(path, kind, staging, limits).map_err(StageError::Archive)?;
            let sha = sha256_file(path).map_err(StageError::Io)?;
            let bytes = fs::metadata(path).map_err(StageError::Io)?.len();
            Ok(StagedSource {
                info: archive_info(
                    "archive",
                    path.to_string_lossy().into_owned(),
                    sha,
                    bytes,
                    report,
                ),
            })
        }
        AppSource::Url { .. } => Err(StageError::Source(SourceError::NotEnabled(
            "URL 来源尚未启用".into(),
        ))),
    }
}

fn archive_info(
    kind: &str,
    display: String,
    sha: String,
    bytes: u64,
    report: ExtractReport,
) -> SourceInfo {
    SourceInfo {
        kind: kind.to_string(),
        display,
        archive_sha256: Some(sha),
        archive_bytes: Some(bytes),
        effective_host: None,
        pinned: false,
        stripped_top_dir: report.stripped_top_dir,
    }
}

/// `stage_source` 的错误,由调用方映射成 `ManagerError`。
#[derive(Debug)]
pub enum StageError {
    /// 来源不合法/尚未启用/URL 校验失败(用户可修正)。
    Source(SourceError),
    /// 解压失败。
    Archive(ArchiveError),
    /// 落地时的 I/O(拷贝、读元数据)。
    Io(io::Error),
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(e) => write!(f, "{e}"),
            Self::Archive(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StageError {}

/// `LocalDir` 必须是绝对路径:相对路径会按 dozerd 的工作目录解析,调用方并不知道那是哪里。
fn ensure_absolute(path: &Path) -> Result<(), SourceError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(SourceError::InvalidUrl(format!(
            "路径必须是绝对路径,收到 {}",
            path.display()
        )))
    }
}

/// 递归拷贝目录。只拷普通文件和目录:符号链接/FIFO/设备一律拒绝(与 `digest_tree` 同口径)。
/// 与 `manager::copy_tree` 同实现;`Archive`/`Url` 落地后由压缩包路径处理,这里只服务 `LocalDir`。
fn copy_tree_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree_dir(&entry.path(), &to)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &to)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("应用包里只允许普通文件和目录: {}", entry.path().display()),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_accepts_https_and_rejects_everything_else() {
        let ok = |u: &str| validate_url(u).unwrap_or_else(|e| panic!("{u}: {e}"));
        assert_eq!(ok("https://example.com/a.zip").host, "example.com");
        assert_eq!(ok("HTTPS://Example.COM/a.zip").host, "example.com");
        assert_eq!(ok("Https://example.com/a.zip").host, "example.com");
        assert_eq!(ok("https://example.com:8443/a.zip").host, "example.com");
        assert_eq!(ok("https://192.168.1.5/a.zip").host, "192.168.1.5");
        assert_eq!(ok("https://[::1]:8443/a.zip").host, "[::1]");
        // 展示串剥掉查询串与片段。
        let p = ok("https://example.com/a.zip?token=SECRET#frag");
        assert_eq!(p.display, "https://example.com/a.zip");
        assert!(!p.display.contains("SECRET"));

        let bad = |u: &str| assert!(validate_url(u).is_err(), "{u}");
        bad("http://example.com/a.zip");
        bad("file:///etc/passwd");
        bad("ftp://example.com/a.zip");
        bad("https://u:p@host/a.zip");
        bad("https://");
        bad("https://?x");
        bad("https://exa\nmple.com/a.zip");
        bad("https://exa mple.com/a.zip");
        bad("https://\u{7}example.com/a.zip");
        let long = format!("https://example.com/{}", "a".repeat(2100));
        bad(&long);
    }

    #[test]
    fn normalize_sha256_is_case_and_whitespace_insensitive() {
        let hex = "A".repeat(64);
        assert_eq!(
            normalize_sha256(&format!("  {hex}\n")).unwrap(),
            "a".repeat(64)
        );
        assert_eq!(normalize_sha256(&"a".repeat(64)).unwrap(), "a".repeat(64));
        assert!(normalize_sha256(&"a".repeat(63)).is_err());
        assert!(normalize_sha256(&"a".repeat(65)).is_err());
        assert!(normalize_sha256("g").is_err());
        assert!(normalize_sha256(&"z".repeat(64)).is_err());
    }

    #[test]
    fn policy_derives_trust_from_the_source_not_the_client() {
        let dir = AppSource::LocalDir { path: "/x".into() };
        let arch = AppSource::Archive {
            path: "/x.zip".into(),
        };
        let url = AppSource::Url {
            url: "https://example.com/a.zip".into(),
            sha256: None,
        };
        assert_eq!(
            policy_for(&dir),
            SourcePolicy {
                provenance: Provenance::Local,
                trust: TrustLevel::Trusted,
                static_only: false
            }
        );
        assert_eq!(policy_for(&arch).trust, TrustLevel::Trusted);
        let u = policy_for(&url);
        assert_eq!(u.provenance, Provenance::ThirdParty);
        assert_eq!(u.trust, TrustLevel::Untrusted);
        assert!(u.static_only);
    }

    #[test]
    fn effective_only_tightens_never_loosens() {
        // 客户端把 URL 自报成 Local/Trusted → 生效仍是 ThirdParty/Untrusted。
        let url = policy_for(&AppSource::Url {
            url: "https://e/x.zip".into(),
            sha256: None,
        });
        assert_eq!(
            effective((Provenance::Local, TrustLevel::Trusted), &url),
            (Provenance::ThirdParty, TrustLevel::Untrusted)
        );
        // 本机目录自报 ThirdParty/Untrusted → 保留更严格的。
        let dir = policy_for(&AppSource::LocalDir { path: "/x".into() });
        assert_eq!(
            effective((Provenance::ThirdParty, TrustLevel::Untrusted), &dir),
            (Provenance::ThirdParty, TrustLevel::Untrusted)
        );
        // 本机目录自报 Local/Trusted → 与策略一致。
        assert_eq!(
            effective((Provenance::Local, TrustLevel::Trusted), &dir),
            (Provenance::Local, TrustLevel::Trusted)
        );
    }
}
