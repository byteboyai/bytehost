//! 父窗口所属对话框的生命周期与几何(纯状态,不依赖任何界面框架)。
//!
//! 产品与驱动只执行这里产出的类型化 [`DialogEffect`],并把运行时回包喂回
//! [`DialogController::handle`]。这样 winit / iced 两个驱动可以共用同一套
//! "重复打开、迟到 Built、失败重试、幂等关闭、父窗退出" 策略。
//!
//! 身份由 `owner + kind + session + attempt` 四段组成:
//! - `session`:一次新开窗口递增;重复打开只激活,不递增。
//! - `attempt`:同一次会话内创建失败后的显式重试递增。
//!
//! 所有回包都必须带完整身份;旧会话 / 旧 attempt 的回包不会修改当前状态,
//! 迟到的创建成功会得到一个 `Destroy` effect,确保不留下孤儿窗口。

use std::fmt;

/// 对话框种类。目前只有设置;其他弹窗迁移时再追加,不为未迁移业务预建枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialogKind {
    Settings,
}

impl DialogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DialogKind::Settings => "settings",
        }
    }
}

/// 一次对话框实例的身份。`owner` 由产品给出(通常是主窗口标识)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DialogKey {
    pub owner: u64,
    pub kind: DialogKind,
    pub session: u64,
    pub attempt: u32,
}

impl fmt::Display for DialogKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:s{}:a{}",
            self.owner,
            self.kind.as_str(),
            self.session,
            self.attempt
        )
    }
}

/// 创建 / 绑定 / 渲染失败的结构化原因。产品负责本地化展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogFailure {
    /// 失败阶段:`create` / `bind` / `render`。
    pub stage: &'static str,
    /// 底层诊断原文(不翻译、不进普通用户文案)。
    pub detail: String,
}

impl DialogFailure {
    pub fn new(stage: &'static str, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
        }
    }
}

/// 控制器当前相位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Closed,
    Opening,
    Open,
    Closing,
    Failed(DialogFailure),
}

impl Phase {
    pub fn is_open(&self) -> bool {
        matches!(self, Phase::Open)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Phase::Closed | Phase::Failed(_))
    }
}

/// 驱动需要执行的原生命令。纯数据,可被任意运行时解释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogEffect {
    /// 创建并绑定新的原生实例;成功后必须回 [`DialogEvent::Opened`]。
    Create(DialogKey),
    /// 已有实例:激活并（可选）切页由产品另外处理。
    Activate(DialogKey),
    /// 销毁实例。`window` 为 `None` 表示取消尚未完成的创建。
    Destroy {
        key: DialogKey,
        window: Option<u64>,
        reason: DestroyReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestroyReason {
    /// 用户关闭。
    Close,
    /// 打开期间被关闭,创建完成后必须立刻销毁。
    CancelledOpen,
    /// 旧会话 / 旧 attempt 的迟到资源。
    Stale,
    /// 创建失败后的清理。
    Failed,
    /// 父窗口已经退出。
    ParentClosed,
    /// 有界销毁超时,驱动必须强制释放。
    Timeout,
}

/// 驱动回包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogEvent {
    /// 原生实例创建并绑定成功。
    Opened { key: DialogKey, window: u64 },
    /// 创建 / 绑定 / 渲染失败。
    Failed {
        key: DialogKey,
        failure: DialogFailure,
    },
    /// 原生实例已被销毁(无论由谁发起)。
    Closed { key: DialogKey, window: u64 },
    /// Esc / 关闭按钮 / 系统关闭。
    CloseRequested,
    /// 父窗口退出;控制器不得停留在 Closing。
    ParentClosed,
}

/// 共享对话框控制器。
#[derive(Debug, Clone)]
pub struct DialogController {
    kind: DialogKind,
    owner: u64,
    phase: Phase,
    current: Option<DialogKey>,
    window: Option<u64>,
    next_session: u64,
    /// 关闭过程中又收到打开请求:等 Closed 回包后自动重开。
    reopen_after_close: bool,
}

