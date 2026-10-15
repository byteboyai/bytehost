//! Surface 生命周期状态机(纯逻辑,无 iced/wry)。
//!
//! 一个 Surface 的身份是 `(PanelInstanceId, SurfaceSlot, generation)`——不用 URL
//! 或名称前缀猜归属(规格 §5.4)。本模块只描述**状态与转移**,不碰原生句柄:
//! 原生创建/销毁/几何在 `bytehost-desktop`(唯一真相在窗口线程)。
//!
//! 生命周期:Absent → Creating → WaitingReady → Ready → Hidden / Destroyed;
//! 创建失败进入可重试 `Failed`。第三方应用未实现 ready 握手时用**导航完成**作为
//! Ready 的触发(见 [`ReadyPolicy`]),不要求它实现内置面板协议。
//!
//! 关键不变量:
//! - 同一时刻每个 Surface 至多一个原生实例;
//! - 旧 generation 的 ready/IPC/脚本结果一律丢弃;
//! - 隐藏时不接受业务输入;
//! - **可见性与就绪度分开维护**:隐藏不撤销已完成的握手,但"未握手即隐藏再恢复"
//!   回到 `WaitingReady`(不凭空 Ready);隐藏期间到达的 ready 被记住,恢复后即为 Ready;
//! - 内置面板首次 ready 后发送**完整快照**,隐藏期间保留最新待推送状态,恢复后同步;
//!   新建实例(重建)必须重新发送快照。

use crate::id::{PanelInstanceId, SurfaceSlot};
use crate::request::Generation;

/// Surface 的信任/内容类别,决定 desktop 用哪种 builder(规格 §5.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceKind {
    /// 内置面板:走 `dozer://`-style 隔离 host + 内置 ready 握手协议。
    Builtin,
    /// 第三方应用:走受限 builder,不注入产品协议;ready 用导航完成触发。
    Application,
}

/// Surface 的 Ready 触发策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyPolicy {
    /// 内置面板:等待页面回传 ready 事件(JS 端 `__host.dispatch` 已注册)。
    Handshake,
    /// 第三方应用:导航完成即视为 Ready(不要求它实现内置协议)。
    NavigationCompleted,
}

impl SurfaceKind {
    pub fn ready_policy(self) -> ReadyPolicy {
        match self {
            SurfaceKind::Builtin => ReadyPolicy::Handshake,
            SurfaceKind::Application => ReadyPolicy::NavigationCompleted,
        }
    }
}

/// Surface 身份:实例 + 槽位 + 代数。旧 generation 的任何事件都必须被拒。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId {
    pub instance: PanelInstanceId,
    pub slot: SurfaceSlot,
    pub generation: Generation,
}

impl SurfaceId {
    pub fn new(instance: PanelInstanceId, slot: SurfaceSlot, generation: Generation) -> Self {
        Self {
            instance,
            slot,
            generation,
        }
    }
}

/// Surface 生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceState {
    /// 尚未创建任何原生实例。
    Absent,
    /// 原生实例创建中(已发起,未回调)。
    Creating,
    /// 已创建,等待 Ready(内置:握手;应用:导航完成)。
    WaitingReady,
    /// 就绪:可接收推送与业务输入。
    Ready,
    /// 已隐藏:不接受业务输入,但保留实例与最新待推送状态。
    Hidden,
    /// 已销毁:注销路由,旧事件一律丢弃。
    Destroyed,
    /// 创建失败,可重试。
    Failed { error: String },
}

impl SurfaceState {
    /// 是否持有原生实例(Creating 表示"已发起",也占一个实例)。
    pub fn has_instance(&self) -> bool {
        matches!(
            self,
            SurfaceState::Creating
                | SurfaceState::WaitingReady
                | SurfaceState::Ready
                | SurfaceState::Hidden
        )
    }

    /// 是否接受业务输入(推送/IPC)。
    pub fn accepts_input(&self) -> bool {
        matches!(self, SurfaceState::Ready)
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, SurfaceState::Ready)
    }
}

