//! 安装计划与审批:`install_plan → 审批 → install`。bytehost 只产出**可审查的信息**并保证"审批的就是装的",
//! **是否需要用户确认由产品决定**(Dozer 可以要求,Digger 可以更宽松)。

use serde::{Deserialize, Serialize};

use crate::id::{AppId, Version};
use crate::manifest::{Manifest, Runtime};
use crate::permissions::{
    Enforcement, PermissionChange, PermissionKey, Permissions, diff_permissions,
};
use crate::proto::SourceInfo;

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
    /// 来源披露(展示用,参与 `verify` 的逐字段核对)。旧形状 JSON(无此字段)解析为默认值。
    #[serde(default)]
    pub source_info: SourceInfo,
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
    pub source_info: SourceInfo,
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
            source_info: input.source_info,
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
            out.push(format!(
                "运行: {}",
                command
                    .iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            out.extend(runtime_version_note(runtime));
            out
        }
        Runtime::Container { image, .. } => {
            vec![format!("拉取镜像 {image}"), "以容器方式运行".to_string()]
        }
    }
}

/// 声明了版本要求时,`will_run` 里如实写一行。
///
/// `uv` 开头的 Python 应用由 uv 在运行时挑解释器,宿主拿不到,所以如实说明检查不适用。
fn runtime_version_note(runtime: &Runtime) -> Option<String> {
    match runtime {
        Runtime::Node {
            node: Some(req), ..
        } => Some(format!("需要 node {req}")),
        Runtime::Python {
            command,
            python: Some(req),
            ..
        } => {
            if command.first().map(String::as_str) == Some("uv") {
                Some(format!(
                    "需要 python {req};但命令由 uv 启动,版本要求不适用于 uv 管理的解释器"
                ))
            } else {
                Some(format!("需要 python {req}"))
            }
        }
        _ => None,
    }
}

/// 内部的 `'` 写成 `'\''`;控制字符与双向控制字符转义成 `\u{..}`——批准界面逐字展示这些文本,
/// 不能让 `["python","-m app"]` 与 `["python","-m","app"]` 看起来一样,也不能靠换行伪造多行。
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        return arg.to_string();
    }
    let mut out = String::from("'");
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else if c.is_control() || crate::manifest::is_bidi_control(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// 谁在什么时候批准的(审计用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub approver: String,
    pub approved_ms: u64,
}

