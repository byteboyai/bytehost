//! Manifest v1:应用对宿主的**申请**。字段、取值与校验规则见 `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md` §4.1。
//! 所有结构 `deny_unknown_fields`;`validate` 一次性收集全部问题,不在第一个问题处停下。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::i18n::{I18n, normalize_locale};
use crate::id::{AppId, Version};
use crate::permissions::Permissions;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    /// 能运行这个应用的最低宿主版本。
    pub min_host_version: Version,
    pub id: AppId,
    pub name: String,
    pub version: Version,
    pub presentation: Presentation,
    pub entrypoints: BTreeMap<String, Entrypoint>,
    pub runtime: Runtime,
    /// 可选 i18n 声明;声明后 `title_key`/`title_fallback` 才能成对出现。
    #[serde(default)]
    pub i18n: Option<I18n>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presentation {
    /// 相对应用包根目录的图标路径。
    pub icon: Option<String>,
    /// 中性的承载提示(如 `browser`),宿主产品可以忽略;不使用 `browser_panel` 这类产品词。
    pub surface_hint: Option<String>,
    /// 默认入口,必须是 `entrypoints` 里的键。
    pub entrypoint: String,
    /// 标题资源 key;与 `title_fallback` 成对必填,且必须有 `[i18n]`。
    #[serde(default)]
    pub title_key: Option<String>,
    /// 标题 fallback;与 `title_key` 成对必填。非空、最长 64 个 Unicode 字符、不含控制/双向控制字符。
    #[serde(default)]
    pub title_fallback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entrypoint {
    #[serde(rename = "type")]
    pub kind: EntrypointKind,
    /// 应用内路径,以 `/` 开头。
    pub path: String,
    pub title: Option<String>,
    /// 标题资源 key;与 `title_fallback` 成对必填,且必须有 `[i18n]`。
    #[serde(default)]
    pub title_key: Option<String>,
    /// 标题 fallback;与 `title_key` 成对必填。
    #[serde(default)]
    pub title_fallback: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointKind {
    Web,
}

/// 进程型应用怎么对外提供 HTTP:宿主把端口放进这个环境变量传给应用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessHttp {
    pub port_env: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerHttp {
    pub container_port: u16,
}

/// 运行方式。一期只实现 `StaticWeb`;其余只定义形状,供 adapter 的 `probe` 与安装计划使用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Runtime {
    StaticWeb {
        /// 相对应用包根目录的静态文件目录。
        source: String,
    },
    Node {
        command: Vec<String>,
        lockfile: Option<String>,
        node: Option<String>,
        http: ProcessHttp,
    },
    Python {
        command: Vec<String>,
        lockfile: Option<String>,
        python: Option<String>,
        http: ProcessHttp,
    },
    Container {
        /// 必须按摘要固定:`name@sha256:<64 位十六进制>`。
        image: String,
        http: ContainerHttp,
    },
}

impl Runtime {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::StaticWeb { .. } => "static_web",
            Self::Node { .. } => "node",
            Self::Python { .. } => "python",
            Self::Container { .. } => "container",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Health {
    pub path: String,
    pub timeout_ms: u64,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            path: "/".to_string(),
            timeout_ms: 3000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// TOML 语法错误、缺字段、未知字段、取值不在枚举内。
    Parse(String),
    /// 语法合法,但有一个或多个语义问题。
    Invalid(Vec<String>),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "manifest 解析失败: {e}"),
            Self::Invalid(problems) => write!(f, "manifest 不合法: {}", problems.join("; ")),
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// 解析 TOML 文本并校验。`host_version` 用来检查 `min_host_version`。
    #[cfg(feature = "manifest-toml")]
    pub fn from_toml(text: &str, host_version: &Version) -> Result<Self, ManifestError> {
        let manifest: Manifest =
            toml::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        manifest
            .validate(host_version)
            .map_err(ManifestError::Invalid)?;
        Ok(manifest)
    }

    /// 语义校验,返回全部问题。
    pub fn validate(&self, host_version: &Version) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            problems.push(format!(
                "schema_version 必须是 {SCHEMA_VERSION},收到 {}",
                self.schema_version
            ));
        }
        if self.min_host_version > *host_version {
            problems.push(format!(
                "需要宿主版本 >= {},当前宿主是 {host_version}",
                self.min_host_version
            ));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            problems.push("name 不能为空且最长 64 个字符".to_string());
        }
        check_display("name", &self.name, &mut problems);
        if self.entrypoints.is_empty() {
            problems.push("entrypoints 至少要有一个".to_string());
        }
        if !self.entrypoints.contains_key(&self.presentation.entrypoint) {
            problems.push(format!(
                "presentation.entrypoint {:?} 不在 entrypoints 里",
                self.presentation.entrypoint
            ));
        }
        for (name, ep) in &self.entrypoints {
            if name.is_empty()
                || name.len() > 32
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                problems.push(format!(
                    "entrypoints 的键 {name:?} 只能是 1–32 个 [a-z0-9_-]"
                ));
            }
            check_url_path(&format!("entrypoints.{name}.path"), &ep.path, &mut problems);
            if let Some(title) = &ep.title {
                check_display(&format!("entrypoints.{name}.title"), title, &mut problems);
                if title.chars().count() > 64 {
                    problems.push(format!("entrypoints.{name}.title 最长 64 个字符"));
                }
            }
        }
        if let Some(icon) = &self.presentation.icon {
            check_relative("presentation.icon", icon, &mut problems);
        }
        if let Some(hint) = &self.presentation.surface_hint
            && (hint.is_empty()
                || hint.len() > 32
                || !hint.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        {
            problems.push("presentation.surface_hint 只能是 1–32 个小写字母或 '_'".to_string());
        }

        // i18n 声明与标题协商:locale 子集校验/规范化、查重、default 包含、key/fallback 成对。
        let has_i18n = self.i18n.is_some();
        if let Some(i18n) = &self.i18n {
            match normalize_locale(&i18n.default_locale) {
                Ok(default) => {
                    if i18n.supported_locales.is_empty() {
                        problems.push("i18n.supported_locales 不能为空".to_string());
                    } else {
                        let mut seen = BTreeSet::new();
                        let mut normalized = Vec::new();
                        let mut bad = false;
                        for loc in &i18n.supported_locales {
                            match normalize_locale(loc) {
                                Ok(n) => {
                                    if !seen.insert(n.clone()) {
                                        problems.push(format!(
                                            "i18n.supported_locales 含重复语言 {loc}"
                                        ));
                                        bad = true;
                                    } else {
                                        normalized.push(n);
                                    }
                                }
                                Err(e) => {
                                    problems.push(format!(
                                        "i18n.supported_locales 含非法语言 {loc}: {e}"
                                    ));
                                    bad = true;
                                }
                            }
                        }
                        if !bad && !normalized.contains(&default) {
                            problems
                                .push("i18n.supported_locales 必须包含 default_locale".to_string());
                        }
                    }
                }
                Err(e) => problems.push(format!("i18n.default_locale 非法: {e}")),
            }
            if let Some(dir) = &i18n.strings_dir {
                check_relative("i18n.strings_dir", dir, &mut problems);
            }
        }

        check_title_pair(
            "presentation",
            &self.presentation.title_key,
            &self.presentation.title_fallback,
            has_i18n,
            &mut problems,
        );
        for (name, ep) in &self.entrypoints {
            check_title_pair(
                &format!("entrypoints.{name}"),
                &ep.title_key,
                &ep.title_fallback,
                has_i18n,
                &mut problems,
            );
        }

        self.validate_runtime(&mut problems);
        check_url_path("health.path", &self.health.path, &mut problems);
        if !(1..=60_000).contains(&self.health.timeout_ms) {
            problems.push("health.timeout_ms 必须在 1..=60000".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    fn validate_runtime(&self, problems: &mut Vec<String>) {
        match &self.runtime {
            Runtime::StaticWeb { source } => check_relative("runtime.source", source, problems),
            Runtime::Node {
                command,
                lockfile,
                node,
                http,
            }
            | Runtime::Python {
                command,
                lockfile,
                python: node,
                http,
            } => {
                if command.is_empty() || command[0].trim().is_empty() {
                    problems.push("runtime.command 不能为空".to_string());
                }
                for arg in command {
                    check_display("runtime.command 的参数", arg, problems);
                }
                if let Some(lock) = lockfile {
                    check_relative("runtime.lockfile", lock, problems);
                }
                let field = match &self.runtime {
                    Runtime::Node { .. } => "runtime.node",
                    _ => "runtime.python",
                };
                if let Some(req) = node
                    && let Err(e) = crate::runtime_version::VersionReq::parse(req)
                {
                    problems.push(format!("{field}: {e}"));
                }
                if !is_env_name(&http.port_env) {
                    problems.push(
                        "runtime.http.port_env 必须是 [A-Z_][A-Z0-9_]* 形式的环境变量名"
                            .to_string(),
                    );
                }
            }
            Runtime::Container { image, http } => {
                if !is_pinned_image(image) {
                    problems.push(
                        "runtime.image 必须按摘要固定,形如 name@sha256:<64 位十六进制>".to_string(),
                    );
                }
                if http.container_port == 0 {
                    problems.push("runtime.http.container_port 不能为 0".to_string());
                }
            }
        }
    }

    /// 给定 UI 语言与已装载资源,返回应用(外壳)标题。
    /// 未声明 `[i18n]` 或未声明 `title_key` 时沿用旧 `Manifest.name`。
    ///
    /// 解析顺序(纯逻辑,见 `I18nBundle::resolve_title`):当前语言 key → default 语言 key → `title_fallback`。
    pub fn resolve_display_title(
        &self,
        bundle: &crate::i18n::I18nBundle,
        ui_locale: &str,
    ) -> String {
        let i18n = match &self.i18n {
            Some(i) => i,
            None => return self.name.clone(),
        };
        let (key, fallback) = match (
            &self.presentation.title_key,
            &self.presentation.title_fallback,
        ) {
            (Some(k), Some(f)) => (k, f),
            _ => return self.name.clone(),
        };
        let negotiated =
            crate::i18n::negotiate_locale(ui_locale, &i18n.supported_locales, &i18n.default_locale);
        let r = crate::i18n::TitleRef {
            key: key.clone(),
            fallback: fallback.clone(),
        };
        bundle.resolve_title(&r, &negotiated, &i18n.default_locale)
    }
}

/// 相对路径:非空、不以 `/` 开头、不含 `..` 段、不含反斜杠或 NUL。
fn check_relative(field: &str, path: &str, problems: &mut Vec<String>) {
    // 允许且只允许**最后**一个空段(目录写法 `web/`);`.`、`..`、中间的空段(`a//b`)都不行
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if bad {
        problems.push(format!(
            "{field} 必须是应用包内的相对路径(不能为空、不能绝对、不能含 `.`/`..`/空段),收到 {path:?}"
        ));
    }
}

fn is_env_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_uppercase() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn is_pinned_image(image: &str) -> bool {
    match image.split_once("@sha256:") {
        Some((name, digest)) => {
            // 名字必须以字母或数字开头(以 `-` 开头的会被 docker CLI 当成选项),且不含空白/控制字符
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
                && !name.chars().any(|c| c.is_whitespace() || c.is_control())
                && digest.len() == 64
                && digest.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// 双向控制字符(U+200E/200F、U+202A–202E、U+2066–2069):能让文本在界面上"看起来"是另一个样子。
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// 会原样显示在批准界面上的文本:不得含控制字符(含换行、制表、NUL)与双向控制字符。
fn check_display(field: &str, text: &str, problems: &mut Vec<String>) {
    if text.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        problems.push(format!("{field} 不能含控制字符或双向控制字符"));
    }
}

/// 校验 `title_key`/`title_fallback` 成对(两者都给或都不给);只给其一则报错。
/// 任一个出现但缺少 `[i18n]` 声明也报错。fallback 必须满足标题字符安全与 64 字符上限。
fn check_title_pair(
    field: &str,
    key: &Option<String>,
    fallback: &Option<String>,
    needs_i18n: bool,
    problems: &mut Vec<String>,
) {
    match (key, fallback) {
        (Some(k), Some(f)) => {
            if k.is_empty() {
                problems.push(format!("{field}.title_key 不能为空"));
            }
            if f.is_empty() {
                problems.push(format!("{field}.title_fallback 不能为空"));
            } else {
                check_display(&format!("{field}.title_fallback"), f, problems);
                if f.chars().count() > 64 {
                    problems.push(format!("{field}.title_fallback 最长 64 个字符"));
                }
            }
        }
        (None, None) => {}
        _ => problems.push(format!("{field} 必须同时声明 title_key 与 title_fallback")),
    }
    if (key.is_some() || fallback.is_some()) && !needs_i18n {
        problems.push(format!(
            "{field} 声明了 title_key/title_fallback 但缺少 [i18n]"
        ));
    }
}

/// 应用内的 URL 路径:以单个 `/` 开头(`//host/x`、`/\\host` 会被浏览器当成另一个 origin),不含反斜杠、
/// `?`、`#`、控制字符与 `..` 段。
fn check_url_path(field: &str, path: &str, problems: &mut Vec<String>) {
    let bad = !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.contains('?')
        || path.contains('#')
        || path.chars().any(|c| c.is_control())
        || path.split('/').any(|seg| seg == "..");
    if bad {
        problems.push(format!(
            "{field} 必须是以单个 '/' 开头的应用内路径(不能含 //开头、反斜杠、?、#、控制字符、.. 段),收到 {path:?}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: Version = Version::new(0, 1, 0);

    /// 随仓库提供的 Excalidraw 打包清单(`scripts/excalidraw/manifest.toml`)必须能被真实解析器接受
    /// (含 `deny_unknown_fields`),并且申请的权限全部取最严——Excalidraw 不联网、不需要宿主数据目录。
    #[cfg(feature = "manifest-toml")]
    #[test]
    fn the_shipped_excalidraw_manifest_parses_and_asks_for_nothing() {
        let text = include_str!("../../../scripts/excalidraw/manifest.toml");
        let m = Manifest::from_toml(text, &HOST).expect("清单应通过校验");
        assert_eq!(m.id.as_str(), "excalidraw");
        assert_eq!(m.permissions, Permissions::default(), "所有权限都应是最严");
        assert!(matches!(m.runtime, Runtime::StaticWeb { ref source } if source == "web/"));
    }

    pub(crate) fn valid() -> Manifest {
        Manifest {
            schema_version: 1,
            min_host_version: Version::new(0, 1, 0),
            id: AppId::new("excalidraw").unwrap(),
            name: "Excalidraw".into(),
            version: Version::new(0, 17, 0),
            presentation: Presentation {
                icon: Some("assets/icon.svg".into()),
                surface_hint: Some("browser".into()),
                entrypoint: "main".into(),
                title_key: None,
                title_fallback: None,
            },
            entrypoints: BTreeMap::from([(
                "main".to_string(),
                Entrypoint {
                    kind: EntrypointKind::Web,
                    path: "/".into(),
                    title: Some("Excalidraw".into()),
                    title_key: None,
                    title_fallback: None,
                },
            )]),
            runtime: Runtime::StaticWeb {
                source: "web/".into(),
            },
            permissions: Permissions::default(),
            health: Health::default(),
            i18n: None,
        }
    }

    fn problems(m: &Manifest) -> Vec<String> {
        m.validate(&HOST).err().unwrap_or_default()
    }

    #[test]
    fn a_valid_static_web_manifest_passes() {
        assert_eq!(valid().validate(&HOST), Ok(()));
    }

    #[test]
    fn host_older_than_min_host_version_is_rejected() {
        let mut m = valid();
        m.min_host_version = Version::new(0, 2, 0);
        let p = problems(&m);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("0.2.0") && p[0].contains("0.1.0"), "{p:?}");
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let mut m = valid();
        m.schema_version = 2;
        assert_eq!(problems(&m).len(), 1);
    }

    #[test]
    fn entrypoint_problems_are_reported() {
        let mut m = valid();
        m.presentation.entrypoint = "nope".into();
        m.entrypoints.get_mut("main").unwrap().path = "no-slash".into();
        assert_eq!(problems(&m).len(), 2);
        m.entrypoints.clear();
        assert!(problems(&m).iter().any(|p| p.contains("至少要有一个")));
    }

    #[test]
    fn paths_that_escape_the_package_are_rejected() {
        for bad in ["", "/abs", "../x", "a/../../b", "a\\b"] {
            let mut m = valid();
            m.runtime = Runtime::StaticWeb { source: bad.into() };
            assert_eq!(problems(&m).len(), 1, "source {bad:?}");
            let mut m = valid();
            m.presentation.icon = Some(bad.into());
            assert_eq!(problems(&m).len(), 1, "icon {bad:?}");
        }
        let mut m = valid();
        m.runtime = Runtime::StaticWeb {
            source: "a/b..c/d".into(),
        };
        assert_eq!(m.validate(&HOST), Ok(()), "含 .. 的文件名不是 .. 段");
    }

    #[test]
    fn all_problems_are_collected_not_just_the_first() {
        let mut m = valid();
        m.name = " ".into();
        m.health = Health {
            path: "x".into(),
            timeout_ms: 0,
        };
        m.presentation.surface_hint = Some("Browser Panel".into());
        assert_eq!(problems(&m).len(), 4);
    }

    #[test]
    fn process_runtimes_need_a_command_and_a_valid_port_env() {
        let mut m = valid();
        m.runtime = Runtime::Python {
            command: vec![],
            lockfile: Some("../lock".into()),
            python: Some(">=3.12".into()),
            http: ProcessHttp {
                port_env: "port".into(),
            },
        };
        assert_eq!(problems(&m).len(), 3);
        m.runtime = Runtime::Node {
            command: vec!["node".into(), "server.js".into()],
            lockfile: Some("package-lock.json".into()),
            node: None,
            http: ProcessHttp {
                port_env: "BYTEHOST_PORT".into(),
            },
        };
        assert_eq!(m.validate(&HOST), Ok(()));
    }

    #[test]
    fn container_images_must_be_pinned_by_digest() {
        let digest = "a".repeat(64);
        let mut m = valid();
        for bad in [
            "docker.io/x/y:latest".to_string(),
            "docker.io/x/y@sha256:abc".to_string(),
            format!("@sha256:{digest}"),
        ] {
            m.runtime = Runtime::Container {
                image: bad.clone(),
                http: ContainerHttp {
                    container_port: 3000,
                },
            };
            assert_eq!(problems(&m).len(), 1, "{bad}");
        }
        m.runtime = Runtime::Container {
            image: format!("docker.io/x/y@sha256:{digest}"),
            http: ContainerHttp {
                container_port: 3000,
            },
        };
        assert_eq!(m.validate(&HOST), Ok(()));
        m.runtime = Runtime::Container {
            image: format!("docker.io/x/y@sha256:{digest}"),
            http: ContainerHttp { container_port: 0 },
        };
        assert_eq!(problems(&m).len(), 1);
    }

    #[test]
    fn runtime_version_requirements_are_validated() {
        let mut m = valid();
        m.runtime = Runtime::Node {
            command: vec!["node".into(), "server.js".into()],
            lockfile: None,
            node: Some(">=22".into()),
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        assert_eq!(m.validate(&HOST), Ok(()));

        m.runtime = Runtime::Python {
            command: vec!["python3".into(), "server.py".into()],
            lockfile: None,
            python: Some(">=3.12, <4".into()),
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        assert_eq!(m.validate(&HOST), Ok(()));

        m.runtime = Runtime::Python {
            command: vec!["python3".into(), "server.py".into()],
            lockfile: None,
            python: Some("3.12".into()),
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        let p = problems(&m);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("runtime.python"), "{p:?}");

        m.runtime = Runtime::Node {
            command: vec!["node".into()],
            lockfile: None,
            node: Some(String::new()),
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        assert!(problems(&m)[0].contains("runtime.node"));
    }

    #[test]
    fn runtime_kind_names_match_the_manifest_vocabulary() {
        assert_eq!(valid().runtime.kind_name(), "static_web");
    }

    #[cfg(feature = "manifest-toml")]
    mod toml_text {
        use super::*;

        const EXCALIDRAW: &str = r#"
schema_version = 1
min_host_version = "0.1.0"
id = "excalidraw"
name = "Excalidraw"
version = "0.17.0"

[presentation]
icon = "assets/icon.svg"
surface_hint = "browser"
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"
title = "Excalidraw"

[runtime]
kind = "static_web"
source = "web/"

[permissions]
clipboard = "read_write"
downloads = "user_confirm"
popups = "deny"

[permissions.network]
outbound = "none"

[permissions.filesystem]
data = "read_write"

[health]
path = "/"
timeout_ms = 3000
"#;

        #[test]
        fn the_spec_example_parses_to_the_expected_manifest() {
            let m = Manifest::from_toml(EXCALIDRAW, &HOST).unwrap();
            assert_eq!(m.id.as_str(), "excalidraw");
            assert_eq!(m.version, Version::new(0, 17, 0));
            assert_eq!(
                m.runtime,
                Runtime::StaticWeb {
                    source: "web/".into()
                }
            );
            assert_eq!(
                m.permissions.clipboard,
                crate::permissions::Access::ReadWrite
            );
            assert_eq!(
                m.permissions.filesystem.data,
                crate::permissions::Access::ReadWrite
            );
            assert_eq!(
                m.permissions.network.outbound,
                crate::permissions::Outbound::None
            );
            assert_eq!(m.entrypoints["main"].kind, EntrypointKind::Web);
        }

        #[test]
        fn unknown_fields_are_a_parse_error_at_the_top_level_and_in_nested_tables() {
            let top = format!("{EXCALIDRAW}\nsurprise = 1\n");
            assert!(matches!(
                Manifest::from_toml(&top, &HOST),
                Err(ManifestError::Parse(_))
            ));
            let perms =
                EXCALIDRAW.replace("popups = \"deny\"", "popups = \"deny\"\ncamera = \"allow\"");
            assert!(matches!(
                Manifest::from_toml(&perms, &HOST),
                Err(ManifestError::Parse(_))
            ));
            let runtime =
                EXCALIDRAW.replace("source = \"web/\"", "source = \"web/\"\nextra = true");
            assert!(matches!(
                Manifest::from_toml(&runtime, &HOST),
                Err(ManifestError::Parse(_))
            ));
        }

        #[test]
        fn missing_required_fields_and_bad_values_are_parse_errors() {
            let no_id = EXCALIDRAW.replace("id = \"excalidraw\"\n", "");
            assert!(matches!(
                Manifest::from_toml(&no_id, &HOST),
                Err(ManifestError::Parse(_))
            ));
            let bad_id = EXCALIDRAW.replace("id = \"excalidraw\"", "id = \"../etc\"");
            assert!(matches!(
                Manifest::from_toml(&bad_id, &HOST),
                Err(ManifestError::Parse(_))
            ));
            let bad_gate = EXCALIDRAW.replace("user_confirm", "maybe");
            assert!(matches!(
                Manifest::from_toml(&bad_gate, &HOST),
                Err(ManifestError::Parse(_))
            ));
            let bad_kind = EXCALIDRAW.replace("kind = \"static_web\"", "kind = \"wasm\"");
            assert!(matches!(
                Manifest::from_toml(&bad_kind, &HOST),
                Err(ManifestError::Parse(_))
            ));
        }

        #[test]
        fn semantic_problems_surface_as_invalid_with_all_messages() {
            let text = EXCALIDRAW
                .replace(
                    "min_host_version = \"0.1.0\"",
                    "min_host_version = \"9.0.0\"",
                )
                .replace("source = \"web/\"", "source = \"../web\"");
            match Manifest::from_toml(&text, &HOST) {
                Err(ManifestError::Invalid(p)) => assert_eq!(p.len(), 2, "{p:?}"),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn omitted_permissions_and_health_take_the_strictest_and_default_values() {
            let minimal = r#"
schema_version = 1
min_host_version = "0.1.0"
id = "tiny"
name = "Tiny"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
"#;
            let m = Manifest::from_toml(minimal, &HOST).unwrap();
            assert_eq!(m.permissions, Permissions::default());
            assert_eq!(m.health, Health::default());
            assert_eq!(m.presentation.icon, None);
        }

        #[test]
        fn a_process_runtime_parses_from_toml() {
            let text = EXCALIDRAW.replace(
                "[runtime]\nkind = \"static_web\"\nsource = \"web/\"",
                "[runtime]\nkind = \"python\"\ncommand = [\"python\", \"-m\", \"app\"]\nlockfile = \"requirements.lock\"\n[runtime.http]\nport_env = \"BYTEHOST_PORT\"",
            );
            let m = Manifest::from_toml(&text, &HOST).unwrap();
            assert_eq!(m.runtime.kind_name(), "python");
        }
    }

    #[test]
    fn entrypoint_and_health_paths_cannot_point_at_another_origin() {
        for bad in [
            "//evil.example/x",
            "/\\evil.example",
            "/a\\b",
            "/a?b",
            "/a#b",
            "/a\nb",
            "/a/../b",
        ] {
            let mut m = valid();
            m.entrypoints.get_mut("main").unwrap().path = bad.into();
            assert_eq!(problems(&m).len(), 1, "entrypoint {bad:?}");
            let mut m = valid();
            m.health.path = bad.into();
            assert_eq!(problems(&m).len(), 1, "health {bad:?}");
        }
        for ok in ["/", "/app/index.html", "/a.b/c", "/a/b..c"] {
            let mut m = valid();
            m.entrypoints.get_mut("main").unwrap().path = ok.into();
            m.health.path = ok.into();
            assert_eq!(m.validate(&HOST), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn text_shown_on_the_approval_screen_cannot_carry_control_or_bidi_characters() {
        for bad in [
            "Ex\ncal",
            "Evil\u{202e}gpj",
            "a\u{2066}b",
            "tab\there",
            "nul\0",
        ] {
            let mut m = valid();
            m.name = bad.into();
            assert_eq!(problems(&m).len(), 1, "name {bad:?}");
            let mut m = valid();
            m.entrypoints.get_mut("main").unwrap().title = Some(bad.into());
            assert_eq!(problems(&m).len(), 1, "title {bad:?}");
            let mut m = valid();
            m.runtime = Runtime::Node {
                command: vec!["node".into(), bad.into()],
                lockfile: None,
                node: None,
                http: ProcessHttp {
                    port_env: "PORT".into(),
                },
            };
            assert_eq!(problems(&m).len(), 1, "command arg {bad:?}");
        }
    }

    #[test]
    fn container_image_names_cannot_start_with_a_dash_or_hide_whitespace() {
        let digest = "a".repeat(64);
        for bad in [
            format!("--privileged@sha256:{digest}"),
            format!("-v@sha256:{digest}"),
            format!("x y@sha256:{digest}"),
            format!("x\n@sha256:{digest}"),
        ] {
            let mut m = valid();
            m.runtime = Runtime::Container {
                image: bad.clone(),
                http: ContainerHttp {
                    container_port: 3000,
                },
            };
            assert_eq!(problems(&m).len(), 1, "{bad:?}");
        }
    }

    #[test]
    fn relative_paths_reject_dot_and_empty_segments_but_allow_one_trailing_slash() {
        for bad in [".", "./web", "a/./b", "a//b", "web//", "//"] {
            let mut m = valid();
            m.runtime = Runtime::StaticWeb { source: bad.into() };
            assert_eq!(problems(&m).len(), 1, "{bad:?}");
        }
        for ok in ["web", "web/", "a/b/c.txt", "a/b..c", ".hidden/x"] {
            let mut m = valid();
            m.runtime = Runtime::StaticWeb { source: ok.into() };
            assert_eq!(m.validate(&HOST), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn entrypoint_keys_and_titles_are_validated() {
        for bad in ["", "Main", "a b", "a/b", &"k".repeat(33)] {
            let mut m = valid();
            m.entrypoints.insert(
                bad.to_string(),
                Entrypoint {
                    kind: EntrypointKind::Web,
                    path: "/x".into(),
                    title: None,
                    title_key: None,
                    title_fallback: None,
                },
            );
            assert_eq!(problems(&m).len(), 1, "key {bad:?}");
        }
        let mut m = valid();
        m.entrypoints.get_mut("main").unwrap().title = Some("t".repeat(65));
        assert_eq!(problems(&m).len(), 1, "标题最长 64 个字符");
        let mut m = valid();
        m.entrypoints.get_mut("main").unwrap().title = Some("t".repeat(64));
        assert_eq!(m.validate(&HOST), Ok(()));
    }
}

#[cfg(all(test, feature = "manifest-toml"))]
#[cfg(test)]
mod i18n_tests {
    use super::tests::valid;
    use super::*;
    use crate::i18n::{I18nBundle, WebViewLocale, negotiate_locale, normalize_locale};

    const HOST: Version = Version::new(0, 1, 0);

    const BASE: &str = r#"
schema_version = 1
min_host_version = "0.1.0"
id = "notes"
name = "Notes"
version = "1.0.0"

[i18n]
default_locale = "en"
supported_locales = ["en", "zh-CN"]
strings_dir = "locales"
webview_locale = "live"

[presentation]
entrypoint = "main"
title_key = "app.title"
title_fallback = "Notes"

[entrypoints.main]
type = "web"
path = "/"
title = "Notes"
title_key = "entry.main.title"
title_fallback = "Notes"

[runtime]
kind = "static_web"
source = "web/"

[health]
path = "/"
timeout_ms = 3000
"#;

    fn parsed() -> Manifest {
        Manifest::from_toml(BASE, &HOST).expect("带 i18n 的清单应通过校验")
    }

    #[test]
    fn a_manifest_with_i18n_parses_and_validates() {
        let m = parsed();
        let i18n = m.i18n.expect("应有 [i18n]");
        assert_eq!(i18n.default_locale, "en");
        assert_eq!(
            i18n.supported_locales,
            vec!["en".to_string(), "zh-CN".to_string()]
        );
        assert_eq!(i18n.strings_dir.as_deref(), Some("locales"));
        assert_eq!(i18n.webview_locale, WebViewLocale::Live);
        assert_eq!(m.presentation.title_key.as_deref(), Some("app.title"));
        assert_eq!(m.presentation.title_fallback.as_deref(), Some("Notes"));
    }

    #[test]
    fn webview_locale_defaults_to_none_when_omitted() {
        let text = BASE.replace("webview_locale = \"live\"\n", "");
        let m = Manifest::from_toml(&text, &HOST).unwrap();
        assert_eq!(m.i18n.unwrap().webview_locale, WebViewLocale::None);
    }

    #[test]
    fn locale_tags_normalize_to_canonical_form() {
        for (raw, want) in [
            ("zh-CN", Ok("zh-CN")),
            ("ZH-CN", Ok("zh-CN")),
            ("zh-Hans-CN", Ok("zh-Hans-CN")),
            ("ZH-HANS-CN", Ok("zh-Hans-CN")),
            ("en", Ok("en")),
        ] {
            assert_eq!(
                normalize_locale(raw),
                want.map(str::to_string),
                "normalize {raw}"
            );
        }
        for bad in [
            "zh_CN",
            "en_US.UTF-8",
            "a",
            "abcdefghi",
            "en--US",
            "en-x-foo",
            "zh-Hans-CN-extra",
            "en-1",
        ] {
            assert!(normalize_locale(bad).is_err(), "应为非法: {bad}");
        }
    }

    #[test]
    fn locale_negotiation_is_exact_no_family_or_simp_trad() {
        let supported = vec!["en".to_string(), "zh-CN".to_string()];
        assert_eq!(negotiate_locale("zh-CN", &supported, "en"), "zh-CN");
        assert_eq!(negotiate_locale("ZH-CN", &supported, "en"), "zh-CN");
        assert_eq!(negotiate_locale("fr", &supported, "en"), "en"); // 未知回到 default
        assert_eq!(negotiate_locale("zh-TW", &supported, "en"), "en"); // 不做简繁转换
    }

    #[test]
    fn title_resolves_current_then_default_then_fallback() {
        let m = parsed();
        let mut bundle = I18nBundle::default();
        bundle
            .locales
            .insert("en".into(), [("app.title".into(), "Notes".into())].into());
        bundle
            .locales
            .insert("zh-CN".into(), [("app.title".into(), "笔记".into())].into());
        assert_eq!(m.resolve_display_title(&bundle, "zh-CN"), "笔记");
        assert_eq!(m.resolve_display_title(&bundle, "en"), "Notes");
        assert_eq!(m.resolve_display_title(&bundle, "fr"), "Notes"); // 未知语言回退 default
        bundle.locales.get_mut("zh-CN").unwrap().remove("app.title");
        assert_eq!(m.resolve_display_title(&bundle, "zh-CN"), "Notes"); // 缺译文→default
        bundle.locales.get_mut("en").unwrap().remove("app.title");
        assert_eq!(m.resolve_display_title(&bundle, "zh-CN"), "Notes"); // 都缺→fallback
    }

    #[test]
    fn no_i18n_or_no_key_falls_back_to_manifest_name() {
        let m = valid();
        assert_eq!(
            m.resolve_display_title(&I18nBundle::default(), "zh-CN"),
            "Excalidraw"
        );
    }

    #[test]
    fn illegal_i18n_declarations_are_rejected_with_field_level_reasons() {
        // supported 必须包含 default
        let t = BASE.replace(
            "supported_locales = [\"en\", \"zh-CN\"]",
            "supported_locales = [\"zh-CN\"]",
        );
        match Manifest::from_toml(&t, &HOST) {
            Err(ManifestError::Invalid(p)) => {
                assert!(
                    p.iter().any(|x| x.contains("必须包含 default_locale")),
                    "{p:?}"
                )
            }
            other => panic!("应为 Invalid: {other:?}"),
        }
        // 重复(规范化后)语言
        let t = BASE.replace(
            "supported_locales = [\"en\", \"zh-CN\"]",
            "supported_locales = [\"en\", \"EN\"]",
        );
        match Manifest::from_toml(&t, &HOST) {
            Err(ManifestError::Invalid(p)) => {
                assert!(p.iter().any(|x| x.contains("重复语言")), "{p:?}")
            }
            other => panic!("应为 Invalid: {other:?}"),
        }
        // 非法语言标签
        let t = BASE.replace(
            "supported_locales = [\"en\", \"zh-CN\"]",
            "supported_locales = [\"en\", \"zh_CN\"]",
        );
        match Manifest::from_toml(&t, &HOST) {
            Err(ManifestError::Invalid(p)) => {
                assert!(p.iter().any(|x| x.contains("非法语言")), "{p:?}")
            }
            other => panic!("应为 Invalid: {other:?}"),
        }
        // title_key 无 fallback
        let t = BASE.replace("title_fallback = \"Notes\"\n", "");
        match Manifest::from_toml(&t, &HOST) {
            Err(ManifestError::Invalid(p)) => assert!(
                p.iter()
                    .any(|x| x.contains("必须同时声明 title_key 与 title_fallback")),
                "{p:?}"
            ),
            other => panic!("应为 Invalid: {other:?}"),
        }
        // fallback 超长
        let t = BASE.replace(
            "title_fallback = \"Notes\"",
            &format!("title_fallback = \"{}\"", "N".repeat(65)),
        );
        match Manifest::from_toml(&t, &HOST) {
            Err(ManifestError::Invalid(p)) => {
                assert!(p.iter().any(|x| x.contains("最长 64")), "{p:?}")
            }
            other => panic!("应为 Invalid: {other:?}"),
        }
        // fallback 含控制字符
        let t = BASE.replace(
            "title_fallback = \"Notes\"",
            "title_fallback = \"Note\\ns\"",
        );
        match Manifest::from_toml(&t, &HOST) {
            Err(ManifestError::Invalid(p)) => {
                assert!(p.iter().any(|x| x.contains("控制字符")), "{p:?}")
            }
            other => panic!("应为 Invalid: {other:?}"),
        }
    }

    #[test]
    fn title_key_without_i18n_is_rejected() {
        let text = r#"
schema_version = 1
min_host_version = "0.1.0"
id = "x"
name = "X"
version = "1.0.0"

[presentation]
entrypoint = "main"
title_key = "app.title"
title_fallback = "X"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
"#;
        match Manifest::from_toml(text, &HOST) {
            Err(ManifestError::Invalid(p)) => {
                assert!(p.iter().any(|x| x.contains("缺少 [i18n]")), "{p:?}")
            }
            other => panic!("应为 Invalid: {other:?}"),
        }
    }

    #[test]
    fn old_manifest_without_i18n_still_validates() {
        assert_eq!(valid().validate(&HOST), Ok(()));
    }

    #[test]
    fn new_manifest_round_trips_through_json() {
        let m = parsed();
        let json = serde_json::to_string(&m).expect("序列化");
        let m2: Manifest = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(m, m2);
    }

    #[test]
    fn compat_boundary_new_fields_are_unknown_to_old_parser() {
        // 新清单的线上文本携带 [i18n] 与 title_key;旧解析器(deny_unknown_fields)会拒绝这些未知字段。
        // 锁定:当前解析器认识它们(否则 parsed() 失败),且文本确实包含这些新段。
        assert!(BASE.contains("[i18n]"));
        assert!(BASE.contains("title_key"));
        assert!(BASE.contains("title_fallback"));
        // 去掉 [i18n] 与 title_key/fallback 后是纯旧清单,必须通过——证明新字段正是这些增量、
        // 且旧清单在新解析器上无回归。
        let without_i18n = BASE
            .replace(
                "[i18n]\ndefault_locale = \"en\"\nsupported_locales = [\"en\", \"zh-CN\"]\nstrings_dir = \"locales\"\nwebview_locale = \"live\"\n",
                "",
            )
            .replace("title_key = \"app.title\"\n", "")
            .replace("title_fallback = \"Notes\"\n", "")
            .replace("title_key = \"entry.main.title\"\n", "")
            .replace("title_fallback = \"Notes\"\n", "");
        assert!(
            Manifest::from_toml(&without_i18n, &HOST).is_ok(),
            "纯旧清单应通过"
        );
    }
}
