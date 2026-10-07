//! `RuntimeManager`(A6d):把固定版本表变成**可审阅的安装计划**、在用户批准后起后台线程安装、
//! 报告进度、列已装/卸载。批准的计划会被**重新计算并逐字段核对**——客户端改过 URL/哈希/版本一律拒绝,
//! 且**不发起任何下载**。Python 走"先装 uv、再由 uv 装 Python"。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::proto::{ManagedRuntime, RuntimeDownload, RuntimeInstallPlan, RuntimeJob};

use super::fetch::{Archive, CurlFetcher, Fetcher, TarArchive};
use super::install::{InstallError, Installer, Phase};
use super::pins::{PYTHON_VERSION, Pin, Target};
use super::store::RuntimeStore;

/// 计划里的一个下载项(线上类型 [`RuntimeDownload`];`sha256: None` 只出现在由 uv 校验的 Python 上)。
pub type DownloadItem = RuntimeDownload;

/// 一份**可审阅的安装计划**(线上类型 [`RuntimeInstallPlan`]);`start_install` 会重算并逐字段核对它。
pub type InstallPlanRt = RuntimeInstallPlan;

/// 任务快照(线上类型 [`RuntimeJob`])。
pub type Job = RuntimeJob;

#[derive(Debug)]
pub enum RtError {
    Unsupported(String),
    Conflict(String),
    PlanChanged,
    /// dozerd 正在退出(`cancel_all_and_join` 之后):不再接受新的安装。
    ShuttingDown,
    Io(io::Error),
}

impl std::fmt::Display for RtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(m) => write!(f, "不支持:{m}"),
            Self::Conflict(m) => write!(f, "冲突:{m}"),
            Self::PlanChanged => write!(f, "计划已变化"),
            Self::ShuttingDown => write!(f, "dozerd 正在退出,暂不接受运行时安装"),
            Self::Io(e) => write!(f, "I/O 错误:{e}"),
        }
    }
}

impl std::error::Error for RtError {}

