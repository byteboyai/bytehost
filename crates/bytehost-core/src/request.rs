//! 请求身份与执行上下文:异步结果的**乱序防护**与**作用域写入**语义。
//!
//! 迁移自 Dozer `request_tracker.rs` 的已验证行为(队列中的旧回包失效、lane 相互独立、
//! 重开面板/切 scope 拒绝旧结果),并补上 spec §5.3 要求:
//! - 结果信封至少含 scope、panel instance、generation、request key、request sequence;
//! - **派发前**核对实例存活、scope 一致、结果属于当前有效请求(latest-only),独立 key 互不覆盖;
//! - 写操作作用域到实例(**永不因新请求被作废**),实例过期只丢弃 UI 回包,业务结果仍由服务持久化;
//! - 面板实例、服务实例、Surface 各自用不同 generation,不用一个全局计数使无关任务失效。
//!
//! 本模块**不含** Tokio/iced 具体实现:执行原语 `run_scoped` 只接收泛型服务上下文、
//! 后台执行器与 emitter,由 `bytehost-desktop`(Task 07)提供具体实现。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::id::{PanelInstanceId, ScopeId};

/// 单调递增的 generation。面板实例、服务实例、Surface 各自持有独立计数器,
/// 不用一个全局计数(避免无关任务因别处重建而失效)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    pub const ZERO: Self = Self(0);

    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// 下一代(销毁重建时调用)。
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gen{}", self.0)
    }
}

/// 请求键:同一面板内区分不同读取通道(如 `list` / `categories`)。
/// 用 `&'static str` 保证是编译期常量,避免运行期拼错导致通道串味。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestKey(&'static str);

impl RequestKey {
    pub const fn new(label: &'static str) -> Self {
        Self(label)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for RequestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// 一次操作的请求身份。读取带 `key`(latest-only);写操作 `key` 为 `None`(作用域写入)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestToken {
    owner: u64,
    scope: ScopeId,
    instance: PanelInstanceId,
    generation: Generation,
    key: Option<RequestKey>,
    seq: u64,
}

impl RequestToken {
    pub fn scope(&self) -> &ScopeId {
        &self.scope
    }
    pub fn instance(&self) -> PanelInstanceId {
        self.instance
    }
    pub fn generation(&self) -> Generation {
        self.generation
    }
    pub fn key(&self) -> Option<RequestKey> {
        self.key
    }
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 每个面板实例一个 tracker。`owner` 是进程内唯一值,使两个 tracker 即便
/// `instance`/`scope`/`generation` 相同(如重开的同 slot 面板)也不互相接受对方 token。
#[derive(Debug, Clone)]
pub struct RequestTracker(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    owner: u64,
    instance: PanelInstanceId,
    generation: Generation,
    scope: ScopeId,
    latest: Mutex<HashMap<RequestKey, u64>>,
}

impl RequestTracker {
    /// 新建一个 tracker。`owner` 由调用方给一个进程内唯一值(如原子计数)。
    pub fn new(
        owner: u64,
        scope: ScopeId,
        instance: PanelInstanceId,
        generation: Generation,
    ) -> Self {
        Self(Arc::new(Inner {
            owner,
            instance,
            generation,
            scope,
            latest: Mutex::new(HashMap::new()),
        }))
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.0.scope
    }

    pub fn instance(&self) -> PanelInstanceId {
        self.0.instance
    }

    pub fn generation(&self) -> Generation {
        self.0.generation
    }

    /// 发起一次 **latest-only 读取**。同 key 的旧 token 立即失效。
    pub fn begin(&self, key: RequestKey) -> RequestToken {
        let seq = next_seq();
        self.0.latest.lock().unwrap().insert(key, seq);
        RequestToken {
            owner: self.0.owner,
            scope: self.0.scope.clone(),
            instance: self.0.instance,
            generation: self.0.generation,
            key: Some(key),
            seq,
        }
    }

    /// **作用域写入**:写操作必须全部完成,不因新请求被作废;但实例过期后其 UI 回包被丢弃。
    pub fn scope(&self) -> RequestToken {
        RequestToken {
            owner: self.0.owner,
            scope: self.0.scope.clone(),
            instance: self.0.instance,
            generation: self.0.generation,
            key: None,
            seq: next_seq(),
        }
    }

