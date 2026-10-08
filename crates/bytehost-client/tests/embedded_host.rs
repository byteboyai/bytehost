//! 嵌入验证:没有 dozerd,在自己的进程里直接用 `InProcess` 驱动一个 `AppService`。
//!
//! 这正是 Digger(以及任何没有守护进程的宿主)要用的形态。全程不依赖任何 dozer 代码,
//! 也不经 UDS:演示「装一个进程型应用、启动、经 gateway 带 Cookie 拿到 200、不带 Cookie 得 403、
//! `shutdown()` 后端口关闭」。
//!
//! 需要真 `python3`(进程型样例 `scripts/samples/py-notes`),默认 `#[ignore]`。
//! 手工运行:
//! `cargo test -p bytehost-client --all-features --test embedded_host -- --ignored --nocapture`

#![cfg(all(feature = "in-process", feature = "conformance"))]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytehost_apps::gateway::GatewayConfig;
use bytehost_apps::id::AppId;
use bytehost_apps::plan::{Approval, Provenance, TrustLevel};
use bytehost_apps::proto::AppSource;
use bytehost_apps::service::AppService;
use bytehost_apps::state::ObservedState;
use bytehost_client::AppHostApi;
use bytehost_client::in_process::InProcess;

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts/samples")
}

fn have(program: &str) -> bool {
    ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"]
        .iter()
        .any(|d| Path::new(d).join(program).is_file())
}

/// 从 `launch_url` 里取出 gateway 端口与一次性令牌。
fn parse_launch(launch_url: &str) -> (u16, String) {
    let rest = launch_url.strip_prefix("http://").unwrap();
    let (authority, path_and_query) = rest.split_once('/').unwrap();
    let port: u16 = authority.rsplit_once(':').unwrap().1.parse().unwrap();
    let token = path_and_query
        .split("bh_token=")
        .nth(1)
        .unwrap()
        .to_string();
    (port, token)
}

/// 经 gateway 发一个带/不带 Cookie 的 GET,返回 (状态码, 正文)。
fn http_get(host: &str, port: u16, path: &str, cookie: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(c) = cookie {
        head.push_str(&format!("Cookie: bh_session={c}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    s.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

// 阻塞 socket I/O 需要多线程运行时:gateway/代理任务和测试正文必须在不同 worker 上跑,
// 否则 current_thread 运行时会被 `read_to_end` 卡死(与 process_apps_live 一致)。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要真 python3;手工带 --ignored 运行"]
async fn embeds_the_host_in_process_without_a_daemon() {
    assert!(have("python3"), "本测试需要 python3");

    // 宿主根目录(自有 staging/registry/下载缓存/runtimes)。
    let host_root = tempfile::tempdir().unwrap();
    // 应用源目录放在另一个临时目录。
    let src_root = tempfile::tempdir().unwrap();
    let app_src = src_root.path().join("py-notes");
    copy_dir(&samples_dir().join("py-notes"), &app_src);

    // 一个没有守护进程、端口由系统分配的宿主。
    let svc = AppService::start_with(host_root.path(), GatewayConfig { port: 0 }).await;
    let host = InProcess::new(svc.clone());

    // 出计划 → 批准 → 安装 → 启动,全程走 AppHostApi(与 UDS 实现同一套类型化方法)。
    let source = AppSource::LocalDir { path: app_src };
    let plan = host
        .app_plan(source.clone(), Provenance::Local, TrustLevel::Trusted)
        .await
        .unwrap();
    let approved = plan.approve(Approval {
        approver: "embed-example".into(),
        approved_ms: 1,
    });
    host.app_install(approved, source).await.unwrap();

    let app = AppId::new("py-notes").unwrap();
    host.app_start(app.clone()).await.unwrap();

    // 等进程起来(过渡态是 Starting;拿到 launch URL 需要已 Running)。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(a) = host
            .app_list()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.id == app)
            && matches!(a.observed, ObservedState::Running)
        {
            break;
        }
        assert!(Instant::now() < deadline, "应用未在 30s 内 Running");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (port, token) = parse_launch(&host.app_launch_url(app.clone()).await.unwrap());
    let host_header = format!("py-notes.localhost:{port}");

    // 带 Cookie:200,正文含运行时版本注入。
    let (status, body) = http_get(&host_header, port, "/", Some(&token));
    assert_eq!(status, 200, "带 Cookie 应 200");
    assert!(body.contains("runtime:"), "正文应含运行时版本:{body}");

    // 不带 Cookie:403。
    let (status, _) = http_get(&host_header, port, "/", None);
    assert_eq!(status, 403, "不带 Cookie 应 403");

    // 收尾:shutdown 后端口关闭。
    svc.shutdown().await;
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "shutdown 后端口应关闭"
    );
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}
