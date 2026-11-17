//! Typed actions. There is deliberately no arbitrary CDP method, no JavaScript
//! `eval`, and no page-controlled `document_id`.

use serde::{Deserialize, Serialize};

use crate::budget::LimitOverrides;
use crate::capabilities::{ActionKind, Capabilities, CaptureKind, FrameKind, LocatorKind};
use crate::error::{BrowserError, BrowserResult, ErrorClass};
use crate::ids::*;
use crate::scope::ProfileScope;

pub const SCHEMA_VERSION: u32 = 2;

/// Correlation id supplied by the caller. Reused ids inside the dedupe window
/// must return the existing operation, not dispatch twice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    pub fn new(value: impl Into<String>) -> BrowserResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "request_id must not be empty",
            ));
        }
        if value.chars().count() > 128 {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "request_id is too long",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RequestId {
    type Error = BrowserError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RequestId> for String {
    fn from(value: RequestId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Modifiers {
    pub alt: bool,
    pub control: bool,
    pub meta: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl Default for MouseButton {
    fn default() -> Self {
        Self::Left
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameTarget {
    Main,
    Frame { frame: FrameId },
}

impl FrameTarget {
    pub fn kind(&self) -> FrameKind {
        match self {
            Self::Main => FrameKind::MainFrame,
            Self::Frame { .. } => FrameKind::SameOriginChild,
        }
    }
}

/// Locator choice. `ElementRef` is the stable path; role/name/label/text/css
/// re-resolve on each action and must be unique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Locator {
    ElementRef {
        element: ElementRef,
    },
    RoleName {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default)]
        exact: bool,
    },
    Label {
        text: String,
        #[serde(default)]
        exact: bool,
    },
    Text {
        text: String,
        #[serde(default)]
        exact: bool,
    },
    Css {
        selector: String,
    },
}

impl Locator {
    pub fn kind(&self) -> LocatorKind {
        match self {
            Self::ElementRef { .. } => LocatorKind::ElementRef,
            Self::RoleName { .. } => LocatorKind::RoleName,
            Self::Label { .. } => LocatorKind::Label,
            Self::Text { .. } => LocatorKind::Text,
            Self::Css { .. } => LocatorKind::Css,
        }
    }

    pub fn element_ref(&self) -> Option<&ElementRef> {
        match self {
            Self::ElementRef { element } => Some(element),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionTarget {
    pub frame: FrameTarget,
    pub locator: Locator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationWait {
    /// Main-frame commit only (default for navigation actions).
    Commit,
    DomContentLoaded,
    Load,
}

impl Default for NavigationWait {
    fn default() -> Self {
        Self::Commit
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentReadyState {
    Loading,
    Interactive,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCondition {
    ElementVisible {
        target: ActionTarget,
    },
    ElementHidden {
        target: ActionTarget,
    },
    TextContains {
        target: ActionTarget,
        text: String,
    },
    UrlMatches {
        pattern: String,
    },
    DocumentReady {
        state: DocumentReadyState,
    },
    /// SPA-friendly: an explicit element condition, not network-idle.
    ElementEnabled {
        target: ActionTarget,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScrollTarget {
    Page {
        x: i32,
        y: i32,
    },
    Container {
        target: ActionTarget,
        x: i32,
        y: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationScope {
    /// Main frame only.
    Main,
    /// Main frame plus every reachable child frame (with per-frame status).
    AllFrames,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticsFilter {
    Console,
    PageErrors,
    FailedRequests,
    All,
}

/// Typed action. Field budgets are enforced by the provider before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Navigate {
        url: String,
        #[serde(default)]
        wait: NavigationWait,
    },
    Back,
    Forward,
    Reload,
    Stop,
    TabList,
    TabNew {
        url: String,
    },
    TabActivate {
        tab: TabId,
    },
    TabClose {
        tab: TabId,
    },
    Observe {
        #[serde(default = "default_observation_scope")]
        scope: ObservationScope,
        #[serde(default)]
        include_shadow_dom: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<LimitOverrides>,
    },
    Click {
        target: ActionTarget,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "default_click_count")]
        click_count: u8,
        #[serde(default)]
        modifiers: Modifiers,
    },
    Hover {
        target: ActionTarget,
    },
    Drag {
        from: ActionTarget,
        to: ActionTarget,
    },
    Fill {
        target: ActionTarget,
        text: String,
        /// Replace (default) or insert at the current selection.
        #[serde(default = "default_true")]
        replace: bool,
    },
    Type {
        target: ActionTarget,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    PressKey {
        keys: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<ActionTarget>,
        #[serde(default)]
        modifiers: Modifiers,
    },
    Check {
        target: ActionTarget,
        checked: bool,
    },
    SelectOption {
        target: ActionTarget,
        values: Vec<String>,
    },
    Scroll {
        target: ScrollTarget,
    },
    Wait {
        condition: WaitCondition,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Screenshot {
        capture: CaptureKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<ActionTarget>,
    },
    DialogList,
    DialogHandle {
        dialog_id: String,
        accept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_text: Option<String>,
    },
    Upload {
        target: ActionTarget,
        files: Vec<String>,
    },
    DownloadList,
    Diagnostics {
        filter: DiagnosticsFilter,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<u32>,
    },
    ClearScopeStorage {
        scope: ProfileScope,
    },
}

const fn default_click_count() -> u8 {
    1
}

const fn default_true() -> bool {
    true
}

fn default_observation_scope() -> ObservationScope {
    ObservationScope::Main
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Navigate { .. } => ActionKind::Navigate,
            Self::Back => ActionKind::Back,
            Self::Forward => ActionKind::Forward,
            Self::Reload => ActionKind::Reload,
            Self::Stop => ActionKind::Stop,
            Self::TabList => ActionKind::TabList,
            Self::TabNew { .. } => ActionKind::TabNew,
            Self::TabActivate { .. } => ActionKind::TabActivate,
            Self::TabClose { .. } => ActionKind::TabClose,
            Self::Observe { .. } => ActionKind::Observe,
            Self::Click { .. } => ActionKind::Click,
            Self::Hover { .. } => ActionKind::Hover,
            Self::Drag { .. } => ActionKind::Drag,
            Self::Fill { .. } => ActionKind::Fill,
            Self::Type { .. } => ActionKind::TypeText,
            Self::PressKey { .. } => ActionKind::PressKey,
            Self::Check { .. } => ActionKind::Check,
            Self::SelectOption { .. } => ActionKind::SelectOption,
            Self::Scroll { .. } => ActionKind::Scroll,
            Self::Wait { .. } => ActionKind::Wait,
            Self::Screenshot { .. } => ActionKind::Screenshot,
            Self::DialogList => ActionKind::DialogList,
            Self::DialogHandle { .. } => ActionKind::DialogHandle,
            Self::Upload { .. } => ActionKind::Upload,
            Self::DownloadList => ActionKind::DownloadList,
            Self::Diagnostics { .. } => ActionKind::Diagnostics,
            Self::ClearScopeStorage { .. } => ActionKind::ClearScopeStorage,
        }
    }

    /// Actions that do not need an existing target.
    pub fn needs_target(&self) -> bool {
        !matches!(
            self,
            Self::TabList | Self::TabNew { .. } | Self::ClearScopeStorage { .. }
        )
    }

    pub fn targets(&self) -> Vec<&ActionTarget> {
        match self {
            Self::Click { target, .. }
            | Self::Hover { target }
            | Self::Fill { target, .. }
            | Self::Type { target, .. }
            | Self::Check { target, .. }
            | Self::SelectOption { target, .. }
            | Self::Upload { target, .. } => vec![target],
            Self::Drag { from, to } => vec![from, to],
            Self::PressKey { target, .. } => target.iter().collect(),
            Self::Screenshot { target, .. } => target.iter().collect(),
            Self::Scroll {
                target: ScrollTarget::Container { target, .. },
            } => vec![target],
            Self::Wait { condition, .. } => match condition {
                WaitCondition::ElementVisible { target }
                | WaitCondition::ElementHidden { target }
                | WaitCondition::TextContains { target, .. }
                | WaitCondition::ElementEnabled { target } => vec![target],
                WaitCondition::UrlMatches { .. } | WaitCondition::DocumentReady { .. } => {
                    Vec::new()
                }
            },
            _ => Vec::new(),
        }
    }
}

/// One typed command. `scope` is the only place product project identity enters;
/// the browser layer never sees a Dozer `Project` struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub schema_version: u32,
    pub request_id: RequestId,
    pub scope: ProfileScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetIdentity>,
    pub action: Action,
    /// Absolute unix-epoch ms. Never renewable: retries reuse the original value.
    pub deadline_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_document: Option<DocumentId>,
}

impl Command {
    pub fn validate(&self, capabilities: &Capabilities, now_ms: u64) -> BrowserResult<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                format!(
                    "unsupported schema_version {} (expected {SCHEMA_VERSION})",
                    self.schema_version
                ),
            ));
        }
        if self.deadline_ms <= now_ms {
            return Err(BrowserError::new(
                ErrorClass::Timeout,
                "command expired before execution",
            ));
        }
        capabilities.require_action(self.action.kind())?;
        if self.action.needs_target() && self.target.is_none() {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "this action requires an explicit target",
            ));
        }
        if !self.action.needs_target() && self.target.is_some() {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "this action must omit target",
            ));
        }
        if let Some(target) = &self.target
            && target.scope != self.scope
        {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "target scope does not match command scope",
            ));
        }
        for action_target in self.action.targets() {
            capabilities.require_locator(action_target.locator.kind())?;
            capabilities.require_frame(action_target.frame.kind())?;
            if let (Some(target), Locator::ElementRef { element }) =
                (&self.target, &action_target.locator)
            {
                if element.target.scope != target.scope {
                    return Err(BrowserError::new(
                        ErrorClass::StaleElement,
                        "element reference belongs to another scope",
                    ));
                }
                if element.target.target != target.target
                    || element.target.generation != target.generation
                {
                    return Err(BrowserError::from_ref_mismatch(RefMismatch::Generation));
                }
                if let Some(expected) = &self.expected_document
                    && &element.document != expected
                {
                    return Err(BrowserError::from_ref_mismatch(RefMismatch::Document));
                }
            }
        }
        if let Action::Click { click_count, .. } = &self.action
            && !(1..=3).contains(click_count)
        {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "click_count must be 1..=3",
            ));
        }
        Ok(())
    }
}