    /// 显式作废某个 key 的当前结果(如面板进入错误态需清空)。
    pub fn invalidate(&self, key: RequestKey) {
        self.0.latest.lock().unwrap().insert(key, next_seq());
    }

    /// **派发前**校验:来源 tracker 一致、实例与 generation 仍存活、scope 一致,
    /// 且按键属于当前有效请求或为写入。
    pub fn accepts(&self, token: &RequestToken) -> bool {
        if token.owner != self.0.owner
            || token.instance != self.0.instance
            || token.generation != self.0.generation
            || token.scope != self.0.scope
        {
            return false;
        }
        match token.key {
            None => true,
            Some(key) => self
                .0
                .latest
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|v| *v == token.seq),
        }
    }
}

/// 结果信封:后台任务产出的结果在 UI 派发时必须带上的身份信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultEnvelope<T> {
    pub token: RequestToken,
    pub payload: T,
}

impl<T> ResultEnvelope<T> {
    pub fn new(token: RequestToken, payload: T) -> Self {
        Self { token, payload }
    }
}

/// 后台执行器抽象:core 不知道 Tokio 还是别的运行时,只要求"能 spawn 一个 'static 任务"。
pub trait BackgroundExecutor: Send + Sync + 'static {
    /// 把一个可能阻塞/异步的工作投递到后台线程/运行时。
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>);
}

/// 结果发射器抽象:core 不知道 iced 的 proxy,只要求"能把信封投回 UI 线程"。
pub trait Emitter<M>: Send + Sync + 'static {
    fn emit(&self, message: M);
}

/// 一个**作用域请求**的执行原语:
/// 1. 读取 `context`(泛型服务上下文,core 不解释其类型);
/// 2. 在后台执行器上跑 `work`,产出信封;
/// 3. 结果经 `emitter` 投回 UI,在后台完成时由 `accepts` 先做一次校验(超期的丢弃)。
///
/// **契约(重要)**:这里的 `accepts` 是**尽力而为的前置过滤**——它在 `work` 完成、
/// `emit` 入队**之前**运行,能挡掉"后台还在跑时又被更晚的同 key 请求取代"的结果,
/// 但**不能**保证"已入队、UI 尚未消费时**又被取代**"的结果不会送达:一旦 `emit`
/// 把信封投进 UI 通道,后续 `begin` 无法撤回它。
///
/// 因此**权威校验必须在消费点**做:UI 收到信封后,应再次用当前 `RequestTracker`
/// 调 [`RequestTracker::accepts`] 再分发给业务(见产品侧 `AsyncResult` 处理路径)。
/// 本函数只负责让"明显过期"的结果尽早消失,减少跨线程投递。
pub fn run_scoped<C, M, W, EX, EM>(
    tracker: &RequestTracker,
    token: RequestToken,
    context: C,
    executor: &EX,
    emitter: &EM,
    work: W,
) where
    C: Send + 'static,
    M: Send + 'static,
    W: FnOnce(C) -> M + Send + 'static,
    EX: BackgroundExecutor,
    EM: Emitter<ResultEnvelope<M>> + Clone,
{
    let tracker = tracker.clone();
    let emitter = emitter.clone();
    executor.spawn(Box::new(move || {
        let envelope = ResultEnvelope::new(token, work(context));
        // 前置过滤:完成时若身份已过期则丢弃(业务结果已由服务持久化)。
        // 注意这是尽力而为——已入队后又被取代的结果仍需消费点重校验(见上方契约)。
        if tracker.accepts(&envelope.token) {
            emitter.emit(envelope);
        }
    }));
}

