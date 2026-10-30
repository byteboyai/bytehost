//! 共享设置窗口的 iced 视图层。
//!
//! 边界:
//! - 只产生消息 / `Element`,不执行服务、读写文件或访问 Keychain;
//! - 窗口生命周期、主题应用、偏好落盘、安装执行链都在产品适配层;
//! - 产品用自己的 `match PageId` 渲染页面,共享层提供容器、主题页、语言页与应用页。
//!
//! 两种扩展方式:
//! 1. **完整自定义页**:产品注册带自己命名空间的 `PageId`,在容器里直接放入自己的
//!    `Element<M>`;
//! 2. **内置页附加区块**:每个内置页接受 `extra: Vec<Element<M>>`,追加在共享内容之后。

pub mod apps;
pub mod language;
pub mod section;
pub mod shell;
pub mod text;
pub mod theme;

pub use apps::{View as AppsPage, View as AppsView};
pub use language::{LanguageOption, view as language_page};
pub use section::{actions_row, dim, heading, scrollable_body, separator};
pub use shell::{Shell, view as settings_shell};
pub use text::Text;
pub use theme::{DARK, LIGHT, ThemeOption, view as theme_page};
