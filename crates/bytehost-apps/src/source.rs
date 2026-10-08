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
use std::sync::atomic::AtomicBool;

use crate::archive::{self, ArchiveError, ArchiveKind, ExtractReport, Limits};
use crate::digest::sha256_file;
use crate::plan::{Provenance, TrustLevel};
use crate::proto::{AppSource, SourceInfo};
use crate::runtime::managed::Fetcher;

/// 应用包(归档)的大小上限:200 MiB。也用作 curl 的 `--max-filesize`。
pub const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

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
        return Err(invalid("URL 不接受内嵌凭据"));
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
/// `LocalDir` 走 [`copy_tree_dir`];`Archive` 走 [`crate::archive::extract`];`Url` 先下载到
/// `downloads/` 里的缓存、按期望 sha256 钉死,再走压缩包路径。
pub fn stage_source(
    source: &AppSource,
    staging: &Path,
    limits: &Limits,
    fetcher: &dyn Fetcher,
    downloads: &Path,
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
            let kind = kind_of(path)?;
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
        AppSource::Url { url, sha256 } => {
            stage_url(url, sha256.as_deref(), staging, limits, fetcher, downloads)
        }
    }
}

fn kind_of(path: &Path) -> Result<ArchiveKind, StageError> {
    ArchiveKind::from_path(path).ok_or_else(|| {
        StageError::Source(SourceError::InvalidUrl(format!(
            "不认识的压缩包扩展名:{}",
            path.display()
        )))
    })
}

/// 下载一个 https URL 压缩包、钉死 sha256、解压到 `staging`。
///
/// 缓存布局:`downloads/<uuid>.part` 是下载中的临时文件;完成后改名 `downloads/<sha256>.bin`。
/// 计划里披露的 `archive_sha256` 就是缓存文件名。
fn stage_url(
    url: &str,
    expected: Option<&str>,
    staging: &Path,
    limits: &Limits,
    fetcher: &dyn Fetcher,
    downloads: &Path,
) -> Result<StagedSource, StageError> {
    let parsed = validate_url(url).map_err(StageError::Source)?;
    // 期望 sha256 格式先校验——格式错误时**不发起下载**(省一次网络往返)。
    let expected = expected
        .map(normalize_sha256)
        .transpose()
        .map_err(StageError::Source)?;

    fs::create_dir_all(downloads).map_err(StageError::Io)?;
    let part = downloads.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
    let cancel = AtomicBool::new(false);
    let mut no_progress = |_: u64, _: Option<u64>| {};
    let fetched = fetcher
        .fetch_meta(url, &part, MAX_ARCHIVE_BYTES, &mut no_progress, &cancel)
        .map_err(|e| {
            // 失败/取消:不留半成品。
            let _ = fs::remove_file(&part);
            StageError::Io(e)
        })?;

    // 防御性:即便 curl 参数被改,最终 URL 也必须仍是 https。
    let final_url = validate_url(&fetched.effective_url).map_err(|_| {
        let _ = fs::remove_file(&part);
        StageError::Source(SourceError::InvalidUrl(
            "下载重定向到了非 https 地址".into(),
        ))
    })?;

    let sha = sha256_file(&part).map_err(|e| {
        let _ = fs::remove_file(&part);
        StageError::Io(e)
    })?;
    let bytes = fs::metadata(&part).map_err(StageError::Io)?.len();

    let pinned = match &expected {
        Some(want) if *want != sha => {
            let _ = fs::remove_file(&part);
            return Err(StageError::Source(SourceError::Sha256Mismatch {
                expected: want.clone(),
                actual: sha,
            }));
        }
        Some(_) => true,
        None => false,
    };

    // 改名进缓存(以实际 sha256 命名),再从缓存解压。
    let cached = downloads.join(format!("{sha}.bin"));
    let _ = fs::remove_file(&cached);
    fs::rename(&part, &cached).map_err(|e| {
        let _ = fs::remove_file(&part);
        StageError::Io(e)
    })?;

    let kind = kind_from_url(&final_url.display);
    let report = archive::extract(&cached, kind, staging, limits).map_err(|e| {
        let _ = fs::remove_file(&cached);
        StageError::Archive(e)
    })?;

    Ok(StagedSource {
        info: SourceInfo {
            kind: "url".into(),
            display: parsed.display,
            archive_sha256: Some(sha),
            archive_bytes: Some(bytes),
            effective_host: Some(final_url.host),
            pinned,
            stripped_top_dir: report.stripped_top_dir,
        },
    })
}