impl DialogController {
    pub fn new(owner: u64, kind: DialogKind) -> Self {
        Self {
            kind,
            owner,
            phase: Phase::Closed,
            current: None,
            window: None,
            next_session: 0,
            reopen_after_close: false,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn key(&self) -> Option<DialogKey> {
        self.current
    }

    pub fn window(&self) -> Option<u64> {
        self.window
    }

    pub fn owner(&self) -> u64 {
        self.owner
    }

    pub fn kind(&self) -> DialogKind {
        self.kind
    }

    /// 重复打开:已 Open 只激活;Opening / Closing 幂等;Closed / Failed 新开会话。
    pub fn open(&mut self) -> Vec<DialogEffect> {
        match self.phase {
            Phase::Open => self
                .current
                .map(|key| vec![DialogEffect::Activate(key)])
                .unwrap_or_default(),
            Phase::Opening => Vec::new(),
            Phase::Closing => {
                // 关闭尚未完成:记下意图,Closed 回包后开始新会话,避免窗口复活。
                self.reopen_after_close = true;
                Vec::new()
            }
            Phase::Closed | Phase::Failed(_) => self.start_session(),
        }
    }

    /// 显式重试:只对 Failed 有效,同一 session 递增 attempt;其他相位等价于 open/幂等。
    pub fn retry(&mut self) -> Vec<DialogEffect> {
        match &self.phase {
            Phase::Failed(_) => {
                let Some(current) = self.current else {
                    return self.start_session();
                };
                let attempt = current.attempt.saturating_add(1);
                let key = DialogKey { attempt, ..current };
                self.current = Some(key);
                self.phase = Phase::Opening;
                self.window = None;
                vec![DialogEffect::Create(key)]
            }
            _ => self.open(),
        }
    }

    /// 统一关闭协议;重复关闭幂等。
    pub fn close(&mut self) -> Vec<DialogEffect> {
        match self.phase.clone() {
            Phase::Open => {
                let Some(key) = self.current else {
                    self.phase = Phase::Closed;
                    return Vec::new();
                };
                self.phase = Phase::Closing;
                vec![DialogEffect::Destroy {
                    key,
                    window: self.window,
                    reason: DestroyReason::Close,
                }]
            }
            Phase::Opening => {
                let Some(key) = self.current else {
                    self.phase = Phase::Closed;
                    return Vec::new();
                };
                self.phase = Phase::Closing;
                vec![DialogEffect::Destroy {
                    key,
                    window: self.window,
                    reason: DestroyReason::CancelledOpen,
                }]
            }
            Phase::Failed(_) => {
                self.reset();
                Vec::new()
            }
            Phase::Closing | Phase::Closed => Vec::new(),
        }
    }

    /// 父窗口退出:即使创建回包永远不到,也必须有界地回到 Closed。
    pub fn parent_closed(&mut self) -> Vec<DialogEffect> {
        let effects = match (self.current, self.window) {
            (Some(key), window) if !matches!(self.phase, Phase::Closed) => {
                vec![DialogEffect::Destroy {
                    key,
                    window,
                    reason: DestroyReason::ParentClosed,
                }]
            }
            _ => Vec::new(),
        };
        self.reset();
        effects
    }

    /// 有界销毁超时:驱动可强制释放句柄,控制器不再等待。
    pub fn destroy_timed_out(&mut self) -> Vec<DialogEffect> {
        let effects = match (self.current, self.window) {
            (Some(key), Some(window)) => vec![DialogEffect::Destroy {
                key,
                window: Some(window),
                reason: DestroyReason::Timeout,
            }],
            _ => Vec::new(),
        };
        self.reset();
        effects
    }

    /// 处理驱动回包。所有 effect 都只针对当前身份产生。
    pub fn handle(&mut self, event: DialogEvent) -> Vec<DialogEffect> {
        match event {
            DialogEvent::Opened { key, window } => self.on_opened(key, window),
            DialogEvent::Failed { key, failure } => self.on_failed(key, failure),
            DialogEvent::Closed { key, window } => self.on_closed(key, window),
            DialogEvent::CloseRequested => self.close(),
            DialogEvent::ParentClosed => self.parent_closed(),
        }
    }

    /// 驱动在已 Open 之后报告绑定 / 渲染失败(例如父窗口被销毁后重新同步失败)。
    /// 保留导航与账户状态,只把相位改为 Failed,由产品显式重试。
    pub fn fail_current(&mut self, failure: DialogFailure) {
        if self.current.is_some() && !matches!(self.phase, Phase::Closed) {
            self.phase = Phase::Failed(failure);
        }
    }

    fn start_session(&mut self) -> Vec<DialogEffect> {
        self.next_session = self.next_session.saturating_add(1).max(1);
        let key = DialogKey {
            owner: self.owner,
            kind: self.kind,
            session: self.next_session,
            attempt: 1,
        };
        self.current = Some(key);
        self.window = None;
        self.phase = Phase::Opening;
        self.reopen_after_close = false;
        vec![DialogEffect::Create(key)]
    }

    fn on_opened(&mut self, key: DialogKey, window: u64) -> Vec<DialogEffect> {
        if self.current != Some(key) {
            // 迟到的旧会话 / 旧 attempt 成功:资源已创建,必须销毁而不是只丢事件。
            return vec![DialogEffect::Destroy {
                key,
                window: Some(window),
                reason: DestroyReason::Stale,
            }];
        }
        match self.phase {
            Phase::Opening => {
                self.window = Some(window);
                self.phase = Phase::Open;
                Vec::new()
            }
            Phase::Closing => {
                self.window = Some(window);
                let effects = vec![DialogEffect::Destroy {
                    key,
                    window: Some(window),
                    reason: DestroyReason::CancelledOpen,
                }];
                self.phase = Phase::Closed;
                self.window = None;
                self.current = None;
                self.finish_pending_reopen(effects)
            }
            Phase::Open if self.window == Some(window) => Vec::new(),
            Phase::Open | Phase::Failed(_) | Phase::Closed => {
                // 同 key 的重复成功(例如驱动重放)只保留一个实例。
                if self.window == Some(window) {
                    Vec::new()
                } else {
                    vec![DialogEffect::Destroy {
                        key,
                        window: Some(window),
                        reason: DestroyReason::Stale,
                    }]
                }
            }
        }
    }

    fn on_failed(&mut self, key: DialogKey, failure: DialogFailure) -> Vec<DialogEffect> {
        if self.current != Some(key) {
            return Vec::new();
        }
        match self.phase {
            Phase::Opening => {
                self.phase = Phase::Failed(failure);
                self.window = None;
                Vec::new()
            }
            Phase::Closing => {
                self.reset();
                Vec::new()
            }
            Phase::Open | Phase::Failed(_) | Phase::Closed => Vec::new(),
        }
    }

    fn on_closed(&mut self, key: DialogKey, window: u64) -> Vec<DialogEffect> {
        if self.current != Some(key) {
            return Vec::new();
        }
        if self.window.is_some_and(|current| current != window) {
            // 不是当前实例的回包。
            return Vec::new();
        }
        self.window = None;
        self.current = None;
        self.phase = Phase::Closed;
        self.finish_pending_reopen(Vec::new())
    }

    fn finish_pending_reopen(&mut self, effects: Vec<DialogEffect>) -> Vec<DialogEffect> {
        if self.reopen_after_close {
            let mut effects = effects;
            effects.extend(self.start_session());
            effects
        } else {
            effects
        }
    }

    fn reset(&mut self) {
        self.phase = Phase::Closed;
        self.current = None;
        self.window = None;
        self.reopen_after_close = false;
    }
}

// ---------------------------------------------------------------------------
// 几何:统一 40% 宽 / 75% 高,正常空间最小 560×520,四周目标留白 16。
// 纯逻辑像素;产品负责把逻辑尺寸换成运行时物理尺寸。
// ---------------------------------------------------------------------------

/// 逻辑尺寸 `(width, height)`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

impl LogicalSize {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// 相对父窗口左上角的逻辑矩形。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// 卡片几何策略。默认值即产品统一规范。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DialogGeometry {
    pub width_fraction: f32,
    pub height_fraction: f32,
    pub min_width: f32,
    pub min_height: f32,
    pub margin: f32,
}

impl Default for DialogGeometry {
    fn default() -> Self {
        Self {
            width_fraction: 0.40,
            height_fraction: 0.75,
            min_width: 560.0,
            min_height: 520.0,
            margin: 16.0,
        }
    }
}

impl DialogGeometry {
    /// 把配置钳进合法范围,避免负尺寸 / 超 100% 的输入把关闭按钮挤出父窗口。
    pub fn sanitized(self) -> Self {
        let fraction = |v: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let non_negative = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        Self {
            width_fraction: fraction(self.width_fraction),
            height_fraction: fraction(self.height_fraction),
            min_width: non_negative(self.min_width),
            min_height: non_negative(self.min_height),
            margin: non_negative(self.margin),
        }
    }

    /// 卡片在父客户区内的居中矩形。父窗口过小时收紧留白并钳到可用区域,
    /// 保证卡片、标题和关闭操作始终可见。
    pub fn card_rect(self, parent: LogicalSize) -> LogicalRect {
        let cfg = self.sanitized();
        let parent_w = finite_positive(parent.width);
        let parent_h = finite_positive(parent.height);
        let margin_x = cfg.margin.min(parent_w / 2.0);
        let margin_y = cfg.margin.min(parent_h / 2.0);
        let avail_w = (parent_w - margin_x * 2.0).max(1.0);
        let avail_h = (parent_h - margin_y * 2.0).max(1.0);

        let width = (parent_w * cfg.width_fraction)
            .max(cfg.min_width)
            .min(avail_w)
            .max(1.0);
        let height = (parent_h * cfg.height_fraction)
            .max(cfg.min_height)
            .min(avail_h)
            .max(1.0);

        LogicalRect {
            x: ((parent_w - width) / 2.0).max(0.0),
            y: ((parent_h - height) / 2.0).max(0.0),
            width,
            height,
        }
    }
}

fn finite_positive(value: f32) -> f32 {
    if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> DialogController {
        DialogController::new(7, DialogKind::Settings)
    }

    fn effect_key(effects: &[DialogEffect]) -> DialogKey {
        match effects.first().expect("expected an effect") {
            DialogEffect::Create(key) | DialogEffect::Activate(key) => *key,
            other => panic!("expected create/activate, got {other:?}"),
        }
    }

    #[test]
    fn first_open_creates_and_repeat_open_only_activates() {
        let mut c = controller();
        let first = c.open();
        assert_eq!(first.len(), 1);
        let key = effect_key(&first);
        assert_eq!(key.owner, 7);
        assert_eq!(key.session, 1);
        assert_eq!(key.attempt, 1);
        assert_eq!(*c.phase(), Phase::Opening);

        // 创建中重复打开幂等。
        assert!(c.open().is_empty());

        let opened = c.handle(DialogEvent::Opened { key, window: 42 });
        assert!(opened.is_empty());
        assert_eq!(*c.phase(), Phase::Open);
        assert_eq!(c.window(), Some(42));

        let again = c.open();
        assert_eq!(again, vec![DialogEffect::Activate(key)]);
        assert_eq!(c.key(), Some(key));
    }

    #[test]
    fn close_is_idempotent_and_waits_for_closed() {
        let mut c = controller();
        let key = effect_key(&c.open());
        c.handle(DialogEvent::Opened { key, window: 42 });

        assert_eq!(
            c.close(),
            vec![DialogEffect::Destroy {
                key,
                window: Some(42),
                reason: DestroyReason::Close
            }]
        );
        assert_eq!(*c.phase(), Phase::Closing);
        assert!(c.close().is_empty());

        assert!(c.handle(DialogEvent::Closed { key, window: 42 }).is_empty());
        assert_eq!(*c.phase(), Phase::Closed);
        assert_eq!(c.window(), None);
        assert!(c.close().is_empty());
    }

    #[test]
    fn close_before_creation_completes_destroys_late_window() {
        let mut c = controller();
        let key = effect_key(&c.open());
        assert_eq!(
            c.close(),
            vec![DialogEffect::Destroy {
                key,
                window: None,
                reason: DestroyReason::CancelledOpen
            }]
        );

        // 创建成功回包晚到:必须销毁其资源,不能复活。
        assert_eq!(
            c.handle(DialogEvent::Opened { key, window: 99 }),
            vec![DialogEffect::Destroy {
                key,
                window: Some(99),
                reason: DestroyReason::CancelledOpen
            }]
        );
        assert_eq!(*c.phase(), Phase::Closed);
        assert_eq!(c.window(), None);
    }

    #[test]
    fn stale_open_and_failure_do_not_touch_current_session() {
        let mut c = controller();
        let first = effect_key(&c.open());
        c.close();
        c.handle(DialogEvent::Closed {
            key: first,
            window: 0,
        });
        let second = effect_key(&c.open());
        assert_ne!(first.session, second.session);

        // 旧会话的迟到成功只销毁旧资源。
        assert_eq!(
            c.handle(DialogEvent::Opened {
                key: first,
                window: 11
            }),
            vec![DialogEffect::Destroy {
                key: first,
                window: Some(11),
                reason: DestroyReason::Stale
            }]
        );
        assert_eq!(c.key(), Some(second));
        assert_eq!(*c.phase(), Phase::Opening);

        // 旧会话失败完全忽略。
        assert!(
            c.handle(DialogEvent::Failed {
                key: first,
                failure: DialogFailure::new("create", "old")
            })
            .is_empty()
        );
        assert_eq!(c.key(), Some(second));
    }

    #[test]
    fn failure_is_visible_and_retry_increments_attempt_in_the_same_session() {
        let mut c = controller();
        let first = effect_key(&c.open());
        c.handle(DialogEvent::Failed {
            key: first,
            failure: DialogFailure::new("bind", "no parent"),
        });
        assert!(matches!(c.phase(), Phase::Failed(_)));
        assert_eq!(c.key(), Some(first));

        let retried = c.retry();
        let second = effect_key(&retried);
        assert_eq!(second.session, first.session);
        assert_eq!(second.attempt, 2);
        assert_eq!(*c.phase(), Phase::Opening);
    }

    #[test]
    fn failed_close_returns_to_closed_without_effects() {
        let mut c = controller();
        let key = effect_key(&c.open());
        c.handle(DialogEvent::Failed {
            key,
            failure: DialogFailure::new("render", "boom"),
        });
        assert!(c.close().is_empty());
        assert_eq!(*c.phase(), Phase::Closed);
        assert_eq!(c.key(), None);
    }

    #[test]
    fn parent_close_always_reaches_closed_and_destroys_known_window() {
        let mut c = controller();
        let key = effect_key(&c.open());
        c.handle(DialogEvent::Opened { key, window: 5 });
        assert_eq!(
            c.handle(DialogEvent::ParentClosed),
            vec![DialogEffect::Destroy {
                key,
                window: Some(5),
                reason: DestroyReason::ParentClosed
            }]
        );
        assert_eq!(*c.phase(), Phase::Closed);

        // 创建中父窗退出:取消创建,不阻塞。
        let mut c = controller();
        let key = effect_key(&c.open());
        assert_eq!(
            c.handle(DialogEvent::ParentClosed),
            vec![DialogEffect::Destroy {
                key,
                window: None,
                reason: DestroyReason::ParentClosed
            }]
        );
        assert_eq!(*c.phase(), Phase::Closed);
    }

    #[test]
    fn destroy_timeout_forces_closed() {
        let mut c = controller();
        let key = effect_key(&c.open());
        c.handle(DialogEvent::Opened { key, window: 5 });
        c.close();
        assert_eq!(
            c.destroy_timed_out(),
            vec![DialogEffect::Destroy {
                key,
                window: Some(5),
                reason: DestroyReason::Timeout
            }]
        );
        assert_eq!(*c.phase(), Phase::Closed);
        assert!(c.handle(DialogEvent::Closed { key, window: 5 }).is_empty());
    }

    #[test]
    fn reopen_during_closing_waits_for_destroy_then_starts_a_new_session() {
        let mut c = controller();
        let first = effect_key(&c.open());
        c.handle(DialogEvent::Opened {
            key: first,
            window: 5,
        });
        c.close();
        assert!(c.open().is_empty());
        assert_eq!(*c.phase(), Phase::Closing);

        let effects = c.handle(DialogEvent::Closed {
            key: first,
            window: 5,
        });
        let second = effect_key(&effects);
        assert!(second.session > first.session);
        assert_eq!(*c.phase(), Phase::Opening);
    }

    #[test]
    fn duplicate_opened_for_another_window_is_destroyed() {
        let mut c = controller();
        let key = effect_key(&c.open());
        c.handle(DialogEvent::Opened { key, window: 5 });
        assert_eq!(
            c.handle(DialogEvent::Opened { key, window: 6 }),
            vec![DialogEffect::Destroy {
                key,
                window: Some(6),
                reason: DestroyReason::Stale
            }]
        );
        assert_eq!(c.window(), Some(5));
    }

    #[test]
    fn default_geometry_matches_the_shared_spec() {
        let rect = DialogGeometry::default().card_rect(LogicalSize::new(1200.0, 800.0));
        // 40% * 1200 = 480 < 560 → 560;75% * 800 = 600。
        assert_eq!(rect.width, 560.0);
        assert_eq!(rect.height, 600.0);
        assert_eq!(rect.x, 320.0);
        assert_eq!(rect.y, 100.0);
    }

    #[test]
    fn large_parent_keeps_fractions() {
        let rect = DialogGeometry::default().card_rect(LogicalSize::new(2000.0, 1600.0));
        assert_eq!(rect.width, 800.0);
        assert_eq!(rect.height, 1200.0);
        assert_eq!(rect.x, 600.0);
        assert_eq!(rect.y, 200.0);
    }

    #[test]
    fn small_parent_clamps_card_inside_margins() {
        let rect = DialogGeometry::default().card_rect(LogicalSize::new(300.0, 200.0));
        assert_eq!(rect.width, 268.0);
        assert_eq!(rect.height, 168.0);
        assert_eq!(rect.x, 16.0);
        assert_eq!(rect.y, 16.0);
    }

    #[test]
    fn tiny_parent_never_produces_negative_or_zero_sizes() {
        for (w, h) in [(0.0, 0.0), (10.0, 10.0), (f32::NAN, f32::INFINITY)] {
            let rect = DialogGeometry::default().card_rect(LogicalSize::new(w, h));
            assert!(rect.width >= 1.0);
            assert!(rect.height >= 1.0);
            assert!(rect.x >= 0.0 && rect.y >= 0.0);
            assert!(rect.width <= w.max(1.0));
            assert!(rect.height <= h.max(1.0));
        }
    }

    #[test]
    fn invalid_geometry_configuration_is_sanitized() {
        let geometry = DialogGeometry {
            width_fraction: 2.0,
            height_fraction: -1.0,
            min_width: -10.0,
            min_height: f32::NAN,
            margin: f32::INFINITY,
        }
        .sanitized();
        assert_eq!(geometry.width_fraction, 1.0);
        assert_eq!(geometry.height_fraction, 0.0);
        assert_eq!(geometry.min_width, 0.0);
        assert_eq!(geometry.min_height, 0.0);
        assert_eq!(geometry.margin, 0.0);
    }
}
