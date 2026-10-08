//! 应用 webview 的纯安全策略(bytehost A4b1 / A7 task 3):应用代码不可信(第三方或 agent 生成),
//! 这里只放与界面框架无关、wry 宿主(dozer)与 Tauri 宿主(Digger)都必须照做的判断:
//!
//! - 这个 URL 是否仍在该应用自己的 origin 内([`AppOrigin::allows_navigation`],导航策略的唯一真相);
//! - 每应用的 WKWebView 数据存储标识([`data_store_identifier`]);
//! - IPC 消息白名单与 nonce([`AppIpc`]、[`new_ipc_nonce`]、[`app_init_script`]);
//! - 卸载"连数据一起删"时数据存储清除的排队([`StoreRemovals`])与系统版本门槛([`supports_store_removal`])。
//!
//! **不**依赖 wry/iced/objc2/tauri:真正创建 webview、取系统版本号、画界面都在宿主里。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 一个应用的 origin(`http://<host>:<port>`)。从应用的**站点地址**解析(启动地址里带的令牌查询串不影响)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppOrigin {
    host: String,
    port: u16,
}

impl AppOrigin {
    /// 只接受 `http://<id>.localhost:<端口>/…`:主机必须是 `<合法 app id>.localhost`,端口必须显式给出,
    /// 不允许用户名/密码。其余(含 `https`、裸 `localhost`、IP)一律 `None`——应用只会由 gateway 以这个形状发布。
    pub fn from_url(url: &str) -> Option<Self> {
        let parsed = url::Url::parse(url).ok()?;
        if parsed.scheme() != "http" || !parsed.username().is_empty() || parsed.password().is_some()
        {
            return None;
        }
        let host = parsed.host_str()?.to_ascii_lowercase();
        let id = host.strip_suffix(".localhost")?;
        if bytehost_apps::id::AppId::new(id).is_err() {
            return None;
        }
        Some(Self {
            host,
            port: parsed.port()?,
        })
    }

    /// 应用 id(主机名 `<id>.localhost` 去掉后缀)。
    pub fn app_id(&self) -> &str {
        self.host.strip_suffix(".localhost").unwrap_or(&self.host)
    }

    /// 导航策略:**只放行本 origin**。wry 的回调拿不到"主框架还是子框架",对每一次导航都问,所以子框架同样适用:
    /// 本 origin 的 `http` 地址、`about:blank`/`about:srcdoc`(应用常用的空 iframe)、
    /// 以及本 origin 创建的 `blob:`。其余一律拒绝(别的站点、`file:`、`dozer:`、`data:`、`javascript:` ……)。
    pub fn allows_navigation(&self, url: &str) -> bool {
        if url == "about:blank" || url == "about:srcdoc" {
            return true;
        }
        let inner = url.strip_prefix("blob:").unwrap_or(url);
        let Ok(parsed) = url::Url::parse(inner) else {
            return false;
        };
        parsed.scheme() == "http"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed
                .host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&self.host))
            && parsed.port() == Some(self.port)
    }
}

/// 每应用的 WKWebView 数据存储标识(macOS 14+ 的 `WKWebsiteDataStore(forIdentifier:)`):由应用 id 确定性
/// 派生,**不能改算法**——改了所有应用的本地数据都会"消失"(测试钉死了一个向量)。FNV-1a 128 位,无新依赖。
pub fn data_store_identifier(app_id: &str) -> [u8; 16] {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut h = OFFSET;
    for b in b"bytehost-app:".iter().chain(app_id.as_bytes()) {
        h ^= u128::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h.to_be_bytes()
}

/// 应用 webview 允许发给宿主的 IPC 消息(白名单)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppIpc {
    Focus,
    MouseUp,
    ZoomIn,
    ZoomOut,
    ZoomReset,
}