impl From<io::Error> for RtError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 由 uv 安装 Python 的接口(测试可替换)。
pub trait UvRunner: Send + Sync {
    fn python_install(
        &self,
        uv: &Path,
        version: &str,
        install_dir: &Path,
        cache_dir: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<()>;
}

/// 真实现:起 `uv python install <version>`,环境清空后只留必要项 + 代理变量。
pub struct RealUvRunner;

impl UvRunner for RealUvRunner {
    fn python_install(
        &self,
        uv: &Path,
        version: &str,
        install_dir: &Path,
        cache_dir: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        std::fs::create_dir_all(install_dir)?;
        std::fs::create_dir_all(cache_dir)?;
        let mut cmd = Command::new(uv);
        cmd.arg("python")
            .arg("install")
            .arg(version)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("UV_PYTHON_INSTALL_DIR", install_dir)
            .env("UV_CACHE_DIR", cache_dir)
            .env("UV_PYTHON_PREFERENCE", "only-managed")
            .env("UV_NO_CONFIG", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("HOME", home);
        }
        // 走代理:只透传这几个,不做其它继承。
        for key in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY"] {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        let mut child = cmd.spawn()?;
        let start = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::Interrupted, "取消"));
            }
            match child.try_wait()? {
                Some(status) if status.success() => return Ok(()),
                Some(status) => {
                    let mut msg = format!("uv python install 退出码 {:?}", status.code());
                    use std::io::Read;
                    if let Some(mut e) = child.stderr.take() {
                        let mut s = String::new();
                        let _ = e.read_to_string(&mut s);
                        if !s.trim().is_empty() {
                            msg.push_str(&format!(":{}", s.trim()));
                        }
                    }
                    return Err(io::Error::other(msg));
                }
                None => {
                    // 没有硬超时:python 下载可能很慢;但也不能忙等。
                    if start.elapsed().as_secs() > 600 {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(io::Error::other("uv python install 超时"));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

struct JobEntry {
    job: Arc<Mutex<Job>>,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

pub struct RuntimeManager {
    store: RuntimeStore,
    fetcher: Arc<dyn Fetcher>,
    archive: Arc<dyn Archive>,
    target: Option<Target>,
    uv_runner: Arc<dyn UvRunner>,
    pins: &'static [Pin],
    jobs: Mutex<HashMap<ManagedRuntime, JobEntry>>,
    /// `cancel_all_and_join` 置位后不再起新任务。在 `jobs` 锁内读写,所以不会有任务在收尾之后才被登记。
    closed: AtomicBool,
}

impl RuntimeManager {
    /// 用系统 `curl`/`tar` 与真 uv。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_parts(
            root,
            Arc::new(CurlFetcher),
            Arc::new(TarArchive),
            Target::current(),
            Arc::new(RealUvRunner),
            super::pins::PINS,
        )
    }

    pub fn with_parts(
        root: impl Into<PathBuf>,
        fetcher: Arc<dyn Fetcher>,
        archive: Arc<dyn Archive>,
        target: Option<Target>,
        uv_runner: Arc<dyn UvRunner>,
        pins: &'static [Pin],
    ) -> Self {
        let store = RuntimeStore::new(root);
        store.sweep_staging();
        Self {
            store,
            fetcher,
            archive,
            target,
            uv_runner,
            pins,
            jobs: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        }
    }

    pub fn store(&self) -> &RuntimeStore {
        &self.store
    }

    fn target(&self) -> Result<Target, RtError> {
        self.target
            .ok_or_else(|| RtError::Unsupported("此平台暂不支持自动安装运行时".to_string()))
    }

    fn pin(&self, name: &str, target: Target) -> Result<&'static Pin, RtError> {
        self.pins
            .iter()
            .find(|p| p.name == name && p.target == target)
            .ok_or_else(|| RtError::Unsupported(format!("没有 {name} 的固定版本")))
    }

    /// 计算一份安装计划(纯计算,不下载)。
    pub fn plan(&self, rt: ManagedRuntime) -> Result<InstallPlanRt, RtError> {
        let target = self.target()?;
        match rt {
            ManagedRuntime::Node => {
                let pin = self.pin("node", target)?;
                Ok(InstallPlanRt {
                    runtime: rt,
                    versions: vec![(pin.name.to_string(), pin.version.to_string())],
                    downloads: vec![DownloadItem {
                        what: format!("Node {}", pin.version),
                        url: pin.url.to_string(),
                        sha256: Some(pin.sha256.to_string()),
                        note: "官方 nodejs.org 发行版,已固定的 SHA-256".to_string(),
                    }],
                    dest: self
                        .store
                        .version_dir(pin.name, pin.version)
                        .display()
                        .to_string(),
                    will_do: vec![
                        "下载并校验 SHA-256".to_string(),
                        format!(
                            "解压到 {}",
                            self.store.version_dir(pin.name, pin.version).display()
                        ),
                        "不修改系统环境与 shell 配置".to_string(),
                    ],
                })
            }
            ManagedRuntime::Python => {
                let uv = self.pin("uv", target)?;
                let python_dest = self.store.root().join("python").join(PYTHON_VERSION);
                Ok(InstallPlanRt {
                    runtime: rt,
                    versions: vec![
                        (uv.name.to_string(), uv.version.to_string()),
                        ("python".to_string(), PYTHON_VERSION.to_string()),
                    ],
                    downloads: vec![
                        DownloadItem {
                            what: format!("uv {}", uv.version),
                            url: uv.url.to_string(),
                            sha256: Some(uv.sha256.to_string()),
                            note: "官方 GitHub 发行版,已固定的 SHA-256".to_string(),
                        },
                        DownloadItem {
                            what: format!("Python {PYTHON_VERSION}"),
                            url: "由 uv 从 python-build-standalone 下载".to_string(),
                            sha256: None,
                            note: "哈希由 uv 内置校验,不是 bytehost 固定的校验和".to_string(),
                        },
                    ],
                    dest: python_dest.display().to_string(),
                    will_do: vec![
                        format!("安装 uv {}", uv.version),
                        format!(
                            "运行 uv python install {PYTHON_VERSION}(UV_PYTHON_INSTALL_DIR={})",
                            self.store.root().join("python").display()
                        ),
                        "Python 由 uv 从 python-build-standalone 下载并校验,不是 bytehost 固定的校验和"
                            .to_string(),
                        "不修改系统环境与 shell 配置".to_string(),
                    ],
                })
            }
        }
    }

    /// 重算计划并与 `approved` 逐字段核对;一致才起后台线程。已有进行中的任务 → `Conflict`。
    pub fn start_install(&self, approved: &InstallPlanRt) -> Result<(), RtError> {
        let recomputed = self.plan(approved.runtime)?;
        if &recomputed != approved {
            return Err(RtError::PlanChanged);
        }

        let mut jobs = self.jobs.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(RtError::ShuttingDown);
        }
        if let Some(entry) = jobs.get(&approved.runtime)
            && !entry.job.lock().unwrap().finished
        {
            return Err(RtError::Conflict(format!(
                "{:?} 的安装正在进行",
                approved.runtime
            )));
        }

        let job = Arc::new(Mutex::new(Job {
            runtime: approved.runtime,
            phase: "pending".to_string(),
            done: 0,
            total: None,
            failed: None,
            finished: false,
        }));
        let cancel = Arc::new(AtomicBool::new(false));

        let handle = {
            let job = Arc::clone(&job);
            let cancel = Arc::clone(&cancel);
            let rt = approved.runtime;
            let target = self.target()?;
            let fetcher = Arc::clone(&self.fetcher);
            let archive = Arc::clone(&self.archive);
            let uv_runner = Arc::clone(&self.uv_runner);
            let store = self.store.clone();
            let pins = self.pins;
            std::thread::spawn(move || {
                let result = run_install(
                    rt,
                    target,
                    pins,
                    &store,
                    fetcher.as_ref(),
                    archive.as_ref(),
                    uv_runner.as_ref(),
                    &job,
                    &cancel,
                );
                let mut j = job.lock().unwrap();
                j.finished = true;
                j.done = j.total.unwrap_or(j.done);
                match result {
                    Ok(()) => j.phase = "done".to_string(),
                    Err(InstallError::Cancelled) => {
                        j.phase = "cancelled".to_string();
                        j.failed = Some("已取消".to_string());
                    }
                    Err(e) => {
                        j.phase = "failed".to_string();
                        j.failed = Some(e.to_string());
                    }
                }
            })
        };

        jobs.insert(
            approved.runtime,
            JobEntry {
                job,
                cancel,
                handle: Some(handle),
            },
        );
        Ok(())
    }

    pub fn jobs(&self) -> Vec<Job> {
        let jobs = self.jobs.lock().unwrap();
        let mut out: Vec<Job> = jobs
            .values()
            .map(|e| e.job.lock().unwrap().clone())
            .collect();
        out.sort_by_key(|j| format!("{:?}", j.runtime));
        out
    }

    pub fn installed(&self, rt: ManagedRuntime) -> Vec<String> {
        let name = match rt {
            ManagedRuntime::Node => "node",
            ManagedRuntime::Python => "python",
        };
        match rt {
            ManagedRuntime::Node => self.store.installed(name),
            ManagedRuntime::Python => {
                // Python 目录名是 uv 的 cpython-* 命名,直接列目录。
                let dir = self.store.root().join("python");
                let mut versions: Vec<String> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .filter_map(|e| e.file_name().into_string().ok())
                    // uv 在安装目录里还放 `.cache`/`.temp` 等自己的内部目录,不是 Python 版本
                    .filter(|n| super::resolve::is_python_install_name(n))
                    .collect();
                versions.sort();
                versions.reverse();
                versions
            }
        }
    }

    pub fn uninstall(&self, rt: ManagedRuntime, version: &str) -> Result<(), RtError> {
        let name = match rt {
            ManagedRuntime::Node => "node",
            ManagedRuntime::Python => "python",
        };
        if rt == ManagedRuntime::Python && !super::resolve::is_python_install_name(version) {
            return Err(RtError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{version:?} 不是一个受管的 Python 版本"),
            )));
        }
        self.store.remove(name, version).map_err(RtError::Io)
    }

    /// dozerd 退出时调:置位取消、`join` 线程,之后清 staging。
    pub fn cancel_all_and_join(&self) {
        let entries: Vec<(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>)> = {
            let mut jobs = self.jobs.lock().unwrap();
            self.closed.store(true, Ordering::SeqCst);
            jobs.values_mut()
                .map(|e| {
                    e.cancel.store(true, Ordering::SeqCst);
                    (Arc::clone(&e.cancel), e.handle.take())
                })
                .collect()
        };
        for (_, handle) in entries {
            if let Some(h) = handle {
                let _ = h.join();
            }
        }
        self.store.sweep_staging();
    }
}

