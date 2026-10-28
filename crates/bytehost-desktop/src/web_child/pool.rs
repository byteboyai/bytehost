//! 浏览器页签池的执行层:多页签 wry 子视图 + 受限 builder。纯规划逻辑在 [`super::plan`]。
//!
//! 所有 wry 调用都在 `iced::window::run` 里(主线程);句柄表是本模块私有的 `thread_local`,与
//! `native_child`、`app_webview` 的句柄表**永不共用**。事件经调用方给的 `Sender<WebEvent>` 送回,
//! 每个事件都带 `generation`,过期的(切项目 / 关项目之后迟到的)由宿主丢弃。
//!
//! **不可信内容**(任意网页,含 `file://`):不注册自定义协议、不注入脚本、不装 IPC 处理器——页面看不到
//! `window.ipc`;导航与新窗口只放行 `bytehost_webview::browse` 的白名单;下载一律拒绝;开发者工具关闭;
//! 媒体捕获由代理委托拒绝(见 `apple.rs`)。这些都由本文件末尾的源码钉死测试守着。

use super::plan::{
    ExistingTab, LoadVerdict, NavTracker, PoolAction, WebTabSpec, plan_sync, wry_target,
};
use crate::native_child::{Probe, inspect, restore_focus};
use bytehost_core::LogicalRect;
use bytehost_webview::browse::{allows_browse_navigation, allows_new_tab};
use iced::{Task, window};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wry::{WebView, WebViewBuilder, WebViewBuilderExtDarwin};

#[cfg(target_os = "macos")]
use super::apple::{Guard, install_media_deny_proxy};
#[cfg(not(target_os = "macos"))]
type Guard = ();
#[cfg(not(target_os = "macos"))]
fn install_media_deny_proxy(_: &WebView) -> Guard {}

/// 浏览器页面专用的 WKWebsiteDataStore 标识:与应用 webview(每应用一个)、其它可信子视图(默认存储)
/// **隔离**,网页的 cookie / localStorage 不会和产品自己的页面混在一起。持久(登录态保留)。
/// **改这个值等于让所有用户丢掉浏览器里的登录态**,发布后不得再动。
const BROWSER_DATA_STORE_ID: [u8; 16] = *b"bytehost-browser";

/// 发出导航后多久没有开始就报告 [`WebEvent::NavigationStalled`]。wry 的 `Started` 对应"服务器已响应、页面提交",
/// 所以响应特别慢的站点也会触发——含义是"没有响应",不是"一定失败"。
pub const NAV_STALL_TIMEOUT: Duration = Duration::from_secs(15);

/// 宿主可以对激活页签发的导航命令。后退 / 前进用页面的 `history`(wry 0.55 没有 `go_back`),由**宿主**
/// 侧 `evaluate_script` 执行,不向页面注入任何脚本;刷新走原生 `reload()`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebNav {
    Back,
    Forward,
    Refresh,
}

/// 池向宿主回报的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebEvent {
    /// 页面整页加载完成(`id` 页签,`url` 是 webview 实际所在的地址)。
    PageLoaded {
        generation: u64,
        id: usize,
        url: String,
    },
    /// 页面标题变了(wry 原生回调,不靠页面脚本)。
    TitleChanged {
        generation: u64,
        id: usize,
        title: String,
    },
    /// 页面请求新窗口(`target=_blank` / `window.open`)且地址通过了白名单——宿主应开新页签。
    NewTabRequested { generation: u64, url: String },
    /// 发出的导航迟迟没有开始(见 [`NAV_STALL_TIMEOUT`])。
    NavigationStalled {
        generation: u64,
        id: usize,
        url: String,
    },
    /// 页签的网址不被允许(`javascript:`、`data:`、网络共享 `file://` …),池没有交给 wry,页签显示 `about:blank`。
    NavigationBlocked {
        generation: u64,
        id: usize,
        url: String,
    },
}

struct Tab {
    view: WebView,
    /// 宿主**要求**的网址(即使被挡下、实际显示 `about:blank`,也记要求的,免得每次同步都重复挡一次)。
    url: String,
    visible: bool,
    /// 保活句柄(媒体捕获代理委托;`WKWebView` 对委托是 weak 引用)。
    _guard: Guard,
}

struct PoolState {
    generation: u64,
    rect: LogicalRect,
    scale: f64,
    tabs: BTreeMap<usize, Tab>,
}

