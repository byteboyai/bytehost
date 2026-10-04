//! gateway:本机 HTTP 服务,**一个固定端口服务所有应用**,按 `Host` 头路由到各应用的站点
//! (`http://<app-id>.localhost:<端口>/`)。设计依据与实测见 `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`
//! §5 与 `spike/origin-gateway/README.md`。
//!
//! 安全顺序(每一步失败就停,不泄露后面步骤的信息):
//! 1. **Host 校验**:必须恰好是 `<合法 app id>.localhost:<本 gateway 的端口>`,否则 421(防 DNS rebinding);
//! 2. **会话令牌**:请求要带 gateway 启动时生成的令牌——首次导航用 `?bh_token=…`,gateway 设 host-only 的
//!    `HttpOnly; SameSite=Strict` Cookie 并 302 到去掉令牌的地址;之后靠 Cookie。没有令牌一律 403
//!    (连"这个应用存不存在"都不告诉本机上的其他网页);
//! 3. **站点查找**:没注册的应用 404;
//! 4. **方法**:只允许 GET/HEAD;
//! 5. **路径解析**(`static_files::resolve`):任何写法都不能走出站点根目录。

mod static_files;

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};

use crate::id::AppId;
use crate::permissions::{Outbound, Permissions};

use static_files::{Resolved, mime_for, resolve};

const TOKEN_PARAM: &str = "bh_token";
const COOKIE_NAME: &str = "bh_session";

#[derive(Debug)]
pub enum GatewayError {
    /// 配置的端口已被占用。**不会静默换端口**——换端口会让所有应用丢失本地存储(origin 含端口)。
    PortInUse(u16),
    Io(std::io::Error),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PortInUse(p) => write!(
                f,
                "gateway 端口 {p} 已被占用(不会自动换端口:换端口会让所有应用丢失本地数据)"
            ),
            Self::Io(e) => write!(f, "gateway I/O 错误: {e}"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// 连接层限制。gateway 跑在 dozerd 里,和 PTY 池共用文件描述符:网页可以让浏览器对 `rN.localhost:端口` 开大量连接,
/// 所以半截请求头、空闲的 keep-alive 连接都不能永远占着 fd。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 读完整个请求头的最长时间,超时断开。
    pub header_read_timeout: std::time::Duration,
    /// 同时处理的连接数上限;超出的连接被立即丢弃。
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_read_timeout: std::time::Duration::from_secs(10),
            max_connections: 128,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GatewayConfig {
    /// 监听端口(127.0.0.1)。`0` 让系统分配——只给测试用;生产用 `port::load_or_choose_port` 的值。
    pub port: u16,
}

/// 一个静态站点:根目录与(可选的)`Content-Security-Policy`。
#[derive(Debug, Clone)]
struct Site {
    root: PathBuf,
    csp: Option<String>,
}

struct State {
    port: u16,
    token: String,
    sites: RwLock<HashMap<String, Site>>,
}

/// 运行中的 gateway。`Drop` 不会停服务,要停请 [`Gateway::shutdown`]。
pub struct Gateway {
    state: Arc<State>,
    shutdown: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

/// 按**已授予**的权限算 CSP:出站网络为 `none` 时,用 CSP 真正禁止页面向外发请求(`Enforced`);
/// 为 `any` 时不加 CSP。
pub fn csp_for(grants: &Permissions) -> Option<String> {
    match grants.network.outbound {
        Outbound::Any => None,
        Outbound::None => Some(
            "default-src 'self' data: blob:; script-src 'self' 'wasm-unsafe-eval'; \
             style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; \
             connect-src 'self'; object-src 'none'; frame-ancestors 'self'; \
             base-uri 'self'; form-action 'self'"
                .to_string(),
        ),
    }
}

impl Gateway {
    pub async fn start(config: GatewayConfig) -> Result<Self, GatewayError> {
        Self::start_with_limits(config, Limits::default()).await
    }

    pub async fn start_with_limits(
        config: GatewayConfig,
        limits: Limits,
    ) -> Result<Self, GatewayError> {
        let listener = TcpListener::bind(("127.0.0.1", config.port))
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    GatewayError::PortInUse(config.port)
                } else {
                    GatewayError::Io(e)
                }
            })?;
        let port = listener.local_addr().map_err(GatewayError::Io)?.port();
        let state = Arc::new(State {
            port,
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            sites: RwLock::new(HashMap::new()),
        });
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(accept_loop(
            listener,
            state.clone(),
            shutdown.clone(),
            limits,
        ));
        Ok(Self {
            state,
            shutdown,
            task,
        })
    }

