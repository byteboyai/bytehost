//! 把解释器名(`node`/`npm`/`python3`/`uv`…)解析成绝对路径。dozerd 若由 GUI 拉起,继承来的 `PATH`
//! 往往只有 `/usr/bin:/bin`,所以这里自己带一份常见目录;A6d 装到 bytehost 自己目录里的运行时也经这个 trait 接入。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub program: PathBuf,
    /// 子进程 `PATH` 的前缀目录(`npm` 要在同目录找到 `node`)。
    pub path_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotInstalled(String),
}

pub trait RuntimeResolver: Send + Sync {
    fn resolve(&self, program: &str) -> Result<Resolved, ResolveError>;
}

pub struct SystemResolver {
    dirs: Vec<PathBuf>,
}

impl SystemResolver {
    pub fn new() -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            dirs.extend([".local/bin", ".cargo/bin", ".volta/bin"].map(|d| home.join(d)));
        }
        Self { dirs }
    }

    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self::new()
    }
}

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

impl RuntimeResolver for SystemResolver {
    fn resolve(&self, program: &str) -> Result<Resolved, ResolveError> {
        let bare = !program.is_empty()
            && program != "."
            && program != ".."
            && !program.contains('/')
            && !program.contains('\0');
        if bare {
            for dir in &self.dirs {
                let candidate = dir.join(program);
                if is_executable_file(&candidate) {
                    return Ok(Resolved {
                        program: candidate,
                        path_dirs: vec![dir.clone()],
                    });
                }
            }
        }
        Err(ResolveError::NotInstalled(program.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn exe(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn the_first_directory_that_holds_an_executable_wins_and_its_dir_goes_on_the_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        exe(b.path(), "node");
        let r = SystemResolver::with_dirs(vec![a.path().into(), b.path().into()])
            .resolve("node")
            .unwrap();
        assert_eq!(r.program, b.path().join("node"));
        assert_eq!(r.path_dirs, vec![b.path().to_path_buf()]);
    }

    #[test]
    fn a_non_executable_file_is_not_a_runtime() {
        let a = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("node"), "x").unwrap(); // 没有执行位
        assert_eq!(
            SystemResolver::with_dirs(vec![a.path().into()]).resolve("node"),
            Err(ResolveError::NotInstalled("node".into()))
        );
    }

    #[test]
    fn only_bare_interpreter_names_are_resolved_never_paths() {
        let a = tempfile::tempdir().unwrap();
        exe(a.path(), "node");
        let r = SystemResolver::with_dirs(vec![a.path().into()]);
        for bad in ["/bin/sh", "../node", "a/b", "", "."] {
            assert!(r.resolve(bad).is_err(), "{bad}");
        }
    }
}
