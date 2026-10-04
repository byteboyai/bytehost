//! 安装计划与审批:`install_plan → 审批 → install`。bytehost 只产出**可审查的信息**并保证"审批的就是装的",
//! **是否需要用户确认由产品决定**(Dozer 可以要求,Digger 可以更宽松)。

use serde::{Deserialize, Serialize};

use crate::id::{AppId, Version};
use crate::manifest::{Manifest, Runtime};
use crate::permissions::{
    Enforcement, PermissionChange, PermissionKey, Permissions, diff_permissions,
};

/// 应用从哪来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// 用户本地目录/文件。
    Local,
    /// Agent 现场生成。
    AgentGenerated,
    /// 第三方来源(下载、仓库)。
    ThirdParty,
}

/// 信任级别:由**产品**根据来源与自己的策略给出,bytehost 只记录并展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

/// 一条权限在目标 runtime 上的强制等级(由 runtime adapter 的 `probe` 给出)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnforcementEntry {
    pub key: PermissionKey,
    pub enforcement: Enforcement,
}

/// 安装计划:UI 需要给用户看的全部信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallPlan {
    pub app_id: AppId,
    pub name: String,
    pub version: Version,
    /// 升级时是当前已安装版本,全新安装为 `None`。
    pub upgrading_from: Option<Version>,
    pub provenance: Provenance,
    pub trust: TrustLevel,
    pub runtime_kind: String,
    pub manifest_digest: String,
    pub source_digest: String,
    /// manifest 申请的权限。
    pub requested: Permissions,
    pub enforcement: Vec<EnforcementEntry>,
    /// 申请相对"当前已授予"的差异(全新安装时相对"什么都没授予")。
    pub permission_diff: Vec<PermissionChange>,
    /// 安装/运行时会执行什么——人类可读的描述,UI 原样展示。
    pub will_run: Vec<String>,
}

/// 当前已安装的版本与已授予的权限(升级时用来算差异)。
#[derive(Debug, Clone, Copy)]
pub struct Installed<'a> {
    pub version: &'a Version,
    pub grants: &'a Permissions,
}

/// 构造安装计划所需的全部输入(具名字段,避免相邻的同类型参数传错)。
pub struct PlanInput<'a> {
    pub manifest: &'a Manifest,
    pub manifest_digest: String,
    pub source_digest: String,
    pub provenance: Provenance,
    pub trust: TrustLevel,
    pub enforcement: Vec<EnforcementEntry>,
    pub installed: Option<Installed<'a>>,
}

impl InstallPlan {
    pub fn build(input: PlanInput<'_>) -> Self {
        let m = input.manifest;
        let baseline = input.installed.map(|i| *i.grants).unwrap_or_default();
        Self {
            app_id: m.id.clone(),
            name: m.name.clone(),
            version: m.version,
            upgrading_from: input.installed.map(|i| *i.version),
            provenance: input.provenance,
            trust: input.trust,
            runtime_kind: m.runtime.kind_name().to_string(),
            manifest_digest: input.manifest_digest,
            source_digest: input.source_digest,
            requested: m.permissions,
            enforcement: input.enforcement,
            permission_diff: diff_permissions(&baseline, &m.permissions),
            will_run: will_run(&m.runtime),
        }
    }

    /// 产品/用户批准这份计划。批准的内容就是这份计划(含两个摘要)。
    pub fn approve(self, approval: Approval) -> ApprovedInstallPlan {
        ApprovedInstallPlan {
            plan: self,
            approval,
        }
    }
}

fn will_run(runtime: &Runtime) -> Vec<String> {
    match runtime {
        Runtime::StaticWeb { source } => vec![format!(
            "不执行任何命令;由宿主直接提供 {source} 里的静态文件"
        )],
        Runtime::Node {
            command, lockfile, ..
        }
        | Runtime::Python {
            command, lockfile, ..
        } => {
            let mut out = Vec::new();
            if let Some(lock) = lockfile {
                out.push(format!(
                    "按 {lock} 安装依赖(依赖的安装脚本本身可以执行任意代码)"
                ));
            }
            out.push(format!("运行: {}", command.join(" ")));
            out
        }
        Runtime::Container { image, .. } => {
            vec![format!("拉取镜像 {image}"), "以容器方式运行".to_string()]
        }
    }
}

/// 谁在什么时候批准的(审计用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub approver: String,
    pub approved_ms: u64,
}