    pub fn port(&self) -> u16 {
        self.state.port
    }

    /// 注册(或替换)一个应用的静态站点。
    pub fn add_site(&self, id: &AppId, root: PathBuf, csp: Option<String>) {
        self.state
            .sites
            .write()
            .expect("sites 锁")
            .insert(id.as_str().to_string(), Site { root, csp });
    }

    /// 注销站点;返回之前是否注册过。
    pub fn remove_site(&self, id: &AppId) -> bool {
        self.state
            .sites
            .write()
            .expect("sites 锁")
            .remove(id.as_str())
            .is_some()
    }

    pub fn has_site(&self, id: &AppId) -> bool {
        self.state
            .sites
            .read()
            .expect("sites 锁")
            .contains_key(id.as_str())
    }

    /// 不含令牌的站点地址(可以放进事件/日志)。
    pub fn site_url(&self, id: &AppId) -> String {
        format!("http://{id}.localhost:{}/", self.state.port)
    }

    /// 首次导航用的地址:带一次性换 Cookie 的令牌。**这是秘密,不要写进日志或广播事件。**
    pub fn launch_url(&self, id: &AppId) -> String {
        format!("{}?{TOKEN_PARAM}={}", self.site_url(id), self.state.token)
    }

    pub async fn shutdown(self) {
        // notify_one 会存一个许可:即使 accept 循环还没跑到 `notified()`,稍后也能收到(notify_waiters 会丢)
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<State>,
    shutdown: Arc<Notify>,
    limits: Limits,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            accepted = listener.accept() => {
                let Ok((stream, _addr)) = accepted else { continue };
                // 超出上限的连接立即丢弃(客户端看到连接被关闭),不排队
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                let state = state.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let service = service_fn(move |req: Request<Incoming>| {
                        let state = state.clone();
                        async move {
                            let method = req.method().clone();
                            let host = header_str(&req, header::HOST);
                            let cookie = header_str(&req, header::COOKIE);
                            let target = req
                                .uri()
                                .path_and_query()
                                .map(|p| p.as_str().to_string())
                                .unwrap_or_else(|| "/".to_string());
                            let reply = tokio::task::spawn_blocking(move || {
                                respond(&state, &method, host.as_deref(), &target, cookie.as_deref())
                            })
                            .await
                            .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "internal error"));
                            Ok::<_, Infallible>(reply)
                        }
                    });
                    // 每个应答之后就关连接(本机静态站点不需要 keep-alive),并给读请求头设超时
                    let mut http = hyper::server::conn::http1::Builder::new();
                    http.timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(limits.header_read_timeout)
                        .keep_alive(false);
                    let _ = http.serve_connection(TokioIo::new(stream), service).await;
                });
            }
        }
    }
}

fn header_str(req: &Request<Incoming>, name: header::HeaderName) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

type Reply = Response<Full<Bytes>>;

fn plain(status: StatusCode, body: &'static str) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    harden(r.headers_mut());
    r
}

fn harden(h: &mut header::HeaderMap) {
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
}

/// `Host` 头必须恰好是 `<合法 app id>.localhost:<port>`;返回 app id。
fn app_from_host(host: Option<&str>, port: u16) -> Option<AppId> {
    let (name, host_port) = host?.rsplit_once(':')?;
    if host_port.parse::<u16>().ok()? != port {
        return None;
    }
    AppId::new(name.strip_suffix(".localhost")?).ok()
}

fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

/// 常量时间比较(不因第一个不同字节提前返回)。
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// 从查询串里取出令牌,返回 (令牌, 去掉令牌后的查询串)。
fn split_token(query: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(q) = query else { return (None, None) };
    let mut token = None;
    let mut rest = Vec::new();
    for pair in q.split('&') {
        match pair.split_once('=') {
            Some((k, v)) if k == TOKEN_PARAM => token = Some(v.to_string()),
            _ if pair.is_empty() => {}
            _ => rest.push(pair),
        }
    }
    (token, (!rest.is_empty()).then(|| rest.join("&")))
}

