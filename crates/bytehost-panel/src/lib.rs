//! 应用面板与日志查看器的状态机:与界面框架无关,以 [`AppKey`] 泛型标识应用,
//! 供 wry 宿主(dozer)与 Tauri 宿主(Digger)共用。
//!
//! - [`key`] 是 [`AppKey`] trait(dozer 用进程内槽号,Digger 可直接用 `AppId`)。
//! - [`notice`] 是宿主无关的瞬时消息等级([`NoticeLevel`])。
//! - [`logs`] 是共享的日志查看器状态机([`LogsState`]/[`LogsView`]/[`LogsEffect`])。
//! - [`host`] 是应用面板状态机([`PanelState`]/[`PanelMessage`]/[`PanelEffect`])。
//!
//! 本 crate **不依赖任何界面框架**(iced/wry/tauri)、也不依赖任何 dozer crate。

pub mod host;
pub mod key;
pub mod logs;
pub mod notice;
