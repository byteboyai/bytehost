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

mod proxy;
mod static_files;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
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
    /// 反向代理:连应用的回环端口最多等多久(超时 502)。
    pub upstream_connect_timeout: std::time::Duration,
    /// 反向代理:连上之后等应用给出**响应头**最多多久(超时 504);响应正文开始流动之后不再有总超时(SSE/长轮询)。
    pub upstream_header_timeout: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_read_timeout: std::time::Duration::from_secs(10),
            max_connections: 128,
            upstream_connect_timeout: std::time::Duration::from_secs(3),
            upstream_header_timeout: std::time::Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GatewayConfig {
    /// 监听端口(127.0.0.1)。`0` 让系统分配——只给测试用;生产用 `port::load_port`/`pick_port` 的值。
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
    /// 进程型应用:`<app-id>` → 应用监听的回环地址(反向代理的目标)。与 `sites` 互斥(同一个应用只有一种)。
    upstreams: RwLock<HashMap<String, SocketAddr>>,
}

impl State {
    fn upstreams_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, SocketAddr>> {
        self.upstreams
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn upstreams_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, SocketAddr>> {
        self.upstreams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 站点表的读/写锁:持锁线程 panic 会毒化它,但表里没有需要保持的不变量,
    /// 后续请求不能因此全部 panic——取回内部数据继续用。
    fn sites_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Site>> {
        self.sites
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn sites_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Site>> {
        self.sites
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 按应用派生的凭证:`sha256(主令牌, app id)`。主令牌(每次 gateway 启动随机生成)**从不出进程**;
/// 启动地址与 Cookie 里放的都是派生值,所以某个应用的页面/Service Worker 看到的凭证只对它自己的主机名有效。
fn app_token(master: &str, app: &AppId) -> String {
    crate::digest::sha256_hex(format!("bytehost-gateway-token\0{master}\0{app}").as_bytes())
}

/// 请求形状的硬性要求:重复的 `Host` 头、绝对形式的请求目标(`GET http://evil/ …`)都是 400——
/// 前者会让"校验的 Host"与"实际路由的 Host"不一致,后者按 RFC 7230 §5.4 本应以请求目标里的 authority 为准。
fn request_shape_problem(host_headers: usize, absolute_form: bool) -> Option<StatusCode> {
    (host_headers > 1 || absolute_form).then_some(StatusCode::BAD_REQUEST)
}

/// 运行中的 gateway。`Drop` 不会停服务,要停请 [`Gateway::shutdown`]。
pub struct Gateway {
    state: Arc<State>,
    shutdown: Arc<Notify>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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
            upstreams: RwLock::new(HashMap::new()),
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
            task: std::sync::Mutex::new(Some(task)),
        })
    }

    pub fn port(&self) -> u16 {
        self.state.port
    }

    /// 注册(或替换)一个应用的静态站点。
    pub fn add_site(&self, id: &AppId, root: PathBuf, csp: Option<String>) {
        self.state.upstreams_write().remove(id.as_str());
        self.state
            .sites_write()
            .insert(id.as_str().to_string(), Site { root, csp });
    }

    /// 注册(或替换)一个进程型应用:请求通过校验后反向代理到 `upstream`(应用监听的 `127.0.0.1:<端口>`)。
    /// 若这个 id 之前是静态站点,会被替换。
    pub fn add_upstream(&self, id: &AppId, upstream: SocketAddr) {
        self.state.sites_write().remove(id.as_str());
        self.state
            .upstreams_write()
            .insert(id.as_str().to_string(), upstream);
    }

    /// 注销站点或上游;返回之前是否注册过。
    pub fn remove_site(&self, id: &AppId) -> bool {
        let was_static = self.state.sites_write().remove(id.as_str()).is_some();
        let was_proxy = self.state.upstreams_write().remove(id.as_str()).is_some();
        was_static || was_proxy
    }

    /// `stop`/`shutdown` 已经完成(accept 循环已退出)。测试与上层用它判断"这个 gateway 确实停了",
    /// 而不是靠"再连一次端口"——端口一释放就可能被别的程序(或别的测试)立刻重新占用,连接探测会误判。
    pub fn is_stopped(&self) -> bool {
        self.task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    }

    pub fn has_site(&self, id: &AppId) -> bool {
        self.state.sites_read().contains_key(id.as_str())
            || self.state.upstreams_read().contains_key(id.as_str())
    }

    /// 不含令牌的站点地址(可以放进事件/日志)。
    pub fn site_url(&self, id: &AppId) -> String {
        format!("http://{id}.localhost:{}/", self.state.port)
    }

    /// 首次导航用的地址:带用来换 Cookie 的**按应用派生**的令牌(可重复使用,不是一次性的)。
    /// **这是秘密,不要写进日志或广播事件。**
    pub fn launch_url(&self, id: &AppId) -> String {
        format!(
            "{}?{TOKEN_PARAM}={}",
            self.site_url(id),
            app_token(&self.state.token, id)
        )
    }

    /// 停止接受新连接并等 accept 循环退出。**幂等**,且只要 `&self`——`AppManager` 与 dozerd 都持有 `Arc<Gateway>`,
    /// 没法交出所有权调用 [`Gateway::shutdown`]。已建立的连接不会被主动掐断。
    pub async fn stop(&self) {
        // notify_one 会存一个许可:即使 accept 循环还没跑到 `notified()`,稍后也能收到(notify_waiters 会丢)
        self.shutdown.notify_one();
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub async fn shutdown(self) {
        self.stop().await;
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
                            if let Some(status) = request_shape_problem(
                                req.headers().get_all(header::HOST).iter().count(),
                                req.uri().authority().is_some(),
                            ) {
                                return Ok::<_, Infallible>(proxy::boxed_reply(plain(
                                    status,
                                    "bad request",
                                )));
                            }
                            let method = req.method().clone();
                            let host = header_str(&req, header::HOST);
                            let cookie = header_str(&req, header::COOKIE);
                            let target = req
                                .uri()
                                .path_and_query()
                                .map(|p| p.as_str().to_string())
                                .unwrap_or_else(|| "/".to_string());
                            let routing_state = state.clone();
                            let routed = tokio::task::spawn_blocking(move || {
                                route(
                                    &routing_state,
                                    &method,
                                    host.as_deref(),
                                    &target,
                                    cookie.as_deref(),
                                )
                            })
                            .await
                            .unwrap_or_else(|_| {
                                Routed::Reply(plain(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "internal error",
                                ))
                            });
                            let reply = match routed {
                                Routed::Reply(r) => proxy::boxed_reply(r),
                                Routed::Proxy(upstream) => {
                                    let own_origin = format!(
                                        "{}://{}",
                                        "http",
                                        header_str(&req, header::HOST).unwrap_or_default()
                                    );
                                    proxy::forward(
                                        req,
                                        upstream,
                                        &own_origin,
                                        COOKIE_NAME,
                                        limits.upstream_connect_timeout,
                                        limits.upstream_header_timeout,
                                    )
                                    .await
                                }
                            };
                            Ok::<_, Infallible>(reply)
                        }
                    });
                    // 每个应答之后就关连接(本机静态站点不需要 keep-alive),并给读请求头设超时
                    let mut http = hyper::server::conn::http1::Builder::new();
                    http.timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(limits.header_read_timeout)
                        .keep_alive(false);
                    let _ = http
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
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

/// 同名 Cookie 的**所有**值(同名 Cookie 可能出现多次,不能只看第一个)。
fn cookie_values<'a>(header: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> {
    header.split(';').filter_map(move |part| {
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

/// 认证之后的去向。
enum Routed {
    /// 直接答复(静态文件、重定向、各种错误)。
    Reply(Reply),
    /// 反向代理到进程型应用监听的回环地址(含 WebSocket 升级)。
    Proxy(SocketAddr),
}

/// 步骤 1–2:Host 与令牌。要么直接得到一个答复(421/403/302),要么得到通过校验的应用与拆好的路径/查询。
enum Authed<'a> {
    Done(Reply),
    App {
        app: AppId,
        path: &'a str,
        query: Option<&'a str>,
    },
}

fn authenticate<'a>(
    state: &State,
    host: Option<&str>,
    target: &'a str,
    cookie: Option<&str>,
) -> Authed<'a> {
    // 1. Host
    let Some(app) = app_from_host(host, state.port) else {
        return Authed::Done(plain(
            StatusCode::MISDIRECTED_REQUEST,
            "misdirected request",
        ));
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    // 2. 令牌(按应用派生)
    let expected = app_token(&state.token, &app);
    let (query_token, rest_query) = split_token(query);
    if let Some(t) = &query_token {
        if !ct_eq(t, &expected) {
            return Authed::Done(plain(StatusCode::FORBIDDEN, "forbidden"));
        }
        // Location 永远是站内路径:开头的多个 `/`、`\\` 折成一个(`//evil.com/` 会被浏览器当成另一个 origin)
        let local_path = format!("/{}", path.trim_start_matches(['/', '\\']));
        let location = match &rest_query {
            Some(q) => format!("{local_path}?{q}"),
            None => local_path,
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
                "{COOKIE_NAME}={expected}; HttpOnly; SameSite=Strict; Path=/"
            ))
            .expect("令牌是十六进制"),
        );
        harden(r.headers_mut());
        return Authed::Done(r);
    }
    let authed = cookie.is_some_and(|c| cookie_values(c, COOKIE_NAME).any(|v| ct_eq(v, &expected)));
    if !authed {
        return Authed::Done(plain(StatusCode::FORBIDDEN, "forbidden"));
    }
    Authed::App { app, path, query }
}

