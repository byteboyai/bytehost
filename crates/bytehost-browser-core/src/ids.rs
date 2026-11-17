//! Opaque identities. No UI index, no engine pointer, no product struct.
//!
//! All identifiers are validated strings so a compromised page or a stale
//! client cannot smuggle paths, whitespace or unbounded data into logs/filenames.
//! Constructors and `Deserialize` both validate.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::scope::ProfileScope;

pub const MAX_ID_CHARS: usize = 256;

/// Error returned by identifier constructors and deserializers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} {:?}: {}", self.kind, self.value, self.reason)
    }
}

impl std::error::Error for InvalidId {}

fn valid_opaque(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("must not be empty");
    }
    if value.chars().count() > MAX_ID_CHARS {
        return Err("is too long");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err("may only contain ASCII alphanumerics and . _ - :");
    }
    Ok(())
}

macro_rules! opaque_id {
    ($name:ident, $kind:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
                let value = value.into();
                valid_opaque(&value).map_err(|reason| InvalidId {
                    kind: $kind,
                    value: value.clone(),
                    reason,
                })?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(
    ProviderVersion,
    "provider_version",
    "Backend label plus version, e.g. `cef-156.0.30` or `webkit-wry-0.55`."
);
opaque_id!(
    BrowserSessionId,
    "browser_session_id",
    "One browser-service run. Changes on GUI restart; not a profile identity."
);
opaque_id!(
    TabId,
    "tab_id",
    "Browser panel tab identity. Never a UI index."
);
opaque_id!(
    TargetId,
    "target_id",
    "Engine target identity. Stable while the page lives."
);
opaque_id!(
    FrameId,
    "frame_id",
    "Frame identity from the host navigation/loader epoch."
);
opaque_id!(
    DocumentId,
    "document_id",
    "Document identity produced by the host, never by page script."
);
opaque_id!(
    SnapshotId,
    "snapshot_id",
    "One observation snapshot. Element refs are scoped to it."
);
opaque_id!(
    ElementNodeId,
    "element_node",
    "Backend node identity inside a snapshot. Opaque to callers."
);

/// Target generation. Any renderer rebuild or target recreation increments it.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TargetGeneration(u64);

impl TargetGeneration {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for TargetGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Everything needed to prove *which* engine target an operation or event refers to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetIdentity {
    pub provider: ProviderVersion,
    pub scope: ProfileScope,
    pub session: BrowserSessionId,
    pub target: TargetId,
    pub generation: TargetGeneration,
}

impl TargetIdentity {
    pub fn new(
        provider: ProviderVersion,
        scope: ProfileScope,
        session: BrowserSessionId,
        target: TargetId,
        generation: TargetGeneration,
    ) -> Self {
        Self {
            provider,
            scope,
            session,
            target,
            generation,
        }
    }
}

/// Reference returned by `observe` and accepted by typed actions.
///
/// It is bound to provider/scope/session/target/generation/frame/document/snapshot.
/// Using it against another target or after a new document is a hard error, never
/// a best-effort CSS fallback.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ElementRef {
    pub target: TargetIdentity,
    pub frame: FrameId,
    pub document: DocumentId,
    pub snapshot: SnapshotId,
    pub node: ElementNodeId,
}

/// Why a stored [`ElementRef`] cannot be used for the requested target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefMismatch {
    Scope,
    Session,
    Target,
    Generation,
    Frame,
    Document,
}

impl fmt::Display for RefMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Scope => "scope",
            Self::Session => "session",
            Self::Target => "target",
            Self::Generation => "generation",
            Self::Frame => "frame",
            Self::Document => "document",
        })
    }
}

impl ElementRef {
    /// Validate before any side effect. `frame`/`document` are the values the
    /// provider resolved *now*; callers must not reuse an old document id when
    /// the page navigated (even to the same URL).
    pub fn validate_for(
        &self,
        target: &TargetIdentity,
        frame: &FrameId,
        document: &DocumentId,
    ) -> Result<(), RefMismatch> {
        if self.target.scope != target.scope {
            return Err(RefMismatch::Scope);
        }
        if self.target.session != target.session {
            return Err(RefMismatch::Session);
        }
        if self.target.target != target.target {
            return Err(RefMismatch::Target);
        }
        if self.target.generation != target.generation {
            return Err(RefMismatch::Generation);
        }
        if &self.frame != frame {
            return Err(RefMismatch::Frame);
        }
        if &self.document != document {
            return Err(RefMismatch::Document);
        }
        Ok(())
    }
}

/// One tab/target as listed by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub identity: TargetIdentity,
    pub tab: TabId,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub active: bool,
    /// `true` while an operation lease or the visible surface keeps it alive.
    pub retained: bool,
}
