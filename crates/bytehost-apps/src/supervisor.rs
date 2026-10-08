//! 进程型应用的**监管线程**(bytehost A6c):装依赖 → 起进程 → 等健康 → 盯着 → 崩溃按退避重启/放弃。
//!
//! 与 manager 解耦:线程只通过 [`Transitions`] 回报阶段变化,由 manager 决定怎么持锁、写 `AppRecord`、发事件、
//! 增删 gateway 上游。**契约:每个 `Transitions` 方法返回 `false` 表示"这次监管已被取消"**——线程必须立刻收尾,
//! 不再回报、不再重启、不再写任何状态(否则会把已被 `stop`/`suspend_all` 停掉的应用又写回 `Running`)。
//!
//! 用 `std::thread`(与 A6a 的阻塞式 `process` 模块一致):子进程启动、健康探测、停止都是阻塞调用。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::id::AppId;
use crate::process::env::{EnvSpec, build_env};
use crate::process::health::{Health, probe_http, wait_healthy};
use crate::process::restart::{Decision, RestartPolicy, RestartTracker};
use crate::process::supervise::{
    ProcessSpec, Running, clear_record, pick_free_port, record_for, write_record,
};

/// 默认启动预算:从这个进程起到它必须在健康检查里应答,最多等这么久。
pub const DEFAULT_STARTUP_BUDGET: Duration = Duration::from_secs(30);
/// 依赖安装的默认超时。
pub const DEFAULT_INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 停止子进程的默认宽限(与 A6a 的 `stop` 语义一致)。
pub const DEFAULT_GRACE: Duration = Duration::from_secs(3);
/// 运行中周期健康检查的默认间隔(A6e)。
pub const DEFAULT_MONITOR_INTERVAL: Duration = Duration::from_secs(15);
/// 运行中健康检查单次探测的默认超时。
pub const DEFAULT_MONITOR_TIMEOUT: Duration = Duration::from_secs(2);
/// 连续多少次运行中健康检查失败才判定卡死并重启。`0` 表示关闭检查。
pub const DEFAULT_MONITOR_FAILURES: u32 = 3;

/// 监管线程在"起来之前"的两个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Starting,
}

/// 监管线程向宿主回报。**每个方法返回 `false` = 这次监管已被取消**,线程立刻收尾,不再回报、不再重启。
pub trait Transitions: Send + Sync + 'static {
    fn phase(&self, phase: Phase) -> bool;
    /// 应用健康了,在 `127.0.0.1:port` 上提供服务。
    fn ready(&self, port: u16) -> bool;
    /// 进程没了/不健康;`restart_in = Some(d)`:`d` 后重启(观察态回 `Starting`),`None`:放弃(`Failed{retryable:false}`)。
    fn down(&self, reason: String, restart_in: Option<Duration>) -> bool;
    /// 起不来且不该重试(拿不到端口等):`Failed`。
    fn failed(&self, reason: String, retryable: bool) -> bool;
    /// 依赖安装(npm ci / uv sync 等)失败,起不来且可重试:`Failed{retryable:true}`
    /// 并记住 `AppIssue::DependencyInstall`,让 GUI 画专门的问题页。
    fn install_failed(&self, summary: String) -> bool;
}

/// 一次监管要起的东西。
pub struct Launch {
    pub app_id: AppId,
    /// 已把 `argv[0]` 换成绝对路径。
    pub argv: Vec<String>,
    pub install: Option<Install>,
    pub cwd: PathBuf,
    /// 已带好重建过的 `PATH` 与 npm/uv 缓存目录变量;端口变量由监管线程每次启动时补上。
    pub parent_env: Vec<(OsString, OsString)>,
    /// 宿主显式追加的环境变量(不经白名单,见 `EnvSpec::extra`)。
    pub extra_env: Vec<(OsString, OsString)>,
    pub port_env: String,
    pub data_dir: PathBuf,
    pub health_path: String,
    /// 默认 30s(`DEFAULT_STARTUP_BUDGET`)。
    pub startup_budget: Duration,
    pub log_path: PathBuf,
    pub log_max_bytes: u64,
    pub log_keep: u32,
    pub run_dir: PathBuf,
    pub policy: RestartPolicy,
    /// 停止宽限,默认 3s。
    pub grace: Duration,
    /// 运行中健康检查间隔(A6e);`monitor_failures == 0` 时不检查。
    pub monitor_interval: Duration,
    /// 运行中健康检查的单次探测超时。
    pub monitor_timeout: Duration,
    /// 连续失败多少次判定卡死并重启;`0` 关闭检查。
    pub monitor_failures: u32,
}

