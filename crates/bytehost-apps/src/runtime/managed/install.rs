//! 一次运行时安装(A6d):下载 → 校验 SHA-256 → 解压到 staging → 路径/符号链接越界检查 → 试跑
//! `--version` → **原子改名**落位。
//!
//! 校验失败、越界、缺可执行文件、试跑失败——**一律删掉 staging,不留任何半成品**;已装版本不覆盖
//! (要换先卸载)。

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::digest::sha256_hex;

use super::fetch::{Archive, Fetcher};
use super::pins::Pin;
use super::store::RuntimeStore;

/// 安装的进度阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Verifying,
    Extracting,
    Checking,
}

#[derive(Debug)]
pub enum InstallError {
    Download(String),
    ChecksumMismatch { expected: String, actual: String },
    UnsafeArchive(String),
    MissingBinary(String),
    SmokeTestFailed(String),
    Cancelled,
    Io(io::Error),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Download(m) => write!(f, "下载失败:{m}"),
            Self::ChecksumMismatch { expected, actual } => {
                write!(f, "校验和不符:期望 {expected},实际 {actual}")
            }
            Self::UnsafeArchive(m) => write!(f, "压缩包不安全:{m}"),
            Self::MissingBinary(m) => write!(f, "压缩包里没有预期的可执行文件:{m}"),
            Self::SmokeTestFailed(m) => write!(f, "可执行文件试跑失败:{m}"),
            Self::Cancelled => write!(f, "安装已取消"),
            Self::Io(e) => write!(f, "I/O 错误:{e}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<io::Error> for InstallError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 试跑 `--version` 的超时。
const SMOKE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Installer<'a> {
    pub store: &'a RuntimeStore,
    pub fetcher: &'a dyn Fetcher,
    pub archive: &'a dyn Archive,
}

impl Installer<'_> {
    /// 下载 `pin.url` → 校验 → 解压 → 安全检查 → 试跑 → 落位到 `store.version_dir(pin.name, pin.version)`。
    ///
    /// 返回落位后的目录。
    pub fn install(
        &self,
        pin: &Pin,
        on_progress: &mut dyn FnMut(Phase, u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, InstallError> {
        let dest = self.store.version_dir(pin.name, pin.version);
        // 已装版本不覆盖:要换先卸载(计划约定)。
        if dest.exists() {
            return Err(InstallError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} {} 已安装,请先卸载", pin.name, pin.version),
            )));
        }

        let staging = self.store.staging_dir();
        // 无论成功失败都清掉 staging;失败时绝不留半成品。
        let result = self.install_into(pin, &staging, on_progress, cancel);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result
    }

    fn install_into(
        &self,
        pin: &Pin,
        staging: &Path,
        on_progress: &mut dyn FnMut(Phase, u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, InstallError> {
        std::fs::create_dir_all(staging)?;
        let archive_path = staging.join("archive.tar.gz");

        // --- 下载 ---
        on_progress(Phase::Downloading, 0, None);
        let mut dl_progress =
            |done: u64, total: Option<u64>| on_progress(Phase::Downloading, done, total);
        self.fetcher
            .fetch(pin.url, &archive_path, &mut dl_progress, cancel)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::Interrupted {
                    InstallError::Cancelled
                } else {
                    InstallError::Download(e.to_string())
                }
            })?;
        if cancel.load(Ordering::SeqCst) {
            return Err(InstallError::Cancelled);
        }

        // --- 校验 ---
        on_progress(Phase::Verifying, 0, None);
        let bytes = std::fs::read(&archive_path)?;
        let actual = sha256_hex(&bytes);
        if actual != pin.sha256 {
            return Err(InstallError::ChecksumMismatch {
                expected: pin.sha256.to_string(),
                actual,
            });
        }

        // --- 解压 ---
        on_progress(Phase::Extracting, 0, None);
        check_archive_entries(self.archive, &archive_path)?;
        let unpack = staging.join("unpack");
        self.archive
            .extract(&archive_path, &unpack, pin.strip_components)?;
        check_extracted_tree(&unpack)?;

        // --- 试跑 ---
        on_progress(Phase::Checking, 0, None);
        let bin = unpack.join(pin.bin_rel);
        if !is_executable_file(&bin) {
            return Err(InstallError::MissingBinary(pin.bin_rel.to_string()));
        }
        smoke_test(&bin, cancel)?;

        // --- 落位(原子改名)---
        let final_dir = self.store.version_dir(pin.name, pin.version);
        if let Some(parent) = final_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&unpack, &final_dir)?;
        // 清掉 staging(空的 unpack 已改名离开,下载归档也删掉)。
        let _ = std::fs::remove_dir_all(staging);
        Ok(final_dir)
    }
}

