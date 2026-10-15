//! Surface 执行器:把 `bytehost_core::surface` 的**状态机**落到窗口线程的原生操作。
//!
//! 分层:
//! - [`geometry`]:逻辑坐标 → bounds 的纯函数;
//! - [`routing`]:`(instance, slot, generation)` 归属与过期拒绝;
//! - [`lifecycle`]:创建/销毁/隐藏/恢复的转移,驱动 `crate::panel` 的原生句柄;
//! - [`executor`](本模块):一张 `SurfaceRouter` + 一组 `Surface` 状态,合成管理器。
//!
//! 不变量(规格 §5.4):
//! - 所有原生 create/destroy/geometry/focus 只在**窗口线程**发生(经 `panel`);
//! - 后台只发"意图",不碰句柄;
//! - 销毁 = 注销路由 + 清窗口内待执行脚本;
//! - 旧 generation 的 ready/IPC/脚本结果一律丢弃。

pub mod geometry;
pub mod lifecycle;
pub mod routing;

pub use routing::{Routed, SurfaceRouter};

use std::collections::HashMap;
use std::time::Duration;

use bytehost_core::{
    PanelInstanceId, Surface, SurfaceEvent as CoreEvent, SurfaceId, SurfaceKind, SurfaceSlot,
    SurfaceState,
};

use routing::PhysicalSlot;

/// 一次"要推送的载荷"构造委托:产品把 desired 状态转成注入脚本。
///
/// 宿主不知道业务内容,只负责决定**何时**推(去重、隐藏缓存、重建重发)。
pub type ScriptBuilder<T> = Box<dyn Fn(&T) -> String + Send>;

/// 应用语言桥快照推送的执行失败有界退避(规格 §6.2 / B11)。
///
/// 失败按 250ms / 1s / 2s 退避三次,之后停止主动重试,等下一次 ready / 恢复 / 语言变化等
/// 事件再触发推送;**绝不**在 `about_to_wait` 里零间隔循环重试。注入脚本执行成功后调用
/// [`PushBackoff::reset`] 复位,恢复即时推送能力。
#[derive(Debug, Clone, Copy, Default)]
pub struct PushBackoff {
    attempts: u8,
}

impl PushBackoff {
    pub fn new() -> Self {
        Self { attempts: 0 }
    }

    /// 重置(脚本成功执行、或活动版本/语言变化后调用)。
    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    /// 记录一次失败,返回下一次重试应等待的延迟;`None` 表示已达上限,停止主动重试
    /// (交由 ready/恢复/语言变更等外部事件重新触发)。
    pub fn on_failure(&mut self) -> Option<Duration> {
        const DELAYS: [Duration; 3] = [
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ];
        let i = self.attempts as usize;
        if i >= DELAYS.len() {
            return None;
        }
        self.attempts += 1;
        Some(DELAYS[i])
    }

    /// 是否已达重试上限。
    pub fn exhausted(&self) -> bool {
        self.attempts as usize >= 3
    }
}

/// 应用语言桥的推送执行器别名:产品用 `AppDisplayI18n` 算出 [`bytehost_webview::i18n::I18nSnapshot`]
/// 后,以 `SurfaceExecutor<bytehost_webview::i18n::I18nSnapshot>` 驱动"首次 ready 推完整快照、
/// 仅语言变更也推、hidden 保留最新恢复同步、rebuild 强制重推"的既有不变量(见 §6.2)。
pub type AppI18nExecutor = SurfaceExecutor<bytehost_webview::i18n::I18nSnapshot>;

/// 单个 Surface 的运行时登记:core 状态 + 物理槽 + 载荷缓存。
struct Entry<T> {
    surface: Surface<T>,
    physical: PhysicalSlot,
    build_script: ScriptBuilder<T>,
}

/// 多 Surface 执行器:统一管理状态、路由与窗口线程原生操作。
///
/// 泛型 `T` 是产品的"推送载荷"(如 Usage 的聚合行);宿主不解释其含义,
/// 只按 `PushState` 语义决定推/不推。载荷 → 脚本的转换由产品经 `build_script` 提供。
pub struct SurfaceExecutor<T> {
    router: SurfaceRouter,
    entries: HashMap<SurfaceId, Entry<T>>,
}

