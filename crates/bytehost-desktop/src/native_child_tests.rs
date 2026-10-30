use super::*;
fn desired() -> Desired {
    Desired {
        identity: "project-7".into(),
        url: "test://localhost/host.html?r=1".into(),
        rect: LogicalRect {
            x: 1.,
            y: 2.,
            width: 100.,
            height: 80.,
        },
        scale: 1.,
        visible: true,
        loaded: true,
    }
}
#[test]
fn create_always_applies_zoom_and_geometry() {
    assert_eq!(
        plan(None, Some(&desired())),
        vec![
            Action::Create,
            Action::Zoom,
            Action::SetBounds,
            Action::Show
        ]
    );
}
#[test]
fn hidden_destroys_and_unchanged_is_idle() {
    let d = desired();
    assert_eq!(plan(Some(&d), None), vec![Action::Destroy]);
    assert!(plan(Some(&d), Some(&d)).is_empty());
}
#[test]
fn project_change_replaces_the_single_slot() {
    let a = desired();
    let mut b = a.clone();
    b.identity = "project-8".into();
    assert_eq!(
        plan(Some(&a), Some(&b)),
        vec![
            Action::Destroy,
            Action::Create,
            Action::Zoom,
            Action::SetBounds,
            Action::Show
        ]
    );
}
#[test]
fn nonce_navigation_parks_until_loaded_then_shows() {
    let a = desired();
    let mut b = a.clone();
    b.url.push_str("&nonce=2");
    b.loaded = false;
    assert_eq!(
        plan(Some(&a), Some(&b)),
        vec![Action::Park, Action::Navigate]
    );
    let mut c = b.clone();
    c.loaded = true;
    assert_eq!(
        plan(Some(&b), Some(&c)),
        vec![Action::SetBounds, Action::Show]
    );
}
#[test]
fn geometry_and_scale_changes_reuse_the_view() {
    let a = desired();
    let mut b = a.clone();
    b.rect.width += 1.;
    b.scale = 1.25;
    assert_eq!(
        plan(Some(&a), Some(&b)),
        vec![Action::Zoom, Action::SetBounds]
    );
}
#[test]
fn navigation_requires_exact_unencoded_authority() {
    assert!(allows_navigation(
        "test",
        "localhost",
        "test://localhost/host.html?_r=123#x"
    ));
    for url in [
        "https://localhost/",
        "test://localhost.evil/",
        "test://localhost@evil/",
        "test://user@localhost/",
        "test://localhost:80/",
        "test://%6cocalhost/",
        "test://localhost%2fevil/",
        "test://localhost\\@evil/",
        "test://localhost",
        "test://LOCALHOST/",
        "test://localhost/\n",
    ] {
        assert!(!allows_navigation("test", "localhost", url), "{url}");
    }
}
#[test]
fn review_trace_authority_is_supported_without_subdomains_credentials_or_ports() {
    assert!(allows_navigation(
        "digger-review",
        "review-trace",
        "digger-review://review-trace/host.html?_r=7"
    ));
    for url in [
        "digger-review://review-trace.evil/",
        "digger-review://review-trace@evil/",
        "digger-review://user@review-trace/",
        "digger-review://review-trace:80/",
        "digger-review://review%2dtrace/",
        "digger-review://localhost/",
    ] {
        assert!(
            !allows_navigation("digger-review", "review-trace", url),
            "{url}"
        );
    }
}
#[test]
fn not_loaded_creation_parks_and_loaded_transition_restores_bounds() {
    let mut pending = desired();
    pending.loaded = false;
    assert_eq!(
        plan(None, Some(&pending)),
        vec![Action::Create, Action::Zoom, Action::Park]
    );
    let loaded = desired();
    assert_eq!(
        plan(Some(&pending), Some(&loaded)),
        vec![Action::SetBounds, Action::Show]
    );
}
#[test]
fn showing_a_hidden_loading_document_keeps_it_parked_but_resumes_native_visibility() {
    let mut hidden = desired();
    hidden.visible = false;
    hidden.loaded = false;
    let mut pending_visible = hidden.clone();
    pending_visible.visible = true;
    assert_eq!(
        plan(Some(&hidden), Some(&pending_visible)),
        vec![Action::Park],
        "Park must apply offscreen bounds and native visibility so document bootstrap can resume"
    );
}
#[test]
fn hiding_a_loading_document_hides_its_parked_native_view() {
    let mut pending_visible = desired();
    pending_visible.loaded = false;
    let mut hidden = pending_visible.clone();
    hidden.visible = false;
    assert_eq!(
        plan(Some(&pending_visible), Some(&hidden)),
        vec![Action::Park]
    );
}
#[test]
fn changing_url_never_shows_content_from_the_previous_loaded_document() {
    let current = desired();
    let mut next_document = current.clone();
    next_document.url.push_str("&nonce=next");
    assert_eq!(
        plan(Some(&current), Some(&next_document)),
        vec![Action::Park, Action::Navigate]
    );
}

/// 子 webview 的 builder 是创建受信原生子视图的唯一路径,没有廉价夹具能真的建一个;这些是它**必须**写出、
/// 且 wry 默认值会反过来的配置,用源码扫描钉住(wry 0.55 默认:下载放行、debug 下开发者工具开)。
/// 与 `app_webview.rs::build_app_child_pins_the_restrictive_settings` 同型。
#[test]
fn native_child_pins_the_restrictive_wry_settings() {
    let src = include_str!("native_child.rs");
    // 只看生产代码:测试模块自身含这些字面量(在断言里),扫进去会自我命中。
    let prod = &src[..src.find("#[cfg(test)]").unwrap()];
    // wry 默认值反转(devtools/下载/新窗口)统一在 wry_common::apply_restricted,
    // 由那里的参数化测试钉住;这里只钉可信原生子视图特有的导航策略。
    for required in [
        "apply_restricted(",
        ".with_navigation_handler(move |url| allows_navigation(scheme, host, &url))",
        "build_as_child(",
    ] {
        assert!(prod.contains(required), "缺少 {required}");
    }
    for forbidden in [
        ".with_devtools(true)",
        "with_custom_protocol",
        "with_download_completed_handler",
        "NewWindowResponse::Allow",
    ] {
        assert!(!prod.contains(forbidden), "不得出现 {forbidden}");
    }
    // 两个策略函数本身的值:下载恒拒绝,新窗口恒拒绝。
    assert!(!allows_download());
    assert!(matches!(
        new_window_response(),
        wry::NewWindowResponse::Deny
    ));
}
