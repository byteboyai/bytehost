//! CEF backend.
//!
//! - Dependency seam: [`CefProvider`] implements the neutral `BrowserHost` shape
//!   without pulling the native SDK.
//! - `profile`: stable `browser/cef/profiles/...` paths (always available).
//! - `runtime` (feature `cef-runtime`): real in-process CEF initialization,
//!   native child surfaces, per-scope request contexts, external message pump,
//!   target lifecycle and DevTools transport. It must run on the GUI thread.

pub mod profile;

#[cfg(any(feature = "cef-runtime", test))]
mod runtime_guard;

#[cfg(feature = "cef-runtime")]
pub mod runtime;

pub use bytehost_browser_core as core;

use bytehost_browser_core::*;

/// Placeholder provider with valid identity/capability plumbing.
///
/// The real GUI-thread runtime is in `runtime`; product code must not share CEF
/// handles across threads. This type exists so the dependency direction and error
/// taxonomy can be tested without the native SDK.
pub struct CefProvider {
    version: ProviderVersion,
    session: BrowserSessionId,
    capabilities: Capabilities,
    targets: Vec<TargetInfo>,
}

impl CefProvider {
    pub fn new(
        version: ProviderVersion,
        session: BrowserSessionId,
        capabilities: Capabilities,
        targets: Vec<TargetInfo>,
    ) -> Self {
        Self {
            version,
            session,
            capabilities,
            targets,
        }
    }

    fn unavailable(&self, what: &str) -> BrowserError {
        BrowserError::new(
            ErrorClass::ProviderUnavailable,
            format!("CEF provider is not wired yet: {what}"),
        )
        .with_detail(serde_json::json!({ "provider": self.version.as_str() }))
    }
}

impl BrowserHost for CefProvider {
    fn provider_version(&self) -> ProviderVersion {
        self.version.clone()
    }

    fn session(&self) -> BrowserSessionId {
        self.session.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn targets(&self, scope: &ProfileScope) -> BrowserResult<Vec<TargetInfo>> {
        Ok(self
            .targets
            .iter()
            .filter(|target| &target.identity.scope == scope)
            .cloned()
            .collect())
    }

    fn execute(&self, _command: Command) -> BrowserResult<OperationHandle> {
        Err(self.unavailable("execute"))
    }

    fn operation_status(&self, _operation: &OperationId) -> BrowserResult<OperationRecord> {
        Err(self.unavailable("operation_status"))
    }

    fn cancel(&self, _operation: &OperationId) -> BrowserResult<()> {
        Err(self.unavailable("cancel"))
    }

    fn observe(&self, _request: ObserveRequest) -> BrowserResult<Observation> {
        Err(self.unavailable("observe"))
    }

    fn capture(&self, _request: CaptureRequest) -> BrowserResult<Capture> {
        Err(self.unavailable("capture"))
    }

    fn wait(&self, _request: WaitRequest) -> BrowserResult<WaitResult> {
        Err(self.unavailable("wait"))
    }

    fn diagnostics(&self, _request: DiagnosticsRequest) -> BrowserResult<DiagnosticsReport> {
        Err(self.unavailable("diagnostics"))
    }

    fn events(&self, _after: u64) -> Vec<EventEnvelope> {
        Vec::new()
    }

    fn clear_scope_storage(&self, _scope: &ProfileScope) -> BrowserResult<()> {
        Err(self.unavailable("clear_scope_storage"))
    }
}
