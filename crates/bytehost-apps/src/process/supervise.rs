//! 子进程的启动、存活检查、优雅停止与孤儿清理(unix)。
//!
//! - 每个应用一个**独立进程组**(`process_group(0)`):停止时对整组发信号,应用自己拉起的孙进程(`npm` → `node`、
//!   `uv run` → `python`)一并结束,不留孤儿;
//! - 停止 = `SIGTERM` → 宽限期 → `SIGKILL`(整组);
//! - dozerd 若被强杀,子进程会变成孤儿并继续占着端口:启动时用 `<run>/process.json` 里记下的 pid+命令行核对后清理
//!   (**核对命令行**,避免 pid 被系统复用后误杀无关进程)。

use std::ffi::OsString;
use std::io;
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::log::{RotatingLog, pump};

/// `stop` 等日志泵线程收尾的最长时间。
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

pub struct ProcessSpec {
    /// `argv[0]` 是程序(按子进程环境里的 `PATH` 查找),其余是参数。
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// **完整**环境(不继承父进程),见 [`super::env::build_env`]。
    pub env: Vec<(OsString, OsString)>,
    pub log_path: PathBuf,
    pub log_max_bytes: u64,
    pub log_keep: u32,
}

pub struct Running {
    child: Child,
    pgid: i32,
    started: Instant,
    pumps: Vec<JoinHandle<()>>,
}

fn is_gone(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ESRCH)
}