/// 处理一个请求:纯同步、不碰网络(反向代理的转发在 `proxy::forward`,这里只决定"去哪");`target` 是请求行里的 path+query。
fn route(
    state: &State,
    method: &Method,
    host: Option<&str>,
    target: &str,
    cookie: Option<&str>,
) -> Routed {
    let (app, path, query) = match authenticate(state, host, target, cookie) {
        Authed::Done(reply) => return Routed::Reply(reply),
        Authed::App { app, path, query } => (app, path, query),
    };
    // 3. 站点:进程型应用走反向代理(不限方法——应用有自己的 API;写方法/WebSocket 的同源检查在 `proxy::forward`)
    if let Some(addr) = state.upstreams_read().get(app.as_str()).copied() {
        return Routed::Proxy(addr);
    }
    Routed::Reply(serve_static(state, &app, method, path, query))
}

/// 静态站点的步骤 3–5:站点存在、方法、路径解析。
fn serve_static(
    state: &State,
    app: &AppId,
    method: &Method,
    path: &str,
    query: Option<&str>,
) -> Reply {
    let Some(site) = state.sites_read().get(app.as_str()).cloned() else {
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
        Resolved::DirRedirect => {
            let local_path = format!("/{}/", path.trim_matches(['/', '\\']));
            let location = match query {
                Some(q) => format!("{local_path}?{q}"),
                None => local_path,
            };
            let mut r = Response::new(Full::new(Bytes::new()));
            *r.status_mut() = StatusCode::MOVED_PERMANENTLY;
            r.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
            );
            harden(r.headers_mut());
            r
        }
    }
}

