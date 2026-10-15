//! bytehost-chrome:共享窗口骨架。(完整的 crate 文档见 Task 10 的最终 lib.rs)

pub mod geometry;
pub mod metrics;

pub use geometry::{Rect, ShellRects, Side};
pub use metrics::ChromeMetrics;
pub mod rail;
pub mod anim;
