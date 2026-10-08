//! `AppManager`:安装、启动、停止、卸载、启动对账——把注册表、gateway 和生命周期纯函数接起来。
//!
//! 一期实现 `static_web` 与进程型(`node`/`python`,A6c);容器在 `install_plan` 就被明确拒绝
//! (`UnsupportedRuntime`)。
//! 所有会改状态的方法都在同一把锁里执行(**单写者**:规格里 supervisor 是唯一写者),
//! 每次状态变化都先落盘(`AppRecord.observed`)再发事件。耗时任务句柄/进度(规格 §4.3)留到
//! 有真正耗时的 runtime 时再加:静态应用的安装是瞬时的。
//!
//! 防 TOCTOU 的安装顺序(见 `digest` 模块文档):**先把包拷进 staging,对 staging 副本重新算摘要并
//! `verify`,再改名落位**;`current_version` 在最后才写。

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::digest::{data_store_id_hex, digest_tree, sha256_hex};
use crate::event::{AppEvent, RuntimeReason};
use crate::gateway::{Gateway, csp_for};
use crate::id::{AppId, Version};
use crate::launch::{check_process_runtime, install_argv, lockfile_of};
use crate::manifest::{Manifest, ManifestError, Runtime};
use crate::plan::{
    ApprovedInstallPlan, InstallPlan, Installed, PlanInput, Provenance, TrustLevel, VerifyError,
};
use crate::registry::{AppRecord, RECORD_FORMAT_VERSION, Registry, UninstallMode, VersionRecord};
use crate::runtime::{
    ResolveError, Resolved, RuntimeResolver, SystemResolver, SystemVersionProbe, VersionProbe,
    enforcement_for,
};
use crate::state::{
    Action, DesiredState, ObservedState, next_action, recover_after_supervisor_restart,
};
use crate::supervisor::{self, Launch, Phase, Transitions};

pub use crate::proto::{AppIssue, AppSource, AppSummary};

#[derive(Debug)]
pub enum ManagerError {
    Io(io::Error),
    Manifest(ManifestError),
    Verify(VerifyError),
    /// 一期只支持 `static_web`。
    UnsupportedRuntime(String),
    /// 这个版本已经装过了(同版本不覆盖)。
    AlreadyInstalled(Version),
    /// 应用正在运行/过渡中,升级前要先停止。
    Busy(AppId),
    NotInstalled(AppId),
    /// 当前状态不允许这个操作。
    BadState {
        app: AppId,
        state: ObservedState,
    },
    /// 应用包里没有 `runtime.source` 指向的目录。
    MissingSource(PathBuf),
    /// supervisor 正在/已经停止(`suspend_all` 之后到下一次 `reconcile` 之前):不再接受会产生新站点或新包的操作。
    ShuttingDown,
    /// 应用来源不合法(如 `LocalDir` 的路径不是绝对路径)。
    BadSource(String),
    /// 进程型应用所需的运行时没装(如没装 `python3`/`node`)。
    RuntimeUnavailable(String),
    /// 运行时装了,但版本不满足清单声明的要求。
    RuntimeVersion {
        runtime: String,
        required: String,
        found: String,
    },
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O 错误: {e}"),
            Self::Manifest(e) => write!(f, "{e}"),
            Self::Verify(e) => write!(f, "安装被拒绝: {e}"),
            Self::UnsupportedRuntime(k) => {
                write!(f, "暂不支持 {k} 类型的应用(一期只支持 static_web)")
            }
            Self::AlreadyInstalled(v) => write!(f, "版本 {v} 已经安装"),
            Self::Busy(id) => write!(f, "应用 {id} 正在运行,请先停止再升级"),
            Self::NotInstalled(id) => write!(f, "应用 {id} 没有安装"),
            Self::BadState { app, state } => {
                write!(f, "应用 {app} 当前状态 {state:?} 不允许这个操作")
            }
            Self::BadSource(why) => write!(f, "应用来源不合法: {why}"),
            Self::ShuttingDown => write!(f, "dozerd 正在停止,暂不接受安装/启动"),
            Self::RuntimeUnavailable(name) => write!(f, "运行时 {name} 不可用:未找到"),
            Self::RuntimeVersion {
                runtime,
                required,
                found,
            } => write!(f, "需要 {runtime} {required},当前 {found}"),
            Self::MissingSource(p) => write!(f, "应用包里缺少站点目录: {}", p.display()),
        }
    }
}

impl ManagerError {
    /// 线上失败类别(GUI 据此呈现,见 [`AppErrorKind`])。
    pub fn kind(&self) -> crate::proto::AppErrorKind {
        use crate::proto::AppErrorKind as K;
        match self {
            Self::Io(_) => K::Internal,
            Self::Manifest(_) | Self::Verify(_) | Self::BadSource(_) | Self::MissingSource(_) => {
                K::Rejected
            }
            Self::UnsupportedRuntime(_) => K::Unsupported,
            Self::AlreadyInstalled(_) | Self::Busy(_) | Self::BadState { .. } => K::Conflict,
            Self::NotInstalled(_) => K::NotFound,
            Self::ShuttingDown => K::Unavailable,
            Self::RuntimeUnavailable(_) => K::Unavailable,
            Self::RuntimeVersion { .. } => K::Unavailable,
        }
    }
}

impl std::error::Error for ManagerError {}

impl From<io::Error> for ManagerError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ManifestError> for ManagerError {
    fn from(e: ManifestError) -> Self {
        Self::Manifest(e)
    }
}

/// `AppManager` 的共享状态:注册表、gateway、事件、单写者锁、关闭标志,以及解析器与在跑的监管线程登记表。
/// 抽出来是为了让监管线程能持有 `Arc<Core>`(Task 5 的 `AppTransitions`),`AppManager` 本身经 `Deref` 直接用它。
///
/// 因为 `AppManager: Deref<Target = Core>`,这个类型必须 `pub`;它的字段仍是私有的,
/// 内部方法(`guard`/`try_guard`/`*_locked` 等)为 `pub(crate)`。
pub struct Core {
    registry: Registry,
    gateway: Arc<Gateway>,
    host_version: Version,
    events: broadcast::Sender<AppEvent>,
    /// 单写者锁:所有改状态的方法都先拿它。
    lock: Mutex<()>,
    /// `suspend_all` 之后置位、`reconcile` 清除:置位期间 `install`/`start` 在**拿到锁之后**被拒绝——
    /// 一个恰好排在锁后面的 `Start` 不能在撤站点之后又把站点注册回已停止的 gateway。
    closed: std::sync::atomic::AtomicBool,
    /// 把解释器名解析成绝对路径(进程型应用)。
    #[allow(dead_code)]
    resolver: Arc<dyn RuntimeResolver>,
    /// 探解释器版本(进程型应用的版本要求校验)。
    version_probe: Arc<dyn VersionProbe>,
    /// `start` 在拿锁**之前**预探的解释器版本输出(`--version` 最多阻塞 3s,不能占着单写者锁)。
    /// 按应用登记 `(解释器路径, 输出)`,启动检查取走即删;`start` 返回时一律清掉,避免陈旧结果被下次启动复用。
    prefetched_versions: Mutex<HashMap<AppId, (PathBuf, Option<String>)>>,
    /// 每个应用当前"看得懂的问题"(运行时缺失/版本不符);只存内存,只在应用 `Failed` 时经 `list` 带出。
    #[allow(dead_code)]
    issues: Mutex<HashMap<AppId, AppIssue>>,
    /// 进程型应用崩溃后的重启策略(默认 `RestartPolicy::default()`;测试里注入小退避)。
    policy: crate::process::restart::RestartPolicy,
    /// 运行中周期健康检查的参数(默认 `DEFAULT_MONITOR_*`;测试里调小以便观察)。
    monitor: MonitorConfig,
    /// 每个应用当前那条监管线程(`cancel` 标志 + 句柄)。静态应用不登记。
    #[allow(dead_code)]
    supervisions: Mutex<HashMap<AppId, Supervision>>,
    /// 指向自己的 `Arc`(经 `Arc::new_cyclic` 装填):监管线程需要一份 `Arc<Core>` 才能回报状态,
    /// 而 `start_process_locked` 只有 `&Core`。用 `Weak` 打破强引用环。
    self_weak: std::sync::OnceLock<std::sync::Weak<Core>>,
    /// (仅测试)`install` 里"拷贝与摘要计算已完成、即将拿锁"的次数,用来证明慢的部分在锁外。
    #[cfg(test)]
    prepared: std::sync::atomic::AtomicUsize,
}

/// 运行中周期健康检查的参数;默认取 `supervisor::DEFAULT_MONITOR_*`。
#[derive(Debug, Clone, Copy)]
pub struct MonitorConfig {
    pub interval: std::time::Duration,
    pub timeout: std::time::Duration,
    pub failures: u32,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: supervisor::DEFAULT_MONITOR_INTERVAL,
            timeout: supervisor::DEFAULT_MONITOR_TIMEOUT,
            failures: supervisor::DEFAULT_MONITOR_FAILURES,
        }
    }
}

/// `version_target` 的结论。
enum VersionTarget {
    /// 不需要检查。
    Skip,
    /// Node 应用但解析不到 `node`。
    NodeMissing,
    /// 探 `program` 的 `--version` 并与 `req` 比对。
    Probe {
        runtime: String,
        req_text: String,
        req: crate::runtime_version::VersionReq,
        program: PathBuf,
    },
}

/// 一条监管线程的取消标志与句柄。
#[allow(dead_code)]
pub(crate) struct Supervision {
    pub(crate) cancel: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
}

/// 监管线程回报状态的适配器:把它接回 `Core` 的单写者锁 + `AppRecord` + gateway。
///
/// **持锁用尝试循环**——`stop`/`suspend_all` 会持着锁 `join` 本线程,线程若阻塞等锁就是死锁。
/// 每次拿到锁后**再查一遍 `cancel`**:取消之后绝不再写任何状态。
struct AppTransitions {
    core: Arc<Core>,
    id: AppId,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl AppTransitions {
    /// 拿单写者锁(尝试循环),拿到后核对取消标志。返回 `None` = 已取消,调用方必须放弃写入。
    fn acquire(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        use std::sync::atomic::Ordering;
        let g = loop {
            if self.cancel.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(g) = self.core.try_guard() {
                break g;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        if self.cancel.load(Ordering::SeqCst) {
            return None;
        }
        Some(g)
    }
}

impl Transitions for AppTransitions {
    fn phase(&self, phase: Phase) -> bool {
        let Some(_g) = self.acquire() else {
            return false;
        };
        let Ok(mut record) = self.core.load_record(&self.id) else {
            return false;
        };
        let observed = match phase {
            Phase::Preparing => ObservedState::Preparing,
            Phase::Starting => ObservedState::Starting,
        };
        self.core.set_observed(&mut record, observed).is_ok()
    }

    fn ready(&self, port: u16) -> bool {
        let Some(_g) = self.acquire() else {
            return false;
        };
        let Ok(mut record) = self.core.load_record(&self.id) else {
            return false;
        };
        let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        self.core.gateway.add_upstream(&self.id, upstream);
        if self
            .core
            .set_observed(&mut record, ObservedState::Running)
            .is_err()
        {
            return false;
        }
        let url = self.core.gateway.site_url(&self.id);
        self.core.emit(AppEvent::EndpointChanged {
            app: self.id.clone(),
            url: Some(url),
        });
        self.core.clear_issue(&self.id);
        true
    }

    fn down(&self, reason: String, restart_in: Option<Duration>) -> bool {
        let Some(_g) = self.acquire() else {
            return false;
        };
        let Ok(mut record) = self.core.load_record(&self.id) else {
            return false;
        };
        let was_serving = self.core.gateway.remove_site(&self.id);
        if was_serving {
            self.core.emit(AppEvent::EndpointChanged {
                app: self.id.clone(),
                url: None,
            });
        }
        let observed = match restart_in {
            Some(_) => ObservedState::Starting,
            None => ObservedState::Failed {
                reason,
                retryable: false,
            },
        };
        self.core.set_observed(&mut record, observed).is_ok()
    }

    fn failed(&self, reason: String, retryable: bool) -> bool {
        let Some(_g) = self.acquire() else {
            return false;
        };
        let Ok(mut record) = self.core.load_record(&self.id) else {
            return false;
        };
        let was_serving = self.core.gateway.remove_site(&self.id);
        if was_serving {
            self.core.emit(AppEvent::EndpointChanged {
                app: self.id.clone(),
                url: None,
            });
        }
        self.core
            .set_observed(&mut record, ObservedState::Failed { reason, retryable })
            .is_ok()
    }

    fn install_failed(&self, summary: String) -> bool {
        let Some(_g) = self.acquire() else {
            return false;
        };
        let Ok(mut record) = self.core.load_record(&self.id) else {
            return false;
        };
        let was_serving = self.core.gateway.remove_site(&self.id);
        if was_serving {
            self.core.emit(AppEvent::EndpointChanged {
                app: self.id.clone(),
                url: None,
            });
        }
        // 先记 issue 再改状态:GUI 因 Changed 重拉时一定看得到问题(推送与 issue 的竞态)。
        self.core.set_issue(
            &self.id,
            AppIssue::DependencyInstall {
                summary: summary.clone(),
            },
        );
        self.core
            .set_observed(
                &mut record,
                ObservedState::Failed {
                    reason: summary,
                    retryable: true,
                },
            )
            .is_ok()
    }
}

pub struct AppManager {
    core: Arc<Core>,
}

impl std::ops::Deref for AppManager {
    type Target = Core;
    fn deref(&self) -> &Core {
        &self.core
    }
}

/// 一个已读入并校验过的应用包。
struct Package {
    manifest: Manifest,
    manifest_digest: String,
    source_digest: String,
}

fn read_package(dir: &Path, host_version: &Version) -> Result<Package, ManagerError> {
    let manifest_text = fs::read_to_string(dir.join("manifest.toml"))?;
    let manifest = Manifest::from_toml(&manifest_text, host_version)?;
    Ok(Package {
        manifest_digest: sha256_hex(manifest_text.as_bytes()),
        source_digest: digest_tree(dir)?,
        manifest,
    })
}

/// "依赖已装好"的标记文件:按 lockfile **内容**的 sha256 命名,换了 lockfile 的新版本必须重新装。
/// `start_process_locked` 与测试共用这一处,避免两边各算一份。
fn deps_marker(cache_dir: &Path, lockfile: Option<&[u8]>) -> PathBuf {
    match lockfile {
        Some(bytes) => cache_dir.join(format!("deps-{}.ok", sha256_hex(bytes))),
        None => cache_dir.join("deps-none.ok"),
    }
}

/// 子进程 `PATH`:各解析结果的目录(去重、保持顺序)+ `/usr/bin:/bin` 兜底。
/// 主命令与依赖安装命令的目录都要在:`npm` 与 `node` 可能不在同一处。
fn child_path(resolved: &[&Resolved]) -> OsString {
    let mut seen: Vec<&Path> = Vec::new();
    let mut path = OsString::new();
    for d in resolved.iter().flat_map(|r| r.path_dirs.iter()) {
        if !seen.contains(&d.as_path()) {
            seen.push(d.as_path());
            path.push(d);
            path.push(":");
        }
    }
    path.push("/usr/bin:/bin");
    path
}

/// 宿主给子进程的额外变量:npm/uv 的缓存指向应用私有的 `cache/`,不写用户主目录。
/// 走 `EnvSpec::extra`(不经父环境白名单)。
fn cache_env(cache_dir: &Path) -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("npm_config_cache"),
            cache_dir.join("npm").into_os_string(),
        ),
        (
            OsString::from("UV_CACHE_DIR"),
            cache_dir.join("uv").into_os_string(),
        ),
    ]
}