impl AppIpc {
    /// 消息体必须是 `<nonce>:<动词>`:nonce 是本 webview 创建时随机生成、只写进注入脚本闭包的,页面脚本拿不到,
    /// 所以页面**不能**自己 `window.ipc.postMessage(..)` 伪造焦点/缩放(抢终端键盘、狂刷全局缩放)。
    /// 其余一切(没有 nonce、nonce 不对、未知动词、多余字段)一律 `None`。
    pub fn parse(body: &str, nonce: &str) -> Option<Self> {
        let verb = body.strip_prefix(nonce)?.strip_prefix(':')?;
        Some(match verb {
            "focus" => Self::Focus,
            "mouseup" => Self::MouseUp,
            "zoom_in" => Self::ZoomIn,
            "zoom_out" => Self::ZoomOut,
            "zoom_reset" => Self::ZoomReset,
            _ => return None,
        })
    }
}

/// 每个应用 webview 一个新 nonce(122 位随机,32 个十六进制字符,可安全嵌进脚本字面量)。
pub fn new_ipc_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 注入脚本(在页面脚本之前运行):先把 `postMessage` 绑定到局部变量——页面之后改写
/// `window.ipc.postMessage` 也截不到 nonce——再只转发 **`isTrusted` 的真实用户事件**
/// (页面脚本 `dispatchEvent` 合成的事件 `isTrusted` 为假)。转发的内容只有焦点/拖拽松开/缩放三类。
pub fn app_init_script(nonce: &str) -> String {
    format!(
        "(function(){{var post=window.ipc.postMessage.bind(window.ipc);var N='{nonce}';\
function send(v){{post(N+':'+v)}}\
document.addEventListener('mousedown',function(e){{if(e.isTrusted)send('focus')}},true);\
document.addEventListener('mouseup',function(e){{if(e.isTrusted)send('mouseup')}},true);\
document.addEventListener('keydown',function(e){{if(!e.isTrusted||!e.ctrlKey)return;var c=e.code,k=e.key;\
if(c==='Equal'||k==='+'||k==='='){{e.preventDefault();send('zoom_in')}}\
else if(c==='Minus'||k==='-'){{e.preventDefault();send('zoom_out')}}\
else if(c==='Digit1'||k==='1'){{e.preventDefault();send('zoom_reset')}}}},true)}})();"
    )
}

/// 清除失败后重试的间隔与次数上限:丢掉 Rust 侧的 `WebView` 句柄不等于 WebKit 已经放开数据存储
/// (WebContent 进程异步退出),刚丢掉就清可能返回 `DataStoreInUse`,隔一会儿再试通常就行。
const STORE_RETRY_DELAY: Duration = Duration::from_secs(1);
const STORE_MAX_ATTEMPTS: u8 = 5;

/// `WebView::remove_data_store`(`WKWebsiteDataStore.removeDataStore(forIdentifier:)`)只在 **macOS 14+** 存在;
/// wry 自己**不检查**系统版本(创建带存储的 webview 时才检查),在 12/13 上直接调会因"未知选择器"让整个进程崩掉
/// (`Info.plist` 的 `LSMinimumSystemVersion` 是 12.0)。所以调用前必须先过这个判断。
pub fn supports_store_removal(os_major: isize) -> bool {
    os_major >= 14
}

/// 一次清除的结果(由窗口层经 `Message::AppStoreRemoval` 送回 `App`)。
#[derive(Debug, Clone, PartialEq)]
pub enum StoreRemovalOutcome {
    Done,
    /// 数据存储还被占用(webview 刚丢掉,WebKit 还没放开)——可重试。
    InUse,
    /// 系统版本不支持(< macOS 14)。
    Unsupported,
    Failed(String),
}

#[derive(Debug)]
struct PendingRemoval {
    id: String,
    /// 重试要等到的时刻;`None` = 随时可以(只要 webview 已不在池里)。
    not_before: Option<Instant>,
}

