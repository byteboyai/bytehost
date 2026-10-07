//! GUI ↔ supervisor 的线上类型(默认 feature,只依赖 serde):请求、应答,以及它们带着的列表项/探测结果。
//! 宿主产品把 [`AppRequest`]/[`AppReply`] 原样塞进自己的 socket 协议里(Dozer 见 `dozer-core::protocol` 的
//! `Request::App`/`Reply::App`),Digger 将来用同一组类型走它自己的 socket。**本 crate 不知道 socket 长什么样。**

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::id::{AppId, Version};
use crate::plan::{ApprovedInstallPlan, InstallPlan, Provenance, TrustLevel};
use crate::registry::UninstallMode;
use crate::state::{DesiredState, ObservedState};

/// 应用从哪来。一期只有本地目录(目录里要有 `manifest.toml`);压缩包/仓库以后再加。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppSource {
    LocalDir { path: PathBuf },
}

/// 列表里的一项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSummary {
    pub id: AppId,
    pub name: String,
    pub version: Version,
    pub desired: DesiredState,
    pub observed: ObservedState,
    /// 运行中才有:不含令牌的站点地址。
    pub url: Option<String>,
}

/// 可由 bytehost **安装到自己的目录**的运行时(规格 A9)。
///
/// `Node` 是 Node.js 本身;`Python` 指 **uv + 由 uv 管理的 CPython**(uv 是我们的可装物,CPython 由 uv 下载)。
/// 定义在 `proto`(默认 feature、只依赖 serde)是因为它要在线上协议里出现;`runtime::managed` 里 `pub use` 它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRuntime {
    Node,
    Python,
}

/// 一个外部运行时的可用性(分层:没装 / 装了但当前不可用 / 可用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum RuntimeAvailability {
    /// 可用;`detail` 是版本之类的人类可读信息。
    Available {
        detail: String,
    },
    /// 装了但当前用不了(如 Docker 守护进程没启动)。
    Unavailable {
        detail: String,
    },
    NotInstalled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProbe {
    /// `docker` / `node` / `python`。
    pub runtime: String,
    #[serde(flatten)]
    pub availability: RuntimeAvailability,
    /// 已装的**受管**版本(新→旧);老客户端没有这个字段,一律默认为空。
    #[serde(default)]
    pub managed: Vec<String>,
    /// 此平台是否有固定版本可自动安装(node/python 为 true,docker 为 false)。
    #[serde(default)]
    pub installable: bool,
    /// 该运行时当前的后台安装任务(有就带上)。
    #[serde(default)]
    pub job: Option<RuntimeJob>,
}

/// 安装计划里的一个下载项。`sha256: None` 只出现在"由 uv 校验"的 Python 上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDownload {
    pub what: String,
    pub url: String,
    pub sha256: Option<String>,
    pub note: String,
}

/// 一份**可审阅的安装计划**;批准后由服务端重算核对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInstallPlan {
    pub runtime: ManagedRuntime,
    /// `(name, version)` 列表(顺序即安装顺序)。
    pub versions: Vec<(String, String)>,
    pub downloads: Vec<RuntimeDownload>,
    pub dest: String,
    pub will_do: Vec<String>,
}

/// 一次运行时安装任务的快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeJob {
    pub runtime: ManagedRuntime,
    pub phase: String,
    pub done: u64,
    pub total: Option<u64>,
    pub failed: Option<String>,
    pub finished: bool,
}

/// 失败的类别——GUI 据此决定怎么呈现(不可用走提示页,被拒绝走审批界面,冲突/不存在走 Toast……),
/// 不靠解析人类可读的 `message`。**新增类别只能往后加**(线上协议)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppErrorKind {
    /// 应用宿主本身不可用(gateway 端口被占、应用目录打不开、dozerd 正在停止)。
    Unavailable,
    /// 请求内容被拒绝:清单/摘要/批准不符、来源不合法、包里缺东西。
    Rejected,
    /// 应用没有安装。
    NotFound,
    /// 与当前状态冲突:版本已装、应用在运行、状态不允许该操作。
    Conflict,
    /// 一期不支持的应用类型。
    Unsupported,
    /// 其他(I/O、后台任务崩溃)。
    Internal,
}

/// 一次失败:类别 + 给人看的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppFailure {
    pub kind: AppErrorKind,
    pub message: String,
}

