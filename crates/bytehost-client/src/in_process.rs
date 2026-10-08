//! 进程内实现:直接把请求交给嵌入的 [`AppService`],订阅直接转发它的事件广播。
//!
//! 供没有 dozerd 的宿主(如 Digger)在自己的进程里嵌入应用宿主。行为与 UDS 实现
//! (Doze 的 `dozer-client`)必须一致——由 [`crate::conformance`] 的两个实现共同保证。

use std::sync::Arc;

use bytehost_apps::event::AppEvent;
use bytehost_apps::proto::{AppErrorKind, AppFailure, AppReply, AppRequest};
use bytehost_apps::service::{AppService, change_reply};
use tokio::sync::{broadcast, mpsc};

use crate::{AppApiError, AppChange, AppHostApi};

/// 直接驱动一个 [`AppService`] 的 [`AppHostApi`]。
pub struct InProcess {
    service: Arc<AppService>,
}

impl InProcess {
    /// 包一个已有的服务。
    pub fn new(service: Arc<AppService>) -> Self {
        Self { service }
    }

    /// 启动一个服务(应用根目录下的自有 staging/registry),再包起来。
    ///
    /// 应用根目录打不开时 `AppService` 会进入 `Unavailable`,此时 `request` 一律回
    /// `Host(Unavailable)`,与经 UDS 连到一个不可用的 dozerd 表现一致。
    pub async fn start(root: &std::path::Path) -> Self {
        Self::new(AppService::start(root).await)
    }

    /// 一个恒不可用的服务(测试/嵌入方还没准备好应用目录时)。
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::new(AppService::unavailable(reason))
    }

    /// 底层服务(嵌入方做生命周期收尾,如 `shutdown`)。
    pub fn service(&self) -> &Arc<AppService> {
        &self.service
    }
}

impl AppHostApi for InProcess {
    async fn request(&self, request: AppRequest) -> Result<AppReply, AppApiError> {
        // `Subscribe` 走 `subscribe`,不走一问一答——与 UDS 实现里 dozerd 单独拦截一致。
        if matches!(request, AppRequest::Subscribe) {
            return Err(AppApiError::Transport(
                "订阅请用 subscribe(),不是 request(Subscribe)".into(),
            ));
        }
        self.service
            .handle(request)
            .await
            .map_err(AppApiError::Host)
    }

    async fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<AppChange>, AppApiError> {
        let Some(mut rx): Option<broadcast::Receiver<AppEvent>> = self.service.subscribe() else {
            return Err(AppApiError::Host(AppFailure::new(
                AppErrorKind::Unavailable,
                "应用宿主不可用",
            )));
        };
        let (tx, out) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match change_reply(rx.recv().await) {
                    Some(AppReply::Changed { app }) => {
                        if tx.send(AppChange::Changed(app)).is_err() {
                            break;
                        }
                    }
                    Some(AppReply::Resync) => {
                        if tx.send(AppChange::Resync).is_err() {
                            break;
                        }
                    }
                    // change_reply 只产出 Changed/Resync;其余形状不应出现。
                    Some(_) => {}
                    // 广播关闭:服务没了。与 UDS 断开一致,发 Disconnected 收尾。
                    None => {
                        let _ = tx.send(AppChange::Disconnected);
                        break;
                    }
                }
            }
        });
        Ok(out)
    }
}
