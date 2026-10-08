//! 应用宿主在 dozerd 里的服务:持有 `AppManager` 与 gateway,把线上的 [`AppRequest`] 翻译成对它们的调用。
//!
//! 设计要点(规格 §6.1,用户已定:**应用跟随 dozerd,dozerd 停止则应用停止**):
//! - **启动永不失败**:gateway 端口被占用、应用目录不可用等问题只会让这个服务变成"不可用"(每个请求回带原因的
//!   错误),不会拖垮 dozerd 的会话功能;
//! - 启动时 `reconcile`(把上次随 dozerd 一起停掉的应用,按 `desired = Running` 重新拉起);
//! - 退出时 `shutdown`:撤下站点、观察态落成 `Stopped`,**保留 `desired`**,下次启动自动恢复;
//! - `AppManager` 做阻塞文件 I/O 并持有 `std::sync::Mutex`,所以每次调用都经 `spawn_blocking`。

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::HOST_VERSION;
use crate::gateway::{Gateway, GatewayConfig, GatewayError};
use crate::manager::{AppManager, ManagerConfig, ManagerError, MonitorConfig};
use crate::port::{load_port, persist_port, pick_port};
use crate::process::restart::RestartPolicy;
use crate::proto::{AppErrorKind, AppFailure, AppReply, AppRequest, ManagedRuntime, RuntimeProbe};
use crate::runtime::managed::{
    ChainResolver, Fetcher, ManagedResolver, RtError, RuntimeManager, RuntimeStore,
};
use crate::runtime::{SystemResolver, SystemRunner, probe_all};

enum State {
    Ready {
        manager: Arc<AppManager>,
        gateway: Arc<Gateway>,
        runtime_manager: Arc<RuntimeManager>,
    },
    Unavailable(String),
}

pub struct AppService {
    state: State,
}