/// 处理一个请求。纯同步、不碰网络,便于直接测试;`target` 是请求行里的 path+query。
fn respond(
    state: &State,
    method: &Method,
    host: Option<&str>,
    target: &str,
    cookie: Option<&str>,
) -> Reply {
    // 1. Host
    let Some(app) = app_from_host(host, state.port) else {
        return plain(StatusCode::MISDIRECTED_REQUEST, "misdirected request");
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    // 2. 令牌
    let (query_token, rest_query) = split_token(query);
    if let Some(t) = &query_token {
        if !ct_eq(t, &state.token) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        let location = match &rest_query {
            Some(q) => format!("{path}?{q}"),
            None => path.to_string(),
        };
        let mut r = Response::new(Full::new(Bytes::new()));
        *r.status_mut() = StatusCode::FOUND;
        r.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        r.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
                state.token
            ))
            .expect("令牌是十六进制"),
        );
        harden(r.headers_mut());
        return r;
    }
    let authed = cookie
        .and_then(|c| cookie_value(c, COOKIE_NAME))
        .is_some_and(|v| ct_eq(v, &state.token));
    if !authed {
        return plain(StatusCode::FORBIDDEN, "forbidden");
    }
    // 3. 站点
    let Some(site) = state
        .sites
        .read()
        .expect("sites 锁")
        .get(app.as_str())
        .cloned()
    else {
        return plain(StatusCode::NOT_FOUND, "no such app");
    };
    // 4. 方法
    if method != Method::GET && method != Method::HEAD {
        let mut r = plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    // 5. 路径
    match resolve(&site.root, path) {
        Resolved::File(file) => serve_file(&file, &site, method == Method::HEAD),
        Resolved::NotFound => plain(StatusCode::NOT_FOUND, "not found"),
        Resolved::Forbidden => plain(StatusCode::FORBIDDEN, "forbidden"),
        Resolved::BadRequest => plain(StatusCode::BAD_REQUEST, "bad request"),
    }
}

