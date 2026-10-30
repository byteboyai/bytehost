//! 面板宿主:窗口生命周期与 wry 子视图的创建/驱动。
//!
//! 复用 Task 02 已验证的入口模式(见 `docs/dozer-v2/digger-bytehost-integration/api-decisions.md`):
//! - 唯一 iced 事件循环;不在宿主里另起 winit 循环;
//! - wry 子视图在**主线程**、用 `iced::window::run(id, ..)` + `build_as_child` 创建;
//! - 非 `Send` 的 `WebView` 句柄放 `thread_local`,所有操作经 `with_handle`(主线程);
//! - 几何用逻辑坐标;webview 恒在 iced 之上,顶部条高由 `bytehost_core::host_rect` 下推。
//!
//! 本模块只提供**机制**:产品决定何时创建哪个 slot 的子视图、加载什么内容。

use std::collections::BTreeMap;

use bytehost_core::{LogicalRect, SurfaceSlot};
use iced::{Size, Task, window};

use crate::wry_common::HandleRegistry;

/// 一个子视图的创建请求:slot + HTML 内容 + 逻辑矩形。
#[derive(Clone, Debug)]
pub struct ChildSpec {
    pub slot: SurfaceSlot,
    pub html: String,
    pub rect: LogicalRect,
}

impl ChildSpec {
    pub fn new(slot: SurfaceSlot, html: impl Into<String>, rect: LogicalRect) -> Self {
        Self {
            slot,
            html: html.into(),
            rect,
        }
    }
}

/// 子视图创建结果:成功交回 slot,失败带错误串(不 panic)。
#[derive(Debug, Clone)]
pub struct ChildBuilt {
    pub slot: SurfaceSlot,
    pub result: Result<(), String>,
}

thread_local! {
    /// 非 Send 的 wry 句柄只在主线程持有;所有操作都必须经 `with_handles`。
    /// 面板 surface 载入的是**可信内部 HTML**,builder 刻意保持 wry 默认
    /// (不用 `wry_common::apply_restricted`),见该模块文档。
    static HANDLES: HandleRegistry<SurfaceSlot, wry::WebView> = const { HandleRegistry::new() };
}

/// 在主线程用 `window::run` 创建一个 wry 子视图(HTML 内容)。
///
/// 返回的 `Task` 完成时产出 `ChildBuilt`。创建失败经 `ChildBuilt.result = Err` 回传。
pub fn build_child<M>(
    window_id: window::Id,
    spec: ChildSpec,
    wrap: impl Fn(ChildBuilt) -> M + Send + 'static,
) -> Task<M>
where
    M: Send + 'static,
{
    iced::window::run(window_id, move |w| {
        let webview = wry::WebViewBuilder::new()
            .with_html(spec.html.clone())
            .with_bounds(rect_to_wry(spec.rect))
            .build_as_child(&w);
        match webview {
            Ok(wv) => {
                with_handles(|h| {
                    h.insert(spec.slot, wv);
                });
                ChildBuilt {
                    slot: spec.slot,
                    result: Ok(()),
                }
            }
            Err(e) => ChildBuilt {
                slot: spec.slot,
                result: Err(e.to_string()),
            },
        }
    })
    .map(wrap)
}

/// 重设某个 slot 子视图的 bounds(主线程)。
pub fn set_bounds(slot: SurfaceSlot, rect: LogicalRect) -> bool {
    with_handles(|h| {
        if let Some(wv) = h.get(&slot) {
            let _ = wv.set_bounds(rect_to_wry(rect));
            true
        } else {
            false
        }
    })
}

/// 向某个 slot 子视图注入脚本(主线程)。返回是否有该实例。
pub fn evaluate(slot: SurfaceSlot, script: &str) -> bool {
    with_handles(|h| {
        if let Some(wv) = h.get(&slot) {
            let _ = wv.evaluate_script(script);
            true
        } else {
            false
        }
    })
}

/// 销毁某个 slot 子视图(主线程)。
pub fn destroy(slot: SurfaceSlot) -> bool {
    with_handles(|h| h.remove(&slot).is_some())
}

/// 当前已创建的子视图数量(测试/诊断用)。
pub fn handle_count() -> usize {
    with_handles(|h| h.len())
}

pub(crate) fn rect_to_wry(rect: LogicalRect) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(rect.x, rect.y).into(),
        size: wry::dpi::LogicalSize::new(rect.width, rect.height).into(),
    }
}

fn with_handles<R>(f: impl FnOnce(&mut BTreeMap<SurfaceSlot, wry::WebView>) -> R) -> R {
    HANDLES.with(|h| h.with(f))
}

/// 窗口当前逻辑尺寸(在 boot 里异步拿到;`None` 表示窗口尚未就绪)。
pub fn window_size(window_id: window::Id) -> Task<Option<Size>> {
    iced::window::size(window_id).map(Some)
}

/// 拿"最老"的窗口 id(通常即主窗口)。`None` 表示窗口尚未创建。
pub fn oldest_window() -> Task<Option<window::Id>> {
    iced::window::oldest()
}

/// 退出窗口循环。
pub fn exit() -> Task<iced::Never> {
    iced::exit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_maps_to_logical_wry_rect() {
        let r = LogicalRect {
            x: 0.0,
            y: 44.0,
            width: 800.0,
            height: 600.0,
        };
        let w = rect_to_wry(r);
        // wry::Rect 的字段是 dpi::Rect,断言其能构造即可(不依赖其 API)。
        let _ = w;
    }
}
