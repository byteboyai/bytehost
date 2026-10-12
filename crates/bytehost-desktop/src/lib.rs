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
//! - [`dispatch`]:Tokio 执行器与 iced 结果投递适配;
//! - [`surface`]:Surface 生命周期状态机执行、`(instance, slot, generation)` 路由与几何。
//! - [`layout`]:产品历史布局 → 稳定 `Layout` 的迁移流程(版本闸门 / 未知保留 / 原子落盘)。
//! - `focus`:独立原生窗口的"因失焦而关闭"判定(与产品无关)。

pub mod app_webview;
pub mod dispatch;
pub mod focus;
pub mod layout;
pub mod panel;
pub mod shell;
pub mod surface;

pub use app_webview::{
    AppSurfaceBuilt, AppSurfaceIpc, AppSurfaceSpec, app_handle_count, build_app_child, destroy_app,
    evaluate_app, set_app_bounds,
};
pub use focus::{FocusTracker, focus_transition};
pub use layout::{
    LegacyPanelAdapter, MigrateError, MigrationOutcome, atomic_save_layout, migrate_legacy_file,
    migrate_legacy_layout, reconcile_with_catalog,
};
pub use shell::{HostMessage, Product, SurfaceEvent, route_lifecycle};
pub use surface::SurfaceExecutor;

/// 复用 core 的几何与身份类型,产品无需再依赖 core 的路径。
pub use bytehost_core::{LogicalRect, SurfaceSlot};