/// 测试辅助:只看直接答复(静态路径与各种错误);进程型应用的路由结果在测试里单独断言。
#[cfg(test)]
fn respond(
    state: &State,
    method: &Method,
    host: Option<&str>,
    target: &str,
    cookie: Option<&str>,
) -> Reply {
    match route(state, method, host, target, cookie) {
        Routed::Reply(r) => r,
        Routed::Proxy(_) => plain(StatusCode::BAD_GATEWAY, "proxied"),
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
    if let Some(csp) = &site.csp {
        // 配了 CSP 却发不出去就必须失败:静默不带 CSP 等于把本该受限的页面放了出去
        match HeaderValue::from_str(csp) {
            Ok(v) => {
                h.insert(header::CONTENT_SECURITY_POLICY, v);
            }
            Err(_) => {
                return plain(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid content security policy",
                );
            }
        }
    }
    r
}

#[cfg(test)]
mod proxy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{NetworkPerm, Outbound};
    use crate::testutil::{Req, send_raw, write_files};

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
            upstreams: RwLock::new(HashMap::new()),
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

    fn tok_for(app: &str) -> String {
        app_token(TOKEN, &AppId::new(app).unwrap())
    }

    fn cookie_for(app: &str) -> String {
        format!("bh_session={}", tok_for(app))
    }

    static COOKIE: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| cookie_for("excalidraw"));

    #[test]
    fn the_host_header_must_be_exactly_a_valid_app_dot_localhost_with_the_gateways_port() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        assert_eq!(
            get(
                &st,
                Some("excalidraw.localhost:5000"),
                "/",
                Some(COOKIE.as_str())
            ),
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
                get(&st, bad, "/", Some(COOKIE.as_str())),
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
            get(
                &st,
                Some("ghost.localhost:5000"),
                "/",
                Some(cookie_for("ghost").as_str())
            ),
            StatusCode::NOT_FOUND
        );
        // 多个 cookie 里取对的那个
        assert_eq!(
            get(
                &st,
                host,
                "/",
                Some(format!("a=1; {}; b=2", COOKIE.as_str()).as_str())
            ),
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
            &format!("/app.js?x=1&bh_token={}&y=2", tok_for("excalidraw")),
            None,
        );
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(r.headers()[header::LOCATION], "/app.js?x=1&y=2");
        let cookie = r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cookie.starts_with(&format!("bh_session={};", tok_for("excalidraw"))),
            "{cookie}"
        );
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/"] {
            assert!(cookie.contains(attr), "{cookie}");
        }
        assert!(
            !cookie.to_ascii_lowercase().contains("domain"),
            "host-only,不设 Domain:{cookie}"
        );
        // 只有令牌时 Location 就是路径本身
        let r = respond(
            &st,
            &Method::GET,
            host,
            &format!("/?bh_token={}", tok_for("excalidraw")),
            None,
        );
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
            &format!("/?bh_token={}", tok_for("excalidraw")),
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
            Some(COOKIE.as_str()),
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
            Some(COOKIE.as_str()),
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
            let s = get(
                &st,
                Some("excalidraw.localhost:5000"),
                target,
                Some(COOKIE.as_str()),
            );
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
                Some(COOKIE.as_str())
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
            let r = respond(&st, &m, host, "/", Some(COOKIE.as_str()));
            assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED, "{m}");
            assert_eq!(r.headers()[header::ALLOW], "GET, HEAD");
        }
        let head = respond(&st, &Method::HEAD, host, "/", Some(COOKIE.as_str()));
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
            Some(COOKIE.as_str()),
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
            cookie_values("a=1; bh_session=x; b=2", "bh_session").collect::<Vec<_>>(),
            ["x"]
        );
        assert_eq!(
            cookie_values("bh_session=x; bh_session=y", "bh_session").collect::<Vec<_>>(),
            ["x", "y"],
            "同名 Cookie 的所有值都要给出来"
        );
        assert_eq!(cookie_values("xbh_session=x", "bh_session").count(), 0);
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
        // 令牌是按应用派生的:每个应用只认自己的 Cookie
        let cookie_a = format!("bh_session={}", app_token(&gw.state.token, &a));
        let cookie_b = format!("bh_session={}", app_token(&gw.state.token, &b));
        let cookie = cookie_a.clone();

        let ra = Req::get(port, &format!("excalidraw.localhost:{port}"), "/")
            .cookie(&cookie)
            .send();
        assert_eq!((ra.status, ra.text().contains("excalidraw")), (200, true));
        let rb = Req::get(port, &format!("drawio.localhost:{port}"), "/")
            .cookie(&cookie_b)
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
            .cookie(&cookie_b)
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
        // launch_url 带的是**按应用派生**的令牌,主令牌本身永远不出进程
        assert!(ua.contains(&format!("bh_token={}", app_token(&a.state.token, &id))));
        assert!(!ua.contains(&a.state.token), "主令牌不能出现在 URL 里");
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
            ..Limits::default()
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
            ..Limits::default()
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

    /// `stop` 只要 `&self`、可以重复调用;完成之后 `is_stopped()` 为真。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_is_idempotent_and_marks_the_gateway_stopped() {
        let gw = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        assert!(!gw.is_stopped());
        gw.stop().await;
        assert!(gw.is_stopped());
        gw.stop().await;
        assert!(gw.is_stopped());
        gw.shutdown().await;
    }

    #[test]
    fn a_cookie_or_token_for_one_app_does_not_work_on_another_app() {
        let (_tmp, root) = site_dir();
        let st = state_with(
            5000,
            &[("excalidraw", &root, None), ("drawio", &root, None)],
        );
        let (ex, dr) = (
            Some("excalidraw.localhost:5000"),
            Some("drawio.localhost:5000"),
        );
        assert_eq!(
            get(&st, ex, "/", Some(&cookie_for("excalidraw"))),
            StatusCode::OK
        );
        assert_eq!(
            get(&st, dr, "/", Some(&cookie_for("excalidraw"))),
            StatusCode::FORBIDDEN
        );
        let r = respond(
            &st,
            &Method::GET,
            dr,
            &format!("/?bh_token={}", tok_for("excalidraw")),
            None,
        );
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(r.headers().get(header::SET_COOKIE).is_none());
        assert_ne!(tok_for("excalidraw"), tok_for("drawio"));
        assert_eq!(tok_for("excalidraw").len(), 64);
        assert_ne!(tok_for("excalidraw"), TOKEN, "主令牌本身从不当作凭证");
    }

    /// 同名 Cookie 出现多次(例如被兄弟站点"cookie 投毒"):只要有一个值对就放行,不能只看第一个。
    #[test]
    fn any_matching_bh_session_value_is_enough() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let host = Some("excalidraw.localhost:5000");
        let both = format!("bh_session=junk; {}", COOKIE.as_str());
        assert_eq!(get(&st, host, "/", Some(&both)), StatusCode::OK);
        assert_eq!(
            get(&st, host, "/", Some("bh_session=junk; bh_session=junk2")),
            StatusCode::FORBIDDEN
        );
    }

    /// 令牌换 Cookie 的重定向地址永远是站内路径:`//evil.com/` 与 `/\evil.com/` 会被浏览器当成另一个 origin。
    #[test]
    fn redirect_locations_never_become_protocol_relative() {
        let (_tmp, root) = site_dir();
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let host = Some("excalidraw.localhost:5000");
        let t = tok_for("excalidraw");
        for (path, want) in [
            ("//evil.com/x", "/evil.com/x"),
            ("/\\evil.com/", "/evil.com/"),
            ("///a", "/a"),
            ("/\\/b", "/b"),
            ("/ok", "/ok"),
        ] {
            let r = respond(
                &st,
                &Method::GET,
                host,
                &format!("{path}?bh_token={t}"),
                None,
            );
            assert_eq!(r.status(), StatusCode::FOUND, "{path}");
            assert_eq!(r.headers()[header::LOCATION], want, "{path}");
        }
    }

    #[test]
    fn duplicate_host_headers_and_absolute_form_targets_are_bad_requests() {
        assert_eq!(request_shape_problem(1, false), None);
        assert_eq!(
            request_shape_problem(0, false),
            None,
            "缺 Host 由后面的 421 处理"
        );
        assert_eq!(
            request_shape_problem(2, false),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            request_shape_problem(1, true),
            Some(StatusCode::BAD_REQUEST)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_request_shapes_are_rejected_on_the_wire() {
        let (_tmp, root) = site_dir();
        let gw = Gateway::start(GatewayConfig { port: 0 }).await.unwrap();
        let id = AppId::new("excalidraw").unwrap();
        gw.add_site(&id, root, None);
        let port = gw.port();
        let host = format!("excalidraw.localhost:{port}");
        let cookie = format!("bh_session={}", app_token(&gw.state.token, &id));
        let dup = send_raw(
            port,
            &format!(
                "GET / HTTP/1.1\r\nHost: {host}\r\nHost: evil.example\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(dup.status, 400);
        let absolute = send_raw(
            port,
            &format!(
                "GET http://evil.example/ HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(absolute.status, 400);
        let fine = send_raw(
            port,
            &format!(
                "GET / HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(fine.status, 200);
        gw.shutdown().await;
    }

    /// 目录不带结尾斜杠:301 到带斜杠的地址(保留查询串),带斜杠才给 index.html。
    #[test]
    fn a_directory_without_a_trailing_slash_is_redirected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        write_files(
            &root,
            &[("index.html", "home"), ("docs/index.html", "docs")],
        );
        let st = state_with(5000, &[("excalidraw", &root, None)]);
        let host = Some("excalidraw.localhost:5000");
        let r = respond(&st, &Method::GET, host, "/docs", Some(COOKIE.as_str()));
        assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(r.headers()[header::LOCATION], "/docs/");
        let r = respond(&st, &Method::GET, host, "/docs?x=1", Some(COOKIE.as_str()));
        assert_eq!(r.headers()[header::LOCATION], "/docs/?x=1");
        assert_eq!(
            get(&st, host, "/docs/", Some(COOKIE.as_str())),
            StatusCode::OK
        );
        assert_eq!(get(&st, host, "/", Some(COOKIE.as_str())), StatusCode::OK);
    }

    /// CSP 值不能作为头部值时必须失败(500),而不是静默不带 CSP 就把页面放出去。
    #[test]
    fn a_csp_that_cannot_be_a_header_value_fails_closed() {
        let (_tmp, root) = site_dir();
        let st = state_with(
            5000,
            &[("excalidraw", &root, Some("bad\nvalue".to_string()))],
        );
        assert_eq!(
            get(
                &st,
                Some("excalidraw.localhost:5000"),
                "/",
                Some(COOKIE.as_str())
            ),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// 某个线程在持有站点表的锁时 panic:锁被毒化,但后续请求不能跟着全部 panic。
    #[test]
    fn a_poisoned_sites_lock_does_not_take_the_gateway_down() {
        let (_tmp, root) = site_dir();
        let st = std::sync::Arc::new(state_with(5000, &[("excalidraw", &root, None)]));
        let st2 = st.clone();
        let _ = std::thread::spawn(move || {
            let _guard = st2.sites.write().unwrap();
            panic!("故意毒化");
        })
        .join();
        assert!(st.sites.is_poisoned());
        assert_eq!(
            get(
                &st,
                Some("excalidraw.localhost:5000"),
                "/",
                Some(COOKIE.as_str())
            ),
            StatusCode::OK
        );
    }
}