/// 归档的全部条目名必须安全(拒绝绝对路径与含 `..` 的条目)。
fn check_archive_entries(archive: &dyn Archive, path: &Path) -> Result<(), InstallError> {
    let entries = archive
        .list(path)
        .map_err(|e| InstallError::UnsafeArchive(format!("列不出归档条目:{e}")))?;
    for name in entries {
        if !entry_is_safe(&name) {
            return Err(InstallError::UnsafeArchive(format!("不安全的条目: {name}")));
        }
    }
    Ok(())
}

/// 解压后遍历:任何符号链接按**字面**解析(`..` 只按层级抵消,不跟随文件系统)必须仍落在 root 内。
/// 相对内部链接(如 Node 的 `bin/npm -> ../lib/node_modules/npm/bin/npm-cli.js`)合法。
fn check_extracted_tree(root: &Path) -> Result<(), InstallError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| {
            InstallError::UnsafeArchive(format!("读目录失败 {}:{e}", dir.display()))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| InstallError::UnsafeArchive(e.to_string()))?;
            let path = entry.path();
            let ft = entry
                .file_type()
                .map_err(|e| InstallError::UnsafeArchive(e.to_string()))?;
            if ft.is_symlink() {
                let target = std::fs::read_link(&path)
                    .map_err(|e| InstallError::UnsafeArchive(e.to_string()))?;
                let resolved = resolve_literal(&path, &target);
                if !path_is_within(root, &resolved) {
                    return Err(InstallError::UnsafeArchive(format!(
                        "符号链接指向 staging 之外: {} -> {}",
                        path.display(),
                        target.display()
                    )));
                }
            } else if ft.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

/// 一个归档条目名是否安全:非空、不是绝对路径、不含 `..` 组件。
fn entry_is_safe(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return false;
    }
    for c in path.components() {
        match c {
            Component::ParentDir => return false,
            Component::RootDir | Component::Prefix(_) => return false,
            _ => {}
        }
    }
    true
}

