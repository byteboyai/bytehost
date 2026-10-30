//! 设置窗口的导航状态:打开 / 切页 / 关闭与窗口代数。
//!
//! 产品拥有窗口本身;本状态只记录"是否打开、当前显示哪页、这是第几次新开"。
//! 重复打开只激活现有窗口,不递增代数——窗口局部校验 / 草稿任务用它丢弃过时结果。

use crate::page::PageId;
use crate::registry::PageRegistry;

/// 共享设置容器状态。
///
/// 不持久化;每次冷启动都是关闭态,重开默认落在主题页或显式目标页。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsState {
    open: bool,
    current: Option<PageId>,
    window_generation: u64,
}

impl SettingsState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn current(&self) -> Option<&PageId> {
        self.current.as_ref()
    }

    /// 当前窗口代数;首次打开后为 1。
    pub fn window_generation(&self) -> u64 {
        self.window_generation
    }

    /// 打开设置窗口并选中目标页。
    ///
    /// - 目标页不存在时回退:主题页 → 首个注册页 → 空状态。
    /// - 已经打开时只切页,不递增 [`Self::window_generation`]。
    /// - 注册表为空时仍可打开,`current` 为 `None`,由 UI 展示空状态。
    ///
    /// 返回 `true` 表示这是**新开**窗口(调用方应创建 / 激活原生窗口)。
    pub fn open(&mut self, registry: &PageRegistry, target: Option<&PageId>) -> bool {
        let newly_opened = !self.open;
        self.open = true;
        self.current = registry.resolve(target).cloned();
        if newly_opened {
            self.window_generation = self.window_generation.saturating_add(1).max(1);
        }
        newly_opened
    }

    /// 按 ID 打开。目标不可用时回退,不会停留在已知无效页。
    pub fn open_page(&mut self, registry: &PageRegistry, id: &PageId) -> bool {
        self.open(registry, Some(id))
    }

    /// 关闭设置窗口;返回原本是否打开。
    pub fn close(&mut self) -> bool {
        let was_open = self.open;
        self.open = false;
        self.current = None;
        was_open
    }

    /// 切页。未知页或窗口未打开时返回 `false`,状态不变。
    pub fn select(&mut self, registry: &PageRegistry, id: &PageId) -> bool {
        if !self.open || !registry.contains(id) {
            return false;
        }
        if self.current.as_ref() == Some(id) {
            return false;
        }
        self.current = Some(id.clone());
        true
    }

    /// 注册集合变化后重新校正当前页:当前页消失则按同一回退规则选择。
    pub fn sync_registry(&mut self, registry: &PageRegistry) {
        if !self.open {
            return;
        }
        let current_is_valid = self
            .current
            .as_ref()
            .is_some_and(|id| registry.contains(id));
        if !current_is_valid {
            self.current = registry.default_page().cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PageDescriptor, PageId};

    fn custom(raw: &str) -> PageDescriptor {
        PageDescriptor::custom(PageId::new(raw).unwrap(), format!("page.{raw}"), raw)
    }

    fn builtin(raw: &str) -> PageDescriptor {
        PageDescriptor::built_in(PageId::new(raw).unwrap(), raw)
    }

    fn registry() -> PageRegistry {
        PageRegistry::new(vec![
            builtin(crate::THEME_ID),
            custom("dozer.git"),
            custom("dozer.apps"),
        ])
        .unwrap()
    }

    #[test]
    fn repeated_open_does_not_bump_the_generation() {
        let registry = registry();
        let mut state = SettingsState::new();
        assert!(state.open(&registry, None));
        assert_eq!(state.window_generation(), 1);
        assert!(!state.open(&registry, None));
        assert_eq!(state.window_generation(), 1);
        assert!(state.close());
        assert!(!state.close());
        assert!(state.open(&registry, None));
        assert_eq!(state.window_generation(), 2);
    }

    #[test]
    fn unknown_target_falls_back_to_theme_then_first() {
        let registry = registry();
        let mut state = SettingsState::new();
        state.open(&registry, Some(&PageId::new("missing.page").unwrap()));
        assert_eq!(state.current().unwrap().as_str(), crate::THEME_ID);

        let git = PageId::new("dozer.git").unwrap();
        state.open(&registry, Some(&git));
        assert_eq!(state.current(), Some(&git));

        let no_theme = PageRegistry::new(vec![custom("dozer.git")]).unwrap();
        state.close();
        state.open(&no_theme, Some(&PageId::new("missing.page").unwrap()));
        assert_eq!(state.current().unwrap().as_str(), "dozer.git");
    }

    #[test]
    fn empty_registry_opens_with_an_empty_selection() {
        let empty = PageRegistry::empty();
        let mut state = SettingsState::new();
        assert!(state.open(&empty, None));
        assert!(state.is_open());
        assert_eq!(state.current(), None);
    }

    #[test]
    fn select_rejects_unknown_pages_and_no_ops_on_the_current_page() {
        let registry = registry();
        let mut state = SettingsState::new();
        state.open(&registry, None);
        assert!(!state.select(&registry, &PageId::new("nope.page").unwrap()));
        assert!(!state.select(&registry, &PageId::new(crate::THEME_ID).unwrap()));
        assert!(state.select(&registry, &PageId::new("dozer.git").unwrap()));
        assert_eq!(state.current().unwrap().as_str(), "dozer.git");
    }

    #[test]
    fn removing_the_selected_page_falls_back_without_closing() {
        let registry = registry();
        let mut state = SettingsState::new();
        state.open(&registry, Some(&PageId::new("dozer.git").unwrap()));

        let trimmed =
            PageRegistry::new(vec![builtin(crate::THEME_ID), custom("dozer.apps")]).unwrap();
        state.sync_registry(&trimmed);
        assert!(state.is_open());
        assert_eq!(state.current().unwrap().as_str(), crate::THEME_ID);

        state.sync_registry(&PageRegistry::empty());
        assert!(state.is_open());
        assert_eq!(state.current(), None);
    }

    #[test]
    fn closed_state_ignores_registry_updates_and_selection() {
        let registry = registry();
        let mut state = SettingsState::new();
        assert!(!state.select(&registry, &PageId::new(crate::THEME_ID).unwrap()));
        state.sync_registry(&registry);
        assert_eq!(state.current(), None);
    }
}
