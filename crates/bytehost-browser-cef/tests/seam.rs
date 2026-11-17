use bytehost_browser_cef::CefProvider;
use bytehost_browser_core::*;
use std::collections::BTreeSet;

fn provider() -> ProviderVersion {
    ProviderVersion::new("cef-156.0.30").unwrap()
}
fn session() -> BrowserSessionId {
    BrowserSessionId::new("session-1").unwrap()
}
fn scope(project: &str) -> ProfileScope {
    ProfileScope::project(project).unwrap()
}
fn target(scope: ProfileScope, id: &str) -> TargetInfo {
    TargetInfo {
        identity: TargetIdentity::new(
            provider(),
            scope,
            session(),
            TargetId::new(id).unwrap(),
            TargetGeneration::ZERO,
        ),
        tab: TabId::new(format!("tab-{id}")).unwrap(),
        url: "https://example.test/".into(),
        title: "Example".into(),
        loading: false,
        active: false,
        retained: true,
    }
}

#[test]
fn seam_keeps_identity_and_scope_filtering_but_does_not_fake_a_browser() {
    let capabilities = Capabilities {
        provider_version: provider(),
        actions: BTreeSet::new(),
        locators: BTreeSet::new(),
        frames: BTreeSet::new(),
        captures: BTreeSet::new(),
        diagnostics: BTreeSet::new(),
        limits: Limits::default(),
        backend_features: BTreeSet::new(),
    };
    let host = CefProvider::new(
        provider(),
        session(),
        capabilities,
        vec![
            target(scope("p7"), "a"),
            target(scope("p8"), "b"),
            target(ProfileScope::Global, "g"),
        ],
    );

    assert_eq!(host.provider_version().as_str(), "cef-156.0.30");
    assert_eq!(host.targets(&scope("p7")).unwrap().len(), 1);
    assert_eq!(host.targets(&scope("p8")).unwrap()[0].tab.as_str(), "tab-b");
    assert_eq!(host.targets(&ProfileScope::Global).unwrap().len(), 1);
    assert!(host.capabilities().actions.is_empty());

    let command = Command {
        schema_version: SCHEMA_VERSION,
        request_id: RequestId::new("req-1").unwrap(),
        scope: scope("p7"),
        target: None,
        action: Action::TabList,
        deadline_ms: u64::MAX,
        expected_document: None,
    };
    let error = host.execute(command).unwrap_err();
    assert_eq!(error.class, ErrorClass::ProviderUnavailable);
    assert_eq!(error.class.retry_policy(), RetryPolicy::Never);

    assert!(host.events(0).is_empty());
    assert_eq!(
        host.clear_scope_storage(&scope("p7")).unwrap_err().class,
        ErrorClass::ProviderUnavailable
    );
}
