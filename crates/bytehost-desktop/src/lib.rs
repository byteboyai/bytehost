//! bytehost 共享桌面宿主:iced 窗口壳 + wry 子视图执行 + 产品泛型回调。
//!
//! 分层(见规格 §4/§5、Task 02 决策):
//! - `bytehost-core` 是纯描述层(身份、请求、布局 DTO,仅 serde);
//! - 本 crate **允许** iced/wry,实现窗口循环与子视图,但**不含任何产品业务 crate**
//!   (门禁:`scripts/check-deps.sh` 的 desktop 组);
//! - 产品(Dozer / Digger)以泛型消息 `M` + 回调接入,不把产品类型搬进宿主。
//!
//! 模块:
//! - [`shell`]:`HostMessage<M>`、`Product<M>` trait 与窗口壳;
//! - [`panel`]:窗口生命周期与 wry 子视图的创建/驱动(主线程 thread_local 句柄);
//! - [`app_webview`]:应用 webview 的**受限 builder**(唯一一份 wry 应用初始化代码);
//! - [`native_child`]:**可信**原生子 webview(按 URL 导航、固定可信 origin)的执行器——
//!   与 [`panel`](推 HTML + 脚本的面板 surface)是两种模型;
//! - [`dispatch`]:Tokio 执行器与 iced 结果投递适配;
//! - [`surface`]:Surface 生命周期状态机执行、`(instance, slot, generation)` 路由与几何。
//! - [`layout`]:产品历史布局 → 稳定 `Layout` 的迁移流程(版本闸门 / 未知保留 / 原子落盘)。
//! - [`panes`]:`PanelId` 键控的 rail 顺序 + 每面板分栏状态与拖拽换算(产品中立)。
//! - [`hover`] 提供产品中立的可悬停元素槽位词汇(`HoverSlot`,与具体面板无关)。
//! - `focus`:独立原生窗口的"因失焦而关闭"判定(与产品无关)。
//! - [`overlay`]:独立原生浮层的建窗 + 定位 + 鼠标穿透/圆角遮罩几何(产品中立,
//!   不绑渲染器后端;渲染管线经泛型工厂接入)。
//! - `overlay_gpu`(`overlay` feature):独立原生浮层共用的 wgpu 渲染管线 +
//!   遮罩外壳(C10 方案 C),仅在开启 `overlay` 时编译。

pub mod app_webview;
pub mod dispatch;
pub mod focus;
pub mod hover;
pub mod layout;
pub mod native_child;
pub mod overlay;
#[cfg(feature = "overlay")]
pub mod overlay_gpu;
pub mod panel;
pub mod panes;
pub mod shell;
pub mod surface;

pub use app_webview::{
    AppSurfaceBuilt, AppSurfaceIpc, AppSurfaceSpec, app_handle_count, build_app_child, destroy_app,
    evaluate_app, set_app_bounds,
};
pub use focus::{FocusRouter, FocusTarget, FocusTracker, focus_transition};
pub use hover::HoverSlot;
pub use layout::{
    LegacyPanelAdapter, MigrateError, MigrationOutcome, atomic_save_layout, migrate_legacy_file,
    migrate_legacy_layout, reconcile_with_catalog,
};
pub use overlay::{
    BACKDROP_CORNER_RADIUS, POPUP_WIDTH_FRACTION, ToastMetrics, centered_card_offset,
    enable_overlay_mouse_moved_events, full_window_overlay_bounds, open_child_window,
    open_child_window_unfocused, open_overlay, popup_card_size, reposition_overlay, stack_bounds,
    window_logical_size,
};
#[cfg(feature = "overlay")]
pub use overlay_gpu::{
    OverlayGpu, apply_cursor, apply_input_method, backdrop_card, backdrop_card_shrink,
    ime_cursor_area,
};
pub use panes::{
    PaneDims, PaneDragInput, PaneDragOutcome, PaneSpec, PaneStore, RailItems, apply_pane_split_drag,
};
pub use shell::{HostMessage, Product, SurfaceEvent, route_lifecycle};
pub use surface::SurfaceExecutor;

/// 复用 core 的几何与身份类型,产品无需再依赖 core 的路径。
pub use bytehost_core::{LogicalRect, SurfaceSlot, id::PanelId};
