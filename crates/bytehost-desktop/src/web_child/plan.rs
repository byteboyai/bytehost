//! 浏览器页签池的**纯逻辑**:动作规划与"导航迟迟没有响应"的追踪。不依赖 wry/iced,可以直接单测。

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

/// 库(`dozer-panel-browser`)交给宿主的"期望的页签":每个页签一个 webview,只有激活者可见。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTabSpec {
    pub id: usize,
    pub url: String,
    pub visible: bool,
}

/// 池里**现有**的页签(宿主记账)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingTab {
    pub id: usize,
    pub url: String,
    pub visible: bool,
}

/// 让池从"现有"走到"期望"需要执行的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolAction {
    Create {
        id: usize,
        url: String,
        visible: bool,
    },
    Navigate {
        id: usize,
        url: String,
    },
    Hide {
        id: usize,
    },
    Show {
        id: usize,
    },
    Destroy {
        id: usize,
    },
}

/// 规划(同一代内按 id 复用视图)。顺序固定:**先销毁 → 再隐藏旧的 → 再创建/导航(按期望顺序)→ 最后显示新的**,
/// 这样任何时刻都不会有两个页签同时可见,也不会在销毁之前就创建出同 id 的视图。
///
/// 期望里重复的 id 只取第一个。
pub fn plan_pool(existing: &[ExistingTab], desired: &[WebTabSpec]) -> Vec<PoolAction> {
    let mut seen = HashSet::new();
    let desired: Vec<&WebTabSpec> = desired.iter().filter(|t| seen.insert(t.id)).collect();
    let wanted: HashSet<usize> = desired.iter().map(|t| t.id).collect();
    let have = |id: usize| existing.iter().find(|e| e.id == id);
    let mut actions = Vec::new();

    for e in existing {
        if !wanted.contains(&e.id) {
            actions.push(PoolAction::Destroy { id: e.id });
        }
    }
    for d in &desired {
        if have(d.id).is_some_and(|e| e.visible) && !d.visible {
            actions.push(PoolAction::Hide { id: d.id });
        }
    }
    for d in &desired {
        match have(d.id) {
            None => actions.push(PoolAction::Create {
                id: d.id,
                url: d.url.clone(),
                visible: d.visible,
            }),
            Some(e) if e.url != d.url => actions.push(PoolAction::Navigate {
                id: d.id,
                url: d.url.clone(),
            }),
            Some(_) => {}
        }
    }
    for d in &desired {
        if have(d.id).is_some_and(|e| !e.visible) && d.visible {
            actions.push(PoolAction::Show { id: d.id });
        }
    }
    actions
}

/// 带"换代"的规划。`generation_changed` 为真(切项目 / 关项目)时,**全部销毁再重建**——
/// 页签 id 是每个 `Tabs` 自己的计数器,不同项目会撞号,按 id 复用会把 A 项目的页面当成 B 项目的。
pub fn plan_sync(
    existing: &[ExistingTab],
    desired: &[WebTabSpec],
    generation_changed: bool,
) -> Vec<PoolAction> {
    if !generation_changed {
        return plan_pool(existing, desired);
    }
    let mut actions: Vec<PoolAction> = existing
        .iter()
        .map(|e| PoolAction::Destroy { id: e.id })
        .collect();
    actions.extend(plan_pool(&[], desired));
    actions
}

/// wry 没有"导航失败"回调:访问不可达地址时 `Started`/`Finished` 都不会触发(探针 S1 实测),
/// 地址栏已经变了而页面没变,宿主无从知道。这里追踪"发出了导航、却迟迟没有开始",超时就报告一次。
///
/// 注意 wry 的 `Started` 对应"服务器已响应、页面提交",所以**响应很慢的站点也会被报告**——
/// 报告的含义是"没有响应",不是"一定失败";之后若页面真的开始加载,宿主应撤掉提示。
#[derive(Debug)]
pub struct NavTracker {
    timeout: Duration,
    pending: BTreeMap<usize, (String, Instant)>,
}

