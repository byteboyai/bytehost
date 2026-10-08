//! 下载与解压的**接口**与两个真实实现(A6d):下载调系统 `/usr/bin/curl`,解压调系统 `/usr/bin/tar`。
//!
//! 不给 dozerd 引入 TLS/HTTP 客户端依赖(与"核心不依赖 Node/Python"的思路一致);校验和由我们自己的
//! `sha2` 计算。测试用假 `Fetcher`,真 `TarArchive`(系统 `tar`)。

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 一次下载的元信息:重定向后的最终 URL 与实际落盘字节数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchMeta {
    pub effective_url: String,
    pub bytes: u64,
}

/// 把 `url` 下载到 `dest`。
pub trait Fetcher: Send + Sync {
    /// 下载;`on_progress(done, total)` 报告进度(总长未知时 `total = None`);
    /// `cancel` 置位时**尽快中止**并返回 `Err(Interrupted)`。
    fn fetch(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> io::Result<()>;

    /// 下载并报告最终 URL/字节数。默认实现:调 `fetch`,用 `dest` 长度与传入 URL 填充。
    /// `CurlFetcher` 覆盖为真实值(重定向后的最终 URL、实际字节数)。
    fn fetch_meta(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> io::Result<FetchMeta> {
        let _ = max_bytes;
        self.fetch(url, dest, on_progress, cancel)?;
        let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        Ok(FetchMeta {
            effective_url: url.to_string(),
            bytes,
        })
    }
}

/// 列出/解压一个归档。
pub trait Archive: Send + Sync {
    /// 列出所有条目名(`tar -tf`)。
    fn list(&self, archive: &Path) -> io::Result<Vec<String>>;
    /// 解压到 `into`(`tar -xf --no-same-owner --strip-components=N`)。
    fn extract(&self, archive: &Path, into: &Path, strip_components: u32) -> io::Result<()>;
}

/// 用系统 `/usr/bin/curl` 下载。**只信 HTTPS**:`--proto =https --proto-redir =https`,
/// 并校验 TLS≥1.2、跟随重定向、失败非 0 退出、限 400MB。
pub struct CurlFetcher;

/// `curl` 的参数拼装(纯函数,便于单测;不包括 `-o <dest>` 与 URL)。
/// `-w %{url_effective}` 让 curl 把重定向后的最终 URL 写到 stdout。
pub fn curl_args(url: &str, dest: &Path, max_bytes: u64) -> Vec<OsString> {
    let _ = url;
    vec![
        "--proto".into(),
        "=https".into(),
        "--proto-redir".into(),
        "=https".into(),
        "--tlsv1.2".into(),
        "--fail".into(),
        "--location".into(),
        "--silent".into(),
        "--show-error".into(),
        "--max-filesize".into(),
        max_bytes.to_string().into(),
        // 卡住的服务器不能把调用方永远挂住:连不上 20s 放弃;持续低于 1 KiB/s 达 30s 放弃;整体上限 1h。
        "--connect-timeout".into(),
        "20".into(),
        "--speed-limit".into(),
        "1024".into(),
        "--speed-time".into(),
        "30".into(),
        "--max-time".into(),
        "3600".into(),
        "-w".into(),
        "%{url_effective}".into(),
        "-o".into(),
        dest.as_os_str().to_os_string(),
    ]
}

impl CurlFetcher {
    /// 下载并返回重定向后的最终 URL 与实际字节数。`max_bytes` 通过 `--max-filesize` 交给 curl。
    pub fn fetch_with_meta(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> io::Result<FetchMeta> {
        let total = curl_content_length(url);

        let mut child = Command::new("/usr/bin/curl")
            .args(curl_args(url, dest, max_bytes))
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // 轮询子进程:每 100ms 看是否退出,并检查 cancel;置位就 kill + wait。
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::Interrupted, "下载已取消"));
            }
            match child.try_wait()? {
                Some(status) => {
                    if status.success() {
                        let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                        on_progress(bytes, total);
                        if bytes > max_bytes {
                            return Err(io::Error::other(format!(
                                "下载超出上限({bytes} > {max_bytes} 字节)"
                            )));
                        }
                        let effective_url = read_child_stdout(&mut child)
                            .lines()
                            .last()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        return Ok(FetchMeta {
                            effective_url,
                            bytes,
                        });
                    }
                    let stderr = read_child_stderr(&mut child);
                    let last = stderr.lines().last().unwrap_or("curl 失败").to_string();
                    return Err(io::Error::other(last));
                }
                None => {
                    let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                    on_progress(done, total);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

impl Fetcher for CurlFetcher {
    fn fetch(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        // 运行时下载沿用 A6d 的 400MB 上限。
        self.fetch_with_meta(url, dest, 400_000_000, on_progress, cancel)
            .map(|_| ())
    }

    fn fetch_meta(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> io::Result<FetchMeta> {
        self.fetch_with_meta(url, dest, max_bytes, on_progress, cancel)
    }
}

/// HEAD 请求取 `Content-Length`;拿不到返回 `None`。
fn curl_content_length(url: &str) -> Option<u64> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--location",
            "--silent",
            "--head",
            "--connect-timeout",
            "10",
            "--max-time",
            "20",
        ])
        .arg(url)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().eq_ignore_ascii_case("content-length") {
            v.trim().parse::<u64>().ok()
        } else {
            None
        }
    })
}

