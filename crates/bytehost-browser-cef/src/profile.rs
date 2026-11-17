//! CEF profile/cache paths derived from a product data root and a neutral scope.
//!
//! Layout is stable and independent of tabs, agent sessions and app versions:
//! `<data_root>/browser/cef/profiles/{global,project-<opaque-id>}` with
//! `cache/` as a child (CEF requires `cache_path` under `root_cache_path`).
//!
//! Every scope root is a *direct child* of `profiles/`: CEF/Chromium rejects a
//! request-context profile directory that is nested deeper and falls back to an
//! off-the-record profile, losing all persistence.

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
        let global_scope = ProfileScope::Global;
        let a_scope = ProfileScope::Project {
            project: ProjectKey::new("p7").unwrap(),
        };
        let b_scope = ProfileScope::Project {
            project: ProjectKey::new("p8").unwrap(),
        };
        let global = profile_paths(data, &global_scope);
        let a = profile_paths(data, &a_scope);
        let b = profile_paths(data, &b_scope);
        assert_eq!(global.root, data.join("browser/cef/profiles/global"));
        assert_eq!(a.root, data.join("browser/cef/profiles/project-p7"));
        assert_ne!(a.root, b.root);
        assert!(a.cache.starts_with(&a.root));
        assert!(a.root.starts_with(data.join("browser/cef")));
        assert!(!a.root.to_string_lossy().contains(".."));
        // CEF only creates a persistent profile for a direct child of the
        // user-data root (`root_cache_path`); anything deeper is off-the-record.
        let profiles_root = data.join("browser/cef/profiles");
        for scope in [global_scope, a_scope, b_scope] {
            assert_eq!(
                profile_paths(data, &scope).root.parent(),
                Some(profiles_root.as_path())
            );
        }
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
