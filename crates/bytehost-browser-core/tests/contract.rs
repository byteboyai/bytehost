use bytehost_browser_core::*;
use std::collections::BTreeSet;

fn provider() -> ProviderVersion {
    ProviderVersion::new("test-1").unwrap()
}
fn session() -> BrowserSessionId {
    BrowserSessionId::new("session-1").unwrap()
}
fn scope_project(key: &str) -> ProfileScope {
    ProfileScope::project(key).unwrap()
}
fn identity(scope: ProfileScope, target: &str, generation: u64) -> TargetIdentity {
    TargetIdentity::new(
        provider(),
        scope,
        session(),
        TargetId::new(target).unwrap(),
        TargetGeneration::new(generation),
    )
}
fn document(name: &str) -> DocumentId {
    DocumentId::new(name).unwrap()
}
fn element_ref(target: &TargetIdentity) -> ElementRef {
    ElementRef {
        target: target.clone(),
        frame: FrameId::new("frame-1").unwrap(),
        document: document("doc-1"),
        snapshot: SnapshotId::new("snap-1").unwrap(),
        node: ElementNodeId::new("node-1").unwrap(),
    }
}
fn capabilities() -> Capabilities {
    Capabilities {
        provider_version: provider(),
        actions: [
            ActionKind::Observe,
            ActionKind::Click,
            ActionKind::Fill,
            ActionKind::Navigate,
        ]
        .into_iter()
        .collect(),
        locators: [LocatorKind::ElementRef, LocatorKind::Css]
            .into_iter()
            .collect(),
        frames: [FrameKind::MainFrame].into_iter().collect(),
        captures: BTreeSet::new(),
        diagnostics: BTreeSet::new(),
        limits: Limits::default(),
        backend_features: BTreeSet::new(),
    }
}
fn now() -> u64 {
    1_700_000_000_000
}
fn command(target: TargetIdentity, action: Action) -> Command {
    Command {
        schema_version: SCHEMA_VERSION,
        request_id: RequestId::new("req-1").unwrap(),
        scope: target.scope.clone(),
        target: Some(target),
        action,
        deadline_ms: now() + 10_000,
        expected_document: Some(document("doc-1")),
    }
}
fn click_command(target: TargetIdentity) -> Command {
    let element = element_ref(&target);
    command(
        target,
        Action::Click {
            target: ActionTarget {
                frame: FrameTarget::Main,
                locator: Locator::ElementRef { element },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Modifiers::default(),
        },
    )
}

#[test]
fn command_round_trips_as_versioned_wire() {
    let target = identity(scope_project("p7"), "target-1", 3);
    let command = click_command(target);
    let json = serde_json::to_string(&command).unwrap();
    let back: Command = serde_json::from_str(&json).unwrap();
    assert_eq!(command, back);
    assert_eq!(back.schema_version, 2);
    assert!(json.contains("\"schema_version\":2"));
    assert!(json.contains("\"scope\":{\"kind\":\"project\",\"project\":\"p7\"}"));
}

#[test]
fn versioned_schema_rejects_unknown_fields_and_kinds() {
    let target = identity(scope_project("p7"), "target-1", 3);
    let mut value = serde_json::to_value(click_command(target)).unwrap();
    value["action"]["kind"] = serde_json::json!("eval_js");
    assert!(serde_json::from_value::<Command>(value.clone()).is_err());

    value["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Command>(value).is_err());
}

#[test]
fn capabilities_are_typed_not_implied_by_the_backend() {
    let target = identity(scope_project("p7"), "target-1", 3);
    let command = command(
        target,
        Action::Screenshot {
            capture: CaptureKind::Viewport,
            target: None,
        },
    );
    let error = command.validate(&capabilities(), now()).unwrap_err();
    assert_eq!(error.class, ErrorClass::UnsupportedCapability);
    assert_eq!(error.code(), "unsupported_capability");
    assert_eq!(error.class.retry_policy(), RetryPolicy::Never);
}

#[test]
fn command_validation_rejects_cross_scope_refs_and_expired_deadlines() {
    let target = identity(scope_project("p7"), "target-1", 3);
    let mut other = element_ref(&target);
    other.target.scope = scope_project("p8");
    let bad = command(
        target.clone(),
        Action::Click {
            target: ActionTarget {
                frame: FrameTarget::Main,
                locator: Locator::ElementRef { element: other },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Modifiers::default(),
        },
    );
    let error = bad.validate(&capabilities(), now()).unwrap_err();
    assert_eq!(error.class, ErrorClass::StaleElement);

    let mut expired = click_command(target);
    expired.deadline_ms = now();
    let error = expired.validate(&capabilities(), now()).unwrap_err();
    assert_eq!(error.class, ErrorClass::Timeout);
}

#[test]
fn element_ref_is_bound_to_target_generation_frame_and_document() {
    let target = identity(scope_project("p7"), "target-1", 3);
    let element = element_ref(&target);
    let frame = FrameId::new("frame-1").unwrap();
    assert!(
        element
            .validate_for(&target, &frame, &document("doc-1"))
            .is_ok()
    );

    let newer = identity(scope_project("p7"), "target-1", 4);
    assert_eq!(
        element.validate_for(&newer, &frame, &document("doc-1")),
        Err(RefMismatch::Generation)
    );
    assert_eq!(
        element.validate_for(&target, &frame, &document("doc-2")),
        Err(RefMismatch::Document)
    );
    let other_frame = FrameId::new("frame-2").unwrap();
    assert_eq!(
        element.validate_for(&target, &other_frame, &document("doc-1")),
        Err(RefMismatch::Frame)
    );
    let error = BrowserError::from_ref_mismatch(RefMismatch::Document);
    assert_eq!(error.class, ErrorClass::StaleDocument);
}

#[test]
fn events_filter_duplicates_and_older_target_generations() {
    let mut gate = EventGate::new();
    let mut buffer = EventBuffer::new(2);
    let event = |sequence, generation| EventEnvelope {
        sequence,
        at_ms: now() + sequence as u64,
        identity: Some(identity(scope_project("p7"), "target-1", generation)),
        event: BrowserEvent::TitleChanged {
            title: format!("t{generation}"),
        },
    };

    assert_eq!(
        buffer.push(&mut gate, event(2, 3)),
        EventAcceptance::Accepted
    );
    assert_eq!(
        buffer.push(&mut gate, event(2, 3)),
        EventAcceptance::DuplicateOrLate
    );
    assert_eq!(
        buffer.push(&mut gate, event(1, 3)),
        EventAcceptance::DuplicateOrLate
    );
    assert_eq!(
        buffer.push(&mut gate, event(3, 2)),
        EventAcceptance::StaleGeneration
    );
    // Generation 4 is a rebuilt target and is accepted.
    assert_eq!(
        buffer.push(&mut gate, event(4, 4)),
        EventAcceptance::Accepted
    );
    // Bounded buffer: the oldest accepted event is dropped, not silently kept.
    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer.dropped(), 0);
    assert_eq!(
        buffer.push(&mut gate, event(5, 4)),
        EventAcceptance::Accepted
    );
    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer.dropped(), 1);
    assert_eq!(gate.last_sequence(), 5);
    let replayed: Vec<_> = buffer.after(2).map(|event| event.sequence).collect();
    assert_eq!(replayed, vec![4, 5]);
}

#[test]
fn budgets_clamp_and_report_overrides() {
    let report = resolve(LimitOverrides {
        observation_chars: Some(usize::MAX),
        observation_elements: Some(0),
        ..Default::default()
    });
    assert_eq!(report.limits.observation_chars, OBSERVATION_CHARS);
    assert_eq!(report.limits.observation_elements, 1);
    assert_eq!(report.clamped.len(), 2);
    assert_eq!(report.clamped[0].field, "observation_chars");

    let text = "中文abc";
    let excerpt = excerpt(text, 2);
    assert_eq!(excerpt.returned_chars, 2);
    assert_eq!(excerpt.total_chars, 5);
    assert!(excerpt.truncated);
}

#[test]
fn operation_registry_is_idempotent_and_never_revives_unknown_writes() {
    let mut registry = OperationRegistry::new(8, 30_000);
    let target = identity(scope_project("p7"), "target-1", 3);
    let request = RequestId::new("req-1").unwrap();
    let first = registry
        .begin(
            request.clone(),
            target.scope.clone(),
            Some(target.clone()),
            now() + 10_000,
            now(),
        )
        .unwrap();
    assert!(!first.deduplicated);

    let second = registry
        .begin(
            request,
            target.scope.clone(),
            Some(target),
            now() + 10_000,
            now() + 1,
        )
        .unwrap();
    assert!(second.deduplicated);
    assert_eq!(first.operation, second.operation);
    assert_eq!(registry.len(), 1);

    registry
        .transition(
            &first.operation,
            OperationState::Running,
            now() + 2,
            None,
            None,
        )
        .unwrap();
    registry
        .transition(
            &first.operation,
            OperationState::UnknownAfterDispatch,
            now() + 3,
            None,
            None,
        )
        .unwrap();
    let error = registry
        .transition(
            &first.operation,
            OperationState::Completed,
            now() + 4,
            None,
            None,
        )
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::InvalidParams);
}

