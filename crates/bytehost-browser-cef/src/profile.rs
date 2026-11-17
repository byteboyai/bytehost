//! CEF profile/cache paths derived from a product data root and a neutral scope.
//!
//! Layout is stable and independent of tabs, agent sessions and app versions:
//! `<data_root>/browser/cef/profiles/{global,projects/<opaque-id>}` with
//! `cache/` as a child (CEF requires `cache_path` under `root_cache_path`).

use std::path::{Path, PathBuf};

use bytehost_browser_core::ProfileScope;

pub const CEF_DATA_SEGMENT: &str = "browser/cef";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefProfilePaths {
    /// `CefSettings.root_cache_path` / request-context root for this scope.
    pub root: PathBuf,
    /// `CefSettings.cache_path`; always a descendant of `root`.
    pub cache: PathBuf,
}

pub fn profile_paths(data_root: &Path, scope: &ProfileScope) -> CefProfilePaths {
    let root = data_root
        .join(CEF_DATA_SEGMENT)
        .join("profiles")
        .join(scope.storage_segment());
    CefProfilePaths {
        cache: root.join("cache"),
        root,
    }
}

pub fn artifacts_root(data_root: &Path) -> PathBuf {
    data_root.join(CEF_DATA_SEGMENT).join("artifacts")
}

pub fn instance_lock_path(data_root: &Path) -> PathBuf {
    data_root.join(CEF_DATA_SEGMENT).join(".instance-lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_browser_core::ProjectKey;

    #[test]
    fn global_and_project_scopes_are_isolated_descendants() {
        let data = Path::new("/tmp/dozer-data");
        let global = profile_paths(data, &ProfileScope::Global);
        let a = profile_paths(
            data,
            &ProfileScope::Project {
                project: ProjectKey::new("p7").unwrap(),
            },
        );
        let b = profile_paths(
            data,
            &ProfileScope::Project {
                project: ProjectKey::new("p8").unwrap(),
            },
        );
        assert_eq!(global.root, data.join("browser/cef/profiles/global"));
        assert_eq!(a.root, data.join("browser/cef/profiles/projects/p7"));
        assert_ne!(a.root, b.root);
        assert!(a.cache.starts_with(&a.root));
        assert!(a.root.starts_with(data.join("browser/cef")));
        assert!(!a.root.to_string_lossy().contains(".."));
        assert_eq!(artifacts_root(data), data.join("browser/cef/artifacts"));
        assert_eq!(
            instance_lock_path(data),
            data.join("browser/cef/.instance-lock")
        );
    }

    #[test]
    fn invalid_project_keys_cannot_escape_the_root() {
        assert!(ProjectKey::new("../escape").is_err());
        assert!(ProjectKey::new("a/b").is_err());
        assert!(ProjectKey::new("").is_err());
    }
}