/// 已批准的计划。字段私有、正常只经 [`InstallPlan::approve`] 构造——**这只是类型层面的约定,不是安全边界**:
/// 它可以从线上 JSON 反序列化出来(客户端可以发任何内容)。安全属性只来自 [`verify`](Self::verify):
/// 安装前必须用重新计算的计划核对,并只使用核对后返回的那份。`Approval` 只是审计记录,不防伪。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedInstallPlan {
    plan: InstallPlan,
    approval: Approval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    ManifestChanged,
    SourceChanged,
    /// 摘要一致,但计划里的其他内容(权限申请、强制等级、差异、来源、信任级别……)与重新计算的不同。
    PlanChanged,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestChanged => write!(f, "manifest 在审批之后发生了变化"),
            Self::SourceChanged => write!(f, "应用源码在审批之后发生了变化"),
            Self::PlanChanged => write!(f, "安装计划的内容与重新计算的不一致"),
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

    /// 安装时用**此刻重新计算**出来的计划 `fresh` 核对,并**返回这份新算的计划**——调用方安装、授予权限时
    /// 一律用返回值,**不要**用 `self.plan()`:`ApprovedInstallPlan` 可以从线上 JSON 反序列化出来,摘要对得上
    /// 并不代表 `requested`/`enforcement`/`permission_diff` 没被改过。
    ///
    /// 先比两个摘要(`ManifestChanged`/`SourceChanged`),再比整份计划的每个字段(`PlanChanged`)。
    pub fn verify(&self, fresh: InstallPlan) -> Result<InstallPlan, VerifyError> {
        if self.plan.manifest_digest != fresh.manifest_digest {
            return Err(VerifyError::ManifestChanged);
        }
        if self.plan.source_digest != fresh.source_digest {
            return Err(VerifyError::SourceChanged);
        }
        if self.plan != fresh {
            return Err(VerifyError::PlanChanged);
        }
        Ok(fresh)
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
            source_info: SourceInfo::default(),
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
    fn will_run_states_the_declared_runtime_version_requirement() {
        let node = Runtime::Node {
            command: vec!["node".into(), "server.js".into()],
            lockfile: None,
            node: Some(">=22".into()),
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        let plan = InstallPlan::build(input(&manifest(node, Permissions::default()), None));
        assert!(
            plan.will_run.iter().any(|l| l.contains("需要 node >=22")),
            "{:?}",
            plan.will_run
        );

        let uv = Runtime::Python {
            command: vec!["uv".into(), "run".into(), "a.py".into()],
            lockfile: None,
            python: Some(">=3.12".into()),
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        let plan = InstallPlan::build(input(&manifest(uv, Permissions::default()), None));
        assert!(
            plan.will_run
                .iter()
                .any(|l| l.contains("版本要求不适用于 uv 管理的解释器")),
            "{:?}",
            plan.will_run
        );

        let no_req = Runtime::Node {
            command: vec!["node".into()],
            lockfile: None,
            node: None,
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        let plan = InstallPlan::build(input(&manifest(no_req, Permissions::default()), None));
        assert!(
            plan.will_run.iter().all(|l| !l.contains("需要 node")),
            "{:?}",
            plan.will_run
        );
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

    fn fresh(m: &Manifest, manifest_digest: &str, source_digest: &str) -> InstallPlan {
        InstallPlan::build(PlanInput {
            manifest_digest: manifest_digest.into(),
            source_digest: source_digest.into(),
            ..input(m, None)
        })
    }

    fn static_manifest() -> Manifest {
        manifest(
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            Permissions::default(),
        )
    }

    #[test]
    fn verify_accepts_only_a_freshly_recomputed_plan_and_returns_that_fresh_plan() {
        let m = static_manifest();
        let approved = InstallPlan::build(input(&m, None)).approve(approval());
        let ok = approved.verify(fresh(&m, "m1", "s1")).unwrap();
        assert_eq!(&ok, approved.plan());
        assert_eq!(
            approved.verify(fresh(&m, "m2", "s1")),
            Err(VerifyError::ManifestChanged)
        );
        assert_eq!(
            approved.verify(fresh(&m, "m1", "s2")),
            Err(VerifyError::SourceChanged)
        );
        assert_eq!(
            approved.verify(fresh(&m, "m2", "s2")),
            Err(VerifyError::ManifestChanged),
            "两者都变:先报 manifest"
        );
    }

    /// 摘要都对、但线上 JSON 里的计划内容被改成更宽——`verify` 不能只比两个摘要。
    #[test]
    fn verify_rejects_a_payload_edited_after_the_digests_were_taken() {
        let m = static_manifest();
        let original = InstallPlan::build(input(&m, None)).approve(approval());

        let mut approved = original.clone();
        approved.plan.requested.clipboard = crate::permissions::Access::ReadWrite;
        assert_eq!(
            approved.verify(fresh(&m, "m1", "s1")),
            Err(VerifyError::PlanChanged)
        );

        let mut approved = original.clone();
        approved.plan.enforcement.clear();
        assert_eq!(
            approved.verify(fresh(&m, "m1", "s1")),
            Err(VerifyError::PlanChanged)
        );

        let mut approved = original.clone();
        approved.plan.upgrading_from = Some(Version::new(0, 0, 1));
        assert_eq!(
            approved.verify(fresh(&m, "m1", "s1")),
            Err(VerifyError::PlanChanged)
        );

        let mut approved = original;
        approved.plan.permission_diff.clear();
        approved.plan.trust = TrustLevel::Trusted;
        assert_eq!(
            approved.verify(fresh(&m, "m1", "s1")),
            Err(VerifyError::PlanChanged)
        );
    }

    /// `source_info` 参与核对:审批时披露的来源与重新算出的不同 → 拒绝。
    #[test]
    fn source_info_participates_in_verification() {
        let m = static_manifest();
        let mut approved = InstallPlan::build(input(&m, None)).approve(approval());
        approved.plan.source_info.kind = "url".into();
        approved.plan.source_info.pinned = true;
        assert_eq!(
            approved.verify(fresh(&m, "m1", "s1")),
            Err(VerifyError::PlanChanged)
        );
    }

    /// 旧形状计划 JSON(无 `source_info`)仍可解析,且默认值下 `verify` 照常工作。
    #[test]
    fn an_old_plan_payload_without_source_info_still_parses_and_verifies() {
        let m = static_manifest();
        let approved = InstallPlan::build(input(&m, None)).approve(approval());
        let mut value = serde_json::to_value(&approved).unwrap();
        value
            .get_mut("plan")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("source_info");
        let parsed: ApprovedInstallPlan = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.plan().source_info, SourceInfo::default());
        assert_eq!(
            parsed.verify(fresh(&m, "m1", "s1")).unwrap(),
            parsed.plan().clone()
        );
    }

    #[test]
    fn will_run_quotes_arguments_so_different_commands_never_render_identically() {
        let run = |cmd: &[&str]| {
            let py = Runtime::Python {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                lockfile: None,
                python: None,
                http: ProcessHttp {
                    port_env: "PORT".into(),
                },
            };
            InstallPlan::build(input(&manifest(py, Permissions::default()), None))
                .will_run
                .last()
                .unwrap()
                .clone()
        };
        assert_ne!(run(&["python", "-m app"]), run(&["python", "-m", "app"]));
        assert_eq!(run(&["python", "-m", "app"]), "运行: python -m app");
        assert_eq!(run(&["python", "a b"]), "运行: python 'a b'");
        assert_eq!(run(&["echo", "it's"]), "运行: echo 'it'\\''s'");
        assert_eq!(run(&["python", ""]), "运行: python ''");
        // 控制字符与双向控制字符被转义,不能在批准界面上伪造多行
        let forged = run(&["python", "x\n运行: rm -rf /"]);
        assert_eq!(forged, "运行: python 'x\\u{a}运行: rm -rf /'");
        assert!(!run(&["python", "a\u{202e}b"]).contains('\u{202e}'));
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