/// 校验一个 runtime 是否受支持:`StaticWeb`/`Node`/`Python` 通过(进程型还要过 `check_process_runtime`),
/// `Container` 明确拒绝。返回 `Ok(())` 或可直接映射成 `ManagerError` 的错误。
fn check_supported(runtime: &Runtime, package_dir: &Path) -> Result<(), ManagerError> {
    match runtime {
        Runtime::StaticWeb { .. } => Ok(()),
        Runtime::Node { .. } | Runtime::Python { .. } => {
            check_process_runtime(runtime, package_dir).map_err(ManagerError::BadSource)
        }
        Runtime::Container { .. } => Err(ManagerError::UnsupportedRuntime("container".to_string())),
    }
}

fn static_source(manifest: &Manifest) -> Result<&str, ManagerError> {
    match &manifest.runtime {
        Runtime::StaticWeb { source } => Ok(source),
        other => Err(ManagerError::UnsupportedRuntime(
            other.kind_name().to_string(),
        )),
    }
}

/// 清掉崩溃时留下的 `.staging-*` 目录。manager 启动时调用:单写者,此刻没有别的安装在进行。
fn sweep_staging(apps_dir: &Path) {
    let Ok(entries) = fs::read_dir(apps_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(".staging-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// `LocalDir` 必须是绝对路径:相对路径会按 dozerd 的工作目录解析,调用方并不知道那是哪里。
fn ensure_absolute(path: &Path) -> Result<(), ManagerError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ManagerError::BadSource(format!(
            "路径必须是绝对路径,收到 {}",
            path.display()
        )))
    }
}

/// 递归拷贝目录。只拷普通文件和目录:符号链接/FIFO/设备一律拒绝(与 `digest_tree` 同口径)。
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &to)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("应用包里只允许普通文件和目录: {}", entry.path().display()),
            ));
        }
    }
    Ok(())
}

impl AppManager {
    pub fn new(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
    ) -> io::Result<Self> {
        Self::with_resolver(root, host_version, gateway, Arc::new(SystemResolver::new()))
    }

    pub fn with_resolver(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
    ) -> io::Result<Self> {
        Self::with_resolver_and_policy(
            root,
            host_version,
            gateway,
            resolver,
            crate::process::restart::RestartPolicy::default(),
        )
    }

    fn with_resolver_and_policy(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
        policy: crate::process::restart::RestartPolicy,
    ) -> io::Result<Self> {
        Self::with_parts(
            root,
            host_version,
            gateway,
            resolver,
            policy,
            Arc::new(SystemVersionProbe::new()),
        )
    }

    fn with_parts(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
        policy: crate::process::restart::RestartPolicy,
        version_probe: Arc<dyn VersionProbe>,
    ) -> io::Result<Self> {
        Self::with_parts_full(
            root,
            host_version,
            gateway,
            resolver,
            policy,
            version_probe,
            MonitorConfig::default(),
        )
    }

    fn with_parts_full(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
        policy: crate::process::restart::RestartPolicy,
        version_probe: Arc<dyn VersionProbe>,
        monitor: MonitorConfig,
    ) -> io::Result<Self> {
        let (events, _) = broadcast::channel(256);
        let registry = Registry::open(root)?;
        sweep_staging(&registry.paths().apps_dir());
        let core = Arc::new_cyclic(|weak| {
            let core = Core {
                registry,
                gateway,
                host_version,
                events,
                lock: Mutex::new(()),
                closed: std::sync::atomic::AtomicBool::new(false),
                resolver,
                version_probe,
                prefetched_versions: Mutex::new(HashMap::new()),
                issues: Mutex::new(HashMap::new()),
                policy,
                monitor,
                supervisions: Mutex::new(HashMap::new()),
                self_weak: std::sync::OnceLock::new(),
                #[cfg(test)]
                prepared: std::sync::atomic::AtomicUsize::new(0),
            };
            let _ = core.self_weak.set(weak.clone());
            core
        });
        Ok(Self { core })
    }

    /// (仅测试)注入一个小退避的重启策略,让崩溃-放弃在毫秒级完成。
    #[cfg(test)]
    pub(crate) fn with_resolver_and_policy_for_test(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
        policy: crate::process::restart::RestartPolicy,
    ) -> io::Result<Self> {
        Self::with_resolver_and_policy(root, host_version, gateway, resolver, policy)
    }

    /// (仅测试)注入假的 `VersionProbe`,并允许调小重启策略。
    #[cfg(test)]
    pub(crate) fn with_parts_for_test(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
        policy: crate::process::restart::RestartPolicy,
        version_probe: Arc<dyn VersionProbe>,
    ) -> io::Result<Self> {
        Self::with_parts(root, host_version, gateway, resolver, policy, version_probe)
    }

    /// (仅测试)调小运行中健康检查的参数与重启退避;解释器版本用真实探测。
    /// 跨 crate 集成测试要用,所以不做 `#[cfg(test)]`,而是 `#[doc(hidden)]` 的不稳定钩子。
    #[doc(hidden)]
    pub fn with_monitor_for_test(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
        resolver: Arc<dyn RuntimeResolver>,
        policy: crate::process::restart::RestartPolicy,
        monitor: MonitorConfig,
    ) -> io::Result<Self> {
        Self::with_parts_full(
            root,
            host_version,
            gateway,
            resolver,
            policy,
            Arc::new(SystemVersionProbe::new()),
            monitor,
        )
    }

    pub fn events(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    /// 首次导航用的地址(带一次性换 Cookie 的令牌)。**秘密,不要写日志、不要广播。**
    pub fn launch_url(&self, id: &AppId) -> String {
        self.gateway.launch_url(id)
    }
}

/// 公开 API 的转发层:实现都在 `Core` 上(`AppManager` 经 `Deref` 也能直接调到它们),
/// 这几个方法在 crate 外(dozerd)使用,必须有公有的接收者类型,故在 `AppManager` 上再暴露一层。
impl AppManager {
    pub fn install_plan(
        &self,
        source: &AppSource,
        provenance: Provenance,
        trust: TrustLevel,
    ) -> Result<InstallPlan, ManagerError> {
        self.core.install_plan(source, provenance, trust)
    }

    pub fn install(
        &self,
        approved: &ApprovedInstallPlan,
        source: &AppSource,
        now_ms: u64,
    ) -> Result<(), ManagerError> {
        self.core.install(approved, source, now_ms)
    }

    pub fn start(&self, id: &AppId) -> Result<String, ManagerError> {
        self.core.start(id)
    }

    pub fn stop(&self, id: &AppId) -> Result<(), ManagerError> {
        self.core.stop(id)
    }

    pub fn uninstall(&self, id: &AppId, mode: UninstallMode) -> Result<(), ManagerError> {
        self.core.uninstall(id, mode)
    }

    pub fn launch_url_if_running(&self, id: &AppId) -> Result<String, ManagerError> {
        self.core.launch_url_if_running(id)
    }

    pub fn list(&self) -> Result<Vec<AppSummary>, ManagerError> {
        self.core.list()
    }

    /// 读某应用日志的末尾(有界、已清洗)。未安装 → `NotInstalled`;日志不存在 → 空文本。
    pub fn logs(&self, id: &AppId, max_lines: u32) -> Result<(String, bool), ManagerError> {
        self.core.logs(id, max_lines)
    }

    pub fn suspend_all(&self) -> ReconcileReport {
        self.core.suspend_all()
    }

    pub fn reconcile(&self) -> ReconcileReport {
        self.core.reconcile()
    }
}

impl Core {
    /// 单写者锁。持锁线程 panic 会毒化它,但它保护的是 `()`(锁住的是"同一时刻只有一个改状态的操作",
    /// 没有需要保持的数据不变量),所以取回内部数据继续用,不能让此后每个加锁的调用都跟着 panic。
    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 非阻塞取锁(毒化时同样取回内部数据)。监管线程用它"拿锁 + 查取消"循环,绝不阻塞在锁上——
    /// `stop_locked`/`suspend_all` 会持着锁 `join` 监管线程,线程若阻塞等锁就是死锁。
    #[allow(dead_code)]
    fn try_guard(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        match self.lock.try_lock() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    fn emit(&self, event: AppEvent) {
        // 没有订阅者时 send 会返回错误——不是问题
        let _ = self.events.send(event);
    }

    /// 拿到指向自己的 `Arc`(装载于 `Arc::new_cyclic`)。构造之后必定已装填,退化为 `panic` 只在被误用时发生。
    fn self_arc(&self) -> Arc<Core> {
        self.self_weak
            .get()
            .expect("Core::self_weak 未装填")
            .upgrade()
            .expect("Core 的 Arc 已全部释放")
    }
}

impl Core {
    pub(crate) fn install_plan(
        &self,
        source: &AppSource,
        provenance: Provenance,
        trust: TrustLevel,
    ) -> Result<InstallPlan, ManagerError> {
        let AppSource::LocalDir { path: dir } = source;
        ensure_absolute(dir)?;
        let package = read_package(dir, &self.host_version)?;
        self.plan_for(&package, dir, provenance, trust)
    }

    fn plan_for(
        &self,
        package: &Package,
        package_dir: &Path,
        provenance: Provenance,
        trust: TrustLevel,
    ) -> Result<InstallPlan, ManagerError> {
        check_supported(&package.manifest.runtime, package_dir)?;
        let record = self.registry.load(&package.manifest.id)?;
        let installed = record.as_ref().map(|r| Installed {
            version: &r.current_version,
            grants: &r.grants,
        });
        Ok(InstallPlan::build(PlanInput {
            manifest: &package.manifest,
            manifest_digest: package.manifest_digest.clone(),
            source_digest: package.source_digest.clone(),
            provenance,
            trust,
            enforcement: enforcement_for(&package.manifest.runtime),
            installed,
        }))
    }

