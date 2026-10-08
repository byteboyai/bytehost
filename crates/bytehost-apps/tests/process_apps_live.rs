//! A6e Task 6:真实示例应用端到端验收(需要真 `python3` 与 `node`,默认 `#[ignore]`)。
//!
//! 用手工要求的命令运行:
//! `cargo test -p bytehost-apps --test process_apps_live -- --ignored --nocapture`
//!
//! 这里用真 `SystemResolver`、真 gateway、真监管线程,对 `scripts/bytehost/samples/`
//! 里的 `py-notes`(Python)与 `node-notes`(Node)各跑同一条流程:安装 → 启动 →
//! 同源/令牌检查 → SSE → WebSocket → 持久化计数 → 崩溃重启 → 卡死被健康检查重启 →
//! 停止/卸载 → 日志 → 版本要求不满足。
//!
//! 缺 `python3` / `node` 就 `panic!`(不静默跳过):本测试只在人工要求时运行,
//! 环境没准备齐就必须醒目地失败。

#![cfg(unix)]
#![cfg(feature = "server")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytehost_apps::gateway::{Gateway, GatewayConfig};
use bytehost_apps::id::AppId;
use bytehost_apps::manager::MonitorConfig;
use bytehost_apps::plan::{Approval, Provenance, TrustLevel};
use bytehost_apps::proto::{AppErrorKind, AppIssue, AppReply, AppRequest, AppSource, AppSummary};
use bytehost_apps::registry::UninstallMode;
use bytehost_apps::runtime::managed::fetch::FetchMeta;
use bytehost_apps::runtime::managed::{Fetcher, RuntimeManager};
use bytehost_apps::service::AppService;
use bytehost_apps::state::ObservedState;

fn id(s: &str) -> AppId {
    AppId::new(s).unwrap()
}

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts/bytehost/samples")
}

/// 一个示例应用:样例目录名(也是应用 id)与它的 manifest 文件名。
struct Sample {
    name: &'static str,
    launcher: &'static str,
}

const PY: Sample = Sample {
    name: "py-notes",
    launcher: "python3",
};
const NODE: Sample = Sample {
    name: "node-notes",
    launcher: "node",
};

fn have(program: &str) -> bool {
    ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"]
        .iter()
        .any(|d| Path::new(d).join(program).is_file())
}

/// 把样例目录拷成一份临时源码目录(测试可写副本),必要时改 manifest 里的一行。
fn stage(sample: &Sample, mutate_manifest: impl Fn(String) -> String) -> AppSource {
    let src = samples_dir().join(sample.name);
    let dst = std::env::temp_dir().join(format!(
        "{}-{}",
        sample.name,
        std::process::id() as u64 ^ nanos()
    ));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&src, &dst);
    let manifest = std::fs::read_to_string(dst.join("manifest.toml")).unwrap();
    std::fs::write(dst.join("manifest.toml"), mutate_manifest(manifest)).unwrap();
    AppSource::LocalDir { path: dst }
}

fn nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
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

async fn install(svc: &AppService, source: AppSource) {
    let reply = svc
        .handle(AppRequest::Plan {
            source: source.clone(),
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        })
        .await
        .unwrap();
    let AppReply::Plan { plan } = reply else {
        panic!("{reply:?}")
    };
    let approved = plan.approve(Approval {
        approver: "live-test".into(),
        approved_ms: 1,
    });
    let done = svc
        .handle(AppRequest::Install {
            approved: Box::new(approved),
            source,
        })
        .await
        .unwrap();
    assert_eq!(done, AppReply::Done);
}

async fn list_one(svc: &AppService, app: &AppId) -> Option<AppSummary> {
    let AppReply::Apps { apps } = svc.handle(AppRequest::List).await.unwrap() else {
        panic!("List 应回 Apps")
    };
    apps.into_iter().find(|a| &a.id == app)
}

