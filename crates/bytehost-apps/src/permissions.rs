//! 权限模型:manifest 里的是**申请**(`Permissions`),用户**实际授予**的另存(`AppRecord.grants`,同一个类型)。
//! 每条权限还有强制等级(`Enforcement`),由 runtime adapter 在探测时给出——UI 必须原样展示。

use serde::{Deserialize, Serialize};

/// 读写访问级别,按从弱到强排序(`None < Read < ReadWrite`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    None,
    Read,
    ReadWrite,
}

/// 需要门控的能力,按从严到松排序(`Deny < UserConfirm < Allow`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    #[default]
    Deny,
    UserConfirm,
    Allow,
}

/// 出站网络(`None < Any`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outbound {
    #[default]
    None,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkPerm {
    pub outbound: Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FilesystemPerm {
    /// 应用自己的 `data/` 目录。
    pub data: Access,
}

/// 一组权限。缺省的字段取**最严**的值;未知字段整体拒绝(新版权限被旧宿主静默忽略是安全问题)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    pub network: NetworkPerm,
    pub filesystem: FilesystemPerm,
    pub clipboard: Access,
    pub downloads: Gate,
    pub popups: Gate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKey {
    NetworkOutbound,
    FilesystemData,
    Clipboard,
    Downloads,
    Popups,
}

impl PermissionKey {
    pub const ALL: [PermissionKey; 5] = [
        Self::NetworkOutbound,
        Self::FilesystemData,
        Self::Clipboard,
        Self::Downloads,
        Self::Popups,
    ];
}

/// 强制等级:该权限在这个 runtime 上能不能真正被执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// 由宿主/runtime 真正强制(如静态 Web 的出站网络限制可由 CSP 强制)。
    Enforced,
    /// 只是声明,不被强制(如 macOS 上 Node/Python 的 `network: none`)。
    Advisory,
    /// 这个 runtime 完全不支持。
    Unsupported,
}

impl Permissions {
    /// 某条权限的(强度序号,展示用标签)。序号越大授予的能力越多。
    pub fn level(&self, key: PermissionKey) -> (u8, &'static str) {
        match key {
            PermissionKey::NetworkOutbound => match self.network.outbound {
                Outbound::None => (0, "none"),
                Outbound::Any => (1, "any"),
            },
            PermissionKey::FilesystemData => access(self.filesystem.data),
            PermissionKey::Clipboard => access(self.clipboard),
            PermissionKey::Downloads => gate(self.downloads),
            PermissionKey::Popups => gate(self.popups),
        }
    }
}

fn access(a: Access) -> (u8, &'static str) {
    match a {
        Access::None => (0, "none"),
        Access::Read => (1, "read"),
        Access::ReadWrite => (2, "read_write"),
    }
}

fn gate(g: Gate) -> (u8, &'static str) {
    match g {
        Gate::Deny => (0, "deny"),
        Gate::UserConfirm => (1, "user_confirm"),
        Gate::Allow => (2, "allow"),
    }
}

/// 申请与授予之间的一处差异。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChange {
    pub key: PermissionKey,
    pub from: String,
    pub to: String,
    /// 申请比当前授予**更多**(需要用户重新确认);降低权限为 `false`。
    pub escalation: bool,
}

/// `requested`(manifest 申请)相对 `granted`(当前已授予)的全部差异,按 `PermissionKey::ALL` 的顺序。
/// 全新安装时 `granted` 传 `Permissions::default()`(什么都没授予)。
pub fn diff_permissions(granted: &Permissions, requested: &Permissions) -> Vec<PermissionChange> {
    PermissionKey::ALL
        .into_iter()
        .filter_map(|key| {
            let (from_n, from) = granted.level(key);
            let (to_n, to) = requested.level(key);
            (from_n != to_n).then(|| PermissionChange {
                key,
                from: from.to_string(),
                to: to.to_string(),
                escalation: to_n > from_n,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_permissions_are_the_most_restrictive() {
        let p = Permissions::default();
        for key in PermissionKey::ALL {
            assert_eq!(p.level(key).0, 0, "{key:?}");
        }
    }

    #[test]
    fn unknown_permission_fields_are_rejected_at_every_level() {
        assert!(serde_json::from_str::<Permissions>(r#"{"telepathy":"allow"}"#).is_err());
        assert!(
            serde_json::from_str::<Permissions>(
                r#"{"network":{"outbound":"none","inbound":"any"}}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<Permissions>(r#"{"filesystem":{"home":"read"}}"#).is_err());
        assert!(serde_json::from_str::<Permissions>(r#"{"clipboard":"sometimes"}"#).is_err());
    }

    #[test]
    fn permissions_serde_uses_snake_case_labels_and_fills_missing_with_strictest() {
        let p: Permissions =
            serde_json::from_str(r#"{"clipboard":"read_write","downloads":"user_confirm"}"#)
                .unwrap();
        assert_eq!(p.clipboard, Access::ReadWrite);
        assert_eq!(p.downloads, Gate::UserConfirm);
        assert_eq!(p.popups, Gate::Deny);
        assert_eq!(p.network.outbound, Outbound::None);
    }

    #[test]
    fn levels_are_ordered_from_strict_to_loose() {
        assert!(Access::None < Access::Read && Access::Read < Access::ReadWrite);
        assert!(Gate::Deny < Gate::UserConfirm && Gate::UserConfirm < Gate::Allow);
        assert!(Outbound::None < Outbound::Any);
    }

    #[test]
    fn fresh_install_diff_lists_every_requested_capability_as_an_escalation() {
        let requested = Permissions {
            network: NetworkPerm {
                outbound: Outbound::Any,
            },
            filesystem: FilesystemPerm {
                data: Access::ReadWrite,
            },
            clipboard: Access::Read,
            downloads: Gate::UserConfirm,
            popups: Gate::Deny,
        };
        let diff = diff_permissions(&Permissions::default(), &requested);
        let keys: Vec<_> = diff.iter().map(|c| c.key).collect();
        assert_eq!(
            keys,
            vec![
                PermissionKey::NetworkOutbound,
                PermissionKey::FilesystemData,
                PermissionKey::Clipboard,
                PermissionKey::Downloads
            ],
            "popups 申请的就是 deny,和默认相同,不算差异;顺序固定"
        );
        assert!(diff.iter().all(|c| c.escalation));
        assert_eq!(diff[1].from, "none");
        assert_eq!(diff[1].to, "read_write");
    }

    #[test]
    fn upgrade_diff_distinguishes_escalation_from_reduction() {
        let granted = Permissions {
            clipboard: Access::ReadWrite,
            downloads: Gate::Allow,
            ..Permissions::default()
        };
        let requested = Permissions {
            clipboard: Access::Read,
            downloads: Gate::Allow,
            popups: Gate::UserConfirm,
            ..Permissions::default()
        };
        let diff = diff_permissions(&granted, &requested);
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].key, PermissionKey::Clipboard);
        assert!(!diff[0].escalation, "read_write → read 是降权");
        assert_eq!(diff[1].key, PermissionKey::Popups);
        assert!(diff[1].escalation);
    }

    #[test]
    fn identical_permissions_have_no_diff() {
        let p = Permissions {
            clipboard: Access::Read,
            ..Permissions::default()
        };
        assert!(diff_permissions(&p, &p).is_empty());
    }
}