/// 一个面板实例、**跨多个 scope** 的 tracker 集合。
///
/// 有些面板同时服务多个作用域(如浏览器既有"全局书签"lane 又有"项目书签"lane)。
/// 单个 [`RequestTracker`] 只绑定一个 scope,故这里惰性为每个 `ScopeId` 建一个
/// core tracker,但共用同一个 `owner`/`instance`/`generation`——保证同一实例不同
/// scope 的 token 因 scope 不同而互不接受,且实例重建后旧 token 全被拒。
///
/// 这是把 Dozer 迁移前 `request_tracker.rs` 的"一 tracker 服务多 scope"语义提升为
/// 中立机制(compat bridge C8):产品不再需要自己的包装类型,只需按业务把项目/全局
/// 映射成 `ScopeId`。
#[derive(Debug, Clone)]
pub struct ScopedTrackers(Arc<ScopedInner>);

#[derive(Debug)]
struct ScopedInner {
    owner: u64,
    instance: PanelInstanceId,
    generation: Generation,
    scopes: Mutex<HashMap<ScopeId, RequestTracker>>,
}

impl ScopedTrackers {
    /// `owner` 由调用方给一个进程内唯一值(重开的同 slot 面板互不接受对方 token)。
    pub fn new(owner: u64, instance: PanelInstanceId, generation: Generation) -> Self {
        Self(Arc::new(ScopedInner {
            owner,
            instance,
            generation,
            scopes: Mutex::new(HashMap::new()),
        }))
    }

    /// 取(或惰性建)某 scope 的 core tracker。
    fn core_for(&self, scope: &ScopeId) -> RequestTracker {
        let mut scopes = self.0.scopes.lock().unwrap();
        scopes
            .entry(scope.clone())
            .or_insert_with(|| {
                RequestTracker::new(
                    self.0.owner,
                    scope.clone(),
                    self.0.instance,
                    self.0.generation,
                )
            })
            .clone()
    }

    /// 发起一次 **latest-only 读取**(该 scope 内)。同 key 的旧 token 立即失效。
    pub fn begin(&self, scope: &ScopeId, key: RequestKey) -> RequestToken {
        self.core_for(scope).begin(key)
    }

    /// **作用域写入**(该 scope 内):不因新请求被作废;实例过期后其 UI 回包被丢弃。
    pub fn scope(&self, scope: &ScopeId) -> RequestToken {
        self.core_for(scope).scope()
    }

    /// 显式作废某个 key 的当前结果,**对所有已知 scope 一并作废**
    /// (等价于迁移前"单 tracker 单表"的语义)。
    pub fn invalidate(&self, key: RequestKey) {
        let scopes = self.0.scopes.lock().unwrap();
        for tracker in scopes.values() {
            tracker.invalidate(key);
        }
    }

