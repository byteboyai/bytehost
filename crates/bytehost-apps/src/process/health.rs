//! 阻塞式 HTTP 健康探测:进程活着不等于服务已经在监听/能应答。只用标准库(监管线程里跑,不引入 async)。
//! 判据:能建立连接并收到一个 HTTP 状态行,且状态码不是 5xx(根路径返回 404 的纯 API 应用也算活着)。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Unhealthy(String),
}

/// 解析状态行 `HTTP/1.x <code> ...`;不是 HTTP 返回 `None`。
fn status_code(head: &[u8]) -> Option<u16> {
    let line = head.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end();
    let rest = line.strip_prefix("HTTP/1.")?;
    let mut it = rest.splitn(3, ' ');
    it.next()?;
    it.next()?.parse().ok()
}

/// 探测 `127.0.0.1:<port><path>` 一次。
pub fn probe_http(port: u16, path: &str, timeout: Duration) -> Health {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => return Health::Unhealthy(format!("连接失败: {e}")),
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nUser-Agent: bytehost-health\r\n\r\n"
    );
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return Health::Unhealthy(format!("发送失败: {e}"));
    }
    let mut buf = [0u8; 256];
    let n = match stream.read(&mut buf) {
        Ok(0) => return Health::Unhealthy("对端直接关闭了连接".into()),
        Ok(n) => n,
        Err(e) => return Health::Unhealthy(format!("读取失败: {e}")),
    };
    match status_code(&buf[..n]) {
        Some(code) if code < 500 => Health::Healthy,
        Some(code) => Health::Unhealthy(format!("状态码 {code}")),
        None => Health::Unhealthy("应答不是 HTTP".into()),
    }
}

/// 轮询到健康、超时、或进程已经死了(`alive()` 为假)为止。返回最后一次的结果。
pub fn wait_healthy(
    port: u16,
    path: &str,
    total: Duration,
    interval: Duration,
    mut alive: impl FnMut() -> bool,
) -> Health {
    let deadline = Instant::now() + total;
    let per_try = interval
        .max(Duration::from_millis(200))
        .min(Duration::from_secs(2));
    loop {
        if !alive() {
            return Health::Unhealthy("进程已经退出".into());
        }
        let h = probe_http(port, path, per_try);
        if h == Health::Healthy || Instant::now() + interval >= deadline {
            return h;
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// 一个肯定没人监听的回环端口。**不能用"绑 0 再释放"**:测试并行时,刚释放的端口可能被别的测试立刻重新绑走,
    /// 探测就会连上无关的服务(偶发失败)。端口 1(tcpmux)在回环上基本不会有人监听,连接会被拒绝。
    const REFUSED_PORT: u16 = 1;

    /// 起一个只应答一次的假服务,返回端口。
    fn serve_once(reply: &'static [u8]) -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(reply);
            }
        });
        port
    }

    #[test]
    fn the_status_line_is_parsed_strictly() {
        assert_eq!(status_code(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(status_code(b"HTTP/1.0 404 Not Found\r\n"), Some(404));
        assert_eq!(status_code(b"HTTP/1.1 503\r\n"), Some(503));
        for bad in [
            &b"SSH-2.0-OpenSSH\r\n"[..],
            b"HTTP/2 200\r\n",
            b"",
            b"HTTP/1.1 abc\r\n",
            b"HTTP/1.1\r\n",
        ] {
            assert_eq!(status_code(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn a_2xx_3xx_or_4xx_answer_means_alive_and_5xx_does_not() {
        let t = Duration::from_secs(2);
        assert_eq!(
            probe_http(serve_once(b"HTTP/1.1 200 OK\r\n\r\n"), "/", t),
            Health::Healthy
        );
        assert_eq!(
            probe_http(serve_once(b"HTTP/1.1 404 Not Found\r\n\r\n"), "/", t),
            Health::Healthy
        );
        assert_eq!(
            probe_http(serve_once(b"HTTP/1.1 302 Found\r\n\r\n"), "/x", t),
            Health::Healthy
        );
        assert!(
            matches!(probe_http(serve_once(b"HTTP/1.1 503 Busy\r\n\r\n"), "/", t), Health::Unhealthy(m) if m.contains("503"))
        );
    }

    #[test]
    fn non_http_closed_and_silent_servers_are_unhealthy() {
        let t = Duration::from_millis(500);
        assert!(matches!(
            probe_http(serve_once(b"garbage\r\n"), "/", t),
            Health::Unhealthy(_)
        ));
        assert!(matches!(
            probe_http(serve_once(b""), "/", t),
            Health::Unhealthy(_)
        ));
        // 端口上没有任何人在听
        let closed = REFUSED_PORT;
        assert!(
            matches!(probe_http(closed, "/", t), Health::Unhealthy(m) if m.contains("连接失败"))
        );
    }

    #[test]
    fn waiting_stops_early_when_the_process_died_and_gives_up_at_the_deadline() {
        let closed = REFUSED_PORT;
        let started = Instant::now();
        let h = wait_healthy(
            closed,
            "/",
            Duration::from_secs(5),
            Duration::from_millis(50),
            || false,
        );
        assert_eq!(h, Health::Unhealthy("进程已经退出".into()));
        assert!(started.elapsed() < Duration::from_secs(1));
        let started = Instant::now();
        let h = wait_healthy(
            closed,
            "/",
            Duration::from_millis(400),
            Duration::from_millis(50),
            || true,
        );
        assert!(matches!(h, Health::Unhealthy(_)));
        assert!(started.elapsed() < Duration::from_secs(3), "超时就放弃");
    }

    #[test]
    fn waiting_succeeds_once_the_service_comes_up_late() {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l); // 先关着
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            // 端口刚被释放,别的测试可能恰好占着:短暂重试绑定
            let bound = (0..40).find_map(|_| {
                TcpListener::bind(("127.0.0.1", port)).ok().or_else(|| {
                    std::thread::sleep(Duration::from_millis(50));
                    None
                })
            });
            if let Some(l) = bound {
                // 应答几次
                for _ in 0..20 {
                    if let Ok((mut s, _)) = l.accept() {
                        let mut buf = [0u8; 256];
                        let _ = s.read(&mut buf);
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
                    }
                }
            }
        });
        let h = wait_healthy(
            port,
            "/",
            Duration::from_secs(5),
            Duration::from_millis(100),
            || true,
        );
        assert_eq!(h, Health::Healthy);
    }
}
