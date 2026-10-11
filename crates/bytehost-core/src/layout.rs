//! 布局 DTO:与 GUI 无关的窗口布局描述(rail / pane 的稳定 id 序列)。
//!
//! 序列化带**显式 schema version**;尺寸异常钳制到可见范围(规格 §5.2)。
//! 产品提供默认布局作为输入;未知历史 id 保留在迁移记录里,不导致窗口打不开。
//!
//! 本模块是纯数据与校验;真正的原子落盘(临时文件 + rename)在 desktop/组合根层调用,
//! 但校验与迁移规则(拒绝未知新版本、钳制尺寸、保留未知 id)在这里定义并被测试钉住。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::PanelId;
use crate::registry::PanelCatalog;

/// 当前支持的布局 schema 版本。读到**更高**版本时拒绝覆盖(防降级破坏)。
pub const LAYOUT_SCHEMA_VERSION: u32 = 1;

/// 窗口/面板尺寸的合法范围(逻辑点)。产品给的尺寸异常时钳到这里。
pub const MIN_PANE_EXTENT: f32 = 160.0;
pub const MAX_PANE_EXTENT: f32 = 4000.0;

/// 一条 rail(栏)的布局:若干面板 id 的顺序 + 该栏的宽度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rail {
    /// 栏内面板 id,**顺序即展示顺序**;允许为空(空栏合法)。
    pub items: Vec<PanelId>,
    /// 该栏的宽度(逻辑点)。读取时会被钳制到合法范围。
    pub width: f32,
}

/// 窗口布局。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub schema_version: u32,
    /// 栏的列表。允许为空(尚无面板),也允许含空栏。
    pub rails: Vec<Rail>,
    /// 迁移时发现、当前目录里不存在的历史面板 id(保留以便恢复/提示)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_unknown: Vec<PanelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// 读到比当前支持的更高的 schema 版本(拒绝覆盖,保留原文件)。
    UnknownSchemaVersion { found: u32, supported: u32 },
    /// 尺寸非法(非有限、负数等)。应被钳制而非报错的走 `clamp`,这里只报告无法钳制的输入。
    IllegalSize(String),
    /// JSON 无法解析。
    Corrupt(String),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSchemaVersion { found, supported } => write!(
                f,
                "布局 schema 版本 {found} 高于本宿主支持的 {supported},拒绝覆盖"
            ),
            Self::IllegalSize(m) => write!(f, "布局尺寸非法:{m}"),
            Self::Corrupt(m) => write!(f, "布局无法解析:{m}"),
        }
    }
}

impl std::error::Error for LayoutError {}

impl Rail {
    pub fn new(items: Vec<PanelId>, width: f32) -> Self {
        Self { items, width }
    }

    /// 宽度钳制到合法可见范围。`NaN`/无穷/负数按最小值处理。
    pub fn clamped_width(&self) -> f32 {
        if !self.width.is_finite() {
            return MIN_PANE_EXTENT;
        }
        self.width.clamp(MIN_PANE_EXTENT, MAX_PANE_EXTENT)
    }

    /// 校验宽度是否为可用的有限值(用于默认布局输入的即时检查)。
    pub fn validate_width(&self) -> Result<(), LayoutError> {
        if !self.width.is_finite() {
            return Err(LayoutError::IllegalSize(format!(
                "栏宽必须是有限数,收到 {}",
                self.width
            )));
        }
        Ok(())
    }
}

impl Layout {
    /// 从一个**产品提供的默认布局**构造(全新安装 / 首次运行)。
    pub fn from_default(rails: Vec<Rail>) -> Result<Self, LayoutError> {
        for rail in &rails {
            rail.validate_width()?;
        }
        Ok(Self {
            schema_version: LAYOUT_SCHEMA_VERSION,
            rails,
            retained_unknown: Vec::new(),
        })
    }

    /// 从磁盘 JSON 读入并迁移:
    /// - 解析失败 → `Corrupt`;
    /// - 版本高于支持 → `UnknownSchemaVersion`(调用方不得覆盖原文件);
    /// - 版本低于当前 → 按当前形状解释(此处只有 v1,无需迁移);
    /// - 栏宽钳制到合法范围;
    /// - `catalog` 中不存在的面板 id 移入 `retained_unknown`(不丢、不阻塞窗口)。
    pub fn load(json: &str, catalog: &PanelCatalog) -> Result<Self, LayoutError> {
        let mut layout: Layout =
            serde_json::from_str(json).map_err(|e| LayoutError::Corrupt(e.to_string()))?;
        if layout.schema_version > LAYOUT_SCHEMA_VERSION {
            return Err(LayoutError::UnknownSchemaVersion {
                found: layout.schema_version,
                supported: LAYOUT_SCHEMA_VERSION,
            });
        }
        layout.rails = layout
            .rails
            .into_iter()
            .map(|mut r| {
                r.width = r.clamped_width();
                r
            })
            .collect();
        // 未知 id 从栏里摘出,记进迁移记录。
        let mut unknown: Vec<PanelId> = layout.retained_unknown;
        for rail in &mut layout.rails {
            let mut kept = Vec::with_capacity(rail.items.len());
            for id in rail.items.drain(..) {
                if catalog.contains(&id) {
                    kept.push(id);
                } else if !unknown.contains(&id) {
                    unknown.push(id);
                }
            }
            rail.items = kept;
        }
        layout.retained_unknown = unknown;
        layout.schema_version = LAYOUT_SCHEMA_VERSION;
        Ok(layout)
    }

