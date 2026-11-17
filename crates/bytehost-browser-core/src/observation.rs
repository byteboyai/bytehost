//! Structured observation. Page text is data, never host instructions; cookies,
//! storage and arbitrary files are not part of an observation.

use serde::{Deserialize, Serialize};

use crate::budget::Excerpt;
use crate::error::{BrowserError, BrowserResult};
use crate::ids::*;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
    /// Device pixels per CSS pixel (Retina/zoom). Coordinates in observations are CSS px.
    pub scale_factor: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementStates {
    pub visible: bool,
    pub enabled: bool,
    pub editable: bool,
    pub focusable: bool,
    pub focused: bool,
    pub obscured: bool,
    /// `Some` only for checkboxes/radios/switches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// `Some` only for options/selected rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FrameStatus {
    Loaded,
    /// Frame exists but could not be inspected. Kept in the list on purpose:
    /// silently dropping cross-origin/OOPIF frames would fake an empty page.
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub frame: FrameId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<FrameId>,
    pub url: String,
    pub security_origin: String,
    pub is_main: bool,
    pub cross_origin: bool,
    pub oopif: bool,
    pub status: FrameStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationKind {
    Text,
    Elements,
    Frames,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    pub kind: TruncationKind,
    pub total: usize,
    pub returned: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementInfo {
    /// Bound to snapshot/document/generation. Never a CSS selector string.
    pub element: ElementRef,
    pub frame: FrameId,
    pub role: String,
    pub name: String,
    pub tag: String,
    pub states: ElementStates,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    /// Open shadow roots from the outermost host to the element, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadow_path: Vec<String>,
}

/// One consistency point. Text, frames and elements all describe `document`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub identity: TargetIdentity,
    pub frame: FrameId,
    pub document: DocumentId,
    pub snapshot: SnapshotId,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub viewport: Viewport,
    #[serde(default)]
    pub frames: Vec<FrameInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Excerpt>,
    #[serde(default)]
    pub elements: Vec<ElementInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<Truncation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page: Option<u32>,
    pub captured_at_ms: u64,
}

impl Observation {
    /// Validate a ref from this snapshot before using it in a follow-up action.
    pub fn validate_element(&self, element: &ElementRef) -> BrowserResult<()> {
        element
            .validate_for(&self.identity, &self.frame, &self.document)
            .map_err(BrowserError::from_ref_mismatch)
    }

    pub fn find_element(&self, node: &ElementNodeId) -> Option<&ElementInfo> {
        self.elements
            .iter()
            .find(|element| &element.element.node == node)
    }
}
