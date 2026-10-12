//! 布局迁移兼容测试(Task 10,计划 `tests/layout_compat.rs`)。
//!
//! 用一个**产品形状**的假适配器(两栏 `rail_layout.left/right`,元素是
//! `"Files"` 之类 token)验证宿主侧的通用迁移流程:版本闸门、未知 token 保留、
//! 栏宽钳制、原子落盘 + 备份。Dozer 的真实样本与黄金期望在 Dozer 侧
//! (`crates/dozer-app/src/app/layout_compat.rs`)验证;此处只钉住与产品无关
//! 的机制。

use bytehost_core::{LAYOUT_SCHEMA_VERSION, Layout, PanelId};
use bytehost_desktop::layout::{
    LegacyPanelAdapter, MigrateError, atomic_save_layout, migrate_legacy_file,
    migrate_legacy_layout, reconcile_with_catalog,
};

/// 每个测试一个独立临时目录(避免引入 tempfile 依赖)。
fn temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bh-layout-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 假产品适配器:两栏,`"dozer.files"` 这类 token 直接构 `PanelId`。
struct FakeAdapter;

impl LegacyPanelAdapter for FakeAdapter {
    fn rails_from_legacy(&self, legacy: &serde_json::Value) -> Vec<Vec<String>> {
        let obj = legacy.as_object().expect("调用方保证是对象");
        let side = |k: &str| -> Vec<String> {
            obj.get("left")
                .filter(|_| k == "left")
                .or_else(|| obj.get("right").filter(|_| k == "right"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        vec![side("left"), side("right")]
    }

    fn map_token(&self, token: &str) -> Option<PanelId> {
        match token {
            "Files" => Some(PanelId::new("dozer.files").unwrap()),
            "Usage" => Some(PanelId::new("dozer.usage").unwrap()),
            "Agent" => Some(PanelId::new("dozer.agent").unwrap()),
            _ => None,
        }
    }

    fn default_rail_width(&self) -> f32 {
        300.0
    }
}

fn catalog() -> bytehost_core::PanelCatalog {
    use bytehost_core::PanelMeta;
    let mut c = bytehost_core::PanelCatalog::new();
    for id in ["dozer.files", "dozer.usage", "dozer.agent"] {
        c.register(PanelMeta {
            id: PanelId::new(id).unwrap(),
            title: id.to_string(),
            surfaces: vec![],
        })
        .unwrap();
    }
    c
}

#[test]
fn legacy_rails_translate_to_stable_ids() {
    let legacy = serde_json::json!({
        "left": ["Files", "Usage"],
        "right": ["Agent"],
        "left_width": 500.0
    });
    let out = migrate_legacy_layout(&legacy, &FakeAdapter).unwrap();
    assert_eq!(out.layout.schema_version, LAYOUT_SCHEMA_VERSION);
    assert_eq!(out.layout.rails.len(), 2);
    assert_eq!(
        out.layout.rails[0].items,
        vec![
            PanelId::new("dozer.files").unwrap(),
            PanelId::new("dozer.usage").unwrap()
        ]
    );
    assert_eq!(
        out.layout.rails[1].items,
        vec![PanelId::new("dozer.agent").unwrap()]
    );
    assert!(out.retained.is_empty());
}

#[test]
fn unknown_tokens_are_retained_not_dropped() {
    let legacy = serde_json::json!({
        "left": ["Files", "GoneLegacy"],
        "right": []
    });
    let out = migrate_legacy_layout(&legacy, &FakeAdapter).unwrap();
    assert_eq!(out.retained, vec!["GoneLegacy"]);
    // 已知的留下,未知的不进任何栏。
    assert_eq!(
        out.layout.rails[0].items,
        vec![PanelId::new("dozer.files").unwrap()]
    );
}

#[test]
fn newer_schema_version_is_rejected() {
    let legacy = serde_json::json!({ "schema_version": 99, "left": [], "right": [] });
    assert_eq!(
        migrate_legacy_layout(&legacy, &FakeAdapter),
        Err(MigrateError::UnsupportedNewerVersion {
            found: 99,
            supported: LAYOUT_SCHEMA_VERSION
        })
    );
}

#[test]
fn non_object_legacy_is_rejected() {
    let legacy = serde_json::json!([1, 2, 3]);
    assert_eq!(
        migrate_legacy_layout(&legacy, &FakeAdapter),
        Err(MigrateError::NotAnObject)
    );
}

#[test]
fn migrate_file_missing_or_corrupt_yields_none() {
    let dir = temp_dir("compat");
    let missing = dir.join("nope.json");
    assert!(
        migrate_legacy_file(&missing, &FakeAdapter)
            .unwrap()
            .is_none()
    );

    let corrupt = dir.join("corrupt.json");
    std::fs::write(&corrupt, "not json").unwrap();
    assert!(
        migrate_legacy_file(&corrupt, &FakeAdapter)
            .unwrap()
            .is_none()
    );
}

#[test]
fn unknown_valid_ids_go_to_retained_unknown_via_catalog() {
    let legacy = serde_json::json!({ "left": ["Files"], "right": ["Usage"] });
    let out = migrate_legacy_layout(&legacy, &FakeAdapter).unwrap();
    // 目录里没有 "dozer.usage"?加上它,只留 files。
    let mut c = bytehost_core::PanelCatalog::new();
    c.register(bytehost_core::PanelMeta {
        id: PanelId::new("dozer.files").unwrap(),
        title: "Files".into(),
        surfaces: vec![],
    })
    .unwrap();
    let reconciled = reconcile_with_catalog(out.layout, &c).unwrap();
    assert_eq!(
        reconciled.rails[0].items,
        vec![PanelId::new("dozer.files").unwrap()]
    );
    assert!(
        reconciled
            .retained_unknown
            .contains(&PanelId::new("dozer.usage").unwrap()),
        "目录未注册的合法 id 应进 retained_unknown"
    );
}

#[test]
fn atomic_save_writes_and_backs_up_once() {
    let dir = temp_dir("compat");
    let path = dir.join("layout.json");

    let layout = Layout::from_default(vec![bytehost_core::Rail::new(
        vec![PanelId::new("dozer.files").unwrap()],
        320.0,
    )])
    .unwrap();
    atomic_save_layout(&path, &layout).unwrap();
    assert!(path.exists());
    assert!(
        !path.with_extension("json.bak").exists(),
        "首次无旧文件不备份"
    );

    // 改内容再存 → 生成一次备份,且备份是旧内容。
    let layout2 = Layout::from_default(vec![bytehost_core::Rail::new(vec![], 400.0)]).unwrap();
    atomic_save_layout(&path, &layout2).unwrap();
    let backup = path.with_extension("json.bak");
    assert!(backup.exists());
    let back = std::fs::read_to_string(&backup).unwrap();
    assert!(back.contains("dozer.files"), "备份保留旧内容");
    // 再存一次不覆盖已有备份。
    atomic_save_layout(&path, &layout).unwrap();
    assert!(
        std::fs::read_to_string(&backup)
            .unwrap()
            .contains("dozer.files")
    );

    // 回读。
    let text = std::fs::read_to_string(&path).unwrap();
    let loaded = Layout::load(&text, &catalog()).unwrap();
    assert_eq!(loaded.visible_panel_count(), 1);
}

#[test]
fn empty_and_single_rail_legacy_is_legal() {
    let adapter = FakeAdapter;
    for legacy in [
        serde_json::json!({ "left": [], "right": [] }),
        serde_json::json!({ "left": ["Usage"], "right": [] }),
    ] {
        let out = migrate_legacy_layout(&legacy, &adapter).unwrap();
        // 迁移不做目录过滤,空栏/单面板都合法。
        let n: usize = out.layout.rails.iter().map(|r| r.items.len()).sum();
        assert!(n <= 1);
    }
    // 宽度用产品默认值且被钳制在合法范围。
    let out =
        migrate_legacy_layout(&serde_json::json!({ "left": [], "right": [] }), &adapter).unwrap();
    assert!(
        out.layout
            .rails
            .iter()
            .all(|r| r.width == adapter.default_rail_width())
    );
}
