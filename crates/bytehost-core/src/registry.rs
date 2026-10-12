//! 面板注册元数据与目录:与 GUI 无关的纯登记。
//!
//! 产品组合根提供每个面板的 `PanelId`、显示名、命名空间与它声明的 Surface 槽位;
//! Host 只据此建立目录(规格 §5.1)。**不含** iced view / 回调(那些在 desktop 层)。
//!
//! 重复 ID 在注册时**立即**报错;未知历史 ID 由布局迁移记录保留,不导致整个窗口打不开。

use std::collections::BTreeMap;
use std::fmt;

use crate::id::{PanelId, SurfaceSlot};

/// 一个面板的登记项(纯元数据)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelMeta {
    /// 稳定面板 id。
    pub id: PanelId,
    /// 用户可见显示名(产品提供;Host 不本地化)。
    pub title: String,
    /// 该面板声明的 Surface 槽位。**可为空**:Surface 是可选的 WebView 承载机制,
    /// 纯 iced 原生面板(如大纲、状态页、文件树)不承载 WebView,不声明任何槽位。
    pub surfaces: Vec<SurfaceSlot>,
}

impl PanelMeta {
    /// 单 Surface 面板的便捷构造。
    pub fn single_surface(id: PanelId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            surfaces: vec![SurfaceSlot::MAIN],
        }
    }
}

/// 注册目录:面板 id → 元数据。构造时拒绝重复 id。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelCatalog {
    entries: BTreeMap<PanelId, PanelMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// 重复 id。带上第一次注册的标题与本次冲突的标题,便于定位来源。
    DuplicateId {
        id: PanelId,
        first_title: String,
        second_title: String,
    },
    /// 同一个面板内声明了重复的 Surface 槽位。
    DuplicateSurface { id: PanelId, slot: SurfaceSlot },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId {
                id,
                first_title,
                second_title,
            } => write!(
                f,
                "面板 id 重复:{id}(先注册为 {first_title:?},又注册为 {second_title:?})"
            ),
            Self::DuplicateSurface { id, slot } => {
                write!(f, "面板 {id} 重复声明 Surface 槽位 {slot}")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

/// 返回第一个重复出现的槽位(若存在),用于注册时拒绝同一面板内重复声明。
fn first_duplicate(surfaces: &[SurfaceSlot]) -> Option<SurfaceSlot> {
    let mut seen = std::collections::BTreeSet::new();
    surfaces.iter().copied().find(|s| !seen.insert(*s))
}

impl PanelCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个面板。重复 id 或同一面板内重复的 Surface 槽位立即报错(不静默覆盖)。
    ///
    /// **允许零 Surface**:Surface 是可选的 WebView 承载机制,纯 iced 原生面板不声明槽位。
    /// 对已声明的槽位仍做重复检查。
    pub fn register(&mut self, meta: PanelMeta) -> Result<(), CatalogError> {
        if let Some(dup) = first_duplicate(&meta.surfaces) {
            return Err(CatalogError::DuplicateSurface {
                id: meta.id.clone(),
                slot: dup,
            });
        }
        if let Some(existing) = self.entries.get(&meta.id) {
            return Err(CatalogError::DuplicateId {
                id: meta.id.clone(),
                first_title: existing.title.clone(),
                second_title: meta.title.clone(),
            });
        }
        self.entries.insert(meta.id.clone(), meta);
        Ok(())
    }

    pub fn get(&self, id: &PanelId) -> Option<&PanelMeta> {
        self.entries.get(id)
    }

    pub fn contains(&self, id: &PanelId) -> bool {
        self.entries.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按 id 字典序枚举(稳定顺序,便于快照测试)。
    pub fn iter(&self) -> impl Iterator<Item = &PanelMeta> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    #[test]
    fn registering_distinct_ids_succeeds() {
        let mut c = PanelCatalog::new();
        c.register(PanelMeta::single_surface(id("shared.usage"), "Usage"))
            .unwrap();
        c.register(PanelMeta::single_surface(id("digger.outline"), "大纲"))
            .unwrap();
        assert_eq!(c.len(), 2);
        assert!(c.contains(&id("shared.usage")));
    }

    #[test]
    fn duplicate_id_is_rejected_without_overwriting() {
        let mut c = PanelCatalog::new();
        c.register(PanelMeta::single_surface(id("shared.usage"), "Usage"))
            .unwrap();
        let err = c
            .register(PanelMeta::single_surface(id("shared.usage"), "Usage2"))
            .unwrap_err();
        assert_eq!(
            err,
            CatalogError::DuplicateId {
                id: id("shared.usage"),
                first_title: "Usage".into(),
                second_title: "Usage2".into(),
            }
        );
        // 未被覆盖:仍是第一次的标题。
        assert_eq!(c.get(&id("shared.usage")).unwrap().title, "Usage");
    }

    #[test]
    fn panel_without_surfaces_is_allowed_for_native_panels() {
        // Surface 是可选的 WebView 承载机制;纯 iced 原生面板不声明任何槽位。
        let mut c = PanelCatalog::new();
        let meta = PanelMeta {
            id: id("shared.x"),
            title: "X".into(),
            surfaces: vec![],
        };
        c.register(meta).unwrap();
        assert_eq!(c.get(&id("shared.x")).unwrap().surfaces.len(), 0);
    }

    #[test]
    fn duplicate_surface_slot_within_a_panel_is_rejected() {
        let mut c = PanelCatalog::new();
        let meta = PanelMeta {
            id: id("shared.dup"),
            title: "Dup".into(),
            surfaces: vec![SurfaceSlot::MAIN, SurfaceSlot::new(1), SurfaceSlot::MAIN],
        };
        assert_eq!(
            c.register(meta),
            Err(CatalogError::DuplicateSurface {
                id: id("shared.dup"),
                slot: SurfaceSlot::MAIN,
            })
        );
    }

    #[test]
    fn native_panel_registers_alongside_surface_panels() {
        let mut c = PanelCatalog::new();
        c.register(PanelMeta::single_surface(id("shared.usage"), "Usage"))
            .unwrap();
        c.register(PanelMeta {
            id: id("digger.outline"),
            title: "大纲".into(),
            surfaces: vec![],
        })
        .unwrap();
        assert_eq!(c.len(), 2);
    }
}