impl NavTracker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            pending: BTreeMap::new(),
        }
    }

    /// 对页签 `id` 发出了一次导航(创建时带的初始 URL 也算)。同一页签的新导航会重新计时、替换 URL。
    pub fn navigate(&mut self, id: usize, url: String, now: Instant) {
        self.pending.insert(id, (url, now));
    }

    /// 页签 `id` 的页面开始加载(`on_page_load` 的 `Started`):清掉挂起的导航。没有挂起的就忽略。
    pub fn started(&mut self, id: usize) {
        self.pending.remove(&id);
    }

    /// 页签被销毁。
    pub fn forget(&mut self, id: usize) {
        self.pending.remove(&id);
    }

    /// 取走所有已经等了不少于超时时间的导航(按 id 升序)。每次导航至多报告一次。
    pub fn poll(&mut self, now: Instant) -> Vec<(usize, String)> {
        let due: Vec<usize> = self
            .pending
            .iter()
            .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= self.timeout)
            .map(|(id, _)| *id)
            .collect();
        due.into_iter()
            .filter_map(|id| self.pending.remove(&id).map(|(url, _)| (id, url)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn spec(id: usize, url: &str, visible: bool) -> WebTabSpec {
        WebTabSpec {
            id,
            url: url.into(),
            visible,
        }
    }
    fn have(id: usize, url: &str, visible: bool) -> ExistingTab {
        ExistingTab {
            id,
            url: url.into(),
            visible,
        }
    }

    /// 把动作应用到"现有状态"上,得到池执行完之后应有的状态——用来验证规划结果**收敛到期望**。
    fn apply(existing: &[ExistingTab], actions: &[PoolAction]) -> Vec<(usize, String, bool)> {
        let mut state: Vec<(usize, String, bool)> = existing
            .iter()
            .map(|t| (t.id, t.url.clone(), t.visible))
            .collect();
        for a in actions {
            match a {
                PoolAction::Destroy { id } => state.retain(|t| t.0 != *id),
                PoolAction::Create { id, url, visible } => {
                    assert!(state.iter().all(|t| t.0 != *id), "重复创建 {id}");
                    state.push((*id, url.clone(), *visible));
                }
                PoolAction::Navigate { id, url } => {
                    state
                        .iter_mut()
                        .find(|t| t.0 == *id)
                        .expect("导航不存在的页签")
                        .1 = url.clone();
                }
                PoolAction::Hide { id } => {
                    state
                        .iter_mut()
                        .find(|t| t.0 == *id)
                        .expect("隐藏不存在的页签")
                        .2 = false;
                }
                PoolAction::Show { id } => {
                    state
                        .iter_mut()
                        .find(|t| t.0 == *id)
                        .expect("显示不存在的页签")
                        .2 = true;
                }
            }
        }
        state.sort();
        state
    }
    fn want(desired: &[WebTabSpec]) -> Vec<(usize, String, bool)> {
        let mut v: Vec<_> = desired
            .iter()
            .map(|t| (t.id, t.url.clone(), t.visible))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_empty_pool_creates_every_desired_tab_in_order() {
        let desired = [
            spec(1, "https://a.com/", true),
            spec(2, "https://b.com/", false),
        ];
        assert_eq!(
            plan_pool(&[], &desired),
            vec![
                PoolAction::Create {
                    id: 1,
                    url: "https://a.com/".into(),
                    visible: true
                },
                PoolAction::Create {
                    id: 2,
                    url: "https://b.com/".into(),
                    visible: false
                },
            ]
        );
    }

    #[test]
    fn an_unchanged_pool_is_idle() {
        let existing = [
            have(1, "https://a.com/", true),
            have(2, "https://b.com/", false),
        ];
        let desired = [
            spec(1, "https://a.com/", true),
            spec(2, "https://b.com/", false),
        ];
        assert!(plan_pool(&existing, &desired).is_empty());
    }

    #[test]
    fn a_url_change_only_navigates_and_keeps_the_view() {
        let existing = [have(1, "https://a.com/", true)];
        let desired = [spec(1, "https://a.com/next", true)];
        assert_eq!(
            plan_pool(&existing, &desired),
            vec![PoolAction::Navigate {
                id: 1,
                url: "https://a.com/next".into()
            }]
        );
    }

    #[test]
    fn switching_the_active_tab_hides_the_old_one_before_showing_the_new_one() {
        let existing = [
            have(1, "https://a.com/", true),
            have(2, "https://b.com/", false),
        ];
        let desired = [
            spec(1, "https://a.com/", false),
            spec(2, "https://b.com/", true),
        ];
        assert_eq!(
            plan_pool(&existing, &desired),
            vec![PoolAction::Hide { id: 1 }, PoolAction::Show { id: 2 }]
        );
    }

    #[test]
    fn tabs_that_disappear_are_destroyed_first() {
        let existing = [
            have(1, "https://a.com/", false),
            have(2, "https://b.com/", true),
            have(3, "https://c.com/", false),
        ];
        let desired = [
            spec(1, "https://a.com/", true),
            spec(3, "https://c.com/", false),
        ];
        let actions = plan_pool(&existing, &desired);
        assert_eq!(actions[0], PoolAction::Destroy { id: 2 });
        assert_eq!(apply(&existing, &actions), want(&desired));
    }

    #[test]
    fn closing_the_last_tab_leaves_exactly_one_blank_webview() {
        // 库在关掉最后一个页签后回落到一个新的 about:blank 页签(新 id)。
        let existing = [have(5, "https://a.com/", true)];
        let desired = [spec(6, "about:blank", true)];
        let actions = plan_pool(&existing, &desired);
        assert_eq!(
            actions,
            vec![
                PoolAction::Destroy { id: 5 },
                PoolAction::Create {
                    id: 6,
                    url: "about:blank".into(),
                    visible: true
                },
            ]
        );
        assert_eq!(apply(&existing, &actions).len(), 1);
    }

    #[test]
    fn a_url_change_together_with_a_visibility_change_orders_hide_navigate_show() {
        let existing = [
            have(1, "https://a.com/", true),
            have(2, "https://b.com/", false),
        ];
        let desired = [
            spec(1, "https://a.com/x", false),
            spec(2, "https://b.com/y", true),
        ];
        let actions = plan_pool(&existing, &desired);
        assert_eq!(
            actions,
            vec![
                PoolAction::Hide { id: 1 },
                PoolAction::Navigate {
                    id: 1,
                    url: "https://a.com/x".into()
                },
                PoolAction::Navigate {
                    id: 2,
                    url: "https://b.com/y".into()
                },
                PoolAction::Show { id: 2 },
            ]
        );
        assert_eq!(apply(&existing, &actions), want(&desired));
    }

    #[test]
    fn a_generation_change_destroys_everything_then_recreates() {
        // 切项目:页签 id 是每个项目自己的计数器,会撞号,所以不能按 id 复用旧视图。
        let existing = [
            have(1, "https://a.com/", true),
            have(2, "https://b.com/", false),
        ];
        let desired = [spec(1, "https://other.com/", true)];
        let actions = plan_sync(&existing, &desired, true);
        assert_eq!(
            actions,
            vec![
                PoolAction::Destroy { id: 1 },
                PoolAction::Destroy { id: 2 },
                PoolAction::Create {
                    id: 1,
                    url: "https://other.com/".into(),
                    visible: true
                },
            ]
        );
        assert_eq!(apply(&existing, &actions), want(&desired));
        // 同一代:按 id 复用。
        assert_eq!(
            plan_sync(&existing, &desired, false),
            plan_pool(&existing, &desired)
        );
    }

    #[test]
    fn duplicate_desired_ids_use_the_first_spec_and_never_create_twice() {
        let desired = [
            spec(1, "https://a.com/", true),
            spec(1, "https://dup.com/", false),
        ];
        let actions = plan_pool(&[], &desired);
        assert_eq!(
            actions,
            vec![PoolAction::Create {
                id: 1,
                url: "https://a.com/".into(),
                visible: true
            }]
        );
    }

    #[test]
    fn planning_always_converges_to_the_desired_state() {
        // 一组手工挑的状态对(含增删、换 URL、换可见),任意一对规划后应用都等于期望。
        let states: Vec<Vec<WebTabSpec>> = vec![
            vec![],
            vec![spec(1, "about:blank", true)],
            vec![
                spec(1, "https://a.com/", true),
                spec(2, "https://b.com/", false),
            ],
            vec![
                spec(1, "https://a.com/", false),
                spec(2, "https://b.com/", true),
            ],
            vec![
                spec(2, "https://b.com/z", true),
                spec(3, "file:///x/a.html", false),
            ],
            vec![spec(7, "https://c.com/", true)],
        ];
        for from in &states {
            let existing: Vec<ExistingTab> =
                from.iter().map(|t| have(t.id, &t.url, t.visible)).collect();
            for to in &states {
                let actions = plan_pool(&existing, to);
                assert_eq!(apply(&existing, &actions), want(to), "{from:?} -> {to:?}");
            }
        }
    }

    // —— NavTracker:wry 没有"导航失败"回调 ——

    const T: Duration = Duration::from_secs(10);

    #[test]
    fn a_navigation_that_never_starts_is_reported_as_stalled_once() {
        let t0 = Instant::now();
        let mut tracker = NavTracker::new(T);
        tracker.navigate(1, "https://dead.example/".into(), t0);
        assert!(tracker.poll(t0 + T - Duration::from_millis(1)).is_empty());
        assert_eq!(
            tracker.poll(t0 + T),
            vec![(1, "https://dead.example/".to_string())]
        );
        assert!(tracker.poll(t0 + T * 5).is_empty(), "同一次导航只报告一次");
    }

    #[test]
    fn a_started_navigation_is_never_reported() {
        let t0 = Instant::now();
        let mut tracker = NavTracker::new(T);
        tracker.navigate(1, "https://a.com/".into(), t0);
        tracker.started(1);
        assert!(tracker.poll(t0 + T * 10).is_empty());
    }

    #[test]
    fn a_new_navigation_restarts_the_clock_and_replaces_the_url() {
        let t0 = Instant::now();
        let mut tracker = NavTracker::new(T);
        tracker.navigate(1, "https://a.com/".into(), t0);
        tracker.navigate(1, "https://b.com/".into(), t0 + Duration::from_secs(8));
        assert!(tracker.poll(t0 + T).is_empty(), "新导航重新计时");
        assert_eq!(
            tracker.poll(t0 + Duration::from_secs(18)),
            vec![(1, "https://b.com/".to_string())]
        );
    }

    #[test]
    fn forgetting_a_tab_drops_its_pending_navigation() {
        let t0 = Instant::now();
        let mut tracker = NavTracker::new(T);
        tracker.navigate(1, "https://a.com/".into(), t0);
        tracker.forget(1);
        assert!(tracker.poll(t0 + T * 2).is_empty());
    }

    #[test]
    fn tabs_stall_independently_and_are_reported_in_id_order() {
        let t0 = Instant::now();
        let mut tracker = NavTracker::new(T);
        tracker.navigate(3, "https://c.com/".into(), t0);
        tracker.navigate(1, "https://a.com/".into(), t0);
        tracker.navigate(2, "https://b.com/".into(), t0 + Duration::from_secs(5));
        tracker.started(2);
        assert_eq!(
            tracker.poll(t0 + T),
            vec![
                (1, "https://a.com/".to_string()),
                (3, "https://c.com/".to_string())
            ]
        );
        tracker.started(9); // 没有挂起的导航:忽略,不 panic
    }
}
