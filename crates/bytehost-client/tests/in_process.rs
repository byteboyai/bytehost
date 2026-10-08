//! 进程内实现必须通过与 UDS 实现共享的契约测试。

#![cfg(all(feature = "in-process", feature = "conformance"))]

use bytehost_apps::gateway::GatewayConfig;
use bytehost_apps::id::AppId;
use bytehost_apps::service::AppService;
use bytehost_client::conformance;
use bytehost_client::in_process::InProcess;

#[tokio::test]
async fn in_process_passes_the_conformance_suite() {
    let root = tempfile::tempdir().unwrap();
    let svc = AppService::start_with(root.path(), GatewayConfig { port: 0 }).await;

    // 应用源目录放在另一个临时目录(别塞进宿主根目录,免得起冲突)。
    let src_root = tempfile::tempdir().unwrap();
    let source = conformance::write_app(&src_root.path().join("conf"), "conf", "<h1>hi</h1>");
    let app_id = AppId::new("conf").unwrap();

    conformance::run(&InProcess::new(svc.clone()), source, app_id).await;

    svc.shutdown().await;
}

/// 不可用宿主:请求与订阅都必须以 `Host(Unavailable)` 失败——这正是 GUI 判断
/// "dozerd 不可用是持久状态而非一次性 Toast"所依赖的类别。
#[tokio::test]
async fn an_unavailable_host_reports_unavailable_not_transport() {
    use bytehost_apps::proto::AppErrorKind;
    use bytehost_client::AppApiError;
    use bytehost_client::AppHostApi;

    let host = InProcess::unavailable("gateway 端口被占");

    let err = host.app_list().await.unwrap_err();
    assert!(
        matches!(err, AppApiError::Host(ref f) if f.kind == AppErrorKind::Unavailable),
        "{err:?}"
    );

    let err = host.subscribe().await.unwrap_err();
    assert!(
        matches!(err, AppApiError::Host(ref f) if f.kind == AppErrorKind::Unavailable),
        "{err:?}"
    );
}
