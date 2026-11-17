//! Operation lifecycle and bounded idempotency.
//!
//! A short RPC may return an `operation_id`; long work is queried/cancelled.
//! A write whose deadline passes after dispatch ends in `unknown_after_dispatch`
//! and must be followed by observation, never by an automatic replay.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fmt;

use crate::action::RequestId;
use crate::error::{BrowserError, BrowserResult, ErrorClass};
use crate::ids::TargetIdentity;
use crate::scope::ProfileScope;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OperationId(String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> BrowserResult<Self> {
        let value = value.into();
        if value.is_empty() || value.chars().count() > 128 {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "operation id must be 1..=128 characters",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OperationId {
    type Error = BrowserError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<OperationId> for String {
    fn from(value: OperationId) -> Self {
        value.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Accepted,
    Running,
    AwaitingUser,
    Completed,
    Failed,
    Cancelled,
    /// Dispatch may have reached the page; observe before any retry.
    UnknownAfterDispatch,
    Expired,
}

impl OperationState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::UnknownAfterDispatch
                | Self::Expired
        )
    }

    pub const fn allows(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (Self::Accepted, Self::Running)
                | (Self::Accepted, Self::AwaitingUser)
                | (Self::Accepted, Self::Completed)
                | (Self::Accepted, Self::Failed)
                | (Self::Accepted, Self::Cancelled)
                | (Self::Accepted, Self::Expired)
                | (Self::Accepted, Self::UnknownAfterDispatch)
                | (Self::Running, Self::AwaitingUser)
                | (Self::Running, Self::Completed)
                | (Self::Running, Self::Failed)
                | (Self::Running, Self::Cancelled)
                | (Self::Running, Self::Expired)
                | (Self::Running, Self::UnknownAfterDispatch)
                | (Self::AwaitingUser, Self::Running)
                | (Self::AwaitingUser, Self::Completed)
                | (Self::AwaitingUser, Self::Failed)
                | (Self::AwaitingUser, Self::Cancelled)
                | (Self::AwaitingUser, Self::Expired)
                | (Self::AwaitingUser, Self::UnknownAfterDispatch)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: OperationId,
    pub request_id: RequestId,
    pub scope: ProfileScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetIdentity>,
    pub state: OperationState,
    pub deadline_ms: u64,
    pub accepted_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BrowserError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationHandle {
    pub operation: OperationId,
    pub state: OperationState,
    /// `true` when the request id was already known inside the dedupe window.
    pub deduplicated: bool,
}

/// Bounded execution records. Records are retained at least until the original
/// deadline plus a reconnect grace; expired replays get `request_expired`.
pub struct OperationRegistry {
    capacity: usize,
    grace_ms: u64,
    next_serial: u64,
    order: VecDeque<OperationId>,
    records: HashMap<OperationId, OperationRecord>,
    by_request: HashMap<RequestId, OperationId>,
}

impl OperationRegistry {
    pub fn new(capacity: usize, grace_ms: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            grace_ms,
            next_serial: 0,
            order: VecDeque::new(),
            records: HashMap::new(),
            by_request: HashMap::new(),
        }
    }

    pub fn begin(
        &mut self,
        request_id: RequestId,
        scope: ProfileScope,
        target: Option<TargetIdentity>,
        deadline_ms: u64,
        now_ms: u64,
    ) -> BrowserResult<OperationHandle> {
        if let Some(existing) = self.by_request.get(&request_id)
            && let Some(record) = self.records.get(existing)
        {
            if record.deadline_ms.saturating_add(self.grace_ms) <= now_ms {
                return Err(BrowserError::new(
                    ErrorClass::RequestExpired,
                    "request id expired; observe and issue a new request",
                ));
            }
            return Ok(OperationHandle {
                operation: record.id.clone(),
                state: record.state,
                deduplicated: true,
            });
        }

        self.evict(now_ms);
        if self.records.len() >= self.capacity {
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "operation registry is full",
            ));
        }

        self.next_serial = self.next_serial.saturating_add(1);
        let id = OperationId::new(format!("op-{}", self.next_serial))?;
        let record = OperationRecord {
            id: id.clone(),
            request_id: request_id.clone(),
            scope,
            target,
            state: OperationState::Accepted,
            deadline_ms,
            accepted_at_ms: now_ms,
            finished_at_ms: None,
            result: None,
            error: None,
        };
        self.order.push_back(id.clone());
        self.by_request.insert(request_id, id.clone());
        self.records.insert(id.clone(), record);
        Ok(OperationHandle {
            operation: id,
            state: OperationState::Accepted,
            deduplicated: false,
        })
    }

    pub fn transition(
        &mut self,
        id: &OperationId,
        next: OperationState,
        now_ms: u64,
        result: Option<Value>,
        error: Option<BrowserError>,
    ) -> BrowserResult<()> {
        let record = self
            .records
            .get_mut(id)
            .ok_or_else(|| BrowserError::new(ErrorClass::InvalidParams, "unknown operation id"))?;
        if !record.state.allows(next) {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                format!(
                    "illegal operation transition {:?} -> {next:?}",
                    record.state
                ),
            ));
        }
        record.state = next;
        record.result = result;
        record.error = error;
        if next.is_terminal() {
            record.finished_at_ms = Some(now_ms);
        }
        Ok(())
    }

    pub fn status(&self, id: &OperationId) -> Option<&OperationRecord> {
        self.records.get(id)
    }

    /// Expires non-terminal records whose deadline passed. Returns expired ids.
    pub fn expire(&mut self, now_ms: u64) -> Vec<OperationId> {
        let mut expired = Vec::new();
        for record in self.records.values_mut() {
            if !record.state.is_terminal() && record.deadline_ms <= now_ms {
                record.state = OperationState::Expired;
                record.finished_at_ms = Some(now_ms);
                expired.push(record.id.clone());
            }
        }
        expired
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn evict(&mut self, now_ms: u64) {
        while let Some(front) = self.order.front().cloned() {
            let evictable = self.records.get(&front).is_some_and(|record| {
                record.state.is_terminal()
                    && record
                        .deadline_ms
                        .saturating_add(self.grace_ms)
                        .saturating_add(60_000)
                        <= now_ms
            });
            if !evictable {
                break;
            }
            self.order.pop_front();
            if let Some(record) = self.records.remove(&front) {
                self.by_request.remove(&record.request_id);
            }
        }
    }
}