impl AppService {
    /// 一个永远回错误的服务(测试里不需要应用宿主时用,也是启动失败时的形态)。
    pub fn unavailable(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            state: State::Unavailable(reason.into()),
        })
    }

    /// 用**持久化的端口**启动。首次运行(没有 `<root>/gateway.json`)随机选端口、**绑定成功之后**才写盘;
    /// 选中的端口恰好被占就换一个再试(此时还没有任何应用数据绑定在旧端口上)。永不失败。
    pub async fn start(root: &Path) -> Arc<Self> {
        match load_port(root) {
            Ok(Some(port)) => Self::start_with(root, GatewayConfig { port }).await,
            Ok(None) => Self::first_run(root, pick_port).await,
            Err(e) => {
                tracing::error!(target: "bytehost::service", error = %e, "读取 gateway 端口失败,应用宿主不可用");
                Self::unavailable(format!("应用宿主不可用:{e}"))
            }
        }
    }

    /// 首次运行选端口的重试次数(范围 12768 个端口,连续占用 5 个几乎不可能,真发生就说明环境有问题)。
    const FIRST_RUN_ATTEMPTS: usize = 5;

    async fn first_run(root: &Path, mut pick: impl FnMut() -> u16) -> Arc<Self> {
        for _ in 0..Self::FIRST_RUN_ATTEMPTS {
            let port = pick();
            match Gateway::start(GatewayConfig { port }).await {
                Ok(gateway) => {
                    // 绑定成功才记住;写不进去就不能继续——否则下次启动换端口,所有应用的本地存储都会丢。
                    if let Err(e) = persist_port(root, port) {
                        gateway.stop().await;
                        tracing::error!(target: "bytehost::service", error = %e, "gateway 端口写盘失败,应用宿主不可用");
                        return Self::unavailable(format!("应用宿主不可用:端口无法保存:{e}"));
                    }
                    return Self::finish_start(root, Arc::new(gateway)).await;
                }
                Err(GatewayError::PortInUse(p)) => {
                    tracing::warn!(target: "bytehost::service", port = p, "首次选的端口被占用,换一个重试");
                }
                Err(e) => {
                    tracing::error!(target: "bytehost::service", error = %e, "gateway 启动失败,应用宿主不可用");
                    return Self::unavailable(format!("应用宿主不可用:{e}"));
                }
            }
        }
        Self::unavailable(format!(
            "应用宿主不可用:连续 {} 次选到的端口都被占用",
            Self::FIRST_RUN_ATTEMPTS
        ))
    }

    /// 用给定的 gateway 配置启动(测试用 `port: 0`)。永不失败。
    pub async fn start_with(root: &Path, config: GatewayConfig) -> Arc<Self> {
        let gateway = match Gateway::start(config).await {
            Ok(g) => Arc::new(g),
            Err(e) => {
                tracing::error!(target: "bytehost::service", error = %e, "gateway 启动失败,应用宿主不可用");
                return Self::unavailable(format!("应用宿主不可用:{e}"));
            }
        };
        Self::finish_start(root, gateway).await
    }

    /// (仅测试)调小周期健康检查参数与重启退避,让卡死/崩溃的进程在秒级被观察到;
    /// 启动流程与 `start_with` 完全相同(同一个 `finish_start_with`)。
    /// 跨 crate 集成测试(`tests/`)要用,不能 `#[cfg(test)]`,故为 `#[doc(hidden)]`。
    #[doc(hidden)]
    pub async fn start_with_monitor_for_test(
        root: &Path,
        config: GatewayConfig,
        monitor: MonitorConfig,
    ) -> Arc<Self> {
        let gateway = match Gateway::start(config).await {
            Ok(g) => Arc::new(g),
            Err(e) => {
                tracing::error!(target: "bytehost::service", error = %e, "gateway 启动失败,应用宿主不可用");
                return Self::unavailable(format!("应用宿主不可用:{e}"));
            }
        };
        let runtime_manager = Arc::new(RuntimeManager::new(root.join("runtimes")));
        Self::finish_start_with(root, gateway, runtime_manager, Some(monitor)).await
    }

    async fn finish_start(root: &Path, gateway: Arc<Gateway>) -> Arc<Self> {
        let runtime_manager = Arc::new(RuntimeManager::new(root.join("runtimes")));
        Self::finish_start_with(root, gateway, runtime_manager, None).await
    }

    /// 真正的构造主体:`RuntimeManager` 可注入(测试用假 `Fetcher`);
    /// `monitor` 只有测试会给(同时把重启退避调小),生产为 `None`;
    /// `app_fetcher` 只有测试会给(注入假的来源下载器,替代真实的 `CurlFetcher`),生产为 `None`。
    async fn finish_start_with(
        root: &Path,
        gateway: Arc<Gateway>,
        runtime_manager: Arc<RuntimeManager>,
        monitor: Option<MonitorConfig>,
    ) -> Arc<Self> {
        Self::finish_start_with_sources(root, gateway, runtime_manager, monitor, None).await
    }

    /// 同 `finish_start_with`,再允许注入应用**来源**的下载器(测试 URL 来源用,不落真实网络)。
    #[doc(hidden)]
    pub async fn finish_start_with_sources(
        root: &Path,
        gateway: Arc<Gateway>,
        runtime_manager: Arc<RuntimeManager>,
        monitor: Option<MonitorConfig>,
        app_fetcher: Option<Arc<dyn Fetcher>>,
    ) -> Arc<Self> {
        // 受管运行时优先,系统兜底。
        let resolver = Arc::new(ChainResolver(vec![
            Arc::new(ManagedResolver::new(RuntimeStore::new(
                runtime_manager.store().root().to_path_buf(),
            ))),
            Arc::new(SystemResolver::new()),
        ]));
        let mut config = ManagerConfig {
            resolver,
            ..ManagerConfig::default()
        };
        if let Some(fetcher) = app_fetcher {
            config.fetcher = fetcher;
        }
        if let Some(monitor) = monitor {
            config.monitor = monitor;
            config.policy = RestartPolicy {
                base: std::time::Duration::from_millis(200),
                ..RestartPolicy::default()
            };
        }
        let manager = match AppManager::with_config(root, HOST_VERSION, gateway.clone(), config) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                gateway.stop().await;
                tracing::error!(target: "bytehost::service", error = %e, "应用目录不可用");
                return Self::unavailable(format!("应用宿主不可用:应用目录打不开:{e}"));
            }
        };
        spawn_event_logger(&manager);
        let for_reconcile = manager.clone();
        match tokio::task::spawn_blocking(move || for_reconcile.reconcile()).await {
            Ok(report) => log_report(&report, "启动对账"),
            Err(e) => {
                tracing::error!(target: "bytehost::service", error = %e, "启动对账任务失败(panic?)")
            }
        }
        tracing::info!(target: "bytehost::service", port = gateway.port(), "应用宿主已启动");
        Arc::new(Self {
            state: State::Ready {
                manager,
                gateway,
                runtime_manager,
            },
        })
    }

    pub async fn handle(&self, request: AppRequest) -> Result<AppReply, AppFailure> {
        let (manager, runtime_manager) = match &self.state {
            State::Ready {
                manager,
                runtime_manager,
                ..
            } => (manager, runtime_manager),
            State::Unavailable(reason) => {
                return Err(AppFailure::new(AppErrorKind::Unavailable, reason.clone()));
            }
        };
        match request {
            AppRequest::List => blocking(manager, |m| m.list())
                .await
                .map(|apps| AppReply::Apps { apps }),
            AppRequest::Plan {
                source,
                provenance,
                trust,
            } => blocking(manager, move |m| m.install_plan(&source, provenance, trust))
                .await
                .map(|plan| AppReply::Plan {
                    plan: Box::new(plan),
                }),
            AppRequest::Install { approved, source } => {
                blocking(manager, move |m| m.install(&approved, &source, now_ms()))
                    .await
                    .map(|()| AppReply::Done)
            }
            AppRequest::Start { id } => blocking(manager, move |m| m.start(&id))
                .await
                .map(|url| AppReply::Started { url }),
            AppRequest::Stop { id } => blocking(manager, move |m| m.stop(&id))
                .await
                .map(|()| AppReply::Done),
            AppRequest::Rollback { id } => blocking(manager, move |m| m.rollback(&id))
                .await
                .map(|()| AppReply::Done),
            AppRequest::Uninstall { id, mode } => {
                blocking(manager, move |m| m.uninstall(&id, mode))
                    .await
                    .map(|()| AppReply::Done)
            }
            AppRequest::LaunchUrl { id } => {
                // 只给正在运行的应用发带令牌的地址;判断与构造在 manager 的锁内一起完成
                blocking(manager, move |m| m.launch_url_if_running(&id))
                    .await
                    .map(|url| AppReply::LaunchUrl { url })
            }
            AppRequest::ProbeRuntimes => {
                let probes = tokio::task::spawn_blocking(|| probe_all(&SystemRunner::default()))
                    .await
                    .map_err(|e| {
                        AppFailure::new(AppErrorKind::Internal, format!("探测任务失败: {e}"))
                    })?;
                let runtime_manager = runtime_manager.clone();
                let installable_target = crate::runtime::managed::Target::current().is_some();
                let jobs = runtime_manager.jobs();
                Ok(AppReply::Runtimes {
                    runtimes: probes
                        .into_iter()
                        .map(|(runtime, availability)| {
                            let (managed, installable) = match runtime {
                                "node" => (
                                    runtime_manager.installed(ManagedRuntime::Node),
                                    installable_target,
                                ),
                                "python" => (
                                    runtime_manager.installed(ManagedRuntime::Python),
                                    installable_target,
                                ),
                                _ => (Vec::new(), false),
                            };
                            let jrt = match runtime {
                                "node" => Some(ManagedRuntime::Node),
                                "python" => Some(ManagedRuntime::Python),
                                _ => None,
                            };
                            let job =
                                jrt.and_then(|rt| jobs.iter().find(|j| j.runtime == rt).cloned());
                            RuntimeProbe {
                                runtime: runtime.to_string(),
                                availability,
                                managed,
                                installable,
                                job,
                            }
                        })
                        .collect(),
                })
            }
            AppRequest::RuntimePlan { runtime } => {
                let runtime_manager = runtime_manager.clone();
                tokio::task::spawn_blocking(move || runtime_manager.plan(runtime))
                    .await
                    .map_err(|e| {
                        AppFailure::new(AppErrorKind::Internal, format!("计划任务失败: {e}"))
                    })?
                    .map(|plan| AppReply::RuntimePlan {
                        plan: Box::new(plan),
                    })
                    .map_err(rt_failure)
            }
            AppRequest::InstallRuntime { plan } => {
                let runtime_manager = runtime_manager.clone();
                tokio::task::spawn_blocking(move || runtime_manager.start_install(&plan))
                    .await
                    .map_err(|e| {
                        AppFailure::new(AppErrorKind::Internal, format!("安装任务失败: {e}"))
                    })?
                    .map(|()| AppReply::Done)
                    .map_err(rt_failure)
            }
            AppRequest::UninstallRuntime { runtime, version } => {
                let runtime_manager = runtime_manager.clone();
                tokio::task::spawn_blocking(move || runtime_manager.uninstall(runtime, &version))
                    .await
                    .map_err(|e| {
                        AppFailure::new(AppErrorKind::Internal, format!("卸载任务失败: {e}"))
                    })?
                    .map(|()| AppReply::Done)
                    .map_err(rt_failure)
            }
            AppRequest::Logs { id, max_lines } => {
                blocking(manager, move |m| m.logs(&id, max_lines))
                    .await
                    .map(|(text, truncated)| AppReply::Logs { text, truncated })
            }
            // `Subscribe` 在 server.rs 的连接循环里就被截住了(它要把本连接写侧改成持续流),
            // 到不了这里。真到了说明有人绕过 server 直接调 handle——直接不实现。
            AppRequest::Subscribe => Err(AppFailure::new(
                AppErrorKind::Internal,
                "订阅必须经连接循环处理",
            )),
        }
    }

    /// 订阅应用变更事件流(`AppManager::events()` 的 broadcast 接收者)。
    /// `Unavailable` 状态返回 `None`——服务不可用时无处订阅,由调用方回一个失败。
    /// 每个订阅者各拿一份独立接收者;容量有限,慢消费者会拿到 `Lagged`(调用方转成整体重拉)。
    pub fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<crate::event::AppEvent>> {
        match &self.state {
            State::Ready { manager, .. } => Some(manager.events()),
            State::Unavailable(_) => None,
        }
    }

    /// gateway 已经停了(或者根本没起来)。测试用它判断"dozerd 停了应用也停了"——不靠再连一次端口,
    /// 端口一释放就可能被别的程序立刻重新占用。
    pub fn gateway_stopped(&self) -> bool {
        match &self.state {
            State::Ready { gateway, .. } => gateway.is_stopped(),
            State::Unavailable(_) => true,
        }
    }

    /// dozerd 退出前调用(幂等):撤下站点、观察态落成 `Stopped`、停 gateway;`desired` 保留。
    pub async fn shutdown(&self) {
        let State::Ready {
            manager,
            gateway,
            runtime_manager,
        } = &self.state
        else {
            return;
        };
        // 先取消并收掉还在跑的运行时安装/下载线程,再撤应用(不能把 dozerd 的退出拖住)。
        let rm = runtime_manager.clone();
        let _ = tokio::task::spawn_blocking(move || rm.cancel_all_and_join()).await;
        let m = manager.clone();
        match tokio::task::spawn_blocking(move || m.suspend_all()).await {
            Ok(report) => log_report(&report, "退出收尾"),
            Err(e) => {
                tracing::error!(target: "bytehost::service", error = %e, "退出收尾任务失败(panic?)")
            }
        }
        gateway.stop().await;
    }
}

