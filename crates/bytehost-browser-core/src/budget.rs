//! Transmission budgets. Providers may return less, never more; the host clamps
//! client overrides to hard caps and reports every clamp instead of silently
//! truncating behavior.

use serde::{Deserialize, Serialize};

pub const OBSERVATION_CHARS: usize = 32 * 1024;
pub const OBSERVATION_ELEMENTS: usize = 200;
pub const SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;
pub const SCREENSHOT_PIXELS: u64 = 16_000_000;
pub const CONSOLE_EVENTS: usize = 500;
pub const NETWORK_EVENTS: usize = 1_000;
pub const DIAGNOSTICS_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub observation_chars: usize,
    pub observation_elements: usize,
    pub screenshot_bytes: usize,
    pub screenshot_pixels: u64,
    pub console_events: usize,
    pub network_events: usize,
    pub diagnostics_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            observation_chars: OBSERVATION_CHARS,
            observation_elements: OBSERVATION_ELEMENTS,
            screenshot_bytes: SCREENSHOT_BYTES,
            screenshot_pixels: SCREENSHOT_PIXELS,
            console_events: CONSOLE_EVENTS,
            network_events: NETWORK_EVENTS,
            diagnostics_bytes: DIAGNOSTICS_BYTES,
        }
    }
}

impl Limits {
    /// Hard caps are a product decision; config may lower but not raise them.
    pub const HARD_CAPS: Self = Self {
        observation_chars: OBSERVATION_CHARS,
        observation_elements: OBSERVATION_ELEMENTS,
        screenshot_bytes: SCREENSHOT_BYTES,
        screenshot_pixels: SCREENSHOT_PIXELS,
        console_events: CONSOLE_EVENTS,
        network_events: NETWORK_EVENTS,
        diagnostics_bytes: DIAGNOSTICS_BYTES,
    };

    pub fn clamped(self) -> Self {
        let cap = Self::HARD_CAPS;
        Self {
            observation_chars: self.observation_chars.clamp(1, cap.observation_chars),
            observation_elements: self.observation_elements.clamp(1, cap.observation_elements),
            screenshot_bytes: self.screenshot_bytes.clamp(1, cap.screenshot_bytes),
            screenshot_pixels: self.screenshot_pixels.clamp(1, cap.screenshot_pixels),
            console_events: self.console_events.clamp(1, cap.console_events),
            network_events: self.network_events.clamp(1, cap.network_events),
            diagnostics_bytes: self.diagnostics_bytes.clamp(1, cap.diagnostics_bytes),
        }
    }
}

/// Optional client override. Absent fields keep the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_elements: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_events: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_events: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clamp {
    pub field: String,
    pub requested: u64,
    pub applied: u64,
}

/// Effective limits plus the clamps the caller must surface to the user/agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReport {
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clamped: Vec<Clamp>,
}

pub fn resolve(overrides: LimitOverrides) -> BudgetReport {
    let mut limits = Limits::default();
    let mut clamped = Vec::new();
    apply(
        &mut limits.observation_chars,
        overrides.observation_chars,
        Limits::HARD_CAPS.observation_chars,
        "observation_chars",
        &mut clamped,
    );
    apply(
        &mut limits.observation_elements,
        overrides.observation_elements,
        Limits::HARD_CAPS.observation_elements,
        "observation_elements",
        &mut clamped,
    );
    apply(
        &mut limits.screenshot_bytes,
        overrides.screenshot_bytes,
        Limits::HARD_CAPS.screenshot_bytes,
        "screenshot_bytes",
        &mut clamped,
    );
    apply(
        &mut limits.console_events,
        overrides.console_events,
        Limits::HARD_CAPS.console_events,
        "console_events",
        &mut clamped,
    );
    apply(
        &mut limits.network_events,
        overrides.network_events,
        Limits::HARD_CAPS.network_events,
        "network_events",
        &mut clamped,
    );
    BudgetReport { limits, clamped }
}

fn apply(
    slot: &mut usize,
    requested: Option<usize>,
    hard_cap: usize,
    field: &'static str,
    clamped: &mut Vec<Clamp>,
) {
    if let Some(requested) = requested {
        let applied = requested.clamp(1, hard_cap);
        *slot = applied;
        if applied != requested {
            clamped.push(Clamp {
                field: field.to_owned(),
                requested: requested as u64,
                applied: applied as u64,
            });
        }
    }
}

/// Character-budgeted excerpt. `total` counts Unicode scalar values, not bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excerpt {
    pub text: String,
    pub total_chars: usize,
    pub returned_chars: usize,
    pub truncated: bool,
}

pub fn excerpt(text: &str, max_chars: usize) -> Excerpt {
    let max_chars = max_chars.max(1);
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return Excerpt {
            text: text.to_owned(),
            total_chars,
            returned_chars: total_chars,
            truncated: false,
        };
    }
    let text: String = text.chars().take(max_chars).collect();
    Excerpt {
        total_chars,
        returned_chars: text.chars().count(),
        text,
        truncated: true,
    }
}
