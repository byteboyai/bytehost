//! 产品中立的 rail/pane 容器数据:`PanelId` 键控的 rail 顺序 + 每面板分栏状态。
//!
//! 本模块只承载**与 GUI 无关的状态与几何换算**:某条 rail 上有哪些面板(顺序)、
//! 每个配对面板的内部分栏比例与列表列收起态、以及"把分栏拖到某个 x 之后的新
//! 状态"。产品提供默认值、图标/标题/业务排序与命令路由;宿主不理解任何具体
//! 面板含义。键是 `bytehost_core::id::PanelId`,与 `Layout`/`Rail` DTO 共用同一
//! 份稳定身份(规格 §5.1)。
//!
//! 迁移自 Dozer 的 `RailLayout` + `PanelDims`(后者原来是逐面板具名字段 + 巨型
//! `match`);这里一般化成 `PanelId → PaneDims` 的映射,产品侧只保留业务枚举到
//! `PanelId` 的映射与默认布局(见兼容桥 C11)。

use std::collections::BTreeMap;

use bytehost_core::id::PanelId;
use serde::{Deserialize, Serialize};

/// 单个配对面板的内部分栏状态。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PaneDims {
    /// pair 内第一个 slot 的宽度占比(0..1)。语义"占第一个 slot"——不区分它
    /// 业务上叫"列表"还是"内容",由产品注解决定默认朝向。
    pub split: f32,
    /// pair 内第一个 slot 是否被收起(仅对称收起的 pair 有意义;无收起能力的
    /// pair 恒 `false`)。
    pub collapsed: bool,
}

impl PaneDims {
    pub const fn new(split: f32, collapsed: bool) -> Self {
        Self { split, collapsed }
    }
}

/// 一个面板的配对几何描述(产品提供,只读):它的两个 slot 是否可收起、默认
/// 哪个 slot 在前、以及折叠阈值。宿主据此跑通用拖拽换算,不解释业务。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneSpec {
    /// 默认(未镜像)态下"第一个 slot 是否渲染在前"。
    pub default_first_first: bool,
    /// 该 pair 是否具备"把第一个 slot 拖窄到阈值即收起"的能力。
    pub collapsible: bool,
}

impl PaneSpec {
    pub const fn new(default_first_first: bool, collapsible: bool) -> Self {
        Self {
            default_first_first,
            collapsible,
        }
    }
}

/// `PanelId` 键控的每面板分栏状态存储。产品组合根给出默认值,宿主负责读写与
/// 拖拽换算。序列化为 `BTreeMap`(稳定顺序,便于落盘 diff 与测试)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneStore {
    dims: BTreeMap<PanelId, PaneDims>,
}

impl PaneStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取某面板的分栏状态;不存在返回 `None`。
    pub fn get(&self, id: &PanelId) -> Option<PaneDims> {
        self.dims.get(id).copied()
    }

    /// 取某面板的分栏状态,缺失时用 `default` 补上并返回(不改动存储)。
    pub fn get_or(&self, id: &PanelId, default: PaneDims) -> PaneDims {
        self.dims.get(id).copied().unwrap_or(default)
    }

    /// 确保某面板存在条目(缺失则插入 `default`)。
    pub fn ensure(&mut self, id: &PanelId, default: PaneDims) {
        self.dims.entry(id.clone()).or_insert(default);
    }

    /// 写入某面板的整个分栏状态。
    pub fn set(&mut self, id: PanelId, dims: PaneDims) {
        self.dims.insert(id, dims);
    }

    /// 仅更新分栏比例,保留收起态;条目不存在则用 `default_split` 作底。
    pub fn set_split(&mut self, id: &PanelId, split: f32, default_split: f32) {
        let mut d = self.get_or(id, PaneDims::new(default_split, false));
        d.split = split;
        self.dims.insert(id.clone(), d);
    }

    /// 仅更新收起态,保留分栏比例。
    pub fn set_collapsed(&mut self, id: &PanelId, collapsed: bool, default_split: f32) {
        let mut d = self.get_or(id, PaneDims::new(default_split, false));
        d.collapsed = collapsed;
        self.dims.insert(id.clone(), d);
    }

    pub fn len(&self) -> usize {
        self.dims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dims.is_empty()
    }
}

