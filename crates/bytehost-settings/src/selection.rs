//! 通用"选中 → 保存 → 确认 / 失败 / 冲突"模型。
//!
//! 仅承载 UI 需要的瞬时状态:已确认值、待保存值、保存结果。revision、服务端快照与
//! 冲突判断由产品适配层负责;本层只按产品回灌的结果迁移。

/// 一次保存的状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SaveStatus {
    /// 空闲:显示已确认值。
    #[default]
    Idle,
    /// 已提交,等待产品回包;此间禁止重复提交。
    Saving,
    /// 保存失败,保留待选值供重试。
    Failed(String),
    /// 服务端值已更新(别处改过),已采纳服务端值,提示重选。
    Conflict,
    /// 当前服务不支持写入。
    Unsupported,
}

/// 泛型选择模型。`T` 是产品自己的偏好类型(例如 `LocalePreference`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionState<T> {
    confirmed: T,
    pending: Option<T>,
    status: SaveStatus,
}

impl<T> SelectionState<T> {
    pub fn new(confirmed: T) -> Self {
        Self {
            confirmed,
            pending: None,
            status: SaveStatus::Idle,
        }
    }

    pub fn confirmed(&self) -> &T {
        &self.confirmed
    }

    pub fn pending(&self) -> Option<&T> {
        self.pending.as_ref()
    }

    pub fn status(&self) -> &SaveStatus {
        &self.status
    }

    pub fn is_saving(&self) -> bool {
        self.status == SaveStatus::Saving
    }

    pub fn is_unsupported(&self) -> bool {
        self.status == SaveStatus::Unsupported
    }

    /// 当前是否允许发起新选择。保存中或服务不支持时禁止。
    pub fn can_select(&self) -> bool {
        !matches!(self.status, SaveStatus::Saving | SaveStatus::Unsupported)
    }
}

impl<T: Clone> SelectionState<T> {
    /// 当前应显示为"已选"的值:有乐观值用乐观值,否则用已确认值。
    pub fn shown(&self) -> &T {
        self.pending.as_ref().unwrap_or(&self.confirmed)
    }

    /// 采纳用户新选择:进入 `Saving`。保存中 / 不支持时忽略并返回 `false`。
    pub fn select(&mut self, value: T) -> bool {
        if !self.can_select() {
            return false;
        }
        self.pending = Some(value);
        self.status = SaveStatus::Saving;
        true
    }

    /// 产品确认保存成功(携带落盘后的权威值):清 pending,更新 confirmed。
    pub fn applied(&mut self, confirmed: T) {
        self.confirmed = confirmed;
        self.pending = None;
        self.status = SaveStatus::Idle;
    }

    /// 保存失败:保留 pending 供重试。
    pub fn failed(&mut self, reason: impl Into<String>) {
        self.status = SaveStatus::Failed(reason.into());
    }

    /// 服务端 revision 冲突:采纳服务端值,清除待选值。
    pub fn conflict(&mut self, server: T) {
        self.confirmed = server;
        self.pending = None;
        self.status = SaveStatus::Conflict;
    }

    /// 服务不支持:禁止继续提交,清除待选值。
    pub fn unsupported(&mut self) {
        self.pending = None;
        self.status = SaveStatus::Unsupported;
    }

    /// 外部权威值变化(例如重连后广播):只更新已确认值,不动 pending / 状态。
    pub fn update_confirmed(&mut self, confirmed: T) {
        self.confirmed = confirmed;
    }

    /// 清除瞬时结果并回到空闲(例如用户关窗再开)。
    pub fn reset_status(&mut self) {
        self.status = SaveStatus::Idle;
    }
}

impl<T: Default> Default for SelectionState<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_enters_saving_and_ignores_repeats() {
        let mut state = SelectionState::new("en");
        assert!(state.select("zh"));
        assert_eq!(state.pending(), Some(&"zh"));
        assert_eq!(state.shown(), &"zh");
        assert!(!state.select("fr"));
        assert_eq!(state.pending(), Some(&"zh"));
    }

    #[test]
    fn applied_clears_pending_and_updates_confirmed() {
        let mut state = SelectionState::new("en");
        state.select("zh");
        state.applied("zh");
        assert_eq!(state.confirmed(), &"zh");
        assert_eq!(state.pending(), None);
        assert_eq!(state.status(), &SaveStatus::Idle);
    }

    #[test]
    fn failure_keeps_pending_for_retry() {
        let mut state = SelectionState::new("en");
        state.select("zh");
        state.failed("io");
        assert_eq!(state.pending(), Some(&"zh"));
        assert!(matches!(state.status(), SaveStatus::Failed(_)));
        assert!(state.can_select());
        assert!(state.select("en"));
    }

    #[test]
    fn conflict_adopts_the_server_value_and_clears_pending() {
        let mut state = SelectionState::new("en");
        state.select("zh");
        state.conflict("fr");
        assert_eq!(state.confirmed(), &"fr");
        assert_eq!(state.pending(), None);
        assert_eq!(state.status(), &SaveStatus::Conflict);
    }

    #[test]
    fn unsupported_blocks_further_selection() {
        let mut state = SelectionState::new("en");
        state.select("zh");
        state.unsupported();
        assert_eq!(state.pending(), None);
        assert!(state.is_unsupported());
        assert!(!state.can_select());
        assert!(!state.select("fr"));
        assert_eq!(state.confirmed(), &"en");
    }

    #[test]
    fn external_confirmation_does_not_touch_pending_or_status() {
        let mut state = SelectionState::new("en");
        state.select("zh");
        state.update_confirmed("en");
        assert_eq!(state.confirmed(), &"en");
        assert_eq!(state.pending(), Some(&"zh"));
        assert!(state.is_saving());
    }
}