/// 待清除的应用 WKWebsiteDataStore(卸载"连数据一起删"时用)。**清除前必须先让使用它的 webview 离开池**
/// (wry:`remove_data_store` 要求先 drop 所有用这个存储的 WebView),所以这里只排队,
/// 窗口层每帧在 `sync_webview_pool` 之后调 [`StoreRemovals::take_ready`],只放行"池里已经没有它的 webview"
/// 且已到重试时刻的那些。
#[derive(Debug, Default)]
pub struct StoreRemovals {
    pending: Vec<PendingRemoval>,
    /// 每个已请求、尚未结束的应用已失败重试了几次(`finish`/放弃时移除)。
    attempts: HashMap<String, u8>,
}

impl StoreRemovals {
    /// 排队清除某应用的数据存储(同一个应用重复排队只留一份)。
    pub fn request(&mut self, app_id: &str) {
        if self.attempts.contains_key(app_id) {
            return;
        }
        self.attempts.insert(app_id.to_owned(), 0);
        self.pending.push(PendingRemoval {
            id: app_id.to_owned(),
            not_before: None,
        });
    }

    /// 取走已经可以清除的。`(应用 id, 存储标识)`。放行后条目离开队列;结果回来时调 `finish` 或 `retry`。
    /// `webview_in_pool(app_id)` 由宿主提供:池里是否还有这个应用的 webview(**没有槽=没有 webview**)。
    pub fn take_ready(
        &mut self,
        now: Instant,
        webview_in_pool: impl Fn(&str) -> bool,
    ) -> Vec<(String, [u8; 16])> {
        let mut ready = Vec::new();
        self.pending.retain(|p| {
            if p.not_before.is_some_and(|t| now < t) {
                return true; // 重试还没到点
            }
            if webview_in_pool(&p.id) {
                return true; // webview 还在池里,等下一帧
            }
            ready.push((p.id.clone(), data_store_identifier(&p.id)));
            false
        });
        ready
    }

    /// 清除成功(或不会再重试了):忘掉这个应用的尝试计数。
    pub fn finish(&mut self, app_id: &str) {
        self.attempts.remove(app_id);
    }

    /// 清除失败(`DataStoreInUse`)后重新排队,`STORE_RETRY_DELAY` 之后再试;次数用完(或这个应用根本没有在途请求)
    /// 返回 `false`——调用方要告诉用户,不能悄悄放弃。
    pub fn retry(&mut self, app_id: &str, now: Instant) -> bool {
        let Some(n) = self.attempts.get_mut(app_id) else {
            return false;
        };
        *n += 1;
        if *n >= STORE_MAX_ATTEMPTS {
            self.attempts.remove(app_id);
            return false;
        }
        self.pending.push(PendingRemoval {
            id: app_id.to_owned(),
            not_before: Some(now + STORE_RETRY_DELAY),
        });
        true
    }