impl<T: PartialEq + Clone + Send + 'static> Default for SurfaceExecutor<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: PartialEq + Clone + Send + 'static> SurfaceExecutor<T> {
    pub fn new() -> Self {
        Self {
            router: SurfaceRouter::new(),
            entries: HashMap::new(),
        }
    }

    /// 注册一个新的 Surface(尚未创建原生实例)。返回其身份供后续引用。
    ///
    /// `physical` 是交给 `crate::panel` 的句柄键;`build_script` 把载荷转成注入脚本。
    ///
    /// **物理槽是独占的**:若该 `physical` 已被别的 Surface 占用(代表重建/替换),
    /// 或同一 `id` 被重复注册,旧登记会被**原子清除**(entries 与路由表一起),
    /// 避免旧身份残留:旧身份不再被 `drain_ready_pushes`/`apply` 命中,也不会
    /// 在 `unregister(old_id)` 时误注销新实例的路由。
    pub fn register(
        &mut self,
        id: SurfaceId,
        kind: SurfaceKind,
        physical: PhysicalSlot,
        build_script: ScriptBuilder<T>,
    ) {
        // 先清掉任何与本次注册冲突的旧登记:同 id,或占用了同一物理槽。
        let conflicting: Vec<SurfaceId> = self
            .entries
            .iter()
            .filter(|(eid, e)| **eid == id || e.physical == physical)
            .map(|(eid, _)| *eid)
            .collect();
        for old in conflicting {
            self.entries.remove(&old);
            self.router.unregister(physical);
        }
        self.router.register(id, physical);
        self.entries.insert(
            id,
            Entry {
                surface: Surface::new(id, kind),
                physical,
                build_script,
            },
        );
    }

    pub fn contains(&self, id: SurfaceId) -> bool {
        self.entries.contains_key(&id)
    }

    pub fn state(&self, id: SurfaceId) -> Option<&SurfaceState> {
        self.entries.get(&id).map(|e| e.surface.state())
    }

    /// 校验一次上行事件的归属(IPC / ready / 脚本结果)。旧代数一律拒绝。
    pub fn route(&self, id: SurfaceId) -> Routed {
        self.router.route(id)
    }

    /// 应用一个状态机事件。返回被接受的事件(未被接受 = 当前状态下无意义)。
    ///
    /// **不执行任何原生操作**;原生动作由调用方(窗口线程,经 `panel`)根据返回值执行。
    /// 这样状态与副作用分离,便于在测试里只驱动状态。
    pub fn apply(&mut self, id: SurfaceId, event: CoreEvent) -> Option<CoreEvent> {
        self.entries.get_mut(&id)?.surface.apply(event)
    }

    /// 发起一次创建(Absent/Failed → Creating)。返回是否真的发起。
    ///
    /// 发起后由窗口线程执行 `panel::build_child`;创建回调再以
    /// `apply(id, Created | CreateFailed)` 回到状态机。
    pub fn begin_create(&mut self, id: SurfaceId) -> bool {
        self.entries
            .get_mut(&id)
            .is_some_and(|e| e.surface.begin_create())
    }

