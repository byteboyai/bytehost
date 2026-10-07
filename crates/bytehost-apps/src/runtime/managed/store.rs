//! 受管运行时的盘上布局:`<root>/<name>/<version>/`。
//!
//! 安装全程在 `<root>/.staging-<uuid>/` 里做,试跑通过后一次 `rename` 落位;失败/取消/崩溃都不留半成品。
//! 这里只管路径与列目录/删除,**不碰网络与解压**(那些在 `install.rs`)。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 一个运行时目录里合法的一段名字(版本号/运行时名):只含 `[A-Za-z0-9._-]`,且不是 `.`/`..`。
/// 目录名会拼进路径,必须挡住能逃出 `root` 的片段。
fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// 受管运行时的存储根(通常是 `<bytehost root>/runtimes`)。
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 存储根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<name>/<version>`,非法名字返回 `None`。
    pub fn try_version_dir(&self, name: &str, version: &str) -> Option<PathBuf> {
        if is_safe_segment(name) && is_safe_segment(version) {
            Some(self.root.join(name).join(version))
        } else {
            None
        }
    }

    /// `<root>/<name>/<version>`。**调用方必须保证 name/version 合法**(内部数据,不是用户输入);
    /// 需要处理不可信输入时用 [`try_version_dir`](Self::try_version_dir)。
    pub fn version_dir(&self, name: &str, version: &str) -> PathBuf {
        self.try_version_dir(name, version)
            .unwrap_or_else(|| panic!("非法的运行时目录名: {name}/{version}"))
    }

    /// `<root>/<name>`,非法名字返回 `None`。
    fn try_runtime_dir(&self, name: &str) -> Option<PathBuf> {
        if is_safe_segment(name) {
            Some(self.root.join(name))
        } else {
            None
        }
    }

    /// 某个运行时已装的版本,版本号**降序**(按语义比较,失败退字符串比较);目录不存在或非目录都忽略。
    pub fn installed(&self, name: &str) -> Vec<String> {
        let Some(dir) = self.try_runtime_dir(name) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut versions: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
            .filter(|s| is_safe_segment(s))
            .collect();
        versions.sort_by(|a, b| compare_versions(b, a));
        versions
    }

    /// 一个新的、**尚未创建**的 staging 目录:`<root>/.staging-<uuid>`。
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(format!(".staging-{}", uuid::Uuid::new_v4()))
    }

    /// 清掉 `root` 下所有遗留的 `.staging-*`(上次崩溃/取消留下的半成品)。幂等,尽力而为。
    pub fn sweep_staging(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with(".staging-") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// 删掉一个受管版本(幂等)。名字非法 → `InvalidInput`;不存在 → `Ok`。
    pub fn remove(&self, name: &str, version: &str) -> io::Result<()> {
        let dir = self.try_version_dir(name, version).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("非法的运行时目录名: {name}/{version}"),
            )
        })?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// 版本号降序比较:先按 `.` 分段,每段能解析成数字就按数字比,否则按字符串;数字段优先于非数字段。
/// 失败的输入不 panic,退化成字符串比较即可。
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let pa: Vec<&str> = a.split('.').collect();
    let pb: Vec<&str> = b.split('.').collect();
    for i in 0..pa.len().max(pb.len()) {
        let sa = pa.get(i).copied();
        let sb = pb.get(i).copied();
        match (sa, sb) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    (Ok(_), Err(_)) => Ordering::Greater,
                    (Err(_), Ok(_)) => Ordering::Less,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_versions_are_listed_newest_first_and_ignore_staging_and_files() {
        let d = tempfile::tempdir().unwrap();
        let s = RuntimeStore::new(d.path());
        for v in ["22.1.0", "24.14.0", "24.9.0"] {
            fs::create_dir_all(s.version_dir("node", v)).unwrap();
        }
        fs::create_dir_all(d.path().join(".staging-x")).unwrap();
        fs::write(d.path().join("node").join("stray.txt"), "x").unwrap();
        assert_eq!(s.installed("node"), vec!["24.14.0", "24.9.0", "22.1.0"]);
        assert!(s.installed("uv").is_empty());
    }

    #[test]
    fn remove_is_idempotent_and_sweep_clears_only_staging() {
        let d = tempfile::tempdir().unwrap();
        let s = RuntimeStore::new(d.path());
        fs::create_dir_all(s.version_dir("uv", "1.0.0")).unwrap();
        fs::create_dir_all(d.path().join(".staging-a/deep")).unwrap();
        s.remove("uv", "1.0.0").unwrap();
        s.remove("uv", "1.0.0").unwrap();
        s.sweep_staging();
        assert!(!d.path().join(".staging-a").exists());
        assert!(s.installed("uv").is_empty());
    }

    #[test]
    fn version_names_that_could_escape_the_store_are_rejected() {
        let s = RuntimeStore::new("/r");
        for bad in ["..", "../x", "a/b", "", ".", "a b", "a\\b"] {
            assert!(s.try_version_dir("node", bad).is_none(), "{bad}");
        }
        assert_eq!(
            s.try_version_dir("node", "24.1.0").unwrap(),
            PathBuf::from("/r/node/24.1.0")
        );
    }

    #[test]
    fn removing_a_version_with_an_illegal_name_is_an_error_not_a_delete() {
        let d = tempfile::tempdir().unwrap();
        let s = RuntimeStore::new(d.path());
        let err = s.remove("node", "../evil").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn staging_dirs_are_unique_and_under_the_root() {
        let s = RuntimeStore::new("/r");
        let a = s.staging_dir();
        let b = s.staging_dir();
        assert_ne!(a, b);
        assert!(a.starts_with("/r"));
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".staging-")
        );
    }
}