    /// **派发前**校验:来源 tracker 一致、实例与 generation 仍存活、scope 一致,
    /// 且属于当前有效请求或写入。scope 直接从 `token.scope()` 取,故无需产品携带。
    pub fn accepts(&self, token: &RequestToken) -> bool {
        let scopes = self.0.scopes.lock().unwrap();
        match scopes.get(token.scope()) {
            Some(tracker) => tracker.accepts(token),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
    const CATEGORIES: RequestKey = RequestKey::new("categories");

    #[test]
    fn queued_results_are_invalidated_and_keys_are_independent() {
        let t = tracker(1, "p1", 1, 0);
        let old = t.begin(LIST);
        let categories = t.begin(CATEGORIES);
        let new = t.begin(LIST);
        assert!(!t.accepts(&old));
        assert!(t.accepts(&new));
        assert!(t.accepts(&categories));
        t.invalidate(LIST);
        assert!(!t.accepts(&new));
    }

    #[test]
    fn reopened_instance_rejects_old_results() {
        let original = tracker(1, "p7", 1, 0);
        let token = original.begin(LIST);
        // 另一个 tracker(owner 不同)——即便 scope/instance/generation 相同也拒绝。
        let reopened = tracker(2, "p7", 1, 0);
        assert!(!reopened.accepts(&token));
        assert!(original.accepts(&token));
    }

    #[test]
    fn same_slot_different_generation_rejects() {
        let t0 = tracker(1, "p", 4, 0);
        let tok = t0.begin(LIST);
        let t1 = tracker(2, "p", 4, 1); // 同 slot,generation+1
        assert!(!t1.accepts(&tok));
        assert!(t0.accepts(&tok));
    }

    #[test]
    fn scoped_write_survives_new_reads_but_dies_with_instance() {
        let t = tracker(1, "p7", 1, 0);
        let write = t.scope();
        t.begin(LIST); // 新的读取不该作废写
        assert!(t.accepts(&write));
        // 实例过期(不同 tracker)则写回包被丢弃。
        let other = tracker(2, "p7", 1, 0);
        assert!(!other.accepts(&write));
    }

    #[test]
    fn same_id_different_scope_is_rejected() {
        let a = tracker(1, "proj-1", 1, 0);
        let tok = a.begin(LIST);
        // 同 owner,但 scope 不同(如切项目)。
        let b = tracker(1, "proj-2", 1, 0);
        assert!(!b.accepts(&tok));
    }

    #[test]
    fn independent_keys_do_not_overwrite_each_other() {
        let t = tracker(1, "p", 1, 0);
        let a = t.begin(LIST);
        let b = t.begin(CATEGORIES);
        assert!(t.accepts(&a));
        assert!(t.accepts(&b));
        // 刷新 list 只作废 list。
        let a2 = t.begin(LIST);
        assert!(!t.accepts(&a));
        assert!(t.accepts(&a2));
        assert!(t.accepts(&b));
    }

    #[test]
    fn new_request_after_queued_reply_rejects_the_old_one() {
        let t = tracker(1, "p", 1, 0);
        let first = t.begin(LIST);
        // 模拟:first 的回包已在队列里,此时又发起了新请求。
        let second = t.begin(LIST);
        assert!(!t.accepts(&first));
        assert!(t.accepts(&second));
    }

    // ---- 多 scope 集合(ScopedTrackers) ----

    #[test]
    fn scoped_trackers_serve_multiple_scopes_independently() {
        let t = ScopedTrackers::new(1, PanelInstanceId::new(0, 0), Generation::new(0));
        let a = t.begin(&scope("project:1"), LIST);
        let b = t.begin(&scope("global"), LIST);
        assert!(t.accepts(&a));
        assert!(t.accepts(&b));
        // 刷新项目 lane 不影响全局 lane。
        let a2 = t.begin(&scope("project:1"), LIST);
        assert!(!t.accepts(&a));
        assert!(t.accepts(&a2));
        assert!(t.accepts(&b));
    }

    #[test]
    fn scoped_trackers_reject_unknown_scope_and_reopened_instance() {
        let t = ScopedTrackers::new(1, PanelInstanceId::new(0, 0), Generation::new(0));
        let tok = t.begin(&scope("project:7"), LIST);
        // 另一个实例(owner 不同)即便 scope/instance/generation 相同也拒绝。
        let reopened = ScopedTrackers::new(2, PanelInstanceId::new(0, 0), Generation::new(0));
        assert!(!reopened.accepts(&tok));
        assert!(t.accepts(&tok));
        // 从未发起的 scope:直接拒。
        let fresh = ScopedTrackers::new(1, PanelInstanceId::new(0, 0), Generation::new(0));
        assert!(!fresh.accepts(&tok));
    }

    #[test]
    fn scoped_trackers_invalidate_applies_to_all_known_scopes() {
        let t = ScopedTrackers::new(1, PanelInstanceId::new(0, 0), Generation::new(0));
        let a = t.begin(&scope("project:1"), LIST);
        let b = t.begin(&scope("project:2"), LIST);
        t.invalidate(LIST);
        assert!(!t.accepts(&a));
        assert!(!t.accepts(&b));
    }

    #[test]
    fn scoped_write_is_scoped_but_survives_new_reads() {
        let t = ScopedTrackers::new(1, PanelInstanceId::new(0, 0), Generation::new(0));
        let write = t.scope(&scope("project:3"));
        t.begin(&scope("project:3"), LIST);
        assert!(t.accepts(&write), "写不因同 scope 新读取被作废");
        // 写属于 project:3;一个独立的实例对 project:4 的写不被本实例接受。
        let other = ScopedTrackers::new(2, PanelInstanceId::new(0, 0), Generation::new(0));
        let other_write = other.scope(&scope("project:4"));
        assert!(!t.accepts(&other_write));
    }

    // ---- 执行原语:派发前校验 ----

    #[derive(Clone)]
    struct InlineExecutor; // 同步"后台",便于测试
    impl BackgroundExecutor for InlineExecutor {
        fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
            task()
        }
    }

    #[derive(Clone)]
    struct ChannelEmitter<T>(mpsc::Sender<T>);
    impl<T: Send + 'static> Emitter<T> for ChannelEmitter<T> {
        fn emit(&self, message: T) {
            let _ = self.0.send(message);
        }
    }