/// 已批准的计划。字段私有、只能经 [`InstallPlan::approve`] 构造,安装前必须 [`verify`](Self::verify)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedInstallPlan {
    plan: InstallPlan,
    approval: Approval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    ManifestChanged,
    SourceChanged,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestChanged => write!(f, "manifest 在审批之后发生了变化"),
            Self::SourceChanged => write!(f, "应用源码在审批之后发生了变化"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl ApprovedInstallPlan {
    pub fn plan(&self) -> &InstallPlan {
        &self.plan
    }

    pub fn approval(&self) -> &Approval {
        &self.approval
    }

    /// 安装时用**此刻重新计算**的两个摘要核对:任何一个与审批时不同就拒绝。
    pub fn verify(
        &self,
        manifest_digest: &str,
        source_digest: &str,
    ) -> Result<&InstallPlan, VerifyError> {
        if self.plan.manifest_digest != manifest_digest {
            return Err(VerifyError::ManifestChanged);
        }
        if self.plan.source_digest != source_digest {
            return Err(VerifyError::SourceChanged);
        }
        Ok(&self.plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        ContainerHttp, Entrypoint, EntrypointKind, Health, Presentation, ProcessHttp,
    };
    use crate::permissions::{Access, Gate};
    use std::collections::BTreeMap;

    fn manifest(runtime: Runtime, permissions: Permissions) -> Manifest {
        Manifest {
            schema_version: 1,
            min_host_version: Version::new(0, 1, 0),
            id: AppId::new("excalidraw").unwrap(),
            name: "Excalidraw".into(),
            version: Version::new(0, 17, 0),
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
            runtime,
            permissions,
            health: Health::default(),
        }
    }

    fn input<'a>(m: &'a Manifest, installed: Option<Installed<'a>>) -> PlanInput<'a> {
        PlanInput {
            manifest: m,
            manifest_digest: "m1".into(),
            source_digest: "s1".into(),
            provenance: Provenance::ThirdParty,
            trust: TrustLevel::Untrusted,
            enforcement: vec![EnforcementEntry {
                key: PermissionKey::NetworkOutbound,
                enforcement: Enforcement::Advisory,
            }],
            installed,
        }
    }

    fn approval() -> Approval {
        Approval {
            approver: "user".into(),
            approved_ms: 1,
        }
    }

    #[test]
    fn a_fresh_install_plan_carries_identity_provenance_digests_and_enforcement() {
        let perms = Permissions {
            clipboard: Access::ReadWrite,
            downloads: Gate::UserConfirm,
            ..Permissions::default()
        };
        let m = manifest(
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            perms,
        );
        let plan = InstallPlan::build(input(&m, None));
        assert_eq!(plan.app_id.as_str(), "excalidraw");
        assert_eq!(plan.upgrading_from, None);
        assert_eq!(plan.provenance, Provenance::ThirdParty);
        assert_eq!(plan.trust, TrustLevel::Untrusted);
        assert_eq!(plan.runtime_kind, "static_web");
        assert_eq!(
            (plan.manifest_digest.as_str(), plan.source_digest.as_str()),
            ("m1", "s1")
        );
        assert_eq!(plan.requested, perms);
        assert_eq!(
            plan.enforcement[0].enforcement,
            Enforcement::Advisory,
            "强制等级原样带进计划"
        );
        assert_eq!(plan.permission_diff.len(), 2);
        assert!(plan.permission_diff.iter().all(|c| c.escalation));
    }

    #[test]
    fn an_upgrade_plan_diffs_against_the_current_grants_not_against_nothing() {
        let granted = Permissions {
            clipboard: Access::ReadWrite,
            ..Permissions::default()
        };
        let requested = Permissions {
            clipboard: Access::ReadWrite,
            popups: Gate::UserConfirm,
            ..Permissions::default()
        };
        let m = manifest(
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            requested,
        );
        let v = Version::new(0, 16, 0);
        let plan = InstallPlan::build(input(
            &m,
            Some(Installed {
                version: &v,
                grants: &granted,
            }),
        ));
        assert_eq!(plan.upgrading_from, Some(v));
        assert_eq!(
            plan.permission_diff.len(),
            1,
            "剪贴板已授予,不再出现在差异里"
        );
        assert_eq!(plan.permission_diff[0].key, PermissionKey::Popups);
    }

    #[test]
    fn will_run_describes_commands_honestly_for_each_runtime() {
        let stat = InstallPlan::build(input(
            &manifest(
                Runtime::StaticWeb {
                    source: "web/".into(),
                },
                Permissions::default(),
            ),
            None,
        ));
        assert_eq!(stat.will_run.len(), 1);
        assert!(stat.will_run[0].contains("不执行任何命令"));

        let py = Runtime::Python {
            command: vec!["python".into(), "-m".into(), "app".into()],
            lockfile: Some("requirements.lock".into()),
            python: None,
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        let plan = InstallPlan::build(input(&manifest(py, Permissions::default()), None));
        assert_eq!(plan.will_run.len(), 2);
        assert!(
            plan.will_run[0].contains("requirements.lock") && plan.will_run[0].contains("任意代码")
        );
        assert_eq!(plan.will_run[1], "运行: python -m app");

        let digest = "b".repeat(64);
        let ct = Runtime::Container {
            image: format!("x/y@sha256:{digest}"),
            http: ContainerHttp { container_port: 80 },
        };
        let plan = InstallPlan::build(input(&manifest(ct, Permissions::default()), None));
        assert!(plan.will_run[0].contains(&digest));
    }

    #[test]
    fn an_approved_plan_verifies_only_against_the_same_digests() {
        let m = manifest(
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            Permissions::default(),
        );
        let approved = InstallPlan::build(input(&m, None)).approve(approval());
        assert!(approved.verify("m1", "s1").is_ok());
        assert_eq!(
            approved.verify("m2", "s1"),
            Err(VerifyError::ManifestChanged)
        );
        assert_eq!(approved.verify("m1", "s2"), Err(VerifyError::SourceChanged));
        assert_eq!(
            approved.verify("m2", "s2"),
            Err(VerifyError::ManifestChanged),
            "两者都变:先报 manifest"
        );
    }

    #[test]
    fn an_approved_plan_exposes_the_plan_and_the_approval_record() {
        let m = manifest(
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            Permissions::default(),
        );
        let approved = InstallPlan::build(input(&m, None)).approve(approval());
        assert_eq!(approved.plan().app_id.as_str(), "excalidraw");
        assert_eq!(approved.approval().approver, "user");
    }

    #[test]
    fn plans_round_trip_through_json_for_the_wire() {
        let m = manifest(
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            Permissions::default(),
        );
        let approved = InstallPlan::build(input(&m, None)).approve(approval());
        let json = serde_json::to_string(&approved).unwrap();
        assert_eq!(
            serde_json::from_str::<ApprovedInstallPlan>(&json).unwrap(),
            approved
        );
    }
}
