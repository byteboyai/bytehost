//! 应用面板的宿主逻辑(bytehost A4b2):dozerd 里已安装应用的列表轮询、每个应用面板的状态机、启动/停止、
//! "该给 webview 的地址"。**纯状态机**:`update`/`poll_if_due` 只返回 [`Effect`],由 `App` 去执行
//! (发请求、同步 rail、写 `AppViews`、弹 Notice)——这样整套逻辑可以不开窗口、不连 dozerd 地测。
//!
//! 约定(规格 A4 与 A4b 评审留下的):
//! - **轮询,不做推送**:应用面板可见时每 [`POLL_INTERVAL`] 拉一次 `List`;没有应用面板可见时只在启动后拉一次,
//!   以及面板切入、启停操作完成后各拉一次;
//! - **启动地址含令牌(秘密)**:每次面板切入都清掉旧地址、重新取(webview 因切面板/切项目被销毁重建时不会拿到
//!   过期地址);地址只经 `PanelEffect::SetUrl` 进内存里的 `AppViews`,不进日志、不落盘;
//! - **瞬时失败走 Notice,持久状态留在面板里**:启停/取地址失败是"刚发生的一件事"→ Notice;宿主不可用
//!   (`AppErrorKind::Unavailable`)是"当前处于某状态"→ 面板里显示原因,不弹 Notice;
//! - 列表请求的传输层失败(连不上 dozerd)不清空已知的应用、不弹 Notice——顶栏的 dozerd 徽标负责告知。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use bytehost_apps::id::Version;
use bytehost_apps::proto::{AppErrorKind, AppIssue, AppSummary};
use bytehost_apps::state::{DesiredState, ObservedState};

use crate::key::AppKey;
use crate::logs as app_logs;
use crate::notice::NoticeLevel;

/// 应用面板可见、且**订阅未就绪**时拉 `List` 的间隔(兜底轮询的快速档)。
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 订阅已 `Live` 时拉 `List` 的兜底间隔:推送是主力,这只是防止极小概率的漏推。
pub const SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// 订阅断开后重订阅的退避序列(封顶取最后一个)。
pub const RESUBSCRIBE_BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// 一次请求的失败:宿主带类别的失败,或者传输/协议层的(连不上 dozerd、协议版本不符)。
/// 与 [`bytehost_client::AppApiError`] 同形(同一套 `Host`/`Transport`),直接复用。
pub use bytehost_client::AppApiError as Failure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Start,
    Stop,
}

#[derive(Debug, Clone)]
pub enum PanelMessage<K: AppKey> {
    ListLoaded(Result<Vec<AppSummary>, Failure>),
    LaunchUrlLoaded(K, Result<String, Failure>),
    ActionDone(K, Act, Result<(), Failure>),
    /// 用户点了面板里的「启动」/「停止」。
    Start(K),
    Stop(K),
    /// 该应用面板被切到前台(图标栏点击或程序化显示)。
    PanelShown(K),
    /// 别处(设置里的安装/停止/卸载)改了已安装集合或运行状态:立刻拉一次列表。
    Refresh,
    /// 用户点了崩溃页的「查看日志」。
    ShowLogs(K),
    /// 用户点了「收起日志」。
    HideLogs(K),
    /// 日志读取的结果。
    LogsLoaded(K, Result<(String, bool), Failure>),
    /// 用户滚动了日志查看器:`true` = 当前贴底。
    LogsScrolled(K, bool),
    /// 用户点了运行时问题页的「去设置安装」。
    OpenRuntimeSettings,
    /// 订阅已建立(dozerd 回了 `Subscribed`)。
    Subscribed,
    /// 收到一条变更失效信号(`Changed`/`Resync`)——都只是"去重拉一次列表"。
    Changed,
    /// 订阅连接断了 / 重订阅失败:回退到快速轮询并按退避重订阅。
    SubscriptionLost,
}

/// 状态机要 `App` 去做的事。
#[derive(Debug, Clone, PartialEq)]
pub enum PanelEffect<K: AppKey> {
    FetchList,
    FetchLaunchUrl(K),
    StartApp(K),
    StopApp(K),
    /// 已安装集合变了(或第一次拿到):`App::sync_installed_apps`。
    SyncRail(Vec<K>),
    SetUrl(K, String),
    ClearUrl(K),
    /// 读某应用日志末尾(`max_lines` 由状态机定,服务端还会再夹一次)。
    FetchLogs(K, u32),
    /// 打开 设置 → 应用 页(运行时问题页的「去设置安装」)。
    OpenSettingsApps,
    /// 建立应用变更订阅(`AppHostApi::subscribe`)。
    Subscribe,
    /// 瞬时提示(宿主映射到自己的 Notice;`key` 用于去重)。
    Notice {
        level: NoticeLevel,
        text: String,
        key: String,
    },
}

/// 崩溃页下方日志查看器的状态(应用面板对外的视图)。
///
/// 实际的刷新逻辑在共享状态机 [`crate::logs::LogsState`] 里(与设置页共用);
/// 这里保留 A6e 就有的两字段形状,`stale`(刷新失败保留旧文本)只影响设置页的呈现。
#[derive(Debug, Clone, PartialEq)]
pub enum LogsView {
    /// 未展开。
    Hidden,
    /// 正在读。
    Loading,
    /// 已读到:`truncated` 表示只显示了末尾若干行。
    Loaded { text: String, truncated: bool },
    /// 读取失败(原因留在页面里,不弹 Notice)。
    Failed(String),
}

impl From<crate::logs::LogsView> for LogsView {
    fn from(v: crate::logs::LogsView) -> Self {
        use crate::logs::LogsView as L;
        match v {
            L::Hidden => LogsView::Hidden,
            L::Loading => LogsView::Loading,
            L::Loaded {
                text, truncated, ..
            } => LogsView::Loaded { text, truncated },
            L::Failed(reason) => LogsView::Failed(reason),
        }
    }
}

/// 一个应用在 dozerd 里的当前状况(只留面板要用的)。
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: String,
    name: String,
    #[allow(dead_code)] // 目前面板只看观察态;期望态留给 A4c 的"开机自启"之类展示
    desired: DesiredState,
    observed: ObservedState,
    /// 生效版本:升级后即使 `observed` 都是 `Running`,版本变了也要重载面板(A6g Task 4)。
    version: Version,
    /// 崩溃/起不来时的"看得懂的问题"(运行时缺失/版本不符);仅 `Failed` 时带出。
    issue: Option<AppIssue>,
}

/// 整个列表的状况(`PanelState::phase` 为 `None` = 还没拿到过列表)。
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Loaded,
    /// 应用宿主本身不可用(原因给人看)。
    Unavailable(String),
    /// 最近一次请求在传输层失败;已知的应用保留。
    Disconnected,
}

/// 变更订阅的状态。推送是主力、轮询是兜底:订阅就绪时轮询降到
/// [`SAFETY_POLL_INTERVAL`],断开时回到 [`POLL_INTERVAL`] 并按 [`RESUBSCRIBE_BACKOFF`] 重订阅。
#[derive(Debug, Clone, PartialEq, Default)]
enum Subscription {
    /// 还没发起第一次订阅。
    #[default]
    Idle,
    /// 已发 `Subscribe`(或重订阅),等 `Subscribed` 确认。
    Pending,
    /// 订阅就绪:变更靠推送失效信号驱动。
    Live,
    /// 订阅断了,到 `retry_at` 再试;`attempt` 是退避档位(封顶在最后一档)。
    Backoff { attempt: usize, retry_at: Instant },
}

