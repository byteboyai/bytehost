//! `bytehost-core::request` 的路由集成测试(计划 Task 05)。
//!
//! 端到端验证:派发前校验、scope 隔离、generation 区分、独立 key 不互相覆盖、
//! 写操作不被读取作废、以及执行原语在结果过期时丢弃 UI 回包。

use std::sync::mpsc;

use bytehost_core::request::{
    BackgroundExecutor, Emitter, Generation, RequestKey, RequestTracker, ResultEnvelope,
    ScopedTrackers, run_scoped,
};
use bytehost_core::{PanelInstanceId, ScopeId};

fn scope(s: &str) -> ScopeId {
    ScopeId::new(s).unwrap()
}

fn tracker(owner: u64, sc: &str, slot: u32, generation: u64) -> RequestTracker {
    RequestTracker::new(
        owner,
        scope(sc),
        PanelInstanceId::new(slot, 0),
        Generation::new(generation),
    )
}

const LIST: RequestKey = RequestKey::new("list");
const DETAIL: RequestKey = RequestKey::new("detail");

#[derive(Clone)]
struct Inline;
impl BackgroundExecutor for Inline {
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        task()
    }
}

#[derive(Clone)]
struct Channel(mpsc::Sender<ResultEnvelope<String>>);
impl Emitter<ResultEnvelope<String>> for Channel {
    fn emit(&self, message: ResultEnvelope<String>) {
        let _ = self.0.send(message);
    }
}

#[test]
fn out_of_order_reads_only_apply_latest_per_key() {
    let t = tracker(1, "p", 1, 0);
    let a = t.begin(LIST);
    let b = t.begin(DETAIL);
    let a2 = t.begin(LIST);

    assert!(!t.accepts(&a), "同 key 的旧读取失效");
    assert!(t.accepts(&a2), "同 key 的最新读取生效");
    assert!(t.accepts(&b), "不同 key 独立,不受影响");
}

#[test]
fn reopened_panel_and_new_generation_reject_stale_results() {
    let inst = PanelInstanceId::new(2, 0);
    let g0 = RequestTracker::new(10, scope("p"), inst, Generation::new(0));
    let tok = g0.begin(LIST);

    // 同 slot、generation+1(重建)的实例拒绝旧 token。
    let g1 = RequestTracker::new(11, scope("p"), inst, Generation::new(1));
    assert!(!g1.accepts(&tok));
    // 完全重开的实例(owner 不同)也拒绝。
    let reopened = RequestTracker::new(12, scope("p"), inst, Generation::new(0));
    assert!(!reopened.accepts(&tok));
    assert!(g0.accepts(&tok));
}

#[test]
fn same_id_different_scope_is_isolated() {
    let proj1 = tracker(1, "proj-1", 1, 0);
    let proj2 = tracker(1, "proj-2", 1, 0);
    let tok = proj1.begin(LIST);
    assert!(proj1.accepts(&tok));
    assert!(
        !proj2.accepts(&tok),
        "切换 scope 后旧结果不得改变新 scope 状态"
    );
}

#[test]
fn scoped_write_completes_regardless_of_concurrent_reads() {
    let t = tracker(1, "p", 1, 0);
    let write = t.scope();
    // 多个新读取发起,写仍有效。
    t.begin(LIST);
    t.begin(LIST);
    assert!(t.accepts(&write), "写操作不被新请求作废");
}

#[test]
fn executor_drops_superseded_dispatch_and_keeps_writes() {
    let t = tracker(1, "p", 1, 0);
    let (tx, rx) = mpsc::channel();
    let emitter = Channel(tx);

    let stale_read = t.begin(LIST);
    let _fresh_read = t.begin(LIST); // 作废 stale_read
    let write = t.scope();

    run_scoped(&t, stale_read, (), &Inline, &emitter, |_| {
        "read-stale".into()
    });
    run_scoped(&t, write, (), &Inline, &emitter, |_| "write".into());

    let payloads: Vec<String> = rx.try_iter().map(|e| e.payload).collect();
    assert!(
        !payloads.contains(&"read-stale".to_string()),
        "过期读取被丢弃"
    );
    assert!(payloads.contains(&"write".to_string()), "写操作仍完成");
}

#[test]
fn independent_env_envelopes_carry_identity() {
    let t = tracker(1, "p", 1, 0);
    let (tx, rx) = mpsc::channel();
    let emitter = Channel(tx);
    let tok = t.begin(LIST);
    run_scoped(&t, tok, (), &Inline, &emitter, |_| "ok".into());
    let env = rx.try_recv().unwrap();
    assert_eq!(env.token.scope(), &scope("p"));
    assert_eq!(env.token.instance(), PanelInstanceId::new(1, 0));
    assert_eq!(env.token.generation(), Generation::new(0));
    assert_eq!(env.token.key(), Some(LIST));
    assert!(env.token.seq() > 0);
}

#[test]
fn scoped_trackers_route_dual_lane_panel_by_scope() {
    // 模拟"一个面板实例同时服务全局 + 项目"两条 lane(浏览器书签场景)。
    let t = ScopedTrackers::new(1, PanelInstanceId::new(0, 0), Generation::new(0));
    let project = t.begin(&scope("project:9"), LIST);
    let global = t.begin(&scope("global"), LIST);

    // 两条 lane 互不接受,即使 key 相同、实例相同。
    assert!(t.accepts(&project));
    assert!(t.accepts(&global));
    assert_ne!(project.scope(), global.scope());

    // 切项目后旧项目 lane 的新读取作废旧结果;全局 lane 不受影响。
    let project2 = t.begin(&scope("project:9"), LIST);
    assert!(!t.accepts(&project));
    assert!(t.accepts(&project2));
    assert!(t.accepts(&global));
}