/// 依赖安装:跑 `argv`(已把 `argv[0]` 换绝对路径),成功写 `marker`。
pub struct Install {
    pub argv: Vec<String>,
    pub marker: PathBuf,
    pub timeout: Duration,
}

/// 监管线程的主体。返回时进程已被收掉、`run_dir/process.json` 已清;`cancel` 之后不再写任何状态。
pub fn run(launch: Launch, cancel: Arc<AtomicBool>, tr: Arc<dyn Transitions>) {
    let cancelled = || cancel.load(Ordering::SeqCst);
    // 1. 依赖(装过的标记在就跳过)
    if let Some(inst) = &launch.install
        && !inst.marker.exists()
    {
        if !tr.phase(Phase::Preparing) || cancelled() {
            return;
        }
        match install_deps(&launch, inst, &cancel) {
            InstallOutcome::Done => {}
            InstallOutcome::Cancelled => return,
            InstallOutcome::Failed(why) => {
                tr.install_failed(why);
                return;
            }
        }
    }
    // 2. 起进程 → 健康 → 盯着 → 崩了按策略重启
    let mut tracker = RestartTracker::default();
    loop {
        if cancelled() || !tr.phase(Phase::Starting) {
            return;
        }
        let Ok(port) = pick_free_port() else {
            tr.failed("拿不到空闲端口".into(), true);
            return;
        };
        let env = app_env(&launch, port);
        let spawned = Running::spawn(&ProcessSpec {
            argv: launch.argv.clone(),
            cwd: launch.cwd.clone(),
            env,
            log_path: launch.log_path.clone(),
            log_max_bytes: launch.log_max_bytes,
            log_keep: launch.log_keep,
        });
        let reason;
        let mut ran = Duration::ZERO;
        match spawned {
            Err(e) => reason = format!("起不来: {e}"),
            Ok(mut running) => {
                let _ = write_record(
                    &launch.run_dir,
                    &record_for(running.pid(), launch.argv.clone()),
                );
                let health = wait_healthy(
                    port,
                    &launch.health_path,
                    launch.startup_budget,
                    Duration::from_millis(100),
                    || !cancelled() && running.try_exit().ok().flatten().is_none(),
                );
                if cancelled() {
                    let _ = running.stop(launch.grace);
                    clear_record(&launch.run_dir);
                    return;
                }
                reason = match health {
                    Health::Healthy => {
                        if !tr.ready(port) {
                            let _ = running.stop(launch.grace);
                            clear_record(&launch.run_dir);
                            return;
                        }
                        // 盯着,直到进程退出、被取消、或运行中健康检查连续失败
                        let mut consecutive: u32 = 0;
                        let mut last_probe = Instant::now();
                        loop {
                            if cancelled() {
                                let _ = running.stop(launch.grace);
                                clear_record(&launch.run_dir);
                                return;
                            }
                            if let Ok(Some(status)) = running.try_exit() {
                                break format!("进程退出: {status}");
                            }
                            if launch.monitor_failures > 0
                                && last_probe.elapsed() >= launch.monitor_interval
                            {
                                last_probe = Instant::now();
                                match probe_http(port, &launch.health_path, launch.monitor_timeout)
                                {
                                    Health::Healthy => consecutive = 0,
                                    Health::Unhealthy(_) => {
                                        consecutive += 1;
                                        if consecutive >= launch.monitor_failures {
                                            let _ = running.stop(launch.grace);
                                            break format!(
                                                "运行中健康检查连续失败 {consecutive} 次"
                                            );
                                        }
                                    }
                                }
                            }
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                    Health::Unhealthy(why) => {
                        let _ = running.stop(launch.grace);
                        format!("健康检查没通过: {why}")
                    }
                };
                ran = running.ran_for();
                clear_record(&launch.run_dir);
            }
        }
        match tracker.on_exit(&launch.policy, Instant::now(), ran) {
            Decision::RestartAfter(d) => {
                if !tr.down(reason, Some(d)) || !sleep_unless_cancelled(&cancel, d) {
                    return;
                }
            }
            Decision::GiveUp => {
                tr.down(format!("{reason}(重启次数过多,已放弃)"), None);
                return;
            }
        }
    }
}

enum InstallOutcome {
    Done,
    Cancelled,
    Failed(String),
}

/// 跑依赖安装进程(与 `run` 同一套启动/停止),轮询到退出/超时/取消。
fn install_deps(launch: &Launch, inst: &Install, cancel: &AtomicBool) -> InstallOutcome {
    let env = app_env(launch, 0);
    let spawned = Running::spawn(&ProcessSpec {
        argv: inst.argv.clone(),
        cwd: launch.cwd.clone(),
        env,
        log_path: launch.log_path.clone(),
        log_max_bytes: launch.log_max_bytes,
        log_keep: launch.log_keep,
    });
    let mut running = match spawned {
        Ok(r) => r,
        Err(e) => return InstallOutcome::Failed(format!("安装依赖起不来: {e}")),
    };
    let deadline = Instant::now() + inst.timeout;
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = running.stop(launch.grace);
            return InstallOutcome::Cancelled;
        }
        match running.try_exit() {
            Ok(Some(status)) => {
                if status.success() {
                    if let Some(parent) = inst.marker.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&inst.marker, "").is_err() {
                        return InstallOutcome::Failed("安装依赖成功但写标记失败".into());
                    }
                    return InstallOutcome::Done;
                }
                return InstallOutcome::Failed(format!("安装依赖失败: {status}"));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = running.stop(launch.grace);
                return InstallOutcome::Failed(format!("安装依赖进程出错: {e}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = running.stop(launch.grace);
            return InstallOutcome::Failed("安装依赖超时".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 每次启动重建子进程环境:白名单父环境 + 端口变量 + 宿主变量。
fn app_env(launch: &Launch, port: u16) -> Vec<(OsString, OsString)> {
    build_env(
        launch.parent_env.clone(),
        &EnvSpec {
            app_id: &launch.app_id,
            port,
            port_env: &launch.port_env,
            data_dir: &launch.data_dir,
            extra: &launch.extra_env,
        },
    )
}

/// 以 50ms 为步长睡 `total`;期间被取消就立即醒来返回 `false`。
fn sleep_unless_cancelled(cancel: &AtomicBool, total: Duration) -> bool {
    let step = Duration::from_millis(50);
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
    }
    !cancel.load(Ordering::SeqCst)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::process::restart::RestartPolicy;
    use std::sync::Mutex;

    /// 记录调用序列的假 `Transitions`;`cancel_on` 命中时(比较方法名)置位 `cancel` 并让方法返回 `false`。
    #[derive(Default)]
    struct Rec {
        log: Mutex<Vec<String>>,
        cancel_on: Mutex<Option<String>>,
        cancel: Arc<AtomicBool>,
    }

    impl Rec {
        fn push(&self, s: String) -> bool {
            let stop_here =
                self.cancel_on.lock().unwrap().as_deref() == Some(s.split(' ').next().unwrap());
            self.log.lock().unwrap().push(s);
            if stop_here {
                self.cancel.store(true, Ordering::SeqCst);
            }
            !self.cancel.load(Ordering::SeqCst)
        }
        fn names(&self) -> Vec<String> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.split(' ').next().unwrap().to_string())
                .collect()
        }
    }

    impl Transitions for Rec {
        fn phase(&self, p: Phase) -> bool {
            self.push(format!("{p:?}"))
        }
        fn ready(&self, port: u16) -> bool {
            self.push(format!("ready {port}"))
        }
        fn down(&self, why: String, r: Option<Duration>) -> bool {
            self.push(format!("down {why} {r:?}"))
        }
        fn failed(&self, why: String, retryable: bool) -> bool {
            self.push(format!("failed {why} {retryable}"))
        }
        fn install_failed(&self, summary: String) -> bool {
            self.push(format!("install_failed {summary}"))
        }
    }

    const FAST_POLICY: RestartPolicy = RestartPolicy {
        max_restarts: 2,
        window: Duration::from_secs(60),
        base: Duration::from_millis(50),
        cap: Duration::from_millis(100),
        stable_after: Duration::from_secs(60),
    };

    fn launch(dir: &std::path::Path, script: &str) -> Launch {
        Launch {
            app_id: AppId::new("sup-test").unwrap(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            install: None,
            cwd: dir.to_path_buf(),
            parent_env: std::env::vars_os().collect(),
            extra_env: Vec::new(),
            port_env: "APP_PORT".into(),
            data_dir: dir.join("data"),
            health_path: "/".into(),
            startup_budget: Duration::from_secs(5),
            log_path: dir.join("logs/app.log"),
            log_max_bytes: 1 << 20,
            log_keep: 2,
            run_dir: dir.join("run"),
            policy: FAST_POLICY,
            grace: Duration::from_secs(1),
            monitor_interval: DEFAULT_MONITOR_INTERVAL,
            monitor_timeout: DEFAULT_MONITOR_TIMEOUT,
            monitor_failures: 0,
        }
    }

    /// 宿主追加的变量(缓存目录)真的进了子进程;而继承来的同类变量不会。
    #[test]
    fn extra_env_reaches_the_child_process() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen.txt");
        let mut l = launch(
            dir.path(),
            &format!("echo \"$npm_config_cache\" > {}; exit 0", out.display()),
        );
        l.extra_env = vec![("npm_config_cache".into(), "/apps/x/cache/npm".into())];
        l.parent_env
            .push(("npm_config_cache".into(), "/home/u/.npm".into()));
        let rec = Arc::new(Rec::default());
        run(l, Arc::new(AtomicBool::new(false)), rec.clone());
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim(),
            "/apps/x/cache/npm"
        );
    }

    fn have_python3() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn serve_script(delay_secs: u64) -> String {
        let prelude = if delay_secs > 0 {
            format!("import time; time.sleep({delay_secs}); ")
        } else {
            String::new()
        };
        format!(
            "python3 -c \"{prelude}import os,http.server,socketserver; \
             socketserver.TCPServer.allow_reuse_address=True; \
             http.server.test(HandlerClass=http.server.SimpleHTTPRequestHandler, \
             port=int(os.environ['APP_PORT']), bind='127.0.0.1')\""
        )
    }

    fn rec() -> (Arc<Rec>, Arc<AtomicBool>) {
        let r = Arc::new(Rec::default());
        (r.clone(), r.cancel.clone())
    }

    /// 一个按"第几次请求"决定行为的 python 服务器:前 `healthy_probes` 次正常应答,
    /// 之后按 `fail_ms`(> 单次探测超时即超时失败)处理。`pidfile` 记录 pid 供断言收进程。
    /// `fail_ms = 0` 表示此后永久卡死(`sleep(3600)`)。
    fn monitoring_script(dir: &std::path::Path, healthy_probes: u32, fail_ms: u64) -> String {
        let pidfile = dir.join("monitor.pid");
        let server = format!(
            r#"
import os, time, http.server, socketserver, sys
count = 0
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        global count
        count += 1
        if count <= {healthy_probes}:
            self.send_response(200); self.end_headers(); self.wfile.write(b'ok')
        elif {fail_ms} == 0:
            time.sleep(3600)
        else:
            time.sleep({fail_ms} / 1000.0)
            self.send_response(200); self.end_headers(); self.wfile.write(b'ok')
    def log_message(self, *a):
        pass
open(r'{pidfile}', 'w').write(str(os.getpid()))
socketserver.TCPServer.allow_reuse_address = True
http.server.test(HandlerClass=H, port=int(os.environ['APP_PORT']), bind='127.0.0.1')
"#,
            healthy_probes = healthy_probes,
            fail_ms = fail_ms,
            pidfile = pidfile.display(),
        );
        format!("python3 -c \"{server}\"")
    }

    /// 监视参数调小:方便测试快速触发。
    fn monitoring_launch(dir: &std::path::Path, script: &str, failures: u32) -> Launch {
        let mut l = launch(dir, script);
        l.monitor_interval = Duration::from_millis(150);
        l.monitor_timeout = Duration::from_millis(300);
        l.monitor_failures = failures;
        l
    }

    fn read_pid(path: &std::path::Path) -> i32 {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_process_that_stops_answering_is_killed_and_restarted() {
        if !have_python3() {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 启动期探测 + 一次监视探测应答(前 3 次),之后永久卡死。
        let script = monitoring_script(dir.path(), 3, 0);
        let (r, cancel) = rec();
        let mut l = monitoring_launch(dir.path(), &script, 3);
        l.startup_budget = Duration::from_secs(5);
        run(l, cancel, r.clone());
        let names = r.names();
        let log = r.log.lock().unwrap().clone();
        // ready 之后应出现一次"运行中健康检查连续失败",随后重启并再次 ready。
        assert_eq!(names.first().map(String::as_str), Some("Starting"));
        assert!(names.contains(&"ready".to_string()), "{log:?}");
        assert!(
            log.iter()
                .any(|s| s.starts_with("down") && s.contains("运行中健康检查连续失败")),
            "{log:?}"
        );
        assert!(
            names.iter().filter(|n| *n == "ready").count() >= 2,
            "应重启并重新就绪:{log:?}"
        );
        // 旧进程已被收掉。
        let pid = read_pid(&dir.path().join("monitor.pid"));
        assert!(!pid_alive(pid), "卡死的旧进程应已被收掉");
    }

    #[test]
    fn a_slow_but_answering_app_is_not_killed() {
        if !have_python3() {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 每次应答 sleep 100ms(< timeout 300ms):慢但活着,不应被判死。
        let script = monitoring_script(dir.path(), u32::MAX, 100);
        let (r, cancel) = rec();
        let l = monitoring_launch(dir.path(), &script, 3);
        let started = Instant::now();
        // 观察 ~1.5s 后取消。
        *r.cancel_on.lock().unwrap() = Some("__timeout__".into());
        let handle = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                // 让监管跑 1.5s 再取消
                std::thread::sleep(Duration::from_millis(1500));
                cancel.store(true, Ordering::SeqCst);
            }
        });
        run(l, cancel, r.clone());
        let _ = handle.join();
        let _ = started;
        assert!(
            !r.names().iter().any(|n| n == "down"),
            "慢但应答的应用不应被杀:{:?}",
            r.log
        );
    }

    #[test]
    fn a_single_failed_probe_does_not_count_after_a_success() {
        if !have_python3() {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 交替:成功一次、失败一次……连续失败永不到 3。
        let script = "python3 -c \"import os,time,http.server,socketserver\n\
             count=0\n\
             class H(http.server.BaseHTTPRequestHandler):\n\
             \x20 def do_GET(self):\n\
             \x20   global count\n\
             \x20   count+=1\n\
             \x20   if count%2==1:\n\
             \x20     self.send_response(200);self.end_headers();self.wfile.write(b'ok')\n\
             \x20   else:\n\
             \x20     time.sleep(0.4);self.send_response(200);self.end_headers()\n\
             \x20 def log_message(self,*a): pass\n\
             socketserver.TCPServer.allow_reuse_address=True\n\
             http.server.test(HandlerClass=H,port=int(os.environ['APP_PORT']),bind='127.0.0.1')\n\"";
        let (r, cancel) = rec();
        let l = monitoring_launch(dir.path(), script, 3);
        let handle = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                std::thread::sleep(Duration::from_millis(2000));
                cancel.store(true, Ordering::SeqCst);
            }
        });
        run(l, cancel, r.clone());
        let _ = handle.join();
        assert!(
            !r.names().iter().any(|n| n == "down"),
            "交替失败不应累积到阈值:{:?}",
            r.log
        );
    }

    #[test]
    fn disabling_the_monitor_keeps_the_old_behaviour() {
        if !have_python3() {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 前 3 次应答后永久卡死,但 monitor_failures=0 表示关闭检查 → 不该被杀。
        let script = monitoring_script(dir.path(), 3, 0);
        let (r, cancel) = rec();
        let l = monitoring_launch(dir.path(), &script, 0);
        let handle = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                std::thread::sleep(Duration::from_millis(1000));
                cancel.store(true, Ordering::SeqCst);
            }
        });
        run(l, cancel, r.clone());
        let _ = handle.join();
        assert!(
            !r.names().iter().any(|n| n == "down"),
            "关闭检查后卡死的应用不该被杀:{:?}",
            r.log
        );
    }

    #[test]
    fn cancelling_during_monitoring_stops_without_further_probes() {
        if !have_python3() {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hi</h1>").unwrap();
        let (r, cancel) = rec();
        *r.cancel_on.lock().unwrap() = Some("ready".into());
        let mut l = monitoring_launch(dir.path(), &serve_script(0), 3);
        l.grace = Duration::from_secs(1);
        let grace = l.grace;
        let started = Instant::now();
        run(l, cancel, r.clone());
        assert!(
            started.elapsed() < grace + Duration::from_secs(1),
            "取消后应尽快收尾:{:?}",
            started.elapsed()
        );
        assert_eq!(
            r.names().iter().next_back().map(String::as_str),
            Some("ready")
        );
    }

    #[test]
    fn a_healthy_app_is_reported_ready_then_cancelling_stops_it_without_further_reports() {
        if !have_python3() {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hi</h1>").unwrap();
        let (r, cancel) = rec();
        *r.cancel_on.lock().unwrap() = Some("ready".into());
        run(launch(dir.path(), &serve_script(0)), cancel, r.clone());
        assert_eq!(
            r.names().iter().next_back().map(String::as_str),
            Some("ready")
        );
        assert!(!r.names().iter().any(|n| n == "down" || n == "failed"));
        assert!(!dir.path().join("run/process.json").exists(), "记录被清掉");
    }

    #[test]
    fn a_crash_loop_restarts_with_backoff_then_gives_up() {
        let dir = tempfile::tempdir().unwrap();
        let (r, cancel) = rec();
        run(launch(dir.path(), "exit 3"), cancel, r.clone());
        let names = r.names();
        assert_eq!(
            names,
            vec!["Starting", "down", "Starting", "down", "Starting", "down"],
            "{names:?}"
        );
        let log = r.log.lock().unwrap();
        assert!(log.last().unwrap().contains("已放弃"), "{log:?}");
    }

    #[test]
    fn an_app_that_never_answers_is_stopped_and_counted_as_a_failed_start() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("app.pid");
        let script = format!("echo $$ > {}; sleep 30", pidfile.display());
        let mut l = launch(dir.path(), &script);
        l.startup_budget = Duration::from_millis(600);
        let (r, cancel) = rec();
        run(l, cancel, r.clone());
        let log = r.log.lock().unwrap();
        assert!(log[0].starts_with("Starting"), "{log:?}");
        assert!(log[1].contains("健康检查没通过"), "{log:?}");
        drop(log);
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(!alive, "sleep 进程应已被收掉");
    }

    #[test]
    fn dependencies_install_once_and_a_failed_install_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let installs = dir.path().join("installs.txt");
        let marker = dir.path().join("cache/deps.ok");
        let install = || {
            Some(Install {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("echo x >> {}", installs.display()),
                ],
                marker: marker.clone(),
                timeout: Duration::from_secs(5),
            })
        };
        let (r, cancel) = rec();
        let mut l = launch(dir.path(), "exit 0");
        l.install = install();
        run(l, cancel, r.clone());
        assert!(
            r.names().contains(&"Preparing".to_string()),
            "{:?}",
            r.names()
        );
        assert_eq!(
            std::fs::read_to_string(&installs).unwrap().lines().count(),
            1
        );

        // 第二次:marker 已在 → 不再装、不再报 Preparing
        let (r2, cancel2) = rec();
        let mut l2 = launch(dir.path(), "exit 0");
        l2.install = install();
        run(l2, cancel2, r2.clone());
        assert_eq!(
            std::fs::read_to_string(&installs).unwrap().lines().count(),
            1
        );
        assert!(
            !r2.names().contains(&"Preparing".to_string()),
            "{:?}",
            r2.names()
        );

        // 安装失败:install_failed(不是 failed),且不起进程(没有 Starting)
        let marker2 = dir.path().join("cache/fail.ok");
        let (r3, cancel3) = rec();
        let mut l3 = launch(dir.path(), "exit 0");
        l3.install = Some(Install {
            argv: vec!["sh".into(), "-c".into(), "exit 9".into()],
            marker: marker2,
            timeout: Duration::from_secs(5),
        });
        run(l3, cancel3, r3.clone());
        let names = r3.names();
        assert_eq!(names, vec!["Preparing", "install_failed"], "{names:?}");
        assert!(
            r3.log.lock().unwrap()[1].contains("安装依赖失败"),
            "{:?}",
            r3.log
        );
    }

    /// 依赖安装失败的四个分支(退出码非 0 / 超时 / 起不来 / 写标记失败)都必须走
    /// `install_failed`,而不是泛化的 `failed`;`summary` 里不含安装输出本身。
    #[test]
    fn every_install_failure_branch_reports_install_failed() {
        if !have_python3() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        // 退出码非 0
        let (r, cancel) = rec();
        let mut l = launch(dir.path(), "exit 0");
        l.install = Some(Install {
            argv: vec![
                "python3".into(),
                "-c".into(),
                "import sys; sys.exit(3)".into(),
            ],
            marker: dir.path().join("m1.ok"),
            timeout: Duration::from_secs(5),
        });
        run(l, cancel, r.clone());
        assert_eq!(
            r.names(),
            vec!["Preparing", "install_failed"],
            "{:?}",
            r.names()
        );
        assert!(
            r.log.lock().unwrap()[1].contains("安装依赖失败"),
            "{:?}",
            r.log
        );

        // 超时
        let (r, cancel) = rec();
        let mut l = launch(dir.path(), "exit 0");
        l.install = Some(Install {
            argv: vec![
                "python3".into(),
                "-c".into(),
                "import time; time.sleep(30)".into(),
            ],
            marker: dir.path().join("m2.ok"),
            timeout: Duration::from_millis(200),
        });
        run(l, cancel, r.clone());
        assert_eq!(
            r.names(),
            vec!["Preparing", "install_failed"],
            "{:?}",
            r.names()
        );
        assert!(r.log.lock().unwrap()[1].contains("超时"), "{:?}", r.log);

        // 起不来(解释器路径不存在)
        let (r, cancel) = rec();
        let mut l = launch(dir.path(), "exit 0");
        l.install = Some(Install {
            argv: vec!["/definitely/not/here".into()],
            marker: dir.path().join("m3.ok"),
            timeout: Duration::from_secs(5),
        });
        run(l, cancel, r.clone());
        assert_eq!(
            r.names(),
            vec!["Preparing", "install_failed"],
            "{:?}",
            r.names()
        );
        assert!(r.log.lock().unwrap()[1].contains("起不来"), "{:?}", r.log);

        // 写标记失败(marker 的父目录不可写)
        let readonly = dir.path().join("ro");
        std::fs::create_dir_all(&readonly).unwrap();
        let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o500);
        std::fs::set_permissions(&readonly, perms).unwrap();
        let (r, cancel) = rec();
        let mut l = launch(dir.path(), "exit 0");
        l.install = Some(Install {
            argv: vec!["python3".into(), "-c".into(), "pass".into()],
            // 父目录只读 → 写 marker 失败
            marker: readonly.join("m4.ok"),
            timeout: Duration::from_secs(5),
        });
        run(l, cancel, r.clone());
        let names = r.names();
        assert_eq!(names, vec!["Preparing", "install_failed"], "{names:?}");
        assert!(
            r.log.lock().unwrap()[1].contains("写标记失败"),
            "{:?}",
            r.log
        );
        // 恢复权限让 tempdir 能清理。
        let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
        std::fs::set_permissions(&readonly, perms).unwrap();
    }

    #[test]
    fn cancelling_during_the_install_stops_the_install_process() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cache/deps.ok");
        let mut l = launch(dir.path(), "exit 0");
        l.install = Some(Install {
            argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            marker,
            timeout: Duration::from_secs(60),
        });
        let (r, cancel) = rec();
        *r.cancel_on.lock().unwrap() = Some("Preparing".into());
        let grace = l.grace;
        let started = Instant::now();
        run(l, cancel, r.clone());
        assert!(
            started.elapsed() < grace + Duration::from_secs(1),
            "取消后安装进程应被收掉,耗时 {:?}",
            started.elapsed()
        );
        assert_eq!(r.names(), vec!["Preparing"], "{:?}", r.names());
    }

    #[test]
    fn a_failed_spawn_counts_as_a_failed_start_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = launch(dir.path(), "ignored");
        l.argv = vec!["/definitely/not/here".into()];
        let (r, cancel) = rec();
        run(l, cancel, r.clone());
        let names = r.names();
        assert!(names.contains(&"down".to_string()), "{names:?}");
        assert!(!names.contains(&"failed".to_string()), "{names:?}");
        assert!(
            r.log.lock().unwrap().iter().any(|s| s.contains("起不来")),
            "{:?}",
            r.log
        );
    }
}
