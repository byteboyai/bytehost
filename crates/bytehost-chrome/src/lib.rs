//! bytehost-chrome:共享窗口骨架。
//!
//! 顶栏、左右两条图标栏、左右两个面板区、分隔线、"列表 | 内容"配对视图、放大态与布局持久化。**以 `PanelId` 为键,
//! 不理解任何具体面板**:产品实现 [`view::ShellHost`] 给出面板清单与 body 渲染,骨架负责其余。
//!
//! 分层(从下往上,每层只依赖下面的):
//! - [`metrics`] / [`geometry`]:纯数据与纯几何,无 iced、无窗口,可直接单测;
//! - [`rail`] / [`anim`]:图标栏的纯逻辑与动画状态机;
//! - [`model`]:无界面的骨架状态机 `ShellModel` + `ShellMsg`;
//! - [`persist`] 负责落盘与读盘消毒;
//! - [`theme`]:把 byteui 的运行时主题读成 `ChromeMetrics`;
//! - [`view`]:iced 控件;
//! - [`platform_macos`]:macOS 红绿灯与全屏的窗口装饰胶水。

pub mod anim;
pub mod geometry;
pub mod metrics;
pub mod model;
pub mod persist;
#[cfg(target_os = "macos")]
pub mod platform_macos;
pub mod rail;
pub mod theme;
pub mod view;

pub use geometry::{Rect, ShellRects, Side};
pub use metrics::ChromeMetrics;
pub use model::{Ctx, Divider, Outcome, PairRects, PaneRules, PanelRules, ShellModel, ShellMsg};
pub use persist::{Defaults, Loaded};
pub use theme::metrics_from_theme;
pub use view::{BodyArea, El, PanelMeta, ShellHost, TopBarSlots, map_event, pair_view, view};