    #[test]
    fn run_scoped_drops_result_when_superseded_before_dispatch() {
        let t = tracker(1, "p", 1, 0);
        let (tx, rx) = mpsc::channel();
        let emitter = ChannelEmitter(tx);
        // 先入队 first 的执行(还未来得及派发),再发起 second 使其失效。
        let first = t.begin(LIST);
        // 手动使 first 过期。
        let _second = t.begin(LIST);
        run_scoped(&t, first, (), &InlineExecutor, &emitter, |_| {
            "stale".to_string()
        });
        assert!(rx.try_recv().is_err(), "过期结果必须在派发前被丢弃");
    }

    #[test]
    fn run_scoped_delivers_current_result() {
        let t = tracker(1, "p", 1, 0);
        let (tx, rx) = mpsc::channel();
        let emitter = ChannelEmitter(tx);
        let token = t.begin(LIST);
        run_scoped(
            &t,
            token,
            "ctx".to_string(),
            &InlineExecutor,
            &emitter,
            |c| format!("{c}-done"),
        );
        let env = rx.try_recv().expect("当前结果应被派发");
        assert_eq!(env.payload, "ctx-done");
        assert_eq!(env.token.key(), Some(LIST));
    }

    #[test]
    fn two_writes_both_complete_and_independent_reads_only_latest_applies() {
        // 写操作即使被新请求"包围"也都能完成;读取只应用最新一次。
        let t = tracker(1, "p", 1, 0);
        let (tx, rx) = mpsc::channel();
        let emitter = ChannelEmitter(tx);

        let w1 = t.scope();
        let w2 = t.scope();
        let r_old = t.begin(LIST);
        let r_new = t.begin(LIST);
        assert!(!t.accepts(&r_old));
        assert!(t.accepts(&r_new));

        for (tok, tag) in [(w1, "w1"), (w2, "w2"), (r_old, "r_old"), (r_new, "r_new")] {
            run_scoped(&t, tok, (), &InlineExecutor, &emitter, move |_| {
                tag.to_string()
            });
        }
        let got: Vec<String> = rx.try_iter().map(|e| e.payload).collect();
        assert!(got.contains(&"w1".to_string()), "写 1 必须完成");
        assert!(got.contains(&"w2".to_string()), "写 2 必须完成");
        assert!(!got.contains(&"r_old".to_string()), "旧读取必须被丢弃");
        assert!(got.contains(&"r_new".to_string()), "最新读取必须应用");
    }

    #[test]
    fn already_emitted_result_is_not_retracted_by_a_later_begin_but_consumer_rejects_it() {
        // 边界契约:结果一旦入队,后续 begin 不能撤回——run_scoped 的前置过滤只
        // 覆盖"完成前被取代"。因此消费点必须再次用 accepts 校验(产品侧 AsyncResult
        // 的权威检查)。本测试把这条边界钉死,防止文档再次夸大前置校验的保证。
        let t = tracker(1, "p", 1, 0);
        let (tx, rx) = mpsc::channel();
        let emitter = ChannelEmitter(tx);
        let first = t.begin(LIST);
        run_scoped(&t, first.clone(), (), &InlineExecutor, &emitter, |_| {
            "first".to_string()
        });
        // 结果已入队(前置校验通过,因为此刻 first 仍是最新)。
        let env = rx.try_recv().expect("first 结果已入队");
        assert_eq!(env.payload, "first");
        // 现在发起新请求:入队的旧结果不会被撤回……
        let _second = t.begin(LIST);
        assert!(!t.accepts(&env.token), "……但消费点重校验会拒绝它");
        assert!(t.accepts(&t.begin(LIST)), "最新请求仍被接受");
    }
}