fn read_child_stderr(child: &mut std::process::Child) -> String {
    use std::io::Read;
    let Some(mut e) = child.stderr.take() else {
        return String::new();
    };
    let mut s = String::new();
    let _ = e.read_to_string(&mut s);
    s
}

fn read_child_stdout(child: &mut std::process::Child) -> String {
    use std::io::Read;
    let Some(mut o) = child.stdout.take() else {
        return String::new();
    };
    let mut s = String::new();
    let _ = o.read_to_string(&mut s);
    s
}

/// 用系统 `/usr/bin/tar` 列出/解压。
pub struct TarArchive;

impl Archive for TarArchive {
    fn list(&self, archive: &Path) -> io::Result<Vec<String>> {
        let out = Command::new("/usr/bin/tar")
            .arg("-tf")
            .arg(archive)
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "tar -tf 失败:{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect())
    }

    fn extract(&self, archive: &Path, into: &Path, strip_components: u32) -> io::Result<()> {
        std::fs::create_dir_all(into)?;
        let mut cmd = Command::new("/usr/bin/tar");
        cmd.arg("-xf")
            .arg(archive)
            .arg("--no-same-owner")
            .arg("-C")
            .arg(into);
        if strip_components > 0 {
            cmd.arg(format!("--strip-components={strip_components}"));
        }
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "tar -xf 失败:{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_args_pin_https_and_report_the_effective_url() {
        let dest = Path::new("/tmp/out.bin");
        let args = curl_args("https://example.com/a.zip", dest, 209715200);
        let s: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // 只信 https:主请求与重定向都钉死协议。
        assert!(s.windows(2).any(|w| w == ["--proto", "=https"]));
        assert!(s.windows(2).any(|w| w == ["--proto-redir", "=https"]));
        assert!(s.iter().any(|a| a == "--tlsv1.2"));
        assert!(s.iter().any(|a| a == "--fail"));
        assert!(s.iter().any(|a| a == "--location"));
        assert!(s.windows(2).any(|w| w == ["--max-filesize", "209715200"]));
        assert!(s.windows(2).any(|w| w == ["-w", "%{url_effective}"]));
        // 卡住的服务器必须有超时/低速中止,否则下载会永远挂住。
        assert!(s.windows(2).any(|w| w == ["--connect-timeout", "20"]));
        assert!(s.windows(2).any(|w| w == ["--speed-limit", "1024"]));
        assert!(s.windows(2).any(|w| w == ["--speed-time", "30"]));
        assert!(s.windows(2).any(|w| w == ["--max-time", "3600"]));
        assert!(s.windows(2).any(|w| w == ["-o", "/tmp/out.bin"]));
        // URL 不在参数里,由调用方单独 .arg(url) 追加。
        assert!(!s.iter().any(|a| a.contains("example.com")));
    }

    #[test]
    fn curl_args_do_not_pass_the_url_through_the_argv_vector() {
        // 参数向量是纯的:换 URL 只改 max_filesize 之外的部分,不把 URL 塞进来。
        let a = curl_args("https://a/x.zip", Path::new("/d"), 1);
        let b = curl_args("https://b/y.zip", Path::new("/d"), 1);
        assert_eq!(a, b);
    }
}
