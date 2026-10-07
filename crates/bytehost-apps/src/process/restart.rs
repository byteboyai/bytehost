//! 崩溃后的重启策略(纯函数,时间由调用方注入)。
//!
//! 规则:每次退出记一笔;一次**稳定运行**(≥ `stable_after`)就清掉历史(它不是崩溃循环);否则按指数退避
//! (`base * 2^(n-1)`,封顶 `cap`)重启,窗口 `window` 内的退出次数超过 `max_restarts` 就**放弃**——
//! 由调用方把应用标成 `Failed{ retryable: false }`,不在后台无限地重启一个起不来的应用。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub window: Duration,
    pub base: Duration,
    pub cap: Duration,
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            window: Duration::from_secs(10 * 60),
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    RestartAfter(Duration),
    GiveUp,
}

#[derive(Debug, Default)]
pub struct RestartTracker {
    exits: VecDeque<Instant>,
}

impl RestartTracker {
    /// 进程在 `now` 退出,它这次跑了 `ran_for`。
    pub fn on_exit(&mut self, policy: &RestartPolicy, now: Instant, ran_for: Duration) -> Decision {
        if ran_for >= policy.stable_after {
            self.exits.clear();
        }
        while self
            .exits
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > policy.window)
        {
            self.exits.pop_front();
        }
        self.exits.push_back(now);
        let n = self.exits.len() as u32;
        if n > policy.max_restarts {
            return Decision::GiveUp;
        }
        let factor = 1u32.checked_shl(n - 1).unwrap_or(u32::MAX);
        Decision::RestartAfter(policy.base.saturating_mul(factor).min(policy.cap))
    }

    /// 用户手动启动/停止后清掉历史(重新开始计数)。
    pub fn reset(&mut self) {
        self.exits.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> RestartPolicy {
        RestartPolicy {
            max_restarts: 3,
            window: Duration::from_secs(100),
            base: Duration::from_secs(1),
            cap: Duration::from_secs(5),
            stable_after: Duration::from_secs(30),
        }
    }

    #[test]
    fn crashes_back_off_exponentially_up_to_the_cap_then_give_up() {
        let mut t = RestartTracker::default();
        let t0 = Instant::now();
        let quick = Duration::from_millis(100);
        assert_eq!(
            t.on_exit(&p(), t0, quick),
            Decision::RestartAfter(Duration::from_secs(1))
        );
        assert_eq!(
            t.on_exit(&p(), t0, quick),
            Decision::RestartAfter(Duration::from_secs(2))
        );
        assert_eq!(
            t.on_exit(&p(), t0, quick),
            Decision::RestartAfter(Duration::from_secs(4))
        );
        assert_eq!(
            t.on_exit(&p(), t0, quick),
            Decision::GiveUp,
            "窗口内第 4 次"
        );
    }

    #[test]
    fn the_backoff_is_capped() {
        let mut t = RestartTracker::default();
        let policy = RestartPolicy {
            max_restarts: 20,
            ..p()
        };
        let t0 = Instant::now();
        let mut last = Duration::ZERO;
        for _ in 0..10 {
            if let Decision::RestartAfter(d) = t.on_exit(&policy, t0, Duration::ZERO) {
                last = d;
            }
        }
        assert_eq!(last, policy.cap);
    }

    #[test]
    fn a_stable_run_clears_the_history() {
        let mut t = RestartTracker::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            t.on_exit(&p(), t0, Duration::ZERO);
        }
        assert_eq!(
            t.on_exit(&p(), t0, Duration::from_secs(31)),
            Decision::RestartAfter(Duration::from_secs(1)),
            "跑稳之后的退出重新从第一次算"
        );
    }

    #[test]
    fn old_exits_fall_out_of_the_window() {
        let mut t = RestartTracker::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            t.on_exit(&p(), t0, Duration::ZERO);
        }
        let later = t0 + Duration::from_secs(101);
        assert_eq!(
            t.on_exit(&p(), later, Duration::ZERO),
            Decision::RestartAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn reset_starts_counting_again() {
        let mut t = RestartTracker::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            t.on_exit(&p(), t0, Duration::ZERO);
        }
        t.reset();
        assert_eq!(
            t.on_exit(&p(), t0, Duration::ZERO),
            Decision::RestartAfter(Duration::from_secs(1))
        );
    }
}