impl AppFailure {
    pub fn new(kind: AppErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AppFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppFailure {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AppRequest {
    /// 已安装的应用(含状态与站点地址)。
    List,
    /// 出安装计划(不安装任何东西)。
    Plan {
        source: AppSource,
        provenance: Provenance,
        trust: TrustLevel,
    },
    /// 安装一份**已批准**的计划;服务端会对 staging 副本重新计算并核对,不信任这里带来的内容。
    Install {
        approved: Box<ApprovedInstallPlan>,
        source: AppSource,
    },
    Start {
        id: AppId,
    },
    Stop {
        id: AppId,
    },
    Uninstall {
        id: AppId,
        mode: UninstallMode,
    },
    /// 首次导航用的地址(含一次性换 Cookie 的令牌,**秘密**)。
    LaunchUrl {
        id: AppId,
    },
    /// 探测 docker/node/python 的可用性(Settings 展示用)。
    ProbeRuntimes,
    /// 出一份运行时的安装计划(不下载任何东西)。
    RuntimePlan {
        runtime: ManagedRuntime,
    },
    /// 安装一份**已批准**的运行时计划;服务端会重算并逐字段核对,不一致即拒绝且不下载。
    InstallRuntime {
        plan: Box<RuntimeInstallPlan>,
    },
    /// 卸载某个受管运行时版本。
    UninstallRuntime {
        runtime: ManagedRuntime,
        version: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum AppReply {
    Apps {
        apps: Vec<AppSummary>,
    },
    Plan {
        plan: Box<InstallPlan>,
    },
    /// 成功且没有内容(安装、停止、卸载)。
    Done,
    /// 已启动;`url` 不含令牌。
    Started {
        url: String,
    },
    LaunchUrl {
        url: String,
    },
    Runtimes {
        runtimes: Vec<RuntimeProbe>,
    },
    /// 一份运行时安装计划。
    RuntimePlan {
        plan: Box<RuntimeInstallPlan>,
    },
    /// 请求失败(带类别)。
    Failed {
        #[serde(flatten)]
        failure: AppFailure,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id() -> AppId {
        AppId::new("excalidraw").unwrap()
    }

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        value: &T,
    ) -> serde_json::Value {
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(&serde_json::from_value::<T>(json.clone()).unwrap(), value);
        json
    }

    #[test]
    fn simple_requests_use_a_stable_op_tag() {
        assert_eq!(round_trip(&AppRequest::List), json!({"op": "list"}));
        assert_eq!(
            round_trip(&AppRequest::ProbeRuntimes),
            json!({"op": "probe_runtimes"})
        );
        assert_eq!(
            round_trip(&AppRequest::Start { id: id() }),
            json!({"op": "start", "id": "excalidraw"})
        );
        assert_eq!(
            round_trip(&AppRequest::Stop { id: id() }),
            json!({"op": "stop", "id": "excalidraw"})
        );
        assert_eq!(
            round_trip(&AppRequest::LaunchUrl { id: id() }),
            json!({"op": "launch_url", "id": "excalidraw"})
        );
        assert_eq!(
            round_trip(&AppRequest::Uninstall {
                id: id(),
                mode: UninstallMode::ProgramAndData
            }),
            json!({"op": "uninstall", "id": "excalidraw", "mode": "program_and_data"})
        );
    }

    #[test]
    fn plan_requests_carry_the_source_provenance_and_trust() {
        let req = AppRequest::Plan {
            source: AppSource::LocalDir {
                path: "/tmp/app".into(),
            },
            provenance: Provenance::AgentGenerated,
            trust: TrustLevel::Untrusted,
        };
        assert_eq!(
            round_trip(&req),
            json!({"op": "plan", "source": {"kind": "local_dir", "path": "/tmp/app"}, "provenance": "agent_generated", "trust": "untrusted"})
        );
    }

    #[test]
    fn failures_carry_a_stable_kind_tag_next_to_the_message() {
        let reply = AppReply::Failed {
            failure: AppFailure::new(AppErrorKind::Unavailable, "端口被占用"),
        };
        assert_eq!(
            round_trip(&reply),
            json!({"reply": "failed", "kind": "unavailable", "message": "端口被占用"})
        );
        for (kind, tag) in [
            (AppErrorKind::Rejected, "rejected"),
            (AppErrorKind::NotFound, "not_found"),
            (AppErrorKind::Conflict, "conflict"),
            (AppErrorKind::Unsupported, "unsupported"),
            (AppErrorKind::Internal, "internal"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(tag));
        }
        assert!(
            serde_json::from_value::<AppReply>(
                json!({"reply": "failed", "kind": "exploded", "message": "x"})
            )
            .is_err(),
            "未知类别被拒绝"
        );
    }

    #[test]
    fn managed_runtimes_round_trip_with_a_stable_snake_case_tag() {
        assert_eq!(
            serde_json::to_value(ManagedRuntime::Node).unwrap(),
            json!("node")
        );
        assert_eq!(
            serde_json::to_value(ManagedRuntime::Python).unwrap(),
            json!("python")
        );
        assert_eq!(round_trip(&ManagedRuntime::Node), json!("node"), "往返");
        assert_eq!(round_trip(&ManagedRuntime::Python), json!("python"));
    }

    #[test]
    fn unknown_ops_and_unknown_sources_are_rejected_not_ignored() {
        assert!(serde_json::from_value::<AppRequest>(json!({"op": "format_disk"})).is_err());
        assert!(serde_json::from_value::<AppRequest>(json!({"op": "plan", "source": {"kind": "url", "url": "x"}, "provenance": "local", "trust": "trusted"})).is_err());
        assert!(
            serde_json::from_value::<AppRequest>(json!({"op": "start", "id": "../etc"})).is_err(),
            "非法 app id 在反序列化时就被拒绝"
        );
    }

    #[test]
    fn replies_round_trip_including_runtime_probes_with_flattened_availability() {
        round_trip(&AppReply::Done);
        round_trip(&AppReply::Started {
            url: "http://excalidraw.localhost:20001/".into(),
        });
        round_trip(&AppReply::LaunchUrl {
            url: "http://excalidraw.localhost:20001/?bh_token=t".into(),
        });
        let apps = AppReply::Apps {
            apps: vec![AppSummary {
                id: id(),
                name: "Excalidraw".into(),
                version: Version::new(0, 17, 0),
                desired: DesiredState::Running,
                observed: ObservedState::Running,
                url: Some("http://excalidraw.localhost:20001/".into()),
            }],
        };
        let json = round_trip(&apps);
        assert_eq!(json["apps"][0]["version"], "0.17.0");
        assert_eq!(json["apps"][0]["observed"], json!({"state": "running"}));

        let runtimes = AppReply::Runtimes {
            runtimes: vec![
                RuntimeProbe {
                    runtime: "docker".into(),
                    availability: RuntimeAvailability::Unavailable {
                        detail: "Colima 没启动".into(),
                    },
                    managed: Vec::new(),
                    installable: false,
                    job: None,
                },
                RuntimeProbe {
                    runtime: "node".into(),
                    availability: RuntimeAvailability::NotInstalled,
                    managed: Vec::new(),
                    installable: true,
                    job: None,
                },
            ],
        };
        let json = round_trip(&runtimes);
        assert_eq!(
            json["runtimes"][0],
            json!({"runtime": "docker", "availability": "unavailable", "detail": "Colima 没启动", "managed": [], "installable": false, "job": null})
        );
        assert_eq!(
            json["runtimes"][1],
            json!({"runtime": "node", "availability": "not_installed", "managed": [], "installable": true, "job": null})
        );
    }

    #[test]
    fn runtime_install_requests_and_replies_round_trip() {
        let plan = RuntimeInstallPlan {
            runtime: ManagedRuntime::Node,
            versions: vec![("node".into(), "24.21.0".into())],
            downloads: vec![RuntimeDownload {
                what: "Node 24.21.0".into(),
                url: "https://nodejs.org/x.tar.gz".into(),
                sha256: Some("a".repeat(64)),
                note: "官方".into(),
            }],
            dest: "/r/node/24.21.0".into(),
            will_do: vec!["下载并校验 SHA-256".into()],
        };
        assert_eq!(
            round_trip(&AppRequest::RuntimePlan {
                runtime: ManagedRuntime::Python
            }),
            json!({"op": "runtime_plan", "runtime": "python"})
        );
        assert_eq!(
            round_trip(&AppRequest::UninstallRuntime {
                runtime: ManagedRuntime::Node,
                version: "24.21.0".into()
            }),
            json!({"op": "uninstall_runtime", "runtime": "node", "version": "24.21.0"})
        );
        let req = AppRequest::InstallRuntime {
            plan: Box::new(plan.clone()),
        };
        let json = round_trip(&req);
        assert_eq!(json["op"], "install_runtime");
        assert_eq!(json["plan"]["runtime"], "node");
        assert_eq!(json["plan"]["downloads"][0]["sha256"], "a".repeat(64));
        round_trip(&AppReply::RuntimePlan {
            plan: Box::new(plan),
        });
    }

    #[test]
    fn runtime_probe_with_managed_installable_and_job_round_trips() {
        let probe = RuntimeProbe {
            runtime: "python".into(),
            availability: RuntimeAvailability::NotInstalled,
            managed: vec!["cpython-3.13.0".into()],
            installable: true,
            job: Some(RuntimeJob {
                runtime: ManagedRuntime::Python,
                phase: "downloading".into(),
                done: 10,
                total: Some(100),
                failed: None,
                finished: false,
            }),
        };
        let json = round_trip(&probe);
        assert_eq!(json["managed"][0], "cpython-3.13.0");
        assert_eq!(json["installable"], true);
        assert_eq!(json["job"]["phase"], "downloading");
    }

    #[test]
    fn an_old_runtime_probe_without_the_new_fields_still_parses() {
        let old = json!({"runtime": "node", "availability": "not_installed"});
        let probe: RuntimeProbe = serde_json::from_value(old).unwrap();
        assert!(probe.managed.is_empty());
        assert!(!probe.installable);
        assert!(probe.job.is_none());
    }
}
