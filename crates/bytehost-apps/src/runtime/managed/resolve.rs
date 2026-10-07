//! 受管运行时优先、系统兜底的解析链(A6d):依赖 `RuntimeResolver` trait(A6c)。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::runtime::resolve::{ResolveError, Resolved, RuntimeResolver};

use super::store::RuntimeStore;

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 从一个目录里的可执行文件构造 `Resolved`(可执行文件所在目录进 `path_dirs`)。
fn resolved_from(program: PathBuf) -> Option<Resolved> {
    if !is_executable_file(&program) {
        return None;
    }
    let dir = program.parent()?.to_path_buf();
    Some(Resolved {
        program,
        path_dirs: vec![dir],
    })
}

/// 从受管 store 解析运行时(受管版本优先)。
pub struct ManagedResolver {
    store: RuntimeStore,
}

impl ManagedResolver {
    pub fn new(store: RuntimeStore) -> Self {
        Self { store }
    }

    /// 最新受管版本的根目录。
    fn newest_dir(&self, name: &str) -> Option<PathBuf> {
        let version = self.store.installed(name).into_iter().next()?;
        Some(self.store.version_dir(name, &version))
    }

    /// 最新受管的 Python 解释器:在 `<root>/python/cpython-*/bin/python3` 里按目录名降序找第一个可执行文件。
    /// 注意:Python 由 uv 装到 `<root>/python`,版本目录名是 uv 自己的命名(如 `cpython-3.13.x-...`),
    /// 不是 `PYTHON_VERSION`;这里只按"有没有 `bin/python3`"挑选。
    fn newest_python(&self) -> Option<PathBuf> {
        let entries = std::fs::read_dir(self.store.root().join("python")).ok()?;
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        for dir in dirs {
            let candidate = dir.join("bin/python3");
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

impl RuntimeResolver for ManagedResolver {
    fn resolve(&self, program: &str) -> Result<Resolved, ResolveError> {
        let bare = is_bare_name(program);
        if !bare {
            return Err(ResolveError::NotInstalled(program.to_string()));
        }
        let found = match program {
            "node" | "npm" | "npx" => self
                .newest_dir("node")
                .map(|dir| dir.join("bin").join(program))
                .and_then(resolved_from),
            "uv" | "uvx" => self
                .newest_dir("uv")
                .map(|dir| dir.join(program))
                .and_then(resolved_from),
            "python" | "python3" => self.newest_python().and_then(resolved_from),
            _ => None,
        };
        found.ok_or_else(|| ResolveError::NotInstalled(program.to_string()))
    }
}

/// 裸解释器名(不含路径分隔符、不是 `.`/`..`、非空、无 NUL)。
fn is_bare_name(program: &str) -> bool {
    !program.is_empty()
        && program != "."
        && program != ".."
        && !program.contains('/')
        && !program.contains('\0')
}

/// 依次尝试多个解析器,**第一个 `Ok` 即用**;全失败返回最后一个 `NotInstalled`。
pub struct ChainResolver(pub Vec<Arc<dyn RuntimeResolver>>);

impl RuntimeResolver for ChainResolver {
    fn resolve(&self, program: &str) -> Result<Resolved, ResolveError> {
        let mut last = ResolveError::NotInstalled(program.to_string());
        for resolver in &self.0 {
            match resolver.resolve(program) {
                Ok(r) => return Ok(r),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::resolve::SystemResolver;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn exe(dir: &Path, rel: &str) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn the_managed_resolver_finds_node_npm_uv_and_python_and_refuses_paths() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        exe(&store.root().join("node/24.1.0"), "bin/node");
        exe(&store.root().join("node/24.1.0"), "bin/npm");
        exe(&store.root().join("uv/0.5.0"), "uv");
        exe(&store.root().join("python/cpython-3.13.0"), "bin/python3");
        let r = ManagedResolver::new(store);

        assert_eq!(
            r.resolve("node").unwrap().program,
            d.path().join("runtimes/node/24.1.0/bin/node")
        );
        assert_eq!(
            r.resolve("npm").unwrap().path_dirs,
            vec![d.path().join("runtimes/node/24.1.0/bin")]
        );
        assert!(r.resolve("uv").unwrap().program.ends_with("uv/0.5.0/uv"));
        assert!(
            r.resolve("python3")
                .unwrap()
                .program
                .ends_with("python/cpython-3.13.0/bin/python3")
        );
        assert!(r.resolve("python").is_ok());

        for bad in ["/bin/sh", "../node", "a/b", "", "."] {
            assert!(r.resolve(bad).is_err(), "{bad}");
        }
        assert!(r.resolve("ruby").is_err());
    }

    #[test]
    fn the_managed_resolver_prefers_the_newest_installed_version() {
        let d = tempfile::tempdir().unwrap();
        let store = RuntimeStore::new(d.path().join("runtimes"));
        exe(&store.root().join("node/22.1.0"), "bin/node");
        exe(&store.root().join("node/24.9.0"), "bin/node");
        let r = ManagedResolver::new(store);
        assert!(
            r.resolve("node")
                .unwrap()
                .program
                .ends_with("node/24.9.0/bin/node")
        );
    }

    #[test]
    fn nothing_installed_is_not_installed() {
        let d = tempfile::tempdir().unwrap();
        let r = ManagedResolver::new(RuntimeStore::new(d.path().join("runtimes")));
        assert_eq!(
            r.resolve("node"),
            Err(ResolveError::NotInstalled("node".into()))
        );
    }

    #[test]
    fn the_chain_prefers_managed_but_falls_back_to_the_system() {
        let managed_dir = tempfile::tempdir().unwrap();
        let system_dir = tempfile::tempdir().unwrap();
        exe(system_dir.path(), "node");

        // 没装受管 → 回落系统
        let chain = ChainResolver(vec![
            Arc::new(ManagedResolver::new(RuntimeStore::new(
                managed_dir.path().join("runtimes"),
            ))),
            Arc::new(SystemResolver::with_dirs(vec![system_dir.path().into()])),
        ]);
        assert_eq!(
            chain.resolve("node").unwrap().program,
            system_dir.path().join("node")
        );

        // 装了受管 → 用受管
        exe(&managed_dir.path().join("runtimes/node/9.9.9"), "bin/node");
        assert_eq!(
            chain.resolve("node").unwrap().program,
            managed_dir.path().join("runtimes/node/9.9.9/bin/node")
        );
    }

    #[test]
    fn the_chain_reports_not_installed_when_every_resolver_fails() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let chain = ChainResolver(vec![
            Arc::new(ManagedResolver::new(RuntimeStore::new(a.path()))),
            Arc::new(SystemResolver::with_dirs(vec![b.path().into()])),
        ]);
        assert_eq!(
            chain.resolve("node"),
            Err(ResolveError::NotInstalled("node".into()))
        );
    }
}