/// 一次创建尝试的结果(由 desktop 回调转换成转移输入)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceEvent {
    /// 原生实例创建成功(拿到句柄)。
    Created,
    /// 原生实例创建失败(可重试)。
    CreateFailed { error: String },
    /// Ready 触发(内置握手 / 应用导航完成,由 `ReadyPolicy` 决定许可)。
    Ready,
    /// 隐藏(切换面板/标签)。
    Hidden,
    /// 从隐藏恢复。
    Resumed,
    /// 销毁(面板关闭)。
    Destroyed,
    /// 旧代数事件到达(用于测试/防御;正常路径下 desktop 已在路由层拒收)。
    StaleGeneration { generation: Generation },
}

/// 隐藏期间保留的最新待推送状态。
///
/// "隐藏时不接受业务输入"指**不再把新状态当作已送达**;但最新 desired 要记住,
/// 恢复后同步;新建实例必须重新发送快照(清空 `sent`)。
#[derive(Debug, Clone, PartialEq)]
pub struct PushState<T> {
    /// 最近一次**真正送达**的值。
    sent: Option<T>,
    /// 最新待推送 desired(与产品期望一致)。
    desired: Option<T>,
}

impl<T> Default for PushState<T> {
    fn default() -> Self {
        Self {
            sent: None,
            desired: None,
        }
    }
}

impl<T: PartialEq + Clone> PushState<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置期望状态(产品每次算出新状态都调用)。
    pub fn set_desired(&mut self, value: T) {
        self.desired = Some(value);
    }

    pub fn desired(&self) -> Option<&T> {
        self.desired.as_ref()
    }

    pub fn sent(&self) -> Option<&T> {
        self.sent.as_ref()
    }

    /// 是否待推送:仅在 Ready 且 desired 与上次送达不同时返回 `Some`。
    pub fn pending(&self) -> Option<&T> {
        let desired = self.desired.as_ref()?;
        if self.sent.as_ref() == Some(desired) {
            return None;
        }
        Some(desired)
    }

    /// 标记已送达(desktop 在真正 `evaluate_script` 成功后调用)。
    pub fn mark_sent(&mut self, value: T) {
        self.sent = Some(value);
    }

    /// 新实例就绪/重建:清空 `sent` 强制重发一次快照。
    pub fn reset_for_new_instance(&mut self) {
        self.sent = None;
    }

    /// 是否有未送达的更新(隐藏期间用于判断"恢复后需同步")。
    pub fn has_unsent(&self) -> bool {
        self.pending().is_some()
    }
}

/// 一个 Surface 的完整状态:身份 + 生命周期 + 推送缓存。
#[derive(Debug, Clone, PartialEq)]
pub struct Surface<T> {
    id: SurfaceId,
    kind: SurfaceKind,
    state: SurfaceState,
    push: PushState<T>,
    /// 本实例是否**曾经**完成 Ready 握手。可见性与就绪度分开维护:
    /// 隐藏不撤销已完成的握手(恢复即 Ready),但"未握手即隐藏再恢复"必须回到
    /// `WaitingReady`,不能凭空 Ready(否则宿主页处理函数未安装时会丢首帧)。
    ready_once: bool,
}

impl<T: PartialEq + Clone> Surface<T> {
    /// 新建一个尚未创建的 Surface。
    pub fn new(id: SurfaceId, kind: SurfaceKind) -> Self {
        Self {
            id,
            kind,
            state: SurfaceState::Absent,
            push: PushState::new(),
            ready_once: false,
        }
    }

    pub fn id(&self) -> SurfaceId {
        self.id
    }

    pub fn kind(&self) -> SurfaceKind {
        self.kind
    }

    pub fn state(&self) -> &SurfaceState {
        &self.state
    }

    pub fn push(&self) -> &PushState<T> {
        &self.push
    }

    pub fn push_mut(&mut self) -> &mut PushState<T> {
        &mut self.push
    }

    /// 该 kind 的 Ready 触发策略。
    pub fn ready_policy(&self) -> ReadyPolicy {
        self.kind.ready_policy()
    }

