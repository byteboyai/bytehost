//! 分发:后台执行器适配与结果投递。
//!
//! core (`bytehost_core::request`) 定义了 `BackgroundExecutor` / `Emitter` 抽象;
//! 本模块提供 Tokio 与 iced 的具体实现:
//! - `TokioExecutor`:把 `'static + Send` 任务投到 Tokio 运行时;
//! - `ProxyEmitter`:把结果信封经 iced `Sender` 投回 UI 线程(线程安全)。
//!
//! 约束:后台任务与 IPC channel 的内容必须 `Send`;`WebView`/`Element` 等非 `Send`
//! 句柄只在主线程(thread_local),不进 `Task` 的 future。

use bytehost_core::{BackgroundExecutor, Emitter};

/// 基于 Tokio 运行时句柄的后台执行器。
#[derive(Clone)]
pub struct TokioExecutor {
    handle: tokio::runtime::Handle,
}

impl TokioExecutor {
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self { handle }
    }

    /// 用当前线程的 Tokio 运行时句柄构造;不在 Tokio 上下文时返回 `None`。
    pub fn current() -> Option<Self> {
        tokio::runtime::Handle::try_current().ok().map(Self::new)
    }

    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.handle
    }
}

impl BackgroundExecutor for TokioExecutor {
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        self.handle.spawn_blocking(task);
    }
}

/// 把结果经 iced `mpsc::Sender` 投回 UI 线程。`Sender` 自身 `Send + Clone`,
/// 因此可在后台线程持有并调用。
///
/// iced 0.14 的跨线程回灌通道是 `futures::channel::mpsc`;产品在
/// `Subscription::run`/`Task::stream` 里创建 `(sender, receiver)`,把 `sender`
/// 交给本类型,再让 `receiver` 产出 `M`。
#[derive(Clone)]
pub struct ProxyEmitter<M> {
    sender: iced::futures::channel::mpsc::Sender<M>,
}

impl<M> ProxyEmitter<M> {
    pub fn new(sender: iced::futures::channel::mpsc::Sender<M>) -> Self {
        Self { sender }
    }
}

impl<M: Send + 'static> Emitter<M> for ProxyEmitter<M> {
    fn emit(&self, message: M) {
        let mut sender = self.sender.clone();
        let _ = sender.try_send(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_core::{
        Generation, PanelInstanceId, RequestKey, RequestTracker, ScopeId, run_scoped,
    };
    use std::sync::mpsc;

    #[derive(Clone)]
    struct ChannelEmitter<T>(mpsc::Sender<T>);
    impl<T: Send + 'static> Emitter<T> for ChannelEmitter<T> {
        fn emit(&self, message: T) {
            let _ = self.0.send(message);
        }
    }

    #[test]
    fn tokio_executor_runs_blocking_task() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let exec = TokioExecutor::new(rt.handle().clone());
        exec.spawn(Box::new(move || {
            let _ = tx.send(42u32);
        }));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            42
        );
    }

    #[test]
    fn run_scoped_with_tokio_executor_dispatches_current_result() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let exec = TokioExecutor::new(rt.handle().clone());
        let (tx, rx) = mpsc::channel();
        let emitter = ChannelEmitter(tx);

        let tracker = RequestTracker::new(
            1,
            ScopeId::new("p").unwrap(),
            PanelInstanceId::new(0, 0),
            Generation::new(0),
        );
        let token = tracker.begin(RequestKey::new("list"));
        run_scoped(&tracker, token, 7u32, &exec, &emitter, |c| c * 2);

        let env = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(env.payload, 14);
    }
}
