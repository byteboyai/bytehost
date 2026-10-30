//! 产品中立的设置状态层。
//!
//! 本 crate 只承载共享设置窗口的**状态与契约**:
//!
//! - [`page`][]:稳定页面身份 [`PageId`] 与页面描述 [`PageDescriptor`];
//! - [`registry`][]:有序页面注册表与重复 / 保留命名空间校验;
//! - [`state`][]:打开、切页、关窗与窗口代数;
//! - [`selection`][]:语言等"选中 → 保存 → 确认 / 失败 / 冲突"的通用模型。
//!
//! 刻意**不**依赖 iced、byteui、wry 或任何 `dozer-*` / `digger-*` crate。窗口驱动、
//! 服务调用、偏好落盘和主题应用都留在产品适配层。

pub mod page;
pub mod registry;
pub mod selection;
pub mod state;

pub use page::{BUILTIN_NAMESPACE, PageDescriptor, PageId, PageIdError, PageKind};
pub use registry::{PageRegistry, RegistryError};
pub use selection::{SaveStatus, SelectionState};
pub use state::SettingsState;

/// 共享页面注册表里内置页的固定 ID。
pub const THEME_ID: &str = "bytehost.theme";
/// 语言页 ID(可选注册)。
pub const LANGUAGE_ID: &str = "bytehost.language";
/// 应用管理页 ID。
pub const APPS_ID: &str = "bytehost.apps";
