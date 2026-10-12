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

use bytehost_core::{
    PanelInstanceId, Surface, SurfaceEvent as CoreEvent, SurfaceId, SurfaceKind, SurfaceSlot,
    SurfaceState,
};

use routing::PhysicalSlot;

/// 一次"要推送的载荷"构造委托:产品把 desired 状态转成注入脚本。
///
/// 宿主不知道业务内容,只负责决定**何时**推(去重、隐藏缓存、重建重发)。
pub type ScriptBuilder<T> = Box<dyn Fn(&T) -> String + Send>;

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
    pub fn register(
        &mut self,
        id: SurfaceId,
        kind: SurfaceKind,
        physical: PhysicalSlot,
        build_script: ScriptBuilder<T>,
    ) {
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
    pub fn unregister(&mut self, id: SurfaceId) -> Option<PhysicalSlot> {
        let entry = self.entries.remove(&id)?;
        self.router.unregister(entry.physical);
        Some(entry.physical)
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
}
