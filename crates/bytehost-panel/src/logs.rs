//! 应用日志查看器的共享状态机(A6f Task 4)。
//!
//! 应用面板的崩溃/依赖失败页与应用设置页都用同一台机器:展开时读一次,之后每
//! [`REFRESH_INTERVAL`] 刷新一次,**保留旧文本**直到新文本到(不回到 `Loading` 闪烁),
//! 刷新失败在旧文本上打标(`Failed` 只用于首次加载失败)。
//!
//! 两个宿主不各写一遍;宿主只负责:把 [`LogsEffect::Fetch`] 映射成自己带 slot / id 的
//! Effect、把结果回喂 [`LogsState::loaded`]、以及在已有的"下一拍唤醒"里调
//! [`LogsState::tick`]。查看器不可见时不调 `tick` 即可(不后台空转)。

use std::time::{Duration, Instant};

/// 自动刷新间隔。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// 每次请求的行数上限(服务端还会再夹一次)。
pub const FETCH_LINES: u32 = 200;

/// 查看器要宿主去做的事。宿主把它映射成自己的 Effect(带上目标应用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogsEffect {
    /// 去读一次日志末尾(`FETCH_LINES` 行)。
    Fetch,
}

/// 查看器该画什么。
#[derive(Debug, Clone, PartialEq)]
pub enum LogsView {
    /// 未展开。
    Hidden,
    /// 首次加载中(刷新时不回到这里)。
    Loading,
    /// 已读到:`truncated` 表示只显示了末尾若干行;`stale` 表示这是刷新失败后保留的旧内容。
    Loaded {
        text: String,
        truncated: bool,
        stale: bool,
    },
    /// 首次加载失败(原因留在页面里,不弹 Notice)。
    Failed(String),
}

impl LogsView {
    /// 是否展开着(非 `Hidden`)——宿主据此决定要不要刷新、要不要 `reset`。
    pub fn is_open(&self) -> bool {
        !matches!(self, LogsView::Hidden)
    }
}

/// 查看器的内部状态机。
#[derive(Debug, Default)]
pub struct LogsState {
    view: Option<LogsView>,
    /// 已发出、还没回来的那次读取(用于丢弃迟到结果)。
    in_flight: bool,
    last_fetch: Option<Instant>,
    /// 是否贴底跟随(默认跟随)。用户手动上滚即置否,滚回底部再置是。
    follow: bool,
    /// 下一次重绘后要把滚动位置钉到底部(一次性位,消费即复位)。
    scroll_pending: bool,
}

impl LogsState {
    /// 当前该画什么。从未展开过时是 `Hidden`。
    pub fn view(&self) -> LogsView {
        self.view.clone().unwrap_or(LogsView::Hidden)
    }

    /// 展开:发一次读取并进入 `Loading`;已展开(在途 / 已加载)时重复调用不重发。
    pub fn show(&mut self, now: Instant) -> Vec<LogsEffect> {
        match self.view {
            Some(LogsView::Loading) | Some(LogsView::Loaded { .. }) => Vec::new(),
            _ => {
                self.view = Some(LogsView::Loading);
                self.in_flight = true;
                self.last_fetch = Some(now);
                // 每次重新展开都从"贴底跟随"开始。
                self.follow = true;
                vec![LogsEffect::Fetch]
            }
        }
    }

    /// 收起。迟到的结果会被 [`LogsState::loaded`] 丢弃。
    pub fn hide(&mut self) {
        self.view = Some(LogsView::Hidden);
        self.in_flight = false;
    }

    /// 复位到未展开、且下次 `show` 会重新读取(不显示旧内容)。
    pub fn reset(&mut self) {
        self.view = None;
        self.in_flight = false;
        self.last_fetch = None;
    }

    /// 读取结果。只接收在途的那次:收起/复位后迟到的结果一律丢弃。
    pub fn loaded(
        &mut self,
        now: Instant,
        result: Result<(String, bool), String>,
    ) -> Vec<LogsEffect> {
        if !self.in_flight {
            return Vec::new();
        }
        self.in_flight = false;
        self.last_fetch = Some(now);
        let view = match result {
            Ok((text, truncated)) => LogsView::Loaded {
                text,
                truncated,
                stale: false,
            },
            Err(reason) => match self.view.as_ref() {
                // 已有内容:保留旧文本,在其上标"刷新失败"。
                Some(LogsView::Loaded {
                    text,
                    truncated,
                    stale: _,
                }) => LogsView::Loaded {
                    text: text.clone(),
                    truncated: *truncated,
                    stale: true,
                },
                // 首次加载就失败。
                _ => LogsView::Failed(reason),
            },
        };
        let was_loaded = matches!(view, LogsView::Loaded { .. });
        self.view = Some(view);
        // 有内容且还贴底:下一帧把滚动钉到底部(用户上滚过则不动)。
        if was_loaded && self.follow {
            self.scroll_pending = true;
        }
        Vec::new()
    }

    /// 用户滚动了查看器:贴底则继续跟随,离底则停止跟随。
    pub fn on_scrolled(&mut self, at_bottom: bool) {
        self.follow = at_bottom;
    }

    /// 取走"把滚动钉到底部"的一次性请求(消费即复位)。
    pub fn take_scroll(&mut self) -> bool {
        std::mem::take(&mut self.scroll_pending)
    }

