//! 同一数据根的**服务所有者互斥**:`<root>/service.lock` 上的一把 `flock(2)` 独占锁。
//!
//! 规格 §6.1:"同一数据根只允许一个服务所有者……缺失时在共享层加锁并验证双启动"。Dozer 与 Digger
//! 各自的产品数据根不同(`ai.byteboy.dozer` / `ai.byteboy.digger`),但都经 `bytehost-apps` 起服务;
//! 若同一个数据根被起两次(同进程两个 `AppService`,或两个进程指向同一 root),第二个必须变成"不可用",
//! **绝不能**去动第一个正在管的 gateway/应用进程。
//!
//! 选 `flock(2)` 而不是 pid 文件(自有锁实现):内核在持有者进程**退出时自动释放**(含崩溃/被 kill),
//! 所以强制退出后下一次启动能立刻拿到锁,不需要"陈旧锁"清理逻辑,也不会误判"还活着"。锁按 fd 归属,
//! 同一进程里对同一文件另开一个 fd 再 `LOCK_EX|LOCK_NB` 同样失败——同进程双启动也能被挡住。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// 数据根里锁文件的固定名字(`<root>/service.lock`)。
const LOCK_FILE: &str = "service.lock";

/// 给已打开的 fd 打上 `FD_CLOEXEC`(双保险,不依赖 `open` 时的 `O_CLOEXEC`)。
fn set_cloexec(file: &File) {
    // SAFETY: `fcntl(F_SETFD)` 只改这个 fd 的标志位,不改文件内容。
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

#[derive(Debug)]
pub enum LockError {
    /// 同一数据根已经有服务所有者(同进程双启动或另一个进程持有)。
    AlreadyOwned,
    /// 打不开/建不了锁文件。
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::AlreadyOwned => write!(f, "该数据根已有一个应用服务在运行"),
            LockError::Io(e) => write!(f, "服务锁不可用:{e}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::AlreadyOwned => None,
            LockError::Io(e) => Some(e),
        }
    }
}

/// 持有期间锁有效;`Drop` 关掉 fd 即释放(内核自动,崩溃亦然)。
#[derive(Debug)]
pub struct ServiceLock {
    _file: File,
    path: PathBuf,
}

impl ServiceLock {
    /// 尝试独占 `<root>/service.lock`。拿不到返回 [`LockError::AlreadyOwned`],不阻塞、不等待。
    pub fn acquire(root: &Path) -> Result<Self, LockError> {
        use std::os::unix::fs::OpenOptionsExt;
        fs::create_dir_all(root).map_err(LockError::Io)?;
        let path = root.join(LOCK_FILE);
        // **O_CLOEXEC 必须显式设**:这个 fd 会被 `bytehost-apps` 后面 spawn 的应用进程继承,
        // 而 `flock` 锁随 fd 的存在而存在——子进程若继承了它,即使本进程退出锁也不会释放,
        // 下一次启动就会误判"已有服务在运行"。Rust 的 `File::open` 默认带 CLOEXEC,但显式钉死更稳。
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
            .map_err(LockError::Io)?;
        set_cloexec(&file);
        // SAFETY: `flock` 只操作这个已打开的 fd;`LOCK_NB` 保证不阻塞。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(LockError::AlreadyOwned);
            }
            return Err(LockError::Io(err));
        }
        Ok(Self { _file: file, path })
    }

    /// 锁文件路径(测试断言用)。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨进程那个用例会 spawn 子进程;子进程会继承本进程当时打开的所有 fd,而 `flock` 锁随 fd 存续——
    /// 并发跑时它可能"顺手"多留一份别的用例的锁 fd。给这些用例上一把测试内串行锁,避免这种互扰。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_second_acquire_on_the_same_root_is_rejected_until_the_first_drops() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let first = ServiceLock::acquire(tmp.path()).expect("首次应拿到锁");
        match ServiceLock::acquire(tmp.path()) {
            Err(LockError::AlreadyOwned) => {}
            other => panic!("二次应被拒:{other:?}"),
        }
        // 释放后可以再拿。
        drop(first);
        let again = ServiceLock::acquire(tmp.path()).expect("释放后应能再拿");
        assert_eq!(again.path(), tmp.path().join(LOCK_FILE));
    }

    #[test]
    fn the_lock_file_is_created_under_the_root() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let lock = ServiceLock::acquire(tmp.path()).unwrap();
        assert!(lock.path().is_file(), "锁文件应落在 <root>/service.lock");
    }

    /// 跨进程:`flock(2)` 是文件(而非进程)级的独占锁——**另一个进程**持锁时,本进程也拿不到。
    /// 用系统 `flock(1)` 等价物(`python3 -c 'fcntl.flock'`)撑着子进程,不新增依赖。
    #[cfg(feature = "server")]
    #[test]
    fn another_process_holding_the_lock_blocks_this_one() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LOCK_FILE);
        // 子进程:打开文件、`flock LOCK_EX`,写一行 "locked",然后等父进程关掉它的 stdin。
        let script = r#"
import fcntl, sys
f = open(sys.argv[1], 'w')
fcntl.flock(f, fcntl.LOCK_EX)
sys.stdout.write('locked\n'); sys.stdout.flush()
sys.stdin.readline()
"#;
        let mut child = match Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return, // 没有 python3:跳过(与其他测试同样的口径)
        };
        // 等它确实持锁。
        {
            use std::io::BufRead as _;
            let stdout = child.stdout.take().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "locked");
        }
        match ServiceLock::acquire(tmp.path()) {
            Err(LockError::AlreadyOwned) => {}
            other => panic!("另一个进程持锁时本进程应被拒:{other:?}"),
        }
        // 放行子进程、等它退出;退出即释放,本进程随后能拿到。
        let _ = child.stdin.take().map(|mut s| s.write_all(b"\n"));
        let _ = child.wait();
        assert!(
            ServiceLock::acquire(tmp.path()).is_ok(),
            "另一个进程退出后应能拿到锁"
        );
    }
}
