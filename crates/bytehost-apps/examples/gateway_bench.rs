//! gateway 静态资源热路径基准(bytehost 优化 Tier4 Task0)。
//!
//! 目标:给 `Gateway` 的静态站点请求一条**可复现**的基线,记录 p50/p99、吞吐与每请求分配次数,
//! 供 Tier4 Task1(`Arc<Site>` 替代每请求 clone)改前改后对比。
//!
//! 用法:
//!   scripts/bench-gateway.sh [并发] [请求数]
//!   cargo run --release -p bytehost-apps --example gateway_bench --features server -- 64 5000
//!
//! 说明:
//! - 客户端是**手写 HTTP/1.1**(一个请求一条连接,与 gateway `Connection: close` 的行为一致),
//!   不引入额外依赖;客户端与服务端同进程,所以"分配次数"是**整进程**(客户端+服务端)的口径,
//!   用来做改前/改后的相对比较,不是纯服务端分配数。
//! - 场景:1 KiB 的 `index.html`,并发 N 连接、每连接顺序发请求。

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use bytehost_apps::gateway::{Gateway, GatewayConfig};
use bytehost_apps::id::AppId;

// ---- 分配计数:只为基准服务,生产构建不启用 ----
struct CountingAlloc;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

const APP: &str = "bench";
const INDEX_BYTES: usize = 1024;

/// 一次原始 HTTP/1.1 请求的结果。
struct RawReply {
    status: u16,
    cookie: Option<String>,
    body_len: usize,
}

/// 发一个 `GET`(一个请求一条连接,读到 EOF)。`cookie` 有值时带上 `Cookie` 头。
async fn get(
    addr: SocketAddr,
    host: &str,
    target: &str,
    cookie: Option<&str>,
) -> std::io::Result<RawReply> {
    let mut stream = TcpStream::connect(addr).await?;
    let cookie_header = match cookie {
        Some(c) => format!("Cookie: {c}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\n{cookie_header}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(2048);
    stream.read_to_end(&mut buf).await?;

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "缺响应头分隔符"))?;
    let (head, body) = buf.split_at(split);
    let head = String::from_utf8_lossy(head);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "缺状态码"))?;
    let cookie = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("set-cookie")
            .then(|| value.split(';').next().unwrap_or(value).trim().to_string())
    });
    Ok(RawReply {
        status,
        cookie,
        body_len: body.len(),
    })
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * pct / 100).min(sorted.len() - 1);
    sorted[idx]
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let concurrency = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| env_usize("BENCH_CONCURRENCY", 64));
    let requests = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| env_usize("BENCH_REQUESTS", 5000));
    let warmup = requests / 10;
    assert!(concurrency > 0 && requests > 0, "并发与请求数必须为正");

    // 站点:index.html 约 1 KiB。
    let root = std::env::temp_dir().join(format!("bytehost-gateway-bench-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    let mut html = String::with_capacity(INDEX_BYTES + 64);
    html.push_str("<!doctype html><html><body><h1>bench</h1>");
    while html.len() < INDEX_BYTES {
        html.push('x');
    }
    html.push_str("</body></html>");
    std::fs::File::create(root.join("index.html"))?.write_all(html.as_bytes())?;

    let app = AppId::new(APP)?;
    let gateway = Gateway::start(GatewayConfig { port: 0 }).await?;
    gateway.add_site(&app, root.clone(), None);
    let port = gateway.port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let host = format!("{APP}.localhost:{port}");

    // 首次导航用 launch_url 换 Cookie,之后只带 Cookie。
    let launch = gateway.launch_url(&app);
    let target = launch
        .strip_prefix(&format!("http://{host}"))
        .expect("launch_url 以站点 origin 开头");
    let first = get(addr, &host, target, None).await?;
    assert_eq!(first.status, 302, "首次导航应 302 换 Cookie");
    let cookie = first.cookie.expect("首次导航应下发 Set-Cookie");

    // 每个 worker 顺序发请求;返回各自的延迟(微秒)。
    async fn run_batch(
        addr: SocketAddr,
        host: String,
        cookie: String,
        per_worker: usize,
    ) -> Vec<u64> {
        let mut latencies = Vec::with_capacity(per_worker);
        for _ in 0..per_worker {
            let started = Instant::now();
            let reply = get(addr, &host, "/", Some(&cookie))
                .await
                .expect("请求失败");
            latencies.push(started.elapsed().as_micros() as u64);
            assert_eq!(reply.status, 200, "静态请求应 200");
            assert!(reply.body_len >= INDEX_BYTES);
        }
        latencies
    }

    // 预热:不计入结果,但把文件系统缓存/连接路径焐热。
    let warm_per_worker = warmup / concurrency;
    if warm_per_worker > 0 {
        let warm: Vec<_> = (0..concurrency)
            .map(|_| run_batch(addr, host.clone(), cookie.clone(), warm_per_worker))
            .collect();
        for batch in warm {
            batch.await;
        }
    }

    // 正式测量:并发起跑,记录整进程分配增量。
    let allocs_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let per_worker = requests.div_ceil(concurrency);
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        workers.push(tokio::spawn(run_batch(
            addr,
            host.clone(),
            cookie.clone(),
            per_worker,
        )));
    }
    let mut latencies = Vec::with_capacity(per_worker * concurrency);
    for worker in workers {
        latencies.extend(worker.await?);
    }
    let elapsed = started.elapsed();
    let allocs = ALLOCATIONS.load(Ordering::Relaxed) - allocs_before;
    let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes_before;

    gateway.stop().await;
    let _ = std::fs::remove_dir_all(&root);

    latencies.sort_unstable();
    let count = latencies.len();
    let as_ms = |us: u64| us as f64 / 1000.0;
    println!("gateway 基准(静态站点,1 KiB index.html)");
    println!(
        "  并发 {concurrency},完成 {count} 请求,用时 {:.2}s",
        elapsed.as_secs_f64()
    );
    println!("  吞吐 {:.0} req/s", count as f64 / elapsed.as_secs_f64());
    println!(
        "  延迟 p50 {:.2}ms  p99 {:.2}ms  max {:.2}ms",
        as_ms(percentile(&latencies, 50)),
        as_ms(percentile(&latencies, 99)),
        as_ms(*latencies.last().unwrap_or(&0)),
    );
    println!(
        "  分配(整进程:客户端+服务端) {:.1} 次/请求,{:.0} B/请求",
        allocs as f64 / count as f64,
        bytes as f64 / count as f64,
    );
    Ok(())
}
