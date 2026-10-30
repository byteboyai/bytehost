//! **可信**原生子 webview 的执行器(原 Digger `adapters/native_host.rs`,X1 阶段 B 上收)。
//!
//! 与 [`crate::panel`](推 HTML + 脚本的面板 surface)是两种模型:这里的子视图**按 URL 导航**,
//! 载入的文档来自固定的可信 origin(自定义协议),由 [`allows_navigation`] 按原始 authority 严格比对。
//! 状态机是纯函数 [`plan`](当前期望 → 动作序列),执行在主线程经 `iced::window::run`;wry 句柄只存在于
//! 本模块的 thread_local,订阅侧状态只走原子量与消息。
//!
//! 安全设置由本模块的 builder 一处写死(开发者工具关、下载恒拒、新窗口恒拒、导航只放行可信 origin),
//! 产品经 `configure` 闭包只能**追加**自己的协议/IPC/初始化脚本;`native_child_pins_the_restrictive_wry_settings`
//! 用源码扫描钉住这几项(wry 默认值不是安全默认)。
use bytehost_core::LogicalRect;
use iced::{Task, window};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::wry_common::{HandleRegistry, apply_restricted};

#[derive(Clone, Debug, PartialEq)]
pub struct Desired {
    /// Changing identity replaces the one child; changing URL navigates it in place.
    pub identity: String,
    pub url: String,
    pub rect: LogicalRect,
    pub scale: f64,
    pub visible: bool,
    /// Readiness belongs to `url`, not to the previously displayed document.
    /// Keep false after URL changes until a matching DocumentLoaded is validated.
    pub loaded: bool,
}
/// Trusted document origin plus the first desired state of its child.
pub struct Document {
    pub scheme: &'static str,
    pub host: &'static str,
    pub desired: Desired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Destroy,
    Create,
    Navigate,
    Zoom,
    SetBounds,
    Park,
    Show,
}

pub fn plan(current: Option<&Desired>, desired: Option<&Desired>) -> Vec<Action> {
    let Some(d) = desired else {
        return current.map_or_else(Vec::new, |_| vec![Action::Destroy]);
    };
    if current.is_none_or(|c| c.identity != d.identity) {
        let mut actions = Vec::new();
        if current.is_some() {
            actions.push(Action::Destroy);
        }
        actions.extend([Action::Create, Action::Zoom]);
        actions.extend(if d.visible && d.loaded {
            vec![Action::SetBounds, Action::Show]
        } else {
            vec![Action::Park]
        });
        return actions;
    }
    let c = current.unwrap();
    let mut actions = Vec::new();
    let navigating = c.url != d.url;
    let showing = d.visible && d.loaded;
    let was_showing = c.visible && c.loaded;
    if (!showing && (was_showing || c.visible != d.visible)) || navigating {
        actions.push(Action::Park);
    }
    if navigating {
        actions.push(Action::Navigate);
    }
    if c.scale != d.scale {
        actions.push(Action::Zoom);
    }
    if showing && (!was_showing || c.rect != d.rect) {
        actions.push(Action::SetBounds);
    }
    if showing && !was_showing {
        actions.push(Action::Show);
    }
    actions
}

/// Compare the raw authority rather than a normalized/prefix hostname. This rejects
/// credentials, ports, percent-encoded authority, backslashes and control characters.
pub fn allows_navigation(scheme: &str, host: &str, url: &str) -> bool {
    if url.chars().any(|c| c.is_control() || c == '\\') {
        return false;
    }
    let Some(rest) = url.strip_prefix(scheme).and_then(|s| s.strip_prefix("://")) else {
        return false;
    };
    rest.split_once('/')
        .is_some_and(|(authority, _)| authority == host)
}

pub fn apply_actions(
    actions: Vec<Action>,
    mut execute: impl FnMut(Action) -> Result<(), String>,
) -> Result<(), String> {
    for action in actions {
        execute(action)?;
    }
    Ok(())
}
pub fn sync_outcome<E>(
    generation: u64,
    result: Result<(), String>,
    failed: impl FnOnce(u64, Result<(), String>) -> E,
    done: E,
) -> E {
    match result {
        Ok(()) => done,
        Err(error) => failed(generation, Err(error)),
    }
}

