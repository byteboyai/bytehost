//! 与传输无关的应用宿主客户端 API。
//!
//! 消费方(Doze 的 `dozer-client`、Digger 等)实现 [`AppHostApi`]:只需要实现 `request` 与
//! `subscribe` 两个底层方法,其余类型化方法都是默认实现。这样同一套状态机(见 `bytehost-panel`)
//! 既能驱动经 UDS 连到 dozerd 的远端宿主,也能驱动进程内直接嵌入的 [`InProcess`](crate::in_process::InProcess)。
//!
//! 失败只有一种形状:[`AppApiError`]——宿主带类别的失败(`Host`)或传输/协议层的失败(`Transport`)。
//! "dozerd 不可用"是 `Host(AppFailure{ kind: Unavailable })`,属于**持久状态**;其余连接问题属 `Transport`。
//! 这个区分是 GUI"不可用是持久状态而非一次性 Toast"判断的依据,两种实现必须给出同样的类别。
//!
//! 本 crate **不依赖任何 dozer crate、iced、wry、tauri**。

use std::future::Future;

use bytehost_apps::id::AppId;
use bytehost_apps::plan::{ApprovedInstallPlan, InstallPlan, Provenance, TrustLevel};
use bytehost_apps::proto::{
    AppErrorKind, AppFailure, AppReply, AppRequest, AppSource, AppSummary, ManagedRuntime,
    RuntimeInstallPlan, RuntimeProbe,
};
use bytehost_apps::registry::UninstallMode;
use tokio::sync::mpsc;

#[cfg(feature = "conformance")]
pub mod conformance;
#[cfg(feature = "in-process")]
pub mod in_process;
/// 应用变更订阅里的一条事件。都是**失效信号**:收到后重拉 [`AppHostApi::app_list`] 即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppChange {
    /// 某个应用的状态/端点/问题变了。
    Changed(AppId),
    /// 广播落后丢了事件(或服务刚恢复):整体重拉。
    Resync,
    /// 订阅连接断了:调用方应退避重订阅,并回退到轮询兜底。
    Disconnected,
}

/// 一次调用的失败:宿主带类别的失败,或传输/协议层的(连不上、协议不符)。
#[derive(Debug, Clone, PartialEq)]
pub enum AppApiError {
    /// 宿主有类别地拒绝了这次请求(含"宿主不可用")。
    Host(AppFailure),
    /// 传输/协议层问题:连不上、应答不符预期、后台任务崩溃。
    Transport(String),
}

impl AppApiError {
    /// 给人看的失败原因。
    pub fn text(&self) -> &str {
        match self {
            Self::Host(f) => &f.message,
            Self::Transport(t) => t,
        }
    }

    /// 宿主失败带类别;传输层失败没有类别(`None`)。
    pub fn kind(&self) -> Option<AppErrorKind> {
        match self {
            Self::Host(f) => Some(f.kind),
            Self::Transport(_) => None,
        }
    }
}

impl std::fmt::Display for AppApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}

impl std::error::Error for AppApiError {}

impl From<AppFailure> for AppApiError {
    fn from(f: AppFailure) -> Self {
        Self::Host(f)
    }
}

fn unexpected(other: &AppReply) -> AppApiError {
    AppApiError::Transport(format!("意外应答: {other:?}"))
}

/// 与传输无关的应用宿主 API。默认实现全部基于 [`AppHostApi::request`];
/// 实现方只需要提供 `request` 与 `subscribe`。
pub trait AppHostApi: Send + Sync {
    /// 发送一个底层请求。失败带类别(见 [`AppApiError`])。
    fn request(
        &self,
        request: AppRequest,
    ) -> impl Future<Output = Result<AppReply, AppApiError>> + Send;

    /// 订阅应用变更。返回事件流;`Disconnected` 通常是流里的最后一条。
    fn subscribe(
        &self,
    ) -> impl Future<Output = Result<mpsc::UnboundedReceiver<AppChange>, AppApiError>> + Send;

