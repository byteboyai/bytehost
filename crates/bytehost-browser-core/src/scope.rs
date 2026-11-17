//! Profile scope. One mapping from product project identity to site-data scope.
//!
//! Scope is a *site data domain*, not a tab, agent session or app version. It never
//! contains a filesystem path; the host derives directories from it (see Dozer's
//! profile adapter) so a project rename or a path with spaces cannot move data.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_PROJECT_KEY_CHARS: usize = 96;

/// Stable, opaque project key. Dozer passes its project id through
/// `ProjectKey::new(format!("p{project_id}"))`; the browser layer does not know
/// what a Dozer project is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectKey(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidProjectKey {
    pub value: String,
    pub reason: &'static str,
}

impl fmt::Display for InvalidProjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid project key {:?}: {}", self.value, self.reason)
    }
}

impl std::error::Error for InvalidProjectKey {}

impl ProjectKey {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidProjectKey> {
        let value = value.into();
        let invalid = |reason| InvalidProjectKey {
            value: value.clone(),
            reason,
        };
        if value.is_empty() {
            return Err(invalid("must not be empty"));
        }
        if value.chars().count() > MAX_PROJECT_KEY_CHARS {
            return Err(invalid("is too long"));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(invalid("may only contain ASCII alphanumerics, - and _"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProjectKey {
    type Error = InvalidProjectKey;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProjectKey> for String {
    fn from(value: ProjectKey) -> Self {
        value.0
    }
}

impl fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Site-data domain. `Global` is the home browser; project MCP must never touch it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileScope {
    Global,
    Project { project: ProjectKey },
}

impl ProfileScope {
    pub fn project(project: impl Into<String>) -> Result<Self, InvalidProjectKey> {
        Ok(Self::Project {
            project: ProjectKey::new(project)?,
        })
    }

    /// Relative segment under the browser data root. Always a descendant; no
    /// absolute path, no `..`, no user-provided separators.
    ///
    /// Project scopes stay on a single level (`project-<key>`) because CEF only
    /// accepts a request-context profile directory that is the user-data root
    /// itself or a direct child of it. A nested `projects/<key>` is rejected by
    /// Chromium (`Cannot create profile at path ...`) and silently falls back to
    /// an off-the-record profile, i.e. nothing persists.
    pub fn storage_segment(&self) -> String {
        match self {
            Self::Global => "global".to_owned(),
            Self::Project { project } => format!("project-{project}"),
        }
    }

    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }
}

impl fmt::Display for ProfileScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str("global"),
            Self::Project { project } => write!(f, "project:{project}"),
        }
    }
}