fn serve_file(file: &Path, site: &Site, head_only: bool) -> Reply {
    let Ok(bytes) = std::fs::read(file) else {
        return plain(StatusCode::NOT_FOUND, "not found");
    };
    let len = bytes.len();
    let mut r = Response::new(Full::new(if head_only {
        Bytes::new()
    } else {
        Bytes::from(bytes)
    }));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime_for(file)),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    harden(h);
    if let Some(csp) = &site.csp
        && let Ok(v) = HeaderValue::from_str(csp)
    {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{NetworkPerm, Outbound};
    use crate::testutil::{Req, write_files};

    const TOKEN: &str = "tok";

    fn state_with(port: u16, apps: &[(&str, &Path, Option<String>)]) -> State {
        let mut sites = HashMap::new();
        for (id, root, csp) in apps {
            sites.insert(
                id.to_string(),
                Site {
                    root: root.to_path_buf(),
                    csp: csp.clone(),
                },
            );
        }
        State {
            port,
            token: TOKEN.to_string(),
            sites: RwLock::new(sites),
        }
    }

    fn site_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        write_files(
            &root,
            &[("index.html", "<h1>excalidraw</h1>"), ("app.js", "1")],
        );
        write_files(tmp.path(), &[("secret.txt", "TOP SECRET")]);
        (tmp, root)
    }

    fn get(state: &State, host: Option<&str>, target: &str, cookie: Option<&str>) -> StatusCode {
        respond(state, &Method::GET, host, target, cookie).status()
    }

    const COOKIE: &str = "bh_session=tok";

    #[test]
    fn the_host_header_must_be_exactly_a_valid_app_dot_localhost_with_the_gateways_port() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        assert_eq!(
            get(&st, Some("excalidraw.localhost:5000"), "/", Some(COOKIE)),
            StatusCode::OK
        );
        for bad in [
            Some("excalidraw.localhost:5001"),
            Some("excalidraw.localhost"),
            Some("evil.example:5000"),
            Some("excalidraw.localhost.evil.example:5000"),
            Some("127.0.0.1:5000"),
            Some("localhost:5000"),
            Some(".localhost:5000"),
            Some("a.b.localhost:5000"),
            Some("EXCALIDRAW.localhost:5000"),
            Some("excalidraw.localhost:5000:1"),
            Some("excalidraw.localhost:notaport"),
            Some(""),
            None,
        ] {
            assert_eq!(
                get(&st, bad, "/", Some(COOKIE)),
                StatusCode::MISDIRECTED_REQUEST,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn without_the_token_every_request_is_403_even_for_apps_that_do_not_exist() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let host = Some("excalidraw.localhost:5000");
        assert_eq!(get(&st, host, "/", None), StatusCode::FORBIDDEN);
        assert_eq!(
            get(&st, host, "/", Some("bh_session=wrong")),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&st, host, "/", Some("other=tok")),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&st, host, "/", Some("bh_session=tokx")),
            StatusCode::FORBIDDEN
        );
        // 不存在的应用:没令牌同样 403(不泄露哪些应用已安装);有令牌才是 404
        assert_eq!(
            get(&st, Some("ghost.localhost:5000"), "/", None),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&st, Some("ghost.localhost:5000"), "/", Some(COOKIE)),
            StatusCode::NOT_FOUND
        );
        // 多个 cookie 里取对的那个
        assert_eq!(
            get(&st, host, "/", Some("a=1; bh_session=tok; b=2")),
            StatusCode::OK
        );
    }

    #[test]
    fn the_token_in_the_query_becomes_a_host_only_strict_cookie_and_is_stripped_by_a_redirect() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let host = Some("excalidraw.localhost:5000");
        let r = respond(
            &st,
            &Method::GET,
            host,
            "/app.js?x=1&bh_token=tok&y=2",
            None,
        );
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(r.headers()[header::LOCATION], "/app.js?x=1&y=2");
        let cookie = r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with("bh_session=tok;"), "{cookie}");
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/"] {
            assert!(cookie.contains(attr), "{cookie}");
        }
        assert!(
            !cookie.to_ascii_lowercase().contains("domain"),
            "host-only,不设 Domain:{cookie}"
        );
        // 只有令牌时 Location 就是路径本身
        let r = respond(&st, &Method::GET, host, "/?bh_token=tok", None);
        assert_eq!(r.headers()[header::LOCATION], "/");
        // 错误令牌:403,且不下发 Cookie
        let r = respond(&st, &Method::GET, host, "/?bh_token=nope", None);
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(r.headers().get(header::SET_COOKIE).is_none());
        // 令牌换 Cookie 之前仍要先过 Host 校验
        let r = respond(
            &st,
            &Method::GET,
            Some("evil.example:5000"),
            "/?bh_token=tok",
            None,
        );
        assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[test]
    fn files_are_served_with_the_right_type_length_and_hardening_headers() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let r = respond(
            &st,
            &Method::GET,
            Some("excalidraw.localhost:5000"),
            "/",
            Some(COOKIE),
        );
        assert_eq!(r.status(), StatusCode::OK);
        let h = r.headers();
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(h[header::CONTENT_LENGTH], "19");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        assert!(
            h.get(header::CONTENT_SECURITY_POLICY).is_none(),
            "没配 CSP 就不带"
        );
        let js = respond(
            &st,
            &Method::GET,
            Some("excalidraw.localhost:5000"),
            "/app.js",
            Some(COOKIE),
        );
        assert_eq!(
            js.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
    }

    #[test]
    fn traversal_attempts_return_403_or_400_and_never_the_secret() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        for target in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/..%5csecret.txt",
            "/%00",
        ] {
            let s = get(&st, Some("excalidraw.localhost:5000"), target, Some(COOKIE));
            assert!(
                s == StatusCode::FORBIDDEN || s == StatusCode::BAD_REQUEST,
                "{target}: {s}"
            );
        }
        assert_eq!(
            get(
                &st,
                Some("excalidraw.localhost:5000"),
                "/nope",
                Some(COOKIE)
            ),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn only_get_and_head_are_allowed_and_head_carries_no_body() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let host = Some("excalidraw.localhost:5000");
        for m in [
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ] {
            let r = respond(&st, &m, host, "/", Some(COOKIE));
            assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED, "{m}");
            assert_eq!(r.headers()[header::ALLOW], "GET, HEAD");
        }
        let head = respond(&st, &Method::HEAD, host, "/", Some(COOKIE));
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::CONTENT_LENGTH], "19", "长度照给");
    }

    #[test]
    fn the_csp_follows_the_granted_outbound_network_permission() {
        let none = Permissions::default();
        let csp = csp_for(&none).expect("outbound=none 要有 CSP");
        for needed in [
            "default-src 'self'",
            "connect-src 'self'",
            "script-src 'self'",
            "object-src 'none'",
            "frame-ancestors 'self'",
        ] {
            assert!(csp.contains(needed), "{csp}");
        }
        assert!(
            !csp.contains("http:") && !csp.contains("https:") && !csp.contains('*'),
            "{csp}"
        );
        let any = Permissions {
            network: NetworkPerm {
                outbound: Outbound::Any,
            },
            ..Permissions::default()
        };
        assert_eq!(csp_for(&any), None);

        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, Some(csp.clone()))]);
        let r = respond(
            &st,
            &Method::GET,
            Some("excalidraw.localhost:5000"),
            "/",
            Some(COOKIE),
        );
        assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], csp.as_str());
    }

    #[test]
    fn query_helpers_split_the_token_and_read_cookies_precisely() {
        assert_eq!(split_token(None), (None, None));
        assert_eq!(split_token(Some("bh_token=t")), (Some("t".into()), None));
        assert_eq!(
            split_token(Some("a=1&bh_token=t&b=2")),
            (Some("t".into()), Some("a=1&b=2".into()))
        );
        assert_eq!(split_token(Some("a=1&&b")), (None, Some("a=1&b".into())));
        assert_eq!(
            split_token(Some("xbh_token=t")),
            (None, Some("xbh_token=t".into())),
            "只认完整的参数名"
        );
        assert_eq!(
            cookie_value("a=1; bh_session=x; b=2", "bh_session"),
            Some("x")
        );
        assert_eq!(cookie_value("xbh_session=x", "bh_session"), None);
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_port_serves_several_apps_each_by_its_own_host_name() {
        let (_tmp_a, root_a) = site_dir();
        let tmp_b = tempfile::tempdir().unwrap();
        write_files(tmp_b.path(), &[("index.html", "<h1>drawio</h1>")]);
        let gw = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let (a, b) = (
            AppId::new("excalidraw").unwrap(),
            AppId::new("drawio").unwrap(),
        );
        gw.add_site(&a, root_a, None);
        gw.add_site(&b, tmp_b.path().to_path_buf(), None);
        let port = gw.port();
        let cookie = format!(
            "bh_session={TOKEN_PLACEHOLDER}",
            TOKEN_PLACEHOLDER = gw.state.token
        );

        let ra = Req::get(port, &format!("excalidraw.localhost:{port}"), "/")
            .cookie(&cookie)
            .send();
        assert_eq!((ra.status, ra.text().contains("excalidraw")), (200, true));
        let rb = Req::get(port, &format!("drawio.localhost:{port}"), "/")
            .cookie(&cookie)
            .send();
        assert_eq!((rb.status, rb.text().contains("drawio")), (200, true));
        // 伪造 Host:421;错误端口:421
        assert_eq!(
            Req::get(port, "evil.example", "/")
                .cookie(&cookie)
                .send()
                .status,
            421
        );
        assert_eq!(
            Req::get(
                port,
                &format!("excalidraw.localhost:{}", port.wrapping_add(1)),
                "/"
            )
            .cookie(&cookie)
            .send()
            .status,
            421
        );
        // 没有 Cookie:403
        assert_eq!(
            Req::get(port, &format!("excalidraw.localhost:{port}"), "/")
                .send()
                .status,
            403
        );

        // 用 launch_url 里的令牌换 Cookie 再访问
        let launch = gw.launch_url(&a);
        let target = launch
            .strip_prefix(&format!("http://excalidraw.localhost:{port}"))
            .unwrap()
            .to_string();
        let r = Req::get(port, &format!("excalidraw.localhost:{port}"), &target).send();
        assert_eq!(r.status, 302);
        assert!(r.header("set-cookie").unwrap().starts_with("bh_session="));
        assert!(!gw.site_url(&a).contains("bh_token"), "site_url 不含令牌");

        // 注销之后 404(带 Cookie)
        assert!(gw.remove_site(&a));
        assert!(!gw.remove_site(&a));
        assert_eq!(
            Req::get(port, &format!("excalidraw.localhost:{port}"), "/")
                .cookie(&cookie)
                .send()
                .status,
            404
        );
        let head = Req::get(port, &format!("drawio.localhost:{port}"), "/")
            .cookie(&cookie)
            .method("HEAD")
            .send();
        assert_eq!((head.status, head.body.len()), (200, 0));
        gw.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_port_that_is_already_taken_is_a_clear_error_never_a_silent_new_port() {
        let first = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let taken = first.port();
        match Gateway::start(GatewayConfig { port: taken }).await {
            Err(GatewayError::PortInUse(p)) => assert_eq!(p, taken),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("不应该成功"),
        }
        first.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokens_are_random_per_gateway_and_launch_urls_carry_them() {
        let a = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let b = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let id = AppId::new("excalidraw").unwrap();
        let (ua, ub) = (a.launch_url(&id), b.launch_url(&id));
        assert_ne!(a.state.token, b.state.token);
        assert_eq!(a.state.token.len(), 64);
        assert!(ua.contains(&format!("bh_token={}", a.state.token)));
        assert_ne!(ua, ub);
        assert!(!a.has_site(&id));
        a.shutdown().await;
        b.shutdown().await;
    }

    /// `Notify::notify_waiters` 只唤醒"此刻正在等"的任务:如果 `shutdown` 在 accept 循环第一次被轮询之前
    /// 就调用(`current_thread` 运行时里必然如此),通知会丢失,`task.await` 永远等下去。
    #[tokio::test]
    async fn shutdown_never_hangs_even_when_called_before_the_accept_loop_first_runs() {
        for _ in 0..20 {
            let gw = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), gw.shutdown())
                .await
                .expect("shutdown 必须返回");
        }
    }

    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    fn raw(port: u16) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    /// 读到对端关闭(EOF 或连接被重置)为止;返回读到的字节数。读超时算测试失败。
    fn read_until_closed(s: &mut TcpStream) -> usize {
        let mut buf = Vec::new();
        match s.read_to_end(&mut buf) {
            Ok(_) => buf.len(),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => buf.len(),
            Err(e) => panic!("连接没有被服务端关闭: {e}"),
        }
    }

    /// 网页可以让浏览器对 `rN.localhost:端口` 开大量连接;半截请求头、空闲的 keep-alive 连接都不能永远占着 fd。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_that_never_finishes_its_headers_is_cut_off() {
        let limits = Limits {
            header_read_timeout: Duration::from_millis(300),
            max_connections: 64,
        };
        let gw = Gateway::start_with_limits(GatewayConfig { port: 0 }, limits)
            .await
            .unwrap();
        let mut s = raw(gw.port());
        s.write_all(b"GET / HTTP/1.1\r\nHost: x").unwrap();
        let started = Instant::now();
        read_until_closed(&mut s);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "半截请求头必须被超时切断"
        );
        gw.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keep_alive_is_off_so_idle_connections_do_not_pile_up() {
        let gw = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let mut s = raw(gw.port());
        // 不带 `Connection: close`:服务端回完应答后必须自己关闭连接
        let id = AppId::new("ghost").unwrap();
        let host = format!("{id}.localhost:{}", gw.port());
        s.write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .unwrap();
        let n = read_until_closed(&mut s);
        assert!(n > 0, "先收到了应答(403),然后连接被关闭");
        gw.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_beyond_the_cap_are_dropped_immediately() {
        let limits = Limits {
            header_read_timeout: Duration::from_secs(10),
            max_connections: 2,
        };
        let gw = Gateway::start_with_limits(GatewayConfig { port: 0 }, limits)
            .await
            .unwrap();
        let (_hold1, _hold2) = (raw(gw.port()), raw(gw.port()));
        std::thread::sleep(Duration::from_millis(200)); // 让两个空闲连接先占住名额
        let mut third = raw(gw.port());
        let _ = third.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        let started = Instant::now();
        assert_eq!(
            read_until_closed(&mut third),
            0,
            "超出上限的连接什么都收不到"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        gw.shutdown().await;
    }
}