    /// 更新某个 Surface 的期望载荷(产品每次算出新状态都调用)。
    pub fn set_desired(&mut self, id: SurfaceId, value: T) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.surface.push_mut().set_desired(value);
        }
    }

    /// 待推送的状态:仅当 Ready 且与上次送达不同时返回 `Some(载荷)`。
    pub fn pending(&self, id: SurfaceId) -> Option<&T> {
        self.entries.get(&id)?.surface.take_pending()
    }

    /// 取该 Surface 的注入脚本(调用方在**窗口线程**执行 `panel::evaluate`)。
    pub fn script_for(&self, id: SurfaceId, value: &T) -> Option<String> {
        self.entries.get(&id).map(|e| (e.build_script)(value))
    }

    /// 标记某 Surface 的载荷已送达(在 `panel::evaluate` 成功返回后调用)。
    pub fn mark_sent(&mut self, id: SurfaceId, value: T) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.surface.push_mut().mark_sent(value);
        }
    }

    /// 该 Surface 的物理槽(交给 `panel`)。
    pub fn physical_slot(&self, id: SurfaceId) -> Option<PhysicalSlot> {
        self.entries.get(&id).map(|e| e.physical)
    }

    /// 注销并返回物理槽(销毁清理路由表用);原生 `panel::destroy` 由调用方执行。
    ///
    /// 只有当 `id` 仍是该物理槽的**当前占用者**时才注销路由,防止旧身份的延迟清理
    /// 误注销已替换的新实例。返回 `None` 表示该身份不是当前登记(无需清理)。
    pub fn unregister(&mut self, id: SurfaceId) -> Option<PhysicalSlot> {
        let entry = self.entries.remove(&id)?;
        let physical = entry.physical;
        // 仅当该物理槽仍归此身份时,才动路由表。
        if self.router.contains(id) {
            self.router.unregister(physical);
        }
        Some(physical)
    }

    /// 迭代所有 Ready 且待推送的 Surface,产出 (身份, 物理槽, 脚本) 供窗口线程执行。
    ///
    /// 调用方对每一项执行 `panel::evaluate(physical, &script)`,成功后对对应身份
    /// 调 `mark_sent`(载荷由调用方从 `desired` 拿)。
    pub fn drain_ready_pushes(&self) -> Vec<(SurfaceId, PhysicalSlot, String)> {
        let mut out = Vec::new();
        for (id, e) in &self.entries {
            if let Some(value) = e.surface.take_pending() {
                out.push((*id, e.physical, (e.build_script)(value)));
            }
        }
        out
    }

    /// 诊断:当前已注册数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 该 Surface 类型的 Ready 触发策略(用于选择握手或导航完成)。
    pub fn ready_policy(&self, id: SurfaceId) -> Option<bytehost_core::ReadyPolicy> {
        self.entries.get(&id).map(|e| e.surface.ready_policy())
    }
}