    /// 序列化为 JSON(desktop/组合根负责原子落盘)。
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("布局可序列化")
    }

    /// 当前布局里实际出现、且仍在目录中的面板数(空栏或空布局返回 0,合法)。
    pub fn visible_panel_count(&self) -> usize {
        self.rails.iter().map(|r| r.items.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::PanelMeta;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    fn catalog() -> PanelCatalog {
        let mut c = PanelCatalog::new();
        c.register(PanelMeta::single_surface(id("shared.usage"), "Usage"))
            .unwrap();
        c.register(PanelMeta::single_surface(id("digger.outline"), "大纲"))
            .unwrap();
        c
    }

    #[test]
    fn empty_and_single_panel_rails_are_legal() {
        let c = catalog();
        // 无面板:空布局。
        let empty = Layout::from_default(vec![]).unwrap();
        assert_eq!(empty.visible_panel_count(), 0);
        // 空栏。
        let with_empty_rail = Layout::from_default(vec![Rail::new(vec![], 300.0)]).unwrap();
        assert_eq!(with_empty_rail.visible_panel_count(), 0);
        // 单面板。
        let single =
            Layout::from_default(vec![Rail::new(vec![id("shared.usage")], 300.0)]).unwrap();
        assert_eq!(single.visible_panel_count(), 1);
        // 都能被目录 load 回来。
        for l in [empty, with_empty_rail, single] {
            let json = l.to_json();
            let back = Layout::load(&json, &c).unwrap();
            assert_eq!(back.visible_panel_count(), l.visible_panel_count());
        }
    }

    #[test]
    fn unknown_historical_ids_are_retained_not_dropped() {
        let c = catalog();
        let json = r#"{
            "schema_version": 1,
            "rails": [
                { "items": ["shared.usage", "gone.legacy"], "width": 320.0 }
            ]
        }"#;
        let l = Layout::load(json, &c).unwrap();
        // 已知的留下,未知的被摘出来保留。
        assert_eq!(l.rails[0].items, vec![id("shared.usage")]);
        assert_eq!(l.retained_unknown, vec![id("gone.legacy")]);
        // 保留项能再序列化回去(不丢失)。
        let json2 = l.to_json();
        assert!(json2.contains("gone.legacy"));
    }

    #[test]
    fn unknown_newer_schema_is_rejected() {
        let c = catalog();
        let json = r#"{ "schema_version": 99, "rails": [] }"#;
        assert_eq!(
            Layout::load(json, &c),
            Err(LayoutError::UnknownSchemaVersion {
                found: 99,
                supported: LAYOUT_SCHEMA_VERSION
            })
        );
    }

    #[test]
    fn corrupt_json_is_a_structured_error() {
        let c = catalog();
        assert!(matches!(
            Layout::load("{ not json", &c),
            Err(LayoutError::Corrupt(_))
        ));
    }

    #[test]
    fn widths_are_clamped_and_illegal_finite_inputs_rejected() {
        assert_eq!(Rail::new(vec![], 10.0).clamped_width(), MIN_PANE_EXTENT);
        assert_eq!(Rail::new(vec![], 99999.0).clamped_width(), MAX_PANE_EXTENT);
        assert_eq!(Rail::new(vec![], 300.0).clamped_width(), 300.0);
        assert_eq!(Rail::new(vec![], f32::NAN).clamped_width(), MIN_PANE_EXTENT);
        assert!(matches!(
            Layout::from_default(vec![Rail::new(vec![], f32::NAN)]),
            Err(LayoutError::IllegalSize(_))
        ));
    }

    #[test]
    fn load_clamps_out_of_range_width_on_read() {
        let c = catalog();
        let json = r#"{ "schema_version": 1, "rails": [{ "items": [], "width": 1.0 }] }"#;
        let l = Layout::load(json, &c).unwrap();
        assert_eq!(l.rails[0].width, MIN_PANE_EXTENT);
    }
}
