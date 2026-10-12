//! bytehost 桌面宿主的**纯契约层**。
//!
//! 只放稳定身份、注册元数据与布局 DTO——不依赖任何产品 crate、不依赖 iced/wry、
//! 不依赖平台 webview 库(门禁:`scripts/check-deps.sh` 的 core 组)。
//!
//! 产品组合根(Dozer / Digger)把这些契约映射到自己的业务枚举;Host 不解释业务含义。
//! iced 视图回调与 wry 执行实现属于 `bytehost-desktop`,不进本 crate(规格 §4/§5)。

pub mod geometry;
pub mod id;
pub mod layout;
pub mod registry;
pub mod request;

pub use geometry::{Insets, LogicalRect, host_rect};
pub use id::{PanelId, PanelIdError, PanelInstanceId, ScopeId, ScopeIdError, SurfaceSlot};
pub use layout::{LAYOUT_SCHEMA_VERSION, Layout, LayoutError, Rail};
pub use registry::{CatalogError, PanelCatalog, PanelMeta};
pub use request::{
    BackgroundExecutor, Emitter, Generation, RequestKey, RequestToken, RequestTracker,
    ResultEnvelope, run_scoped,
};
