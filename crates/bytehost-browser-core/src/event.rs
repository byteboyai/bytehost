//! Events carry identity, sequence and time. Late/duplicate events and events
//! from an older target generation must not drive UI or operation state.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::ids::*;
use crate::operation::OperationId;
use crate::scope::ProfileScope;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserEvent {
    SessionCreated,
    SessionClosed,
    TargetCreated {
        tab: TabId,
    },
    TargetClosed {
        tab: TabId,
    },
    NavigationStarted {
        url: String,
        is_main: bool,
    },
    NavigationCommitted {
        url: String,
        is_main: bool,
        document: DocumentId,
    },
    NavigationReady {
        url: String,
        is_main: bool,
        document: DocumentId,
        http_status: i32,
    },
    NavigationFailed {
        url: String,
        is_main: bool,
        code: i32,
        message: String,
    },
    TitleChanged {
        title: String,
    },
    UrlChanged {
        url: String,
        is_main: bool,
    },
    Popup {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opener: Option<TargetId>,
    },
    DialogOpened {
        dialog_id: String,
        dialog_kind: String,
        message: String,
    },
    DialogClosed {
        dialog_id: String,
    },
    Console {
        level: String,
        message: String,
        source: String,
        line: u32,
    },
    PageError {
        message: String,
        source: String,
        line: u32,
    },
    RequestFailed {
        url: String,
        method: String,
        error: String,
    },
    Download {
        download_id: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        state: String,
    },
    OperationCompleted {
        operation: OperationId,
        state: crate::operation::OperationState,
    },
    UserInterrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation: Option<OperationId>,
    },
    RendererCrashed,
    ProviderUnavailable {
        reason: String,
    },
    ScopeStorageCleared {
        scope: ProfileScope,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Monotonic per browser service run.
    pub sequence: u64,
    pub at_ms: u64,
    /// Absent for service/session-level events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<TargetIdentity>,
    pub event: BrowserEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAcceptance {
    Accepted,
    /// Sequence already seen (duplicate or reordered after a newer event).
    DuplicateOrLate,
    /// Same target, older generation: the page was replaced.
    StaleGeneration,
}

#[derive(Debug, Default)]
pub struct EventGate {
    last_sequence: u64,
    targets: HashMap<TargetId, (BrowserSessionId, TargetGeneration)>,
}

impl EventGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn admit(&mut self, envelope: &EventEnvelope) -> EventAcceptance {
        if envelope.sequence <= self.last_sequence {
            return EventAcceptance::DuplicateOrLate;
        }
        if let Some(identity) = &envelope.identity {
            match self.targets.get(&identity.target) {
                Some((session, generation))
                    if session == &identity.session && identity.generation < *generation =>
                {
                    return EventAcceptance::StaleGeneration;
                }
                Some((session, generation))
                    if session == &identity.session && identity.generation == *generation => {}
                _ => {
                    self.targets.insert(
                        identity.target.clone(),
                        (identity.session.clone(), identity.generation),
                    );
                }
            }
        }
        self.last_sequence = envelope.sequence;
        EventAcceptance::Accepted
    }
}

/// Bounded replay buffer for diagnostics/panel state. `dropped` counts events
/// evicted from the front, never events rejected by the gate.
#[derive(Debug)]
pub struct EventBuffer {
    capacity: usize,
    entries: VecDeque<EventEnvelope>,
    dropped: u64,
}

impl EventBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
            dropped: 0,
        }
    }

    pub fn push(&mut self, gate: &mut EventGate, envelope: EventEnvelope) -> EventAcceptance {
        let acceptance = gate.admit(&envelope);
        if acceptance != EventAcceptance::Accepted {
            return acceptance;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(envelope);
        acceptance
    }

    pub fn after(&self, sequence: u64) -> impl Iterator<Item = &EventEnvelope> {
        self.entries
            .iter()
            .filter(move |envelope| envelope.sequence > sequence)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