    /// 距最近一次重试还有多久(没有等待中的重试返回 `None`)——`about_to_wait` 据此排唤醒。
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        self.pending
            .iter()
            .filter_map(|p| p.not_before)
            .map(|t| t.saturating_duration_since(now))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钉死:本 crate 与 dozer 宿主的 `valid_app_id` 是同一口径(origin 主机名校验从前者换成后者前先验证一致)。
    #[test]
    fn valid_app_id_matches_appid_new() {
        for ok in [
            "a",
            "excalidraw",
            "my-app-2",
            "0day",
            "a--b",
            &"a".repeat(63),
        ] {
            assert!(bytehost_apps::id::AppId::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "A",
            "UPPER",
            "under_score",
            "-lead",
            "trail-",
            "with.dot",
            "with space",
            "中文",
            &"a".repeat(64),
        ] {
            assert!(bytehost_apps::id::AppId::new(bad).is_err(), "{bad}");
        }
    }

    fn origin() -> AppOrigin {
        AppOrigin::from_url("http://excalidraw.localhost:20001/?bh_token=secret").unwrap()
    }

    #[test]
    fn origin_is_parsed_only_from_the_gateway_shaped_address() {
        assert_eq!(
            origin(),
            AppOrigin::from_url("http://EXCALIDRAW.localhost:20001/other").unwrap(),
            "大小写与路径/查询串不影响 origin"
        );
        for bad in [
            "https://excalidraw.localhost:20001/",
            "http://excalidraw.localhost/",
            "http://localhost:20001/",
            "http://127.0.0.1:20001/",
            "http://Bad_Id.localhost:20001/",
            "http://-a.localhost:20001/",
            "http://u:p@excalidraw.localhost:20001/",
            "dozer://flyfish/host.html",
            "not a url",
        ] {
            assert!(AppOrigin::from_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_app_id_is_the_host_without_the_localhost_suffix() {
        assert_eq!(origin().app_id(), "excalidraw");
    }

    #[test]
    fn navigation_stays_inside_the_apps_own_origin() {
        let o = origin();
        for ok in [
            "http://excalidraw.localhost:20001/",
            "http://excalidraw.localhost:20001/a/b?x=1#h",
            "HTTP://Excalidraw.Localhost:20001/up",
            "about:blank",
            "about:srcdoc",
            "blob:http://excalidraw.localhost:20001/6f1c",
        ] {
            assert!(o.allows_navigation(ok), "{ok}");
        }
        for bad in [
            "http://excalidraw.localhost:20002/",
            "http://other.localhost:20001/",
            "http://excalidraw.localhost.evil.com:20001/",
            "http://evil.com/",
            "https://excalidraw.localhost:20001/",
            "http://u:p@excalidraw.localhost:20001/",
            "http://excalidraw.localhost@evil.com:20001/",
            "file:///etc/passwd",
            "dozer://flyfish/host.html",
            "data:text/html,<script>1</script>",
            "javascript:alert(1)",
            "blob:http://evil.com/6f1c",
            "blob:https://excalidraw.localhost:20001/6f1c",
            "about:config",
            "",
        ] {
            assert!(!o.allows_navigation(bad), "{bad}");
        }
    }

    /// 钉死:这个标识决定每个应用的持久存储,算法一变所有应用数据都"丢"。
    #[test]
    fn the_data_store_identifier_is_stable_and_per_app() {
        assert_eq!(
            data_store_identifier("excalidraw"),
            // 同一算法的独立实现(Python)算出的值:30cd3bc2160471a7ee03a52d8c2e59db
            [
                0x30, 0xcd, 0x3b, 0xc2, 0x16, 0x04, 0x71, 0xa7, 0xee, 0x03, 0xa5, 0x2d, 0x8c, 0x2e,
                0x59, 0xdb
            ]
        );
        assert_ne!(data_store_identifier("a"), data_store_identifier("b"));
    }

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn ipc_messages_need_the_per_webview_nonce_and_a_known_verb() {
        for (verb, want) in [
            ("focus", AppIpc::Focus),
            ("mouseup", AppIpc::MouseUp),
            ("zoom_in", AppIpc::ZoomIn),
            ("zoom_out", AppIpc::ZoomOut),
            ("zoom_reset", AppIpc::ZoomReset),
        ] {
            assert_eq!(AppIpc::parse(&format!("{NONCE}:{verb}"), NONCE), Some(want));
        }
        for bad in [
            "focus",
            "mouseup",
            "title:1:x",
            "find_native",
            &format!("{NONCE}:"),
            &format!("{NONCE}:format_disk"),
            &format!("{NONCE}x:focus"),
            &format!("x{NONCE}:focus"),
            ":focus",
            &format!("{NONCE}:focus:extra"),
            "wrong-nonce:focus",
            "",
        ] {
            assert_eq!(AppIpc::parse(bad, NONCE), None, "{bad}");
        }
    }

    #[test]
    fn nonces_are_unguessable_and_safe_to_embed_in_a_script() {
        let a = new_ipc_nonce();
        let b = new_ipc_nonce();
        assert_ne!(a, b);
        assert!(
            a.len() >= 32 && a.bytes().all(|c| c.is_ascii_hexdigit()),
            "{a}"
        );
    }

    /// 页面脚本不能伪造 IPC:消息带只有注入脚本闭包知道的 nonce,注入脚本在页面脚本之前
    /// 绑定好 `postMessage`(页面之后改写 `window.ipc.postMessage` 也截不到),且只转发
    /// `isTrusted` 的真实用户事件。
    #[test]
    fn the_init_script_binds_post_message_first_and_forwards_only_trusted_events() {
        let script = app_init_script(NONCE);
        assert!(script.contains(NONCE));
        assert!(script.contains("window.ipc.postMessage.bind(window.ipc)"));
        assert!(
            !script.contains("window.ipc.postMessage('"),
            "不得再直接调用页面可改写的 postMessage"
        );
        for listener in ["mousedown", "mouseup", "keydown"] {
            let at = script
                .find(&format!("addEventListener('{listener}'"))
                .unwrap();
            assert!(
                script[at..].contains("isTrusted"),
                "{listener} 监听必须校验 isTrusted"
            );
        }
        assert!(script.matches("isTrusted").count() >= 3);
    }

    #[test]
    fn store_removal_waits_until_the_apps_webview_has_left_the_pool() {
        let mut q = StoreRemovals::default();
        let now = Instant::now();
        q.request("purge-a");
        q.request("purge-a");
        assert!(
            q.take_ready(now, |id| id == "purge-a").is_empty(),
            "webview 还在池里:不能清"
        );
        let ready = q.take_ready(now, |_| false);
        assert_eq!(
            ready,
            vec![("purge-a".to_string(), data_store_identifier("purge-a"))]
        );
        assert!(q.take_ready(now, |_| false).is_empty(), "只清一次");
    }

    #[test]
    fn store_removal_only_releases_apps_that_are_ready_and_keeps_the_rest_queued() {
        let mut q = StoreRemovals::default();
        let now = Instant::now();
        q.request("purge-busy");
        q.request("purge-free");
        let ready = q.take_ready(now, |id| id == "purge-busy");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, "purge-free");
        assert_eq!(q.take_ready(now, |_| false).len(), 1, "busy 的下一帧放行");
    }

    /// 刚丢掉 webview 就清可能 `DataStoreInUse`:隔 `STORE_RETRY_DELAY` 重试,最多 `STORE_MAX_ATTEMPTS` 次,
    /// 用完返回 `false` 让调用方告诉用户(不能悄悄放弃)。
    #[test]
    fn a_failed_removal_is_retried_after_a_delay_and_then_given_up_on() {
        let mut q = StoreRemovals::default();
        let t0 = Instant::now();
        q.request("retry-a");
        assert_eq!(q.take_ready(t0, |_| false).len(), 1);
        for attempt in 1..STORE_MAX_ATTEMPTS {
            assert!(q.retry("retry-a", t0), "第 {attempt} 次失败后应重新排队");
            assert!(q.take_ready(t0, |_| false).is_empty(), "没到重试时刻");
            assert_eq!(q.next_wake(t0), Some(STORE_RETRY_DELAY));
            assert_eq!(
                q.take_ready(t0 + STORE_RETRY_DELAY, |_| false).len(),
                1,
                "到点放行"
            );
        }
        assert!(!q.retry("retry-a", t0), "次数用完:放弃");
        assert!(
            q.take_ready(t0 + STORE_RETRY_DELAY * 9, |_| false)
                .is_empty()
        );
        assert_eq!(q.next_wake(t0), None);
    }

    #[test]
    fn retrying_an_unknown_removal_does_nothing() {
        let mut q = StoreRemovals::default();
        assert!(!q.retry("never-requested", Instant::now()));
    }

    /// wry 不检查系统版本,12/13 上调 `remove_data_store` 会崩进程。
    #[test]
    fn store_removal_is_only_attempted_on_macos_14_and_later() {
        assert!(!supports_store_removal(12));
        assert!(!supports_store_removal(13));
        assert!(supports_store_removal(14));
        assert!(supports_store_removal(26));
    }
}