/// URL 结尾定压缩类型:`.tgz`/`.tar.gz` → tar.gz,其余按 `.zip`。查询串已在显示串里剥掉。
fn kind_from_url(display: &str) -> ArchiveKind {
    let lower = display.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        ArchiveKind::TarGz
    } else {
        ArchiveKind::Zip
    }
}

/// 已缓存的归档(按计划里的 sha256 找 `downloads/<sha>.bin`),读出重算 sha256 校验后解压。
/// 缓存缺失或损坏时返回 `Ok(None)`,由调用方决定是否重新下载。
pub fn extract_cached_archive(
    downloads: &Path,
    archive_sha256: &str,
    staging: &Path,
    limits: &Limits,
    kind: ArchiveKind,
) -> Result<Option<ExtractReport>, StageError> {
    let cached = downloads.join(format!("{archive_sha256}.bin"));
    if !cached.is_file() {
        return Ok(None);
    }
    let actual = sha256_file(&cached).map_err(StageError::Io)?;
    if actual != archive_sha256 {
        // 缓存被改过:当作没有,交给调用方重新下载。
        return Ok(None);
    }
    let report = archive::extract(&cached, kind, staging, limits).map_err(StageError::Archive)?;
    Ok(Some(report))
}

/// 删除某个归档的下载缓存(安装成功/失败、或计划作废时调用)。
pub fn discard_cached_archive(downloads: &Path, archive_sha256: &str) {
    let _ = fs::remove_file(downloads.join(format!("{archive_sha256}.bin")));
}

/// 把一次下载结果写进缓存,返回实际 sha256 与字节数(供 `install` 复用)。
pub struct Downloaded {
    pub sha256: String,
    pub bytes: u64,
    pub effective_host: String,
    pub display: String,
}

/// 仅下载(不落地):`install` 若发现缓存缺失,按计划里的 sha256 重新下载并核对。
pub fn download_to_cache(
    url: &str,
    fetcher: &dyn Fetcher,
    downloads: &Path,
) -> Result<Downloaded, StageError> {
    let parsed = validate_url(url).map_err(StageError::Source)?;
    fs::create_dir_all(downloads).map_err(StageError::Io)?;
    let part = downloads.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
    let cancel = AtomicBool::new(false);
    let mut no_progress = |_: u64, _: Option<u64>| {};
    let fetched = fetcher
        .fetch_meta(url, &part, MAX_ARCHIVE_BYTES, &mut no_progress, &cancel)
        .map_err(|e| {
            let _ = fs::remove_file(&part);
            StageError::Io(e)
        })?;
    let final_url = validate_url(&fetched.effective_url).map_err(|_| {
        let _ = fs::remove_file(&part);
        StageError::Source(SourceError::InvalidUrl(
            "下载重定向到了非 https 地址".into(),
        ))
    })?;
    let sha = sha256_file(&part).map_err(|e| {
        let _ = fs::remove_file(&part);
        StageError::Io(e)
    })?;
    let bytes = fs::metadata(&part).map_err(StageError::Io)?.len();
    let cached = downloads.join(format!("{sha}.bin"));
    let _ = fs::remove_file(&cached);
    fs::rename(&part, &cached).map_err(|e| {
        let _ = fs::remove_file(&part);
        StageError::Io(e)
    })?;
    Ok(Downloaded {
        sha256: sha,
        bytes,
        effective_host: final_url.host,
        display: parsed.display,
    })
}

/// 从已下载并落缓存的归档解压到 `staging`(安装阶段用;调用方先确认缓存 sha 匹配)。
pub fn extract_from_cache(
    downloads: &Path,
    archive_sha256: &str,
    display: &str,
    staging: &Path,
    limits: &Limits,
) -> Result<SourceInfo, StageError> {
    let kind = kind_from_url(display);
    let cached = downloads.join(format!("{archive_sha256}.bin"));
    let report = archive::extract(&cached, kind, staging, limits).map_err(StageError::Archive)?;
    let bytes = fs::metadata(&cached).map_err(StageError::Io)?.len();
    Ok(archive_info(
        "url",
        display.to_string(),
        archive_sha256.to_string(),
        bytes,
        report,
    ))
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