/// 便捷:为一个身份构造 `SurfaceId`。
pub fn surface_id(instance: PanelInstanceId, slot: SurfaceSlot, generation: u64) -> SurfaceId {
    SurfaceId::new(instance, slot, bytehost_core::Generation::new(generation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_core::{Generation, SurfaceSlot as Slot};

    fn sid(slot: u32, generation: u64) -> SurfaceId {
        SurfaceId::new(
            PanelInstanceId::new(slot, 0),
            Slot(slot),
            Generation::new(generation),
        )
    }

    fn script(v: &u32) -> String {
        format!("window.__host.push({v})")
    }

    fn executor_with(id: SurfaceId, kind: SurfaceKind) -> SurfaceExecutor<u32> {
        let mut ex = SurfaceExecutor::<u32>::new();
        ex.register(id, kind, Slot(id.slot.0), Box::new(script));
        ex
    }

    #[test]
    fn full_lifecycle_and_single_push() {
        let id = sid(0, 0);
        let mut ex = executor_with(id, SurfaceKind::Builtin);
        assert_eq!(ex.state(id), Some(&SurfaceState::Absent));
        assert!(ex.begin_create(id));
        ex.apply(id, CoreEvent::Created);
        ex.apply(id, CoreEvent::Ready);
        assert_eq!(ex.state(id), Some(&SurfaceState::Ready));
        ex.set_desired(id, 1);
        let pushes = ex.drain_ready_pushes();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].2, "window.__host.push(1)");
    }

    #[test]
    fn hidden_surface_does_not_push_and_resyncs_on_resume() {
        let id = sid(0, 0);
        let mut ex = executor_with(id, SurfaceKind::Builtin);
        ex.begin_create(id);
        ex.apply(id, CoreEvent::Created);
        ex.apply(id, CoreEvent::Ready);
        ex.set_desired(id, 1);
        ex.mark_sent(id, 1);
        ex.apply(id, CoreEvent::Hidden);
        ex.set_desired(id, 2);
        assert!(ex.drain_ready_pushes().is_empty(), "隐藏时不推");
        ex.apply(id, CoreEvent::Resumed);
        let pushes = ex.drain_ready_pushes();
        assert_eq!(pushes.len(), 1);
        assert!(pushes[0].2.contains('2'));
    }

    #[test]
    fn stale_generation_is_rejected_by_router() {
        let id = sid(1, 1);
        let mut ex = executor_with(id, SurfaceKind::Builtin);
        let stale = sid(1, 0);
        assert!(!ex.route(stale).is_accepted());
        assert!(ex.route(id).is_accepted());
        // 旧代数的状态机事件不会命中任何 entry。
        assert!(ex.apply(stale, CoreEvent::Ready).is_none());
    }

    #[test]
    fn unregister_removes_routing() {
        let id = sid(2, 0);
        let mut ex = executor_with(id, SurfaceKind::Builtin);
        ex.begin_create(id);
        ex.apply(id, CoreEvent::Created);
        ex.apply(id, CoreEvent::Ready);
        assert!(ex.unregister(id).is_some());
        assert!(ex.route(id).slot().is_none(), "注销后路由为空");
        assert!(!ex.contains(id));
    }

    #[test]
    fn create_failure_is_retryable_through_executor() {
        let id = sid(3, 0);
        let mut ex = executor_with(id, SurfaceKind::Builtin);
        assert!(ex.begin_create(id));
        ex.apply(id, CoreEvent::CreateFailed { error: "x".into() });
        assert!(matches!(ex.state(id), Some(SurfaceState::Failed { .. })));
        // 失败后可直接重试:begin_create 再次发起。
        assert!(ex.begin_create(id), "失败后可重试");
        assert_eq!(ex.state(id), Some(&SurfaceState::Creating));
        assert!(ex.apply(id, CoreEvent::Created).is_some());
        assert_eq!(ex.state(id), Some(&SurfaceState::WaitingReady));
    }

    #[test]
    fn application_ready_policy_is_navigation_completed() {
        let id = sid(4, 0);
        let ex = executor_with(id, SurfaceKind::Application);
        assert_eq!(
            ex.ready_policy(id),
            Some(bytehost_core::ReadyPolicy::NavigationCompleted)
        );
    }

    #[test]
    fn replacing_a_slot_evicts_the_old_ready_entry() {
        // 旧实例已 Ready 且有挂起推送;同物理槽换新身份后:
        // (a) 新实例的首次 push 是它自己的快照,drain 不得把旧载荷送给新槽;
        // (b) apply(old_id, ..) 不再命中旧 entry(旧身份已被清)。
        let old = sid(1, 0);
        let new = sid(1, 1);
        let mut ex = executor_with(old, SurfaceKind::Builtin);
        ex.begin_create(old);
        ex.apply(old, CoreEvent::Created);
        ex.apply(old, CoreEvent::Ready);
        ex.set_desired(old, 11); // 旧实例挂起一个待推送

        ex.register(
            new,
            SurfaceKind::Builtin,
            Slot(new.slot.0),
            Box::new(script),
        );

        assert!(!ex.contains(old), "旧身份已被清出 entries");
        assert!(!ex.route(old).is_accepted(), "旧身份路由已被注销");
        assert!(
            ex.apply(old, CoreEvent::Ready).is_none(),
            "旧身份事件不再命中"
        );
        assert!(
            ex.drain_ready_pushes().is_empty(),
            "新实例尚未 ready,不得推出旧载荷"
        );
        // 新实例走自己的生命周期,推自己的载荷。
        ex.begin_create(new);
        ex.apply(new, CoreEvent::Created);
        ex.apply(new, CoreEvent::Ready);
        ex.set_desired(new, 22);
        let pushes = ex.drain_ready_pushes();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].0, new);
        assert!(pushes[0].2.contains("22"), "推的是新实例的载荷");
    }

    #[test]
    fn late_unregister_of_old_id_does_not_drop_the_new_instance() {
        // 旧身份的延迟清理不得误注销已替换的新实例路由。
        let old = sid(2, 0);
        let new = sid(2, 1);
        let mut ex = executor_with(old, SurfaceKind::Builtin);
        ex.register(
            new,
            SurfaceKind::Builtin,
            Slot(new.slot.0),
            Box::new(script),
        );
        // 旧身份已被 register 清掉;晚到的 unregister 是 no-op。
        assert!(ex.unregister(old).is_none());
        assert!(ex.contains(new), "新实例仍在登记");
        assert!(ex.route(new).is_accepted(), "新实例路由仍在");
        assert_eq!(ex.len(), 1);
    }

    #[test]
    fn re_register_same_id_is_idempotent_reset() {
        let id = sid(3, 0);
        let mut ex = executor_with(id, SurfaceKind::Builtin);
        ex.begin_create(id);
        ex.apply(id, CoreEvent::Created);
        ex.apply(id, CoreEvent::Ready);
        ex.set_desired(id, 5);
        // 同 id 重注册:旧状态被重置为 Absent,载荷清空。
        ex.register(id, SurfaceKind::Builtin, Slot(id.slot.0), Box::new(script));
        assert_eq!(ex.state(id), Some(&SurfaceState::Absent));
        assert_eq!(ex.len(), 1);
        assert!(ex.drain_ready_pushes().is_empty());
    }

    #[test]
    fn app_i18n_snapshot_is_pushed_on_ready_and_repushed_on_rebuild() {
        // 应用语言桥走 SurfaceExecutor<I18nSnapshot>:首次 ready 推首帧,同源再导航(新 Ready)
        // 强制重推,仅语言变化(revision 推进)也触发 pending(B06 / B07)。
        use bytehost_webview::i18n::{I18nSnapshot, i18n_set_locale_script};
        let mut ex: SurfaceExecutor<I18nSnapshot> = SurfaceExecutor::new();
        let id = sid(0, 0);
        ex.register(
            id,
            SurfaceKind::Application,
            Slot(0),
            Box::new(i18n_set_locale_script),
        );
        ex.begin_create(id);
        ex.apply(id, CoreEvent::Created);
        ex.apply(id, CoreEvent::Ready);
        ex.set_desired(id, I18nSnapshot::new("zh-CN", "zh-CN", 1));
        let pushes = ex.drain_ready_pushes();
        assert_eq!(pushes.len(), 1, "首次 ready 推首帧");
        assert!(pushes[0].2.contains("window.__bytehost_i18n_set"));
        assert!(
            pushes[0].2.contains("\"ui_locale\":\"zh-CN\""),
            "推送携带最新 ui_locale"
        );
        ex.mark_sent(id, I18nSnapshot::new("zh-CN", "zh-CN", 1));

        // 同源再导航(新 Ready)→ 强制重推,即使 desired 没变。
        ex.apply(id, CoreEvent::Ready);
        let pushes = ex.drain_ready_pushes();
        assert_eq!(pushes.len(), 1, "重建/new ready 强制重推");

        // 仅语言变化(revision 推进)→ 触发 pending。
        ex.mark_sent(id, I18nSnapshot::new("zh-CN", "zh-CN", 1));
        ex.set_desired(id, I18nSnapshot::new("en", "en", 2));
        assert!(ex.pending(id).is_some(), "语言变化应待推送");
        let pushes = ex.drain_ready_pushes();
        assert_eq!(pushes.len(), 1);
        assert!(pushes[0].2.contains("\"ui_locale\":\"en\""));
    }

    #[test]
    fn push_backoff_is_bounded_and_resets_on_success() {
        // 脚本执行失败按 250ms / 1s / 2s 退避,三次后停止主动重试,成功/变化后复位(B11)。
        let mut b = PushBackoff::new();
        assert_eq!(b.on_failure(), Some(Duration::from_millis(250)));
        assert_eq!(b.on_failure(), Some(Duration::from_secs(1)));
        assert_eq!(b.on_failure(), Some(Duration::from_secs(2)));
        assert!(b.exhausted());
        assert_eq!(b.on_failure(), None, "三次后停止主动重试(等外部事件)");
        b.reset();
        assert!(!b.exhausted());
        assert_eq!(
            b.on_failure(),
            Some(Duration::from_millis(250)),
            "重置后恢复即时重试能力"
        );
    }
}
