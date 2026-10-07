//! bytehost 应用宿主的无界面部分:应用模型、安装计划、生命周期状态、注册表。
//!
//! 设计见 `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`。本 crate **不依赖任何 dozer crate**
//! (门禁:`scripts/check-bytehost-apps-deps.sh`),也不含界面/wry/iced。
//!
//! Cargo features(默认全空,只有类型与纯逻辑,依赖仅 serde/serde_json):
//! - `manifest-toml`:`Manifest::from_toml`
//! - `digest`:`digest` 模块(SHA-256、源码目录摘要、数据存储标识)
//! - `server`:`gateway`、`runtime`、`manager`、`port`、`process`(只有 dozerd 打开;隐含 `digest` 与 `manifest-toml`)

#[cfg(feature = "digest")]
pub mod digest;
pub mod event;
#[cfg(feature = "server")]
pub mod gateway;
pub mod id;
#[cfg(feature = "server")]
pub mod manager;
pub mod manifest;
pub mod permissions;
pub mod plan;
#[cfg(feature = "server")]
pub mod port;
#[cfg(feature = "server")]
pub mod process;
pub mod proto;
pub mod registry;
#[cfg(feature = "server")]
pub mod runtime;
pub mod state;
#[cfg(all(test, feature = "server"))]
mod testutil;

/// 应用宿主的 API 版本:manifest 的 `min_host_version` 比较的是它,**不是**宿主产品(Dozer/Digger)自己的版本号。
pub const HOST_VERSION: id::Version = id::Version::new(0, 1, 0);
