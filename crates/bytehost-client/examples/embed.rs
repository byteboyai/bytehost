//! 把一个 bytehost 应用宿主**嵌入自己的进程**(没有守护进程)的最小演示。
//!
//! 运行:
//! `cargo run -p bytehost-client --example embed --features in-process -- <应用目录>`
//!
//! 打印安装、启动、以及首次导航要用的带令牌地址(`bh_token=…`,是秘密,别写日志)。
//! 真实宿主(如 Tauri 的 Digger)应把这个地址交给自己的 webview,并在导航时用
//! `bytehost_webview::AppOrigin::allows_navigation` 与 `AppIpc::parse` 做校验。

use std::sync::Arc;

use bytehost_apps::plan::{Approval, Provenance, TrustLevel};
use bytehost_apps::proto::AppSource;
use bytehost_apps::service::AppService;
use bytehost_apps::state::ObservedState;
use bytehost_client::AppHostApi;
use bytehost_client::in_process::InProcess;

#[tokio::main]
async fn main() {
    let app_dir = std::env::args().nth(1).expect("用法: embed <应用目录>");

    // 宿主自己的根目录(可放 registry / staging / 下载缓存 / 受管运行时)。
    let root = std::env::temp_dir().join("bytehost-embed-demo");
    let svc: Arc<AppService> = AppService::start(&root).await;
    let host = InProcess::new(svc.clone());

    let source = AppSource::LocalDir {
        path: app_dir.into(),
    };
    let plan = host
        .app_plan(source.clone(), Provenance::Local, TrustLevel::Trusted)
        .await
        .expect("出计划失败");
    let id = plan.app_id.clone();

    let approved = plan.approve(Approval {
        approver: "embed-demo".into(),
        approved_ms: 0,
    });
    host.app_install(approved, source).await.expect("安装失败");
    host.app_start(id.clone()).await.expect("启动失败");

    // 等进程真正 Running(过渡态是 Starting)。
    for _ in 0..300 {
        if let Some(a) = host
            .app_list()
            .await
            .expect("列应用失败")
            .into_iter()
            .find(|a| a.id == id)
            && matches!(a.observed, ObservedState::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let launch = host.app_launch_url(id.clone()).await.expect("取地址失败");
    println!("已启动 {id};首次导航地址:{launch}");

    // 示例到此为止:真实宿主在这里把 launch URL 交给 webview,并用 bytehost-webview 的
    // AppOrigin::allows_navigation / AppIpc::parse 做导航与 IPC 校验。这里跑完就收尾。
    svc.shutdown().await;
}