/// `apply_pane_split_drag` 的结果:新的分栏状态(比例 + 收起态)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneDragOutcome {
    pub split: f32,
    pub collapsed: bool,
}

/// `apply_pane_split_drag` 的几何输入(具名字段,避免 9 个位置参数里多个同类型
/// `f32` 相邻易传错——见仓库"参数结构体"约定)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneDragInput {
    /// pair 内容区左边界(窗口逻辑 x)。
    pub x0: f32,
    /// pair 可分配宽度。
    pub pair_w: f32,
    /// 拖到的窗口逻辑 x。
    pub logical_x: f32,
    /// 合法比例区间下限/上限。
    pub min_ratio: f32,
    pub max_ratio: f32,
    /// 收起判据:第一个 slot 宽度小于它即收起。
    pub min_first_px: f32,
}

/// 产品中立的"配对分栏拖拽"换算——迁移自 Dozer `apply_pair_split_drag` 的核心
/// 算术。给定几何输入 `input`、当前状态 `current`、该 pair 的 `spec`、以及当前
/// 是否镜像,算出新的分栏状态。
///
/// `spec.collapsible` 为真时:拖窄到第一个 slot 宽度小于 `input.min_first_px`
/// 直接收起(冻结 `current` 里上次仍够宽的比例);拖回超过阈值即展开并连续写
/// 比例,与 Dozer 原实现对称、可逆(拖拽逐帧调用,不是一次性判定)。不可收起时
/// 只写比例。
pub fn apply_pane_split_drag(
    current: PaneDims,
    spec: PaneSpec,
    mirrored: bool,
    input: PaneDragInput,
) -> PaneDragOutcome {
    let pair_w = input.pair_w.max(0.0);
    if pair_w <= 0.0 {
        return PaneDragOutcome {
            split: current.split,
            collapsed: current.collapsed,
        };
    }
    let raw_ratio = ((input.logical_x - input.x0) / pair_w).clamp(input.min_ratio, input.max_ratio);
    // `ratio` 恒是"pair 内第一个 slot 的宽度占比"。只有第一个 slot 现在确实
    // 渲染在前时,鼠标左侧占比才直接等于它;镜像时取反。
    let first_rendered_first = spec.default_first_first != mirrored;
    let ratio = if first_rendered_first {
        raw_ratio
    } else {
        1.0 - raw_ratio
    };
    if !spec.collapsible {
        return PaneDragOutcome {
            split: ratio,
            collapsed: false,
        };
    }
    let first_w = pair_w * ratio;
    if first_w < input.min_first_px {
        return PaneDragOutcome {
            // 冻结上次仍够宽的比例,不被拖成挤爆按钮的小数值。
            split: current.split,
            collapsed: true,
        };
    }
    PaneDragOutcome {
        split: ratio,
        collapsed: false,
    }
}

