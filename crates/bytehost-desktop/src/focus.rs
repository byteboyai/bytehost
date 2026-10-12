//! 独立原生窗口共用的"是否该因失焦而关闭"判定(计划 Task 10 clause 158)。
//!
//! winit 在窗口刚创建时会无条件排一个合成 `Focused(false)`,必须**先收到过
//! 真 `Focused(true)`、再收到 `Focused(false)`** 才算真正失焦。本模块与产品
//! 无关:只看聚焦历史,不认识任何面板/业务类型。

/// 焦点转移的纯判定:`(新的 focused 状态, 是否该因失焦关闭)`。
pub fn focus_transition(was_focused: bool, now_focused: bool) -> (bool, bool) {
    if now_focused {
        (true, false)
    } else if was_focused {
        (false, true)
    } else {
        (false, false)
    }
}

/// 记录一扇独立窗口的真实聚焦历史,`handle_focus` 返回"这次事件是否该
/// 触发关闭"。
#[derive(Default)]
pub struct FocusTracker {
    focused: bool,
}

impl FocusTracker {
    /// 记录一次焦点事件,返回"是否该因失焦而关闭"。合成的首个
    /// `Focused(false)`(以及任何还没真聚焦过就来的 `Focused(false)`)
    /// 返回 `false`,不触发关闭。
    pub fn handle_focus(&mut self, focused: bool) -> bool {
        let (new_focused, should_close) = focus_transition(self.focused, focused);
        self.focused = new_focused;
        should_close
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_transition_ignores_synthetic_false_and_closes_on_real_loss() {
        assert_eq!(focus_transition(false, false), (false, false));
        assert_eq!(focus_transition(false, true), (true, false));
        assert_eq!(focus_transition(true, false), (false, true));
        assert_eq!(focus_transition(true, true), (true, false));
    }

    #[test]
    fn handle_focus_tracks_real_focus_and_reports_close_only_on_real_loss() {
        let mut t = FocusTracker::default();
        // 合成的首个 Focused(false):不关闭。
        assert!(!t.handle_focus(false));
        // 真聚焦:不关闭。
        assert!(!t.handle_focus(true));
        // 真失焦:关闭。
        assert!(t.handle_focus(false));
    }
}