fn signal_group(pgid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: `kill` 只是发信号;pgid 来自我们自己 spawn 的子进程(`process_group(0)` 使它等于子进程 pid)。
    let r = unsafe { libc::kill(-pgid, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Running {
    pub fn spawn(spec: &ProcessSpec) -> io::Result<Self> {
        let (program, args) = spec
            .argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "argv 为空"))?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let pgid = child.id() as i32;
        let mut pumps = Vec::new();
        for reader in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let log = RotatingLog::open(&spec.log_path, spec.log_max_bytes, spec.log_keep)?;
            pumps.push(pump(reader, log));
        }
        Ok(Self {
            child,
            pgid,
            started: Instant::now(),
            pumps,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn ran_for(&self) -> Duration {
        self.started.elapsed()
    }

    /// 进程已经退出就返回它的状态(不阻塞)。
    pub fn try_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// 优雅停止整个进程组:`SIGTERM`,最多等 `grace`,还没退就 `SIGKILL`。返回领头进程的退出状态。
    pub fn stop(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        if let Some(status) = self.child.try_wait()? {
            // 领头进程已退,但它的孙进程可能还在:仍对整组补一刀。
            let _ = signal_group(self.pgid, libc::SIGKILL);
            self.join_pumps();
            return Ok(status);
        }
        match signal_group(self.pgid, libc::SIGTERM) {
            Ok(()) => {}
            Err(e) if is_gone(&e) => {}
            Err(e) => return Err(e),
        }
        let deadline = Instant::now() + grace;
        let status = loop {
            if let Some(s) = self.child.try_wait()? {
                break s;
            }
            if Instant::now() >= deadline {
                match signal_group(self.pgid, libc::SIGKILL) {
                    Ok(()) => {}
                    // 组已经不存在而领头进程还活着(进程组没建成):直接杀领头进程,绝不能无限 `wait`。
                    Err(e) if is_gone(&e) => {
                        let _ = self.child.kill();
                    }
                    Err(e) => return Err(e),
                }
                break self.child.wait()?;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // 领头进程退了,整组里残留的(忽略 SIGTERM 的孙进程)也收掉。
        let _ = signal_group(self.pgid, libc::SIGKILL);
        self.join_pumps();
        Ok(status)
    }

    /// 等日志泵线程收尾——但**有上限**:领头进程退了、整组也杀了,正常情况下管道很快就关;可万一有个逃出了进程组的
    /// 孙进程还攥着管道,`join` 会一直阻塞到它退出(几小时甚至永远),把 `stop` 卡死。超时就放手(线程会在管道最终关闭时自己结束)。
    fn join_pumps(&mut self) {
        let deadline = Instant::now() + PUMP_JOIN_TIMEOUT;
        for h in self.pumps.drain(..) {
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // 被丢掉时不留子进程(例如 manager 出错路径上忘了 stop)。
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = signal_group(self.pgid, libc::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

/// 在回环上拿一个当前空闲的端口(绑 0 再释放;释放到子进程绑定之间有极小的竞争窗口,调用方遇到
/// "子进程起不来/端口被占"按普通启动失败处理即可)。
pub fn pick_free_port() -> io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

// ---------------------------------------------------------------------------------------------
// 孤儿清理
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub pid: u32,
    pub argv: Vec<String>,
}

fn record_path(run_dir: &Path) -> PathBuf {
    run_dir.join("process.json")
}

pub fn write_record(run_dir: &Path, record: &RunRecord) -> io::Result<()> {
    std::fs::create_dir_all(run_dir)?;
    let tmp = run_dir.join("process.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(record).map_err(io::Error::other)?)?;
    std::fs::rename(&tmp, record_path(run_dir))
}

pub fn clear_record(run_dir: &Path) {
    let _ = std::fs::remove_file(record_path(run_dir));
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reaped {
    /// 没有遗留记录。
    Nothing,
    /// 记录里的进程已经不在了(或 pid 被无关进程复用,命令行对不上):只清掉记录,**不动**那个进程。
    Stale,
    /// 找到并结束了遗留的进程组。
    Killed(u32),
}

/// 读进程 `pid` 的完整命令行(`ps -ww -o command= -p`);进程不存在返回 `None`。
fn command_line(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// 命令行是否就是我们当初启动的那条(`ps` 把 argv 用空格拼起来,所以按拼接后的字符串比较)。
fn command_matches(actual: &str, argv: &[String]) -> bool {
    actual == argv.join(" ")
}

/// dozerd 启动时对每个应用调用:有遗留记录且核对一致就结束整个进程组。
pub fn reap_orphan(run_dir: &Path, grace: Duration) -> io::Result<Reaped> {
    let path = record_path(run_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Reaped::Nothing),
        Err(e) => return Err(e),
    };
    let record: RunRecord = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => {
            clear_record(run_dir);
            return Ok(Reaped::Stale);
        }
    };
    let alive = command_line(record.pid).filter(|c| command_matches(c, &record.argv));
    let Some(_) = alive else {
        clear_record(run_dir);
        return Ok(Reaped::Stale);
    };
    let pgid = record.pid as i32;
    let _ = signal_group(pgid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && command_line(record.pid).is_some() {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = signal_group(pgid, libc::SIGKILL);
    clear_record(run_dir);
    Ok(Reaped::Killed(record.pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dir: &Path, script: &str) -> ProcessSpec {
        ProcessSpec {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            log_path: dir.join("logs/app.log"),
            log_max_bytes: 1 << 20,
            log_keep: 2,
        }
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < end, "超时: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn output_goes_to_the_log_and_the_exit_status_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Running::spawn(&spec(
            dir.path(),
            "echo hello-out; echo hello-err 1>&2; exit 3",
        ))
        .unwrap();
        wait_until("退出", || r.try_exit().unwrap().is_some());
        let status = r.stop(Duration::from_secs(1)).unwrap();
        assert_eq!(status.code(), Some(3));
        let log = std::fs::read_to_string(dir.path().join("logs/app.log")).unwrap();
        assert!(
            log.contains("hello-out") && log.contains("hello-err"),
            "{log}"
        );
    }

    #[test]
    fn the_child_gets_only_the_given_environment() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY(测试): 只在本进程环境里加一个变量,随后用 env_clear 验证它没传给子进程。
        unsafe { std::env::set_var("BYTEHOST_TEST_SECRET", "leak") };
        let mut s = spec(dir.path(), "env");
        s.env
            .push((OsString::from("ONLY_THIS"), OsString::from("yes")));
        let mut r = Running::spawn(&s).unwrap();
        wait_until("退出", || r.try_exit().unwrap().is_some());
        r.stop(Duration::from_secs(1)).unwrap();
        let log = std::fs::read_to_string(dir.path().join("logs/app.log")).unwrap();
        assert!(log.contains("ONLY_THIS=yes"), "{log}");
        assert!(
            !log.contains("BYTEHOST_TEST_SECRET"),
            "不得继承父进程环境: {log}"
        );
    }

    #[test]
    fn stop_terminates_the_whole_process_group_including_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let mut r = Running::spawn(&spec(dir.path(), &script)).unwrap();
        wait_until("孙进程 pid 写出", || {
            pidfile.exists() && !std::fs::read_to_string(&pidfile).unwrap().trim().is_empty()
        });
        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(grandchild));
        r.stop(Duration::from_secs(2)).unwrap();
        wait_until("孙进程结束", || !alive(grandchild));
    }

    /// 逃出进程组(自己 `setsid`)又攥着输出管道的孙进程:`stop` 不能被它卡住(否则 dozerd 的停止/退出会挂死)。
    #[test]
    fn stop_does_not_hang_on_a_process_that_escaped_the_group_and_holds_the_pipe() {
        if Command::new("perl").arg("-v").output().is_err() {
            eprintln!("跳过:没有 perl");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("escapee.pid");
        // perl 先 setsid 再 exec sleep(新会话/新进程组,但继承了 stdout/stderr 管道)
        let script = format!(
            "perl -MPOSIX -e 'setsid(); open(F, \">{}\"); print F $$; close F; exec \"sleep\", \"20\"' & wait",
            pidfile.display()
        );
        let mut r = Running::spawn(&spec(dir.path(), &script)).unwrap();
        wait_until("逃逸进程写出 pid", || {
            std::fs::read_to_string(&pidfile)
                .map(|t| !t.trim().is_empty())
                .unwrap_or(false)
        });
        let escapee: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let started = Instant::now();
        r.stop(Duration::from_secs(1)).unwrap();
        let took = started.elapsed();
        unsafe { libc::kill(escapee, libc::SIGKILL) };
        assert!(
            took < Duration::from_secs(5),
            "stop 被攥着管道的进程卡住了 {took:?}"
        );
    }

    #[test]
    fn a_child_that_ignores_sigterm_is_killed_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let script = format!(
            "trap '' TERM; touch {}; while :; do sleep 1; done",
            ready.display()
        );
        let mut r = Running::spawn(&spec(dir.path(), &script)).unwrap();
        wait_until("子进程就绪(已忽略 TERM)", || ready.exists());
        let started = Instant::now();
        let status = r.stop(Duration::from_millis(300)).unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "先给了宽限期"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!status.success());
    }

    #[test]
    fn dropping_a_running_process_does_not_leave_it_behind() {
        let dir = tempfile::tempdir().unwrap();
        let r = Running::spawn(&spec(dir.path(), "sleep 300")).unwrap();
        let pid = r.pid() as i32;
        assert!(alive(pid));
        drop(r);
        wait_until("被 Drop 掉的进程结束", || !alive(pid));
    }

    #[test]
    fn a_missing_program_and_an_empty_argv_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), "");
        s.argv = vec!["definitely-not-a-program-xyz".into()];
        assert!(Running::spawn(&s).is_err());
        s.argv = vec![];
        assert_eq!(
            Running::spawn(&s).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn free_ports_are_loopback_bindable() {
        // 选出来到绑定之间有竞争窗口(文档里写明了),并行的别的测试可能恰好抢走:重试几次再下结论。
        let bindable = (0..20).any(|_| {
            let p = pick_free_port().unwrap();
            TcpListener::bind(("127.0.0.1", p)).is_ok()
        });
        assert!(bindable);
    }

    #[test]
    fn a_leftover_process_with_a_matching_command_line_is_killed_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let argv: Vec<String> = ["sleep".into(), "301".into()].to_vec();
        // 模拟"dozerd 被强杀后留下的孤儿":自己起一个独立进程组的 sleep,不经 Running(没人会 stop 它)
        let child = Command::new("sleep")
            .arg("301")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        write_record(dir.path(), &RunRecord { pid, argv }).unwrap();
        let mut child = child;
        assert_eq!(
            reap_orphan(dir.path(), Duration::from_secs(1)).unwrap(),
            Reaped::Killed(pid)
        );
        let _ = child.wait();
        assert!(!alive(pid as i32));
        assert!(!record_path(dir.path()).exists(), "记录被清掉");
    }

    /// pid 被系统复用给了无关进程:命令行对不上就**不能杀**。
    #[test]
    fn a_process_with_a_different_command_line_is_never_killed() {
        let dir = tempfile::tempdir().unwrap();
        let mut innocent = Command::new("sleep")
            .arg("302")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = innocent.id();
        write_record(
            dir.path(),
            &RunRecord {
                pid,
                argv: vec!["node".into(), "server.js".into()],
            },
        )
        .unwrap();
        assert_eq!(
            reap_orphan(dir.path(), Duration::from_millis(200)).unwrap(),
            Reaped::Stale
        );
        assert!(alive(pid as i32), "无关进程不能被杀");
        assert!(!record_path(dir.path()).exists());
        innocent.kill().unwrap();
        let _ = innocent.wait();
    }

    #[test]
    fn missing_dead_and_corrupt_records_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            reap_orphan(dir.path(), Duration::from_millis(100)).unwrap(),
            Reaped::Nothing
        );
        // 已经退出的进程
        let mut gone = Command::new("true").spawn().unwrap();
        let pid = gone.id();
        gone.wait().unwrap();
        write_record(
            dir.path(),
            &RunRecord {
                pid,
                argv: vec!["true".into()],
            },
        )
        .unwrap();
        assert_eq!(
            reap_orphan(dir.path(), Duration::from_millis(100)).unwrap(),
            Reaped::Stale
        );
        std::fs::write(record_path(dir.path()), "{not json").unwrap();
        assert_eq!(
            reap_orphan(dir.path(), Duration::from_millis(100)).unwrap(),
            Reaped::Stale
        );
        assert!(!record_path(dir.path()).exists());
    }

    #[test]
    fn command_lines_are_compared_as_joined_argv() {
        assert!(command_matches(
            "node server.js --port 1",
            &[
                "node".into(),
                "server.js".into(),
                "--port".into(),
                "1".into()
            ]
        ));
        assert!(!command_matches("node server.js", &["node".into()]));
    }
}