    /// 发起一次创建(仅当 Absent 或 Failed 时可重试)。返回是否真的发起了。
    pub fn begin_create(&mut self) -> bool {
        if matches!(
            self.state,
            SurfaceState::Absent | SurfaceState::Failed { .. }
        ) {
            self.state = SurfaceState::Creating;
            // 新实例尚未握手;可见性与就绪度都从零开始。
            self.ready_once = false;
            true
        } else {
            false
        }
    }

    /// 应用一个事件。返回**被接受**的事件(用于让 desktop 决定后续动作;
    /// `None` 表示该事件在当前状态下被忽略)。
    pub fn apply(&mut self, event: SurfaceEvent) -> Option<SurfaceEvent> {
        // 旧代数事件一律丢弃。
        if let SurfaceEvent::StaleGeneration { generation } = event {
            return if generation != self.id.generation {
                None
            } else {
                Some(SurfaceEvent::StaleGeneration { generation })
            };
        }

        match (&self.state, &event) {
            (SurfaceState::Creating, SurfaceEvent::Created) => {
                self.state = SurfaceState::WaitingReady;
                Some(event)
            }
            (SurfaceState::Creating, SurfaceEvent::CreateFailed { error }) => {
                self.state = SurfaceState::Failed {
                    error: error.clone(),
                };
                Some(event)
            }
            (SurfaceState::WaitingReady, SurfaceEvent::Ready) => {
                // 新实例就绪:强制重发快照。
                self.push.reset_for_new_instance();
                self.ready_once = true;
                self.state = SurfaceState::Ready;
                Some(event)
            }
            (SurfaceState::Ready, SurfaceEvent::Ready) => {
                // 重复 ready 幂等(但保留一次重发语义):不切换状态,返回事件让上层可重发。
                self.push.reset_for_new_instance();
                self.ready_once = true;
                Some(event)
            }
            (SurfaceState::Ready, SurfaceEvent::Hidden)
            | (SurfaceState::WaitingReady, SurfaceEvent::Hidden) => {
                self.state = SurfaceState::Hidden;
                Some(event)
            }
            (SurfaceState::Hidden, SurfaceEvent::Resumed) => {
                // 可见性恢复:只有**本实例曾完成握手**才回到 Ready;否则回到
                // WaitingReady 等握手——否则宿主页处理函数未安装时会丢首帧。
                self.state = if self.ready_once {
                    SurfaceState::Ready
                } else {
                    SurfaceState::WaitingReady
                };
                Some(event)
            }
            (SurfaceState::Hidden, SurfaceEvent::Ready) => {
                // 隐藏期间到达的 ready:记住握手已发生(可见性暂不变),恢复后即为 Ready。
                self.push.reset_for_new_instance();
                self.ready_once = true;
                Some(event)
            }
            // 任何持有实例的状态都可被销毁。
            (s, SurfaceEvent::Destroyed) if s.has_instance() => {
                self.state = SurfaceState::Destroyed;
                self.ready_once = false;
                Some(event)
            }
            // 其余组合在当前状态下无意义。
            _ => None,
        }
    }