/// 把对账/收尾的结果写进日志:单个应用的失败,以及读不出来的记录(损坏的 `state.json`)。
fn log_report(report: &crate::manager::ReconcileReport, what: &str) {
    for (app, error) in &report.failures {
        tracing::warn!(target: "bytehost::service", app = %app, error = %error, "{what}:应用处理失败");
    }
    for (dir, error) in &report.problems {
        tracing::warn!(target: "bytehost::service", dir = %dir, error = %error, "{what}:读不出应用记录");
    }
}

async fn blocking<T, F>(manager: &Arc<AppManager>, f: F) -> Result<T, AppFailure>
where
    T: Send + 'static,
    F: FnOnce(&AppManager) -> Result<T, ManagerError> + Send + 'static,
{
    let manager = manager.clone();
    tokio::task::spawn_blocking(move || f(&manager))
        .await
        .map_err(|e| AppFailure::new(AppErrorKind::Internal, format!("应用宿主任务失败: {e}")))?
        .map_err(|e| AppFailure::new(e.kind(), e.to_string()))
}

/// `RuntimeManager` 的错误映射成带类别的失败:不支持→Unsupported,冲突→Conflict,
/// 计划变化→Rejected,I/O→Internal。
fn rt_failure(e: RtError) -> AppFailure {
    let kind = match &e {
        RtError::Unsupported(_) => AppErrorKind::Unsupported,
        RtError::Conflict(_) => AppErrorKind::Conflict,
        RtError::PlanChanged => AppErrorKind::Rejected,
        RtError::ShuttingDown => AppErrorKind::Unavailable,
        RtError::Io(_) => AppErrorKind::Internal,
    };
    AppFailure::new(kind, e.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把广播收到的一条应用事件(或错误)转成推给订阅连接的一行应答。
/// - `Ok(event)` → `Changed{app}`(只带 app id,**不带状态**——GUI 收到即重拉 `List`)。
/// - `Lagged(_)` → `Resync`(慢消费者漏了事件,让它整体重拉;不能静默漏掉)。
/// - `Closed` → `None`(发送端没了:订阅结束)。
pub fn change_reply(
    event: Result<crate::event::AppEvent, tokio::sync::broadcast::error::RecvError>,
) -> Option<AppReply> {
    match event {
        Ok(event) => Some(AppReply::Changed {
            app: event.app().clone(),
        }),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some(AppReply::Resync),
        Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
    }
}

/// 把应用事件写进日志(事件里没有令牌:`EndpointChanged` 带的是不含令牌的站点地址)。
fn spawn_event_logger(manager: &Arc<AppManager>) {
    let mut rx = manager.events();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    tracing::info!(target: "bytehost::service", app = %event.app(), event = ?event, "应用事件")
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::AppId;
    use crate::plan::{Approval, Provenance, TrustLevel};
    use crate::proto::{AppRequest, AppSource};
    use crate::registry::UninstallMode;
    use std::io::{Read, Write};

    fn write_app(dir: &Path, id: &str, body: &str) -> AppSource {
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(
            dir.join("manifest.toml"),
            format!(
                r#"schema_version = 1
min_host_version = "0.1.0"
id = "{id}"
name = "{id} app"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
"#
            ),
        )
        .unwrap();
        std::fs::write(dir.join("web/index.html"), body).unwrap();
        AppSource::LocalDir {
            path: dir.to_path_buf(),
        }
    }

    async fn install(svc: &AppService, source: AppSource) {
        let reply = svc
            .handle(AppRequest::Plan {
                source: source.clone(),
                provenance: Provenance::Local,
                trust: TrustLevel::Trusted,
            })
            .await
            .unwrap();
        let AppReply::Plan { plan } = reply else {
            panic!("{reply:?}")
        };
        let approved = plan.approve(Approval {
            approver: "test".into(),
            approved_ms: 1,
        });
        let done = svc
            .handle(AppRequest::Install {
                approved: Box::new(approved),
                source,
            })
            .await
            .unwrap();
        assert_eq!(done, AppReply::Done);
    }

    fn id(s: &str) -> AppId {
        AppId::new(s).unwrap()
    }
    /// 带着从 `launch_url` 里取出的令牌访问应用站点,返回(状态码, 正文)。
    fn fetch(launch_url: &str, id: &str) -> (u16, String) {
        let rest = launch_url.strip_prefix("http://").unwrap();
        let (authority, path_and_query) = rest.split_once('/').unwrap();
        let port: u16 = authority.rsplit_once(':').unwrap().1.parse().unwrap();
        let token = path_and_query.split("bh_token=").nth(1).unwrap();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(s, "GET / HTTP/1.1\r\nHost: {id}.localhost:{port}\r\nCookie: bh_session={token}\r\nConnection: close\r\n\r\n").unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status = raw.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
        )
    }

    fn running_url(reply: AppReply) -> String {
        match reply {
            AppReply::LaunchUrl { url } => url,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn change_reply_maps_events_to_invalidation_signals() {
        use crate::event::AppEvent;
        use tokio::sync::broadcast::error::RecvError;

        assert_eq!(
            change_reply(Ok(AppEvent::StateChanged {
                app: id("excalidraw"),
                state: crate::state::ObservedState::Running,
            })),
            Some(AppReply::Changed {
                app: id("excalidraw")
            })
        );
        assert_eq!(
            change_reply(Err(RecvError::Lagged(3))),
            Some(AppReply::Resync)
        );
        assert_eq!(change_reply(Err(RecvError::Closed)), None);
    }

    /// 广播落后(`Lagged`)必须整体重拉,不能静默漏掉——把广播灌满后消费者应收到 `Resync`。
    #[tokio::test]
    async fn a_lagging_consumer_gets_a_resync_instead_of_silently_missing_events() {
        use crate::event::AppEvent;

        let (tx, mut rx) = tokio::sync::broadcast::channel::<AppEvent>(2);
        for _ in 0..5 {
            let _ = tx.send(AppEvent::Installed { app: id("a") });
        }
        let mut saw_resync = false;
        for _ in 0..5 {
            match change_reply(rx.recv().await) {
                Some(AppReply::Resync) => {
                    saw_resync = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(saw_resync, "慢消费者必须收到 Resync");
    }

    #[tokio::test]
    async fn subscribe_is_none_when_the_service_is_unavailable() {
        assert!(AppService::unavailable("x").subscribe().is_none());
    }

    #[tokio::test]
    async fn an_unavailable_service_answers_every_request_with_its_reason() {
        let svc = AppService::unavailable("原因 X");
        for req in [
            AppRequest::List,
            AppRequest::ProbeRuntimes,
            AppRequest::Stop { id: id("a") },
            AppRequest::Rollback { id: id("a") },
        ] {
            let failure = svc.handle(req).await.unwrap_err();
            assert_eq!(failure.message, "原因 X");
            assert_eq!(failure.kind, AppErrorKind::Unavailable);
        }
        svc.shutdown().await; // 不可用的服务关停也是空操作
    }

    /// 端口被占用不能拖垮 dozerd:服务变成"不可用",并把原因(含端口)告诉请求方。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_taken_gateway_port_makes_the_service_unavailable_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let squatter = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let taken = squatter.port();
        let svc = AppService::start_with(tmp.path(), GatewayConfig { port: taken }).await;
        let err = svc.handle(AppRequest::List).await.unwrap_err().message;
        assert!(
            err.contains(&taken.to_string()) && err.contains("占用"),
            "{err}"
        );
        squatter.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unusable_application_directory_makes_the_service_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let svc =
            AppService::start_with(&blocker.join("bytehost"), GatewayConfig { port: 0 }).await;
        let err = svc.handle(AppRequest::List).await.unwrap_err().message;
        assert!(err.contains("应用目录"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launch_urls_are_only_given_for_running_apps() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;
        install(&svc, write_app(&tmp.path().join("src/a"), "alpha", "A")).await;
        // 已安装但没运行
        assert!(
            svc.handle(AppRequest::LaunchUrl { id: id("alpha") })
                .await
                .is_err()
        );
        // 没安装
        assert!(
            svc.handle(AppRequest::LaunchUrl { id: id("ghost") })
                .await
                .is_err()
        );
        svc.handle(AppRequest::Start { id: id("alpha") })
            .await
            .unwrap();
        let url = running_url(
            svc.handle(AppRequest::LaunchUrl { id: id("alpha") })
                .await
                .unwrap(),
        );
        assert!(
            url.starts_with("http://alpha.localhost:") && url.contains("bh_token="),
            "{url}"
        );
        assert_eq!(fetch(&url, "alpha"), (200, "A".to_string()));
        svc.shutdown().await;
    }

    /// 应用跟随 dozerd:dozerd 停止则应用停止(站点撤下),但用户想要运行的应用在下次启动时自动恢复。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apps_wanted_running_come_back_after_a_dozerd_restart_and_stopped_ones_stay_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let first = AppService::start_with(&root, GatewayConfig { port: 0 }).await;
        install(
            &first,
            write_app(&tmp.path().join("src/run"), "runner", "R"),
        )
        .await;
        install(
            &first,
            write_app(&tmp.path().join("src/idle"), "idler", "I"),
        )
        .await;
        first
            .handle(AppRequest::Start { id: id("runner") })
            .await
            .unwrap();
        first
            .handle(AppRequest::Start { id: id("idler") })
            .await
            .unwrap();
        first
            .handle(AppRequest::Stop { id: id("idler") })
            .await
            .unwrap();
        let old_url = running_url(
            first
                .handle(AppRequest::LaunchUrl { id: id("runner") })
                .await
                .unwrap(),
        );
        assert_eq!(fetch(&old_url, "runner").0, 200);

        first.shutdown().await;
        assert!(first.gateway_stopped(), "dozerd 停了,gateway 也停了");

        let second = AppService::start_with(&root, GatewayConfig { port: 0 }).await;
        let AppReply::Apps { apps } = second.handle(AppRequest::List).await.unwrap() else {
            panic!()
        };
        let runner = apps.iter().find(|a| a.id == id("runner")).unwrap();
        let idler = apps.iter().find(|a| a.id == id("idler")).unwrap();
        assert!(
            runner.url.is_some(),
            "desired=Running 的应用被自动恢复: {runner:?}"
        );
        assert!(idler.url.is_none(), "desired=Stopped 的保持停止: {idler:?}");
        let url = running_url(
            second
                .handle(AppRequest::LaunchUrl { id: id("runner") })
                .await
                .unwrap(),
        );
        assert_eq!(fetch(&url, "runner"), (200, "R".to_string()));
        second.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uninstalling_removes_the_app_from_the_list_and_probes_name_the_three_runtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;
        install(&svc, write_app(&tmp.path().join("src/a"), "alpha", "A")).await;
        svc.handle(AppRequest::Uninstall {
            id: id("alpha"),
            mode: UninstallMode::ProgramAndData,
        })
        .await
        .unwrap();
        let AppReply::Apps { apps } = svc.handle(AppRequest::List).await.unwrap() else {
            panic!()
        };
        assert!(apps.is_empty());
        let AppReply::Runtimes { runtimes } = svc.handle(AppRequest::ProbeRuntimes).await.unwrap()
        else {
            panic!()
        };
        let names: Vec<_> = runtimes.iter().map(|r| r.runtime.as_str()).collect();
        assert_eq!(names, ["docker", "node", "python"]);
        svc.shutdown().await;
    }

    /// 端口文件坏了:服务不可用并说明原因,**坏文件原样保留**(悄悄重选一个端口会让所有应用丢本地数据)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_corrupt_gateway_port_file_makes_the_service_unavailable_and_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("gateway.json"), "{not json").unwrap();
        let svc = AppService::start(&root).await;
        let err = svc.handle(AppRequest::List).await.unwrap_err().message;
        assert!(
            err.contains("应用宿主不可用") && err.contains("gateway.json"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("gateway.json")).unwrap(),
            "{not json"
        );
    }

    /// `start` 用持久化的端口:停了再起,端口不变(origin 含端口,端口变了所有应用的本地存储都会丢)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_uses_the_persisted_port_across_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        std::fs::create_dir_all(&root).unwrap();
        let free = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let free = if free < 1024 { 20000 } else { free };
        std::fs::write(root.join("gateway.json"), format!("{{\"port\": {free}}}")).unwrap();

        install_and_run_on(&root, tmp.path(), free).await;
        install_and_run_on(&root, tmp.path(), free).await;
    }

    async fn install_and_run_on(root: &Path, scratch: &Path, expected_port: u16) {
        let svc = AppService::start(root).await;
        let AppReply::Apps { .. } = svc.handle(AppRequest::List).await.unwrap() else {
            panic!()
        };
        install_if_missing(&svc, scratch).await;
        let url = match svc
            .handle(AppRequest::Start { id: id("alpha") })
            .await
            .unwrap()
        {
            AppReply::Started { url } => url,
            other => panic!("{other:?}"),
        };
        assert!(url.contains(&format!(":{expected_port}/")), "{url}");
        svc.shutdown().await;
    }

    async fn install_if_missing(svc: &AppService, scratch: &Path) {
        let AppReply::Apps { apps } = svc.handle(AppRequest::List).await.unwrap() else {
            panic!()
        };
        if apps.is_empty() {
            install(svc, write_app(&scratch.join("src/a"), "alpha", "A")).await;
        }
    }

    /// dozerd 停止之后到达的请求不能再启动/安装任何东西:否则会给调用方一个"成功"的假应答,
    /// 并在一个已停止的 gateway 上注册站点(将来进程型 runtime 还会因此留下没人管的子进程)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_arriving_after_shutdown_do_not_start_or_install_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;
        let a = write_app(&tmp.path().join("src/a"), "alpha", "A");
        install(&svc, a).await;
        svc.shutdown().await;

        let err = svc
            .handle(AppRequest::Start { id: id("alpha") })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("停止") && err.kind == AppErrorKind::Unavailable,
            "{err}"
        );
        let b = write_app(&tmp.path().join("src/b"), "beta", "B");
        let plan = svc
            .handle(AppRequest::Plan {
                source: b.clone(),
                provenance: Provenance::Local,
                trust: TrustLevel::Trusted,
            })
            .await
            .unwrap();
        let AppReply::Plan { plan } = plan else {
            panic!()
        };
        let approved = plan.approve(Approval {
            approver: "t".into(),
            approved_ms: 1,
        });
        let err = svc
            .handle(AppRequest::Install {
                approved: Box::new(approved),
                source: b,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("停止") && err.kind == AppErrorKind::Unavailable,
            "{err}"
        );
        let AppReply::Apps { apps } = svc.handle(AppRequest::List).await.unwrap() else {
            panic!()
        };
        assert_eq!(apps.len(), 1, "beta 没有被安装");
        assert!(apps[0].url.is_none(), "alpha 没有被再次启动");
        assert!(svc.gateway_stopped());
    }

    /// 首次运行(还没有 `gateway.json`):经 `AppService::start` 选端口、持久化、真正绑定并能提供服务。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_run_picks_persists_and_binds_a_port() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        assert!(!root.join("gateway.json").exists());
        let svc = AppService::start(&root).await;
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("gateway.json")).unwrap())
                .unwrap();
        let port = persisted["port"].as_u64().unwrap();
        assert!((20000..=32767).contains(&port), "{port}");
        install(&svc, write_app(&tmp.path().join("src/a"), "alpha", "A")).await;
        let AppReply::Started { url } = svc
            .handle(AppRequest::Start { id: id("alpha") })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(url.contains(&format!(":{port}/")), "{url}");
        svc.shutdown().await;
    }
    /// 一个已被占住的端口(持有 listener 直到测试结束)。
    fn squat() -> (std::net::TcpListener, u16) {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    fn free_port() -> u16 {
        squat().1
    }

    fn persisted_port(root: &Path) -> Option<u16> {
        crate::port::load_port(root).unwrap()
    }

    /// 首次运行选中的端口恰好被占:换一个再试,**只记住真正绑上的那个**。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_run_retries_a_taken_port_and_persists_only_the_one_it_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (_held, taken) = squat();
        let good = free_port();
        let mut picks = vec![taken, good].into_iter();
        let svc = AppService::first_run(&root, move || picks.next().unwrap()).await;
        assert!(svc.handle(AppRequest::List).await.is_ok(), "服务可用");
        assert_eq!(persisted_port(&root), Some(good));
        svc.shutdown().await;
    }

    /// 连续都被占:服务不可用,并且**什么都没写盘**(下次启动还能重新选)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_run_gives_up_after_repeated_collisions_without_persisting_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (_held, taken) = squat();
        let svc = AppService::first_run(&root, move || taken).await;
        let failure = svc.handle(AppRequest::List).await.unwrap_err();
        assert_eq!(failure.kind, AppErrorKind::Unavailable);
        assert!(failure.message.contains("占用"), "{failure}");
        assert_eq!(persisted_port(&root), None, "没绑上的端口不被记住");
    }

    /// 端口写不进磁盘:不能带着一个下次会变的端口继续跑——服务不可用,gateway 被撤下。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_run_is_unavailable_when_the_port_cannot_be_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let port = free_port();
        let svc = AppService::first_run(&blocker.join("bytehost"), move || port).await;
        let failure = svc.handle(AppRequest::List).await.unwrap_err();
        assert_eq!(failure.kind, AppErrorKind::Unavailable);
        assert!(failure.message.contains("无法保存"), "{failure}");
        assert!(svc.gateway_stopped());
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "gateway 已释放端口"
        );
    }

    /// 失败带类别:相对路径的来源是 `Rejected`,没装的应用是 `NotFound`。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failures_are_classified_not_just_described() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;
        let plan = AppRequest::Plan {
            source: AppSource::LocalDir {
                path: "relative/dir".into(),
            },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        };
        assert_eq!(
            svc.handle(plan).await.unwrap_err().kind,
            AppErrorKind::Rejected
        );
        assert_eq!(
            svc.handle(AppRequest::Stop { id: id("ghost") })
                .await
                .unwrap_err()
                .kind,
            AppErrorKind::NotFound
        );
        // 读没装的应用的日志也是 NotFound(日志只在装了之后才存在)。
        assert_eq!(
            svc.handle(AppRequest::Logs {
                id: id("ghost"),
                max_lines: 100,
            })
            .await
            .unwrap_err()
            .kind,
            AppErrorKind::NotFound
        );
        // 回滚一个没装的应用 / 没有上一版的应用都是 NotFound。
        assert_eq!(
            svc.handle(AppRequest::Rollback { id: id("ghost") })
                .await
                .unwrap_err()
                .kind,
            AppErrorKind::NotFound
        );
        svc.shutdown().await;
    }

    /// 手动回滚经线上协议走通:装两版 → `Rollback` → `List` 反映回滚后的版本与记录。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rollback_request_restores_the_previous_version_over_the_wire() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;
        install(&svc, write_app(&tmp.path().join("src/v1"), "site", "V1")).await;
        // 第二版:同 id、版本提到 1.1.0。
        let v2 = tmp.path().join("src/v2");
        write_app(&v2, "site", "V2");
        let manifest = std::fs::read_to_string(v2.join("manifest.toml"))
            .unwrap()
            .replace("version = \"1.0.0\"", "version = \"1.1.0\"");
        std::fs::write(v2.join("manifest.toml"), manifest).unwrap();
        install(&svc, AppSource::LocalDir { path: v2.clone() }).await;

        assert_eq!(
            svc.handle(AppRequest::Rollback { id: id("site") })
                .await
                .unwrap(),
            AppReply::Done
        );
        let AppReply::Apps { apps } = svc.handle(AppRequest::List).await.unwrap() else {
            panic!("expected apps")
        };
        let row = apps.into_iter().find(|r| r.id == id("site")).unwrap();
        assert_eq!(row.version, crate::id::Version::new(1, 0, 0));
        assert_eq!(row.previous_version, None);
        let note = row.rollback_note.unwrap();
        assert!(!note.automatic);
        // 再回滚:上一版已消耗。
        assert_eq!(
            svc.handle(AppRequest::Rollback { id: id("site") })
                .await
                .unwrap_err()
                .kind,
            AppErrorKind::NotFound
        );
        svc.shutdown().await;
    }

    // ===== A6c Task 6:进程型应用经 dozerd 线上协议 =====

    #[cfg(unix)]
    fn have_python3() -> bool {
        ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"]
            .iter()
            .any(|d| Path::new(d).join("python3").is_file())
    }

    #[cfg(unix)]
    fn write_py_app(dir: &Path, id: &str) -> AppSource {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("manifest.toml"),
            format!(
                r#"schema_version = 1
min_host_version = "0.1.0"
id = "{id}"
name = "{id} app"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "python"
command = ["python3", "server.py"]

[runtime.http]
port_env = "APP_PORT"
"#
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("server.py"),
            "import os, http.server\n\
             print(\"listening on\", os.environ[\"APP_PORT\"], flush=True)\n\
             http.server.test(HandlerClass=http.server.SimpleHTTPRequestHandler, port=int(os.environ[\"APP_PORT\"]), bind=\"127.0.0.1\")\n",
        )
        .unwrap();
        AppSource::LocalDir {
            path: dir.to_path_buf(),
        }
    }

    /// 轮询 `List` 直到该应用满足 `pred`(最多 `secs` 秒)。
    #[cfg(unix)]
    async fn wait_app<F: Fn(&crate::proto::AppSummary) -> bool>(
        svc: &AppService,
        app: &AppId,
        secs: u64,
        pred: F,
    ) -> crate::proto::AppSummary {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let AppReply::Apps { apps } = svc.handle(AppRequest::List).await.unwrap() else {
                panic!("List 应回 Apps")
            };
            if let Some(a) = apps.into_iter().find(|a| &a.id == app)
                && pred(&a)
            {
                return a;
            }
            if std::time::Instant::now() >= deadline {
                panic!("等待 {app} 超时");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_python_app_runs_through_the_wire_and_dies_with_dozerd() {
        use crate::state::{DesiredState, ObservedState};
        if !have_python3() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let svc = AppService::start_with(&root, GatewayConfig { port: 0 }).await;
        install(&svc, write_py_app(&tmp.path().join("src/app"), "pyapp")).await;
        svc.handle(AppRequest::Start { id: id("pyapp") })
            .await
            .unwrap();
        let running = wait_app(&svc, &id("pyapp"), 15, |a| {
            matches!(a.observed, ObservedState::Running)
        })
        .await;
        assert!(running.url.is_some(), "{running:?}");
        let url = running_url(
            svc.handle(AppRequest::LaunchUrl { id: id("pyapp") })
                .await
                .unwrap(),
        );
        let (status, _) = fetch(&url, "pyapp");
        assert_eq!(status, 200);

        // 日志经线上协议读回,含应用启动时打印的一行(有界、已清洗)。
        let AppReply::Logs { text, .. } = svc
            .handle(AppRequest::Logs {
                id: id("pyapp"),
                max_lines: 100,
            })
            .await
            .unwrap()
        else {
            panic!("期望 Logs 应答")
        };
        assert!(text.contains("listening on"), "{text:?}");
        let upstream_port: u16 = url
            .strip_prefix("http://")
            .unwrap()
            .split(':')
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        svc.shutdown().await;

        // 应用跟随 dozerd:进程已被收掉,端口不再应答
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", upstream_port)).is_err(),
            "dozerd 停了,应用进程也该没了"
        );

        // 同根目录重建:desired 仍是 Running(应用随上次 dozerd 停止,但想要的还在);reconcile 会把它重新拉起
        let again = AppService::start_with(&root, GatewayConfig { port: 0 }).await;
        let rec = wait_app(&again, &id("pyapp"), 15, |a| {
            matches!(a.observed, ObservedState::Running)
        })
        .await;
        assert_eq!(rec.desired, DesiredState::Running);
        again.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dozerd_restart_brings_a_python_app_back() {
        if !have_python3() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let first = AppService::start_with(&root, GatewayConfig { port: 0 }).await;
        install(&first, write_py_app(&tmp.path().join("src/app"), "pyapp")).await;
        first
            .handle(AppRequest::Start { id: id("pyapp") })
            .await
            .unwrap();
        wait_app(&first, &id("pyapp"), 15, |a| {
            matches!(a.observed, crate::state::ObservedState::Running)
        })
        .await;
        first.shutdown().await;

        let second = AppService::start_with(&root, GatewayConfig { port: 0 }).await;
        wait_app(&second, &id("pyapp"), 15, |a| {
            matches!(a.observed, crate::state::ObservedState::Running)
        })
        .await;
        let url = running_url(
            second
                .handle(AppRequest::LaunchUrl { id: id("pyapp") })
                .await
                .unwrap(),
        );
        assert_eq!(fetch(&url, "pyapp").0, 200);
        second.shutdown().await;
    }

    // ===== A6d Task 4:运行时安装经 dozerd 线上协议 =====
    use crate::digest::sha256_hex;
    use crate::proto::RuntimeDownload;
    use crate::runtime::managed::{Fetcher, UvRunner};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// 造一个顶层 `bin/tool`(strip 0)的 `.tar.gz`。
    fn tool_tar(dir: &Path, script: &str) -> (std::path::PathBuf, String) {
        let src = dir.join(format!("tsrc-{}", std::process::id()));
        std::fs::create_dir_all(src.join("bin")).unwrap();
        let tool = src.join("bin/tool");
        std::fs::write(&tool, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = dir.join(format!("t-{}.tar.gz", std::process::id()));
        let status = std::process::Command::new("/usr/bin/tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&archive).unwrap();
        (archive, sha256_hex(&bytes))
    }

    /// 顶层就是 `uv`(strip 0)。
    fn uv_tar(dir: &Path) -> (std::path::PathBuf, String) {
        let src = dir.join(format!("usrc-{}", std::process::id()));
        std::fs::create_dir_all(&src).unwrap();
        let uv = src.join("uv");
        std::fs::write(&uv, "#!/bin/sh\necho uv\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = dir.join(format!("u-{}.tar.gz", std::process::id()));
        let status = std::process::Command::new("/usr/bin/tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("uv")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&archive).unwrap();
        (archive, sha256_hex(&bytes))
    }

    struct FakeFetcher {
        source: std::path::PathBuf,
        calls: AtomicUsize,
        block: bool,
    }

    impl Fetcher for FakeFetcher {
        fn fetch(
            &self,
            _url: &str,
            dest: &Path,
            on_progress: &mut dyn FnMut(u64, Option<u64>),
            cancel: &AtomicBool,
        ) -> std::io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block {
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "取消"));
            }
            let bytes = std::fs::read(&self.source)?;
            std::fs::write(dest, &bytes)?;
            on_progress(bytes.len() as u64, Some(bytes.len() as u64));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeUv {
        calls: Mutex<usize>,
    }

    impl UvRunner for FakeUv {
        fn python_install(
            &self,
            _uv: &Path,
            version: &str,
            install_dir: &Path,
            _cache_dir: &Path,
            _cancel: &AtomicBool,
        ) -> std::io::Result<()> {
            *self.calls.lock().unwrap() += 1;
            let d = install_dir.join(format!("cpython-{version}.0-macos-aarch64-none"));
            std::fs::create_dir_all(d.join("bin"))?;
            let py = d.join("bin/python3");
            std::fs::write(&py, "#!/bin/sh\n")?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&py, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        }
    }

    /// 用测试专用 pin 表(受管版本固定为 24.21.0),假 fetcher/uv,启动服务。
    async fn start_with_fake_runtime(
        root: &Path,
        fetcher: Arc<dyn Fetcher>,
        pins: &'static [crate::runtime::managed::Pin],
    ) -> Arc<AppService> {
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let rm = Arc::new(RuntimeManager::with_parts(
            root.join("runtimes"),
            fetcher,
            Arc::new(crate::runtime::managed::TarArchive),
            Some(crate::runtime::managed::Target::Aarch64Apple),
            Arc::new(FakeUv::default()),
            pins,
        ));
        AppService::finish_start_with(root, gateway, rm, None).await
    }

    fn node_pins(url: &str, sha: String) -> &'static [crate::runtime::managed::Pin] {
        use crate::runtime::managed::{Pin, Target};
        Box::leak(
            vec![Pin {
                name: "node",
                version: "24.21.0",
                target: Target::Aarch64Apple,
                url: leak(url.to_string()),
                sha256: leak(sha),
                strip_components: 0,
                bin_rel: "bin/tool",
            }]
            .into_boxed_slice(),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runtime_install_goes_plan_approve_progress_installed_and_probes_show_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (tar, sha) = tool_tar(tmp.path(), "#!/bin/sh\necho v1\n");
        let pins = node_pins("https://nodejs.org/x.tar.gz", sha);
        let svc = start_with_fake_runtime(
            &root,
            Arc::new(FakeFetcher {
                source: tar,
                calls: AtomicUsize::new(0),
                block: false,
            }),
            pins,
        )
        .await;

        let AppReply::RuntimePlan { plan } = svc
            .handle(AppRequest::RuntimePlan {
                runtime: ManagedRuntime::Node,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        svc.handle(AppRequest::InstallRuntime { plan })
            .await
            .unwrap();

        // 轮询探测直到任务 finished 且版本出现。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let AppReply::Runtimes { runtimes } =
                svc.handle(AppRequest::ProbeRuntimes).await.unwrap()
            else {
                panic!()
            };
            let node = runtimes.iter().find(|r| r.runtime == "node").unwrap();
            if node.job.as_ref().map(|j| j.finished).unwrap_or(false)
                && node.managed.contains(&"24.21.0".to_string())
            {
                assert!(node.installable);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "安装未在 10s 内完成: {node:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        svc.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_runtime_plan_is_rejected_without_downloading() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (tar, sha) = tool_tar(tmp.path(), "#!/bin/sh\necho v1\n");
        let pins = node_pins("https://nodejs.org/x.tar.gz", sha);
        let fetcher = Arc::new(FakeFetcher {
            source: tar,
            calls: AtomicUsize::new(0),
            block: false,
        });
        let svc = start_with_fake_runtime(&root, fetcher.clone(), pins).await;

        let AppReply::RuntimePlan { plan } = svc
            .handle(AppRequest::RuntimePlan {
                runtime: ManagedRuntime::Node,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let mut forged = *plan;
        forged.downloads[0] = RuntimeDownload {
            url: "https://evil/x.tar.gz".into(),
            ..forged.downloads[0].clone()
        };
        let err = svc
            .handle(AppRequest::InstallRuntime {
                plan: Box::new(forged),
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind, AppErrorKind::Rejected);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0, "不得发起下载");
        svc.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_a_running_download() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (tar, sha) = tool_tar(tmp.path(), "#!/bin/sh\necho v1\n");
        let pins = node_pins("https://nodejs.org/x.tar.gz", sha);
        let svc = start_with_fake_runtime(
            &root,
            Arc::new(FakeFetcher {
                source: tar,
                calls: AtomicUsize::new(0),
                block: true,
            }),
            pins,
        )
        .await;
        let AppReply::RuntimePlan { plan } = svc
            .handle(AppRequest::RuntimePlan {
                runtime: ManagedRuntime::Node,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        svc.handle(AppRequest::InstallRuntime { plan })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let start = std::time::Instant::now();
        svc.shutdown().await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "取消不应拖住退出"
        );
        let leftovers: Vec<_> = std::fs::read_dir(root.join("runtimes"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_process_app_uses_the_managed_runtime_before_the_system_one() {
        if !have_python3() {
            return;
        }
        use crate::runtime::managed::Pin;
        use crate::runtime::managed::Target;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        // 受管的 python3 包装脚本:内部转调系统 python3,并留下"我是受管版本"的标记。
        let marker = tmp.path().join("managed-marker");
        let wrapper = format!(
            "#!/bin/sh\ntouch {}\nexec /usr/bin/python3 \"$@\"\n",
            marker.display()
        );
        // 这里直接手工把受管 python 摆进 store(模拟 uv python install 完成);pin 表只需让
        // RuntimeManager 构造得起来,python 应用不会触发任何下载。
        let pins: &'static [Pin] = Box::leak(
            vec![Pin {
                name: "uv",
                version: "0.12.23",
                target: Target::Aarch64Apple,
                url: leak("https://github.com/uv.tar.gz".into()),
                sha256: leak("0".repeat(64)),
                strip_components: 0,
                bin_rel: "uv",
            }]
            .into_boxed_slice(),
        );
        let (tar, _sha) = uv_tar(tmp.path());
        let svc = start_with_fake_runtime(
            &root,
            Arc::new(FakeFetcher {
                source: tar,
                calls: AtomicUsize::new(0),
                block: false,
            }),
            pins,
        )
        .await;
        let pydir = root.join("runtimes/python/cpython-3.13.0-macos-aarch64-none/bin");
        std::fs::create_dir_all(&pydir).unwrap();
        let py = pydir.join("python3");
        std::fs::write(&py, &wrapper).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&py, std::fs::Permissions::from_mode(0o755)).unwrap();

        install(&svc, write_py_app(&tmp.path().join("src/app"), "pyapp")).await;
        svc.handle(AppRequest::Start { id: id("pyapp") })
            .await
            .unwrap();
        let running = wait_app(&svc, &id("pyapp"), 15, |a| {
            matches!(a.observed, crate::state::ObservedState::Running)
        })
        .await;
        assert!(running.url.is_some(), "{running:?}");
        assert!(marker.exists(), "受管 python 被使用(标记文件应存在)");
        svc.shutdown().await;
    }

    // ===== A6h Task 4:压缩包 / URL 来源经 dozerd 线上协议 =====

    /// 在 `dir` 下造一个静态应用的顶层目录,打成 `.tar.gz`(与 `tool_tar` 同一手法,不新增依赖)。
    fn static_app_targz(dir: &Path, id: &str, body: &str) -> std::path::PathBuf {
        let src = dir.join(format!("{id}-src-{}", std::process::id()));
        std::fs::create_dir_all(src.join("web")).unwrap();
        std::fs::write(
            src.join("manifest.toml"),
            format!(
                r#"schema_version = 1
min_host_version = "0.1.0"
id = "{id}"
name = "{id} app"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
"#
            ),
        )
        .unwrap();
        std::fs::write(src.join("web/index.html"), body).unwrap();
        let archive = dir.join(format!("{id}-{}.tar.gz", std::process::id()));
        let status = std::process::Command::new("/usr/bin/tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .args(["manifest.toml", "web"])
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    /// 从内存字节"下载"到 `dest` 的假来源下载器;`effective_url` 可配。
    struct SourceFetcher {
        bytes: Vec<u8>,
        effective_url: String,
        calls: AtomicUsize,
    }

    impl SourceFetcher {
        fn new(bytes: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                bytes,
                effective_url: "https://example.com/app.tar.gz".into(),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl Fetcher for SourceFetcher {
        fn fetch(
            &self,
            url: &str,
            dest: &Path,
            on_progress: &mut dyn FnMut(u64, Option<u64>),
            cancel: &AtomicBool,
        ) -> std::io::Result<()> {
            self.fetch_meta(url, dest, 0, on_progress, cancel)
                .map(|_| ())
        }

        fn fetch_meta(
            &self,
            _url: &str,
            dest: &Path,
            _max_bytes: u64,
            _on_progress: &mut dyn FnMut(u64, Option<u64>),
            _cancel: &AtomicBool,
        ) -> std::io::Result<crate::runtime::managed::fetch::FetchMeta> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(dest, &self.bytes)?;
            Ok(crate::runtime::managed::fetch::FetchMeta {
                effective_url: self.effective_url.clone(),
                bytes: self.bytes.len() as u64,
            })
        }
    }

    /// 造一个只跑 AppManager、可注入来源下载器的服务(不复制启动流程)。
    async fn start_with_source_fetcher(root: &Path, fetcher: Arc<dyn Fetcher>) -> Arc<AppService> {
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let rm = Arc::new(RuntimeManager::new(root.join("runtimes")));
        AppService::finish_start_with_sources(root, gateway, rm, None, Some(fetcher)).await
    }

    /// 各新错误类别的线上映射:`SourceNotAllowed`/`Archive`/`Source` 的输入类问题都是 `Rejected`。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_and_url_source_errors_are_classified_on_the_wire() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;

        // 相对路径的压缩包 → Rejected。
        let rel = AppRequest::Plan {
            source: AppSource::Archive {
                path: "relative/app.zip".into(),
            },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        };
        assert_eq!(
            svc.handle(rel).await.unwrap_err().kind,
            AppErrorKind::Rejected
        );

        // 不认识扩展名的压缩包 → Rejected(Source 类)。
        let bogus = tmp.path().join("app.rar");
        std::fs::write(&bogus, b"x").unwrap();
        let unknown = AppRequest::Plan {
            source: AppSource::Archive { path: bogus },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        };
        assert_eq!(
            svc.handle(unknown).await.unwrap_err().kind,
            AppErrorKind::Rejected
        );

        // 坏的 https URL(格式错)→ Rejected,且未发起下载。
        let badurl = AppRequest::Plan {
            source: AppSource::Url {
                url: "http://example.com/a.zip".into(),
                sha256: None,
            },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        };
        assert_eq!(
            svc.handle(badurl).await.unwrap_err().kind,
            AppErrorKind::Rejected
        );
        svc.shutdown().await;
    }

    /// `Plan{Archive}` 经 `handle` 得到带 `source_info` 的计划(本机压缩包 = `Local/Trusted`)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_archive_source_plans_over_the_wire_with_source_info() {
        let tmp = tempfile::tempdir().unwrap();
        let svc =
            AppService::start_with(&tmp.path().join("bytehost"), GatewayConfig { port: 0 }).await;
        let archive = static_app_targz(tmp.path(), "alpha", "A");
        let reply = svc
            .handle(AppRequest::Plan {
                source: AppSource::Archive { path: archive },
                provenance: Provenance::Local,
                trust: TrustLevel::Trusted,
            })
            .await
            .unwrap();
        let AppReply::Plan { plan } = reply else {
            panic!("{reply:?}")
        };
        assert_eq!(plan.source_info.kind, "archive");
        assert!(plan.source_info.archive_sha256.is_some());
        assert_eq!(plan.provenance, Provenance::Local);
        assert_eq!(plan.trust, TrustLevel::Trusted);
        svc.shutdown().await;
    }

    /// `Plan{Url}` + 注入的假下载器:得到 `ThirdParty/Untrusted`;客户端在请求里自报 `Local/Trusted`
    /// 也被服务端覆盖(端到端版的 Review Focus 4)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_url_source_is_untrusted_no_matter_what_the_client_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = static_app_targz(tmp.path(), "alpha", "A");
        let bytes = std::fs::read(&archive).unwrap();
        let fetcher = SourceFetcher::new(bytes);
        let svc = start_with_source_fetcher(&tmp.path().join("bytehost"), fetcher.clone()).await;
        let reply = svc
            .handle(AppRequest::Plan {
                source: AppSource::Url {
                    url: "https://example.com/app.tar.gz".into(),
                    sha256: None,
                },
                // 客户端谎报受信,服务端必须按来源推导。
                provenance: Provenance::Local,
                trust: TrustLevel::Trusted,
            })
            .await
            .unwrap();
        let AppReply::Plan { plan } = reply else {
            panic!("{reply:?}")
        };
        assert_eq!(plan.source_info.kind, "url");
        assert_eq!(plan.provenance, Provenance::ThirdParty);
        assert_eq!(plan.trust, TrustLevel::Untrusted);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "出计划下载一次");
        svc.shutdown().await;
    }

    /// URL 来源的**进程型**应用出计划即被拒(网络来源只允许静态应用),经线上协议验证。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_url_source_of_a_process_app_is_rejected_over_the_wire() {
        let tmp = tempfile::tempdir().unwrap();
        // python 应用的 tar.gz。
        let src = tmp.path().join("pysrc");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("manifest.toml"),
            r#"schema_version = 1
min_host_version = "0.1.0"
id = "pypkg"
name = "Py"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "python"
command = ["python3", "server.py"]

[runtime.http]
port_env = "APP_PORT"
"#,
        )
        .unwrap();
        std::fs::write(src.join("server.py"), "print('hi')\n").unwrap();
        let archive = tmp.path().join("py.tar.gz");
        let status = std::process::Command::new("/usr/bin/tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .args(["manifest.toml", "server.py"])
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&archive).unwrap();
        let fetcher = SourceFetcher::new(bytes);
        let svc = start_with_source_fetcher(&tmp.path().join("bytehost"), fetcher).await;
        let err = svc
            .handle(AppRequest::Plan {
                source: AppSource::Url {
                    url: "https://example.com/app.tar.gz".into(),
                    sha256: None,
                },
                provenance: Provenance::Local,
                trust: TrustLevel::Trusted,
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind, AppErrorKind::Rejected);
        assert!(err.message.contains("静态应用"), "{err}");
        svc.shutdown().await;
    }
}
