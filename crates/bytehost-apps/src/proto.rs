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
}

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
                },
                RuntimeProbe {
                    runtime: "node".into(),
                    availability: RuntimeAvailability::NotInstalled,
                },
            ],
        };
        let json = round_trip(&runtimes);
        assert_eq!(
            json["runtimes"][0],
            json!({"runtime": "docker", "availability": "unavailable", "detail": "Colima 没启动"})
        );
        assert_eq!(
            json["runtimes"][1],
            json!({"runtime": "node", "availability": "not_installed"})
        );
    }
}