    /// 到点就刷新:展开着、距上次 >= [`REFRESH_INTERVAL`]、无在途 → 发一次读取。
    /// `Hidden` 时永远返回空(不后台空转)。
    pub fn tick(&mut self, now: Instant) -> Vec<LogsEffect> {
        if !self.view.as_ref().is_some_and(LogsView::is_open) {
            return Vec::new();
        }
        if self.in_flight {
            return Vec::new();
        }
        match self.last_fetch {
            Some(t) if now.duration_since(t) < REFRESH_INTERVAL => Vec::new(),
            _ => {
                self.in_flight = true;
                self.last_fetch = Some(now);
                vec![LogsEffect::Fetch]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn show_fetches_once_and_enters_loading() {
        let mut s = LogsState::default();
        let now = t0();
        assert_eq!(s.view(), LogsView::Hidden);
        assert_eq!(s.show(now), vec![LogsEffect::Fetch]);
        assert_eq!(s.view(), LogsView::Loading);
        // 在途时重复 show / tick 不重发。
        assert_eq!(s.show(now), Vec::new());
        assert_eq!(s.tick(now), Vec::new());
    }

    #[test]
    fn loaded_then_tick_refreshes_after_the_interval_and_keeps_old_text() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        s.loaded(now, Ok(("a".into(), false)));
        assert_eq!(
            s.view(),
            LogsView::Loaded {
                text: "a".into(),
                truncated: false,
                stale: false
            }
        );
        // 间隔未到不发。
        assert_eq!(s.tick(now + REFRESH_INTERVAL / 2), Vec::new());
        // 到点发一次,仍是旧文本(不回 Loading)。
        assert_eq!(s.tick(now + REFRESH_INTERVAL), vec![LogsEffect::Fetch]);
        assert_eq!(
            s.view(),
            LogsView::Loaded {
                text: "a".into(),
                truncated: false,
                stale: false
            }
        );
    }

    #[test]
    fn a_late_result_after_hiding_is_dropped() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        s.hide();
        s.loaded(now, Ok(("late".into(), false)));
        assert_eq!(s.view(), LogsView::Hidden);
        // 收起后 tick 永远为空。
        assert_eq!(s.tick(now + REFRESH_INTERVAL * 10), Vec::new());
    }

    #[test]
    fn reset_drops_old_content_and_refetches_on_next_show() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        s.loaded(now, Ok(("old".into(), false)));
        s.reset();
        assert_eq!(s.view(), LogsView::Hidden);
        assert_eq!(s.show(now + REFRESH_INTERVAL), vec![LogsEffect::Fetch]);
        assert_eq!(s.view(), LogsView::Loading);
    }

    #[test]
    fn a_refresh_failure_keeps_the_old_text_but_first_failure_is_failed() {
        let mut s = LogsState::default();
        let now = t0();
        // 首次失败 → Failed。
        s.show(now);
        s.loaded(now, Err("挂了".into()));
        assert_eq!(s.view(), LogsView::Failed("挂了".into()));
        // 有旧文本后刷新失败 → 保留文本并标 stale。
        let mut s2 = LogsState::default();
        s2.show(now);
        s2.loaded(now, Ok(("keep".into(), true)));
        s2.tick(now + REFRESH_INTERVAL);
        s2.loaded(now + REFRESH_INTERVAL, Err("又挂了".into()));
        assert_eq!(
            s2.view(),
            LogsView::Loaded {
                text: "keep".into(),
                truncated: true,
                stale: true
            }
        );
    }

    #[test]
    fn identical_results_leave_the_view_unchanged() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        s.loaded(now, Ok(("x".into(), false)));
        let before = s.view();
        s.tick(now + REFRESH_INTERVAL);
        s.loaded(now + REFRESH_INTERVAL, Ok(("x".into(), false)));
        assert_eq!(s.view(), before);
    }

    #[test]
    fn new_content_asks_to_scroll_to_bottom_while_following() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        assert!(!s.take_scroll(), "加载期间没有内容可滚");
        s.loaded(now, Ok(("a".into(), false)));
        assert!(s.take_scroll());
        // 一次性:取走后不再重复。
        assert!(!s.take_scroll());
    }

    #[test]
    fn scrolling_up_stops_auto_follow_until_back_at_the_bottom() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        s.loaded(now, Ok(("a".into(), false)));
        let _ = s.take_scroll();
        // 用户上滚:不再跟随。
        s.on_scrolled(false);
        s.tick(now + REFRESH_INTERVAL);
        s.loaded(now + REFRESH_INTERVAL, Ok(("a\nb".into(), false)));
        assert!(!s.take_scroll(), "上滚后新内容不应把用户拉回底部");
        // 滚回底部:恢复跟随。
        s.on_scrolled(true);
        s.tick(now + REFRESH_INTERVAL * 2);
        s.loaded(now + REFRESH_INTERVAL * 2, Ok(("a\nb\nc".into(), false)));
        assert!(s.take_scroll());
    }

    #[test]
    fn re_expanding_restarts_following() {
        let mut s = LogsState::default();
        let now = t0();
        s.show(now);
        s.loaded(now, Ok(("a".into(), false)));
        s.on_scrolled(false);
        s.hide();
        s.reset();
        s.show(now + REFRESH_INTERVAL);
        s.loaded(now + REFRESH_INTERVAL, Ok(("fresh".into(), false)));
        assert!(s.take_scroll(), "重新展开默认贴底跟随");
    }
}