pub fn allows_download() -> bool {
    false
}
pub fn new_window_response() -> wry::NewWindowResponse {
    wry::NewWindowResponse::Deny
}

struct Entry {
    generation: u64,
    view: wry::WebView,
}
thread_local! {
    static VIEWS: HandleRegistry<u64, Entry> = const { HandleRegistry::new() };
}

/// 在主线程借出句柄表(所有 wry 调用都必须在 `window::run` 里)。
fn with_views<R>(f: impl FnOnce(&mut BTreeMap<u64, Entry>) -> R) -> R {
    VIEWS.with(|views| views.with(f))
}
static NEXT_HOST: AtomicU64 = AtomicU64::new(1);
pub struct Host {
    slot: u64,
    epoch: Arc<AtomicU64>,
    scheduled: Option<Desired>,
}
impl Host {
    pub fn new(epoch: Arc<AtomicU64>) -> Self {
        Self {
            slot: NEXT_HOST.fetch_add(1, Ordering::Relaxed),
            epoch,
            scheduled: None,
        }
    }
    pub fn destroy<E: Send + 'static>(
        &mut self,
        id: window::Id,
        generation: u64,
        done: E,
    ) -> Task<E> {
        self.scheduled = None;
        let slot = self.slot;
        window::run(id, move |_| {
            with_views(|views| {
                if views.get(&slot).is_some_and(|e| e.generation == generation)
                    && let Some(entry) = views.remove(&slot)
                {
                    restore_focus(&entry.view);
                }
            });
            done
        })
    }
    pub fn create<E, F>(
        &mut self,
        id: window::Id,
        generation: u64,
        document: Document,
        configure: F,
        built: fn(u64, Result<(), String>) -> E,
        done: E,
    ) -> Task<E>
    where
        E: Send + 'static,
        F: for<'a> FnOnce(wry::WebViewBuilder<'a>) -> wry::WebViewBuilder<'a> + Send + 'static,
    {
        let Document {
            scheme,
            host,
            desired,
        } = document;
        self.scheduled = Some(desired.clone());
        let slot = self.slot;
        let epoch = self.epoch.clone();
        window::run(id, move |window| {
            if epoch.load(Ordering::SeqCst) != generation {
                return done;
            }
            let result = with_views(|views| {
                if let Some(old) = views.remove(&slot) {
                    restore_focus(&old.view);
                }
                // 宿主策略在 `configure` **之后**应用:产品只能追加,不能把 devtools/
                // 下载/新窗口放开。
                let builder = apply_restricted(configure(wry::WebViewBuilder::new()))
                    .with_url(&desired.url)
                    .with_bounds(if desired.loaded {
                        bounds(desired.rect)
                    } else {
                        parked_bounds(desired.rect)
                    })
                    .with_visible(desired.visible)
                    .with_navigation_handler(move |url| allows_navigation(scheme, host, &url));
                let view = builder.build_as_child(&window).map_err(|e| e.to_string())?;
                view.zoom(desired.scale).map_err(|e| e.to_string())?;
                views.insert(slot, Entry { generation, view });
                Ok(())
            });
            built(generation, result)
        })
    }
    /// Update the current identity. A changed URL starts a parked document even
    /// if the previous document was loaded. Subsequent `loaded=true` explicitly
    /// asserts that the caller validated DocumentLoaded for this exact URL;
    /// callers must keep it false until that event arrives.
    pub fn sync<E: Send + 'static>(
        &mut self,
        id: window::Id,
        generation: u64,
        desired: Desired,
        script: Option<String>,
        failed: impl FnOnce(u64, Result<(), String>) -> E + Send + 'static,
        done: E,
    ) -> Task<E> {
        // URL changes start a new document. A previous document's loaded bit must
        // never reveal old content while its replacement is loading.
        let mut desired = desired;
        if self
            .scheduled
            .as_ref()
            .is_some_and(|c| c.url != desired.url)
        {
            desired.loaded = false;
        }
        let actions = plan(self.scheduled.as_ref(), Some(&desired));
        if actions.is_empty() && script.is_none() {
            return Task::none();
        }
        self.scheduled = Some(desired.clone());
        let slot = self.slot;
        let epoch = self.epoch.clone();
        window::run(id, move |_| {
            if epoch.load(Ordering::SeqCst) != generation {
                return done;
            }
            let result = with_views(|views| {
                let e = views
                    .get_mut(&slot)
                    .filter(|e| e.generation == generation)
                    .ok_or_else(|| "native child unavailable during sync".to_owned())?;
                apply_actions(actions, |action| {
                    let result = match action {
                        Action::Navigate => e.view.load_url(&desired.url),
                        Action::Zoom => e.view.zoom(desired.scale),
                        Action::SetBounds => e.view.set_bounds(bounds(desired.rect)),
                        Action::Park => {
                            restore_focus(&e.view);
                            e.view
                                .set_bounds(parked_bounds(desired.rect))
                                .map_err(|error| format!("native park bounds failed: {error}"))?;
                            // Keep WKWebView running rAF while parked offscreen.
                            e.view.set_visible(desired.visible)
                        }
                        Action::Show => e.view.set_visible(true),
                        Action::Create | Action::Destroy => {
                            unreachable!("identity replacement requires create")
                        }
                    };
                    result.map_err(|error| format!("native {action:?} failed: {error}"))
                })?;
                if let Some(script) = script {
                    e.view
                        .evaluate_script(&script)
                        .map_err(|error| format!("native script failed: {error}"))?;
                }
                Ok(())
            });
            sync_outcome(generation, result, failed, done)
        })
    }
    pub fn probe<E: Send + 'static>(
        &self,
        id: window::Id,
        generation: u64,
        map: impl FnOnce(Probe) -> E + Send + 'static,
    ) -> Task<E> {
        let slot = self.slot;
        window::run(id, move |_| {
            map(with_views(|views| {
                let entry = views.get(&slot).filter(|e| e.generation == generation);
                let mut probe = Probe {
                    exists: entry.is_some(),
                    visible: false,
                    bounds: None,
                    handles: usize::from(views.contains_key(&slot)),
                };
                if let Some(e) = entry {
                    inspect(&e.view, &mut probe);
                }
                probe
            }))
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub exists: bool,
    pub visible: bool,
    pub bounds: Option<(f64, f64, f64, f64)>,
    pub handles: usize,
}
#[cfg(target_os = "macos")]
pub(crate) fn inspect(view: &wry::WebView, probe: &mut Probe) {
    use wry::WebViewExtMacOS;
    let ns = view.webview();
    let frame = ns.frame();
    // SAFETY: live child and its parent are inspected only inside window::run.
    probe.bounds = unsafe { ns.superview() }.map(|parent| {
        let y = if parent.isFlipped() {
            frame.origin.y
        } else {
            parent.frame().size.height - frame.origin.y - frame.size.height
        };
        (frame.origin.x, y, frame.size.width, frame.size.height)
    });
    probe.visible = !ns.isHidden();
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn inspect(view: &wry::WebView, probe: &mut Probe) {
    // wry exposes no cross-platform native visibility getter.
    probe.bounds = view.bounds().ok().map(|r| {
        let p = r.position.to_logical::<f64>(1.);
        let s = r.size.to_logical::<f64>(1.);
        (p.x, p.y, s.width, s.height)
    });
}
#[cfg(target_os = "macos")]
pub(crate) fn restore_focus(view: &wry::WebView) {
    use wry::WebViewExtMacOS;
    // window::run supplies main-thread execution; the child is still alive here.
    if let Some(window) = view.webview().window()
        && let Some(content) = window.contentView()
    {
        let _ = window.makeFirstResponder(Some(&content));
    }
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn restore_focus(_: &wry::WebView) {}
fn parked_bounds(mut rect: LogicalRect) -> wry::Rect {
    rect.x = -100_000.;
    rect.y = -100_000.;
    bounds(rect)
}
fn bounds(rect: LogicalRect) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(rect.x, rect.y).into(),
        size: wry::dpi::LogicalSize::new(rect.width, rect.height).into(),
    }
}
#[cfg(test)]
#[path = "native_child_tests.rs"]
mod tests;
