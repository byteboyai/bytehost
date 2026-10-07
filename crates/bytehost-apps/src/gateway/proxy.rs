//! 进程型应用的反向代理(bytehost A6b):gateway 通过了 Host 校验与令牌校验之后,把请求原样转发给应用监听的
//! 回环端口(`127.0.0.1:<端口>`),应答(含**流式**正文,SSE/长轮询)与 **WebSocket 升级**都透传。
//!
//! 与静态站点不同的几点,每一点都是有意的:
//! - **所有方法**都放行(应用有自己的 API),但**写方法与 WebSocket 升级必须同源**:带 `Origin` 头且不是应用自己的
//!   origin 就 403(令牌 Cookie 是 `SameSite=Strict`,这是纵深防御);
//! - **不向应用泄露 gateway 的会话 Cookie**(`bh_session` 只给 gateway 自己看);其余头原样转发,**`Host` 保持原样**
//!   (`<id>.localhost:<端口>`——应用按 Host 生成绝对地址、校验 WebSocket 的 Origin 时才对得上),并补
//!   `X-Forwarded-For/Host/Proto`;
//! - **不加 CSP、不加缓存头**:进程型应用的出站网络强制等级是 `Advisory`(见 `runtime::enforcement_for`),
//!   页面与响应头由应用自己负责;
//! - 应用没起来/崩了返回 **502**,连得上但迟迟不出响应头返回 **504**(正文流开始之后不再有总超时)。

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

pub(super) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(super) type Body = BoxBody<Bytes, BoxError>;

/// 把纯应答(`Full`)装进统一的正文类型。
pub(super) fn boxed_reply(r: Response<Full<Bytes>>) -> Response<Body> {
    r.map(|b| b.map_err(|never| match never {}).boxed())
}

fn gateway_error(status: StatusCode, msg: &'static str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from_static(msg.as_bytes())));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    boxed_reply(r)
}

/// 逐跳头(RFC 7230 §6.1):只对一跳有效,代理不能原样转发。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

/// `Connection` 头里点名的额外逐跳头(`Connection: close, X-Foo`)。
fn connection_listed(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// 请求是不是 WebSocket 升级:`Upgrade: websocket` 且 `Connection` 里有 `upgrade`。
pub(super) fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    upgrade && connection_listed(headers).iter().any(|t| t == "upgrade")
}

/// 去掉 Cookie 头里 gateway 自己的会话 Cookie,其余原样保留;剩下什么都没有时返回 `None`(整头不转发)。
pub(super) fn strip_session_cookie(header_value: &str, cookie_name: &str) -> Option<String> {
    let kept: Vec<&str> = header_value
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| p.split_once('=').map(|(k, _)| k.trim()) != Some(cookie_name))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// 写方法与 WebSocket 升级必须同源:有 `Origin` 头就必须等于应用自己的 origin(`null` 与别家都不行);
/// 没有 `Origin`(curl、同源的简单 GET)放行——能走到这里的请求都已经带着令牌 Cookie。
pub(super) fn origin_allowed(
    method: &Method,
    headers: &HeaderMap,
    websocket: bool,
    own_origin: &str,
) -> bool {
    let must_be_same_origin = websocket || (method != Method::GET && method != Method::HEAD);
    if !must_be_same_origin {
        return true;
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(v) => v.to_str().is_ok_and(|o| o == own_origin),
    }
}

/// 给应用的请求头:去逐跳头、去会话 Cookie,补 `X-Forwarded-*`;WebSocket 升级时显式带回 `Upgrade`/`Connection`。
pub(super) fn upstream_headers(src: &HeaderMap, cookie_name: &str, websocket: bool) -> HeaderMap {
    let listed = connection_listed(src);
    let mut out = HeaderMap::new();
    for (name, value) in src {
        if is_hop_by_hop(name)
            || listed.iter().any(|t| t == name.as_str())
            || name == header::COOKIE
            || name.as_str().starts_with("x-forwarded-")
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    let cookies: Vec<String> = src
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| strip_session_cookie(v, cookie_name))
        .collect();
    if !cookies.is_empty()
        && let Ok(v) = HeaderValue::from_str(&cookies.join("; "))
    {
        out.insert(header::COOKIE, v);
    }
    out.insert("x-forwarded-for", HeaderValue::from_static("127.0.0.1"));
    out.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    if let Some(host) = src.get(header::HOST) {
        out.insert("x-forwarded-host", host.clone());
    }
    if websocket {
        out.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        out.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    }
    out
}

