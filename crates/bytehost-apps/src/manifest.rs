//! Manifest v1:应用对宿主的**申请**。字段、取值与校验规则见 `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md` §4.1。
//! 所有结构 `deny_unknown_fields`;`validate` 一次性收集全部问题,不在第一个问题处停下。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entrypoint {
    #[serde(rename = "type")]
    pub kind: EntrypointKind,
    /// 应用内路径,以 `/` 开头。
    pub path: String,
    pub title: Option<String>,
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
            if !ep.path.starts_with('/') {
                problems.push(format!("entrypoints.{name}.path 必须以 '/' 开头"));
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
        self.validate_runtime(&mut problems);
        if !self.health.path.starts_with('/') {
            problems.push("health.path 必须以 '/' 开头".to_string());
        }
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
                http,
                ..
            }
            | Runtime::Python {
                command,
                lockfile,
                http,
                ..
            } => {
                if command.is_empty() || command[0].trim().is_empty() {
                    problems.push("runtime.command 不能为空".to_string());
                }
                if let Some(lock) = lockfile {
                    check_relative("runtime.lockfile", lock, problems);
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
}

/// 相对路径:非空、不以 `/` 开头、不含 `..` 段、不含反斜杠或 NUL。
fn check_relative(field: &str, path: &str, problems: &mut Vec<String>) {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.split('/').any(|seg| seg == "..");
    if bad {
        problems.push(format!(
            "{field} 必须是应用包内的相对路径(不能为空、不能绝对、不能含 ..),收到 {path:?}"
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
            !name.is_empty() && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: Version = Version::new(0, 1, 0);

    fn valid() -> Manifest {
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
            },
            entrypoints: BTreeMap::from([(
                "main".to_string(),
                Entrypoint {
                    kind: EntrypointKind::Web,
                    path: "/".into(),
                    title: Some("Excalidraw".into()),
                },
            )]),
            runtime: Runtime::StaticWeb {
                source: "web/".into(),
            },
            permissions: Permissions::default(),
            health: Health::default(),
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
}
