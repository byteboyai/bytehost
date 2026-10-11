//! `bytehost-core` 的目录/身份/布局端到端测试(计划 Task 04 要求的四条场景)。
//!
//! 覆盖:重复 ID 拒绝;无面板/单面板/空栏合法;未知历史 ID 可保留;应用 ID 从
//! 校验后的值映射。

use bytehost_core::{
    CatalogError, Layout, LayoutError, PanelCatalog, PanelId, PanelInstanceId, PanelMeta, Rail,
    ScopeId, SurfaceSlot,
};

fn pid(s: &str) -> PanelId {
    PanelId::new(s).expect("test id")
}

#[test]
fn duplicate_ids_are_rejected_at_registration() {
    let mut catalog = PanelCatalog::new();
    catalog
        .register(PanelMeta::single_surface(pid("shared.usage"), "Usage"))
        .unwrap();
    let err = catalog
        .register(PanelMeta::single_surface(
            pid("shared.usage"),
            "Usage (dup)",
        ))
        .unwrap_err();
    assert!(matches!(err, CatalogError::DuplicateId { .. }));
    // 目录仍只有一条,且标题未被覆盖。
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.get(&pid("shared.usage")).unwrap().title, "Usage");
}

#[test]
fn no_panel_single_panel_and_empty_rail_layouts_are_all_legal() {
    let mut catalog = PanelCatalog::new();
    catalog
        .register(PanelMeta::single_surface(pid("shared.usage"), "Usage"))
        .unwrap();

    let cases = [
        Layout::from_default(vec![]).unwrap(),
        Layout::from_default(vec![Rail::new(vec![], 280.0)]).unwrap(),
        Layout::from_default(vec![Rail::new(vec![pid("shared.usage")], 280.0)]).unwrap(),
    ];
    for layout in cases {
        let round = Layout::load(&layout.to_json(), &catalog).unwrap();
        assert_eq!(round.visible_panel_count(), layout.visible_panel_count());
    }
}

#[test]
fn unknown_historical_id_is_retained_for_migration() {
    let mut catalog = PanelCatalog::new();
    catalog
        .register(PanelMeta::single_surface(pid("shared.usage"), "Usage"))
        .unwrap();

    let json = r#"{
        "schema_version": 1,
        "rails": [ { "items": ["shared.usage", "legacy.gone"], "width": 300.0 } ]
    }"#;
    let layout = Layout::load(json, &catalog).unwrap();
    assert_eq!(layout.rails[0].items, vec![pid("shared.usage")]);
    assert_eq!(layout.retained_unknown, vec![pid("legacy.gone")]);
    // 且窗口照常可用(不是错误)。
    assert_eq!(layout.visible_panel_count(), 1);
}

#[test]
fn app_panel_id_is_mapped_from_a_validated_app_id() {
    // 产品侧先用 AppId 校验(此处以其字符串表示替代),再用 for_app 包装。
    let validated = "excalidraw";
    let id = PanelId::for_app(validated).unwrap();
    assert_eq!(id.as_str(), "app:excalidraw");
    assert_eq!(id.namespace(), "app");
    // 未校验的非法值被本类型挡住。
    assert!(PanelId::for_app("../escape").is_err());
}

#[test]
fn instance_generation_and_surface_slots_are_distinct() {
    let a = PanelInstanceId::new(2, 0);
    assert_eq!(a.next_generation(), PanelInstanceId::new(2, 1));
    assert_ne!(SurfaceSlot::MAIN, SurfaceSlot::new(1));
    assert_eq!(SurfaceSlot::MAIN.0, 0);
}

#[test]
fn scope_id_stays_opaque_to_the_host() {
    // Host 不解释业务含义,只保证非空与原样回传。
    let scope = ScopeId::new("project:42").unwrap();
    assert_eq!(scope.as_str(), "project:42");
    assert!(ScopeId::new("").is_err());
}

#[test]
fn newer_schema_is_rejected_so_callers_do_not_overwrite() {
    let catalog = PanelCatalog::new();
    let json = r#"{ "schema_version": 2, "rails": [] }"#;
    assert!(matches!(
        Layout::load(json, &catalog),
        Err(LayoutError::UnknownSchemaVersion { .. })
    ));
}
