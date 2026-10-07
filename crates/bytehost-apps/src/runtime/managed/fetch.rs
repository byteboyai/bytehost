//! 下载与解压的**接口**与两个真实实现(A6d):下载调系统 `/usr/bin/curl`,解压调系统 `/usr/bin/tar`。
//!
//! 不给 dozerd 引入 TLS/HTTP 客户端依赖(与"核心不依赖 Node/Python"的思路一致);校验和由我们自己的
//! `sha2` 计算。测试用假 `Fetcher`,真 `TarArchive`(系统 `tar`)。

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

impl Fetcher for CurlFetcher {
    fn fetch(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        // 先尝试 HEAD 拿总长(拿不到就 total = None,不影响下载)。
        let total = curl_content_length(url);

        let mut child = Command::new("/usr/bin/curl")
            .args([
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--tlsv1.2",
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--max-filesize",
                "400000000",
                "-o",
            ])
            .arg(dest)
            .arg(url)
            .stdout(Stdio::null())
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
                        if let Ok(len) = std::fs::metadata(dest) {
                            on_progress(len.len(), total);
                        }
                        return Ok(());
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
