//! Surface 生命周期执行:把状态机接受的转移落到窗口线程的原生操作。
//!
//! **副作用只在窗口线程**发生。本模块把"要做什么"编码为 `LifecycleAction`,
//! 由宿主在主线程执行(经 `crate::panel`);后台只产出意图(不碰句柄)。
//!
//! 状态机(`bytehost_core::surface::Surface`)决定**是否**转移;本模块把被接受的
//! 转移翻译成具体动作清单。这是"状态与副作用分离"的执行侧。

use bytehost_core::{LogicalRect, SurfaceEvent as CoreEvent};

use crate::panel;

/// 一个生命周期动作:由窗口线程执行对 `panel` 的调用。
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleAction {
    /// 创建原生实例(HTML + rect)。
    Create { html: String, rect: LogicalRect },
    /// 重设 bounds。
    SetBounds(LogicalRect),
    /// 注入脚本(推送/握手)。
    Evaluate(String),
    /// 销毁原生实例。
    Destroy,
    /// 隐藏:把矩形收零(不销毁实例)。
    Hide,
    /// 恢复:重建 bounds。
    Resume(LogicalRect),
}

/// 把被接受的状态机事件翻译成待执行动作。
///
/// - `html_for_create`:创建时的初始 HTML(仅 Created/CreateFailed 重试需要);
/// - `rect`:当前应有的 bounds(由 `geometry::surface_rect` 算出)。
///
/// 返回 `None` 表示该事件在当前状态被忽略(状态机已过滤);空 `Vec` 表示
/// 事件被接受但无原生副作用(如 WaitingReady 的重复事件)。
pub fn actions_for(
    event: &CoreEvent,
    rect: LogicalRect,
    html_for_create: Option<String>,
) -> Vec<LifecycleAction> {
    match event {
        // Created 且带初始 HTML:产出一次原生创建。
        CoreEvent::Created => match html_for_create {
            Some(html) => vec![LifecycleAction::Create { html, rect }],
            None => Vec::new(),
        },
        // CreateFailed 由上层转入 Failed 状态;无原生副作用。
        CoreEvent::CreateFailed { .. } => Vec::new(),
        CoreEvent::Ready => Vec::new(),
        CoreEvent::Hidden => vec![LifecycleAction::Hide],
        CoreEvent::Resumed => vec![LifecycleAction::Resume(rect)],
        CoreEvent::Destroyed => vec![LifecycleAction::Destroy],
        CoreEvent::StaleGeneration { .. } => Vec::new(),
    }
}

/// 在窗口线程对一个**已存在**的原生实例执行动作(单测/示例可用;生产由宿主逐条驱动)。
///
/// 返回该动作是否命中一个存在的原生实例。
///
/// `Create` 不在此处理:创建需要 `window_id` 并产出 iced `Task`,由宿主调用
/// `crate::panel::build_child`(唯一创建入口);本函数对 `Create` 返回 `false`。
pub fn execute_on_handle(slot: bytehost_core::SurfaceSlot, action: &LifecycleAction) -> bool {
    match action {
        LifecycleAction::Create { .. } => false,
        LifecycleAction::SetBounds(r) | LifecycleAction::Resume(r) => panel::set_bounds(slot, *r),
        LifecycleAction::Evaluate(s) => panel::evaluate(slot, s),
        LifecycleAction::Destroy => panel::destroy(slot),
        LifecycleAction::Hide => panel::set_bounds(slot, LogicalRect::ZERO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_core::LogicalRect;

    fn rect() -> LogicalRect {
        LogicalRect {
            x: 0.0,
            y: 44.0,
            width: 800.0,
            height: 556.0,
        }
    }

    #[test]
    fn hidden_produces_hide_action() {
        let a = actions_for(&CoreEvent::Hidden, rect(), None);
        assert_eq!(a, vec![LifecycleAction::Hide]);
    }

    #[test]
    fn resumed_restores_bounds() {
        let a = actions_for(&CoreEvent::Resumed, rect(), None);
        assert_eq!(a, vec![LifecycleAction::Resume(rect())]);
    }

    #[test]
    fn destroyed_produces_destroy() {
        let a = actions_for(&CoreEvent::Destroyed, rect(), None);
        assert_eq!(a, vec![LifecycleAction::Destroy]);
    }

    #[test]
    fn created_with_html_produces_create() {
        let a = actions_for(&CoreEvent::Created, rect(), Some("<html></html>".into()));
        assert_eq!(
            a,
            vec![LifecycleAction::Create {
                html: "<html></html>".into(),
                rect: rect(),
            }]
        );
    }

    #[test]
    fn created_without_html_has_no_side_effect() {
        let a = actions_for(&CoreEvent::Created, rect(), None);
        assert!(a.is_empty(), "无 HTML 的 Created 不产出原生动作");
    }

    #[test]
    fn stale_generation_has_no_action() {
        let a = actions_for(
            &CoreEvent::StaleGeneration {
                generation: bytehost_core::Generation::new(0),
            },
            rect(),
            None,
        );
        assert!(a.is_empty());
    }
}
