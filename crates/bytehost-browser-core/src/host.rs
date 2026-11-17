//! The provider-facing host contract. Implementations (CEF, wry test double,
//! future backends) own engine handles; callers never see them.

use serde::{Deserialize, Serialize};

use crate::action::Command;
use crate::budget::LimitOverrides;
use crate::capabilities::{Capabilities, CaptureKind};
use crate::error::BrowserResult;
use crate::event::EventEnvelope;
use crate::ids::*;
use crate::observation::{Observation, Rect};
use crate::operation::{OperationHandle, OperationId, OperationRecord};
use crate::scope::ProfileScope;

pub const READ_TIMEOUT_MS: u64 = 10_000;
pub const INPUT_TIMEOUT_MS: u64 = 10_000;
pub const NAVIGATION_TIMEOUT_MS: u64 = 30_000;
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveRequest {
    pub target: TargetIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<FrameId>,
    #[serde(default)]
    pub all_frames: bool,
    #[serde(default)]
    pub include_shadow_dom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default)]
    pub limits: LimitOverrides,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRequest {
    pub target: TargetIdentity,
    pub capture: CaptureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<ElementRef>,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub mime: String,
    /// Bounded by `Limits::screenshot_bytes`; larger results are rejected or
    /// cropped with `truncated` set. Large payloads should move to artifacts
    /// before crossing a text UDS frame.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    pub captured_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitRequest {
    pub target: TargetIdentity,
    pub condition: crate::action::WaitCondition,
    pub timeout_ms: u64,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResult {
    pub satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEntry {
    pub at_ms: u64,
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsRequest {
    pub target: TargetIdentity,
    pub filter: crate::action::DiagnosticsFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsReport {
    pub entries: Vec<DiagnosticEntry>,
    pub dropped: u64,
    pub truncated: bool,
}

pub trait BrowserHost: Send + Sync {
    fn provider_version(&self) -> ProviderVersion;
    fn session(&self) -> BrowserSessionId;
    fn capabilities(&self) -> Capabilities;

    fn targets(&self, scope: &ProfileScope) -> BrowserResult<Vec<TargetInfo>>;
    fn execute(&self, command: Command) -> BrowserResult<OperationHandle>;
    fn operation_status(&self, operation: &OperationId) -> BrowserResult<OperationRecord>;
    fn cancel(&self, operation: &OperationId) -> BrowserResult<()>;

    fn observe(&self, request: ObserveRequest) -> BrowserResult<Observation>;
    fn capture(&self, request: CaptureRequest) -> BrowserResult<Capture>;
    fn wait(&self, request: WaitRequest) -> BrowserResult<WaitResult>;
    fn diagnostics(&self, request: DiagnosticsRequest) -> BrowserResult<DiagnosticsReport>;

    /// Replay events after `sequence`; bounded by the provider's buffer.
    fn events(&self, after: u64) -> Vec<EventEnvelope>;

    /// Ends targets/operations in `scope`, then removes site data. Must not touch
    /// other scopes or trusted app data.
    fn clear_scope_storage(&self, scope: &ProfileScope) -> BrowserResult<()>;
}
