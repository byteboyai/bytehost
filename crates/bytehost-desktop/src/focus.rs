//! 独立原生窗口共用的"是否该因失焦而关闭"判定(计划 Task 10 clause 158),
//! 以及产品中立的**键盘焦点去向**路由(计划 Task 21 / 兼容桥 C12)。
//!
//! winit 在窗口刚创建时会无条件排一个合成 `Focused(false)`,必须**先收到过
//! 真 `Focused(true)`、再收到 `Focused(false)`** 才算真正失焦。本模块与产品
//! 无关:只看聚焦历史,不认识任何面板/业务类型。
//!
//! [`FocusRouter`] 把"键盘焦点该去哪儿"抽象成两个中立去向:某扇**原生子视图**
//! (用一个不透明句柄 [`FocusTarget::Native`] 指代,宿主不解其业务含义)或
//! **窗口本身**(终端)。产品只负责把业务意图(哪个面板的 webview)解成这个
//! 不透明句柄后 `set`,宿主负责记录 `current`/`pending` 并 `apply`。

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

/// 键盘焦点去向:某扇原生子视图(产品给出的**不透明键** `K`,宿主不解其业务
/// 含义)或窗口本身。
///
/// `K` 是产品自定的焦点键(如"哪个面板/标签的 webview")。宿主只把它拿去问产品
/// 要一个原生子视图句柄,自己不解析它的构成。用泛型承载键类型,宿主侧零业务依赖。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FocusTarget<K> {
    /// 聚焦由产品键 `K` 指代的原生子视图。
    Native(K),
    /// 聚焦宿主窗口本身(终端/默认态)。
    #[default]
    Window,
}

/// 产品中立的焦点路由:记录当前焦点去向与一个待处理去向,并提供统一的
/// `apply`(由宿主在派发完消息后调用,拿到"把产品键解成原生子视图"的解析器
/// 与窗口聚焦能力)。
///
/// 产品设焦点只调 [`set`](Self::set);宿主查询当前落点用
/// [`current_native`](Self::current_native)(对应"原生编辑器是否该收键盘"这类判断),
/// 消费待处理去向用 [`apply`](Self::apply)——把 `K` 解成原生子视图句柄
/// ([`resolve_native`](Self::apply) 的第一个回调),解不出则回落窗口。
#[derive(Debug)]
pub struct FocusRouter<K> {
    pending: Option<FocusTarget<K>>,
    current: FocusTarget<K>,
}

impl<K> Default for FocusRouter<K> {
    fn default() -> Self {
        Self {
            pending: None,
            current: FocusTarget::Window,
        }
    }
}

impl<K> FocusRouter<K> {
    /// 设一个待处理 + 当前去向(与迁移前的 `pending_focus = Some(x); current_focus = x;`
    /// 一一对应)。不立刻执行聚焦,等宿主 `apply`。
    pub fn set(&mut self, target: FocusTarget<K>)
    where
        K: Clone,
    {
        self.pending = Some(target.clone());
        self.current = target;
    }

    /// 只更新当前去向、不排待处理聚焦(与迁移前"只改 `current_focus` 不改
    /// `pending_focus`"的调用点对应)。
    pub fn set_current(&mut self, target: FocusTarget<K>) {
        self.current = target;
    }

    /// 当前去向。
    pub fn current(&self) -> &FocusTarget<K> {
        &self.current
    }

    /// 当前是否落在某扇原生子视图上;是则返回产品键。
    pub fn current_native(&self) -> Option<&K> {
        match &self.current {
            FocusTarget::Native(k) => Some(k),
            FocusTarget::Window => None,
        }
    }

    /// 取出并清空待处理去向。
    pub fn take_pending(&mut self) -> Option<FocusTarget<K>> {
        self.pending.take()
    }
}

impl<K> FocusRouter<K> {
    /// 消费待处理去向:对 [`FocusTarget::Native`] 用 `resolve_native(&key)` 把产品键
    /// 解成原生子视图句柄并 `focus_native(handle)`(返回是否命中——未命中回落窗口),
    /// 对 [`FocusTarget::Window`] 直接 `focus_window()`。
    pub fn apply<R, F>(
        &mut self,
        resolve_native: R,
        mut focus_native: F,
        mut focus_window: impl FnMut(),
    ) where
        R: FnOnce(&K) -> Option<u64>,
        F: FnMut(u64) -> bool,
    {
        match self.pending.take() {
            Some(FocusTarget::Native(k)) => match resolve_native(&k) {
                Some(handle) if focus_native(handle) => {}
                _ => focus_window(),
            },
            Some(FocusTarget::Window) => focus_window(),
            None => {}
        }
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

    #[test]
    fn focus_router_keeps_current_and_applies_pending_native() {
        let mut r: FocusRouter<u8> = FocusRouter::default();
        assert_eq!(*r.current(), FocusTarget::Window);
        assert!(r.current_native().is_none());
        r.set(FocusTarget::Native(7));
        assert_eq!(r.current_native(), Some(&7));
        let mut focused = Vec::new();
        let mut window = 0;
        r.apply(|k| Some(*k as u64), |h| { focused.push(h); true }, || window += 1);
        assert_eq!(focused, vec![7]);
        assert_eq!(window, 0);
        // 待处理已清空:再 apply 不产生动作。
        r.apply(|k| Some(*k as u64), |h| { focused.push(h); true }, || window += 1);
        assert_eq!(focused, vec![7]);
    }

    #[test]
    fn focus_router_falls_back_to_window_when_native_missing() {
        let mut r: FocusRouter<u8> = FocusRouter::default();
        r.set(FocusTarget::Native(3));
        let mut focused = 0;
        let mut window = 0;
        r.apply(|_| None, |_| { focused += 1; false }, || window += 1);
        assert_eq!(focused, 0);
        assert_eq!(window, 1);
        // 解析到句柄但聚焦失败:也回落窗口。
        r.set(FocusTarget::Native(3));
        r.apply(|k| Some(*k as u64), |_| { focused += 1; false }, || window += 1);
        assert_eq!(focused, 1);
        assert_eq!(window, 2);
    }

    #[test]
    fn focus_router_window_target_focuses_window() {
        let mut r: FocusRouter<u8> = FocusRouter::default();
        r.set(FocusTarget::Native(1));
        r.set(FocusTarget::Window);
        assert!(r.current_native().is_none());
        let mut window = 0;
        r.apply(|k| Some(*k as u64), |_| true, || window += 1);
        assert_eq!(window, 1);
    }

    #[test]
    fn set_current_does_not_schedule_pending_focus() {
        let mut r: FocusRouter<u8> = FocusRouter::default();
        r.set_current(FocusTarget::Native(9));
        let mut focused = 0;
        r.apply(|k| Some(*k as u64), |_| { focused += 1; true }, || {});
        assert_eq!(focused, 0);
    }
}
