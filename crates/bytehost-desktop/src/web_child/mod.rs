//! **浏览器页面**(不可信内容)的多页签 wry 子视图池。
//!
//! 与 [`crate::native_child`](可信文档:固定 origin、单槽)和 [`crate::app_webview`](应用:按应用 origin 圈定)
//! 是三种不同的信任模型,**句柄表互不共用**。这里承载任意网页(含 `file://`),所以:
//! 不注册任何自定义协议、不注入脚本、不装 IPC 处理器(wry 仍会注入一个空的 `window.ipc` 壳,但宿主这头没有任何处理器,页面 `postMessage` 会抛错——Dozer 真机实测);导航与新窗口只放行
//! `bytehost_webview::browse` 的白名单;下载一律拒绝;开发者工具关闭。

#[cfg(target_os = "macos")]
mod apple;
pub mod edit_menu;
pub mod plan;
mod pool;

#[cfg(target_os = "macos")]
pub use apple::{install_find_shortcut, install_standard_edit_menu};
pub use edit_menu::{EditItem, standard_edit_menu_items};
pub use plan::{
    ExistingTab, LoadVerdict, NavTracker, PoolAction, WebTabSpec, plan_pool, plan_sync, wry_target,
};
pub use pool::{NAV_STALL_TIMEOUT, WebEvent, WebNav, WebPool};