/// 一条 rail 的稳定内容:`PanelId` 顺序即展示顺序。
pub type RailItems = Vec<PanelId>;

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    #[test]
    fn store_reads_writes_and_defaults() {
        let mut store = PaneStore::new();
        let a = id("p.a");
        assert_eq!(store.get(&a), None);
        assert_eq!(store.get_or(&a, PaneDims::new(0.4, false)).split, 0.4);
        store.ensure(&a, PaneDims::new(0.4, false));
        assert_eq!(store.get(&a), Some(PaneDims::new(0.4, false)));
        store.set_split(&a, 0.7, 0.5);
        assert_eq!(store.get(&a), Some(PaneDims::new(0.7, false)));
        store.set_collapsed(&a, true, 0.5);
        assert_eq!(store.get(&a), Some(PaneDims::new(0.7, true)));
    }

    #[test]
    fn store_round_trips_as_stable_map() {
        let mut store = PaneStore::new();
        store.set(id("z.panel"), PaneDims::new(0.3, false));
        store.set(id("a.panel"), PaneDims::new(0.6, true));
        let json = serde_json::to_string(&store).unwrap();
        // BTreeMap:按 id 排序,输出稳定。
        assert!(json.find("a.panel").unwrap() < json.find("z.panel").unwrap());
        let back: PaneStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);
    }

    #[test]
    fn drag_writes_ratio_for_non_mirrored_list_first() {
        let spec = PaneSpec::new(true, true);
        let out = apply_pane_split_drag(
            PaneDims::new(0.5, false),
            spec,
            false,
            PaneDragInput {
                x0: 100.0,
                pair_w: 600.0,
                logical_x: 340.0,
                min_ratio: 0.1,
                max_ratio: 0.9,
                min_first_px: 100.0,
            },
        );
        // (340-100)/600 = 0.4,列表在前 → 直接用。
        assert!((out.split - 0.4).abs() < 1e-4);
        assert!(!out.collapsed);
    }

    #[test]
    fn drag_mirrors_ratio_when_content_first() {
        let spec = PaneSpec::new(true, true);
        let out = apply_pane_split_drag(
            PaneDims::new(0.5, false),
            spec,
            true,
            PaneDragInput {
                x0: 100.0,
                pair_w: 600.0,
                logical_x: 340.0,
                min_ratio: 0.1,
                max_ratio: 0.9,
                min_first_px: 100.0,
            },
        );
        // 镜像 → 1 - 0.4 = 0.6。
        assert!((out.split - 0.6).abs() < 1e-4);
    }

    #[test]
    fn drag_collapses_below_threshold_and_freezes_ratio() {
        let spec = PaneSpec::new(true, true);
        let out = apply_pane_split_drag(
            PaneDims::new(0.5, false),
            spec,
            false,
            PaneDragInput {
                x0: 0.0,
                pair_w: 600.0,
                logical_x: 30.0,
                min_ratio: 0.01,
                max_ratio: 0.9,
                min_first_px: 100.0,
            },
        );
        assert!(out.collapsed);
        // 冻结上次比例,不写 0.05。
        assert!((out.split - 0.5).abs() < 1e-4);
    }

    #[test]
    fn drag_expands_back_above_threshold() {
        let spec = PaneSpec::new(true, true);
        let out = apply_pane_split_drag(
            PaneDims::new(0.5, true),
            spec,
            false,
            PaneDragInput {
                x0: 0.0,
                pair_w: 600.0,
                logical_x: 240.0,
                min_ratio: 0.01,
                max_ratio: 0.9,
                min_first_px: 100.0,
            },
        );
        assert!(!out.collapsed);
        assert!((out.split - 0.4).abs() < 1e-4);
    }

    #[test]
    fn non_collapsible_never_collapses() {
        // default_first_first=false → 第一个 slot(此处是"内容")默认在后;
        // 拖到很靠左 → 该 slot 占据余下的大部分,写 1 - ratio。
        let spec = PaneSpec::new(false, false);
        let out = apply_pane_split_drag(
            PaneDims::new(0.5, false),
            spec,
            false,
            PaneDragInput {
                x0: 0.0,
                pair_w: 600.0,
                logical_x: 10.0,
                min_ratio: 0.01,
                max_ratio: 0.9,
                min_first_px: 100.0,
            },
        );
        assert!(!out.collapsed);
        assert!(out.split > 0.98);
    }

    #[test]
    fn zero_width_pair_is_noop() {
        let spec = PaneSpec::new(true, true);
        let cur = PaneDims::new(0.5, true);
        let out = apply_pane_split_drag(
            cur,
            spec,
            false,
            PaneDragInput {
                x0: 0.0,
                pair_w: 0.0,
                logical_x: 50.0,
                min_ratio: 0.1,
                max_ratio: 0.9,
                min_first_px: 100.0,
            },
        );
        assert_eq!(out.split, cur.split);
        assert_eq!(out.collapsed, cur.collapsed);
    }
}
