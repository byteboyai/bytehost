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

/// 接收端被丢弃后,转发任务必须随之退出(不能等到下一个事件才发现),
/// 否则嵌入方反复订阅又丢弃会积累空闲任务和广播接收端。
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_subscription_stops_the_forwarding_task() {
    let root = tempfile::tempdir().unwrap();
    let svc = AppService::start_with(root.path(), GatewayConfig { port: 0 }).await;
    let host = InProcess::new(svc.clone());
    let metrics = tokio::runtime::Handle::current().metrics();

    let before = metrics.num_alive_tasks();
    let subs: Vec<_> = subscribe_n(&host, 5).await;
    assert!(
        metrics.num_alive_tasks() >= before + 5,
        "订阅应各起一个转发任务"
    );
    drop(subs);

    let mut alive = metrics.num_alive_tasks();
    for _ in 0..50 {
        if alive <= before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        alive = metrics.num_alive_tasks();
    }
    assert!(
        alive <= before,
        "丢弃订阅后转发任务应退出:之前 {before},现在 {alive}"
    );
    svc.shutdown().await;
}

async fn subscribe_n(
    host: &InProcess,
    n: usize,
) -> Vec<tokio::sync::mpsc::UnboundedReceiver<bytehost_client::AppChange>> {
    use bytehost_client::AppHostApi;
    let mut v = Vec::new();
    for _ in 0..n {
        v.push(host.subscribe().await.unwrap());
    }
    v
}