    /// 已安装的应用(含状态与站点地址)。
    fn app_list(&self) -> impl Future<Output = Result<Vec<AppSummary>, AppApiError>> + Send {
        async move {
            match self.request(AppRequest::List).await? {
                AppReply::Apps { apps } => Ok(apps),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 出安装计划(不安装任何东西)。
    fn app_plan(
        &self,
        source: AppSource,
        provenance: Provenance,
        trust: TrustLevel,
    ) -> impl Future<Output = Result<InstallPlan, AppApiError>> + Send {
        async move {
            match self
                .request(AppRequest::Plan {
                    source,
                    provenance,
                    trust,
                })
                .await?
            {
                AppReply::Plan { plan } => Ok(*plan),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 安装一份已批准的计划(服务端对 staging 副本重新计算并核对)。
    fn app_install(
        &self,
        approved: ApprovedInstallPlan,
        source: AppSource,
    ) -> impl Future<Output = Result<(), AppApiError>> + Send {
        async move {
            let request = AppRequest::Install {
                approved: Box::new(approved),
                source,
            };
            self.app_expect_done(request).await
        }
    }

    /// 启动应用,返回不含令牌的站点地址。
    fn app_start(&self, id: AppId) -> impl Future<Output = Result<String, AppApiError>> + Send {
        async move {
            match self.request(AppRequest::Start { id }).await? {
                AppReply::Started { url } => Ok(url),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 停止应用。
    fn app_stop(&self, id: AppId) -> impl Future<Output = Result<(), AppApiError>> + Send {
        async move { self.app_expect_done(AppRequest::Stop { id }).await }
    }

    /// 手动回滚到上一版(仅当有可回滚的上一版时可用;回滚不得提升权限)。
    fn app_rollback(&self, id: AppId) -> impl Future<Output = Result<(), AppApiError>> + Send {
        async move { self.app_expect_done(AppRequest::Rollback { id }).await }
    }

    /// 卸载应用。
    fn app_uninstall(
        &self,
        id: AppId,
        mode: UninstallMode,
    ) -> impl Future<Output = Result<(), AppApiError>> + Send {
        async move {
            self.app_expect_done(AppRequest::Uninstall { id, mode })
                .await
        }
    }

    /// 首次导航用的地址(含令牌,**秘密**:不要写日志)。应用没在运行会失败。
    fn app_launch_url(
        &self,
        id: AppId,
    ) -> impl Future<Output = Result<String, AppApiError>> + Send {
        async move {
            match self.request(AppRequest::LaunchUrl { id }).await? {
                AppReply::LaunchUrl { url } => Ok(url),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 探测可用运行时。
    fn app_probe_runtimes(
        &self,
    ) -> impl Future<Output = Result<Vec<RuntimeProbe>, AppApiError>> + Send {
        async move {
            match self.request(AppRequest::ProbeRuntimes).await? {
                AppReply::Runtimes { runtimes } => Ok(runtimes),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 读某应用日志的末尾(有界、已清洗);返回 `(text, truncated)`。
    fn app_logs(
        &self,
        id: AppId,
        max_lines: u32,
    ) -> impl Future<Output = Result<(String, bool), AppApiError>> + Send {
        async move {
            match self.request(AppRequest::Logs { id, max_lines }).await? {
                AppReply::Logs { text, truncated } => Ok((text, truncated)),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 出一份运行时安装计划(不下载任何东西)。
    fn app_runtime_plan(
        &self,
        runtime: ManagedRuntime,
    ) -> impl Future<Output = Result<RuntimeInstallPlan, AppApiError>> + Send {
        async move {
            match self.request(AppRequest::RuntimePlan { runtime }).await? {
                AppReply::RuntimePlan { plan } => Ok(*plan),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// 安装一份已批准的运行时计划(服务端重算并逐字段核对)。
    fn app_install_runtime(
        &self,
        plan: RuntimeInstallPlan,
    ) -> impl Future<Output = Result<(), AppApiError>> + Send {
        async move {
            self.app_expect_done(AppRequest::InstallRuntime {
                plan: Box::new(plan),
            })
            .await
        }
    }

    /// 卸载某个受管运行时版本。
    fn app_uninstall_runtime(
        &self,
        runtime: ManagedRuntime,
        version: &str,
    ) -> impl Future<Output = Result<(), AppApiError>> + Send {
        let version = version.to_string();
        async move {
            self.app_expect_done(AppRequest::UninstallRuntime { runtime, version })
                .await
        }
    }

    /// 期望一个不带内容的应答(`Done`)。
    fn app_expect_done(
        &self,
        request: AppRequest,
    ) -> impl Future<Output = Result<(), AppApiError>> + Send {
        async move {
            match self.request(request).await? {
                AppReply::Done => Ok(()),
                other => Err(unexpected(&other)),
            }
        }
    }
}