    /// 当前是否应把 desired 推给实例(仅 Ready 且与上次送达不同)。
    pub fn take_pending(&self) -> Option<&T> {
        if !self.state.is_ready() {
            return None;
        }
        self.push.pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(slot: u32, generation: u64) -> SurfaceId {
        SurfaceId::new(
            PanelInstanceId::new(slot, 0),
            SurfaceSlot(slot),
            Generation::new(generation),
        )
    }

    fn builtin(slot: u32, generation: u64) -> Surface<u32> {
        Surface::new(id(slot, generation), SurfaceKind::Builtin)
    }

    fn to_ready(s: &mut Surface<u32>) {
        assert!(s.begin_create());
        s.apply(SurfaceEvent::Created);
        s.apply(SurfaceEvent::Ready);
        assert!(s.state().is_ready());
    }

    #[test]
    fn first_ready_pushes_full_snapshot() {
        let mut s = builtin(0, 0);
        to_ready(&mut s);
        s.push_mut().set_desired(42);
        assert_eq!(s.take_pending(), Some(&42), "首次 ready 后必须推送完整快照");
    }

    #[test]
    fn same_desired_is_deduped_after_sent() {
        let mut s = builtin(0, 0);
        to_ready(&mut s);
        s.push_mut().set_desired(7);
        assert_eq!(s.take_pending(), Some(&7));
        s.push_mut().mark_sent(7);
        assert_eq!(s.take_pending(), None, "已送达且未变,不再推");
    }

    #[test]
    fn hidden_keeps_latest_state_and_resyncs_on_resume() {
        let mut s = builtin(0, 0);
        to_ready(&mut s);
        s.push_mut().set_desired(1);
        s.push_mut().mark_sent(1);
        assert!(s.apply(SurfaceEvent::Hidden).is_some());
        assert!(!s.state().accepts_input(), "隐藏时不接受业务输入");
        // 隐藏期间产品更新了状态。
        s.push_mut().set_desired(2);
        assert_eq!(s.take_pending(), None, "隐藏时不推");
        assert!(s.push().has_unsent(), "隐藏期间产生了未送达更新");
        assert!(s.apply(SurfaceEvent::Resumed).is_some());
        assert_eq!(s.take_pending(), Some(&2), "恢复后同步最新状态");
    }

    #[test]
    fn rebuild_requires_a_new_snapshot_even_if_desired_unchanged() {
        let mut s = builtin(0, 0);
        to_ready(&mut s);
        s.push_mut().set_desired(5);
        s.push_mut().mark_sent(5);
        assert_eq!(s.take_pending(), None);
        // 模拟重建:新的 ready 触发。
        s.apply(SurfaceEvent::Ready);
        assert_eq!(
            s.take_pending(),
            Some(&5),
            "新实例必须重发快照(即使 desired 没变)"
        );
    }

    #[test]
    fn create_failure_is_retryable() {
        let mut s = builtin(0, 0);
        assert!(s.begin_create());
        s.apply(SurfaceEvent::CreateFailed {
            error: "boom".into(),
        });
        assert!(matches!(s.state(), SurfaceState::Failed { .. }));
        assert!(s.begin_create(), "失败后可重试");
        assert_eq!(s.state(), &SurfaceState::Creating);
    }

    #[test]
    fn stale_generation_event_is_dropped() {
        let mut s = builtin(0, 1);
        to_ready(&mut s);
        // 旧代数的 ready/IPC 结果一律丢弃。
        let stale = s.apply(SurfaceEvent::StaleGeneration {
            generation: Generation::new(0),
        });
        assert!(stale.is_none(), "旧 generation 事件必须被丢弃");
        let current = s.apply(SurfaceEvent::StaleGeneration {
            generation: Generation::new(1),
        });
        assert!(current.is_some(), "当前 generation 事件保留");
    }

    #[test]
    fn hidden_before_ready_does_not_become_ready_on_resume() {
        // Created → Hidden → Resumed,且从未收到 Ready:恢复后必须回到 WaitingReady,
        // 不能凭空 Ready(否则宿主页处理函数未安装时会丢首帧)。
        let mut s = builtin(0, 0);
        assert!(s.begin_create());
        s.apply(SurfaceEvent::Created);
        assert!(s.apply(SurfaceEvent::Hidden).is_some());
        assert!(s.apply(SurfaceEvent::Resumed).is_some());
        assert_eq!(
            s.state(),
            &SurfaceState::WaitingReady,
            "未握手即隐藏再恢复,必须等待握手"
        );
        s.push_mut().set_desired(9);
        assert_eq!(s.take_pending(), None, "未 ready 不推");
        // 握手完成后退可推。
        assert!(s.apply(SurfaceEvent::Ready).is_some());
        assert_eq!(s.take_pending(), Some(&9));
    }

    #[test]
    fn ready_during_hidden_is_remembered_and_resumes_to_ready() {
        // WaitingReady → Hidden → (隐藏期间 Ready) → Resumed:恢复后必须是 Ready,
        // 且重发快照(隐藏期间的 ready 也要重置 sent)。
        let mut s = builtin(0, 0);
        assert!(s.begin_create());
        s.apply(SurfaceEvent::Created);
        assert!(s.apply(SurfaceEvent::Hidden).is_some());
        assert!(
            s.apply(SurfaceEvent::Ready).is_some(),
            "隐藏期间的 ready 要被记住"
        );
        assert_eq!(s.state(), &SurfaceState::Hidden, "隐藏期间可见性不变");
        assert!(s.apply(SurfaceEvent::Resumed).is_some());
        assert_eq!(s.state(), &SurfaceState::Ready, "恢复即 Ready");
        s.push_mut().set_desired(3);
        assert_eq!(s.take_pending(), Some(&3), "恢复后推最新快照");
    }

    #[test]
    fn destroyed_surface_rejects_further_events() {
        let mut s = builtin(0, 0);
        to_ready(&mut s);
        assert!(s.apply(SurfaceEvent::Destroyed).is_some());
        assert_eq!(s.state(), &SurfaceState::Destroyed);
        assert!(!s.state().has_instance());
        // 销毁后再来 ready 无意义。
        assert!(s.apply(SurfaceEvent::Ready).is_none());
    }

    #[test]
    fn application_uses_navigation_completed_ready_policy() {
        let s: Surface<u32> = Surface::new(id(0, 0), SurfaceKind::Application);
        assert_eq!(s.ready_policy(), ReadyPolicy::NavigationCompleted);
        assert_eq!(SurfaceKind::Builtin.ready_policy(), ReadyPolicy::Handshake);
    }

    #[test]
    fn begin_create_refused_when_already_creating_or_ready() {
        let mut s = builtin(0, 0);
        assert!(s.begin_create());
        assert!(!s.begin_create(), "创建中不得重复发起");
        s.apply(SurfaceEvent::Created);
        s.apply(SurfaceEvent::Ready);
        assert!(!s.begin_create(), "就绪时不得再创建");
    }

    /// 语言桥快照的推送维度(B06 / B07):`Surface` 泛型 `PushState` 已原生满足
    /// "首 ready 推完整快照、仅语言变更也推、hidden 保留最新恢复同步、rebuild 强制重推"。
    /// 这里用与 `I18nSnapshot` 同形的局部值证明语义,不引入 webview 依赖。
    #[test]
    fn locale_push_fires_on_language_change_even_if_shape_unchanged() {
        #[derive(Debug, Clone, PartialEq)]
        struct Locale {
            ui_locale: &'static str,
            app_locale: &'static str,
            revision: u64,
        }
        let mut s: Surface<Locale> = Surface::new(id(0, 0), SurfaceKind::Application);
        assert!(s.begin_create());
        s.apply(SurfaceEvent::Created);
        s.apply(SurfaceEvent::Ready);
        s.push_mut().set_desired(Locale {
            ui_locale: "zh-CN",
            app_locale: "zh-CN",
            revision: 1,
        });
        assert_eq!(
            s.take_pending().unwrap().revision,
            1,
            "首 ready 推送完整快照"
        );
        s.push_mut().mark_sent(Locale {
            ui_locale: "zh-CN",
            app_locale: "zh-CN",
            revision: 1,
        });
        assert!(s.take_pending().is_none(), "已送达且未变,不重推");

        // 语言没变但 revision 推进(宿主语言切换代次)→ 必须重推。
        s.push_mut().set_desired(Locale {
            ui_locale: "zh-CN",
            app_locale: "zh-CN",
            revision: 2,
        });
        assert_eq!(
            s.take_pending().unwrap().revision,
            2,
            "仅 revision 变也应重推(精确协商后 app_locale 不变也算变更)"
        );

        // 隐藏期间 revision 推进:隐藏不推,恢复后推最新。
        s.push_mut().mark_sent(Locale {
            ui_locale: "zh-CN",
            app_locale: "zh-CN",
            revision: 2,
        });
        s.apply(SurfaceEvent::Hidden);
        s.push_mut().set_desired(Locale {
            ui_locale: "zh-CN",
            app_locale: "zh-CN",
            revision: 3,
        });
        assert!(s.take_pending().is_none(), "隐藏期间不推");
        s.apply(SurfaceEvent::Resumed);
        assert_eq!(s.take_pending().unwrap().revision, 3, "恢复后推最新快照");
    }
}