/// 按**字面**把 `link`(一个坏链接的绝对位置)与 `target` 解析成一个绝对路径:遇到 `..` 弹一层,
/// 不跟随文件系统上的实际链接。返回的路径可能带不存在的中间层,只用于判断是否仍在 root 内。
fn resolve_literal(link: &Path, target: &Path) -> PathBuf {
    let base: PathBuf = if target.is_absolute() {
        PathBuf::new()
    } else {
        link.parent().map(|p| p.to_path_buf()).unwrap_or_default()
    };
    let joined = base.join(target);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `child` 按字面解析后是否仍在 `root` 之内(`root` 也已字面规范化)。
fn path_is_within(root: &Path, child: &Path) -> bool {
    let root = resolve_literal(Path::new("/"), root);
    let child = resolve_literal(Path::new("/"), child);
    child.starts_with(&root)
}

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 试跑 `bin --version`,5s 超时,清空环境只留 `PATH=/usr/bin:/bin`,退出码 0 才算过;
/// 结果输出不信任、不解析。
fn smoke_test(bin: &Path, cancel: &AtomicBool) -> Result<(), InstallError> {
    let mut child = Command::new(bin)
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| InstallError::SmokeTestFailed(e.to_string()))?;

    let start = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InstallError::Cancelled);
        }
        match child
            .try_wait()
            .map_err(|e| InstallError::SmokeTestFailed(e.to_string()))?
        {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                let mut msg = format!("退出码 {:?}", status.code());
                use std::io::Read;
                if let Some(mut e) = child.stderr.take() {
                    let mut s = String::new();
                    let _ = e.read_to_string(&mut s);
                    if !s.trim().is_empty() {
                        msg.push_str(&format!(":{}", s.trim()));
                    }
                }
                return Err(InstallError::SmokeTestFailed(msg));
            }
            None => {
                if start.elapsed() > SMOKE_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(InstallError::SmokeTestFailed("试跑超时".into()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `Pin` 的字段是 `&'static str`;测试里用 `Box::leak` 造。
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn tool_pin(url: &str, sha: String, strip: u32, bin_rel: &str) -> Pin {
        Pin {
            name: "tool",
            version: "1.0.0",
            target: super::super::pins::Target::Aarch64Apple,
            url: leak(url.to_string()),
            sha256: leak(sha),
            strip_components: strip,
            bin_rel: leak(bin_rel.to_string()),
        }
    }

    /// 造一个含 `bin/tool`(可执行脚本)的 `.tar.gz`,返回路径与 sha256。
    fn make_tar(dir: &Path, script: &str) -> (PathBuf, String) {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        let tool = src.join("bin/tool");
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = dir.join("tool.tar.gz");
        let status = Command::new("/usr/bin/tar")
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

    /// 手工写 512 字节 ustar 头 + 内容(造 tar 表达不了的畸形归档:绝对路径、`..`、符号链接)。
    /// `typeflag`: `b'0'` 文件、`b'2'` 符号链接、`b'5'` 目录。
    fn raw_tar(entries: &[(&str, &[u8], u8, &str)]) -> Vec<u8> {
        fn put(buf: &mut [u8], at: usize, s: &[u8]) {
            let n = s.len().min(buf.len().saturating_sub(at));
            buf[at..at + n].copy_from_slice(&s[..n]);
        }
        let mut out = Vec::new();
        for (name, content, typeflag, linkname) in entries {
            let mut header = [0u8; 512];
            put(&mut header, 0, name.as_bytes());
            put(&mut header, 100, format!("{:07o}", 0o755).as_bytes());
            put(&mut header, 108, format!("{:07o}", 0).as_bytes());
            put(&mut header, 116, format!("{:07o}", 0).as_bytes());
            put(
                &mut header,
                124,
                format!("{:011o}", content.len() as u64).as_bytes(),
            );
            put(&mut header, 136, format!("{:011o}", 0).as_bytes());
            header[156] = *typeflag;
            put(&mut header, 157, linkname.as_bytes());
            put(&mut header, 257, b"ustar");
            put(&mut header, 263, b"00");
            for b in header[148..156].iter_mut() {
                *b = b' ';
            }
            let sum: u32 = header.iter().map(|b| *b as u32).sum();
            put(&mut header, 148, format!("{:06o}\0 ", sum).as_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(content);
            let pad = (512 - content.len() % 512) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    /// 把原始 tar 字节 gzip 成 `.tar.gz`,返回 (路径, 压缩包字节)。
    fn write_raw_tar(dir: &Path, raw: &[u8], tag: &str) -> (PathBuf, Vec<u8>) {
        let plain = dir.join(format!("raw-{tag}.tar"));
        std::fs::write(&plain, raw).unwrap();
        let gz = dir.join(format!("raw-{tag}.tar.gz"));
        let out = Command::new("/usr/bin/gzip")
            .arg("-c")
            .arg(&plain)
            .output()
            .unwrap();
        assert!(out.status.success());
        std::fs::write(&gz, &out.stdout).unwrap();
        (gz, out.stdout)
    }

    /// 假 fetcher:把预置文件拷到 dest;可配置 sleep / 报错。
    struct FileFetcher {
        source: PathBuf,
        sleep: Duration,
        fail: Option<String>,
    }

    impl FileFetcher {
        fn ok(source: PathBuf) -> Self {
            Self {
                source,
                sleep: Duration::ZERO,
                fail: None,
            }
        }
    }

    impl Fetcher for FileFetcher {
        fn fetch(
            &self,
            _url: &str,
            dest: &Path,
            on_progress: &mut dyn FnMut(u64, Option<u64>),
            cancel: &AtomicBool,
        ) -> io::Result<()> {
            if let Some(reason) = &self.fail {
                return Err(io::Error::other(reason.clone()));
            }
            if !self.sleep.is_zero() {
                let start = std::time::Instant::now();
                while start.elapsed() < self.sleep {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "取消"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let bytes = std::fs::read(&self.source)?;
            std::fs::write(dest, &bytes)?;
            on_progress(bytes.len() as u64, Some(bytes.len() as u64));
            Ok(())
        }
    }

    fn real_archive() -> super::super::fetch::TarArchive {
        super::super::fetch::TarArchive
    }

    fn store_root(store: &RuntimeStore) -> PathBuf {
        store.root().to_path_buf()
    }

    fn is_empty_or_missing(p: &Path) -> bool {
        match std::fs::read_dir(p) {
            Ok(entries) => entries.flatten().next().is_none(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }

    #[test]
    fn a_valid_archive_is_installed_atomically_and_smoke_tested() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\necho v1\n");
        let fetcher = FileFetcher::ok(tar);
        let archive = real_archive();

        let phases: Mutex<Vec<Phase>> = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let dest = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 1, "bin/tool"),
            &mut |phase, _, _| phases.lock().unwrap().push(phase),
            &cancel,
        )
        .unwrap();

        assert!(dest.join("bin/tool").exists());
        assert!(is_executable_file(&dest.join("bin/tool")));
        let leftovers: Vec<_> = std::fs::read_dir(store_root(&store))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty(), "staging 残留: {leftovers:?}");
        let phases = phases.into_inner().unwrap();
        for expected in [
            Phase::Downloading,
            Phase::Verifying,
            Phase::Extracting,
            Phase::Checking,
        ] {
            assert!(
                phases.contains(&expected),
                "缺少阶段 {expected:?}: {phases:?}"
            );
        }
    }

    #[test]
    fn a_wrong_checksum_fails_and_leaves_nothing_behind() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\necho v1\n");
        let mut bad = sha.clone();
        bad.replace_range(0..1, if &sha[0..1] == "0" { "1" } else { "0" });
        let fetcher = FileFetcher::ok(tar);
        let archive = real_archive();
        let cancel = AtomicBool::new(false);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", bad, 1, "bin/tool"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(
            matches!(err, InstallError::ChecksumMismatch { .. }),
            "{err:?}"
        );
        assert!(is_empty_or_missing(&store_root(&store)));
        assert!(store.installed("tool").is_empty());
    }

    #[test]
    fn path_traversal_entries_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let archive = real_archive();
        let cancel = AtomicBool::new(false);

        // ../evil
        let raw = raw_tar(&[
            ("bin/tool", b"#!/bin/sh\necho v1\n", b'0', ""),
            ("../evil", b"x", b'0', ""),
        ]);
        let (tar, bytes) = write_raw_tar(d.path(), &raw, "parent");
        let sha = sha256_hex(&bytes);
        let fetcher = FileFetcher::ok(tar);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 0, "bin/tool"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::UnsafeArchive(_)), "{err:?}");
        assert!(!d.path().join("evil").exists());
        assert!(is_empty_or_missing(&store_root(&store)));

        // 绝对路径
        let raw = raw_tar(&[
            ("bin/tool", b"#!/bin/sh\necho v1\n", b'0', ""),
            ("/tmp/x", b"x", b'0', ""),
        ]);
        let (tar, bytes) = write_raw_tar(d.path(), &raw, "abs");
        let sha = sha256_hex(&bytes);
        let fetcher = FileFetcher::ok(tar);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 0, "bin/tool"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::UnsafeArchive(_)), "{err:?}");
    }

    #[test]
    fn a_symlink_pointing_outside_the_staging_dir_is_refused_but_a_relative_inner_link_is_fine() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let archive = real_archive();
        let cancel = AtomicBool::new(false);

        // 指向 staging 之外
        let raw = raw_tar(&[("bin/a", b"", b'2', "../../../etc/passwd")]);
        let (tar, bytes) = write_raw_tar(d.path(), &raw, "escape");
        let sha = sha256_hex(&bytes);
        let fetcher = FileFetcher::ok(tar);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 0, "bin/a"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::UnsafeArchive(_)), "{err:?}");

        // 指向内部(相对)
        let raw = raw_tar(&[
            ("lib/real", b"#!/bin/sh\necho v1\n", b'0', ""),
            ("bin/b", b"", b'2', "../lib/real"),
        ]);
        let (tar, bytes) = write_raw_tar(d.path(), &raw, "inner");
        let sha = sha256_hex(&bytes);
        let fetcher = FileFetcher::ok(tar);
        let dest = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 0, "bin/b"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap();
        assert!(dest.join("bin/b").exists());
    }

    #[test]
    fn an_archive_without_the_expected_binary_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\necho v1\n");
        let fetcher = FileFetcher::ok(tar);
        let archive = real_archive();
        let cancel = AtomicBool::new(false);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 1, "bin/missing"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::MissingBinary(_)), "{err:?}");
        assert!(store.installed("tool").is_empty());
    }

    #[test]
    fn a_binary_that_fails_its_smoke_test_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\nexit 1\n");
        let fetcher = FileFetcher::ok(tar);
        let archive = real_archive();
        let cancel = AtomicBool::new(false);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 1, "bin/tool"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::SmokeTestFailed(_)), "{err:?}");
        assert!(store.installed("tool").is_empty());
    }

    #[test]
    fn cancelling_during_the_download_aborts_and_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\necho v1\n");
        let fetcher = FileFetcher {
            source: tar,
            sleep: Duration::from_secs(2),
            fail: None,
        };
        let archive = real_archive();
        let cancel = AtomicBool::new(true);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 1, "bin/tool"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::Cancelled), "{err:?}");
        assert!(is_empty_or_missing(&store_root(&store)));
        assert!(store.installed("tool").is_empty());
    }

    #[test]
    fn a_failing_download_is_reported_with_its_reason_and_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\necho v1\n");
        let fetcher = FileFetcher {
            source: tar,
            sleep: Duration::ZERO,
            fail: Some("连接超时".into()),
        };
        let archive = real_archive();
        let cancel = AtomicBool::new(false);
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(
            &tool_pin("https://example.com/tool.tar.gz", sha, 1, "bin/tool"),
            &mut |_, _, _| {},
            &cancel,
        )
        .unwrap_err();
        assert!(
            matches!(&err, InstallError::Download(m) if m.contains("连接超时")),
            "{err:?}"
        );
        assert!(is_empty_or_missing(&store_root(&store)));
    }

    #[test]
    fn reinstalling_an_existing_version_is_refused_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        let (tar, sha) = make_tar(d.path(), "#!/bin/sh\necho v1\n");
        let fetcher = FileFetcher::ok(tar);
        let archive = real_archive();
        let cancel = AtomicBool::new(false);
        let p = tool_pin("https://example.com/tool.tar.gz", sha, 1, "bin/tool");
        Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(&p, &mut |_, _, _| {}, &cancel)
        .unwrap();
        let err = Installer {
            store: &store,
            fetcher: &fetcher,
            archive: &archive,
        }
        .install(&p, &mut |_, _, _| {}, &cancel)
        .unwrap_err();
        assert!(
            matches!(&err, InstallError::Io(e) if e.kind() == io::ErrorKind::AlreadyExists),
            "{err:?}"
        );
    }

    #[test]
    fn entry_safety_and_literal_resolution_are_table_driven() {
        for ok in ["bin/node", "lib/node_modules/npm/cli.js", "a/b.txt"] {
            assert!(entry_is_safe(ok), "{ok}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "./../x"] {
            assert!(!entry_is_safe(bad), "{bad}");
        }

        assert!(path_is_within(
            Path::new("/r/run/unpack"),
            &resolve_literal(Path::new("/r/run/unpack/bin/b"), Path::new("../lib/real"))
        ));
        assert!(!path_is_within(
            Path::new("/r/run/unpack"),
            &resolve_literal(
                Path::new("/r/run/unpack/bin/a"),
                Path::new("../../../etc/passwd")
            )
        ));
    }
}
