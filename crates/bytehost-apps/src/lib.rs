//! bytehost 应用宿主的无界面部分:应用模型、安装计划、生命周期状态、注册表。
//!
//! 设计见 `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`。本 crate **不依赖任何 dozer crate**
//! (门禁:`scripts/check-bytehost-apps-deps.sh`),也不含界面/wry/iced。
//!
//! Cargo features(默认全空,只有类型与纯逻辑,依赖仅 serde/serde_json):
//! - `manifest-toml`:`Manifest::from_toml`
//! - `digest`:`digest` 模块(SHA-256、源码目录摘要、数据存储标识)

#[cfg(feature = "digest")]
pub mod digest;
pub mod event;
pub mod id;
pub mod manifest;
pub mod permissions;
pub mod plan;
pub mod registry;
pub mod state;