async fn wait_satisfy<F: Fn(&AppSummary) -> bool>(
    svc: &AppService,
    app: &AppId,
    secs: u64,
    pred: F,
    what: &str,
) -> AppSummary {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(a) = list_one(svc, app).await
            && pred(&a)
        {
            return a;
        }
        if Instant::now() >= deadline {
            panic!("等待「{what}」超时({app})");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_running_async(svc: &AppService, app: &AppId, secs: u64) -> AppSummary {
    wait_satisfy(
        svc,
        app,
        secs,
        |a| matches!(a.observed, ObservedState::Running),
        "Running",
    )
    .await
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

async fn launch(svc: &AppService, app: &AppId) -> (u16, String) {
    let AppReply::LaunchUrl { url } = svc
        .handle(AppRequest::LaunchUrl { id: app.clone() })
        .await
        .unwrap()
    else {
        panic!("LaunchUrl 应回 LaunchUrl")
    };
    parse_launch(&url)
}

/// 一个经 gateway 的 HTTP 请求的简化描述。
struct Req<'a> {
    method: &'a str,
    path: &'a str,
    cookie: Option<&'a str>,
    origin: Option<&'a str>,
    body: Option<&'a str>,
    extra: Vec<(&'a str, &'a str)>,
}

impl<'a> Req<'a> {
    fn get(path: &'a str) -> Self {
        Self {
            method: "GET",
            path,
            cookie: None,
            origin: None,
            body: None,
            extra: Vec::new(),
        }
    }
    fn cookie(mut self, c: &'a str) -> Self {
        self.cookie = Some(c);
        self
    }
    fn origin(mut self, o: &'a str) -> Self {
        self.origin = Some(o);
        self
    }
    fn post(path: &'a str) -> Self {
        Self {
            method: "POST",
            path,
            cookie: None,
            origin: None,
            body: Some(""),
            extra: Vec::new(),
        }
    }
}

struct Resp {
    status: u16,
    body: String,
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn host_for(app: &AppId, port: u16) -> String {
    format!("{}.localhost:{}", app.as_str(), port)
}

/// 发一个请求,读完整个响应(Connection: close)。
fn request(app: &AppId, port: u16, req: &Req) -> Resp {
    let mut s = connect(port);
    let host = host_for(app, port);
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", req.method, req.path, host);
    if let Some(c) = req.cookie {
        head.push_str(&format!("Cookie: bh_session={c}\r\n"));
    }
    if let Some(o) = req.origin {
        head.push_str(&format!("Origin: {o}\r\n"));
    }
    for (k, v) in &req.extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = req.body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    if let Some(b) = req.body {
        head.push_str(b);
    }
    s.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Resp {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status_line = head.split("\r\n").next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Resp {
        status,
        body: body.to_string(),
    }
}

/// 读 SSE:在 `within` 内累积,直到收到至少 `want` 条 `data:` 行或超时。
fn read_sse(app: &AppId, port: u16, token: &str, want: usize, within: Duration) -> Vec<String> {
    let mut s = connect(port);
    let host = host_for(app, port);
    let req = format!(
        "GET /events HTTP/1.1\r\nHost: {host}\r\nCookie: bh_session={token}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = Instant::now() + within;
    let mut buf = String::new();
    let mut ticks = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < deadline && ticks.len() < want {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                for line in buf.lines() {
                    if let Some(rest) = line.strip_prefix("data: ")
                        && rest.starts_with("tick")
                        && !ticks.iter().any(|t| t == rest)
                    {
                        ticks.push(rest.to_string());
                    }
                }
            }
            Err(_) => {}
        }
    }
    ticks
}

/// 做一次 WebSocket 升级并回显:发 `payload`,读回应;`origin` 为 `None` 时不带 Origin。
fn ws_echo(
    app: &AppId,
    port: u16,
    token: &str,
    origin: Option<&str>,
) -> Result<u16, (u16, String)> {
    let mut s = connect(port);
    let host = host_for(app, port);
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let mut head = format!(
        "GET /ws HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    // 注意:应用自己的 origin 是 http://<id>.localhost:<port>。
    if let Some(o) = origin {
        head.push_str(&format!("Origin: {o}\r\n"));
    }
    head.push_str(&format!("Cookie: bh_session={token}\r\n\r\n"));
    s.write_all(head.as_bytes()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // 读响应头
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        let n = s.read(&mut tmp).map_err(|e| (0u16, e.to_string()))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            let head_str = String::from_utf8_lossy(&buf[..pos]).to_string();
            let status = head_str
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if status != 101 {
                return Err((status, head_str));
            }
            // 握手成功:发一帧文本(mask 位必须是客户端置的)
            let payload = b"hello";
            let frame = client_text_frame(payload);
            s.write_all(&frame).unwrap();
            // 读回显帧
            let after = &buf[pos + 4..];
            let echo = read_ws_reply(&mut s, after);
            assert_eq!(echo, payload.to_vec(), "WebSocket 应原样回显");
            return Ok(101);
        }
    }
    Err((0, String::from_utf8_lossy(&buf).to_string()))
}

fn client_text_frame(payload: &[u8]) -> Vec<u8> {
    let mask = [0x01u8, 0x02, 0x03, 0x04];
    let mut out = vec![0x81u8];
    let n = payload.len();
    if n < 126 {
        out.push(0x80 | n as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        out.push(b ^ mask[i % 4]);
    }
    out
}

fn read_ws_reply(s: &mut TcpStream, pre: &[u8]) -> Vec<u8> {
    let mut buf = pre.to_vec();
    let mut tmp = [0u8; 1024];
    loop {
        // 尝试解析一帧
        if buf.len() >= 2 {
            let b2 = buf[1];
            let mut len = (b2 & 0x7f) as usize;
            let mut off = 2;
            if len == 126 {
                if buf.len() >= 4 {
                    len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                    off = 4;
                } else {
                    // 继续读
                    let n = s.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                    continue;
                }
            }
            let masked = b2 & 0x80 != 0;
            if masked {
                if buf.len() >= off + 4 + len {
                    let mask = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
                    off += 4;
                    let mut out = buf[off..off + len].to_vec();
                    for (i, b) in out.iter_mut().enumerate() {
                        *b ^= mask[i % 4];
                    }
                    return out;
                }
            } else if buf.len() >= off + len {
                return buf[off..off + len].to_vec();
            }
        }
        let n = s.read(&mut tmp).unwrap();
        if n == 0 {
            return Vec::new();
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ===== 主流程:对两个示例各跑一遍 =====

async fn run_sample(sample: &Sample) {
    let t_start = Instant::now();
    let mut last = t_start;
    let mut mark = |name: &str| {
        let now = Instant::now();
        eprintln!(
            "[{}] {:<28} +{:.0}ms (total {:.0}ms)",
            sample.name,
            name,
            (now - last).as_secs_f64() * 1000.0,
            (now - t_start).as_secs_f64() * 1000.0
        );
        last = now;
    };
    assert!(
        have(sample.launcher),
        "缺少 {} —— 本用例要求真运行时,不静默跳过",
        sample.launcher
    );

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bytehost");
    // 监视参数调小:卡死进程在秒级被观察重启。
    let svc = AppService::start_with_monitor_for_test(
        &root,
        GatewayConfig { port: 0 },
        MonitorConfig {
            interval: Duration::from_millis(300),
            timeout: Duration::from_millis(400),
            failures: 3,
        },
    )
    .await;

    let app = id(sample.name);
    let source = stage(sample, |m| m);
    install(&svc, source).await;
    svc.handle(AppRequest::Start { id: app.clone() })
        .await
        .unwrap();
    wait_running_async(&svc, &app, 20).await;
    mark("1 install+start→Running");

    let (port, token) = launch(&svc, &app).await;

    // 2. 带会话 Cookie 请求 /:200 且正文含运行时版本;不带 Cookie → 403 且计数不变。
    let count_before = read_counter(&root, &app);
    let ok = request(&app, port, &Req::get("/").cookie(&token));
    assert_eq!(ok.status, 200, "{:?}", ok.body);
    assert!(
        ok.body.contains("runtime:") && ok.body.contains("hits: 0"),
        "正文应含运行时版本:{}",
        &ok.body[..ok.body.len().min(300)]
    );
    let no_cookie = request(&app, port, &Req::get("/"));
    assert_eq!(no_cookie.status, 403, "无令牌应 403");
    assert_eq!(
        read_counter(&root, &app),
        count_before,
        "无令牌请求不得触及应用"
    );
    mark("2 token gate + no-cookie 403");

    // 3. SSE:1.5s 内收到 ≥ 3 条 tick。
    let ticks = read_sse(&app, port, &token, 3, Duration::from_millis(1500));
    assert!(ticks.len() >= 3, "SSE 应流式收到 ≥3 条 tick,实得 {ticks:?}");
    mark("3 SSE ≥3 ticks");

    // 4. WebSocket:同源升级成功并回显;跨源被 403。
    let own_origin = format!("http://{}", host_for(&app, port));
    ws_echo(&app, port, &token, Some(&own_origin)).expect("同源 WS 升级应成功");
    let bad = ws_echo(&app, port, &token, Some("http://evil.example"));
    assert_eq!(bad.err().map(|(s, _)| s), Some(403), "跨源 WS 应 403");
    mark("4 WebSocket echo + cross-origin 403");

    // 5. POST /hit:同源 Origin → 计数 1;跨源 Origin → 403,计数不变。
    let hit = request(
        &app,
        port,
        &Req::post("/hit").cookie(&token).origin(&own_origin),
    );
    assert_eq!(hit.status, 200, "{:?}", hit.body);
    assert!(
        hit.body.contains("\"hits\":1") || hit.body.contains("\"hits\": 1"),
        "{}",
        hit.body
    );
    let evil = request(
        &app,
        port,
        &Req::post("/hit")
            .cookie(&token)
            .origin("http://evil.example"),
    );
    assert_eq!(evil.status, 403, "跨源写应 403");
    assert_eq!(read_counter(&root, &app), 1, "跨源写不得改计数");
    mark("5 POST /hit same-origin + count");

    // 6. GET /pid → /crash → 监管重启 → 新 pid。
    let old_pid = request(&app, port, &Req::get("/pid").cookie(&token)).body;
    assert!(is_pid(&old_pid), "崩溃前应拿到真实 pid:{old_pid:?}");
    let _ = request(&app, port, &Req::get("/crash").cookie(&token));
    let (port2, token2) = wait_new_pid_async(&svc, &app, &old_pid, 20).await;
    let new_pid = request(&app, port2, &Req::get("/pid").cookie(&token2)).body;
    assert!(is_pid(&new_pid), "崩溃后应拿到真实 pid:{new_pid:?}");
    assert_ne!(old_pid, new_pid, "崩溃后应换一个新进程");
    mark("6 crash → restart (new pid)");

    // 7. 计数仍是 1(跨进程重启持久化)。
    assert_eq!(read_counter(&root, &app), 1, "计数应跨重启保留");
    mark("7 counter persisted across restart");

    // 8. GET /hang → 健康检查杀掉并重启;观察 pid 变化(监控 300ms×3 + 余量 → ≤10s)。
    let (port3, token3) = launch(&svc, &app).await;
    let pid_before_hang = request(&app, port3, &Req::get("/pid").cookie(&token3)).body;
    assert!(
        is_pid(&pid_before_hang),
        "卡死前应拿到真实 pid:{pid_before_hang:?}"
    );
    let _ = request(&app, port3, &Req::get("/hang").cookie(&token3)); // 让它卡住(此请求会读到 EOF 或超时)
    let (p8, t8) = wait_new_pid_async(&svc, &app, &pid_before_hang, 12).await;
    let pid_after_hang = request(&app, p8, &Req::get("/pid").cookie(&t8)).body;
    assert!(is_pid(&pid_after_hang) && pid_after_hang != pid_before_hang);
    mark("8 hang → health-check restart");

    // 10. 日志:含启动时打印的 `listening on`。
    let AppReply::Logs { text, .. } = svc
        .handle(AppRequest::Logs {
            id: app.clone(),
            max_lines: 200,
        })
        .await
        .unwrap()
    else {
        panic!("期望 Logs 应答")
    };
    assert!(text.contains("listening on"), "日志应含启动行:{text:?}");
    mark("10 Logs contains startup line");

    // 9. Stop → LaunchUrl 不再给(没在运行);经 gateway 请求该应用站点得到 404(站点已注销);
    //    Uninstall(含数据)→ 目录消失。
    svc.handle(AppRequest::Stop { id: app.clone() })
        .await
        .unwrap();
    assert!(
        svc.handle(AppRequest::LaunchUrl { id: app.clone() })
            .await
            .is_err(),
        "停止后不应再给 launch url"
    );
    let stopped = request(&app, port, &Req::get("/").cookie(&token));
    assert_eq!(stopped.status, 404, "停止后站点应从 gateway 注销");
    let data_dir = root.join("apps").join(sample.name).join("data");
    assert!(data_dir.exists(), "数据目录应在");
    svc.handle(AppRequest::Uninstall {
        id: app.clone(),
        mode: UninstallMode::ProgramAndData,
    })
    .await
    .unwrap();
    assert!(
        !root.join("apps").join(sample.name).exists(),
        "含数据卸载后整个应用目录应消失"
    );
    mark("9 stop + uninstall(ProgramAndData)");

    // 11. 版本要求:装一个声明 *>=99* 的副本 → Start 失败(Unavailable),List 里 issue 是 RuntimeVersion。
    let bad_source = stage(sample, |m| {
        if sample.launcher == "python3" {
            m.replace("python = \">=3.9\"", "python = \">=99\"")
        } else {
            m.replace("node = \">=18\"", "node = \">=99\"")
        }
    });
    let bad_app = id(&format!("{}-bad", sample.name));
    let bad_source = rename_manifest_id(bad_source, bad_app.as_str());
    install(&svc, bad_source).await;
    let err = svc
        .handle(AppRequest::Start {
            id: bad_app.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, AppErrorKind::Unavailable, "{err:?}");
    let summary = wait_satisfy(
        &svc,
        &bad_app,
        10,
        |a| matches!(a.observed, ObservedState::Failed { .. }) && a.issue.is_some(),
        "Failed+issue",
    )
    .await;
    assert!(
        matches!(summary.issue, Some(AppIssue::RuntimeVersion { .. })),
        "{:?}",
        summary.issue
    );
    mark("11 version requirement unsat → issue");

    svc.shutdown().await;
}

/// 改名:改 manifest 的 `id`(样例里 id 与目录名一致)。
fn rename_manifest_id(source: AppSource, new_id: &str) -> AppSource {
    let AppSource::LocalDir { path } = source else {
        panic!("本用例只处理本机目录来源")
    };
    let manifest = std::fs::read_to_string(path.join("manifest.toml")).unwrap();
    let updated = manifest
        .lines()
        .map(|l| {
            if l.starts_with("id = ") {
                format!("id = \"{new_id}\"")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path.join("manifest.toml"), updated).unwrap();
    AppSource::LocalDir { path }
}

fn read_counter(root: &Path, app: &AppId) -> u64 {
    let p = root
        .join("apps")
        .join(app.as_str())
        .join("data")
        .join("counter.txt");
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// `/pid` 的正文是不是一个真实的进程号。
fn is_pid(body: &str) -> bool {
    !body.is_empty() && body.chars().all(|c| c.is_ascii_digit())
}

async fn wait_new_pid_async(
    svc: &AppService,
    app: &AppId,
    old_pid: &str,
    secs: u64,
) -> (u16, String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let running = matches!(
            list_one(svc, app).await.map(|a| a.observed),
            Some(ObservedState::Running)
        );
        if running
            && let Ok(AppReply::LaunchUrl { url }) =
                svc.handle(AppRequest::LaunchUrl { id: app.clone() }).await
        {
            let (port, token) = parse_launch(&url);
            let pid = request(app, port, &Req::get("/pid").cookie(&token)).body;
            // 必须是真 pid(纯数字):应用刚崩溃时,网关会对已死的上游返回
            // "application is not reachable" 之类的错误页,那不是新进程。
            if is_pid(&pid) && pid != old_pid {
                return (port, token);
            }
        }
        if Instant::now() >= deadline {
            panic!("等待新 pid 超时(旧 {old_pid})");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要真 node/npm;人工运行时删除 --ignored"]
async fn a_failing_dependency_install_surfaces_a_dependency_issue() {
    // 目标:声明 `package-lock.json` 且 lock 里引用了不存在的包 → `npm ci` 必然失败。
    // 断言 `Start` 之后 `list` 出现 `Failed{retryable:true}` + `DependencyInstall`,
    // 且 `Logs` 里能看到安装器的错误输出(证明输出进了 app.log,没被吞掉)。
    assert!(
        have("node") && have("npm"),
        "缺少 node/npm —— 本用例要求真运行时,不静默跳过"
    );

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bytehost");
    let svc = AppService::start_with_monitor_for_test(
        &root,
        GatewayConfig { port: 0 },
        MonitorConfig {
            interval: Duration::from_millis(300),
            timeout: Duration::from_millis(400),
            failures: 3,
        },
    )
    .await;

    // 在 manifest 的运行时块里声明 lockfile(样例本身没声明),并写入一份引用了
    // 不存在依赖的 lock 文件:`npm ci` 起不来。
    let source = stage(&NODE, |m| {
        m.replace(
            "command = [\"node\", \"server.js\"]",
            "command = [\"node\", \"server.js\"]\nlockfile = \"package-lock.json\"",
        )
    });
    let AppSource::LocalDir { path } = &source else {
        panic!("本用例只处理本机目录来源")
    };
    std::fs::write(
        path.join("package-lock.json"),
        r#"{
  "name": "node-notes",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "node-notes",
      "version": "1.0.0",
      "dependencies": { "this-package-does-not-exist-anywhere-xyz": "1.0.0" }
    },
    "node_modules/this-package-does-not-exist-anywhere-xyz": {
      "version": "1.0.0",
      "resolved": "https://registry.npmjs.org/this-package-does-not-exist-anywhere-xyz/-/this-package-does-not-exist-anywhere-xyz-1.0.0.tgz",
      "integrity": "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
    }
  }
}
"#,
    )
    .unwrap();

    let app = id("node-notes");
    install(&svc, source).await;
    svc.handle(AppRequest::Start { id: app.clone() })
        .await
        .unwrap();

    let summary = wait_satisfy(
        &svc,
        &app,
        60,
        |a| matches!(a.observed, ObservedState::Failed { .. }) && a.issue.is_some(),
        "依赖安装失败 → Failed+issue,开始等待",
    )
    .await;
    assert!(
        matches!(
            &summary.observed,
            ObservedState::Failed {
                retryable: true,
                ..
            }
        ),
        "{:?}",
        summary.observed
    );
    let Some(AppIssue::DependencyInstall { summary: why }) = &summary.issue else {
        panic!("应为 DependencyInstall,得到 {:?}", summary.issue);
    };
    assert!(!why.is_empty(), "summary 不能为空");

    let AppReply::Logs { text, .. } = svc
        .handle(AppRequest::Logs {
            id: app.clone(),
            max_lines: 200,
        })
        .await
        .unwrap()
    else {
        panic!("Logs 应回 Logs")
    };
    assert!(
        text.to_lowercase().contains("npm")
            || text.contains("this-package-does-not-exist-anywhere-xyz")
            || text.to_lowercase().contains("err"),
        "安装输出应在应用日志里,得到:\n{text}"
    );
    // 输出里的转义序列必须已被清洗(应用日志经 logs.rs 逐行清洗)。
    assert!(
        !text.contains('\u{1b}'),
        "应用日志不应含 ANSI 转义:{text:?}"
    );

    svc.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要真 python3 与 node;人工运行时删除 --ignored"]
async fn python_sample_end_to_end() {
    run_sample(&PY).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要真 python3 与 node;人工运行时删除 --ignored"]
async fn node_sample_end_to_end() {
    run_sample(&NODE).await;
}

// ===== A6g Task 5:升级 → 自动回滚 → 手动回滚(真 python3) =====

/// 把 manifest 的 `version = "..."` 改成给定值,并往 HTML 里塞一个可读的版本标记。
/// 页面标记让"到底跑的是哪一版"可被外部观察(而不是只看记录)。
fn stage_py_version(version: &str) -> AppSource {
    let marker = format!("<p id=\"ver\">APPVERSION:{version}</p>");
    let source = stage(&PY, {
        let version = version.to_string();
        move |manifest| {
            manifest
                .lines()
                .map(|l| {
                    if l.starts_with("version = ") {
                        format!("version = \"{version}\"")
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    // `stage` 只改 manifest;这里再改页面,把版本写进 HTML。
    let AppSource::LocalDir { path } = &source else {
        panic!("本用例只处理本机目录来源")
    };
    let server = path.join("server.py");
    let text = std::fs::read_to_string(&server).unwrap();
    let updated = text.replace("<h1>Py Notes</h1>", &format!("<h1>Py Notes</h1>\n{marker}"));
    std::fs::write(&server, updated).unwrap();
    source
}

/// 经 gateway 读页面正文(带令牌)。
fn get_page(app: &AppId, port: u16, token: &str) -> String {
    let resp = request(app, port, &Req::get("/").cookie(token));
    assert_eq!(resp.status, 200, "读页面应 200:{}", resp.body);
    resp.body
}

/// 页面上是否显示指定版本。
fn page_shows_version(app: &AppId, port: u16, token: &str, version: &str) -> bool {
    get_page(app, port, token).contains(&format!("APPVERSION:{version}"))
}

/// 等待页面显示指定版本(重启后 gateway 会短暂不可达)。
async fn wait_page_version(
    svc: &AppService,
    app: &AppId,
    version: &str,
    secs: u64,
) -> (u16, String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if matches!(
            list_one(svc, app).await.map(|a| a.observed),
            Some(ObservedState::Running)
        ) && let Ok(AppReply::LaunchUrl { url }) =
            svc.handle(AppRequest::LaunchUrl { id: app.clone() }).await
        {
            let (port, token) = parse_launch(&url);
            if page_shows_version(app, port, &token, version) {
                return (port, token);
            }
        }
        if Instant::now() >= deadline {
            panic!("等待页面显示 {version} 超时({app})");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 磁盘上 `apps/<id>/package/` 下的版本子目录名(排序后)。
fn package_versions_on_disk(root: &Path, app: &AppId) -> Vec<String> {
    let dir = root.join("apps").join(app.as_str()).join("package");
    let mut out: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要真 python3;人工运行时删除 --ignored"]
async fn python_sample_upgrade_and_rollback() {
    assert!(
        have("python3"),
        "缺少 python3 —— 本用例要求真运行时,不静默跳过"
    );

    let t_start = Instant::now();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bytehost");
    let svc = AppService::start_with_monitor_for_test(
        &root,
        GatewayConfig { port: 0 },
        MonitorConfig {
            interval: Duration::from_millis(300),
            timeout: Duration::from_millis(400),
            failures: 3,
        },
    )
    .await;

    let app = id("py-notes");

    // 1. 装 1.0.0 → 启动 → 页面显示 1.0.0;经 gateway 计一次数。
    install(&svc, stage_py_version("1.0.0")).await;
    svc.handle(AppRequest::Start { id: app.clone() })
        .await
        .unwrap();
    let (port, token) = wait_page_version(&svc, &app, "1.0.0", 20).await;
    let own_origin = format!("http://{}", host_for(&app, port));
    let hit = request(
        &app,
        port,
        &Req::post("/hit").cookie(&token).origin(&own_origin),
    );
    assert_eq!(hit.status, 200, "{:?}", hit.body);
    assert_eq!(read_counter(&root, &app), 1, "计数应为 1");
    let pid_v1 = request(&app, port, &Req::get("/pid").cookie(&token)).body;
    assert!(is_pid(&pid_v1), "1.0.0 应有真 pid:{pid_v1:?}");

    // 2. 升级到 1.1.0(运行中):自动停→换→再启;页面显示 1.1.0、新 pid、计数仍为 1。
    install(&svc, stage_py_version("1.1.0")).await;
    let (port2, token2) = wait_page_version(&svc, &app, "1.1.0", 20).await;
    let pid_v2 = request(&app, port2, &Req::get("/pid").cookie(&token2)).body;
    assert!(is_pid(&pid_v2), "1.1.0 应有真 pid:{pid_v2:?}");
    assert_ne!(pid_v1, pid_v2, "升级应换一个新进程");
    assert_eq!(read_counter(&root, &app), 1, "计数应跨升级保留");
    let after_upgrade = list_one(&svc, &app).await.expect("应列出 py-notes");
    assert_eq!(
        after_upgrade.previous_version,
        Some(bytehost_apps::id::Version::new(1, 0, 0)),
        "升级后应记下上一版"
    );

    // 3. 手动回滚 → 1.0.0(消耗"上一版");页面显示 1.0.0、计数仍 1;再回滚 → NotFound。
    svc.handle(AppRequest::Rollback { id: app.clone() })
        .await
        .unwrap();
    let (port3, token3) = wait_page_version(&svc, &app, "1.0.0", 20).await;
    assert_eq!(read_counter(&root, &app), 1, "计数应跨手动回滚保留");
    let after_manual = wait_satisfy(
        &svc,
        &app,
        10,
        |a| a.previous_version.is_none() && matches!(a.observed, ObservedState::Running),
        "手动回滚后 previous_version 清空",
    )
    .await;
    let manual_note = after_manual.rollback_note.clone().unwrap();
    assert!(!manual_note.automatic, "应为手动回滚:{manual_note:?}");
    assert_eq!(
        manual_note.from,
        bytehost_apps::id::Version::new(1, 1, 0),
        "{manual_note:?}"
    );
    assert_eq!(
        manual_note.to,
        bytehost_apps::id::Version::new(1, 0, 0),
        "{manual_note:?}"
    );
    let again = svc
        .handle(AppRequest::Rollback { id: app.clone() })
        .await
        .unwrap_err();
    assert_eq!(again.kind, AppErrorKind::NotFound, "{again:?}");

    // 4. 升级到起不来的 1.2.0 → 数秒内自动回滚回 1.0.0,页面显示 1.0.0、rollback_note.automatic == true。
    let broken = stage_py_version("1.2.0");
    // 让 1.2.0 必起不来:把服务换成启动即退出的脚本(监管会连续失败 → 试用期内回滚)。
    {
        let AppSource::LocalDir { path } = &broken else {
            panic!("本用例只处理本机目录来源")
        };
        std::fs::write(
            path.join("server.py"),
            "import sys\nsys.stderr.write('boom: cannot start\\n')\nsys.exit(1)\n",
        )
        .unwrap();
    }
    install(&svc, broken).await;
    let settled = wait_satisfy(
        &svc,
        &app,
        30,
        |a| {
            a.rollback_note
                .as_ref()
                .is_some_and(|n| n.automatic && n.from == bytehost_apps::id::Version::new(1, 2, 0))
                && a.previous_version.is_none()
                && matches!(a.observed, ObservedState::Running)
        },
        "自动回滚回 1.0.0",
    )
    .await;
    let note = settled.rollback_note.clone().unwrap();
    assert!(note.automatic, "应为自动回滚:{note:?}");
    assert_eq!(
        note.from,
        bytehost_apps::id::Version::new(1, 2, 0),
        "{note:?}"
    );
    assert_eq!(
        note.to,
        bytehost_apps::id::Version::new(1, 0, 0),
        "{note:?}"
    );
    let (port4, token4) = wait_page_version(&svc, &app, "1.0.0", 20).await;
    assert_eq!(read_counter(&root, &app), 1, "计数应跨自动回滚保留");
    // 自动回滚同样消耗"上一版":此刻没有可再回滚的目标。
    let after_auto = svc
        .handle(AppRequest::Rollback { id: app.clone() })
        .await
        .unwrap_err();
    assert_eq!(after_auto.kind, AppErrorKind::NotFound, "{after_auto:?}");

    // 5. 磁盘:只剩当前版本 1.0.0——1.1.0 在装 1.2.0 时被"当前+上一版"清理,1.2.0 在自动回滚时清掉。
    let versions = package_versions_on_disk(&root, &app);
    assert_eq!(
        versions,
        vec!["1.0.0".to_string()],
        "包目录应只剩当前版本:{versions:?}"
    );
    assert!(page_shows_version(&app, port4, &token4, "1.0.0"));

    eprintln!(
        "[py-notes upgrade/rollback] 全流程耗时 {:.0}ms",
        t_start.elapsed().as_secs_f64() * 1000.0
    );
    let _ = (port2, token2, port3, token3);
    svc.shutdown().await;
}

// ===== A6h Task 6:压缩包来源与 URL 来源的端到端验收(真运行时,默认 `#[ignore]`) =====
//
// 覆盖:
//   1. 把样例**现场**打成 zip 与 tar.gz(临时目录,不提交二进制),经 `Plan(Archive)` → `Install`
//      → `Start`,验证 A6e 里与来源无关的核心几步(令牌门、SSE、计数跨重启、崩溃重启)同样成立;
//   2. 恶意压缩包(zip-slip、符号链接)经 `AppService` 被拒,且**数据根目录之外没有新文件**
//      (对测试根的父目录前后快照比较);
//   3. `Url` 来源:用假 `Fetcher` 注入静态应用 zip,走完 Plan→Install→Start→经 gateway 取到页面;
//      同一假 fetcher 供给进程型(Node)应用 → 出计划即被拒。

/// 把一个源码目录打成 `.tar.gz`(用系统 `tar`;生产代码绝不调它,这里只是造测试输入)。
///
/// 打成**单一顶层目录**形态(`<name>/...`):这是 GitHub 风格、也是解压器明确支持的形态。
/// (`tar -C src .` 会产生 `./` 与 `./file` 条目,解压器按设计拒绝含 `.` 段的路径——测试因此
/// 不用那种形态,详见报告"已知局限"。)
///
/// `--no-mac-metadata` 关掉 macOS bsdtar 的 AppleDouble(`._*`)伴随文件:它们会在归档根部
/// 多出条目、破坏"唯一顶层目录"的判定。真实用户若用会带 `._` 的工具打包,宁可先在解压层
/// 拒绝(见报告"已知局限"),测试这里打成干净形态以聚焦来源流程。
fn pack_targz(src: &Path, out: &Path) {
    let name = src.file_name().unwrap().to_string_lossy().into_owned();
    let parent = src.parent().unwrap();
    let status = std::process::Command::new("/usr/bin/tar")
        .args(["--no-mac-metadata", "-czf"])
        .arg(out)
        .arg("-C")
        .arg(parent)
        .arg(&name)
        .status()
        .unwrap();
    assert!(status.success(), "打 tar.gz 失败:{out:?}");
}

/// 把一个源码目录打成 `.zip`(用系统 `zip`;同上,单一顶层目录形态)。
fn pack_zip(src: &Path, out: &Path) {
    let name = src.file_name().unwrap().to_string_lossy().into_owned();
    let parent = src.parent().unwrap();
    let status = std::process::Command::new("/usr/bin/zip")
        .env("COPYFILE_DISABLE", "1")
        .args(["-r", "-q", "-X", "--symlinks"])
        .arg(out)
        .arg(&name)
        .current_dir(parent)
        .status()
        .unwrap();
    assert!(status.success(), "打 zip 失败:{out:?}");
}

/// 把样例拷成可写副本并(可选)改 manifest;与 `stage` 相同,但结果仍是目录(供打包)。
fn copy_sample(sample: &Sample, mutate_manifest: impl Fn(String) -> String) -> PathBuf {
    let src = samples_dir().join(sample.name);
    let dst = std::env::temp_dir().join(format!(
        "pack-{}-{}",
        sample.name,
        std::process::id() as u64 ^ nanos()
    ));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&src, &dst);
    let manifest = std::fs::read_to_string(dst.join("manifest.toml")).unwrap();
    std::fs::write(dst.join("manifest.toml"), mutate_manifest(manifest)).unwrap();
    dst
}

/// 递归收集一棵树的 `相对路径 -> 类型`;用于比较"根目录之外有没有新文件"。
/// 不引入 `walkdir`(计划里的手段提示),用一个小小的本地递归即可。
fn snapshot_tree(root: &Path) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeSet<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                out.insert(format!("d {rel}"));
                walk(base, &path, out);
            } else if ft.is_symlink() {
                out.insert(format!("l {rel}"));
            } else {
                out.insert(format!("f {rel}"));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

/// 假来源下载器:把给定字节"下载"到目标;`effective_url` 可配(默认与请求同 URL)。
struct TestFetcher {
    bytes: Vec<u8>,
    effective_url: Option<String>,
    calls: AtomicUsize,
}

impl TestFetcher {
    fn new(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes,
            effective_url: None,
            calls: AtomicUsize::new(0),
        })
    }
}

impl Fetcher for TestFetcher {
    fn fetch(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> std::io::Result<()> {
        self.fetch_meta(url, dest, 0, on_progress, cancel)
            .map(|_| ())
    }

    fn fetch_meta(
        &self,
        url: &str,
        dest: &Path,
        _max_bytes: u64,
        _on_progress: &mut dyn FnMut(u64, Option<u64>),
        _cancel: &AtomicBool,
    ) -> std::io::Result<FetchMeta> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::fs::write(dest, &self.bytes)?;
        Ok(FetchMeta {
            effective_url: self
                .effective_url
                .clone()
                .unwrap_or_else(|| url.to_string()),
            bytes: self.bytes.len() as u64,
        })
    }
}

/// 起一个只跑 AppManager、注入假来源下载器的服务(沿用 `finish_start_with_sources` 这个跨 crate 测试钩子)。
async fn start_with_fetcher(root: &Path, fetcher: Arc<dyn Fetcher>) -> Arc<AppService> {
    let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
    let rm = Arc::new(RuntimeManager::new(root.join("runtimes")));
    AppService::finish_start_with_sources(root, gateway, rm, None, Some(fetcher)).await
}

/// 用给定来源安装一个应用(受理与安装都与是否压缩包/URL 无关)。
async fn install_source(svc: &AppService, source: AppSource) {
    let reply = svc
        .handle(AppRequest::Plan {
            source: source.clone(),
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        })
        .await
        .unwrap();
    let AppReply::Plan { plan } = reply else {
        panic!("{reply:?}")
    };
    let approved = plan.approve(Approval {
        approver: "live-test".into(),
        approved_ms: 1,
    });
    let done = svc
        .handle(AppRequest::Install {
            approved: Box::new(approved),
            source,
        })
        .await
        .unwrap();
    assert_eq!(done, AppReply::Done);
}

/// 压缩包来源(zip 与 tar.gz 各一次)安装真 python 样例,验证 A6e 核心几步仍成立。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要真 python3;人工运行时删除 --ignored"]
async fn archive_sourced_python_app_end_to_end() {
    assert!(
        have("python3"),
        "缺少 python3 —— 本用例要求真运行时,不静默跳过"
    );

    for (label, pack) in [
        ("tar.gz", pack_targz as fn(&Path, &Path)),
        ("zip", pack_zip as fn(&Path, &Path)),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let svc = AppService::start_with_monitor_for_test(
            &root,
            GatewayConfig { port: 0 },
            MonitorConfig {
                interval: Duration::from_millis(300),
                timeout: Duration::from_millis(400),
                failures: 3,
            },
        )
        .await;

        // 现场打包(临时目录,不进仓库)。
        let src = copy_sample(&PY, |m| m);
        let archive = tmp.path().join(format!("py-notes.{label}"));
        pack(&src, &archive);

        let app = id("py-notes");
        install_source(
            &svc,
            AppSource::Archive {
                path: archive.clone(),
            },
        )
        .await;
        svc.handle(AppRequest::Start { id: app.clone() })
            .await
            .unwrap();
        wait_running_async(&svc, &app, 20).await;

        // 令牌门:带 Cookie 200、无 Cookie 403 且计数不变。
        let (port, token) = launch(&svc, &app).await;
        let count_before = read_counter(&root, &app);
        let ok = request(&app, port, &Req::get("/").cookie(&token));
        assert_eq!(ok.status, 200, "[{label}] {:?}", ok.body);
        let no_cookie = request(&app, port, &Req::get("/"));
        assert_eq!(no_cookie.status, 403, "[{label}] 无令牌应 403");
        assert_eq!(read_counter(&root, &app), count_before, "[{label}]");

        // SSE:1.5s 内收到 ≥ 3 条 tick。
        let ticks = read_sse(&app, port, &token, 3, Duration::from_millis(1500));
        assert!(
            ticks.len() >= 3,
            "[{label}] SSE 应流式收到 ≥3 条 tick,实得 {ticks:?}"
        );

        // 计数持久化:同源 POST /hit → 1;崩溃重启后仍是 1。
        let own_origin = format!("http://{}", host_for(&app, port));
        let hit = request(
            &app,
            port,
            &Req::post("/hit").cookie(&token).origin(&own_origin),
        );
        assert_eq!(hit.status, 200, "[{label}] {:?}", hit.body);
        assert_eq!(read_counter(&root, &app), 1, "[{label}] 计数应为 1");
        let old_pid = request(&app, port, &Req::get("/pid").cookie(&token)).body;
        assert!(
            is_pid(&old_pid),
            "[{label}] 崩溃前应拿到真实 pid:{old_pid:?}"
        );
        let _ = request(&app, port, &Req::get("/crash").cookie(&token));
        let (port2, token2) = wait_new_pid_async(&svc, &app, &old_pid, 20).await;
        let new_pid = request(&app, port2, &Req::get("/pid").cookie(&token2)).body;
        assert!(
            is_pid(&new_pid) && new_pid != old_pid,
            "[{label}] 崩溃后应换一个新进程:{new_pid:?}"
        );
        assert_eq!(read_counter(&root, &app), 1, "[{label}] 计数应跨重启保留");

        svc.handle(AppRequest::Uninstall {
            id: app.clone(),
            mode: UninstallMode::ProgramAndData,
        })
        .await
        .unwrap();
        svc.shutdown().await;
    }
}

/// 恶意压缩包被拒,且数据根目录之外没有新文件。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要 /usr/bin/tar;人工运行时删除 --ignored"]
async fn hostile_archives_are_refused_with_no_files_outside_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let root = parent.join("bytehost");
    let svc = AppService::start_with(&root, GatewayConfig { port: 0 }).await;

    // 快照"根的父目录":任何越界的写入都会出现在这里。
    let before = snapshot_tree(&parent);

    // a) zip-slip:`../evil.txt`。
    let stage = tmp.path().join("slip-stage");
    std::fs::create_dir_all(stage.join("child")).unwrap();
    std::fs::write(stage.join("evil.txt"), b"x").unwrap();
    let slip = tmp.path().join("slip.tar.gz");
    let status = std::process::Command::new("/usr/bin/tar")
        .args(["-czf"])
        .arg(&slip)
        .arg("-C")
        .arg(stage.join("child"))
        .arg("../evil.txt")
        .status()
        .unwrap();
    assert!(status.success());
    // 先证明这个包确实含越界条目(否则测试会假通过)。
    let listing = std::process::Command::new("/usr/bin/tar")
        .args(["-tf"])
        .arg(&slip)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listing.stdout).contains(".."),
        "造的包应含 `..` 条目:{:?}",
        String::from_utf8_lossy(&listing.stdout)
    );
    let err = svc
        .handle(AppRequest::Plan {
            source: AppSource::Archive { path: slip },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, AppErrorKind::Rejected, "zip-slip 应被拒:{err:?}");

    // b) 符号链接。
    let link_stage = tmp.path().join("link-stage");
    std::fs::create_dir_all(&link_stage).unwrap();
    std::fs::write(
        link_stage.join("manifest.toml"),
        "schema_version = 1\nmin_host_version = \"0.1.0\"\nid = \"evil\"\nname = \"Evil\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", link_stage.join("link")).unwrap();
    let link = tmp.path().join("link.tar.gz");
    pack_targz(&link_stage, &link);
    let err = svc
        .handle(AppRequest::Plan {
            source: AppSource::Archive { path: link },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, AppErrorKind::Rejected, "符号链接应被拒:{err:?}");

    // 两次拒绝后,根的父目录应与快照完全一致(无 `evil.txt`、无 `/etc/passwd` 落盘、无残留)。
    let after = snapshot_tree(&parent);
    assert_eq!(before, after, "数据根目录之外不得出现任何新文件/目录");
    assert!(
        !parent.join("evil.txt").exists(),
        "zip-slip 不得在根外写文件"
    );

    svc.shutdown().await;
}

/// URL 来源:假 Fetcher 供静态应用 zip → Plan/Install/Start/取页面;
/// 同一假 fetcher 供进程型(Node)应用 → 出计划即被拒(网络来源只允许静态应用)。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要真 node;人工运行时删除 --ignored"]
async fn url_sourced_static_app_runs_and_a_process_app_is_rejected() {
    // 1) 静态应用 zip → 能装能跑能取页面。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bytehost");
    let src = tmp.path().join("static-src");
    std::fs::create_dir_all(src.join("web")).unwrap();
    std::fs::write(
        src.join("manifest.toml"),
        r#"schema_version = 1
min_host_version = "0.1.0"
id = "net-static"
name = "Net Static"
version = "1.0.0"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"
title = "Net Static"

[runtime]
kind = "static_web"
source = "web/"
"#,
    )
    .unwrap();
    std::fs::write(src.join("web/index.html"), "<h1>hello-from-url</h1>").unwrap();
    let zip = tmp.path().join("static.zip");
    pack_zip(&src, &zip);
    let bytes = std::fs::read(&zip).unwrap();
    let fetcher = TestFetcher::new(bytes);
    let svc = start_with_fetcher(&root, fetcher.clone()).await;

    let app = id("net-static");
    install_source(
        &svc,
        AppSource::Url {
            url: "https://example.com/static.zip".into(),
            sha256: None,
        },
    )
    .await;
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "出计划下载一次");
    svc.handle(AppRequest::Start { id: app.clone() })
        .await
        .unwrap();
    wait_running_async(&svc, &app, 20).await;
    let (port, token) = launch(&svc, &app).await;
    let page = get_page(&app, port, &token);
    assert!(
        page.contains("hello-from-url"),
        "URL 来源的静态应用页面应可取到:{page:?}"
    );
    svc.handle(AppRequest::Uninstall {
        id: app.clone(),
        mode: UninstallMode::ProgramAndData,
    })
    .await
    .unwrap();
    svc.shutdown().await;

    // 2) 进程型(Node)应用 zip 走 URL 来源 → 出计划即被拒。
    assert!(have("node"), "缺少 node —— 本用例要求真运行时,不静默跳过");
    let tmp2 = tempfile::tempdir().unwrap();
    let root2 = tmp2.path().join("bytehost");
    let nsrc = copy_sample(&NODE, |m| m);
    let nzip = tmp2.path().join("node.zip");
    pack_zip(&nsrc, &nzip);
    let nbytes = std::fs::read(&nzip).unwrap();
    let svc2 = start_with_fetcher(&root2, TestFetcher::new(nbytes)).await;
    let err = svc2
        .handle(AppRequest::Plan {
            source: AppSource::Url {
                url: "https://example.com/node.zip".into(),
                sha256: None,
            },
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, AppErrorKind::Rejected, "{err:?}");
    assert!(
        err.message.contains("静态应用"),
        "报文应说明只允许静态应用:{err}"
    );
    svc2.shutdown().await;
}