/// 面板该画什么(`PanelState::view_model` 的结果)。
#[derive(Debug, Clone, PartialEq)]
pub enum PanelView {
    /// 应用宿主不可用,带原因。
    HostUnavailable(String),
    /// 还没拿到列表。
    Loading,
    /// 连不上 dozerd,又没有任何已知的应用。
    Disconnected,
    /// 列表里已经没有这个应用(刚被卸载)。
    Missing,
    Stopped,
    /// 过渡态,文案给人看("启动中…"/"停止中…")。
    Busy(&'static str),
    /// 应用在跑,正在取启动地址。
    Opening,
    /// 取启动地址失败了(再点一次面板图标重试)。
    OpenFailed,
    /// 应用在跑且地址已就绪:webview 盖在面板上,这里只是它下面的底。
    Running,
    /// 应用崩溃/启动失败,带原因。
    Crashed(String),
    /// 应用因运行时缺失/版本不符而起不来:显示专门的提示页(规格 §6.3)。
    RuntimeIssue(AppIssue),
}

#[derive(Debug)]
pub struct PanelState<K: AppKey> {
    rows: HashMap<K, Row>,
    order: Vec<K>,
    phase: Option<Phase>,
    in_flight: bool,
    last_poll: Option<Instant>,
    /// 正在取启动地址的应用。
    launching: HashSet<K>,
    /// 取启动地址失败过的应用(面板切入时清掉才会再试,避免每次轮询都重试)。
    launch_failed: HashSet<K>,
    /// 已经把启动地址交给 `AppViews` 的应用。
    url_set: HashSet<K>,
    acting: HashMap<K, Act>,
    /// 每个应用崩溃页下方日志查看器的状态(共享状态机,A6f Task 4)。应用离开 `Failed` 时复位。
    logs: HashMap<K, app_logs::LogsState>,
    /// 已经成功拉到过一次列表(并因此同步过图标栏)。首次必须无条件同步:磁盘里的布局可能留着已卸载应用的
    /// 条目,而 `order` 的初值也是空,不能靠"集合变了"来触发。
    synced_once: bool,
    /// 变更订阅的状态(推送取代轮询的主力)。
    subscription: Subscription,
    /// 订阅/推送期间又来了变更,但当时有 `List` 在途:等它回来再补发一次(合并风暴)。
    dirty: bool,
}

impl<K: AppKey> Default for PanelState<K> {
    fn default() -> Self {
        Self {
            rows: HashMap::new(),
            order: Vec::new(),
            phase: None,
            in_flight: false,
            last_poll: None,
            launching: HashSet::new(),
            launch_failed: HashSet::new(),
            url_set: HashSet::new(),
            acting: HashMap::new(),
            logs: HashMap::new(),
            synced_once: false,
            subscription: Subscription::default(),
            dirty: false,
        }
    }
}

impl<K: AppKey> PanelState<K> {
    /// 连不上 dozerd(传输层失败):没有应用面板可见时也要按间隔重试,连上后才恢复安静。
    fn disconnected(&self) -> bool {
        matches!(self.phase, Some(Phase::Disconnected))
    }

    /// 要不要排下一拍唤醒:从没拉过(启动后的第一次,顺带发起首次订阅)、有应用面板可见(持续轮询)、
    /// 连不上 dozerd(自愈),或者订阅正处于退避(到点要重订阅)。订阅 `Live`/`Pending` 时不需要
    /// 空转——`Subscribed`/`Changed` 由订阅任务经 proxy 事件驱动,不依赖定时器。
    pub fn poll_wanted(&self, app_panel_visible: bool) -> bool {
        if self.in_flight {
            return false;
        }
        self.last_poll.is_none()
            || app_panel_visible
            || self.disconnected()
            || matches!(self.subscription, Subscription::Backoff { .. })
    }

    /// 下一拍唤醒的间隔:订阅 `Live` 时降到兜底档 [`SAFETY_POLL_INTERVAL`],否则用快速档 [`POLL_INTERVAL`]。
    pub fn poll_interval(&self) -> Duration {
        if matches!(self.subscription, Subscription::Live) {
            SAFETY_POLL_INTERVAL
        } else {
            POLL_INTERVAL
        }
    }

    /// `ResumeTimeReached` 时调用:到点就拉一次列表。**订阅的发起/重订阅不走这里**
    /// (见 [`PanelState::subscribe_if_due`]),所以这里的行为与 A6e 完全一致。
    pub fn poll_if_due(&mut self, now: Instant, visible: &[K]) -> Vec<PanelEffect<K>> {
        if self.in_flight {
            return Vec::new();
        }
        let due = match self.last_poll {
            None => true,
            Some(t) => {
                (!visible.is_empty() || self.disconnected())
                    && now.duration_since(t) >= self.poll_interval()
            }
        };
        if !due {
            return Vec::new();
        }
        self.begin_fetch(now);
        vec![PanelEffect::FetchList]
    }

    /// 订阅的发起/重订阅:还没订就订一次,退避到点再订一次;`Live`/`Pending` 时什么都不做。
    /// 与列表轮询分开,保证轮询逻辑与 A6e 逐字一致。
    pub fn subscribe_if_due(&mut self, now: Instant) -> Vec<PanelEffect<K>> {
        match &self.subscription {
            Subscription::Idle => {
                self.subscription = Subscription::Pending;
                vec![PanelEffect::Subscribe]
            }
            Subscription::Pending | Subscription::Live => Vec::new(),
            Subscription::Backoff { retry_at, .. } if now >= *retry_at => {
                self.subscription = Subscription::Pending;
                vec![PanelEffect::Subscribe]
            }
            Subscription::Backoff { .. } => Vec::new(),
        }
    }

    fn begin_fetch(&mut self, now: Instant) {
        self.in_flight = true;
        self.last_poll = Some(now);
    }

    pub fn update(
        &mut self,
        msg: PanelMessage<K>,
        now: Instant,
        visible: &[K],
    ) -> Vec<PanelEffect<K>> {
        match msg {
            PanelMessage::ListLoaded(result) => self.list_loaded(result, visible, now),
            PanelMessage::LaunchUrlLoaded(slot, result) => self.launch_url_loaded(slot, result),
            PanelMessage::Start(slot) => self.act(slot, Act::Start),
            PanelMessage::Stop(slot) => self.act(slot, Act::Stop),
            PanelMessage::ActionDone(slot, act, result) => self.action_done(slot, act, result, now),
            PanelMessage::Refresh => {
                if self.in_flight {
                    return Vec::new();
                }
                self.begin_fetch(now);
                vec![PanelEffect::FetchList]
            }
            PanelMessage::PanelShown(slot) => {
                self.launch_failed.remove(&slot);
                let mut effects = Vec::new();
                if self.url_set.remove(&slot) {
                    effects.push(PanelEffect::ClearUrl(slot));
                }
                if !self.in_flight {
                    self.begin_fetch(now);
                    effects.push(PanelEffect::FetchList);
                }
                effects
            }
            PanelMessage::ShowLogs(slot) => self.show_logs(slot, now),
            PanelMessage::HideLogs(slot) => {
                self.logs.entry(slot).or_default().hide();
                Vec::new()
            }
            PanelMessage::LogsLoaded(slot, result) => {
                // 只接收仍在等的那次读取:共享状态机内部会丢弃收起/复位后迟到的结果。
                let mapped = result.map_err(|f| f.text().to_owned());
                self.logs.entry(slot).or_default().loaded(now, mapped);
                Vec::new()
            }
            PanelMessage::LogsScrolled(slot, at_bottom) => {
                self.logs.entry(slot).or_default().on_scrolled(at_bottom);
                Vec::new()
            }
            PanelMessage::OpenRuntimeSettings => vec![PanelEffect::OpenSettingsApps],
            PanelMessage::Subscribed => {
                self.subscription = Subscription::Live;
                // 刚订上:断线/退避期间可能漏了变更,立刻重拉一次(有在途则合并)。
                self.request_refetch(now)
            }
            PanelMessage::Changed => self.request_refetch(now),
            PanelMessage::SubscriptionLost => {
                // 退避档位:第一次丢失用第 0 档,重订阅再失败逐档递增(封顶最后一档)。
                let attempt = match self.subscription {
                    Subscription::Backoff { attempt, .. } => {
                        (attempt + 1).min(RESUBSCRIBE_BACKOFF.len() - 1)
                    }
                    _ => 0,
                };
                self.subscription = Subscription::Backoff {
                    attempt,
                    retry_at: now + RESUBSCRIBE_BACKOFF[attempt],
                };
                Vec::new()
            }
        }
    }

    /// 收到变更失效信号(或刚订阅上):有 `List` 在途就记 `dirty`,否则立刻重拉。
    fn request_refetch(&mut self, now: Instant) -> Vec<PanelEffect<K>> {
        if self.in_flight {
            self.dirty = true;
            return Vec::new();
        }
        self.begin_fetch(now);
        vec![PanelEffect::FetchList]
    }

