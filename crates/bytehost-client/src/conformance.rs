//! 契约测试:同一份断言跑在**每一种** [`AppHostApi`] 实现上(UDS 实现 / 进程内实现),
//! 保证两者对同一组请求给出同样的答复与同样的失败类别。
//!
//! 用法:调用方造好一个**静态应用目录**(见 [`write_app`]),再
//! `bytehost_client::conformance::run(&host, source, app_id).await`。

use std::path::Path;

use bytehost_apps::id::AppId;
use bytehost_apps::plan::{Approval, Provenance, TrustLevel};
use bytehost_apps::proto::{AppErrorKind, AppSource};
use bytehost_apps::registry::UninstallMode;

use crate::{AppApiError, AppChange, AppHostApi};

/// 在 `dir` 下写一个可安装的静态应用源,返回它的 `AppSource`。列 `Local`/`Trusted` 装。
pub fn write_app(dir: &Path, id: &str, body: &str) -> AppSource {
    std::fs::create_dir_all(dir.join("web")).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        format!(
            r#"schema_version = 1
min_host_version = "0.1.0"
id = "{id}"
name = "{id} app"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("web/index.html"), body).unwrap();
    AppSource::LocalDir {
        path: dir.to_path_buf(),
    }
}

/// 走完一遍典型的静态应用生命周期,断言两种实现表现一致。
///
/// `source` 是调用方造的静态应用目录;`app_id` 是它的 id。
pub async fn run<A: AppHostApi>(api: &A, source: AppSource, app_id: AppId) {
    // ① 初始列表为空。
    assert!(
        api.app_list().await.expect("空列表应成功").is_empty(),
        "初始列表应为空"
    );

    // ② 出计划,id 对得上。
    let plan = api
        .app_plan(source.clone(), Provenance::Local, TrustLevel::Trusted)
        .await
        .expect("出计划应成功");
    assert_eq!(plan.app_id, app_id, "计划的应用 id 应一致");

    // ③ 安装后列表里有它,观察态是 `Stopped`。
    let approved = plan.approve(Approval {
        approver: "conformance".into(),
        approved_ms: 1,
    });
    api.app_install(approved, source.clone())
        .await
        .expect("安装应成功");
    let apps = api.app_list().await.expect("安装后列表应成功");
    assert_eq!(apps.len(), 1, "安装后应有一个应用: {apps:?}");
    assert_eq!(apps[0].id, app_id);
    assert_eq!(
        apps[0].observed,
        bytehost_apps::state::ObservedState::Installed,
        "全新安装后应为 Installed"
    );

    // ④ 启动:静态应用给一个 `http://<id>.localhost:` 开头的地址;随后列表里 `Running`。
    //    订阅先于停机建立(⑤ 用到),所以放在启动之前。
    let mut sub = api.subscribe().await.expect("订阅应成功");
    let url = api.app_start(app_id.clone()).await.expect("启动应成功");
    assert!(
        url.starts_with(&format!("http://{}.localhost:", app_id.as_str())),
        "静态应用启动地址前缀应正确: {url}"
    );
    let apps = api.app_list().await.expect("启动后列表应成功");
    assert_eq!(
        apps[0].observed,
        bytehost_apps::state::ObservedState::Running,
        "启动后应为 Running"
    );

    // ⑤ 停机后 5s 内收到该应用的 `Changed`(失效信号)。
    api.app_stop(app_id.clone()).await.expect("停止应成功");
    let change = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
        .await
        .expect("应在 5s 内收到变更推送")
        .expect("订阅流不应提前关闭");
    match change {
        AppChange::Changed(id) => assert_eq!(id, app_id, "推送的应是这个应用"),
        other => panic!("应收到 Changed({app_id}),得到 {other:?}"),
    }

    // ⑥ 未知 id 的启动错误带类别 `NotFound`,**不是** `Transport`。
    let missing = AppId::new("definitely-not-installed").unwrap();
    let err = api.app_start(missing.clone()).await.unwrap_err();
    assert_eq!(
        err.kind(),
        Some(AppErrorKind::NotFound),
        "启动不存在的应用应是 Host(NotFound),得到 {err:?}"
    );
    assert!(
        matches!(err, AppApiError::Host(_)),
        "不应是 Transport: {err:?}"
    );

    // ⑦ 卸载(连数据)后列表为空。
    api.app_uninstall(app_id.clone(), UninstallMode::ProgramAndData)
        .await
        .expect("卸载应成功");
    assert!(
        api.app_list().await.expect("卸载后列表应成功").is_empty(),
        "卸载后列表应为空"
    );

    // ⑧ 订阅接收端被丢弃后:服务端不残留。进程内实现里订阅转发任务应随之退出;
    //    UDS 实现的读任务也应在下一行读取处反映连接被关。这里只做"丢弃不 panic"的冒烟。
    drop(sub);
}
