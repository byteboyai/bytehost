//! T5 i18n 样例应用校验(无需运行时,`feature = "server"` 即可)。
//!
//! 覆盖:三个 `scripts/samples/i18n/*` 样例与复用桥的 `scripts/samples/node-notes`
//! 在 manifest 解析 + 资源装载 + 标题投影层面都合法,且 resolve_title 的精确匹配 /
//! default 回落语义正确。这是 B02/B05 的样例级证据(运行时热切换由 Dozer 侧 T6/T7 补)。
//!
//! 运行:`cargo test -p bytehost-apps --features server --test i18n_samples`

#![cfg(feature = "server")]

use std::path::Path;

use bytehost_apps::i18n::WebViewLocale;
use bytehost_apps::i18n::resources::{ResourceLimits, load_i18n_resources, project_display_i18n};
use bytehost_apps::id::Version;
use bytehost_apps::manifest::Manifest;
use bytehost_apps::proto::{AppDisplayI18n, DisplayTitleScope};

fn samples_dir() -> std::path::PathBuf {
    // crates/bytehost-apps -> 仓库根 -> scripts/samples
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts/samples")
}

/// 解析一个样例目录的 manifest,装载其 i18n 资源并投影成 `AppDisplayI18n`。
/// `expect_i18n=false` 时要求 manifest 根本不声明 `[i18n]`(legacy 样例)。
fn project(name: &str, expect_i18n: bool) -> (Manifest, Option<AppDisplayI18n>) {
    let dir = samples_dir().join(name);
    let toml = std::fs::read_to_string(dir.join("manifest.toml"))
        .unwrap_or_else(|e| panic!("读 {name}/manifest.toml 失败: {e}"));
    let m = Manifest::from_toml(&toml, &Version::new(0, 1, 0))
        .unwrap_or_else(|e| panic!("{name} manifest 解析失败: {e:?}"));

    if !expect_i18n {
        assert!(
            m.i18n.is_none(),
            "{name} 不应声明 [i18n](legacy 样例走旧路径)"
        );
        return (m, None);
    }

    let i18n = m
        .i18n
        .as_ref()
        .unwrap_or_else(|| panic!("{name} 应声明 [i18n]"));
    let bundle = load_i18n_resources(&dir, i18n, &ResourceLimits::default())
        .unwrap_or_else(|e| panic!("{name} i18n 资源装载失败: {e:?}"));
    // 资源校验:默认语言文件必须存在、key 在 [A-Za-z0-9_.-]、值非空(由 load_i18n_resources 保证)。
    let proj = project_display_i18n(&m, &bundle, &ResourceLimits::default())
        .unwrap_or_else(|e| panic!("{name} 标题投影失败: {e:?}"));
    (m, Some(proj))
}

fn presentation() -> DisplayTitleScope {
    DisplayTitleScope::Presentation
}

fn entrypoint(name: &str) -> DisplayTitleScope {
    DisplayTitleScope::Entrypoint {
        name: name.to_string(),
    }
}

#[test]
fn bilingual_live_is_live_and_resolves_all_locales() {
    let (_m, proj) = project("i18n/bilingual-live", true);
    let proj = proj.expect("应有投影");
    assert_eq!(
        proj.webview_locale,
        WebViewLocale::Live,
        "bilingual-live 应为 live"
    );

    // 精确匹配命中。
    assert_eq!(
        proj.resolve_title(&presentation(), "zh-CN"),
        Some("双语实时"),
        "zh-CN 应精确命中"
    );
    assert_eq!(
        proj.resolve_title(&entrypoint("main"), "zh-CN"),
        Some("打开演示"),
        "入口标题 zh-CN 应精确命中"
    );
    // 未知语言回落 default(en)。
    assert_eq!(
        proj.resolve_title(&presentation(), "fr"),
        Some("Bilingual Live"),
        "未知语言应回落 default(en) fallback"
    );
    assert_eq!(
        proj.resolve_title(&presentation(), "en"),
        Some("Bilingual Live")
    );
}

#[test]
fn partial_initial_is_initial_and_falls_back_for_missing_translation() {
    let (_m, proj) = project("i18n/partial-initial", true);
    let proj = proj.expect("应有投影");
    assert_eq!(
        proj.webview_locale,
        WebViewLocale::Initial,
        "partial-initial 应为 initial(首屏一次语言,不热更新)"
    );

    // zh-CN 只译了 app.title → 精确命中。
    assert_eq!(
        proj.resolve_title(&presentation(), "zh-CN"),
        Some("部分初始"),
        "zh-CN 的 app.title 应精确命中"
    );
    // en 完整(包含默认翻译)。
    assert_eq!(
        proj.resolve_title(&presentation(), "en"),
        Some("Partial Initial"),
        "en 应完整"
    );
}

#[test]
fn legacy_sample_has_no_i18n_section() {
    let (m, proj) = project("i18n/legacy-sample", false);
    assert!(m.i18n.is_none());
    assert!(proj.is_none(), "legacy 不应投影出 display_i18n");
}

#[test]
fn node_notes_reuses_the_bridge() {
    // 现有 Node HTTP 样例就地加 [i18n],证明桥与运行时无关(static_web / Node 共用同一策略)。
    let (_m, proj) = project("node-notes", true);
    let proj = proj.expect("node-notes 应声明 [i18n]");
    assert_eq!(
        proj.webview_locale,
        WebViewLocale::Live,
        "node-notes 复用 live 桥"
    );
    assert_eq!(
        proj.resolve_title(&presentation(), "zh-CN"),
        Some("Node 笔记"),
        "node-notes 标题 zh-CN 应精确命中"
    );
    assert_eq!(
        proj.resolve_title(&entrypoint("main"), "zh-CN"),
        Some("打开演示")
    );
    assert_eq!(
        proj.resolve_title(&presentation(), "fr"),
        Some("Node Notes"),
        "未知语言回落 default(en)"
    );
}
