//! 引擎无关的浏览器契约。
//!
//! 这一层只描述"产品需要浏览器做什么"，不持有 wry、CEF、NSView、DevTools 会话或任何
//! 产品结构。后端（CEF/wry/测试替身）实现 [`host::BrowserHost`]；上层只看到 target/
//! generation/document/ref/operation，不拿引擎句柄。
//!
//! 关键语义：
//! - `target_generation` 变化使旧 element ref、在途写操作结果失效；
//! - `document_id` 由宿主维护的导航/loader/frame epoch 产生，网页不能自报；
//! - 跨 scope/target/dataset 的 ref 必须在副作用前拒绝；
//! - 事件按序号与 generation 过滤，迟到/重复事件不得驱动 UI 或操作状态。

pub mod action;
pub mod budget;
pub mod capabilities;
pub mod error;
pub mod event;
pub mod host;
pub mod ids;
pub mod observation;
pub mod operation;
pub mod scope;

pub use action::*;
pub use budget::*;
pub use capabilities::*;
pub use error::*;
pub use event::*;
pub use host::*;
pub use ids::*;
pub use observation::*;
pub use operation::*;
pub use scope::*;