/// 后台线程主体:按需装 pins(可重复使用的 uv 会跳过),再按运行时补做 Python。
#[allow(clippy::too_many_arguments)]
fn run_install(
    rt: ManagedRuntime,
    target: Target,
    pins: &'static [Pin],
    store: &RuntimeStore,
    fetcher: &dyn Fetcher,
    archive: &dyn Archive,
    uv_runner: &dyn UvRunner,
    job: &Arc<Mutex<Job>>,
    cancel: &AtomicBool,
) -> Result<(), InstallError> {
    let names: &[&str] = match rt {
        ManagedRuntime::Node => &["node"],
        ManagedRuntime::Python => &["uv"],
    };

    for name in names {
        if cancel.load(Ordering::SeqCst) {
            return Err(InstallError::Cancelled);
        }
        let pin = pins
            .iter()
            .find(|p| p.name == *name && p.target == target)
            .ok_or_else(|| InstallError::Download(format!("没有 {name} 的固定版本")))?;
        // 已装该版本 → 跳过(如 uv 已为别的运行时装过)。
        if store.version_dir(pin.name, pin.version).exists() {
            continue;
        }
        let installer = Installer {
            store,
            fetcher,
            archive,
        };
        let mut on_progress = |phase: Phase, done: u64, total: Option<u64>| {
            let mut j = job.lock().unwrap();
            j.phase = phase_name(phase).to_string();
            // 后续阶段(校验/解压/试跑)不报字节数,不要因此把已下载进度清零。
            if total.is_some() || done > 0 {
                j.done = done;
                j.total = total;
            }
        };
        installer.install(pin, &mut on_progress, cancel)?;
    }

    if rt == ManagedRuntime::Python {
        let uv_pin = pins
            .iter()
            .find(|p| p.name == "uv" && p.target == target)
            .ok_or_else(|| InstallError::Download("没有 uv 的固定版本".to_string()))?;
        let uv_bin = store
            .version_dir(uv_pin.name, uv_pin.version)
            .join(uv_pin.bin_rel);
        {
            let mut j = job.lock().unwrap();
            j.phase = "python".to_string();
            j.done = 0;
            j.total = None;
        }
        uv_runner
            .python_install(
                &uv_bin,
                PYTHON_VERSION,
                &store.root().join("python"),
                &store.root().join("cache").join("uv"),
                cancel,
            )
            .map_err(|e| {
                if e.kind() == io::ErrorKind::Interrupted {
                    InstallError::Cancelled
                } else {
                    InstallError::Io(e)
                }
            })?;
    }

    Ok(())
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Downloading => "downloading",
        Phase::Verifying => "verifying",
        Phase::Extracting => "extracting",
        Phase::Checking => "checking",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn tar_gz(dir: &Path, script: &str) -> (PathBuf, String) {
        use crate::digest::sha256_hex;
        let src = dir.join(format!("src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(src.join("bin")).unwrap();
        let tool = src.join("bin/tool");
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        // uv 归档没有前缀层,顶层就是 `uv`;node 有 `node-vX/` 前缀。这里造 strip=0 的 uv 形状。
        let archive = dir.join(format!("{}.tar.gz", uuid::Uuid::new_v4()));
        let status = Command::new("/usr/bin/tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("bin")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&archive).unwrap();
        (archive, sha256_hex(&bytes))
    }

    /// 造一个顶层 `bin/tool`(strip 0)的假 pin。
    fn fake_pin(name: &str, version: &str, url: &str, sha: String, bin_rel: &str) -> Pin {
        Pin {
            name: leak(name.to_string()),
            version: leak(version.to_string()),
            target: Target::Aarch64Apple,
            url: leak(url.to_string()),
            sha256: leak(sha),
            strip_components: 0,
            bin_rel: leak(bin_rel.to_string()),
        }
    }

    /// 假 fetcher:从预置文件拷贝,可配置阻塞(等 cancel)、失败、调用计数。
    struct CountingFetcher {
        source: Mutex<PathBuf>,
        calls: AtomicUsize,
        block: bool,
        fail: Option<String>,
        bytes: Mutex<Vec<u8>>,
    }

    impl CountingFetcher {
        fn ok(source: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                source: Mutex::new(source),
                calls: AtomicUsize::new(0),
                block: false,
                fail: None,
                bytes: Mutex::new(Vec::new()),
            })
        }
        fn blocking(source: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                source: Mutex::new(source),
                calls: AtomicUsize::new(0),
                block: true,
                fail: None,
                bytes: Mutex::new(Vec::new()),
            })
        }
        fn failing(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                source: Mutex::new(PathBuf::new()),
                calls: AtomicUsize::new(0),
                block: false,
                fail: Some(reason.to_string()),
                bytes: Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Fetcher for CountingFetcher {
        fn fetch(
            &self,
            _url: &str,
            dest: &Path,
            on_progress: &mut dyn FnMut(u64, Option<u64>),
            cancel: &AtomicBool,
        ) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(r) = &self.fail {
                return Err(io::Error::other(r.clone()));
            }
            if self.block {
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(io::Error::new(io::ErrorKind::Interrupted, "取消"));
            }
            let src = self.source.lock().unwrap().clone();
            let bytes = std::fs::read(src)?;
            std::fs::write(dest, &bytes)?;
            *self.bytes.lock().unwrap() = bytes.clone();
            // 分两步报进度,便于测试观察 `done > 0`。
            on_progress(1, Some(bytes.len() as u64));
            on_progress(bytes.len() as u64, Some(bytes.len() as u64));
            Ok(())
        }
    }

    /// 假 uv runner:记录调用参数。
    #[derive(Default)]
    struct FakeUv {
        calls: Mutex<Vec<(PathBuf, String, PathBuf, PathBuf)>>,
    }

    impl FakeUv {
        fn calls(&self) -> Vec<(PathBuf, String, PathBuf, PathBuf)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl UvRunner for FakeUv {
        fn python_install(
            &self,
            uv: &Path,
            version: &str,
            install_dir: &Path,
            cache_dir: &Path,
            _cancel: &AtomicBool,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push((
                uv.to_path_buf(),
                version.to_string(),
                install_dir.to_path_buf(),
                cache_dir.to_path_buf(),
            ));
            // 造一个 cpython-* 目录,模拟 uv 落位。
            let d = install_dir.join(format!("cpython-{version}.0-macos-aarch64-none"));
            std::fs::create_dir_all(d.join("bin"))?;
            let py = d.join("bin/python3");
            std::fs::write(&py, "#!/bin/sh\n")?;
            std::fs::set_permissions(&py, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        }
    }

    fn manager(
        dir: &Path,
        fetcher: Arc<dyn Fetcher>,
        uv: Arc<dyn UvRunner>,
        pins: &'static [Pin],
    ) -> RuntimeManager {
        RuntimeManager::with_parts(
            dir.join("runtimes"),
            fetcher,
            Arc::new(TarArchive),
            Some(Target::Aarch64Apple),
            uv,
            pins,
        )
    }

    fn wait_finished(m: &RuntimeManager, rt: ManagedRuntime) -> Job {
        for _ in 0..500 {
            if let Some(j) = m.jobs().into_iter().find(|j| j.runtime == rt && j.finished) {
                return j;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("任务未在 5s 内结束");
    }

    fn node_pins(url: &str, sha: String) -> &'static [Pin] {
        Box::leak(vec![fake_pin("node", "24.21.0", url, sha, "bin/tool")].into_boxed_slice())
    }

    #[test]
    fn the_node_plan_lists_exactly_the_pinned_download_and_destination() {
        let d = tempfile::tempdir().unwrap();
        let pins = node_pins("https://nodejs.org/x.tar.gz", "a".repeat(64));
        let m = manager(
            d.path(),
            CountingFetcher::ok(d.path().into()),
            Arc::new(FakeUv::default()),
            pins,
        );
        let plan = m.plan(ManagedRuntime::Node).unwrap();
        assert_eq!(plan.runtime, ManagedRuntime::Node);
        assert_eq!(
            plan.versions,
            vec![("node".to_string(), "24.21.0".to_string())]
        );
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].url, "https://nodejs.org/x.tar.gz");
        assert_eq!(plan.downloads[0].sha256, Some("a".repeat(64)));
        assert!(plan.dest.ends_with("node/24.21.0"));
        assert!(plan.will_do.iter().any(|w| w.contains("SHA-256")));
        assert!(plan.will_do.iter().any(|w| w.contains("不修改系统环境")));
    }

    #[test]
    fn the_python_plan_discloses_that_python_itself_is_verified_by_uv_not_by_a_pinned_checksum() {
        let d = tempfile::tempdir().unwrap();
        let uv_pins: &'static [Pin] = Box::leak(
            vec![fake_pin(
                "uv",
                "0.12.23",
                "https://github.com/uv.tar.gz",
                "b".repeat(64),
                "uv",
            )]
            .into_boxed_slice(),
        );
        let m = manager(
            d.path(),
            CountingFetcher::ok(d.path().into()),
            Arc::new(FakeUv::default()),
            uv_pins,
        );
        let plan = m.plan(ManagedRuntime::Python).unwrap();
        assert_eq!(plan.downloads.len(), 2);
        assert_eq!(plan.downloads[1].sha256, None);
        assert!(plan.downloads[1].note.contains("uv 内置"));
        assert!(plan.will_do.iter().any(|w| w.contains("uv python install")));
    }

    #[test]
    fn an_unsupported_target_cannot_be_planned_or_installed() {
        let d = tempfile::tempdir().unwrap();
        let pins = node_pins("https://nodejs.org/x.tar.gz", "a".repeat(64));
        let m = RuntimeManager::with_parts(
            d.path().join("runtimes"),
            CountingFetcher::ok(d.path().into()),
            Arc::new(TarArchive),
            None,
            Arc::new(FakeUv::default()),
            pins,
        );
        assert!(matches!(
            m.plan(ManagedRuntime::Node),
            Err(RtError::Unsupported(_))
        ));
    }

    #[test]
    fn a_changed_plan_is_refused_and_nothing_is_downloaded() {
        let d = tempfile::tempdir().unwrap();
        let pins = node_pins("https://nodejs.org/x.tar.gz", "a".repeat(64));
        let fetcher = CountingFetcher::ok(d.path().into());
        let m = manager(d.path(), fetcher.clone(), Arc::new(FakeUv::default()), pins);
        let good = m.plan(ManagedRuntime::Node).unwrap();

        for mutate in [
            |p: &mut InstallPlanRt| p.downloads[0].url = "https://evil/x.tar.gz".into(),
            |p: &mut InstallPlanRt| p.downloads[0].sha256 = Some("c".repeat(64)),
            |p: &mut InstallPlanRt| p.versions[0].1 = "99.0.0".into(),
        ] {
            let mut bad = good.clone();
            mutate(&mut bad);
            assert!(matches!(m.start_install(&bad), Err(RtError::PlanChanged)));
        }
        assert_eq!(fetcher.calls(), 0, "改过的计划不得触发任何下载");
        assert!(m.jobs().is_empty());
    }

    #[test]
    fn an_approved_install_runs_in_the_background_and_reports_progress_then_finishes() {
        let d = tempfile::tempdir().unwrap();
        let (tar, sha) = tar_gz(d.path(), "#!/bin/sh\necho v1\n");
        let pins = Box::leak(
            vec![Pin {
                name: "node",
                version: "24.21.0",
                target: Target::Aarch64Apple,
                url: leak("https://nodejs.org/x.tar.gz".into()),
                sha256: leak(sha),
                strip_components: 0,
                bin_rel: "bin/tool",
            }]
            .into_boxed_slice(),
        );
        let m = manager(
            d.path(),
            CountingFetcher::ok(tar),
            Arc::new(FakeUv::default()),
            pins,
        );
        let plan = m.plan(ManagedRuntime::Node).unwrap();
        m.start_install(&plan).unwrap();
        let job = wait_finished(&m, ManagedRuntime::Node);
        assert!(job.failed.is_none(), "{job:?}");
        assert_eq!(job.phase, "done");
        assert!(job.done > 0);
        assert_eq!(m.installed(ManagedRuntime::Node), vec!["24.21.0"]);
    }

    #[test]
    fn a_second_install_of_the_same_runtime_while_one_is_running_conflicts() {
        let d = tempfile::tempdir().unwrap();
        let (tar, sha) = tar_gz(d.path(), "#!/bin/sh\necho v1\n");
        let pins = Box::leak(
            vec![Pin {
                name: "node",
                version: "24.21.0",
                target: Target::Aarch64Apple,
                url: leak("https://nodejs.org/x.tar.gz".into()),
                sha256: leak(sha),
                strip_components: 0,
                bin_rel: "bin/tool",
            }]
            .into_boxed_slice(),
        );
        let m = manager(
            d.path(),
            CountingFetcher::blocking(tar),
            Arc::new(FakeUv::default()),
            pins,
        );
        let plan = m.plan(ManagedRuntime::Node).unwrap();
        m.start_install(&plan).unwrap();
        assert!(matches!(m.start_install(&plan), Err(RtError::Conflict(_))));
        m.cancel_all_and_join();
    }

    #[test]
    fn a_failed_install_is_reported_and_a_retry_is_allowed() {
        let d = tempfile::tempdir().unwrap();
        let pins = node_pins("https://nodejs.org/x.tar.gz", "a".repeat(64));
        let m = manager(
            d.path(),
            CountingFetcher::failing("网络不通") as Arc<dyn Fetcher>,
            Arc::new(FakeUv::default()),
            pins,
        );
        let plan = m.plan(ManagedRuntime::Node).unwrap();
        m.start_install(&plan).unwrap();
        let job = wait_finished(&m, ManagedRuntime::Node);
        assert_eq!(job.phase, "failed");
        assert!(job.failed.as_ref().unwrap().contains("网络不通"), "{job:?}");

        // 再起一次(会再次失败,但不应是 Conflict)。
        m.start_install(&plan).unwrap();
        let job2 = wait_finished(&m, ManagedRuntime::Node);
        assert!(job2.failed.is_some());
    }

    #[test]
    fn python_install_runs_uv_after_installing_uv_and_skips_uv_when_already_present() {
        let d = tempfile::tempdir().unwrap();
        let (uv_tar, uv_sha) = tar_gz_uv(d.path());
        let pins = Box::leak(
            vec![fake_pin(
                "uv",
                "0.12.23",
                "https://github.com/uv.tar.gz",
                uv_sha,
                "uv",
            )]
            .into_boxed_slice(),
        );
        let fetcher = CountingFetcher::ok(uv_tar);
        let uv = Arc::new(FakeUv::default());
        let m = manager(d.path(), fetcher.clone(), uv.clone(), pins);

        let plan = m.plan(ManagedRuntime::Python).unwrap();
        m.start_install(&plan).unwrap();
        let job = wait_finished(&m, ManagedRuntime::Python);
        assert!(job.failed.is_none(), "{job:?}");
        let calls = uv.calls();
        assert_eq!(calls.len(), 1);
        let (_uvbin, version, install_dir, _cache) = &calls[0];
        assert_eq!(version, "3.13");
        assert!(install_dir.starts_with(d.path().join("runtimes")));
        let uv_downloads = fetcher.calls();

        // 第二次:uv 已在 store,不应再下载 uv。
        m.start_install(&plan).unwrap();
        let job2 = wait_finished(&m, ManagedRuntime::Python);
        assert!(job2.failed.is_none(), "{job2:?}");
        assert_eq!(fetcher.calls(), uv_downloads, "uv 已装不该再下载");
        assert_eq!(uv.calls().len(), 2, "python_install 应再跑一次");
    }

    #[test]
    fn cancel_all_stops_a_running_download_and_cleans_staging() {
        let d = tempfile::tempdir().unwrap();
        let (tar, sha) = tar_gz(d.path(), "#!/bin/sh\necho v1\n");
        let pins = Box::leak(
            vec![Pin {
                name: "node",
                version: "24.21.0",
                target: Target::Aarch64Apple,
                url: leak("https://nodejs.org/x.tar.gz".into()),
                sha256: leak(sha),
                strip_components: 0,
                bin_rel: "bin/tool",
            }]
            .into_boxed_slice(),
        );
        let m = manager(
            d.path(),
            CountingFetcher::blocking(tar),
            Arc::new(FakeUv::default()),
            pins,
        );
        let plan = m.plan(ManagedRuntime::Node).unwrap();
        m.start_install(&plan).unwrap();
        // 等任务真的开跑。
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        m.cancel_all_and_join();
        assert!(start.elapsed() < Duration::from_secs(2), "取消不应拖住退出");
        let leftovers: Vec<_> = std::fs::read_dir(d.path().join("runtimes"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn uninstall_removes_one_version_and_rejects_names_that_escape() {
        let d = tempfile::tempdir().unwrap();
        let pins = node_pins("https://nodejs.org/x.tar.gz", "a".repeat(64));
        let m = manager(
            d.path(),
            CountingFetcher::ok(d.path().into()),
            Arc::new(FakeUv::default()),
            pins,
        );
        std::fs::create_dir_all(m.store().version_dir("node", "24.21.0")).unwrap();
        m.uninstall(ManagedRuntime::Node, "24.21.0").unwrap();
        assert!(m.installed(ManagedRuntime::Node).is_empty());
        assert!(m.uninstall(ManagedRuntime::Node, "../evil").is_err());
    }

    /// 造一个顶层就是 `uv` 的归档(strip 0,bin_rel = "uv")。
    fn tar_gz_uv(dir: &Path) -> (PathBuf, String) {
        use crate::digest::sha256_hex;
        let src = dir.join(format!("uvsrc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&src).unwrap();
        let uv = src.join("uv");
        std::fs::write(&uv, "#!/bin/sh\necho uv\n").unwrap();
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = dir.join(format!("uv-{}.tar.gz", uuid::Uuid::new_v4()));
        let status = Command::new("/usr/bin/tar")
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

    /// 退出收尾之后再来的安装请求被拒绝,而且没有发起任何下载、没有起任何线程。
    #[test]
    fn installs_requested_after_shutdown_began_are_refused_without_downloading() {
        let d = tempfile::tempdir().unwrap();
        let (tar, sha) = tar_gz(d.path(), "#!/bin/sh\necho v1\n");
        let pins = node_pins("https://nodejs.org/x.tar.gz", sha);
        let fetcher = CountingFetcher::ok(tar);
        let m = manager(d.path(), fetcher.clone(), Arc::new(FakeUv::default()), pins);
        let plan = m.plan(ManagedRuntime::Node).unwrap();
        m.cancel_all_and_join();
        assert!(matches!(m.start_install(&plan), Err(RtError::ShuttingDown)));
        assert_eq!(fetcher.calls(), 0);
        assert!(m.jobs().is_empty());
    }

    /// uv 在安装目录里放的 `.cache`/`.temp`/`.lock` 不是 Python 版本:不列出、不能卸载(卸载会毁掉 uv 的缓存)。
    #[test]
    fn uvs_internal_directories_are_not_listed_as_python_versions_nor_removable() {
        let d = tempfile::tempdir().unwrap();
        let pins = node_pins("https://nodejs.org/x.tar.gz", "a".repeat(64));
        let m = manager(
            d.path(),
            CountingFetcher::ok(d.path().into()),
            Arc::new(FakeUv::default()),
            pins,
        );
        let py = m.store().root().join("python");
        for dir in [
            "cpython-3.13.1-macos-aarch64-none",
            "cpython-3.12.9-macos-aarch64-none",
            ".cache",
            ".temp",
        ] {
            std::fs::create_dir_all(py.join(dir)).unwrap();
        }
        std::fs::write(py.join(".lock"), "").unwrap();
        assert_eq!(
            m.installed(ManagedRuntime::Python),
            vec![
                "cpython-3.13.1-macos-aarch64-none".to_string(),
                "cpython-3.12.9-macos-aarch64-none".to_string()
            ]
        );
        for internal in [".cache", ".temp", ".lock", "bin"] {
            assert!(
                m.uninstall(ManagedRuntime::Python, internal).is_err(),
                "{internal}"
            );
        }
        assert!(py.join(".cache").exists(), "uv 的缓存不能被卸载掉");
        m.uninstall(ManagedRuntime::Python, "cpython-3.12.9-macos-aarch64-none")
            .unwrap();
        assert_eq!(m.installed(ManagedRuntime::Python).len(), 1);
    }
}