    /// 安装一份**已批准**的计划。**一切决定都只基于 staging 里的副本**:源目录只被"拷贝"这一个动作读取,
    /// 拷完之后它再怎么变都与本次安装无关;对 staging 副本重新计算并 `verify`,通过才落位。
    pub(crate) fn install(
        &self,
        approved: &ApprovedInstallPlan,
        source: &AppSource,
        now_ms: u64,
    ) -> Result<(), ManagerError> {
        let AppSource::LocalDir { path: dir } = source;
        ensure_absolute(dir)?;
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ManagerError::ShuttingDown);
        }
        let staging = self
            .registry
            .paths()
            .apps_dir()
            .join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
        // 拷贝是慢的(可能是带 node_modules 的大目录):**不持锁**做,否则 `suspend_all`(dozerd 退出收尾)要一直
        // 等到它结束;拷完之后才拿锁做决定、核对、落位
        let result = copy_tree(dir, &staging)
            .map_err(ManagerError::from)
            // 对整棵 staging 树读 manifest、算摘要同样是慢的(macOS 上拷贝是 clone,哈希才是大头):也在锁外做
            .and_then(|()| read_package(&staging, &self.host_version))
            .and_then(|package| {
                #[cfg(test)]
                self.prepared
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _guard = self.guard();
                self.install_staged(approved, &staging, package, now_ms)
            });
        // 成功时 staging 已经改名走了,这里是空操作;失败时清掉
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    fn install_staged(
        &self,
        approved: &ApprovedInstallPlan,
        staging: &Path,
        package: Package,
        now_ms: u64,
    ) -> Result<(), ManagerError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ManagerError::ShuttingDown);
        }
        let id = package.manifest.id.clone();
        let version = package.manifest.version;
        let plan = self.plan_for(
            &package,
            staging,
            approved.plan().provenance,
            approved.plan().trust,
        )?;
        let verified = approved.verify(plan).map_err(ManagerError::Verify)?;

        let existing = self.registry.load(&id)?;
        if let Some(r) = &existing {
            if r.versions.iter().any(|v| v.version == version) {
                return Err(ManagerError::AlreadyInstalled(version));
            }
            if !matches!(
                r.observed,
                ObservedState::Installed | ObservedState::Stopped | ObservedState::Failed { .. }
            ) {
                return Err(ManagerError::Busy(id));
            }
        }

        let final_dir = self.registry.paths().package_dir(&id, &version);
        fs::create_dir_all(final_dir.parent().expect("package_dir 有父目录"))?;
        // 没有版本记录却已经存在的版本目录,是上一次崩溃留下的残骸:清掉,不能让它挡住重试
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)?;
        }
        fs::rename(staging, &final_dir)?;
        // 落位之后的任何一步失败都要把包撤回,否则会留下"有包目录、没有记录"的状态
        let upgrading = existing.is_some();
        let recorded = (|| -> Result<(), ManagerError> {
            let mut record = existing.unwrap_or_else(|| AppRecord {
                format_version: RECORD_FORMAT_VERSION,
                id: id.clone(),
                desired: DesiredState::Stopped,
                observed: ObservedState::Installed,
                current_version: version,
                grants: verified.requested,
                versions: Vec::new(),
                data_store_id: data_store_id_hex(&id),
            });
            record.grants = verified.requested;
            record.versions.push(VersionRecord {
                version,
                manifest_digest: verified.manifest_digest.clone(),
                source_digest: verified.source_digest.clone(),
                installed_ms: now_ms,
            });
            if upgrading && matches!(record.observed, ObservedState::Failed { .. }) {
                record.observed = ObservedState::Installed;
            }
            // current_version 最后才写;它之前的任何一步失败,调用方会撤回刚落位的包,旧版本不受影响
            record.current_version = version;
            self.registry.save(&record)?;
            Ok(())
        })();
        if let Err(e) = recorded {
            let _ = fs::remove_dir_all(&final_dir);
            return Err(e);
        }

        self.emit(AppEvent::Installed { app: id.clone() });
        if upgrading && !verified.permission_diff.is_empty() {
            self.emit(AppEvent::ManifestChanged {
                app: id,
                permission_changes: verified.permission_diff,
            });
        }
        Ok(())
    }

    fn load_record(&self, id: &AppId) -> Result<AppRecord, ManagerError> {
        self.registry
            .load(id)?
            .ok_or_else(|| ManagerError::NotInstalled(id.clone()))
    }

    fn set_observed(
        &self,
        record: &mut AppRecord,
        observed: ObservedState,
    ) -> Result<(), ManagerError> {
        record.observed = observed.clone();
        self.registry.save(record)?;
        self.emit(AppEvent::StateChanged {
            app: record.id.clone(),
            state: observed,
        });
        Ok(())
    }

    /// 启动:把应用的静态站点注册进 gateway。返回不含令牌的站点地址。
    pub(crate) fn start(&self, id: &AppId) -> Result<String, ManagerError> {
        // 版本探测可能阻塞数秒,放在锁外做;结果在锁内的检查里取用。
        self.prefetch_version_output(id);
        let result = {
            let _guard = self.guard();
            self.start_locked(id)
        };
        self.take_prefetched_version(id);
        result
    }

    fn start_locked(&self, id: &AppId) -> Result<String, ManagerError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ManagerError::ShuttingDown);
        }
        let mut record = self.load_record(id)?;
        match &record.observed {
            ObservedState::Running => return Ok(self.gateway.site_url(id)),
            ObservedState::Installed | ObservedState::Stopped => {}
            ObservedState::Failed {
                retryable: true, ..
            } => {}
            other => {
                return Err(ManagerError::BadState {
                    app: id.clone(),
                    state: other.clone(),
                });
            }
        }
        let record_dir = self
            .registry
            .paths()
            .package_dir(id, &record.current_version);
        let text = fs::read_to_string(record_dir.join("manifest.toml"))?;
        let manifest = Manifest::from_toml(&text, &self.host_version)?;
        match &manifest.runtime {
            Runtime::StaticWeb { .. } => {}
            Runtime::Node { .. } | Runtime::Python { .. } => {
                return self.start_process_locked(&mut record, &manifest, &record_dir);
            }
            Runtime::Container { .. } => {
                return Err(ManagerError::UnsupportedRuntime("container".into()));
            }
        }
        let source = static_source(&manifest)?.to_string();
        let root = record_dir.join(&source);
        if !root.is_dir() {
            let reason = format!("应用包里缺少站点目录 {source}");
            self.set_observed(
                &mut record,
                ObservedState::Failed {
                    reason,
                    retryable: false,
                },
            )?;
            return Err(ManagerError::MissingSource(root));
        }
        self.gateway.add_site(id, root, csp_for(&record.grants));
        record.desired = DesiredState::Running;
        self.set_observed(&mut record, ObservedState::Running)?;
        let url = self.gateway.site_url(id);
        self.emit(AppEvent::EndpointChanged {
            app: id.clone(),
            url: Some(url.clone()),
        });
        Ok(url)
    }

    /// 进程型应用:`start` 在监管线程起来后**立即返回**站点地址(`Started{url}` = "已受理"),
    /// 观察态依次为 `Preparing`(装依赖)→ `Starting`(起进程/等健康)→ `Running`(监管线程回调)。
    fn start_process_locked(
        &self,
        record: &mut AppRecord,
        manifest: &Manifest,
        package_dir: &Path,
    ) -> Result<String, ManagerError> {
        let id = record.id.clone();
        // 收掉上一条已结束的监管线程(崩溃后重试、或 Failed 复位后再启动)
        let _ = self.cancel_supervision(&id);
        self.clear_issue(&id);

        let (argv, port_env) = match &manifest.runtime {
            Runtime::Node { command, http, .. } | Runtime::Python { command, http, .. } => {
                (command.clone(), http.port_env.clone())
            }
            _ => unreachable!("start_process_locked 只接进程型 runtime"),
        };
        // argv[0] 与依赖安装命令的 argv[0] 都要解析成绝对路径(npm 是脚本,得在同目录找到 node)
        let resolved = match self.resolve_program(&argv) {
            Ok(r) => r,
            Err(e) => return self.fail_runtime_missing(record, &argv, e),
        };
        let install_resolved = match install_argv(&manifest.runtime) {
            Some(install) => match self.resolve_program(&install) {
                Ok(r) => Some(r),
                Err(e) => return self.fail_runtime_missing(record, &install, e),
            },
            None => None,
        };
        self.check_version_requirement(record, manifest, &argv, &resolved)?;

        let cache_dir = self.registry.paths().cache_dir(&id);
        let data_dir = self.registry.paths().data_dir(&id);
        let logs_dir = self.registry.paths().logs_dir(&id);
        let run_dir = self.registry.paths().run_dir(&id);
        for d in [&cache_dir, &data_dir, &logs_dir, &run_dir] {
            fs::create_dir_all(d)?;
        }

        let mut argv = argv;
        argv[0] = resolved.program.to_string_lossy().into_owned();
        let install = match (install_argv(&manifest.runtime), install_resolved.as_ref()) {
            (Some(mut inst_argv), Some(inst_resolved)) => {
                inst_argv[0] = inst_resolved.program.to_string_lossy().into_owned();
                let marker = deps_marker(
                    &cache_dir,
                    lockfile_of(&manifest.runtime)
                        .map(|lf| fs::read(package_dir.join(lf)).unwrap_or_default())
                        .as_deref(),
                );
                Some(crate::supervisor::Install {
                    argv: inst_argv,
                    marker,
                    timeout: supervisor::DEFAULT_INSTALL_TIMEOUT,
                })
            }
            _ => None,
        };

        let path_sources: Vec<&Resolved> = std::iter::once(&resolved)
            .chain(install_resolved.as_ref())
            .collect();
        let mut parent_env: Vec<(OsString, OsString)> =
            std::env::vars_os().filter(|(k, _)| k != "PATH").collect();
        parent_env.push((OsString::from("PATH"), child_path(&path_sources)));
        let launch = Launch {
            app_id: id.clone(),
            argv,
            install,
            cwd: package_dir.to_path_buf(),
            parent_env,
            extra_env: cache_env(&cache_dir),
            port_env,
            data_dir,
            health_path: manifest.health.path.clone(),
            startup_budget: supervisor::DEFAULT_STARTUP_BUDGET,
            log_path: logs_dir.join("app.log"),
            log_max_bytes: 1 << 20,
            log_keep: 3,
            run_dir,
            policy: self.policy,
            grace: supervisor::DEFAULT_GRACE,
            monitor_interval: self.monitor.interval,
            monitor_timeout: self.monitor.timeout,
            monitor_failures: self.monitor.failures,
        };

        record.desired = DesiredState::Running;
        self.set_observed(record, ObservedState::Starting)?;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let core = self.self_arc();
        let thread = {
            let cancel = cancel.clone();
            let app_id = id.clone();
            let tr: Arc<dyn Transitions> = Arc::new(AppTransitions {
                core: core.clone(),
                id: app_id.clone(),
                cancel: cancel.clone(),
            });
            std::thread::Builder::new()
                .name(format!("bytehost-app-{app_id}"))
                .spawn(move || supervisor::run(launch, cancel, tr))
                .map_err(|e| ManagerError::BadSource(format!("无法创建监管线程: {e}")))?
        };
        self.supervisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.clone(),
                Supervision {
                    cancel,
                    thread: Some(thread),
                },
            );
        Ok(self.gateway.site_url(&id))
    }

    /// 把解释器名解析成绝对路径,失败映射为 `RuntimeUnavailable`。
    fn resolve_program(&self, argv: &[String]) -> Result<Resolved, ManagerError> {
        let program = argv.first().map(String::as_str).unwrap_or("");
        match self.resolver.resolve(program) {
            Ok(r) => Ok(r),
            Err(ResolveError::NotInstalled(name)) => Err(ManagerError::RuntimeUnavailable(name)),
        }
    }

    /// 校验清单声明的运行时版本要求(只对能确定解释器的情况做)。
    ///
    /// - Node 应用:即使 `argv[0]` 是 `npm`/`npx`,也解析 `node` 来检查;解析不到 `node` 走 `RuntimeUnavailable`。
    /// - Python 应用:`argv[0]` 是 `uv` 时**不检查**(解释器由 uv 在运行时挑,宿主拿不到)。
    ///
    /// 不满足 → `Failed{retryable:true}` + `emit(RuntimeUnavailable{Unsatisfied})` + 记 `AppIssue` +
    /// 返回 `RuntimeVersion`。解析不出输出按"不满足"处理。
    fn check_version_requirement(
        &self,
        record: &mut AppRecord,
        manifest: &Manifest,
        argv: &[String],
        resolved: &Resolved,
    ) -> Result<(), ManagerError> {
        let (runtime_name, req_text, req, program) =
            match self.version_target(manifest, argv, resolved) {
                VersionTarget::Skip => return Ok(()),
                VersionTarget::NodeMissing => {
                    return self
                        .fail_runtime_missing(
                            record,
                            &[String::from("node")],
                            ManagerError::RuntimeUnavailable("node".into()),
                        )
                        .map(|_| ());
                }
                VersionTarget::Probe {
                    runtime,
                    req_text,
                    req,
                    program,
                } => (runtime, req_text, req, program),
            };
        // 优先用锁外预探的结果;没有(如启动对账走的 `start_locked`)才在这里现探。
        let output = match self.take_prefetched_version_for(&record.id, &program) {
            Some(out) => out,
            None => self.version_probe.version_output(&program),
        };
        let found = match output
            .as_deref()
            .and_then(crate::runtime_version::parse_version_output)
        {
            Some(v) => v,
            None => {
                let first_line = output
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                let found = format!("无法识别: {first_line}");
                return self.fail_runtime_version(record, &runtime_name, &req_text, found);
            }
        };
        if req.matches(found) {
            return Ok(());
        }
        let found = format!("{}.{}.{}", found.0, found.1, found.2);
        self.fail_runtime_version(record, &runtime_name, &req_text, found)
    }

    /// 决定这次启动要不要、以及探哪个解释器。
    ///
    /// - Node 应用:即使 `argv[0]` 是 `npm`/`npx`,也解析 `node` 来检查。
    /// - Python 应用:`argv[0]` 是 `uv` 时不检查。
    /// - 清单校验已拒绝畸形要求;这里解析不了也按不检查处理。
    fn version_target(
        &self,
        manifest: &Manifest,
        argv: &[String],
        resolved: &Resolved,
    ) -> VersionTarget {
        let (runtime, req_text, program) = match &manifest.runtime {
            Runtime::Node {
                node: Some(req), ..
            } => match self.resolver.resolve("node") {
                Ok(r) => ("node".to_string(), req.clone(), r.program),
                Err(ResolveError::NotInstalled(_)) => return VersionTarget::NodeMissing,
            },
            Runtime::Python {
                python: Some(req), ..
            } => {
                if argv.first().map(String::as_str) == Some("uv") {
                    return VersionTarget::Skip;
                }
                (
                    argv.first().cloned().unwrap_or_default(),
                    req.clone(),
                    resolved.program.clone(),
                )
            }
            _ => return VersionTarget::Skip,
        };
        match crate::runtime_version::VersionReq::parse(&req_text) {
            Ok(req) => VersionTarget::Probe {
                runtime,
                req_text,
                req,
                program,
            },
            Err(_) => VersionTarget::Skip,
        }
    }

    /// 拿锁前预探版本,结果登记到 `prefetched_versions`。任何一步不顺(没装、读不到清单……)就什么都不做,
    /// 交给锁内的检查现探并报出正式的错误。
    fn prefetch_version_output(&self, id: &AppId) {
        self.take_prefetched_version(id);
        let Ok(record) = self.load_record(id) else {
            return;
        };
        let dir = self
            .registry
            .paths()
            .package_dir(id, &record.current_version);
        let Ok(text) = fs::read_to_string(dir.join("manifest.toml")) else {
            return;
        };
        let Ok(manifest) = Manifest::from_toml(&text, &self.host_version) else {
            return;
        };
        let argv = match &manifest.runtime {
            Runtime::Node { command, .. } | Runtime::Python { command, .. } => command.clone(),
            _ => return,
        };
        let Ok(resolved) = self.resolve_program(&argv) else {
            return;
        };
        if let VersionTarget::Probe { program, .. } =
            self.version_target(&manifest, &argv, &resolved)
        {
            let output = self.version_probe.version_output(&program);
            self.prefetched_versions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.clone(), (program, output));
        }
    }

    /// 丢掉某应用登记的预探结果。
    fn take_prefetched_version(&self, id: &AppId) {
        self.prefetched_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// 取走预探结果;解释器路径不一致(清单在预探后变了)视为没有。
    fn take_prefetched_version_for(&self, id: &AppId, program: &Path) -> Option<Option<String>> {
        let (probed, output) = self
            .prefetched_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)?;
        (probed == program).then_some(output)
    }

    /// 版本不满足:记问题、发事件、把 `Failed{retryable:true}` 落盘,返回 `RuntimeVersion`。
    fn fail_runtime_version(
        &self,
        record: &mut AppRecord,
        runtime: &str,
        required: &str,
        found: String,
    ) -> Result<(), ManagerError> {
        let reason = format!("需要 {runtime} {required},当前 {found}");
        let _ = self.set_observed(
            record,
            ObservedState::Failed {
                reason,
                retryable: true,
            },
        );
        self.set_issue(
            &record.id,
            AppIssue::RuntimeVersion {
                runtime: runtime.to_string(),
                required: required.to_string(),
                found: found.clone(),
            },
        );
        self.emit(AppEvent::RuntimeUnavailable {
            app: record.id.clone(),
            reason: RuntimeReason::Unsatisfied {
                runtime: runtime.to_string(),
                required: required.to_string(),
                found: found.clone(),
            },
        });
        Err(ManagerError::RuntimeVersion {
            runtime: runtime.to_string(),
            required: required.to_string(),
            found,
        })
    }

    /// 记下/清除某应用的当前问题。
    fn set_issue(&self, id: &AppId, issue: AppIssue) {
        self.issues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), issue);
    }

    fn clear_issue(&self, id: &AppId) {
        self.issues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// 运行时缺失:`set_observed(Failed{retryable:true})` + `emit(RuntimeUnavailable{NotInstalled})`,
    /// 然后把错误交回给调用方(线上类别 `Unavailable`,用户装好运行时可以重试)。
    fn fail_runtime_missing(
        &self,
        record: &mut AppRecord,
        argv: &[String],
        err: ManagerError,
    ) -> Result<String, ManagerError> {
        let runtime = argv.first().cloned().unwrap_or_default();
        let reason = format!("运行时 {runtime} 未安装");
        let _ = self.set_observed(
            record,
            ObservedState::Failed {
                reason,
                retryable: true,
            },
        );
        self.set_issue(
            &record.id,
            AppIssue::RuntimeMissing {
                runtime: runtime.clone(),
            },
        );
        self.emit(AppEvent::RuntimeUnavailable {
            app: record.id.clone(),
            reason: RuntimeReason::NotInstalled { runtime },
        });
        Err(err)
    }

    /// 取消并取出(已结束的)监管线程句柄。不 `join`——调用方按上下文决定。
    fn cancel_supervision(&self, id: &AppId) -> Option<std::thread::JoinHandle<()>> {
        let mut map = self
            .supervisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(id).map(|mut s| {
            s.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            s.thread.take().expect("登记时必有句柄")
        })
    }

    /// 停止(也用来把 `Failed` 复位成 `Stopped`)。对已经停止的应用是空操作。
    pub(crate) fn stop(&self, id: &AppId) -> Result<(), ManagerError> {
        let _guard = self.guard();
        self.stop_locked(id)
    }

    fn stop_locked(&self, id: &AppId) -> Result<(), ManagerError> {
        // 先叫停监管线程并等它退出(它绝不会阻塞等锁,所以持锁 join 不会死锁)
        if let Some(h) = self.cancel_supervision(id) {
            let _ = h.join();
        }
        self.clear_issue(id);
        let mut record = self.load_record(id)?;
        let was_serving = self.gateway.remove_site(id);
        record.desired = DesiredState::Stopped;
        match record.observed {
            ObservedState::Running
            | ObservedState::Failed { .. }
            | ObservedState::Preparing
            | ObservedState::Starting => {
                self.set_observed(&mut record, ObservedState::Stopped)?;
            }
            _ => self.registry.save(&record)?,
        }
        if was_serving {
            self.emit(AppEvent::EndpointChanged {
                app: id.clone(),
                url: None,
            });
        }
        Ok(())
    }

    /// 卸载(不存在不算错误)。运行中先停。
    pub(crate) fn uninstall(&self, id: &AppId, mode: UninstallMode) -> Result<(), ManagerError> {
        let _guard = self.guard();
        if self.registry.load(id)?.is_none() {
            self.registry.uninstall(id, mode)?;
            self.clear_issue(id);
            return Ok(());
        }
        self.stop_locked(id)?;
        self.registry.uninstall(id, mode)?;
        self.clear_issue(id);
        self.emit(AppEvent::StateChanged {
            app: id.clone(),
            state: ObservedState::NotInstalled,
        });
        Ok(())
    }

    /// 带令牌的启动地址,**只给正在运行的应用**(没运行的应用打开只会是 404)。在锁内判断并构造,
    /// 不会出现"刚判断完它在运行、构造地址前它被停掉"的空档。**秘密:不要写日志、不要广播。**
    pub(crate) fn launch_url_if_running(&self, id: &AppId) -> Result<String, ManagerError> {
        let _guard = self.guard();
        let record = self.load_record(id)?;
        match record.observed {
            ObservedState::Running => Ok(self.gateway.launch_url(id)),
            other => Err(ManagerError::BadState {
                app: id.clone(),
                state: other,
            }),
        }
    }

    /// 读某应用日志的末尾(有界、已清洗)。未安装 → `NotInstalled`;日志不存在 → 空文本。
    /// 只在本机返回给 GUI,不写 dozerd 日志、不广播事件。
    pub(crate) fn logs(&self, id: &AppId, max_lines: u32) -> Result<(String, bool), ManagerError> {
        let _guard = self.guard();
        let _ = self.load_record(id)?;
        let tail = crate::logs::read_tail(&self.registry.paths().logs_dir(id), max_lines as usize)
            .map_err(|e| ManagerError::Io(io::Error::other(format!("读取应用日志失败:{e}"))))?;
        Ok((tail.text, tail.truncated))
    }

    pub(crate) fn list(&self) -> Result<Vec<AppSummary>, ManagerError> {
        let listing = self.registry.list()?;
        Ok(listing
            .apps
            .into_iter()
            .map(|r| {
                let name = fs::read_to_string(
                    self.registry
                        .paths()
                        .package_dir(&r.id, &r.current_version)
                        .join("manifest.toml"),
                )
                .ok()
                .and_then(|t| Manifest::from_toml(&t, &self.host_version).ok())
                .map(|m| m.name)
                .unwrap_or_else(|| r.id.to_string());
                let url = matches!(r.observed, ObservedState::Running)
                    .then(|| self.gateway.site_url(&r.id));
                let issue = matches!(r.observed, ObservedState::Failed { .. })
                    .then(|| {
                        self.issues
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&r.id)
                            .cloned()
                    })
                    .flatten();
                AppSummary {
                    name,
                    version: r.current_version,
                    desired: r.desired,
                    observed: r.observed,
                    url,
                    issue,
                    id: r.id,
                }
            })
            .collect())
    }

    /// supervisor 退出前调用:撤下所有站点,把运行中的应用的观察态落成 `Stopped`,但**保留 `desired`**——
    /// 下次启动时 `reconcile` 会按 `desired = Running` 把它们重新拉起("应用跟随 dozerd")。
    pub(crate) fn suspend_all(&self) -> ReconcileReport {
        let _guard = self.guard();
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        // 第一阶段:置取消并收集所有监管线程句柄。线程拿锁用尝试循环,所以持锁 join 不会死锁;
        // 每个线程的收尾是并行的,总耗时 ≈ 最长的一个宽限,不是 N×宽限。
        let handles: Vec<std::thread::JoinHandle<()>> = {
            let mut map = self
                .supervisions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.drain()
                .map(|(_, mut s)| {
                    s.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                    s.thread.take().expect("登记时必有句柄")
                })
                .collect()
        };
        for h in handles {
            let _ = h.join();
        }
        let mut report = ReconcileReport::default();
        let listing = match self.registry.list() {
            Ok(l) => l,
            Err(e) => {
                report
                    .problems
                    .push(("(应用目录)".to_string(), e.to_string()));
                return report;
            }
        };
        report.problems.extend(listing.problems);
        for mut record in listing.apps {
            let was_serving = self.gateway.remove_site(&record.id);
            if matches!(
                record.observed,
                ObservedState::Running | ObservedState::Preparing | ObservedState::Starting
            ) && let Err(e) = self.set_observed(&mut record, ObservedState::Stopped)
            {
                report.failures.push((record.id.clone(), e));
            }
            if was_serving {
                self.emit(AppEvent::EndpointChanged {
                    app: record.id.clone(),
                    url: None,
                });
            }
        }
        report
    }

    /// supervisor 启动时调用:把持久化的观察态修正为现实(应用随 supervisor 一起停了),再按 `desired` 对账。
    /// 一个应用失败不影响其他应用;读不出来的记录(损坏的 `state.json`)单独报告,不会被悄悄忽略。
    pub(crate) fn reconcile(&self) -> ReconcileReport {
        let _guard = self.guard();
        // reconcile 代表 supervisor(重新)开始工作
        self.closed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mut report = ReconcileReport::default();
        let listing = match self.registry.list() {
            Ok(l) => l,
            Err(e) => {
                report
                    .problems
                    .push(("(应用目录)".to_string(), e.to_string()));
                return report;
            }
        };
        report.problems.extend(listing.problems);
        for mut record in listing.apps {
            let id = record.id.clone();
            // 孤儿回收:supervisor(我们)重启后,上次留下的 `run_dir/process.json` 记的进程可能还活着,
            // 先按记录的 pid 收掉,再按 desired 启动,避免"旧进程占着端口、新进程起不来"。
            if let Err(e) = crate::process::supervise::reap_orphan(
                &self.registry.paths().run_dir(&id),
                std::time::Duration::from_secs(2),
            ) {
                report
                    .problems
                    .push((format!("(孤儿回收) {id}"), e.to_string()));
            }
            let recovered = recover_after_supervisor_restart(record.observed.clone());
            if recovered != record.observed
                && let Err(e) = self.set_observed(&mut record, recovered)
            {
                report.failures.push((id, e));
                continue;
            }
            let result = match next_action(record.desired, &record.observed) {
                Some(Action::Start) => self.start_locked(&id).map(|_| ()),
                Some(Action::Stop) => self.stop_locked(&id),
                // 卸载只删程序(数据由用户显式要求才删)
                Some(Action::Uninstall) => self
                    .registry
                    .uninstall(&id, UninstallMode::Program)
                    .map_err(Into::into),
                None => Ok(()),
            };
            if let Err(e) = result {
                report.failures.push((id, e));
            }
        }
        report
    }
}

