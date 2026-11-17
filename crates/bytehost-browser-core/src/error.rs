//! Structured error taxonomy. MCP/daemon may render these, but never flatten
//! them into one "browser failed" string.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

use crate::ids::{RefMismatch, TargetIdentity};
use crate::operation::OperationId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    InvalidParams,
    UnsupportedCapability,
    ProjectUnavailable,
    TargetUnavailable,
    ProviderUnavailable,
    StaleDocument,
    StaleElement,
    AmbiguousTarget,
    NotInteractable,
    Timeout,
    Cancelled,
    UserInterrupted,
    UnknownAfterDispatch,
    RequestExpired,
    Forbidden,
    Internal,
}

/// What a caller may do after seeing this class. The important rule: a write that
/// may have reached the page is never retried automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPolicy {
    /// Read/idempotent operation: retrying is safe within the deadline.
    Safe,
    /// Re-observe the page, then decide. Never replay the write blindly.
    ObserveFirst,
    /// Do not retry; the request or scope is invalid/forbidden.
    Never,
}

impl ErrorClass {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidParams => "invalid_params",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::ProjectUnavailable => "project_unavailable",
            Self::TargetUnavailable => "target_unavailable",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::StaleDocument => "stale_document",
            Self::StaleElement => "stale_element",
            Self::AmbiguousTarget => "ambiguous_target",
            Self::NotInteractable => "not_interactable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::UserInterrupted => "user_interrupted",
            Self::UnknownAfterDispatch => "unknown_after_dispatch",
            Self::RequestExpired => "request_expired",
            Self::Forbidden => "navigation_denied",
            Self::Internal => "internal",
        }
    }

    pub const fn retry_policy(self) -> RetryPolicy {
        match self {
            Self::Timeout | Self::TargetUnavailable => RetryPolicy::Safe,
            Self::StaleDocument | Self::StaleElement | Self::AmbiguousTarget => {
                RetryPolicy::ObserveFirst
            }
            Self::UnknownAfterDispatch | Self::UserInterrupted => RetryPolicy::ObserveFirst,
            Self::InvalidParams
            | Self::UnsupportedCapability
            | Self::ProjectUnavailable
            | Self::ProviderUnavailable
            | Self::NotInteractable
            | Self::Cancelled
            | Self::RequestExpired
            | Self::Forbidden
            | Self::Internal => RetryPolicy::Never,
        }
    }

    pub const fn is_recoverable(self) -> bool {
        !matches!(
            self,
            Self::InvalidParams
                | Self::UnsupportedCapability
                | Self::Forbidden
                | Self::Internal
                | Self::RequestExpired
        )
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserError {
    pub class: ErrorClass,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl BrowserError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            target: None,
            operation: None,
            detail: None,
        }
    }

    pub fn with_target(mut self, target: TargetIdentity) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_operation(mut self, operation: OperationId) -> Self {
        self.operation = Some(operation);
        self
    }

    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn code(&self) -> &'static str {
        self.class.code()
    }

    pub fn from_ref_mismatch(mismatch: RefMismatch) -> Self {
        let class = match mismatch {
            RefMismatch::Document => ErrorClass::StaleDocument,
            _ => ErrorClass::StaleElement,
        };
        Self::new(
            class,
            format!("element reference does not belong to this {mismatch}; observe again"),
        )
        .with_detail(serde_json::json!({ "component": mismatch.to_string() }))
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.class.code(), self.message)
    }
}

impl std::error::Error for BrowserError {}

pub type BrowserResult<T> = Result<T, BrowserError>;