    /// 取走"把滚动钉到底部"的一次性请求:返回待滚到底的那个槽(消费即复位)。
    /// 同一时刻通常只有一个查看器展开,遍历全部以容纳极端情况。
    pub fn take_log_scroll(&mut self) -> Option<K> {
        self.logs
            .iter_mut()
            .find_map(|(slot, s)| s.take_scroll().then_some(*slot))
    }

    /// 读日志:未展开时置 `Loading` 并发一次请求;在途时重复点不重发(逻辑在共享状态机)。
    fn show_logs(&mut self, slot: K, now: Instant) -> Vec<PanelEffect<K>> {
        let slot_effects = self.logs.entry(slot).or_default().show(now);
        slot_effects
            .into_iter()
            .map(|_| PanelEffect::FetchLogs(slot, app_logs::FETCH_LINES))
            .collect()
    }

    fn list_loaded(
        &mut self,
        result: Result<Vec<AppSummary>, Failure>,
        visible: &[K],
        now: Instant,
    ) -> Vec<PanelEffect<K>> {
        self.in_flight = false;
        let apps = match result {
            Ok(apps) => apps,
            Err(Failure::Host(f)) if f.kind == AppErrorKind::Unavailable => {
                self.phase = Some(Phase::Unavailable(f.message));
                return Vec::new();
            }
            Err(Failure::Host(f)) => {
                // 其他类别的列表失败(理论上只有 Internal):保留已知应用,弹一条去重的 Notice。
                return vec![PanelEffect::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("读取应用列表失败:{}", f.message),
                    key: "app_host:list".into(),
                }];
            }
            Err(Failure::Transport(_)) => {
                self.phase = Some(Phase::Disconnected);
                return Vec::new();
            }
        };
        self.phase = Some(Phase::Loaded);

        let mut effects = Vec::new();
        let mut rows = HashMap::new();
        let mut order = Vec::new();
        for app in apps {
            // 形状非法的 id 不会出现(宿主已校验);万一槽表满了就跳过这个应用,不让整个列表失败。
            let Some(slot) = K::from_app_id(&app.id) else {
                continue;
            };
            order.push(slot);
            rows.insert(
                slot,
                Row {
                    id: app.id.as_str().to_owned(),
                    name: app.name,
                    desired: app.desired,
                    observed: app.observed,
                    version: app.version,
                    issue: app.issue,
                },
            );
        }
        // 消失的应用:撤掉它的地址与在途状态。
        let gone: Vec<K> = self
            .rows
            .keys()
            .filter(|s| !rows.contains_key(*s))
            .copied()
            .collect();
        for slot in gone {
            self.forget(slot, &mut effects);
        }
        // 不再运行的应用:撤掉地址(webview 随之被池回收)。
        for (slot, row) in &rows {
            if row.observed != ObservedState::Running {
                self.launching.remove(slot);
                self.launch_failed.remove(slot);
                if self.url_set.remove(slot) {
                    effects.push(PanelEffect::ClearUrl(*slot));
                }
            }
            // 离开崩溃态(被重启/运行/停止):旧日志查看器复位,防陈旧。
            if !matches!(row.observed, ObservedState::Failed { .. }) {
                self.logs.entry(*slot).or_default().reset();
            }
        }
        // 升级后版本变了:即使两次 `List` 之间 `observed` 都是 `Running`,面板上盖着的
        // 还是旧版本的页面,必须当作"重新启动"——撤掉旧地址、重新取一次(webview 重载)。
        // 首次出现(没有上一行)不算变化。
        for (slot, row) in &rows {
            let Some(old) = self.rows.get(slot) else {
                continue;
            };
            if old.version != row.version {
                self.launching.remove(slot);
                self.launch_failed.remove(slot);
                if self.url_set.remove(slot) {
                    effects.push(PanelEffect::ClearUrl(*slot));
                }
            }
        }
        let set_changed = !self.synced_once || order != self.order;
        self.synced_once = true;
        self.rows = rows;
        self.order = order;
        if set_changed {
            effects.push(PanelEffect::SyncRail(self.order.clone()));
        }
        // 可见的应用在跑、还没有地址:去取一个(左右两栏可以同时显示两个应用面板,每个都要取)。
        for &slot in visible {
            if self
                .rows
                .get(&slot)
                .is_some_and(|r| r.observed == ObservedState::Running)
                && !self.url_set.contains(&slot)
                && !self.launching.contains(&slot)
                && !self.launch_failed.contains(&slot)
            {
                self.launching.insert(slot);
                effects.push(PanelEffect::FetchLaunchUrl(slot));
            }
        }
        // 拉列表期间又来了变更(合并风暴):补发一次,确保拿到最新状态。`in_flight` 已在开头清掉。
        if self.dirty {
            self.dirty = false;
            self.begin_fetch(now);
            effects.push(PanelEffect::FetchList);
        }
        effects
    }

    fn forget(&mut self, slot: K, effects: &mut Vec<PanelEffect<K>>) {
        self.launching.remove(&slot);
        self.launch_failed.remove(&slot);
        self.acting.remove(&slot);
        self.logs.remove(&slot);
        if self.url_set.remove(&slot) {
            effects.push(PanelEffect::ClearUrl(slot));
        }
    }

    fn launch_url_loaded(
        &mut self,
        slot: K,
        result: Result<String, Failure>,
    ) -> Vec<PanelEffect<K>> {
        self.launching.remove(&slot);
        let still_running = self
            .rows
            .get(&slot)
            .is_some_and(|r| r.observed == ObservedState::Running);
        match result {
            Ok(url) if still_running => {
                self.url_set.insert(slot);
                vec![PanelEffect::SetUrl(slot, url)]
            }
            // 应用在取地址期间停了:丢掉这个(已经没用的)地址。
            Ok(_) => Vec::new(),
            Err(failure) => {
                self.launch_failed.insert(slot);
                vec![PanelEffect::Notice {
                    level: NoticeLevel::Error,
                    text: format!("打开应用 {} 失败:{}", self.name(slot), failure.text()),
                    key: format!("app_host:launch:{}", self.id(slot)),
                }]
            }
        }
    }

    fn act(&mut self, slot: K, act: Act) -> Vec<PanelEffect<K>> {
        if self.acting.contains_key(&slot) || !self.rows.contains_key(&slot) {
            return Vec::new();
        }
        self.acting.insert(slot, act);
        vec![match act {
            Act::Start => PanelEffect::StartApp(slot),
            Act::Stop => PanelEffect::StopApp(slot),
        }]
    }

    fn action_done(
        &mut self,
        slot: K,
        act: Act,
        result: Result<(), Failure>,
        now: Instant,
    ) -> Vec<PanelEffect<K>> {
        self.acting.remove(&slot);
        let mut effects = Vec::new();
        if let Err(failure) = result {
            let verb = match act {
                Act::Start => "启动",
                Act::Stop => "停止",
            };
            effects.push(PanelEffect::Notice {
                level: NoticeLevel::Error,
                text: format!("{verb}应用 {} 失败:{}", self.name(slot), failure.text()),
                key: format!("app_host:act:{}", self.id(slot)),
            });
        }
        // 不管成败都立刻刷新一次,面板马上反映真实状态。
        if !self.in_flight {
            self.begin_fetch(now);
            effects.push(PanelEffect::FetchList);
        }
        effects
    }

