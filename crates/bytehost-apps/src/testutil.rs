//! 测试辅助(只在 `cargo test --features server` 下编译):一个最小的原始 TCP HTTP/1.1 客户端,
//! 不依赖任何 HTTP 客户端库,这样测试能精确控制 `Host`、`Cookie` 等头,模拟"伪造 Host"之类的请求。

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub(crate) struct Req<'a> {
    pub port: u16,
    pub method: &'a str,
    pub host: Option<&'a str>,
    pub target: &'a str,
    pub cookie: Option<&'a str>,
}

impl<'a> Req<'a> {
    pub fn get(port: u16, host: &'a str, target: &'a str) -> Self {
        Self {
            port,
            method: "GET",
            host: Some(host),
            target,
            cookie: None,
        }
    }

    pub fn cookie(mut self, cookie: &'a str) -> Self {
        self.cookie = Some(cookie);
        self
    }

    pub fn method(mut self, method: &'a str) -> Self {
        self.method = method;
        self
    }

    pub fn send(&self) -> Resp {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("连上 gateway");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("读超时");
        let mut raw = format!("{} {} HTTP/1.1\r\n", self.method, self.target);
        if let Some(h) = self.host {
            raw.push_str(&format!("Host: {h}\r\n"));
        }
        if let Some(c) = self.cookie {
            raw.push_str(&format!("Cookie: {c}\r\n"));
        }
        raw.push_str("Connection: close\r\n\r\n");
        stream.write_all(raw.as_bytes()).expect("发请求");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("读响应");
        parse(&buf)
    }
}

fn parse(buf: &[u8]) -> Resp {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("响应有头");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("状态行");
    let headers = lines
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    Resp {
        status,
        headers,
        body: buf[split + 4..].to_vec(),
    }
}

/// 在 `dir` 下按 (相对路径, 内容) 写出文件。
pub(crate) fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
}

/// 原样发送一段原始请求(测试重复 `Host` 头、绝对形式请求目标这类 `Req` 表达不了的畸形请求)。
pub(crate) fn send_raw(port: u16, raw: &str) -> Resp {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("连上 gateway");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("读超时");
    stream.write_all(raw.as_bytes()).expect("发请求");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("读响应");
    parse(&buf)
}
