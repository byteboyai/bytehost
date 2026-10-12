//! Surface 生命周期集成测试(Task 08 checklist):
//! 1. 首次 ready 全量推送;隐藏缓存最新;重建需新快照;旧 ready/IPC 被拒;创建失败可重试;
//! 2. 按 `(instance, slot, generation)` 显式路由;第三方应用导航完成 ≠ 内置 ready 协议;
//! 3. 路由/注销与隐藏语义(原生句柄仅在窗口线程,这里验证状态与路由侧);
//! 4. 推送去重与恢复同步(载荷构造留在产品,执行器只管"何时推")。

use bytehost_core::{
    Generation, LogicalRect, PanelInstanceId, SurfaceEvent as CoreEvent, SurfaceId, SurfaceKind,
    SurfaceSlot, SurfaceState,
};
use bytehost_desktop::surface::geometry::{Placement, surface_rect};
use bytehost_desktop::surface::{Routed, SurfaceExecutor};

/// 产品的推送载荷:这里用整数代表业务状态(执行器不解释其含义)。
type Payload = u32;

fn sid(slot: u32, instance_generation: u32, generation: u64) -> SurfaceId {
    SurfaceId::new(
        PanelInstanceId::new(slot, instance_generation),
        SurfaceSlot(slot),
        Generation::new(generation),
    )
}

fn executor(id: SurfaceId, kind: SurfaceKind) -> SurfaceExecutor<Payload> {
    let mut ex = SurfaceExecutor::<Payload>::new();
    ex.register(
        id,
        kind,
        SurfaceSlot(id.slot.0),
        Box::new(|v| format!("host.push({v})")),
    );
    ex
}

fn drive_to_ready(ex: &mut SurfaceExecutor<Payload>, id: SurfaceId) {
    assert!(ex.begin_create(id));
    assert!(ex.apply(id, CoreEvent::Created).is_some());
    assert!(ex.apply(id, CoreEvent::Ready).is_some());
    assert_eq!(ex.state(id), Some(&SurfaceState::Ready));
}

#[test]
fn first_ready_pushes_full_snapshot_then_dedupes() {
    let id = sid(0, 0, 0);
    let mut ex = executor(id, SurfaceKind::Builtin);
    drive_to_ready(&mut ex, id);

    ex.set_desired(id, 42);
    let first = ex.drain_ready_pushes();
    assert_eq!(first.len(), 1, "首次 ready 必须全量推送");
    assert_eq!(first[0].2, "host.push(42)");

    // 标为已送达后,同样的 desired 不再推。
    ex.mark_sent(id, 42);
    assert!(ex.drain_ready_pushes().is_empty(), "已送达且未变不重复推");

    // 变了才推。
    ex.set_desired(id, 43);
    assert_eq!(ex.drain_ready_pushes().len(), 1);
}

#[test]
fn hidden_caches_latest_and_resume_syncs() {
    let id = sid(1, 0, 0);
    let mut ex = executor(id, SurfaceKind::Builtin);
    drive_to_ready(&mut ex, id);
    ex.set_desired(id, 1);
    ex.mark_sent(id, 1);

    ex.apply(id, CoreEvent::Hidden);
    assert_eq!(ex.state(id), Some(&SurfaceState::Hidden));
    ex.set_desired(id, 2);
    assert!(
        ex.drain_ready_pushes().is_empty(),
        "隐藏时不接受业务输入(不推)"
    );

    ex.apply(id, CoreEvent::Resumed);
    let resumed = ex.drain_ready_pushes();
    assert_eq!(resumed.len(), 1, "恢复后同步最新状态");
    assert_eq!(resumed[0].2, "host.push(2)");
}

#[test]
fn rebuild_requires_a_new_snapshot() {
    let id = sid(2, 0, 0);
    let mut ex = executor(id, SurfaceKind::Builtin);
    drive_to_ready(&mut ex, id);
    ex.set_desired(id, 7);
    ex.mark_sent(id, 7);
    assert!(ex.drain_ready_pushes().is_empty());

    // 重建:新的 ready 到达(重复 Ready 视为新实例就绪)。
    ex.apply(id, CoreEvent::Ready);
    let snapshot = ex.drain_ready_pushes();
    assert_eq!(snapshot.len(), 1, "新实例必须重新发送快照");
    assert_eq!(snapshot[0].2, "host.push(7)");
}

#[test]
fn stale_generation_ready_and_ipc_are_rejected() {
    let current = sid(3, 0, 1);
    let stale = sid(3, 0, 0);
    let mut ex = executor(current, SurfaceKind::Builtin);

    // 旧代数的上行事件在路由层被明确报告为过期。
    match ex.route(stale) {
        Routed::StaleGeneration { expected, got } => {
            assert_eq!(expected, Generation::new(1));
            assert_eq!(got, Generation::new(0));
        }
        other => panic!("旧代数应报过期,得到 {other:?}"),
    }
    assert!(!ex.route(stale).is_accepted());

    // 旧代数的状态机事件不会命中任何登记项。
    assert!(ex.apply(stale, CoreEvent::Ready).is_none());
    assert!(ex.apply(stale, CoreEvent::Created).is_none());
}

#[test]
fn create_failure_is_retryable() {
    let id = sid(4, 0, 0);
    let mut ex = executor(id, SurfaceKind::Builtin);
    assert!(ex.begin_create(id));
    ex.apply(
        id,
        CoreEvent::CreateFailed {
            error: "webview build error".into(),
        },
    );
    assert!(matches!(ex.state(id), Some(SurfaceState::Failed { .. })));

    // 重试成功。
    assert!(ex.begin_create(id), "失败后可重试");
    ex.apply(id, CoreEvent::Created);
    ex.apply(id, CoreEvent::Ready);
    assert_eq!(ex.state(id), Some(&SurfaceState::Ready));
}

#[test]
fn destroy_unregisters_routing() {
    let id = sid(5, 0, 0);
    let mut ex = executor(id, SurfaceKind::Builtin);
    drive_to_ready(&mut ex, id);
    assert!(ex.route(id).is_accepted());

    assert!(ex.apply(id, CoreEvent::Destroyed).is_some());
    let slot = ex.unregister(id).expect("注销返回物理槽");
    assert_eq!(slot, SurfaceSlot(5));
    assert!(!ex.route(id).is_accepted(), "注销后路由拒绝旧事件");
}

#[test]
fn builtin_and_application_use_different_ready_policies() {
    let builtin = sid(6, 0, 0);
    let app = sid(7, 0, 0);
    let mut ex = executor(builtin, SurfaceKind::Builtin);
    ex.register(
        app,
        SurfaceKind::Application,
        SurfaceSlot(7),
        Box::new(|v| format!("nav.sync({v})")),
    );

    assert_eq!(
        ex.ready_policy(builtin),
        Some(bytehost_core::ReadyPolicy::Handshake),
        "内置面板等待 ready 握手"
    );
    assert_eq!(
        ex.ready_policy(app),
        Some(bytehost_core::ReadyPolicy::NavigationCompleted),
        "第三方应用以导航完成作为 Ready,不要求实现内置协议"
    );
}

#[test]
fn geometry_feeds_bounds_into_lifecycle() {
    // 几何与状态机协作:窗口尺寸算出的矩形用于创建/恢复。
    let rect = surface_rect(
        1000.0,
        700.0,
        Placement::full(bytehost_core::Insets { top: 48.0 }),
    );
    assert_eq!(
        rect,
        LogicalRect {
            x: 0.0,
            y: 48.0,
            width: 1000.0,
            height: 652.0,
        }
    );
}
