//! Capability negotiation. Existence of CDP does not imply Playwright parity:
//! the provider advertises exactly what it implements, and callers get
//! `unsupported_capability` for everything else.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::budget::Limits;
use crate::error::{BrowserError, ErrorClass};
use crate::ids::ProviderVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Navigate,
    Back,
    Forward,
    Reload,
    Stop,
    TabList,
    TabNew,
    TabActivate,
    TabClose,
    Observe,
    Click,
    Hover,
    Drag,
    Fill,
    TypeText,
    PressKey,
    Check,
    SelectOption,
    Scroll,
    Wait,
    Screenshot,
    DialogList,
    DialogHandle,
    Upload,
    DownloadList,
    Diagnostics,
    ClearScopeStorage,
}

impl ActionKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Back => "back",
            Self::Forward => "forward",
            Self::Reload => "reload",
            Self::Stop => "stop",
            Self::TabList => "tab_list",
            Self::TabNew => "tab_new",
            Self::TabActivate => "tab_activate",
            Self::TabClose => "tab_close",
            Self::Observe => "observe",
            Self::Click => "click",
            Self::Hover => "hover",
            Self::Drag => "drag",
            Self::Fill => "fill",
            Self::TypeText => "type",
            Self::PressKey => "press_key",
            Self::Check => "check",
            Self::SelectOption => "select_option",
            Self::Scroll => "scroll",
            Self::Wait => "wait",
            Self::Screenshot => "screenshot",
            Self::DialogList => "dialog_list",
            Self::DialogHandle => "dialog_handle",
            Self::Upload => "upload",
            Self::DownloadList => "download_list",
            Self::Diagnostics => "diagnostics",
            Self::ClearScopeStorage => "clear_scope_storage",
        }
    }

    /// Mutation actions are serialized per target and need stale-ref checks.
    pub const fn mutates_page(self) -> bool {
        matches!(
            self,
            Self::Navigate
                | Self::Back
                | Self::Forward
                | Self::Reload
                | Self::Click
                | Self::Hover
                | Self::Drag
                | Self::Fill
                | Self::TypeText
                | Self::PressKey
                | Self::Check
                | Self::SelectOption
                | Self::Scroll
                | Self::Upload
                | Self::DialogHandle
                | Self::ClearScopeStorage
        )
    }

    /// A write whose deadline passes after dispatch must be reported as
    /// `unknown_after_dispatch`, never silently retried.
    pub const fn may_be_unknown_after_dispatch(self) -> bool {
        matches!(
            self,
            Self::Click
                | Self::Drag
                | Self::Fill
                | Self::TypeText
                | Self::PressKey
                | Self::Check
                | Self::SelectOption
                | Self::Upload
                | Self::DialogHandle
                | Self::Navigate
                | Self::Back
                | Self::Forward
                | Self::Reload
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocatorKind {
    /// Preferred: snapshot element_ref.
    ElementRef,
    RoleName,
    Label,
    Text,
    Css,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    MainFrame,
    SameOriginChild,
    CrossOriginChild,
    OpenShadowRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Viewport,
    FullPage,
    Element,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub provider_version: ProviderVersion,
    pub actions: BTreeSet<ActionKind>,
    pub locators: BTreeSet<LocatorKind>,
    pub frames: BTreeSet<FrameKind>,
    pub captures: BTreeSet<CaptureKind>,
    pub diagnostics: BTreeSet<String>,
    pub limits: Limits,
    /// Free-form backend facts that never replace typed capability checks.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub backend_features: BTreeSet<String>,
}

impl Capabilities {
    pub fn require_action(&self, action: ActionKind) -> Result<(), BrowserError> {
        if self.actions.contains(&action) {
            Ok(())
        } else {
            Err(unsupported("action", action.code()))
        }
    }

    pub fn require_locator(&self, locator: LocatorKind) -> Result<(), BrowserError> {
        if self.locators.contains(&locator) {
            Ok(())
        } else {
            Err(unsupported("locator", &format!("{locator:?}")))
        }
    }

    pub fn require_frame(&self, frame: FrameKind) -> Result<(), BrowserError> {
        if self.frames.contains(&frame) {
            Ok(())
        } else {
            Err(unsupported("frame", &format!("{frame:?}")))
        }
    }

    pub fn require_capture(&self, capture: CaptureKind) -> Result<(), BrowserError> {
        if self.captures.contains(&capture) {
            Ok(())
        } else {
            Err(unsupported("capture", &format!("{capture:?}")))
        }
    }
}

fn unsupported(kind: &str, value: &str) -> BrowserError {
    BrowserError::new(
        ErrorClass::UnsupportedCapability,
        format!("unsupported {kind}: {value}"),
    )
    .with_detail(serde_json::json!({ "kind": kind, "value": value }))
}
