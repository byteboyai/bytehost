//! 反向代理的端到端测试:真 gateway + 一个手写的假上游(std 线程),全部走回环。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

/// 假上游:把收到的每个请求的原始头部记下来;按路径给出不同行为。
struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    /// `/ws` 的那条连接是否已被关闭(读到 EOF 或写失败)。
    ws_closed: Arc<AtomicBool>,
}

fn read_head(s: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).ok()? == 0 {
            return None;
        }
        buf.push(b[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).ok()?;
    Some((head, body))
}

fn spawn_upstream() -> Upstream {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let ws_closed = Arc::new(AtomicBool::new(false));
    let ws_closed2 = ws_closed.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { return };
            let seen = seen2.clone();
            let ws_closed = ws_closed2.clone();
            std::thread::spawn(move || {
                let Some((head, body)) = read_head(&mut s) else {
                    return;
                };
                seen.lock().unwrap().push(head.clone());
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                match path.as_str() {
                    "/echo" => {
                        let payload =
                            format!("{head}\n--body--\n{}", String::from_utf8_lossy(&body));
                        let _ = write!(
                            s,
                            "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nX-App: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                    }
                    "/sse" => {
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n8\r\ndata: 1\n\r\n");
                        let _ = s.flush();
                        std::thread::sleep(Duration::from_millis(700));
                        let _ = s.write_all(b"8\r\ndata: 2\n\r\n0\r\n\r\n");
                    }
                    "/ws" => {
                        let _ = s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: abc\r\n\r\n");
                        let mut b = [0u8; 64];
                        while let Ok(n) = s.read(&mut b) {
                            if n == 0 || s.write_all(&b[..n]).is_err() {
                                break;
                            }
                        }
                        ws_closed.store(true, Ordering::SeqCst);
                    }
                    "/slow" => std::thread::sleep(Duration::from_secs(3)),
                    _ => {
                        let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            });
        }
    });
    Upstream {
        addr,
        seen,
        ws_closed,
    }
}

struct Fixture {
    gw: Gateway,
    host: String,
    cookie: String,
    port: u16,
}

async fn fixture(up: Option<SocketAddr>, limits: Limits) -> Fixture {
    let gw = Gateway::start_with_limits(GatewayConfig { port: 0 }, limits)
        .await
        .unwrap();
    let id = AppId::new("pyapp").unwrap();
    gw.add_upstream(&id, up.unwrap_or_else(|| "127.0.0.1:1".parse().unwrap()));
    let port = gw.port();
    Fixture {
        host: format!("pyapp.localhost:{port}"),
        cookie: format!("bh_session={}", app_token(&gw.state.token, &id)),
        gw,
        port,
    }
}

/// 发一个原始请求(带 Host 与会话 Cookie),读到连接关闭。
fn exchange(f: &Fixture, req_line: &str, extra: &str, body: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", f.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "{req_line} HTTP/1.1\r\nHost: {}\r\nCookie: {}; theme=dark\r\n{extra}Content-Length: {}\r\n\r\n{body}", f.host, f.cookie, body.len()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_is_forwarded_with_its_body_and_the_apps_status_and_headers_come_back() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let resp = exchange(
        &f,
        "POST /echo",
        "Content-Type: application/json\r\n",
        "{\"a\":1}",
    );
    assert!(resp.starts_with("HTTP/1.1 201"), "{resp}");
    assert!(resp.to_ascii_lowercase().contains("x-app: yes"));
    assert!(resp.contains("{\"a\":1}"), "正文要到达应用");
    let seen = up.seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        seen.contains(&format!("host: {}", f.host)),
        "Host 保持原样: {seen}"
    );
    assert!(
        seen.contains("x-forwarded-proto: http") && seen.contains("x-forwarded-for: 127.0.0.1")
            || seen.contains("x-forwarded-for"),
        "{seen}"
    );
    assert!(
        !seen.contains("bh_session"),
        "会话 Cookie 不能泄露给应用: {seen}"
    );
    assert!(seen.contains("theme=dark"), "其他 Cookie 原样转发: {seen}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_requests_never_reach_the_app() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let mut s = TcpStream::connect(("127.0.0.1", f.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET /echo HTTP/1.1\r\nHost: {}\r\n\r\n", f.host).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    let mut s = TcpStream::connect(("127.0.0.1", f.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET /echo HTTP/1.1\r\nHost: pyapp.localhost:{}\r\nCookie: bh_session=wrong\r\n\r\n",
        f.port
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    assert!(up.seen.lock().unwrap().is_empty(), "未认证请求不得到达应用");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_origin_write_is_refused_before_it_reaches_the_app() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let resp = exchange(&f, "POST /echo", "Origin: http://evil.example\r\n", "x");
    assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
    assert!(up.seen.lock().unwrap().is_empty());
    let same = format!("Origin: http://{}\r\n", f.host);
    assert!(exchange(&f, "POST /echo", &same, "x").starts_with("HTTP/1.1 201"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_response_reaches_the_client_before_the_app_finishes() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let mut s = TcpStream::connect(("127.0.0.1", f.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET /sse HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\r\n",
        f.host, f.cookie
    )
    .unwrap();
    let start = Instant::now();
    let mut got = Vec::new();
    let mut b = [0u8; 256];
    while !String::from_utf8_lossy(&got).contains("data: 1") {
        let n = s.read(&mut b).unwrap();
        assert!(n > 0, "连接过早关闭");
        got.extend_from_slice(&b[..n]);
    }
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "第一块应立即到达,而不是等应用结束: {:?}",
        start.elapsed()
    );
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    assert!(String::from_utf8_lossy(&rest).contains("data: 2"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_websocket_upgrade_becomes_a_two_way_tunnel() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let mut s = TcpStream::connect(("127.0.0.1", f.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET /ws HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\nOrigin: http://{}\r\n\r\n", f.host, f.cookie, f.host).unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).unwrap(), 1);
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(
        head.starts_with("http/1.1 101") && head.contains("sec-websocket-accept: abc"),
        "{head}"
    );
    s.write_all(b"ping-frame").unwrap();
    let mut echo = [0u8; 10];
    s.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"ping-frame");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_origin_websocket_upgrade_is_refused() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let resp = exchange(
        &f,
        "GET /ws",
        "Connection: Upgrade\r\nUpgrade: websocket\r\nOrigin: http://evil.example\r\n",
        "",
    );
    assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
    assert!(up.seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_upstream_is_502_and_a_silent_one_is_504() {
    // 502:端口上没人监听
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let f = fixture(Some(dead_addr), Limits::default()).await;
    assert!(exchange(&f, "GET /echo", "", "").starts_with("HTTP/1.1 502"));
    // 504:连得上但不给响应头
    let up = spawn_upstream();
    let limits = Limits {
        upstream_header_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let f = fixture(Some(up.addr), limits).await;
    let start = Instant::now();
    assert!(exchange(&f, "GET /slow", "", "").starts_with("HTTP/1.1 504"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_app_stops_proxying_to_it() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    assert!(f.gw.remove_site(&AppId::new("pyapp").unwrap()));
    assert!(exchange(&f, "GET /echo", "", "").starts_with("HTTP/1.1 404"));
    assert!(up.seen.lock().unwrap().is_empty());
}

/// 同一个应用 id 同时只在一张表里:静态站点与上游互相替换,而不是并存。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_and_an_upstream_replace_each_other_for_the_same_app() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let id = AppId::new("pyapp").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "static").unwrap();
    f.gw.add_site(&id, dir.path().to_path_buf(), None);
    assert!(f.gw.has_site(&id));
    assert!(exchange(&f, "GET /", "", "").contains("static"));
    assert!(
        up.seen.lock().unwrap().is_empty(),
        "已换成静态站点,不该再代理"
    );
    f.gw.add_upstream(&id, up.addr);
    assert!(exchange(&f, "GET /echo", "", "").starts_with("HTTP/1.1 201"));
    assert!(f.gw.state.sites_read().get("pyapp").is_none());
    // 注销一次就彻底没了
    assert!(f.gw.remove_site(&id));
    assert!(!f.gw.has_site(&id));
}

/// 客户端断开后,隧道另一端(应用那条连接)也要被关掉,不能泄漏。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_client_side_of_a_websocket_closes_the_apps_connection() {
    let up = spawn_upstream();
    let f = fixture(Some(up.addr), Limits::default()).await;
    let mut s = TcpStream::connect(("127.0.0.1", f.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET /ws HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\nOrigin: http://{}\r\n\r\n", f.host, f.cookie, f.host).unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).unwrap(), 1);
        head.push(b[0]);
    }
    assert!(!up.ws_closed.load(Ordering::SeqCst));
    drop(s);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !up.ws_closed.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "应用那条连接没被关闭");
        std::thread::sleep(Duration::from_millis(20));
    }
}