/// 给浏览器的应答头:去逐跳头(`Connection`/`Transfer-Encoding` 等由 hyper 自己管);WebSocket 101 保留 `Upgrade`/`Connection`。
fn client_headers(src: &HeaderMap, keep_upgrade: bool) -> HeaderMap {
    let listed = connection_listed(src);
    let mut out = HeaderMap::new();
    for (name, value) in src {
        let upgrade_header = name == header::UPGRADE || name == header::CONNECTION;
        if (is_hop_by_hop(name) && !(keep_upgrade && upgrade_header))
            || (!keep_upgrade && listed.iter().any(|t| t == name.as_str()))
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// 转发一个已经通过 gateway 校验的请求。
pub(super) async fn forward(
    req: Request<Incoming>,
    upstream: SocketAddr,
    own_origin: &str,
    cookie_name: &str,
    connect_timeout: Duration,
    header_timeout: Duration,
) -> Response<Body> {
    let websocket = is_websocket_upgrade(req.headers());
    if !origin_allowed(req.method(), req.headers(), websocket, own_origin) {
        return gateway_error(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    let mut req = req;
    let on_client_upgrade = websocket.then(|| hyper::upgrade::on(&mut req));
    let (parts, body) = req.into_parts();

    let mut builder = Request::builder().method(parts.method.clone()).uri(
        parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/"),
    );
    *builder.headers_mut().expect("builder 没出错") =
        upstream_headers(&parts.headers, cookie_name, websocket);
    let Ok(up_req) = builder.body(body.map_err(|e| Box::new(e) as BoxError).boxed()) else {
        return gateway_error(StatusCode::BAD_REQUEST, "bad request");
    };

    let stream = match tokio::time::timeout(connect_timeout, TcpStream::connect(upstream)).await {
        Ok(Ok(s)) => s,
        _ => return gateway_error(StatusCode::BAD_GATEWAY, "application is not reachable"),
    };
    let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    else {
        return gateway_error(StatusCode::BAD_GATEWAY, "application is not reachable");
    };
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let mut resp = match tokio::time::timeout(header_timeout, sender.send_request(up_req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return gateway_error(StatusCode::BAD_GATEWAY, "application closed the connection");
        }
        Err(_) => {
            return gateway_error(
                StatusCode::GATEWAY_TIMEOUT,
                "application did not answer in time",
            );
        }
    };

    if websocket
        && resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(on_client) = on_client_upgrade
    {
        let on_upstream = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            if let (Ok(client), Ok(app)) = tokio::join!(on_client, on_upstream) {
                let (mut client, mut app) = (TokioIo::new(client), TokioIo::new(app));
                let _ = tokio::io::copy_bidirectional(&mut client, &mut app).await;
            }
        });
        let mut out = Response::new(Full::new(Bytes::new()).map_err(|n| match n {}).boxed());
        *out.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *out.headers_mut() = client_headers(resp.headers(), true);
        return out;
    }

    let (rparts, rbody) = resp.into_parts();
    let mut out = Response::new(rbody.map_err(|e| Box::new(e) as BoxError).boxed());
    *out.status_mut() = rparts.status;
    *out.headers_mut() = client_headers(&rparts.headers, false);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn the_gateway_session_cookie_never_reaches_the_application() {
        assert_eq!(
            strip_session_cookie("a=1; bh_session=SECRET; b=2", "bh_session").as_deref(),
            Some("a=1; b=2")
        );
        assert_eq!(
            strip_session_cookie("bh_session=SECRET", "bh_session"),
            None
        );
        assert_eq!(strip_session_cookie("", "bh_session"), None);
        // 同名 Cookie 出现多次都要去掉;名字相近的不能误伤
        assert_eq!(
            strip_session_cookie("bh_session=A; bh_session2=keep; bh_session=B", "bh_session")
                .as_deref(),
            Some("bh_session2=keep")
        );
    }

    #[test]
    fn hop_by_hop_headers_and_connection_listed_headers_are_not_forwarded() {
        let src = headers(&[
            ("host", "demo.localhost:24000"),
            ("connection", "keep-alive, x-secret-hop"),
            ("keep-alive", "timeout=5"),
            ("x-secret-hop", "1"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("accept", "text/html"),
            ("x-custom", "kept"),
        ]);
        let out = upstream_headers(&src, "bh_session", false);
        for gone in [
            "connection",
            "keep-alive",
            "x-secret-hop",
            "te",
            "transfer-encoding",
        ] {
            assert!(!out.contains_key(gone), "{gone}");
        }
        assert_eq!(out["accept"], "text/html");
        assert_eq!(out["x-custom"], "kept");
        assert_eq!(out["host"], "demo.localhost:24000", "Host 保持原样");
    }

    #[test]
    fn forwarded_headers_are_set_by_the_gateway_not_the_client() {
        let src = headers(&[
            ("host", "demo.localhost:24000"),
            ("x-forwarded-for", "6.6.6.6"),
            ("x-forwarded-host", "evil.example"),
            ("x-forwarded-proto", "https"),
        ]);
        let out = upstream_headers(&src, "bh_session", false);
        assert_eq!(out["x-forwarded-for"], "127.0.0.1");
        assert_eq!(out["x-forwarded-host"], "demo.localhost:24000");
        assert_eq!(out["x-forwarded-proto"], "http");
        assert_eq!(out.get_all("x-forwarded-for").iter().count(), 1);
    }

    #[test]
    fn cookies_are_forwarded_minus_the_session_cookie() {
        let src = headers(&[
            ("cookie", "theme=dark; bh_session=SECRET"),
            ("cookie", "bh_session=ALSO"),
        ]);
        let out = upstream_headers(&src, "bh_session", false);
        assert_eq!(out["cookie"], "theme=dark");
        let only = upstream_headers(
            &headers(&[("cookie", "bh_session=SECRET")]),
            "bh_session",
            false,
        );
        assert!(!only.contains_key("cookie"));
    }

    #[test]
    fn websocket_upgrades_are_detected_precisely_and_re_asserted_upstream() {
        assert!(is_websocket_upgrade(&headers(&[
            ("upgrade", "WebSocket"),
            ("connection", "keep-alive, Upgrade")
        ])));
        assert!(
            !is_websocket_upgrade(&headers(&[("upgrade", "websocket")])),
            "没有 Connection: upgrade"
        );
        assert!(!is_websocket_upgrade(&headers(&[
            ("upgrade", "h2c"),
            ("connection", "upgrade")
        ])));
        assert!(!is_websocket_upgrade(&headers(&[(
            "connection",
            "upgrade"
        )])));
        let src = headers(&[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", "k"),
        ]);
        let out = upstream_headers(&src, "bh_session", true);
        assert_eq!(out["upgrade"], "websocket");
        assert_eq!(out["connection"], "Upgrade");
        assert_eq!(out["sec-websocket-key"], "k");
        assert!(!upstream_headers(&src, "bh_session", false).contains_key("upgrade"));
    }

    #[test]
    fn writes_and_websockets_must_be_same_origin_but_plain_reads_are_not_checked() {
        let own = "http://demo.localhost:24000";
        let evil = headers(&[("origin", "http://evil.example")]);
        let same = headers(&[("origin", own)]);
        let null = headers(&[("origin", "null")]);
        let none = HeaderMap::new();
        for m in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(!origin_allowed(&m, &evil, false, own), "{m}");
            assert!(!origin_allowed(&m, &null, false, own), "{m} null");
            assert!(origin_allowed(&m, &same, false, own), "{m}");
            assert!(origin_allowed(&m, &none, false, own), "{m} 无 Origin");
        }
        assert!(
            !origin_allowed(&Method::GET, &evil, true, own),
            "跨源 WebSocket 升级"
        );
        assert!(origin_allowed(&Method::GET, &same, true, own));
        assert!(
            origin_allowed(&Method::GET, &evil, false, own),
            "普通 GET 不查 Origin"
        );
        assert!(origin_allowed(&Method::HEAD, &evil, false, own));
    }

    #[test]
    fn client_headers_drop_hop_by_hop_but_a_101_keeps_the_upgrade_pair() {
        let src = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-accept", "x"),
            ("transfer-encoding", "chunked"),
            ("content-type", "text/plain"),
        ]);
        let normal = client_headers(&src, false);
        assert!(
            !normal.contains_key("connection")
                && !normal.contains_key("upgrade")
                && !normal.contains_key("transfer-encoding")
        );
        assert_eq!(normal["content-type"], "text/plain");
        let ws = client_headers(&src, true);
        assert_eq!(ws["connection"], "Upgrade");
        assert_eq!(ws["upgrade"], "websocket");
        assert_eq!(ws["sec-websocket-accept"], "x");
        assert!(!ws.contains_key("transfer-encoding"));
    }
}