    fn name(&self, slot: K) -> String {
        self.rows
            .get(&slot)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| slot.app_id().to_owned())
    }

    fn id(&self, slot: K) -> String {
        match self.rows.get(&slot) {
            Some(r) => r.id.clone(),
            None => slot.app_id().to_owned(),
        }
    }

    /// 面板该画什么。
    pub fn view_model(&self, slot: K) -> PanelView {
        if let Some(Phase::Unavailable(reason)) = &self.phase {
            return PanelView::HostUnavailable(reason.clone());
        }
        let Some(row) = self.rows.get(&slot) else {
            return match self.phase {
                None => PanelView::Loading,
                // 连不上 dozerd 时什么都不知道——不能说"应用已不在列表里"。
                Some(Phase::Disconnected) => PanelView::Disconnected,
                Some(_) => PanelView::Missing,
            };
        };
        match self.acting.get(&slot) {
            Some(Act::Start) => return PanelView::Busy("启动中…"),
            Some(Act::Stop) => return PanelView::Busy("停止中…"),
            None => {}
        }
        match &row.observed {
            ObservedState::Running => {
                if self.url_set.contains(&slot) {
                    PanelView::Running
                } else if self.launch_failed.contains(&slot) {
                    PanelView::OpenFailed
                } else {
                    PanelView::Opening
                }
            }
            ObservedState::Preparing | ObservedState::Starting | ObservedState::Updating => {
                PanelView::Busy("启动中…")
            }
            ObservedState::Stopping | ObservedState::Uninstalling => PanelView::Busy("停止中…"),
            ObservedState::Failed { reason, .. } => match &row.issue {
                // 有"看得懂的问题"→ 专门的运行时提示页;否则普通崩溃页(可看日志)。
                Some(issue) => PanelView::RuntimeIssue(issue.clone()),
                None => PanelView::Crashed(reason.clone()),
            },
            ObservedState::Installed | ObservedState::Stopped | ObservedState::NotInstalled => {
                PanelView::Stopped
            }
        }
    }

    /// 崩溃页下方日志查看器的当前状态(未知应用按 `Hidden`)。
    pub fn logs_view(&self, slot: K) -> LogsView {
        self.logs
            .get(&slot)
            .map(|s| s.view().into())
            .unwrap_or(LogsView::Hidden)
    }

    /// 到点刷新所有展开着的日志查看器(崩溃页/依赖失败页),返回要执行的 `FetchLogs`。
    /// 没有展开的查看器、或其面板不在 `visible` 里时为空(不后台空转)。
    pub fn tick_logs(&mut self, now: Instant, visible: &[K]) -> Vec<PanelEffect<K>> {
        let mut effects = Vec::new();
        for (&slot, state) in self.logs.iter_mut() {
            if !visible.contains(&slot) {
                continue;
            }
            for _ in state.tick(now) {
                effects.push(PanelEffect::FetchLogs(slot, app_logs::FETCH_LINES));
            }
        }
        effects
    }

    /// 有面板可见、且展开着日志查看器吗(据此决定要不要排下一拍唤醒)。
    pub fn any_logs_open(&self, visible: &[K]) -> bool {
        self.logs
            .iter()
            .any(|(slot, s)| visible.contains(slot) && s.view().is_open())
    }

    /// 某应用的显示名(面板标题行用);未知时退回 id。
    pub fn display_name(&self, slot: K) -> String {
        self.name(slot)
    }
}

/// 把解释器名映射成给人看的运行时名。
fn runtime_display_name(runtime: &str) -> &str {
    match runtime {
        "node" | "nodejs" => "Node.js",
        "python" | "python3" => "Python",
        "uv" => "uv",
        other => other,
    }
}

