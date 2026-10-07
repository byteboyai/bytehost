//! 有大小上限、会轮转的应用日志。子进程的 stdout/stderr 经管道泵进 [`RotatingLog`](不让子进程直接写文件:
//! 一个跑了几个月的应用不能把磁盘写满)。`path` → `path.1` → … → `path.<keep>`,最旧的被丢弃。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    file: File,
    written: u64,
}

impl RotatingLog {
    pub fn open(path: &Path, max_bytes: u64, keep: u32) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes: max_bytes.max(1),
            keep: keep.max(1),
            file,
            written,
        })
    }

    fn numbered(&self, n: u32) -> PathBuf {
        let mut os = self.path.clone().into_os_string();
        os.push(format!(".{n}"));
        PathBuf::from(os)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        for n in (1..self.keep).rev() {
            let from = self.numbered(n);
            if from.exists() {
                fs::rename(&from, self.numbered(n + 1))?;
            }
        }
        fs::rename(&self.path, self.numbered(1))?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// stdout 与 stderr 的两个泵线程**共用同一份**日志(各自开一份会各数各的字节、互相把对方的文件轮转走)。
pub type SharedLog = Arc<Mutex<RotatingLog>>;

pub fn shared(log: RotatingLog) -> SharedLog {
    Arc::new(Mutex::new(log))
}

/// 把 `reader`(子进程的管道)一直读到 EOF,写进 `log`。线程在管道关闭(子进程退出)时结束。
/// 写日志失败(磁盘满、文件被删)时**继续读空管道并丢弃数据**:管道的读端一关,应用下一次写就会 SIGPIPE/EPIPE 崩掉。
pub fn pump<R: Read + Send + 'static>(mut reader: R, log: SharedLog) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = log.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = g.write_all(&buf[..n]);
                }
            }
        }
        let _ = log.lock().unwrap_or_else(|e| e.into_inner()).flush();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_rotate_at_the_size_limit_and_keep_a_bounded_history() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.log");
        let mut log = RotatingLog::open(&p, 10, 2).unwrap();
        for chunk in ["aaaaaa", "bbbbbb", "cccccc", "dddddd"] {
            log.write_all(chunk.as_bytes()).unwrap();
        }
        log.flush().unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "dddddd");
        assert_eq!(
            fs::read_to_string(dir.path().join("app.log.1")).unwrap(),
            "cccccc"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("app.log.2")).unwrap(),
            "bbbbbb"
        );
        assert!(!dir.path().join("app.log.3").exists(), "只留 keep=2 份历史");
    }

    #[test]
    fn a_single_oversized_write_is_not_split_or_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.log");
        let mut log = RotatingLog::open(&p, 4, 1).unwrap();
        log.write_all(b"0123456789").unwrap();
        log.flush().unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "0123456789");
    }

    #[test]
    fn reopening_appends_and_counts_what_is_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.log");
        {
            let mut log = RotatingLog::open(&p, 10, 1).unwrap();
            log.write_all(b"12345678").unwrap();
        }
        let mut log = RotatingLog::open(&p, 10, 1).unwrap();
        log.write_all(b"abc").unwrap();
        log.flush().unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("app.log.1")).unwrap(),
            "12345678"
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "abc");
    }

    #[test]
    fn the_pump_copies_a_pipe_into_the_log_until_eof() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.log");
        let log = RotatingLog::open(&p, 1024, 1).unwrap();
        let data: &'static [u8] = b"line1\nline2\n";
        pump(data, shared(log)).join().unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "line1\nline2\n");
    }

    /// 写日志失败(这里:文件被删导致轮转 rename 失败)后,泵仍把管道读到 EOF——应用不会因 EPIPE 崩。
    #[test]
    fn a_failing_log_never_stops_the_pump_from_draining_the_pipe() {
        struct Counting(
            &'static [u8],
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        );
        impl Read for Counting {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(4);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                self.1.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
                Ok(n)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.log");
        let mut log = RotatingLog::open(&p, 4, 1).unwrap();
        log.write_all(b"ab").unwrap();
        fs::remove_file(&p).unwrap(); // 之后一轮转,rename 就会 ENOENT
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let data: &'static [u8] = b"0123456789abcdefghij";
        pump(Counting(data, count.clone()), shared(log))
            .join()
            .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), data.len());
    }
}