thread_local! {
    static POOLS: RefCell<BTreeMap<u64, PoolState>> = const { RefCell::new(BTreeMap::new()) };
}
static NEXT_POOL: AtomicU64 = AtomicU64::new(1);

/// 一个产品槽位对应一个池(Digger 的浏览器面板一个)。方法只在主线程的 `window::run` 里碰 wry。
pub struct WebPool {
    slot: u64,
    epoch: Arc<AtomicU64>,
    events: Sender<WebEvent>,
    tracker: Arc<Mutex<NavTracker>>,
}

impl WebPool {
    /// `epoch` 是宿主维护的"当前有效的代":任务执行时发现 `epoch != generation` 就什么都不做(迟到作废)。
    pub fn new(epoch: Arc<AtomicU64>, events: Sender<WebEvent>) -> Self {
        Self {
            slot: NEXT_POOL.fetch_add(1, Ordering::Relaxed),
            epoch,
            events,
            tracker: Arc::new(Mutex::new(NavTracker::new(NAV_STALL_TIMEOUT))),
        }
    }

    /// 让池收敛到 `desired`。`generation` 与上次不同(切项目)就全部销毁重建。失败返回 `failed(消息)`。
    #[allow(clippy::too_many_arguments)]
    pub fn sync<E: Send + 'static>(
        &self,
        id: window::Id,
        generation: u64,
        desired: Vec<WebTabSpec>,
        rect: LogicalRect,
        scale: f64,
        failed: impl FnOnce(String) -> E + Send + 'static,
        done: E,
    ) -> Task<E> {
        let (slot, epoch) = (self.slot, self.epoch.clone());
        let (events, tracker) = (self.events.clone(), self.tracker.clone());
        window::run(id, move |window| {
            if epoch.load(Ordering::SeqCst) != generation {
                return done;
            }
            match apply_sync(
                slot, &window, generation, &desired, rect, scale, &events, &tracker,
            ) {
                Ok(()) => done,
                Err(error) => failed(error),
            }
        })
    }

    /// 对页签 `tab` 发后退 / 前进 / 刷新。
    pub fn nav<E: Send + 'static>(
        &self,
        id: window::Id,
        generation: u64,
        tab: usize,
        nav: WebNav,
        done: E,
    ) -> Task<E> {
        let (slot, epoch) = (self.slot, self.epoch.clone());
        window::run(id, move |_| {
            if epoch.load(Ordering::SeqCst) == generation {
                POOLS.with(|pools| {
                    let pools = pools.borrow();
                    let Some(state) = pools.get(&slot).filter(|s| s.generation == generation)
                    else {
                        return;
                    };
                    if let Some(tab) = state.tabs.get(&tab) {
                        let _ = match nav {
                            WebNav::Back => tab.view.evaluate_script("window.history.back()"),
                            WebNav::Forward => tab.view.evaluate_script("window.history.forward()"),
                            WebNav::Refresh => tab.view.reload(),
                        };
                    }
                });
            }
            done
        })
    }

    /// 销毁池里全部视图(面板被关闭 / 项目被关闭)。
    pub fn destroy_all<E: Send + 'static>(&self, id: window::Id, done: E) -> Task<E> {
        let slot = self.slot;
        if let Ok(mut tracker) = self.tracker.lock() {
            *tracker = NavTracker::new(NAV_STALL_TIMEOUT);
        }
        window::run(id, move |_| {
            POOLS.with(|pools| {
                if let Some(state) = pools.borrow_mut().remove(&slot) {
                    for tab in state.tabs.values() {
                        restore_focus(&tab.view);
                    }
                }
            });
            done
        })
    }

    /// 把等得太久的导航作为 [`WebEvent::NavigationStalled`] 送出。宿主在自己的定时 tick 里调,
    /// 不碰 webview,所以不需要主线程。
    pub fn poll_stalled(&self, now: Instant) {
        let generation = self.epoch.load(Ordering::SeqCst);
        let stalled = self
            .tracker
            .lock()
            .map(|mut t| t.poll(now))
            .unwrap_or_default();
        for (id, url) in stalled {
            let _ = self.events.send(WebEvent::NavigationStalled {
                generation,
                id,
                url,
            });
        }
    }

    /// 读真实 NSView 的状态(生命周期自检读它,不读我们自己的记账)。
    pub fn probe<E: Send + 'static>(
        &self,
        id: window::Id,
        generation: u64,
        tab: usize,
        map: impl FnOnce(Probe) -> E + Send + 'static,
    ) -> Task<E> {
        let slot = self.slot;
        window::run(id, move |_| {
            map(POOLS.with(|pools| {
                let pools = pools.borrow();
                let state = pools.get(&slot);
                let entry = state
                    .filter(|s| s.generation == generation)
                    .and_then(|s| s.tabs.get(&tab));
                let mut probe = Probe {
                    exists: entry.is_some(),
                    visible: false,
                    bounds: None,
                    handles: state.map_or(0, |s| s.tabs.len()),
                };
                if let Some(tab) = entry {
                    inspect(&tab.view, &mut probe);
                }
                probe
            }))
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_sync(
    slot: u64,
    window: &dyn window::Window,
    generation: u64,
    desired: &[WebTabSpec],
    rect: LogicalRect,
    scale: f64,
    events: &Sender<WebEvent>,
    tracker: &Arc<Mutex<NavTracker>>,
) -> Result<(), String> {
    POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        let state = pools.entry(slot).or_insert_with(|| PoolState {
            generation,
            rect,
            scale,
            tabs: BTreeMap::new(),
        });
        let generation_changed = state.generation != generation;
        let existing: Vec<ExistingTab> = state
            .tabs
            .iter()
            .map(|(id, t)| ExistingTab {
                id: *id,
                url: t.url.clone(),
                visible: t.visible,
            })
            .collect();
        let now = Instant::now();
        for action in plan_sync(&existing, desired, generation_changed) {
            match action {
                PoolAction::Destroy { id } => {
                    if let Some(tab) = state.tabs.remove(&id) {
                        restore_focus(&tab.view);
                    }
                    if let Ok(mut t) = tracker.lock() {
                        t.forget(id);
                    }
                }
                PoolAction::Create { id, url, visible } => {
                    let target = target_or_block(events, generation, id, &url);
                    let (view, guard) = build_tab(
                        window, generation, id, &target, visible, rect, scale, events, tracker,
                    )?;
                    state.tabs.insert(
                        id,
                        Tab {
                            view,
                            url: url.clone(),
                            visible,
                            _guard: guard,
                        },
                    );
                    note_navigation(tracker, id, &target, now);
                }
                PoolAction::Navigate { id, url } => {
                    let target = target_or_block(events, generation, id, &url);
                    if let Some(tab) = state.tabs.get_mut(&id) {
                        tab.view.load_url(&target).map_err(|e| e.to_string())?;
                        tab.url = url;
                    }
                    note_navigation(tracker, id, &target, now);
                }
                PoolAction::Hide { id } => {
                    if let Some(tab) = state.tabs.get_mut(&id) {
                        restore_focus(&tab.view);
                        tab.view.set_visible(false).map_err(|e| e.to_string())?;
                        tab.visible = false;
                    }
                }
                PoolAction::Show { id } => {
                    if let Some(tab) = state.tabs.get_mut(&id) {
                        tab.view
                            .set_bounds(crate::panel::rect_to_wry(rect))
                            .map_err(|e| e.to_string())?;
                        tab.view.set_visible(true).map_err(|e| e.to_string())?;
                        tab.visible = true;
                    }
                }
            }
        }
        if state.rect != rect {
            for tab in state.tabs.values().filter(|t| t.visible) {
                tab.view
                    .set_bounds(crate::panel::rect_to_wry(rect))
                    .map_err(|e| e.to_string())?;
            }
        }
        if state.scale != scale {
            for tab in state.tabs.values() {
                let _ = tab.view.zoom(scale);
            }
        }
        state.generation = generation;
        state.rect = rect;
        state.scale = scale;
        Ok(())
    })
}

/// 要求的网址 → 真正交给 wry 的地址;不被允许的换成 `about:blank` 并报告 `NavigationBlocked`。
fn target_or_block(events: &Sender<WebEvent>, generation: u64, id: usize, url: &str) -> String {
    wry_target(url).unwrap_or_else(|| {
        let _ = events.send(WebEvent::NavigationBlocked {
            generation,
            id,
            url: url.to_owned(),
        });
        "about:blank".to_owned()
    })
}

fn note_navigation(tracker: &Arc<Mutex<NavTracker>>, id: usize, target: &str, now: Instant) {
    // `about:blank` 不走网络,页面提交很快但不一定触发 Started——不追踪,免得误报"没有响应"。
    if target == "about:blank" {
        return;
    }
    if let Ok(mut t) = tracker.lock() {
        t.navigate(id, target.to_owned(), now);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_tab(
    window: &dyn window::Window,
    generation: u64,
    id: usize,
    target: &str,
    visible: bool,
    rect: LogicalRect,
    scale: f64,
    events: &Sender<WebEvent>,
    tracker: &Arc<Mutex<NavTracker>>,
) -> Result<(WebView, Guard), String> {
    let (title_tx, load_tx, window_tx) = (events.clone(), events.clone(), events.clone());
    let load_tracker = tracker.clone();
    let view = WebViewBuilder::new()
        .with_url(target)
        .with_bounds(crate::panel::rect_to_wry(rect))
        .with_visible(visible)
        .with_devtools(false)
        .with_allow_link_preview(false)
        .with_data_store_identifier(BROWSER_DATA_STORE_ID)
        .with_download_started_handler(|_, _| false)
        .with_navigation_handler(|url| allows_browse_navigation(&url))
        .with_new_window_req_handler(move |url, _features| {
            if allows_new_tab(&url) {
                let _ = window_tx.send(WebEvent::NewTabRequested { generation, url });
            }
            wry::NewWindowResponse::Deny
        })
        .with_document_title_changed_handler(move |title| {
            let _ = title_tx.send(WebEvent::TitleChanged {
                generation,
                id,
                title,
            });
        })
        .with_on_page_load_handler(move |event, url| {
            let verdict = load_tracker
                .lock()
                .map(|mut t| t.on_page_load(id, &url))
                .unwrap_or(LoadVerdict::Forward);
            if verdict == LoadVerdict::Forward && matches!(event, wry::PageLoadEvent::Finished) {
                let _ = load_tx.send(WebEvent::PageLoaded {
                    generation,
                    id,
                    url,
                });
            }
        })
        .build_as_child(&window)
        .map_err(|e| e.to_string())?;
    let _ = view.zoom(scale);
    let guard = install_media_deny_proxy(&view);
    Ok((view, guard))
}

#[cfg(test)]
mod tests {
    /// 池的 builder 是创建浏览器子视图的唯一路径,没有廉价夹具能真的建一个;这些是它**必须**写出、
    /// 且 wry 默认值会反过来的配置,用源码扫描钉住(wry 0.55 默认:下载放行、debug 下开发者工具开)。
    /// 页面是**不可信内容**:不得注册自定义协议、不得注入脚本、不得装 IPC 处理器。
    #[test]
    fn web_child_pins_untrusted_content_settings() {
        let src = include_str!("pool.rs");
        // 只看生产代码:测试模块自身含这些字面量(在断言里),扫进去会自我命中。
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        for required in [
            ".with_devtools(false)",
            ".with_download_started_handler(|_, _| false)",
            ".with_allow_link_preview(false)",
            ".with_data_store_identifier(BROWSER_DATA_STORE_ID)",
            ".with_navigation_handler(",
            ".with_new_window_req_handler(",
            ".with_document_title_changed_handler(",
            ".with_on_page_load_handler(",
            "wry::NewWindowResponse::Deny",
            "wry_target(",
            ".on_page_load(id, &url)",
            // 必须是**调用点**:非 macOS 的空实现定义里也有 `install_media_deny_proxy(`,只认函数名会漏掉"删掉调用"。
            "= install_media_deny_proxy(&view)",
            "build_as_child(",
        ] {
            assert!(prod.contains(required), "缺少 {required}");
        }
        for forbidden in [
            "with_custom_protocol",
            "with_initialization_script",
            "with_ipc_handler",
            "with_html(",
            "load_html(",
            "NewWindowResponse::Allow",
            ".with_devtools(true)",
            ".with_incognito(true)",
            "with_download_completed_handler",
            ".with_clipboard(",
        ] {
            assert!(!prod.contains(forbidden), "不得出现 {forbidden}");
        }
        // 导航与新窗口的处理器**内部**必须真的走白名单,而不是只是挂了个回调。
        let after = |needle: &str| {
            let at = prod.find(needle).unwrap_or_else(|| panic!("缺少 {needle}"));
            &prod[at..(at + 400).min(prod.len())]
        };
        assert!(
            after(".with_navigation_handler(").contains("allows_browse_navigation("),
            "导航处理器必须调用 allows_browse_navigation"
        );
        assert!(
            after(".with_new_window_req_handler(").contains("allows_new_tab("),
            "新窗口处理器必须调用 allows_new_tab"
        );
    }
}