/// 运行时问题页的标题与详情(纯函数,便于表驱动测试)。
pub fn issue_texts(issue: &AppIssue) -> (String, String) {
    match issue {
        AppIssue::RuntimeMissing { runtime } => (
            format!("需要 {}", runtime_display_name(runtime)),
            "尚未安装,可在 设置 → 应用 里一键安装".into(),
        ),
        AppIssue::RuntimeVersion {
            runtime,
            required,
            found,
        } => (
            format!("需要 {}", runtime_display_name(runtime)),
            format!("要求 {required},当前 {found}"),
        ),
        AppIssue::DependencyInstall { summary } => (
            "依赖安装失败".into(),
            format!("{summary}\n查看日志了解安装输出"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_apps::id::{AppId, Version};
    use bytehost_apps::proto::AppFailure;

    /// 测试替身键:用一张 up-to-256 的小表把 id 映射成 `u8`,非 ASCII/超长 id 映射不了 → `None`。
    /// 生产里 dozer 用进程内槽号、Digger 用 `AppId`,行为由各自的 `AppKey` 实现钉住。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct TestKey(u8);

    const TEST_IDS: [&str; 63] = [
        "hp-a",
        "rail-sync-a",
        "rail-sync-b",
        "launch-a",
        "hidden-a",
        "late-a",
        "fail-a",
        "shown-a",
        "drop-a",
        "drop-b",
        "upg-a",
        "same-a",
        "unavail-a",
        "transport-a",
        "act-a",
        "actfail-a",
        "nobody-a",
        "vm-a",
        "disc-a",
        "two-a",
        "two-b",
        "issue-a",
        "issue-b",
        "issue-c",
        "dep-a",
        "logs-a",
        "logs-late-hide",
        "logs-late-recover",
        "logsfail-a",
        "reset-a",
        "tick-a",
        "hid-a",
        "retry-a",
        "sub-a",
        "lost-a",
        "a",
        "b",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ];

    impl AppKey for TestKey {
        fn from_app_id(id: &AppId) -> Option<Self> {
            TEST_IDS
                .iter()
                .position(|known| !known.is_empty() && *known == id.as_str())
                .and_then(|i| u8::try_from(i).ok())
                .map(TestKey)
        }

        fn app_id(&self) -> &str {
            TEST_IDS[usize::from(self.0)]
        }
    }

    fn slot(id: &str) -> TestKey {
        TestKey::from_app_id(&AppId::new(id).unwrap()).unwrap()
    }

    fn app(id: &str, observed: ObservedState) -> AppSummary {
        AppSummary {
            id: AppId::new(id).unwrap(),
            name: format!("name-{id}"),
            version: Version::new(1, 0, 0),
            desired: DesiredState::Running,
            observed,
            url: None,
            issue: None,
            previous_version: None,
            rollback_note: None,
        }
    }

    fn app_with_issue(id: &str, observed: ObservedState, issue: AppIssue) -> AppSummary {
        AppSummary {
            issue: Some(issue),
            ..app(id, observed)
        }
    }

    fn failed(reason: &str) -> ObservedState {
        ObservedState::Failed {
            reason: reason.into(),
            retryable: true,
        }
    }

    fn runtime_version_issue() -> AppIssue {
        AppIssue::RuntimeVersion {
            runtime: "python3".into(),
            required: ">=99".into(),
            found: "3.13.0".into(),
        }
    }

    fn running(id: &str) -> AppSummary {
        app(id, ObservedState::Running)
    }

    fn host_failure(kind: AppErrorKind, message: &str) -> Failure {
        Failure::Host(AppFailure::new(kind, message))
    }

    fn loaded(
        state: &mut PanelState<TestKey>,
        apps: Vec<AppSummary>,
        visible: &[TestKey],
    ) -> Vec<PanelEffect<TestKey>> {
        state.update(PanelMessage::ListLoaded(Ok(apps)), Instant::now(), visible)
    }

    /// `from_app_id` 认不出的 id(键表满/映射不了)不进 `order`,也不进 rail;其余应用照常。
    #[test]
    fn an_app_id_the_key_cannot_map_is_skipped_from_the_order() {
        let mut s = PanelState::<TestKey>::default();
        let effects = loaded(
            &mut s,
            vec![
                app("not-in-table", ObservedState::Stopped),
                app("rail-sync-a", ObservedState::Stopped),
            ],
            &[],
        );
        let a = slot("rail-sync-a");
        assert_eq!(
            effects,
            vec![PanelEffect::SyncRail(vec![a])],
            "认不出的 id 不进 order/rail"
        );
    }

    #[test]
    fn the_first_poll_is_due_and_later_ones_only_while_an_app_panel_is_visible() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        assert!(s.poll_wanted(false), "启动后至少拉一次");
        assert_eq!(s.poll_if_due(t0, &[]), vec![PanelEffect::FetchList]);
        assert!(s.poll_if_due(t0, &[]).is_empty(), "在途时不重复发");
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);
        assert!(!s.poll_wanted(false), "没有应用面板可见就不再轮询");
        assert!(s.poll_if_due(t0 + POLL_INTERVAL * 5, &[]).is_empty());
        let a = [slot("hp-a")];
        assert!(s.poll_wanted(true));
        assert!(
            s.poll_if_due(t0 + POLL_INTERVAL / 2, &a).is_empty(),
            "没到点"
        );
        assert_eq!(
            s.poll_if_due(t0 + POLL_INTERVAL, &a),
            vec![PanelEffect::FetchList]
        );
    }

    #[test]
    fn the_rail_is_synced_when_the_installed_set_changes_and_not_otherwise() {
        let mut s = PanelState::<TestKey>::default();
        let (a, b) = (slot("rail-sync-a"), slot("rail-sync-b"));
        let first = loaded(
            &mut s,
            vec![app("rail-sync-a", ObservedState::Stopped)],
            &[],
        );
        assert_eq!(first, vec![PanelEffect::SyncRail(vec![a])]);
        assert!(
            loaded(
                &mut s,
                vec![app("rail-sync-a", ObservedState::Stopped)],
                &[]
            )
            .is_empty()
        );
        let both = loaded(
            &mut s,
            vec![
                app("rail-sync-a", ObservedState::Stopped),
                app("rail-sync-b", ObservedState::Stopped),
            ],
            &[],
        );
        assert_eq!(both, vec![PanelEffect::SyncRail(vec![a, b])]);
        let removed = loaded(
            &mut s,
            vec![app("rail-sync-b", ObservedState::Stopped)],
            &[],
        );
        assert_eq!(removed, vec![PanelEffect::SyncRail(vec![b])]);
    }

    #[test]
    fn a_visible_running_app_gets_exactly_one_launch_url_fetch() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("launch-a");
        let first = loaded(&mut s, vec![running("launch-a")], &[a]);
        assert!(first.contains(&PanelEffect::FetchLaunchUrl(a)));
        let again = loaded(&mut s, vec![running("launch-a")], &[a]);
        assert!(
            !again.contains(&PanelEffect::FetchLaunchUrl(a)),
            "在途时不重复取"
        );
        assert_eq!(s.view_model(a), PanelView::Opening);
        let url = "http://launch-a.localhost:20001/?bh_token=t".to_string();
        let done = s.update(
            PanelMessage::LaunchUrlLoaded(a, Ok(url.clone())),
            Instant::now(),
            &[a],
        );
        assert_eq!(done, vec![PanelEffect::SetUrl(a, url)]);
        assert_eq!(s.view_model(a), PanelView::Running);
        let later = loaded(&mut s, vec![running("launch-a")], &[a]);
        assert!(later.is_empty(), "已有地址就不再取");
    }

    #[test]
    fn a_running_app_whose_panel_is_not_visible_is_not_opened() {
        let mut s = PanelState::<TestKey>::default();
        let effects = loaded(&mut s, vec![running("hidden-a")], &[]);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, PanelEffect::FetchLaunchUrl(_)))
        );
    }

    #[test]
    fn a_launch_url_that_arrives_after_the_app_stopped_is_dropped() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("late-a");
        loaded(&mut s, vec![running("late-a")], &[a]);
        loaded(&mut s, vec![app("late-a", ObservedState::Stopped)], &[a]);
        let done = s.update(
            PanelMessage::LaunchUrlLoaded(a, Ok("http://late-a.localhost:1/".into())),
            Instant::now(),
            &[a],
        );
        assert!(done.is_empty());
        assert_eq!(s.view_model(a), PanelView::Stopped);
    }

    #[test]
    fn a_failed_launch_notices_once_and_is_not_retried_until_the_panel_is_shown_again() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("fail-a");
        loaded(&mut s, vec![running("fail-a")], &[a]);
        let failed = s.update(
            PanelMessage::LaunchUrlLoaded(a, Err(host_failure(AppErrorKind::Conflict, "不在运行"))),
            Instant::now(),
            &[a],
        );
        assert_eq!(failed.len(), 1);
        match &failed[0] {
            PanelEffect::Notice { level, text, key } => {
                assert_eq!(*level, NoticeLevel::Error);
                assert!(
                    text.contains("name-fail-a") && text.contains("不在运行"),
                    "{text}"
                );
                assert_eq!(key, "app_host:launch:fail-a");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.view_model(a), PanelView::OpenFailed);
        let poll = loaded(&mut s, vec![running("fail-a")], &[a]);
        assert!(
            !poll
                .iter()
                .any(|e| matches!(e, PanelEffect::FetchLaunchUrl(_))),
            "轮询不重试"
        );
        // 再次切入面板:清状态并重新拉列表,列表回来后再取地址。
        let shown = s.update(PanelMessage::PanelShown(a), Instant::now(), &[a]);
        assert_eq!(shown, vec![PanelEffect::FetchList]);
        let after = loaded(&mut s, vec![running("fail-a")], &[a]);
        assert!(after.contains(&PanelEffect::FetchLaunchUrl(a)));
    }

    #[test]
    fn refresh_fetches_the_list_unless_one_is_already_in_flight() {
        let mut s = PanelState::<TestKey>::default();
        assert_eq!(
            s.update(PanelMessage::Refresh, Instant::now(), &[]),
            vec![PanelEffect::FetchList]
        );
        assert!(
            s.update(PanelMessage::Refresh, Instant::now(), &[])
                .is_empty()
        );
    }

    #[test]
    fn showing_the_panel_discards_the_old_address_so_a_fresh_one_is_fetched() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("shown-a");
        loaded(&mut s, vec![running("shown-a")], &[a]);
        s.update(
            PanelMessage::LaunchUrlLoaded(a, Ok("http://shown-a.localhost:1/?bh_token=old".into())),
            Instant::now(),
            &[a],
        );
        let shown = s.update(PanelMessage::PanelShown(a), Instant::now(), &[a]);
        assert_eq!(
            shown,
            vec![PanelEffect::ClearUrl(a), PanelEffect::FetchList]
        );
        assert_eq!(s.view_model(a), PanelView::Opening);
        let again = s.update(PanelMessage::PanelShown(a), Instant::now(), &[a]);
        assert!(again.is_empty(), "已有列表请求在途,不重复发");
    }

    #[test]
    fn an_app_that_stops_or_disappears_loses_its_address() {
        let mut s = PanelState::<TestKey>::default();
        let (a, b) = (slot("drop-a"), slot("drop-b"));
        loaded(&mut s, vec![running("drop-a"), running("drop-b")], &[a]);
        for x in [a, b] {
            s.update(
                PanelMessage::LaunchUrlLoaded(x, Ok(format!("http://{}.localhost:1/", x.app_id()))),
                Instant::now(),
                &[a],
            );
        }
        let effects = loaded(&mut s, vec![app("drop-a", ObservedState::Stopped)], &[a]);
        assert!(effects.contains(&PanelEffect::ClearUrl(a)), "停了");
        assert!(effects.contains(&PanelEffect::ClearUrl(b)), "没了");
        assert!(effects.contains(&PanelEffect::SyncRail(vec![a])));
        assert_eq!(s.view_model(b), PanelView::Missing);
    }

    #[test]
    fn a_version_change_while_still_running_reloads_the_panel() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("upg-a");
        loaded(&mut s, vec![app("upg-a", ObservedState::Running)], &[a]);
        s.update(
            PanelMessage::LaunchUrlLoaded(a, Ok("http://upg-a.localhost:1/?bh_token=old".into())),
            Instant::now(),
            &[a],
        );
        assert_eq!(s.view_model(a), PanelView::Running);
        // 升级:两次都是 Running,只有版本变了。
        let upgraded = AppSummary {
            version: Version::new(2, 0, 0),
            ..running("upg-a")
        };
        let effects = loaded(&mut s, vec![upgraded], &[a]);
        assert!(
            effects.contains(&PanelEffect::ClearUrl(a)),
            "升级后要撤掉旧版本的地址"
        );
        assert!(
            effects.contains(&PanelEffect::FetchLaunchUrl(a)),
            "并立刻重取(webview 重载)"
        );
        assert_eq!(s.view_model(a), PanelView::Opening);
    }

    #[test]
    fn the_same_version_keeps_the_address() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("same-a");
        loaded(&mut s, vec![running("same-a")], &[a]);
        s.update(
            PanelMessage::LaunchUrlLoaded(a, Ok("http://same-a.localhost:1/".into())),
            Instant::now(),
            &[a],
        );
        let effects = loaded(&mut s, vec![running("same-a")], &[a]);
        assert_eq!(effects, Vec::new(), "版本没变就不动地址");
        assert_eq!(s.view_model(a), PanelView::Running);
    }

    #[test]
    fn an_unavailable_host_is_a_persistent_panel_state_not_a_notice() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("unavail-a");
        loaded(&mut s, vec![running("unavail-a")], &[]);
        let effects = s.update(
            PanelMessage::ListLoaded(Err(host_failure(AppErrorKind::Unavailable, "端口被占用"))),
            Instant::now(),
            &[],
        );
        assert!(effects.is_empty());
        assert_eq!(
            s.view_model(a),
            PanelView::HostUnavailable("端口被占用".into())
        );
        loaded(&mut s, vec![running("unavail-a")], &[]);
        assert_eq!(s.view_model(a), PanelView::Opening, "恢复后回到正常状态");
    }

    #[test]
    fn a_transport_failure_keeps_the_known_apps_and_stays_quiet() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("transport-a");
        loaded(
            &mut s,
            vec![app("transport-a", ObservedState::Stopped)],
            &[],
        );
        let effects = s.update(
            PanelMessage::ListLoaded(Err(Failure::Transport("连不上".into()))),
            Instant::now(),
            &[],
        );
        assert!(effects.is_empty());
        assert_eq!(s.view_model(a), PanelView::Stopped);
    }

    #[test]
    fn start_and_stop_are_deduplicated_and_refresh_the_list_when_done() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("act-a");
        loaded(&mut s, vec![app("act-a", ObservedState::Stopped)], &[]);
        s.update(
            PanelMessage::ListLoaded(Ok(vec![app("act-a", ObservedState::Stopped)])),
            Instant::now(),
            &[],
        );
        assert_eq!(
            s.update(PanelMessage::Start(a), Instant::now(), &[]),
            vec![PanelEffect::StartApp(a)]
        );
        assert!(
            s.update(PanelMessage::Start(a), Instant::now(), &[])
                .is_empty()
        );
        assert!(
            s.update(PanelMessage::Stop(a), Instant::now(), &[])
                .is_empty(),
            "在途时也不接受另一个动作"
        );
        assert_eq!(s.view_model(a), PanelView::Busy("启动中…"));
        let done = s.update(
            PanelMessage::ActionDone(a, Act::Start, Ok(())),
            Instant::now(),
            &[],
        );
        assert_eq!(done, vec![PanelEffect::FetchList]);
    }

    #[test]
    fn a_failed_action_notices_with_the_verb_and_still_refreshes() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("actfail-a");
        loaded(&mut s, vec![app("actfail-a", ObservedState::Stopped)], &[]);
        s.update(PanelMessage::Stop(a), Instant::now(), &[]);
        let done = s.update(
            PanelMessage::ActionDone(
                a,
                Act::Stop,
                Err(host_failure(AppErrorKind::Conflict, "状态冲突")),
            ),
            Instant::now(),
            &[],
        );
        assert_eq!(done.len(), 2);
        match &done[0] {
            PanelEffect::Notice { text, key, .. } => {
                assert!(text.contains("停止") && text.contains("状态冲突"), "{text}");
                assert_eq!(key, "app_host:act:actfail-a");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(done[1], PanelEffect::FetchList);
    }

    #[test]
    fn actions_on_unknown_apps_are_ignored() {
        let mut s = PanelState::<TestKey>::default();
        assert!(
            s.update(PanelMessage::Start(slot("nobody-a")), Instant::now(), &[])
                .is_empty()
        );
    }

    #[test]
    fn the_view_model_covers_every_observed_state() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("vm-a");
        let cases = [
            (ObservedState::Installed, PanelView::Stopped),
            (ObservedState::Stopped, PanelView::Stopped),
            (ObservedState::Preparing, PanelView::Busy("启动中…")),
            (ObservedState::Starting, PanelView::Busy("启动中…")),
            (ObservedState::Stopping, PanelView::Busy("停止中…")),
            (
                ObservedState::Failed {
                    reason: "崩了".into(),
                    retryable: false,
                },
                PanelView::Crashed("崩了".into()),
            ),
            (ObservedState::Running, PanelView::Opening),
        ];
        for (observed, want) in cases {
            loaded(&mut s, vec![app("vm-a", observed.clone())], &[]);
            assert_eq!(s.view_model(a), want, "{observed:?}");
        }
        assert_eq!(
            PanelState::<TestKey>::default().view_model(a),
            PanelView::Loading
        );
    }

    /// 磁盘里留着已卸载应用的 `app:<id>` 条目;第一次拉到的列表是空的也必须同步(把它清掉),
    /// 不能因为 `order` 的初值也是空就当成"没变"。
    #[test]
    fn the_first_successful_list_always_syncs_the_rail_even_when_empty() {
        let mut s = PanelState::<TestKey>::default();
        let effects = loaded(&mut s, vec![], &[]);
        assert_eq!(effects, vec![PanelEffect::SyncRail(vec![])]);
        assert!(loaded(&mut s, vec![], &[]).is_empty(), "之后没变就不再同步");
    }

    #[test]
    fn the_first_list_syncs_even_if_an_unavailable_answer_came_first() {
        let mut s = PanelState::<TestKey>::default();
        s.update(
            PanelMessage::ListLoaded(Err(host_failure(AppErrorKind::Unavailable, "端口被占用"))),
            Instant::now(),
            &[],
        );
        let effects = loaded(&mut s, vec![], &[]);
        assert!(
            effects.contains(&PanelEffect::SyncRail(vec![])),
            "{effects:?}"
        );
    }

    /// 连不上 dozerd 而且没有任何已知应用时,面板不能说"这个应用已不在已安装列表里"。
    #[test]
    fn a_disconnected_panel_with_no_known_apps_says_so_instead_of_claiming_the_app_is_gone() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("disc-a");
        s.update(
            PanelMessage::ListLoaded(Err(Failure::Transport("连不上".into()))),
            Instant::now(),
            &[],
        );
        assert_eq!(s.view_model(a), PanelView::Disconnected);
        // 成功拉到列表之后才有资格说"已不在列表里"。
        loaded(&mut s, vec![], &[]);
        assert_eq!(s.view_model(a), PanelView::Missing);
    }

    /// 左右两栏可以同时各显示一个应用面板,两个都要取启动地址。
    #[test]
    fn two_visible_app_panels_each_get_their_launch_url_fetch() {
        let mut s = PanelState::<TestKey>::default();
        let (a, b) = (slot("two-a"), slot("two-b"));
        let effects = loaded(&mut s, vec![running("two-a"), running("two-b")], &[a, b]);
        assert!(effects.contains(&PanelEffect::FetchLaunchUrl(a)));
        assert!(effects.contains(&PanelEffect::FetchLaunchUrl(b)));
        let again = loaded(&mut s, vec![running("two-a"), running("two-b")], &[a, b]);
        assert!(
            !again
                .iter()
                .any(|e| matches!(e, PanelEffect::FetchLaunchUrl(_))),
            "各自在途时不重复取"
        );
    }

    /// dozerd 暂时连不上(启动时没起、被重启中)时,即使没有应用面板可见也要继续按间隔重试,
    /// 连上后恢复安静——否则"启动时没起、之后才起"的 dozerd 永远等不到应用列表。
    #[test]
    fn a_disconnected_host_keeps_polling_until_the_daemon_answers() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        s.poll_if_due(t0, &[]);
        s.update(
            PanelMessage::ListLoaded(Err(Failure::Transport("连不上".into()))),
            t0,
            &[],
        );
        assert!(s.poll_wanted(false), "断开状态要继续排唤醒");
        assert!(
            s.poll_if_due(t0 + POLL_INTERVAL / 2, &[]).is_empty(),
            "没到点"
        );
        assert_eq!(
            s.poll_if_due(t0 + POLL_INTERVAL, &[]),
            vec![PanelEffect::FetchList]
        );
        s.update(
            PanelMessage::ListLoaded(Ok(vec![])),
            t0 + POLL_INTERVAL,
            &[],
        );
        assert!(!s.poll_wanted(false), "连上以后恢复到不轮询");
        assert!(s.poll_if_due(t0 + POLL_INTERVAL * 9, &[]).is_empty());
    }

    // ===== A6e Task 5:运行时问题页 / 日志查看器 / 跳设置 =====

    #[test]
    fn a_failed_app_with_a_runtime_issue_shows_the_issue_page_not_the_crash_page() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("issue-a");
        loaded(
            &mut s,
            vec![app_with_issue(
                "issue-a",
                failed("需要 python3 >=99,当前 3.13.0"),
                runtime_version_issue(),
            )],
            &[],
        );
        assert_eq!(
            s.view_model(a),
            PanelView::RuntimeIssue(runtime_version_issue())
        );

        // 无 issue 的 Failed 仍是普通崩溃页。
        let b = slot("issue-b");
        loaded(&mut s, vec![app("issue-b", failed("崩了"))], &[]);
        assert_eq!(s.view_model(b), PanelView::Crashed("崩了".into()));

        // 依赖安装失败也是一个"看得懂的问题" → 走问题页(不是崩溃页)。
        let d = slot("dep-a");
        let dep = AppIssue::DependencyInstall {
            summary: "npm ci 失败(退出码 1)".into(),
        };
        loaded(
            &mut s,
            vec![app_with_issue(
                "dep-a",
                failed("npm ci 失败(退出码 1)"),
                dep.clone(),
            )],
            &[],
        );
        assert_eq!(s.view_model(d), PanelView::RuntimeIssue(dep));

        // 陈旧问题:应用已经跑起来了,就不该再显示问题页。
        let c = slot("issue-c");
        loaded(
            &mut s,
            vec![app_with_issue(
                "issue-c",
                ObservedState::Running,
                runtime_version_issue(),
            )],
            &[],
        );
        assert_eq!(s.view_model(c), PanelView::Opening);
    }

    #[test]
    fn showing_logs_fetches_once_and_stores_the_result() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("logs-a");
        loaded(&mut s, vec![app("logs-a", failed("崩了"))], &[]);
        assert_eq!(s.logs_view(a), LogsView::Hidden);

        let effects = s.update(PanelMessage::ShowLogs(a), Instant::now(), &[]);
        assert_eq!(effects, vec![PanelEffect::FetchLogs(a, 200)]);
        assert_eq!(s.logs_view(a), LogsView::Loading);
        // 在途时重复点不重复发。
        assert!(
            s.update(PanelMessage::ShowLogs(a), Instant::now(), &[])
                .is_empty()
        );

        s.update(
            PanelMessage::LogsLoaded(a, Ok(("line1\nline2".into(), true))),
            Instant::now(),
            &[],
        );
        assert_eq!(
            s.logs_view(a),
            LogsView::Loaded {
                text: "line1\nline2".into(),
                truncated: true
            }
        );
        assert!(
            s.update(PanelMessage::HideLogs(a), Instant::now(), &[])
                .is_empty()
        );
        assert_eq!(s.logs_view(a), LogsView::Hidden);
    }

    #[test]
    fn a_late_logs_result_after_hiding_does_not_reopen_the_viewer() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("logs-late-hide");
        loaded(&mut s, vec![app("logs-late-hide", failed("崩了"))], &[]);
        s.update(PanelMessage::ShowLogs(a), Instant::now(), &[]);
        s.update(PanelMessage::HideLogs(a), Instant::now(), &[]);
        s.update(
            PanelMessage::LogsLoaded(a, Ok(("late".into(), false))),
            Instant::now(),
            &[],
        );
        assert_eq!(s.logs_view(a), LogsView::Hidden);
    }

    #[test]
    fn a_late_logs_result_after_the_app_recovered_is_dropped_not_kept_stale() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("logs-late-recover");
        loaded(&mut s, vec![app("logs-late-recover", failed("崩了"))], &[]);
        s.update(PanelMessage::ShowLogs(a), Instant::now(), &[]);
        // 应用恢复运行:状态复位。
        loaded(
            &mut s,
            vec![app("logs-late-recover", ObservedState::Running)],
            &[],
        );
        s.update(
            PanelMessage::LogsLoaded(a, Ok(("stale".into(), false))),
            Instant::now(),
            &[],
        );
        assert_eq!(s.logs_view(a), LogsView::Hidden);
        // 下一次崩溃再点「查看日志」要重新读,而不是显示旧内容。
        loaded(
            &mut s,
            vec![app("logs-late-recover", failed("又崩了"))],
            &[],
        );
        assert_eq!(
            s.update(PanelMessage::ShowLogs(a), Instant::now(), &[]),
            vec![PanelEffect::FetchLogs(a, 200)]
        );
    }

    #[test]
    fn logs_failure_is_kept_in_place_not_noticed() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("logsfail-a");
        loaded(&mut s, vec![app("logsfail-a", failed("崩了"))], &[]);
        s.update(PanelMessage::ShowLogs(a), Instant::now(), &[]);
        let effects = s.update(
            PanelMessage::LogsLoaded(a, Err(host_failure(AppErrorKind::NotFound, "没有这个应用"))),
            Instant::now(),
            &[],
        );
        assert!(effects.is_empty(), "日志失败不进 Notice:{effects:?}");
        assert_eq!(s.logs_view(a), LogsView::Failed("没有这个应用".into()));
    }

    #[test]
    fn logs_are_hidden_again_when_the_app_leaves_the_failed_state() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("reset-a");
        loaded(&mut s, vec![app("reset-a", failed("崩了"))], &[]);
        s.update(PanelMessage::ShowLogs(a), Instant::now(), &[]);
        s.update(
            PanelMessage::LogsLoaded(a, Ok(("x".into(), false))),
            Instant::now(),
            &[],
        );
        assert!(matches!(s.logs_view(a), LogsView::Loaded { .. }));

        // 被重启回 Starting/Running:日志查看器复位。
        loaded(&mut s, vec![app("reset-a", ObservedState::Running)], &[]);
        assert_eq!(s.logs_view(a), LogsView::Hidden);
    }

    #[test]
    fn an_open_log_viewer_is_refreshed_on_tick_and_closed_ones_are_not() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("tick-a");
        loaded(&mut s, vec![app("tick-a", failed("崩了"))], &[]);
        assert!(!s.any_logs_open(&[a]));
        let t0 = Instant::now();
        s.update(PanelMessage::ShowLogs(a), t0, &[]);
        s.update(
            PanelMessage::LogsLoaded(a, Ok(("x".into(), false))),
            t0,
            &[],
        );
        assert!(s.any_logs_open(&[a]));
        // 间隔未到不发;到点发一次。
        assert!(
            s.tick_logs(t0 + Duration::from_millis(500), &[a])
                .is_empty()
        );
        assert_eq!(
            s.tick_logs(t0 + app_logs::REFRESH_INTERVAL, &[a]),
            vec![PanelEffect::FetchLogs(a, app_logs::FETCH_LINES)]
        );
        // 收起后不再刷。
        s.update(PanelMessage::HideLogs(a), t0, &[]);
        assert!(!s.any_logs_open(&[a]));
        assert!(
            s.tick_logs(t0 + app_logs::REFRESH_INTERVAL * 5, &[a])
                .is_empty()
        );
    }

    /// 面板不可见(被关掉/切走)时,展开着的日志查看器不刷新、也不要求唤醒;重新可见后立刻续上。
    #[test]
    fn a_log_viewer_whose_panel_is_not_visible_is_not_refreshed() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("hid-a");
        loaded(&mut s, vec![app("hid-a", failed("崩了"))], &[]);
        let t0 = Instant::now();
        s.update(PanelMessage::ShowLogs(a), t0, &[]);
        s.update(
            PanelMessage::LogsLoaded(a, Ok(("x".into(), false))),
            t0,
            &[],
        );
        let later = t0 + app_logs::REFRESH_INTERVAL * 5;
        assert!(s.tick_logs(later, &[]).is_empty());
        assert!(!s.any_logs_open(&[]));
        assert_eq!(
            s.tick_logs(later, &[a]),
            vec![PanelEffect::FetchLogs(a, app_logs::FETCH_LINES)]
        );
    }

    #[test]
    fn open_runtime_settings_emits_exactly_one_effect() {
        let mut s = PanelState::<TestKey>::default();
        let effects = s.update(PanelMessage::OpenRuntimeSettings, Instant::now(), &[]);
        assert_eq!(effects, vec![PanelEffect::OpenSettingsApps]);
    }
    #[test]
    fn retry_from_the_issue_page_starts_the_app() {
        let mut s = PanelState::<TestKey>::default();
        let a = slot("retry-a");
        loaded(
            &mut s,
            vec![app_with_issue(
                "retry-a",
                failed("需要 python3"),
                AppIssue::RuntimeMissing {
                    runtime: "python3".into(),
                },
            )],
            &[],
        );
        let effects = s.update(PanelMessage::Start(a), Instant::now(), &[]);
        assert_eq!(effects, vec![PanelEffect::StartApp(a)]);
        // issue 页下 acting 优先 → 显示"启动中…"。
        assert_eq!(s.view_model(a), PanelView::Busy("启动中…"));
    }

    #[test]
    fn issue_texts_cover_missing_and_version_mismatch() {
        let cases = [
            (
                AppIssue::RuntimeMissing {
                    runtime: "node".into(),
                },
                ("需要 Node.js", "尚未安装,可在 设置 → 应用 里一键安装"),
            ),
            (
                AppIssue::RuntimeMissing {
                    runtime: "python3".into(),
                },
                ("需要 Python", "尚未安装,可在 设置 → 应用 里一键安装"),
            ),
            (
                AppIssue::RuntimeVersion {
                    runtime: "node".into(),
                    required: ">=18".into(),
                    found: "16.0.0".into(),
                },
                ("需要 Node.js", "要求 >=18,当前 16.0.0"),
            ),
            (
                AppIssue::DependencyInstall {
                    summary: "npm ci 失败(退出码 1)".into(),
                },
                (
                    "依赖安装失败",
                    "npm ci 失败(退出码 1)\n查看日志了解安装输出",
                ),
            ),
        ];
        for (issue, (title, detail)) in cases {
            let (t, d) = issue_texts(&issue);
            assert_eq!(t, title, "{issue:?}");
            assert_eq!(d, detail, "{issue:?}");
        }
    }

    // ===== A6f Task 2:订阅取代轮询(轮询降为兜底) =====

    /// 首次 tick 先发订阅;`Subscribed` 后轮询降到兜底档(30s),不再每 2s 拉。
    #[test]
    fn a_live_subscription_slows_polling_to_the_safety_interval() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        // 启动第一拍:发起订阅。
        assert_eq!(s.subscribe_if_due(t0), vec![PanelEffect::Subscribe]);
        // 列表照旧拉一次。
        assert_eq!(s.poll_if_due(t0, &[]), vec![PanelEffect::FetchList]);
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);
        // 订阅确认本身会补一次重拉,消化掉。
        assert_eq!(
            s.update(PanelMessage::Subscribed, t0, &[]),
            vec![PanelEffect::FetchList]
        );
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);
        assert_eq!(s.poll_interval(), SAFETY_POLL_INTERVAL);

        let a = [slot("sub-a")];
        assert!(
            s.poll_if_due(t0 + POLL_INTERVAL, &a).is_empty(),
            "订阅就绪后 2s 不该再拉"
        );
        assert_eq!(
            s.poll_if_due(t0 + SAFETY_POLL_INTERVAL, &a),
            vec![PanelEffect::FetchList],
            "到了兜底档才拉"
        );
    }

    /// 订阅请求发出后到确认前,不重复发(Idle→Pending 只发一次)。
    #[test]
    fn subscribe_is_requested_once_until_it_resolves() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        assert_eq!(s.subscribe_if_due(t0), vec![PanelEffect::Subscribe]);
        assert!(
            s.subscribe_if_due(t0 + POLL_INTERVAL).is_empty(),
            "Pending 时不再发订阅"
        );
    }

    /// 推送风暴:订阅就绪、无在途时连发 10 次 `Changed` → 只产出一次 `FetchList`;
    /// 在途时再来 → 合并成 `dirty`,列表回来后恰好再补一次。
    #[test]
    fn a_change_triggers_exactly_one_refetch_even_in_a_storm() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        s.subscribe_if_due(t0);
        s.poll_if_due(t0, &[]);
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);
        s.update(PanelMessage::Subscribed, t0, &[]);
        // 订阅确认自带的重拉,消化掉。
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);

        // 第一次 Changed:立刻拉一次。
        assert_eq!(
            s.update(PanelMessage::Changed, t0, &[]),
            vec![PanelEffect::FetchList]
        );
        // 之后在途时再来 10 次:全部合并成 dirty,不各自发请求。
        for _ in 0..10 {
            assert!(s.update(PanelMessage::Changed, t0, &[]).is_empty());
        }
        // 在途的 List 回来:合并后恰好再补一次。
        assert_eq!(
            s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]),
            vec![PanelEffect::FetchList]
        );
        // 再回来时没有脏标记:不再多发。
        assert!(
            s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[])
                .is_empty()
        );
    }

    /// 丢订阅后回到 2s 快速轮询,并按退避重订阅;连续失败退避停在 30s。
    #[test]
    fn losing_the_subscription_goes_back_to_fast_polling_and_retries_with_backoff() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        s.subscribe_if_due(t0);
        s.poll_if_due(t0, &[]);
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);
        s.update(PanelMessage::Subscribed, t0, &[]);
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);

        // 丢订阅:立刻回到快速档。
        s.update(PanelMessage::SubscriptionLost, t0, &[]);
        assert_eq!(s.poll_interval(), POLL_INTERVAL);
        let a = [slot("lost-a")];
        assert!(
            s.subscribe_if_due(t0 + Duration::from_millis(1)).is_empty(),
            "退避没到点不发订阅"
        );
        assert_eq!(
            s.poll_if_due(t0 + POLL_INTERVAL, &a),
            vec![PanelEffect::FetchList],
            "断开后按 2s 轮询"
        );
        s.update(
            PanelMessage::ListLoaded(Ok(vec![])),
            t0 + POLL_INTERVAL,
            &[],
        );

        // 第 0 档到点:发一次订阅。
        let retry0 = t0 + RESUBSCRIBE_BACKOFF[0] + Duration::from_millis(1);
        assert_eq!(s.subscribe_if_due(retry0), vec![PanelEffect::Subscribe]);

        // 连续失败:退避逐档递增,封顶 30s。
        let mut now = retry0;
        for _ in 0..6 {
            s.update(PanelMessage::SubscriptionLost, now, &[]);
            now += Duration::from_secs(60);
        }
        assert_eq!(
            s.poll_interval(),
            POLL_INTERVAL,
            "还没订阅上,轮询仍是快速档"
        );
        let (attempt, retry_at, at) = match s.subscription {
            Subscription::Backoff { attempt, retry_at } => (attempt, retry_at, now),
            ref other => panic!("{other:?}"),
        };
        assert_eq!(attempt, RESUBSCRIBE_BACKOFF.len() - 1, "封顶在最后一档");
        assert_eq!(
            retry_at.duration_since(at - Duration::from_secs(60)),
            RESUBSCRIBE_BACKOFF[RESUBSCRIBE_BACKOFF.len() - 1]
        );
    }

    /// dozerd 重启:订阅丢失 → 退避到期重订阅 → `Subscribed` 恢复 `Live`,
    /// 且恢复那一刻补一次重拉(断线期间可能漏了事件)。
    #[test]
    fn dozerd_restart_is_recovered() {
        let mut s = PanelState::<TestKey>::default();
        let t0 = Instant::now();
        s.subscribe_if_due(t0);
        s.poll_if_due(t0, &[]);
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);
        s.update(PanelMessage::Subscribed, t0, &[]);
        s.update(PanelMessage::ListLoaded(Ok(vec![])), t0, &[]);

        s.update(PanelMessage::SubscriptionLost, t0, &[]);
        let retry = t0 + POLL_INTERVAL + RESUBSCRIBE_BACKOFF[0] + Duration::from_millis(1);
        assert_eq!(s.subscribe_if_due(retry), vec![PanelEffect::Subscribe]);
        // 订阅恢复:补一次重拉。
        assert_eq!(
            s.update(PanelMessage::Subscribed, retry, &[]),
            vec![PanelEffect::FetchList]
        );
        assert_eq!(s.poll_interval(), SAFETY_POLL_INTERVAL);
    }
}