#[test]
fn expired_request_ids_are_not_replayed() {
    let mut registry = OperationRegistry::new(8, 1_000);
    let target = identity(scope_project("p7"), "target-1", 3);
    let request = RequestId::new("req-1").unwrap();
    registry
        .begin(
            request.clone(),
            target.scope.clone(),
            Some(target),
            now() + 10,
            now(),
        )
        .unwrap();
    let error = registry
        .begin(
            request,
            scope_project("p7"),
            None,
            now() + 10,
            now() + 20_000,
        )
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::RequestExpired);
}

#[test]
fn scope_is_opaque_and_path_safe() {
    assert_eq!(ProfileScope::Global.storage_segment(), "global");
    assert_eq!(scope_project("p7").storage_segment(), "project-p7");
    // CEF requires profile dirs to be direct children of the user-data root.
    assert!(!scope_project("p7").storage_segment().contains('/'));
    assert!(ProfileScope::project("../escape").is_err());
    assert!(ProfileScope::project("a/b").is_err());
    assert!(ProfileScope::project("").is_err());
    let json = serde_json::to_string(&scope_project("p7")).unwrap();
    assert_eq!(json, r#"{"kind":"project","project":"p7"}"#);
    let back: ProfileScope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, scope_project("p7"));
}

#[test]
fn invalid_identifiers_fail_on_construction_and_deserialization() {
    assert!(TargetId::new("").is_err());
    assert!(TargetId::new("a/b").is_err());
    assert!(TargetId::new("x".repeat(MAX_ID_CHARS + 1)).is_err());
    assert!(serde_json::from_str::<TargetId>(r#""../x""#).is_err());
}

#[test]
fn observation_round_trip_keeps_frame_and_snapshot_identity() {
    let target = identity(scope_project("p7"), "target-1", 3);
    let frame = FrameId::new("frame-1").unwrap();
    let observation = Observation {
        identity: target.clone(),
        frame: frame.clone(),
        document: document("doc-1"),
        snapshot: SnapshotId::new("snap-1").unwrap(),
        url: "https://example.test/".into(),
        title: "Example".into(),
        loading: false,
        viewport: Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 2.0,
        },
        frames: vec![FrameInfo {
            frame: frame.clone(),
            parent: None,
            url: "https://example.test/".into(),
            security_origin: "https://example.test".into(),
            is_main: true,
            cross_origin: false,
            oopif: false,
            status: FrameStatus::Loaded,
        }],
        text: Some(excerpt("hello", 100)),
        elements: vec![ElementInfo {
            element: element_ref(&target),
            frame,
            role: "button".into(),
            name: "Submit".into(),
            tag: "button".into(),
            states: ElementStates {
                visible: true,
                enabled: true,
                ..Default::default()
            },
            bounds: None,
            shadow_path: Vec::new(),
        }],
        truncated: Vec::new(),
        next_page: None,
        captured_at_ms: now(),
    };
    let json = serde_json::to_string(&observation).unwrap();
    let back: Observation = serde_json::from_str(&json).unwrap();
    assert_eq!(observation, back);
    assert!(back.validate_element(&element_ref(&target)).is_ok());
    assert!(
        back.find_element(&ElementNodeId::new("node-1").unwrap())
            .is_some()
    );
}