/// `reconcile`/`suspend_all` 的结果:单个应用的失败,以及读不出来的记录(目录名, 原因)。
#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub failures: Vec<(AppId, ManagerError)>,
    pub problems: Vec<(String, String)>,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && self.problems.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn manager_errors_map_to_stable_wire_kinds() {
        use crate::proto::AppErrorKind as K;
        let id = || AppId::new("a").unwrap();
        let cases = [
            (ManagerError::BadSource("x".into()), K::Rejected),
            (ManagerError::MissingSource("/x".into()), K::Rejected),
            (
                ManagerError::UnsupportedRuntime("python".into()),
                K::Unsupported,
            ),
            (
                ManagerError::AlreadyInstalled(Version::new(1, 0, 0)),
                K::Conflict,
            ),
            (ManagerError::Busy(id()), K::Conflict),
            (
                ManagerError::BadState {
                    app: id(),
                    state: ObservedState::Stopped,
                },
                K::Conflict,
            ),
            (ManagerError::NotInstalled(id()), K::NotFound),
            (ManagerError::ShuttingDown, K::Unavailable),
            (ManagerError::Io(io::Error::other("x")), K::Internal),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind(), kind, "{error}");
        }
    }

    use super::*;
    use crate::gateway::GatewayConfig;
    use crate::permissions::{Access, Enforcement, PermissionKey};
    use crate::plan::Approval;
    use crate::testutil::{Req, Resp, write_files};

    const HOST: Version = Version::new(0, 1, 0);

    fn manifest_toml(id: &str, version: &str, extra: &str) -> String {
        format!(
            r#"schema_version = 1
min_host_version = "0.1.0"
id = "{id}"
name = "{id} app"
version = "{version}"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
{extra}"#
        )
    }

    /// 在 `dir` 下写一个静态应用(manifest.toml + web/index.html),返回来源。
    fn write_app(dir: &Path, id: &str, version: &str, extra: &str, body: &str) -> AppSource {
        write_files(
            dir,
            &[
                ("manifest.toml", &manifest_toml(id, version, extra)),
                ("web/index.html", body),
            ],
        );
        AppSource::LocalDir {
            path: dir.to_path_buf(),
        }
    }

    struct Rig {
        tmp: tempfile::TempDir,
        gateway: Arc<Gateway>,
        manager: AppManager,
    }

    async fn rig() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let manager = AppManager::new(tmp.path().join("bytehost"), HOST, gateway.clone()).unwrap();
        Rig {
            tmp,
            gateway,
            manager,
        }
    }

    impl Rig {
        fn src_dir(&self, name: &str) -> PathBuf {
            self.tmp.path().join("src").join(name)
        }

        fn approve(&self, source: &AppSource) -> ApprovedInstallPlan {
            self.manager
                .install_plan(source, Provenance::Local, TrustLevel::Trusted)
                .unwrap()
                .approve(Approval {
                    approver: "test".into(),
                    approved_ms: 1,
                })
        }

        fn install(&self, source: &AppSource) -> Result<(), ManagerError> {
            self.manager.install(&self.approve(source), source, 100)
        }

        /// 带着有效 Cookie 取应用的某个路径。
        fn fetch(&self, id: &AppId, target: &str) -> Resp {
            let url = self.manager.launch_url(id);
            let token = url.split("bh_token=").nth(1).unwrap().to_string();
            let port = self.gateway.port();
            Req::get(port, &format!("{id}.localhost:{port}"), target)
                .cookie(&format!("bh_session={token}"))
                .send()
        }
    }

    fn id(s: &str) -> AppId {
        AppId::new(s).unwrap()
    }

    /// 安装失败之后,`apps/` 下不能留下 staging 目录,也不能凭空多出应用目录。
    fn assert_no_leftovers(rig: &Rig) {
        let apps = rig.manager.registry.paths().apps_dir();
        let names: Vec<String> = fs::read_dir(&apps)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.starts_with(".staging-")),
            "{names:?}"
        );
    }

    fn drain(rx: &mut broadcast::Receiver<AppEvent>) -> Vec<AppEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_plan_describes_the_app_and_is_honest_about_static_web_enforcement() {
        let rig = rig().await;
        let src = write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "0.17.0",
            "[permissions]\nclipboard = \"read_write\"\n",
            "<h1>x</h1>",
        );
        let plan = rig
            .manager
            .install_plan(&src, Provenance::ThirdParty, TrustLevel::Untrusted)
            .unwrap();
        assert_eq!(plan.app_id, id("excalidraw"));
        assert_eq!(plan.runtime_kind, "static_web");
        assert_eq!(plan.provenance, Provenance::ThirdParty);
        assert_eq!(plan.permission_diff.len(), 1);
        assert_eq!(plan.permission_diff[0].key, PermissionKey::Clipboard);
        let net = plan
            .enforcement
            .iter()
            .find(|e| e.key == PermissionKey::NetworkOutbound)
            .unwrap();
        assert_eq!(net.enforcement, Enforcement::Advisory);
        assert!(plan.will_run[0].contains("不执行任何命令"));
        assert_eq!(plan.manifest_digest.len(), 64);
        assert_eq!(plan.source_digest.len(), 64);
        assert!(
            rig.manager.list().unwrap().is_empty(),
            "出计划不会安装任何东西"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_start_serve_and_stop_round_trip_with_events() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let src = write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "0.17.0",
            "",
            "<h1>excalidraw</h1>",
        );
        rig.install(&src).unwrap();
        let app = id("excalidraw");

        let listed = rig.manager.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "excalidraw app");
        assert_eq!(listed[0].observed, ObservedState::Installed);
        assert_eq!(listed[0].desired, DesiredState::Stopped);
        assert_eq!(listed[0].url, None);
        assert_eq!(rig.fetch(&app, "/").status, 404, "没启动:站点没注册");

        let url = rig.manager.start(&app).unwrap();
        assert_eq!(
            url,
            format!("http://excalidraw.localhost:{}/", rig.gateway.port())
        );
        let r = rig.fetch(&app, "/");
        assert_eq!(r.status, 200);
        assert!(r.text().contains("excalidraw"));
        assert!(
            r.header("content-security-policy").is_some(),
            "默认出站网络为 none → 带 CSP"
        );
        let listed = rig.manager.list().unwrap();
        assert_eq!(
            (listed[0].observed.clone(), listed[0].url.clone()),
            (ObservedState::Running, Some(url.clone()))
        );
        assert_eq!(rig.manager.start(&app).unwrap(), url, "重复启动是幂等的");

        rig.manager.stop(&app).unwrap();
        assert_eq!(rig.fetch(&app, "/").status, 404);
        assert_eq!(
            rig.manager.list().unwrap()[0].observed,
            ObservedState::Stopped
        );
        rig.manager.stop(&app).unwrap();

        let events = drain(&mut rx);
        assert_eq!(
            events,
            vec![
                AppEvent::Installed { app: app.clone() },
                AppEvent::StateChanged {
                    app: app.clone(),
                    state: ObservedState::Running
                },
                AppEvent::EndpointChanged {
                    app: app.clone(),
                    url: Some(url)
                },
                AppEvent::StateChanged {
                    app: app.clone(),
                    state: ObservedState::Stopped
                },
                AppEvent::EndpointChanged {
                    app: app.clone(),
                    url: None
                },
            ]
        );
        assert!(
            !format!("{events:?}").contains("bh_token"),
            "事件里不能出现令牌"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn granting_outbound_network_removes_the_csp() {
        let rig = rig().await;
        let src = write_app(
            &rig.src_dir("a"),
            "online",
            "1.0.0",
            "[permissions.network]\noutbound = \"any\"\n",
            "hi",
        );
        rig.install(&src).unwrap();
        rig.manager.start(&id("online")).unwrap();
        let r = rig.fetch(&id("online"), "/");
        assert_eq!(r.status, 200);
        assert!(r.header("content-security-policy").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_or_manifest_changed_after_approval_is_refused_and_leaves_nothing_behind() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        let src = write_app(&dir, "excalidraw", "0.17.0", "", "<h1>good</h1>");
        let approved = rig.approve(&src);

        fs::write(dir.join("web/index.html"), "<h1>evil</h1>").unwrap();
        assert!(matches!(
            rig.manager.install(&approved, &src, 1),
            Err(ManagerError::Verify(VerifyError::SourceChanged))
        ));

        fs::write(dir.join("web/index.html"), "<h1>good</h1>").unwrap();
        fs::write(
            dir.join("manifest.toml"),
            manifest_toml(
                "excalidraw",
                "0.17.0",
                "[permissions]\nclipboard = \"read_write\"\n",
            ),
        )
        .unwrap();
        assert!(matches!(
            rig.manager.install(&approved, &src, 1),
            Err(ManagerError::Verify(VerifyError::ManifestChanged))
        ));

        assert!(rig.manager.list().unwrap().is_empty());
        assert_no_leftovers(&rig);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_approved_payload_with_widened_permissions_is_refused() {
        let rig = rig().await;
        let src = write_app(&rig.src_dir("a"), "excalidraw", "0.17.0", "", "hi");
        let approved = rig.approve(&src);
        let mut json: serde_json::Value = serde_json::to_value(&approved).unwrap();
        json["plan"]["requested"]["clipboard"] = serde_json::json!("read_write");
        let forged: ApprovedInstallPlan = serde_json::from_value(json).unwrap();
        assert!(matches!(
            rig.manager.install(&forged, &src, 1),
            Err(ManagerError::Verify(VerifyError::PlanChanged))
        ));
        assert!(rig.manager.list().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn containers_are_still_unsupported() {
        let rig = rig().await;
        let dir = rig.src_dir("c");
        let digest = "a".repeat(64);
        write_files(
            &dir,
            &[(
                "manifest.toml",
                &manifest_toml("cont", "1.0.0", "").replace(
                    "kind = \"static_web\"\nsource = \"web/\"",
                    &format!(
                        "kind = \"container\"\nimage = \"docker.io/x/y@sha256:{digest}\"\n[runtime.http]\ncontainer_port = 80"
                    ),
                ),
            )],
        );
        match rig.manager.install_plan(
            &AppSource::LocalDir { path: dir },
            Provenance::Local,
            TrustLevel::Trusted,
        ) {
            Err(ManagerError::UnsupportedRuntime(k)) => assert_eq!(k, "container"),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_package_with_a_symlink_is_refused() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        let src = write_app(&dir, "excalidraw", "0.17.0", "", "hi");
        std::os::unix::fs::symlink("/etc/hosts", dir.join("web/leak")).unwrap();
        assert!(matches!(
            rig.manager
                .install_plan(&src, Provenance::Local, TrustLevel::Trusted),
            Err(ManagerError::Io(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_same_version_is_never_overwritten_and_an_upgrade_records_new_grants() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let app = id("excalidraw");
        let v1 = write_app(
            &rig.src_dir("v1"),
            "excalidraw",
            "1.0.0",
            "",
            "<h1>one</h1>",
        );
        rig.install(&v1).unwrap();
        assert!(
            matches!(rig.install(&v1), Err(ManagerError::AlreadyInstalled(v)) if v == Version::new(1, 0, 0))
        );
        drain(&mut rx);

        let v2 = write_app(
            &rig.src_dir("v2"),
            "excalidraw",
            "1.1.0",
            "[permissions]\nclipboard = \"read\"\n",
            "<h1>two</h1>",
        );
        let plan = rig
            .manager
            .install_plan(&v2, Provenance::Local, TrustLevel::Trusted)
            .unwrap();
        assert_eq!(plan.upgrading_from, Some(Version::new(1, 0, 0)));
        assert_eq!(plan.permission_diff.len(), 1, "升级差异是相对当前授予的");
        rig.install(&v2).unwrap();

        let events = drain(&mut rx);
        assert!(matches!(&events[0], AppEvent::Installed { .. }));
        assert!(
            matches!(&events[1], AppEvent::ManifestChanged { permission_changes, .. } if permission_changes.len() == 1)
        );
        let record = rig.manager.registry.load(&app).unwrap().unwrap();
        assert_eq!(record.current_version, Version::new(1, 1, 0));
        assert_eq!(record.versions.len(), 2);
        assert_eq!(record.grants.clipboard, Access::Read);
        let pkg = rig.manager.registry.paths();
        assert!(
            pkg.package_dir(&app, &Version::new(1, 0, 0)).is_dir(),
            "旧版本目录保留"
        );
        rig.manager.start(&app).unwrap();
        assert!(
            rig.fetch(&app, "/").text().contains("two"),
            "启动的是当前版本"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrading_a_running_app_is_refused_until_it_is_stopped() {
        let rig = rig().await;
        let app = id("excalidraw");
        rig.install(&write_app(
            &rig.src_dir("v1"),
            "excalidraw",
            "1.0.0",
            "",
            "one",
        ))
        .unwrap();
        rig.manager.start(&app).unwrap();
        let v2 = write_app(&rig.src_dir("v2"), "excalidraw", "1.1.0", "", "two");
        assert!(matches!(rig.install(&v2), Err(ManagerError::Busy(_))));
        rig.manager.stop(&app).unwrap();
        rig.install(&v2).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_package_without_its_site_directory_fails_to_start_cleanly_and_stop_resets_it() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        write_files(
            &dir,
            &[("manifest.toml", &manifest_toml("hollow", "1.0.0", ""))],
        );
        let src = AppSource::LocalDir { path: dir };
        rig.install(&src).unwrap();
        let app = id("hollow");
        assert!(matches!(
            rig.manager.start(&app),
            Err(ManagerError::MissingSource(_))
        ));
        assert!(matches!(
            rig.manager.list().unwrap()[0].observed,
            ObservedState::Failed {
                retryable: false,
                ..
            }
        ));
        assert!(
            matches!(rig.manager.start(&app), Err(ManagerError::BadState { .. })),
            "不可重试的失败不自动重来"
        );
        rig.manager.stop(&app).unwrap();
        assert_eq!(
            rig.manager.list().unwrap()[0].observed,
            ObservedState::Stopped
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uninstalling_the_program_stops_serving_and_keeps_user_data() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let app = id("excalidraw");
        rig.install(&write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "1.0.0",
            "",
            "hi",
        ))
        .unwrap();
        rig.manager.start(&app).unwrap();
        let data = rig.manager.registry.paths().data_dir(&app);
        fs::write(data.join("drawing.json"), "{}").unwrap();
        drain(&mut rx);

        rig.manager.uninstall(&app, UninstallMode::Program).unwrap();
        assert!(rig.manager.list().unwrap().is_empty());
        assert!(!rig.gateway.has_site(&app));
        assert!(data.join("drawing.json").exists());
        let events = drain(&mut rx);
        assert!(events.contains(&AppEvent::StateChanged {
            app: app.clone(),
            state: ObservedState::NotInstalled
        }));

        rig.manager.uninstall(&app, UninstallMode::Program).unwrap();
        rig.manager
            .uninstall(&app, UninstallMode::ProgramAndData)
            .unwrap();
        assert!(!rig.manager.registry.paths().app_dir(&app).exists());
        rig.manager
            .uninstall(&id("never-installed"), UninstallMode::ProgramAndData)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_a_supervisor_restart_reconcile_restarts_apps_that_wanted_to_run() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (run, idle) = (id("runner"), id("idler"));
        {
            let gw = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
            let m = AppManager::new(&root, HOST, gw.clone()).unwrap();
            for (app, dir) in [(&run, "runner"), (&idle, "idler")] {
                let src = write_app(
                    &tmp.path().join("src").join(dir),
                    app.as_str(),
                    "1.0.0",
                    "",
                    "hello",
                );
                let plan = m
                    .install_plan(&src, Provenance::Local, TrustLevel::Trusted)
                    .unwrap();
                m.install(
                    &plan.approve(Approval {
                        approver: "t".into(),
                        approved_ms: 1,
                    }),
                    &src,
                    1,
                )
                .unwrap();
            }
            m.start(&run).unwrap();
            // "崩溃":不做任何清理,直接丢掉 gateway 与 manager
            drop(m);
            Arc::try_unwrap(gw).ok().unwrap().shutdown().await;
        }
        let gw2 = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let m2 = AppManager::new(&root, HOST, gw2.clone()).unwrap();
        let before = m2.list().unwrap();
        let stale = before.iter().find(|a| a.id == run).unwrap();
        assert_eq!(
            stale.observed,
            ObservedState::Running,
            "磁盘上是崩溃前的陈旧观察态"
        );
        assert!(!gw2.has_site(&run), "但新的 gateway 里没有站点");

        assert!(m2.reconcile().is_clean());
        assert!(gw2.has_site(&run), "desired=Running 的应用被重新启动");
        assert!(!gw2.has_site(&idle), "desired=Stopped 的保持停止");
        let after = m2.list().unwrap();
        assert_eq!(
            after.iter().find(|a| a.id == run).unwrap().observed,
            ObservedState::Running
        );
        assert_eq!(
            after.iter().find(|a| a.id == idle).unwrap().observed,
            ObservedState::Installed
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_reports_a_broken_app_without_blocking_the_others() {
        let rig = rig().await;
        let (good, bad) = (id("good"), id("bad"));
        rig.install(&write_app(&rig.src_dir("good"), "good", "1.0.0", "", "ok"))
            .unwrap();
        rig.install(&write_app(&rig.src_dir("bad"), "bad", "1.0.0", "", "ok"))
            .unwrap();
        rig.manager.start(&good).unwrap();
        rig.manager.start(&bad).unwrap();
        // 模拟崩溃后:坏应用的站点目录被删,两个应用都停在"想运行"
        rig.gateway.remove_site(&good);
        rig.gateway.remove_site(&bad);
        let pkg = rig
            .manager
            .registry
            .paths()
            .package_dir(&bad, &Version::new(1, 0, 0));
        fs::remove_dir_all(pkg.join("web")).unwrap();

        let failures = rig.manager.reconcile().failures;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, bad);
        assert!(matches!(failures[0].1, ManagerError::MissingSource(_)));
        assert!(rig.gateway.has_site(&good));
        assert!(!rig.gateway.has_site(&bad));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listing_falls_back_to_the_id_when_the_manifest_cannot_be_read() {
        let rig = rig().await;
        let app = id("excalidraw");
        rig.install(&write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "1.0.0",
            "",
            "hi",
        ))
        .unwrap();
        // 清单从**当前版本的包**里读(包里本来就拷着 manifest.toml),不再依赖一份共享的应用级文件
        fs::write(
            rig.manager
                .registry
                .paths()
                .package_dir(&app, &Version::new(1, 0, 0))
                .join("manifest.toml"),
            "not toml {{{",
        )
        .unwrap();
        assert_eq!(rig.manager.list().unwrap()[0].name, "excalidraw");
    }

    /// 安装的每个失败出口(摘要对不上、同版本、应用在运行)都不能留下 staging;而且保存下来的 manifest
    /// 是 staging 副本里被 verify 过的那份。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_failed_install_cleans_up_and_the_saved_manifest_is_the_staged_one() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        let src = write_app(&dir, "excalidraw", "1.0.0", "", "one");
        rig.install(&src).unwrap();
        // 清单就在落位的包里(staging 副本被 verify 过的那份);不再另存一份应用级的 manifest.toml
        let saved = fs::read_to_string(
            rig.manager
                .registry
                .paths()
                .package_dir(&id("excalidraw"), &Version::new(1, 0, 0))
                .join("manifest.toml"),
        )
        .unwrap();
        assert!(
            !rig.manager
                .registry
                .paths()
                .manifest_path(&id("excalidraw"))
                .exists()
        );
        assert_eq!(
            saved,
            fs::read_to_string(dir.join("manifest.toml")).unwrap()
        );

        assert!(matches!(
            rig.install(&src),
            Err(ManagerError::AlreadyInstalled(_))
        ));
        assert_no_leftovers(&rig);

        rig.manager.start(&id("excalidraw")).unwrap();
        let v2 = write_app(&rig.src_dir("v2"), "excalidraw", "1.1.0", "", "two");
        assert!(matches!(rig.install(&v2), Err(ManagerError::Busy(_))));
        assert_no_leftovers(&rig);

        let unsupported = rig.src_dir("py");
        write_files(
            &unsupported,
            &[(
                "manifest.toml",
                &manifest_toml("py", "1.0.0", "").replace(
                    "kind = \"static_web\"\nsource = \"web/\"",
                    "kind = \"python\"\ncommand = [\"p\"]\n[runtime.http]\nport_env = \"PORT\"",
                ),
            )],
        );
        assert!(
            rig.manager
                .install(
                    &rig.approve(&src),
                    &AppSource::LocalDir { path: unsupported },
                    1
                )
                .is_err()
        );
        assert_no_leftovers(&rig);
    }

    /// 落位(rename)之后的任何一步失败,都要把刚落位的包目录撤回——否则重试永远撞上"目录已存在"。
    /// 这里用"`data` 被一个同名文件占着"让 `registry.save` 在 rename 之后失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failure_after_the_rename_rolls_the_package_back_so_a_retry_works() {
        let rig = rig().await;
        let app = id("excalidraw");
        let src = write_app(&rig.src_dir("a"), "excalidraw", "1.0.0", "", "hi");
        let blocker = rig.manager.registry.paths().data_dir(&app);
        fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        fs::write(&blocker, "i am a file, not a directory").unwrap();

        assert!(matches!(rig.install(&src), Err(ManagerError::Io(_))));
        let pkg = rig
            .manager
            .registry
            .paths()
            .package_dir(&app, &Version::new(1, 0, 0));
        assert!(!pkg.exists(), "失败后包目录必须撤回");
        assert!(!rig.manager.registry.paths().state_path(&app).exists());
        assert_no_leftovers(&rig);

        fs::remove_file(&blocker).unwrap();
        rig.install(&src).expect("障碍移走后重试必须成功");
        assert_eq!(rig.manager.list().unwrap().len(), 1);
    }

    /// 上一次崩溃留下的"包目录存在但没有版本记录"的残骸,不能挡住重装。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debris_from_a_crashed_install_never_blocks_a_retry_or_an_upgrade() {
        let rig = rig().await;
        let app = id("excalidraw");
        // 全新安装:残骸目录已经在了
        let debris = rig
            .manager
            .registry
            .paths()
            .package_dir(&app, &Version::new(1, 0, 0));
        write_files(&debris, &[("junk.txt", "left over")]);
        rig.install(&write_app(
            &rig.src_dir("v1"),
            "excalidraw",
            "1.0.0",
            "",
            "one",
        ))
        .unwrap();
        assert!(!debris.join("junk.txt").exists(), "残骸被清掉,落位的是新包");
        assert!(debris.join("web/index.html").is_file());

        // 升级:新版本目录的残骸
        let debris2 = rig
            .manager
            .registry
            .paths()
            .package_dir(&app, &Version::new(1, 1, 0));
        write_files(&debris2, &[("junk.txt", "left over")]);
        rig.install(&write_app(
            &rig.src_dir("v2"),
            "excalidraw",
            "1.1.0",
            "",
            "two",
        ))
        .unwrap();
        assert!(!debris2.join("junk.txt").exists());
        rig.manager.start(&app).unwrap();
        assert!(rig.fetch(&app, "/").text().contains("two"));
    }

    /// supervisor 退出:站点撤下、观察态落成 Stopped,但 `desired` 保持 Running,下次 `reconcile` 把它们拉起来。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspending_keeps_what_the_user_wanted_so_the_next_start_restores_it() {
        let rig = rig().await;
        let (a, b) = (id("alpha"), id("beta"));
        rig.install(&write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A"))
            .unwrap();
        rig.install(&write_app(&rig.src_dir("b"), "beta", "1.0.0", "", "B"))
            .unwrap();
        rig.manager.start(&a).unwrap();
        rig.manager.start(&b).unwrap();
        rig.manager.stop(&b).unwrap(); // beta:用户想要停止

        assert!(rig.manager.suspend_all().is_clean());
        assert!(!rig.gateway.has_site(&a), "站点已撤下");
        let after = rig.manager.list().unwrap();
        let alpha = after.iter().find(|x| x.id == a).unwrap();
        assert_eq!(
            (alpha.desired, alpha.observed.clone()),
            (DesiredState::Running, ObservedState::Stopped)
        );
        let beta = after.iter().find(|x| x.id == b).unwrap();
        assert_eq!(
            (beta.desired, beta.observed.clone()),
            (DesiredState::Stopped, ObservedState::Stopped)
        );

        assert!(rig.manager.reconcile().is_clean());
        assert!(rig.gateway.has_site(&a), "desired=Running 的被重新拉起");
        assert!(!rig.gateway.has_site(&b));
    }

    /// `suspend_all` 之后到下一次 `reconcile` 之前,manager 不再接受会产生新站点/新包的操作(安装、启动),
    /// 否则一个恰好排在锁后面的 `Start` 会在撤站点之后又把站点注册回一个已经停掉的 gateway。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_suspend_all_installs_and_starts_are_refused_until_the_next_reconcile() {
        let rig = rig().await;
        let a = id("alpha");
        rig.install(&write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A"))
            .unwrap();
        rig.manager.start(&a).unwrap();
        assert!(rig.manager.suspend_all().is_clean());

        assert!(matches!(
            rig.manager.start(&a),
            Err(ManagerError::ShuttingDown)
        ));
        assert!(
            !rig.gateway.has_site(&a),
            "被拒绝的 start 不能把站点注册回来"
        );
        let b = write_app(&rig.src_dir("b"), "beta", "1.0.0", "", "B");
        let approved = rig.approve(&b);
        assert!(matches!(
            rig.manager.install(&approved, &b, 1),
            Err(ManagerError::ShuttingDown)
        ));
        assert_no_leftovers(&rig);
        // 停止与卸载照常允许(清理类操作)
        rig.manager.stop(&a).unwrap();

        assert!(
            rig.manager.reconcile().is_clean(),
            "reconcile 代表 supervisor 重新开始工作"
        );
        rig.manager.start(&a).unwrap();
        rig.install(&b).unwrap();
    }

    /// 慢的部分——拷贝**和对整棵 staging 树算摘要**——必须发生在拿 manager 锁**之前**:否则 `suspend_all`
    /// (dozerd 退出收尾)要一直等到一次大目录安装的哈希结束(macOS 上拷贝是 clone,哈希才是大头)。
    /// 做法:测试线程先占住锁,另一个线程调 `install`——它应该已经拷完、算完摘要(`prepared` 计数加一),
    /// 正卡在拿锁上。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_slow_copy_and_hashing_happen_before_the_manager_lock_is_taken() {
        let rig = rig().await;
        let src = write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A");
        let approved = rig.approve(&src);
        let apps_dir = rig.manager.registry.paths().apps_dir();
        let guard = rig.manager.lock.lock().unwrap();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| rig.manager.install(&approved, &src, 1));
            let started = std::time::Instant::now();
            let mut prepared = false;
            while started.elapsed() < std::time::Duration::from_secs(5) {
                if rig
                    .manager
                    .prepared
                    .load(std::sync::atomic::Ordering::SeqCst)
                    >= 1
                {
                    prepared = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // 此刻锁还被测试线程占着:staging 里应该已经有完整的拷贝
            let staged_manifest = fs::read_dir(&apps_dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".staging-")
                })
                .map(|p| p.join("manifest.toml").is_file());
            drop(guard);
            assert!(prepared, "install 在锁被占着时也应该先完成拷贝与摘要计算");
            assert_eq!(staged_manifest, Some(true));
            handle.join().unwrap().unwrap();
        });
        assert_eq!(rig.manager.list().unwrap().len(), 1);
        assert_no_leftovers(&rig);
    }

    /// `LocalDir.path` 必须是绝对路径:相对路径会按 dozerd 的工作目录解析,调用方并不知道那是哪里。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relative_source_paths_are_refused_before_anything_is_read() {
        let rig = rig().await;
        let real = write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A");
        let approved = rig.approve(&real);
        let relative = AppSource::LocalDir {
            path: PathBuf::from("relative/dir"),
        };
        assert!(matches!(
            rig.manager
                .install_plan(&relative, Provenance::Local, TrustLevel::Trusted),
            Err(ManagerError::BadSource(_))
        ));
        assert!(matches!(
            rig.manager.install(&approved, &relative, 1),
            Err(ManagerError::BadSource(_))
        ));
        assert_no_leftovers(&rig);
    }

    /// 崩溃时留下的 `.staging-*` 目录在 manager 启动时清掉(单写者:此刻没有别的安装在进行)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leftover_staging_directories_are_swept_when_the_manager_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let apps = root.join("apps");
        write_files(&apps.join(".staging-dead"), &[("junk.txt", "x")]);
        write_files(&apps.join("keepme"), &[("data/file.txt", "user data")]);
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let _manager = AppManager::new(&root, HOST, gateway).unwrap();
        assert!(!apps.join(".staging-dead").exists());
        assert!(
            apps.join("keepme/data/file.txt").exists(),
            "别的目录不受影响"
        );
    }

    /// 损坏的 `state.json` 不能被 `reconcile` 悄悄忽略,也不能拖住别的应用;读不出来的记录单独报告。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_reports_corrupt_records_and_still_restores_the_others() {
        let rig = rig().await;
        let good = id("good");
        rig.install(&write_app(&rig.src_dir("good"), "good", "1.0.0", "", "ok"))
            .unwrap();
        rig.manager.start(&good).unwrap();
        rig.gateway.remove_site(&good); // 模拟 supervisor 重启
        let broken = rig.manager.registry.paths().apps_dir().join("broken");
        write_files(&broken, &[("state.json", "{not json")]);

        let report = rig.manager.reconcile();
        assert!(report.failures.is_empty());
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert_eq!(report.problems[0].0, "broken");
        assert!(!report.is_clean());
        assert!(rig.gateway.has_site(&good), "好应用照常恢复");
    }

    /// 持锁线程 panic 会毒化 manager 的锁;之后每个加锁的调用都不能跟着 panic。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_poisoned_manager_lock_does_not_make_every_later_call_panic() {
        let rig = rig().await;
        let a = id("alpha");
        rig.install(&write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A"))
            .unwrap();
        let _ = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _g = rig.manager.lock.lock().unwrap();
                    panic!("故意毒化");
                })
                .join()
        });
        assert!(rig.manager.lock.is_poisoned());
        rig.manager.start(&a).unwrap();
        rig.manager.stop(&a).unwrap();
        assert!(rig.manager.reconcile().is_clean());
    }

    /// 带令牌的启动地址:在锁内判断"是否在运行"并构造,只给运行中的应用。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_launch_url_is_only_given_for_a_running_app() {
        let rig = rig().await;
        let a = id("alpha");
        rig.install(&write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A"))
            .unwrap();
        assert!(matches!(
            rig.manager.launch_url_if_running(&a),
            Err(ManagerError::BadState { .. })
        ));
        assert!(matches!(
            rig.manager.launch_url_if_running(&id("ghost")),
            Err(ManagerError::NotInstalled(_))
        ));
        rig.manager.start(&a).unwrap();
        let url = rig.manager.launch_url_if_running(&a).unwrap();
        assert!(
            url.starts_with("http://alpha.localhost:") && url.contains("bh_token="),
            "{url}"
        );
        rig.manager.stop(&a).unwrap();
        assert!(rig.manager.launch_url_if_running(&a).is_err());
    }

    /// `suspend_all` 撤站点时也要发 `EndpointChanged { url: None }`(与 `stop` 一致),订阅方才知道地址没了。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspending_announces_that_the_endpoints_are_gone() {
        let rig = rig().await;
        let a = id("alpha");
        rig.install(&write_app(&rig.src_dir("a"), "alpha", "1.0.0", "", "A"))
            .unwrap();
        rig.manager.start(&a).unwrap();
        let mut rx = rig.manager.events();
        assert!(rig.manager.suspend_all().is_clean());
        let events = drain(&mut rx);
        assert!(
            events.contains(&AppEvent::EndpointChanged {
                app: a.clone(),
                url: None
            }),
            "{events:?}"
        );
        assert!(events.contains(&AppEvent::StateChanged {
            app: a,
            state: ObservedState::Stopped
        }));
    }

    // ===== A6c Task 5:进程型应用 =====

    #[cfg(unix)]
    fn have(program: &str) -> bool {
        SystemResolver::with_dirs(dirs_for(program))
            .resolve(program)
            .is_ok()
    }

    #[cfg(unix)]
    fn dirs_for(program: &str) -> Vec<PathBuf> {
        for d in ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"] {
            if Path::new(d).join(program).is_file() {
                return vec![PathBuf::from(d)];
            }
        }
        Vec::new()
    }

    /// 在 `dir` 下写一个进程型(python)应用,`server_py` 是被 `python3` 执行的内容。
    #[cfg(unix)]
    fn write_py_app(dir: &Path, id: &str, version: &str, server_py: &str) -> AppSource {
        let manifest = manifest_toml(id, version, "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            "kind = \"python\"\ncommand = [\"python3\", \"server.py\"]\n[runtime.http]\nport_env = \"APP_PORT\"",
        );
        write_files(
            dir,
            &[("manifest.toml", &manifest), ("server.py", server_py)],
        );
        AppSource::LocalDir {
            path: dir.to_path_buf(),
        }
    }

    const PY_SERVER: &str = "import os, http.server\n\
        http.server.test(HandlerClass=http.server.SimpleHTTPRequestHandler, port=int(os.environ[\"APP_PORT\"]), bind=\"127.0.0.1\")\n";

    #[cfg(unix)]
    fn observed_of(rig: &Rig, id: &AppId) -> Option<ObservedState> {
        rig.manager
            .list()
            .ok()?
            .into_iter()
            .find(|s| &s.id == id)
            .map(|s| s.observed)
    }

    /// 轮询直到观察态满足 `pred`(最多 `secs` 秒)。
    #[cfg(unix)]
    fn wait_for<F: Fn(&ObservedState) -> bool>(
        rig: &Rig,
        id: &AppId,
        secs: u64,
        pred: F,
    ) -> ObservedState {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(o) = observed_of(rig, id)
                && pred(&o)
            {
                return o;
            }
            if std::time::Instant::now() >= deadline {
                panic!("等待超时:{id} 停在 {:?}", observed_of(rig, id));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    fn is_running(o: &ObservedState) -> bool {
        matches!(o, ObservedState::Running)
    }

    #[cfg(unix)]
    fn is_terminal_failure(o: &ObservedState) -> bool {
        matches!(
            o,
            ObservedState::Failed {
                retryable: false,
                ..
            }
        )
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_python_app_installs_starts_runs_behind_the_gateway_and_stops() {
        if !have("python3") {
            return;
        }
        let rig = rig().await;
        let a = id("pyapp");
        let src = write_py_app(&rig.src_dir("a"), "pyapp", "1.0.0", PY_SERVER);
        let mut rx = rig.manager.events();
        rig.install(&src).unwrap();
        let url = rig.manager.start(&a).unwrap();
        assert!(url.starts_with("http://pyapp.localhost:"), "{url}");
        wait_for(&rig, &a, 15, is_running);
        assert_eq!(rig.fetch(&a, "/").status, 200);

        let events = drain(&mut rx);
        assert!(
            events.contains(&AppEvent::StateChanged {
                app: a.clone(),
                state: ObservedState::Starting
            }),
            "{events:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            AppEvent::StateChanged {
                state: ObservedState::Running,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AppEvent::EndpointChanged { url: Some(_), .. }))
        );

        rig.manager.stop(&a).unwrap();
        assert_eq!(observed_of(&rig, &a), Some(ObservedState::Stopped));
        assert_eq!(rig.fetch(&a, "/").status, 404);
        let events = drain(&mut rx);
        assert!(events.contains(&AppEvent::EndpointChanged { app: a, url: None }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plans_for_process_apps_reject_reserved_port_names_and_foreign_commands_and_missing_lockfiles()
     {
        let rig = rig().await;
        // 保留名作端口变量
        let d1 = rig.src_dir("p1");
        let m1 = manifest_toml("p1", "1.0.0", "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            "kind = \"python\"\ncommand = [\"python3\", \"server.py\"]\n[runtime.http]\nport_env = \"NODE_OPTIONS\"",
        );
        write_files(&d1, &[("manifest.toml", &m1), ("server.py", "")]);
        let e1 = rig
            .manager
            .install_plan(
                &AppSource::LocalDir { path: d1 },
                Provenance::Local,
                TrustLevel::Trusted,
            )
            .unwrap_err();
        assert!(matches!(e1, ManagerError::BadSource(_)), "{e1:?}");
        assert_eq!(e1.kind(), crate::proto::AppErrorKind::Rejected);

        // 外来命令(argv[0] 不是该运行时的解释器)
        let d2 = rig.src_dir("p2");
        let m2 = manifest_toml("p2", "1.0.0", "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            "kind = \"python\"\ncommand = [\"sh\", \"-c\", \"x\"]\n[runtime.http]\nport_env = \"APP_PORT\"",
        );
        write_files(&d2, &[("manifest.toml", &m2)]);
        assert!(matches!(
            rig.manager.install_plan(
                &AppSource::LocalDir { path: d2 },
                Provenance::Local,
                TrustLevel::Trusted
            ),
            Err(ManagerError::BadSource(_))
        ));

        // 声明了 lockfile 但包里没有
        let d3 = rig.src_dir("p3");
        let m3 = manifest_toml("p3", "1.0.0", "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            "kind = \"node\"\ncommand = [\"node\", \"server.js\"]\nlockfile = \"package-lock.json\"\n[runtime.http]\nport_env = \"APP_PORT\"",
        );
        write_files(&d3, &[("manifest.toml", &m3), ("server.js", "")]);
        assert!(matches!(
            rig.manager.install_plan(
                &AppSource::LocalDir { path: d3 },
                Provenance::Local,
                TrustLevel::Trusted
            ),
            Err(ManagerError::BadSource(_))
        ));

        // 合法进程型清单:enforcement 里没有任何 Enforced(全是 Advisory)
        if have("python3") {
            let d4 = rig.src_dir("p4");
            let src = write_py_app(&d4, "p4", "1.0.0", PY_SERVER);
            let plan = rig
                .manager
                .install_plan(&src, Provenance::Local, TrustLevel::Trusted)
                .unwrap();
            assert!(
                plan.enforcement
                    .iter()
                    .all(|e| e.enforcement != crate::permissions::Enforcement::Enforced),
                "{:?}",
                plan.enforcement
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_runtime_fails_the_start_with_a_retryable_failure_and_an_event() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let manager = AppManager::with_resolver(
            tmp.path().join("bytehost"),
            HOST,
            gateway,
            Arc::new(SystemResolver::with_dirs(vec![])),
        )
        .unwrap();
        let src = write_py_app(&tmp.path().join("src"), "pyapp", "1.0.0", PY_SERVER);
        let approved = manager
            .install_plan(&src, Provenance::Local, TrustLevel::Trusted)
            .unwrap()
            .approve(Approval {
                approver: "test".into(),
                approved_ms: 1,
            });
        manager.install(&approved, &src, 100).unwrap();
        let mut rx = manager.events();
        let a = id("pyapp");
        assert!(matches!(
            manager.start(&a),
            Err(ManagerError::RuntimeUnavailable(name)) if name == "python3"
        ));
        let summary = manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert!(
            matches!(
                summary.observed,
                ObservedState::Failed {
                    retryable: true,
                    ..
                }
            ),
            "{:?}",
            summary.observed
        );
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                AppEvent::RuntimeUnavailable {
                    reason: RuntimeReason::NotInstalled { runtime },
                    ..
                } if runtime == "python3"
            )),
            "{events:?}"
        );
    }

    // ===== A6e Task 2:运行时版本要求 =====

    #[cfg(unix)]
    struct FakeVersionProbe {
        output: Mutex<HashMap<PathBuf, String>>,
        calls: Mutex<Vec<PathBuf>>,
    }

    #[cfg(unix)]
    impl FakeVersionProbe {
        fn new() -> Self {
            Self {
                output: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn expects(self, program: &Path, output: &str) -> Arc<Self> {
            self.output
                .lock()
                .unwrap()
                .insert(program.to_path_buf(), output.to_string());
            Arc::new(self)
        }
    }

    #[cfg(unix)]
    impl VersionProbe for FakeVersionProbe {
        fn version_output(&self, program: &Path) -> Option<String> {
            self.calls.lock().unwrap().push(program.to_path_buf());
            self.output.lock().unwrap().get(program).cloned()
        }
    }

    #[cfg(unix)]
    fn rig_with_probe(
        tmp: &tempfile::TempDir,
        gateway: Arc<Gateway>,
        dirs: Vec<PathBuf>,
        probe: Arc<dyn VersionProbe>,
    ) -> Rig {
        let manager = AppManager::with_parts_for_test(
            tmp.path().join("bytehost"),
            HOST,
            gateway.clone(),
            Arc::new(SystemResolver::with_dirs(dirs)),
            crate::process::restart::RestartPolicy::default(),
            probe,
        )
        .unwrap();
        Rig {
            tmp: tempfile::tempdir().unwrap(),
            gateway,
            manager,
        }
    }

    /// 写一个带 `python = "<req>"` 要求的 python 应用。
    #[cfg(unix)]
    fn write_py_app_with_req(dir: &Path, id: &str, req: &str) -> AppSource {
        let manifest = manifest_toml(id, "1.0.0", "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            &format!(
                "kind = \"python\"\npython = \"{req}\"\ncommand = [\"python3\", \"server.py\"]\n[runtime.http]\nport_env = \"APP_PORT\""
            ),
        );
        write_files(
            dir,
            &[("manifest.toml", &manifest), ("server.py", PY_SERVER)],
        );
        AppSource::LocalDir {
            path: dir.to_path_buf(),
        }
    }

    /// 写一个带 `node = "<req>"` 要求的 node 应用(argv[0] 是 `node`)。
    #[cfg(unix)]
    fn write_node_app_with_req(dir: &Path, id: &str, req: &str) -> AppSource {
        let manifest = manifest_toml(id, "1.0.0", "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            &format!(
                "kind = \"node\"\nnode = \"{req}\"\ncommand = [\"node\", \"server.js\"]\n[runtime.http]\nport_env = \"APP_PORT\""
            ),
        );
        write_files(
            dir,
            &[("manifest.toml", &manifest), ("server.js", "// noop")],
        );
        AppSource::LocalDir {
            path: dir.to_path_buf(),
        }
    }

    #[cfg(unix)]
    fn install_with(
        manager: &AppManager,
        dir: &Path,
        id: &str,
        writer: fn(&Path, &str, &str) -> AppSource,
        req: &str,
    ) {
        let src = writer(dir, id, req);
        let approved = manager
            .install_plan(&src, Provenance::Local, TrustLevel::Trusted)
            .unwrap()
            .approve(Approval {
                approver: "test".into(),
                approved_ms: 1,
            });
        manager.install(&approved, &src, 100).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_satisfied_python_version_requirement_lets_the_app_run() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let programs = dirs_for("python3");
        if programs.is_empty() {
            return;
        }
        let probe = FakeVersionProbe::new().expects(&programs[0].join("python3"), "3.12.4\n");
        let rig = rig_with_probe(&tmp, gateway, programs, probe.clone());
        let a = id("pyver");
        install_with(
            &rig.manager,
            &rig.src_dir("a"),
            "pyver",
            write_py_app_with_req,
            ">=3.12",
        );
        assert!(rig.manager.start(&a).is_ok());
        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert!(summary.issue.is_none(), "{:?}", summary.issue);
        assert!(!probe.calls.lock().unwrap().is_empty(), "应真的探过版本");
    }

    /// `--version` 最多阻塞 3s,不能占着单写者锁(否则这期间停止/安装/卸载全被拖住)。
    /// 测试线程先占住锁,另一线程调 `start`:探针应已被调用过(探测在拿锁之前),`start` 正卡在拿锁上。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_version_probe_runs_before_the_manager_lock_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let programs = dirs_for("python3");
        if programs.is_empty() {
            return;
        }
        let probe = FakeVersionProbe::new().expects(&programs[0].join("python3"), "3.12.4\n");
        let rig = rig_with_probe(&tmp, gateway, programs, probe.clone());
        let a = id("lockfree");
        install_with(
            &rig.manager,
            &rig.src_dir("a"),
            "lockfree",
            write_py_app_with_req,
            ">=3.12",
        );
        let guard = rig.manager.lock.lock().unwrap();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| rig.manager.start(&a));
            let started = std::time::Instant::now();
            let mut probed = false;
            while started.elapsed() < std::time::Duration::from_secs(5) {
                if !probe.calls.lock().unwrap().is_empty() {
                    probed = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(!handle.is_finished(), "start 应卡在拿锁上");
            drop(guard);
            assert!(probed, "探测应发生在拿锁之前");
            assert!(handle.join().unwrap().is_ok());
        });
        assert_eq!(
            probe.calls.lock().unwrap().len(),
            1,
            "预探的结果应被锁内检查取用,不重复探"
        );
        assert!(rig.manager.prefetched_versions.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_violated_python_version_requirement_fails_the_start_with_an_issue_and_event() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let programs = dirs_for("python3");
        if programs.is_empty() {
            return;
        }
        let probe = FakeVersionProbe::new().expects(&programs[0].join("python3"), "3.9.1\n");
        let rig = rig_with_probe(&tmp, gateway, programs, probe);
        let a = id("oldpy");
        install_with(
            &rig.manager,
            &rig.src_dir("a"),
            "oldpy",
            write_py_app_with_req,
            ">=3.12",
        );
        let mut rx = rig.manager.events();
        let e = rig.manager.start(&a).unwrap_err();
        assert!(
            matches!(
                e,
                ManagerError::RuntimeVersion {
                    ref runtime,
                    ref required,
                    ref found,
                } if runtime == "python3" && required == ">=3.12" && found == "3.9.1"
            ),
            "{e:?}"
        );
        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert!(
            matches!(
                summary.observed,
                ObservedState::Failed {
                    retryable: true,
                    ..
                }
            ),
            "{:?}",
            summary.observed
        );
        assert!(
            matches!(
                summary.issue,
                Some(AppIssue::RuntimeVersion {
                    ref runtime,
                    ref required,
                    ref found,
                }) if runtime == "python3" && required == ">=3.12" && found == "3.9.1"
            ),
            "{:?}",
            summary.issue
        );
        assert!(
            drain(&mut rx).iter().any(|e| matches!(
                e,
                AppEvent::RuntimeUnavailable {
                    reason: RuntimeReason::Unsatisfied { runtime, required, found },
                    ..
                } if runtime == "python3" && required == ">=3.12" && found == "3.9.1"
            )),
            "应发 Unsatisfied 事件"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unparsable_version_output_counts_as_unsatisfied_and_names_the_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let programs = dirs_for("python3");
        if programs.is_empty() {
            return;
        }
        let probe = FakeVersionProbe::new()
            .expects(&programs[0].join("python3"), "command not found\nmore\n");
        let rig = rig_with_probe(&tmp, gateway, programs, probe);
        let a = id("weird");
        install_with(
            &rig.manager,
            &rig.src_dir("a"),
            "weird",
            write_py_app_with_req,
            ">=3.12",
        );
        let e = rig.manager.start(&a).unwrap_err();
        assert!(
            matches!(
                e,
                ManagerError::RuntimeVersion { ref found, .. } if found.starts_with("无法识别: command not found")
            ),
            "{e:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_uv_managed_python_app_skips_the_version_check() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let programs = dirs_for("uv");
        if programs.is_empty() {
            return;
        }
        let probe = FakeVersionProbe::new().expects(&programs[0].join("uv"), "3.9.1\n");
        let rig = rig_with_probe(&tmp, gateway, programs.clone(), probe.clone());
        let a = id("uvapp");
        let manifest = manifest_toml("uvapp", "1.0.0", "").replace(
            "kind = \"static_web\"\nsource = \"web/\"",
            "kind = \"python\"\npython = \">=3.12\"\ncommand = [\"uv\", \"run\", \"server.py\"]\n[runtime.http]\nport_env = \"APP_PORT\"",
        );
        write_files(
            &rig.src_dir("a"),
            &[("manifest.toml", &manifest), ("server.py", "")],
        );
        let src = AppSource::LocalDir {
            path: rig.src_dir("a"),
        };
        let approved = rig
            .manager
            .install_plan(&src, Provenance::Local, TrustLevel::Trusted)
            .unwrap()
            .approve(Approval {
                approver: "test".into(),
                approved_ms: 1,
            });
        rig.manager.install(&approved, &src, 100).unwrap();
        // uv 存在且版本"过低",但因为是 uv 启动,不做版本检查 → 不因版本报错
        let err = rig.manager.start(&a).unwrap_err();
        assert!(
            !matches!(err, ManagerError::RuntimeVersion { .. }),
            "uv 应用不该做版本检查:{err:?}"
        );
        assert!(probe.calls.lock().unwrap().is_empty(), "不该探版本");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_app_version_requirement_checks_the_node_interpreter() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let programs = dirs_for("node");
        if programs.is_empty() {
            return;
        }
        let probe = FakeVersionProbe::new().expects(&programs[0].join("node"), "v20.1.0\n");
        let rig = rig_with_probe(&tmp, gateway, programs, probe);
        let a = id("nodeapp");
        install_with(
            &rig.manager,
            &rig.src_dir("a"),
            "nodeapp",
            write_node_app_with_req,
            ">=22",
        );
        let e = rig.manager.start(&a).unwrap_err();
        assert!(
            matches!(
                e,
                ManagerError::RuntimeVersion { ref runtime, ref found, .. } if runtime == "node" && found == "20.1.0"
            ),
            "{e:?}"
        );
        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert!(
            matches!(
                summary.issue,
                Some(AppIssue::RuntimeVersion { ref runtime, .. }) if runtime == "node"
            ),
            "{:?}",
            summary.issue
        );
    }

    // ===== A6f Task 3:依赖安装失败的问题页 =====

    /// 依赖安装失败:`install_failed` 先记 issue 再置 `Failed{retryable:true}`,
    /// `list()` 里因此一定同时看得到两者;装配成功(`ready`)后 issue 清除。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_failed_records_the_issue_and_a_later_success_clears_it() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let src = write_app(&rig.src_dir("a"), "excalidraw", "0.17.0", "", "<h1>x</h1>");
        rig.install(&src).unwrap();
        let a = id("excalidraw");

        let tr = AppTransitions {
            core: rig.manager.core.clone(),
            id: a.clone(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert!(tr.install_failed("npm ci 失败(退出码 1)".into()));

        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert_eq!(
            summary.observed,
            ObservedState::Failed {
                reason: "npm ci 失败(退出码 1)".into(),
                retryable: true,
            }
        );
        assert_eq!(
            summary.issue,
            Some(AppIssue::DependencyInstall {
                summary: "npm ci 失败(退出码 1)".into(),
            })
        );
        // 记 issue 先于置状态:重拉时事件里 already 有 Failed。
        assert!(drain(&mut rx).contains(&AppEvent::StateChanged {
            app: a.clone(),
            state: ObservedState::Failed {
                reason: "npm ci 失败(退出码 1)".into(),
                retryable: true,
            },
        }));

        // 重试成功(装配好):issue 清除,状态回到 Running。
        assert!(tr.ready(3000));
        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert_eq!(summary.observed, ObservedState::Running);
        assert!(summary.issue.is_none(), "{:?}", summary.issue);
    }

    /// `list()` 不持单写者锁,GUI 因推送重拉时可能与 `install_failed` 并发:读到 `Failed` 就一定要
    /// 同时带着 issue,所以 issue 必须先于状态落地。确定性地钉住:测试线程攥住 issue 表的锁,
    /// `install_failed` 卡在"记 issue"这一步时,注册表里的状态还不能是 `Failed`。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_issue_is_recorded_before_the_failed_state_becomes_visible() {
        let rig = rig().await;
        let src = write_app(&rig.src_dir("a"), "excalidraw", "0.17.0", "", "<h1>x</h1>");
        rig.install(&src).unwrap();
        let a = id("excalidraw");
        let tr = AppTransitions {
            core: rig.manager.core.clone(),
            id: a.clone(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let held = rig.manager.core.issues.lock().unwrap();
        let worker = std::thread::spawn(move || tr.install_failed("npm ci 失败".into()));
        std::thread::sleep(Duration::from_millis(300));
        let during = rig.manager.core.load_record(&a).unwrap().observed;
        assert!(
            !matches!(during, ObservedState::Failed { .. }),
            "issue 还没记下,状态不该已是 Failed:{during:?}"
        );
        drop(held);
        assert!(worker.join().unwrap());
        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert!(matches!(summary.observed, ObservedState::Failed { .. }));
        assert!(summary.issue.is_some());
    }

    /// issue 只在 `Failed` 时带出:装依赖失败后即使还没重试,只要不是 `Failed`(这里用
    /// `set_observed(Stopped)` 模拟)就不带 issue。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_dependency_install_issue_is_only_exposed_while_failed() {
        let rig = rig().await;
        let src = write_app(&rig.src_dir("a"), "excalidraw", "0.17.0", "", "<h1>x</h1>");
        rig.install(&src).unwrap();
        let a = id("excalidraw");
        let tr = AppTransitions {
            core: rig.manager.core.clone(),
            id: a.clone(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert!(tr.install_failed("uv sync 失败".into()));

        let stopped = ObservedState::Stopped;
        {
            let _g = rig.manager.guard();
            let mut record = rig.manager.load_record(&a).unwrap();
            rig.manager
                .set_observed(&mut record, stopped.clone())
                .unwrap();
        }
        let summary = rig
            .manager
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == a)
            .unwrap();
        assert_eq!(summary.observed, stopped);
        assert!(summary.issue.is_none(), "{:?}", summary.issue);
    }

    // ===== A6e Task 3:日志末尾 =====

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn logs_for_an_uninstalled_app_are_not_found_and_for_a_never_run_app_are_empty() {
        let rig = rig().await;
        assert!(matches!(
            rig.manager.logs(&id("nope"), 100),
            Err(ManagerError::NotInstalled(_))
        ));
        let src = write_app(&rig.src_dir("a"), "excalidraw", "0.17.0", "", "<h1>x</h1>");
        rig.install(&src).unwrap();
        let (text, truncated) = rig.manager.logs(&id("excalidraw"), 100).unwrap();
        assert_eq!(text, "");
        assert!(!truncated);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn logs_return_the_tail_of_the_apps_log_file() {
        if !have("python3") {
            return;
        }
        let rig = rig().await;
        let a = id("logger");
        let src = write_py_app(&rig.src_dir("a"), "logger", "1.0.0", PY_SERVER);
        rig.install(&src).unwrap();
        let logs_dir = rig.manager.registry.paths().logs_dir(&a);
        fs::create_dir_all(&logs_dir).unwrap();
        fs::write(
            logs_dir.join("app.log"),
            (0..600)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let (text, truncated) = rig.manager.logs(&a, 10).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[9], "line 599");
        assert!(truncated);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_while_the_app_is_still_starting_does_not_leave_it_running() {
        if !have("python3") {
            return;
        }
        let rig = rig().await;
        let a = id("slow");
        let slow = "import os, time, http.server\ntime.sleep(3)\n\
            http.server.test(HandlerClass=http.server.SimpleHTTPRequestHandler, port=int(os.environ[\"APP_PORT\"]), bind=\"127.0.0.1\")\n";
        let src = write_py_app(&rig.src_dir("a"), "slow", "1.0.0", slow);
        rig.install(&src).unwrap();
        rig.manager.start(&a).unwrap();
        // 此刻还在 Starting(server 先睡 3 秒)
        let t0 = std::time::Instant::now();
        rig.manager.stop(&a).unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "stop 拖太久:{:?}",
            t0.elapsed()
        );
        assert_eq!(observed_of(&rig, &a), Some(ObservedState::Stopped));
        std::thread::sleep(Duration::from_secs(4));
        assert_eq!(observed_of(&rig, &a), Some(ObservedState::Stopped));
        assert_eq!(rig.fetch(&a, "/").status, 404);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_crashing_app_ends_up_failed_and_not_retryable_and_its_site_is_gone() {
        if !have("python3") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let manager = AppManager::with_resolver_and_policy_for_test(
            tmp.path().join("bytehost"),
            HOST,
            gateway.clone(),
            Arc::new(SystemResolver::new()),
            crate::process::restart::RestartPolicy {
                max_restarts: 1,
                base: Duration::from_millis(50),
                cap: Duration::from_millis(100),
                ..Default::default()
            },
        )
        .unwrap();
        let rig = Rig {
            tmp,
            gateway,
            manager,
        };
        let a = id("boomer");
        let src = write_py_app(
            &rig.src_dir("a"),
            "boomer",
            "1.0.0",
            "raise SystemExit(1)\n",
        );
        rig.install(&src).unwrap();
        rig.manager.start(&a).unwrap();
        wait_for(&rig, &a, 15, is_terminal_failure);
        assert_eq!(rig.fetch(&a, "/").status, 404);
        rig.manager.stop(&a).unwrap();
        assert_eq!(observed_of(&rig, &a), Some(ObservedState::Stopped));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_all_stops_every_process_and_reconcile_brings_wanted_ones_back() {
        if !have("python3") {
            return;
        }
        let rig = rig().await;
        let a = id("a-app");
        let b = id("b-app");
        rig.install(&write_py_app(
            &rig.src_dir("a"),
            "a-app",
            "1.0.0",
            PY_SERVER,
        ))
        .unwrap();
        rig.install(&write_py_app(
            &rig.src_dir("b"),
            "b-app",
            "1.0.0",
            PY_SERVER,
        ))
        .unwrap();
        rig.manager.start(&a).unwrap();
        rig.manager.start(&b).unwrap();
        wait_for(&rig, &a, 15, is_running);
        wait_for(&rig, &b, 15, is_running);

        let t0 = std::time::Instant::now();
        assert!(rig.manager.suspend_all().is_clean());
        // 并行收尾:总耗时 ≈ 最长的宽限,不是两倍
        assert!(
            t0.elapsed() < 2 * supervisor::DEFAULT_GRACE + Duration::from_secs(1),
            "suspend_all 太慢:{:?}",
            t0.elapsed()
        );
        for app in [&a, &b] {
            let s = rig
                .manager
                .list()
                .unwrap()
                .into_iter()
                .find(|s| &s.id == app)
                .unwrap();
            assert_eq!(s.observed, ObservedState::Stopped);
            assert_eq!(s.desired, DesiredState::Running, "desired 不该被改");
        }
        assert_eq!(rig.fetch(&a, "/").status, 404);

        assert!(rig.manager.reconcile().is_clean());
        wait_for(&rig, &a, 15, is_running);
        wait_for(&rig, &b, 15, is_running);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_reaps_a_leftover_process_recorded_in_the_run_dir() {
        use crate::process::supervise::{record_for, write_record};
        use std::os::unix::process::CommandExt;

        let rig = rig().await;
        // 装一个应用,好让 reconcile 有 AppPaths(apps/<id>/run)
        let a = id("orphan");
        rig.install(&write_app(&rig.src_dir("a"), "orphan", "1.0.0", "", "x"))
            .unwrap();
        let run_dir = rig.manager.registry.paths().run_dir(&a);
        fs::create_dir_all(&run_dir).unwrap();

        let mut child = std::process::Command::new("sleep")
            .arg("306")
            .process_group(0)
            .spawn()
            .unwrap();
        write_record(
            &run_dir,
            &record_for(child.id(), vec!["sleep".into(), "306".into()]),
        )
        .unwrap();

        assert!(rig.manager.reconcile().is_clean());
        let status = child.wait().unwrap();
        assert!(!status.success(), "孤儿进程应被信号终止:{status:?}");
        assert!(!run_dir.join("process.json").exists(), "记录应被清掉");
    }

    #[cfg(unix)]
    #[test]
    fn the_dependency_marker_follows_the_lockfile_content() {
        let cache = Path::new("/apps/x/cache");
        let one = deps_marker(cache, Some(b"one"));
        assert_ne!(
            one,
            deps_marker(cache, Some(b"two")),
            "换了 lockfile 必须重装"
        );
        assert_eq!(one, deps_marker(cache, Some(b"one")), "同一份内容标记不变");
        assert!(one.starts_with(cache));
        assert_eq!(deps_marker(cache, None), cache.join("deps-none.ok"));
    }

    #[test]
    fn the_child_path_lists_every_resolved_dir_once_and_keeps_the_system_fallback() {
        let r = |d: &str| Resolved {
            program: PathBuf::from(d).join("x"),
            path_dirs: vec![PathBuf::from(d)],
        };
        let (node, npm, node2) = (r("/opt/node/bin"), r("/opt/npm/bin"), r("/opt/node/bin"));
        assert_eq!(
            child_path(&[&node, &npm, &node2]),
            OsString::from("/opt/node/bin:/opt/npm/bin:/usr/bin:/bin")
        );
        assert_eq!(child_path(&[]), OsString::from("/usr/bin:/bin"));
    }

    /// 宿主给的缓存目录变量必须真的穿过 `build_env` 的白名单到达子进程。
    #[test]
    fn the_private_cache_variables_survive_the_child_environment_filter() {
        let id = id("x");
        let cache = Path::new("/apps/x/cache");
        let extra = cache_env(cache);
        let env = crate::process::env::build_env(
            std::env::vars_os(),
            &crate::process::env::EnvSpec {
                app_id: &id,
                port: 1,
                port_env: "PORT",
                data_dir: Path::new("/apps/x/data"),
                extra: &extra,
            },
        );
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("npm_config_cache"),
            Some(OsString::from("/apps/x/cache/npm"))
        );
        assert_eq!(
            get("UV_CACHE_DIR"),
            Some(OsString::from("/apps/x/cache/uv"))
        );
    }
}
