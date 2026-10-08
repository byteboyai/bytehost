//! 给产品层的事件:Rail 据此更新图标状态;bytehost 不理解"左栏/右栏"。

use serde::{Deserialize, Serialize};

use crate::id::AppId;
use crate::manifest::Manifest;
use crate::permissions::PermissionChange;
use crate::state::ObservedState;

/// 一个耗时任务(安装/准备/启动…)的标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// 当前阶段的人类可读名称(如 "安装依赖"、"拉取镜像")。
    pub phase: String,
    pub done: u64,
    /// 总量未知时为 `None`。
    pub total: Option<u64>,
}

/// 运行时不可用的原因(产品据此在应用面板里画提示页,见规格 §6.3)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RuntimeReason {
    /// 系统里没有这个运行时(如没装 docker/node/uv)。
    NotInstalled { runtime: String },
    /// 装了但当前用不了(如 Colima 没启动)。
    Unavailable { runtime: String, detail: String },
    /// 装了,但版本不满足清单声明的要求。
    Unsatisfied {
        runtime: String,
        required: String,
        found: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AppEvent {
    Installed {
        app: AppId,
    },
    StateChanged {
        app: AppId,
        state: ObservedState,
    },
    /// 应用的访问地址变化(启动后出现、停止后消失)。
    EndpointChanged {
        app: AppId,
        url: Option<String>,
    },
    /// 升级后 manifest 的权限变化(供产品展示)。
    ManifestChanged {
        app: AppId,
        permission_changes: Vec<PermissionChange>,
    },
    Progress {
        app: AppId,
        task: TaskId,
        progress: Progress,
    },
    LogAvailable {
        app: AppId,
    },
    RuntimeUnavailable {
        app: AppId,
        reason: RuntimeReason,
    },
}

impl AppEvent {
    /// 这条事件属于哪个应用。
    pub fn app(&self) -> &AppId {
        match self {
            Self::Installed { app }
            | Self::StateChanged { app, .. }
            | Self::EndpointChanged { app, .. }
            | Self::ManifestChanged { app, .. }
            | Self::Progress { app, .. }
            | Self::LogAvailable { app }
            | Self::RuntimeUnavailable { app, .. } => app,
        }
    }
}

/// 比较新旧 manifest 的权限申请,给出 `ManifestChanged` 需要的差异;没有变化返回 `None`。
pub fn manifest_changed(old: &Manifest, new: &Manifest) -> Option<AppEvent> {
    let changes = crate::permissions::diff_permissions(&old.permissions, &new.permissions);
    (!changes.is_empty()).then(|| AppEvent::ManifestChanged {
        app: new.id.clone(),
        permission_changes: changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Version;
    use crate::manifest::{Entrypoint, EntrypointKind, Health, Presentation, Runtime};
    use crate::permissions::{Access, PermissionKey, Permissions};
    use std::collections::BTreeMap;

    fn manifest(permissions: Permissions) -> Manifest {
        Manifest {
            schema_version: 1,
            min_host_version: Version::new(0, 1, 0),
            id: AppId::new("excalidraw").unwrap(),
            name: "E".into(),
            version: Version::new(1, 0, 0),
            presentation: Presentation {
                icon: None,
                surface_hint: None,
                entrypoint: "main".into(),
            },
            entrypoints: BTreeMap::from([(
                "main".to_string(),
                Entrypoint {
                    kind: EntrypointKind::Web,
                    path: "/".into(),
                    title: None,
                },
            )]),
            runtime: Runtime::StaticWeb {
                source: "web/".into(),
            },
            permissions,
            health: Health::default(),
        }
    }

    fn id() -> AppId {
        AppId::new("excalidraw").unwrap()
    }

    #[test]
    fn every_event_reports_its_app() {
        let events = [
            AppEvent::Installed { app: id() },
            AppEvent::StateChanged {
                app: id(),
                state: ObservedState::Running,
            },
            AppEvent::EndpointChanged {
                app: id(),
                url: Some("http://excalidraw.localhost:1/".into()),
            },
            AppEvent::ManifestChanged {
                app: id(),
                permission_changes: vec![],
            },
            AppEvent::Progress {
                app: id(),
                task: TaskId(1),
                progress: Progress {
                    phase: "安装依赖".into(),
                    done: 1,
                    total: None,
                },
            },
            AppEvent::LogAvailable { app: id() },
            AppEvent::RuntimeUnavailable {
                app: id(),
                reason: RuntimeReason::NotInstalled {
                    runtime: "docker".into(),
                },
            },
        ];
        for e in &events {
            assert_eq!(e.app(), &id());
        }
    }

    #[test]
    fn events_round_trip_through_json_with_tagged_names() {
        let e = AppEvent::RuntimeUnavailable {
            app: id(),
            reason: RuntimeReason::Unavailable {
                runtime: "docker".into(),
                detail: "Colima 没启动".into(),
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"event\":\"runtime_unavailable\""), "{json}");
        assert!(json.contains("\"reason\":\"unavailable\""), "{json}");
        assert_eq!(serde_json::from_str::<AppEvent>(&json).unwrap(), e);
    }

    #[test]
    fn manifest_changed_reports_only_real_permission_differences() {
        let old = manifest(Permissions {
            clipboard: Access::Read,
            ..Permissions::default()
        });
        let same = manifest(Permissions {
            clipboard: Access::Read,
            ..Permissions::default()
        });
        assert_eq!(manifest_changed(&old, &same), None);

        let more = manifest(Permissions {
            clipboard: Access::ReadWrite,
            ..Permissions::default()
        });
        match manifest_changed(&old, &more) {
            Some(AppEvent::ManifestChanged {
                app,
                permission_changes,
            }) => {
                assert_eq!(app, id());
                assert_eq!(permission_changes.len(), 1);
                assert_eq!(permission_changes[0].key, PermissionKey::Clipboard);
                assert!(permission_changes[0].escalation);
            }
            other => panic!("{other:?}"),
        }
    }
}
