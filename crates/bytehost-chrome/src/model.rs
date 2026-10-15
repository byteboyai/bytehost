//! 骨架状态机:无界面、以 `PanelId` 为键。产品把 `ShellMsg` 交给 `update`,`update` 只改状态并
//! 告诉调用方"要不要落盘 / 哪个面板刚被切入";文件读写与面板切入动作由产品决定。

use bytehost_core::id::PanelId;
use bytehost_desktop::{PaneDims, PaneDragInput, PaneSpec, PaneStore, apply_pane_split_drag};

use std::collections::HashMap;

use crate::anim::{HoverAnim, SlotAnim};
use crate::geometry::{self, Rect, ShellRects, Side, ZoneState};
use crate::metrics::ChromeMetrics;
use crate::rail::{self, RailDrag, Rails};

/// 产品对一个面板的静态描述(骨架不理解面板含义,只按这几条规则摆放)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelRules {
    /// 默认挂在哪条栏;被拖到另一栏时配对视图左右镜像。
    pub default_side: Side,
    /// 有"列表 | 内容"配对的面板才有。
    pub pane: Option<PaneRules>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRules {
    pub spec: PaneSpec,
    pub default_split: f32,
    /// 第一个 slot 窄于它就收起(例如列表底部按钮行放得下的最小宽度)。
    pub min_first_px: f32,
}

/// `update` 需要的只读环境。
pub struct Ctx<'a> {
    pub metrics: &'a ChromeMetrics,
    pub rules: &'a dyn Fn(&PanelId) -> Option<PanelRules>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    /// 左右两个面板区之间那条恒在的分隔线。
    Zones,
    /// 某一侧当前面板的"列表 | 内容"分隔线。
    Pair(Side),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShellMsg {
    /// 在图标栏某图标上按下:选中 / 切换收起,并武装拖拽(拖拽起点取模型记录的光标位置)。
    PanelPress { panel: PanelId },
    /// 图标悬停进出(驱动颜色渐变)。
    Hover { panel: PanelId, hovered: bool },
    /// 动画推进一拍(~60fps,仅在 `animating()` 时需要)。
    Tick,
    /// 什么都不做(控件需要一个"吞掉点击"的消息时用)。
    Noop,
    /// 光标悬停到某栏第 `index` 个图标(含"末尾命中区",`index == len`)上,拖拽期间才有意义。
    RailDragMove { side: Side, index: usize },
    /// 光标移动(窗口逻辑坐标):记录光标位置,并驱动正在拖的分隔线。
    CursorMoved { x: f32, y: f32 },
    /// 松开鼠标:结束图标栏拖拽与分隔线拖拽。
    PointerReleased,
    DividerPress(Divider),
    /// 程序化"确保这个面板看得见"(绝不收起)。
    ShowPanel(PanelId),
    Maximize(Side),
    MaximizeClose,
    /// 切换配对面板"第一个 slot 收起"(列表的收起 / 展开按钮)。面板没有配对规格时什么都不做。
    TogglePane(PanelId),
    /// 左键按下:按模型记录的光标位置决定聚焦哪一侧。
    ZoneClicked,
    Resized { w: f32, h: f32 },
}

/// `update` 的结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// 持久状态变了,调用方应落盘。
    pub persist: bool,
    /// 这个面板刚被"真切换"进来(此前该侧显示的是别的面板),产品可据此触发切入动作。
    pub switched_in: Option<PanelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    EmptyRail(Side),
    Duplicate(PanelId),
    SelectedNotInRail(Side),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRail(s) => write!(f, "{s:?} 栏不能为空"),
            Self::Duplicate(p) => write!(f, "面板 {p} 在两条栏里重复出现"),
            Self::SelectedNotInRail(s) => write!(f, "{s:?} 侧选中的面板不在该栏里"),
        }
    }
}

impl std::error::Error for ModelError {}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellModel {
    rails: Rails,
    selected: [PanelId; 2],
    collapsed: [bool; 2],
    left_width: f32,
    window: (f32, f32),
    panes: PaneStore,
    // 瞬态,不落盘
    maximized: Option<Side>,
    active_zone: Option<Side>,
    rail_drag: Option<RailDrag>,
    divider_drag: Option<Divider>,
    cursor: (f32, f32),
    hover: HashMap<PanelId, HoverAnim>,
    slots: HashMap<PanelId, SlotAnim>,
}

impl ShellModel {
    pub fn new(rails: Rails, selected: [PanelId; 2], left_width: f32, window: (f32, f32)) -> Result<Self, ModelError> {
        for side in [Side::Left, Side::Right] {
            let items = &rails[side.index()];
            if items.is_empty() {
                return Err(ModelError::EmptyRail(side));
            }
            if !items.contains(&selected[side.index()]) {
                return Err(ModelError::SelectedNotInRail(side));
            }
        }
        for p in &rails[0] {
            if rails[1].contains(p) {
                return Err(ModelError::Duplicate(p.clone()));
            }
        }
        for side in [0, 1] {
            let mut seen = std::collections::HashSet::new();
            for p in &rails[side] {
                if !seen.insert(p) {
                    return Err(ModelError::Duplicate(p.clone()));
                }
            }
        }
        Ok(Self {
            rails,
            selected,
            collapsed: [false, false],
            left_width,
            window,
            panes: PaneStore::new(),
            maximized: None,
            active_zone: None,
            rail_drag: None,
            divider_drag: None,
            cursor: (0.0, 0.0),
            hover: HashMap::new(),
            slots: HashMap::new(),
        })
    }

    // ---- 读 ----
    pub fn rails(&self) -> &Rails {
        &self.rails
    }
    pub fn rail(&self, side: Side) -> &[PanelId] {
        &self.rails[side.index()]
    }
    pub fn selected(&self, side: Side) -> &PanelId {
        &self.selected[side.index()]
    }
    pub fn collapsed(&self, side: Side) -> bool {
        self.collapsed[side.index()]
    }
    pub fn left_width(&self) -> f32 {
        self.left_width
    }
    pub fn window(&self) -> (f32, f32) {
        self.window
    }
    pub fn panes(&self) -> &PaneStore {
        &self.panes
    }
    pub fn maximized(&self) -> Option<Side> {
        self.maximized
    }
    pub fn active_zone(&self) -> Option<Side> {
        self.active_zone
    }
    pub fn rail_drag(&self) -> Option<&RailDrag> {
        self.rail_drag.as_ref()
    }
    pub fn dragging_divider(&self) -> Option<Divider> {
        self.divider_drag
    }
    pub fn cursor(&self) -> (f32, f32) {
        self.cursor
    }
    /// 被拖拽的图标已越过阈值(不是单击)。
    pub fn rail_drag_confirmed(&self) -> bool {
        self.rail_drag.as_ref().is_some_and(|d| rail::past_threshold(d, self.cursor))
    }
    pub fn dragged_panel(&self) -> Option<PanelId> {
        rail::dragged_panel(&self.rails, self.rail_drag.as_ref())
    }
    pub fn side_of(&self, panel: &PanelId) -> Option<Side> {
        rail::side_of(&self.rails, panel)
    }

    /// 图标悬停进度(0..=1),给视图做颜色插值。
    pub fn hover_progress(&self, panel: &PanelId) -> f32 {
        self.hover.get(panel).map(HoverAnim::t).unwrap_or(0.0)
    }

    /// `panel` 在 `side` 栏此刻应渲染的浮点槽位号(逼近中的下标);没有动画记录或刚跨栏时就是目标下标本身。
    pub fn slot_position(&self, side: Side, panel: &PanelId, target_index: usize) -> f32 {
        self.slots.get(panel).and_then(|a| a.current(side)).unwrap_or(target_index as f32)
    }

    /// 还有动画在进行,产品需要继续排 `Tick`。
    pub fn animating(&self) -> bool {
        self.hover.values().any(HoverAnim::active)
            || [Side::Left, Side::Right].into_iter().any(|side| {
                self.rails[side.index()].iter().enumerate().any(|(i, p)| self.slots.get(p).is_some_and(|a| a.active(side, i as f32)))
            })
    }

    pub fn zone_state(&self) -> ZoneState {
        ZoneState {
            left_collapsed: self.collapsed[0],
            right_collapsed: self.collapsed[1],
            left_width: self.left_width,
            maximized: self.maximized,
        }
    }

    pub fn rects(&self, m: &ChromeMetrics) -> ShellRects {
        geometry::layout(self.window.0, self.window.1, &self.zone_state(), m)
    }

    /// 左面板区外框当前实际宽度(已夹取)。
    pub fn effective_left_width(&self, m: &ChromeMetrics) -> f32 {
        geometry::left_zone_width(self.window.0, &self.zone_state(), m)
    }

    // ---- 持久状态的整体写入(读盘后用) ----
    pub fn restore(&mut self, collapsed: [bool; 2], left_width: f32, panes: PaneStore) {
        self.collapsed = collapsed;
        self.left_width = left_width;
        self.panes = panes;
    }

    fn pair_geometry(&self, side: Side, m: &ChromeMetrics) -> Option<(f32, f32)> {
        let content = self.rects(m).content(side)?;
        Some((content.x, (content.w - m.divider_width).max(0.0)))
    }

    /// 某侧当前面板的配对矩形(无配对返回 `None`)。
    pub fn pair_rects(&self, side: Side, ctx: &Ctx) -> Option<PairRects> {
        let panel = self.selected(side).clone();
        let rules = (ctx.rules)(&panel)?;
        let pane = rules.pane?;
        let content = self.rects(ctx.metrics).content(side)?;
        let dims = self.panes.get_or(&panel, PaneDims::new(pane.default_split, false));
        let mirrored = side != rules.default_side;
        Some(pair_rects(content, dims, pane.spec, mirrored, ctx.metrics))
    }

    // ---- 更新 ----
    pub fn update(&mut self, msg: ShellMsg, ctx: &Ctx) -> Outcome {
        match msg {
            ShellMsg::PanelPress { panel } => self.press(panel),
            ShellMsg::Hover { panel, hovered } => {
                self.hover.entry(panel).or_default().set(hovered);
                Outcome::default()
            }
            ShellMsg::Noop => Outcome::default(),
            ShellMsg::Tick => {
                for a in self.hover.values_mut() {
                    a.advance();
                }
                for side in [Side::Left, Side::Right] {
                    for (i, p) in self.rails[side.index()].iter().enumerate() {
                        self.slots.entry(p.clone()).or_insert(SlotAnim::new(side, i as f32)).retarget(side, i as f32);
                    }
                }
                Outcome::default()
            }
            ShellMsg::RailDragMove { side, index } => {
                if let Some(mut drag) = self.rail_drag.take() {
                    rail::drag_move_into(&mut self.rails, &mut drag, side, index);
                    self.rail_drag = Some(drag);
                }
                Outcome::default()
            }
            ShellMsg::CursorMoved { x, y } => {
                self.cursor = (x, y);
                self.drag_divider(x, ctx);
                Outcome::default()
            }
            ShellMsg::PointerReleased => self.release(),
            ShellMsg::DividerPress(d) => {
                self.divider_drag = Some(d);
                Outcome::default()
            }
            ShellMsg::ShowPanel(panel) => self.show(panel),
            ShellMsg::Maximize(side) => {
                if !self.collapsed[side.index()] {
                    self.maximized = Some(side);
                }
                Outcome::default()
            }
            ShellMsg::MaximizeClose => {
                self.maximized = None;
                Outcome::default()
            }
            ShellMsg::TogglePane(panel) => {
                let Some(pane) = (ctx.rules)(&panel).and_then(|r| r.pane) else {
                    return Outcome::default();
                };
                let collapsed = self.panes.get_or(&panel, PaneDims::new(pane.default_split, false)).collapsed;
                self.panes.set_collapsed(&panel, !collapsed, pane.default_split);
                Outcome { persist: true, ..Outcome::default() }
            }
            ShellMsg::ZoneClicked => {
                if let Some(side) = geometry::zone_at_x(self.cursor.0, self.window.0, &self.zone_state(), ctx.metrics) {
                    self.active_zone = Some(side);
                }
                Outcome::default()
            }
            ShellMsg::Resized { w, h } => {
                self.window = (w, h);
                Outcome::default()
            }
        }
    }

    fn press(&mut self, panel: PanelId) -> Outcome {
        let Some(side) = self.side_of(&panel) else {
            return Outcome::default();
        };
        let idx = self.rails[side.index()].iter().position(|p| *p == panel).expect("side_of found it");
        self.rail_drag = Some(RailDrag { source_side: side, source_index: idx, origin_index: idx, pending_cross: None, press_pos: self.cursor });
        let mut switched_in = None;
        if self.selected[side.index()] == panel {
            // 点已激活的图标:收起 / 展开;对侧也收起时,当前侧是最后一个开着的区,不能关。
            if !self.collapsed[side.other().index()] {
                self.collapsed[side.index()] = !self.collapsed[side.index()];
            }
        } else {
            self.selected[side.index()] = panel.clone();
            self.collapsed[side.index()] = false;
            switched_in = Some(panel);
        }
        // 图标栏点击一律退出放大态:遮罩不拦图标栏上的点击,不清就会留下空的描边浮层。
        self.maximized = None;
        Outcome { persist: true, switched_in }
    }

    fn release(&mut self) -> Outcome {
        let mut out = Outcome::default();
        if let Some(d) = self.divider_drag.take() {
            let _ = d;
            out.persist = true;
        }
        let Some(drag) = self.rail_drag.take() else {
            return out;
        };
        let Some((target_side, target_index)) = drag.pending_cross else {
            // 纯同栏重排:真重排过才落盘
            if drag.source_index != drag.origin_index {
                out.persist = true;
            }
            return out;
        };
        let Some(moved) = rail::cross_apply(&mut self.rails, drag.source_side, drag.source_index, target_side, target_index) else {
            return out; // 会清空源栏:挡住,栏未改动
        };
        // 源栏正显示的就是被搬走的面板:退到该栏现在的第一个,免得两侧同时显示同一面板
        if self.selected[drag.source_side.index()] == moved {
            self.selected[drag.source_side.index()] = self.rails[drag.source_side.index()][0].clone();
        }
        // 被移动的面板成为目标栏新的选中并展开
        self.selected[target_side.index()] = moved;
        self.collapsed[target_side.index()] = false;
        out.persist = true;
        out
    }

    fn show(&mut self, panel: PanelId) -> Outcome {
        let Some(side) = self.side_of(&panel) else {
            return Outcome::default();
        };
        let r = rail::show_panel_in(side, &panel, &self.selected[side.index()], self.collapsed[side.index()], self.maximized);
        if !r.changed {
            return Outcome::default();
        }
        self.selected[side.index()] = r.selected;
        self.collapsed[side.index()] = r.collapsed;
        self.maximized = r.maximized;
        Outcome { persist: true, switched_in: r.switched.then_some(panel) }
    }

    fn drag_divider(&mut self, x: f32, ctx: &Ctx) {
        match self.divider_drag {
            Some(Divider::Zones) => self.drag_zones(x, ctx),
            Some(Divider::Pair(side)) => self.drag_pair(side, x, ctx),
            None => {}
        }
    }

    fn drag_zones(&mut self, x: f32, ctx: &Ctx) {
        let m = ctx.metrics;
        // 拖动前两侧配对里"第一个 slot"的像素宽,拖完要保持不变——否则用户只想调左右分界,
        // 配对里的列表却跟着按比例缩放。
        let mut fixed_px = Vec::new();
        for side in [Side::Left, Side::Right] {
            if let (Some(rules), Some((_, pair_w))) = ((ctx.rules)(self.selected(side)), self.pair_geometry(side, m)) {
                if let Some(pane) = rules.pane {
                    let ratio = self.panes.get_or(self.selected(side), PaneDims::new(pane.default_split, false)).split;
                    fixed_px.push((side, pair_w * ratio));
                }
            }
        }
        self.left_width = geometry::clamp_left_width(self.window.0, x - m.icon_rail_width, m);
        for (side, px) in fixed_px {
            let Some((_, new_w)) = self.pair_geometry(side, m) else { continue };
            if new_w <= 0.0 {
                continue;
            }
            let panel = self.selected(side).clone();
            let ratio = (px / new_w).clamp(m.min_split_ratio, m.max_split_ratio);
            let collapsed = self.panes.get(&panel).is_some_and(|d| d.collapsed);
            self.panes.set(panel, PaneDims::new(ratio, collapsed));
        }
    }

    fn drag_pair(&mut self, side: Side, x: f32, ctx: &Ctx) {
        let m = ctx.metrics;
        let panel = self.selected(side).clone();
        let Some(rules) = (ctx.rules)(&panel) else { return };
        let Some(pane) = rules.pane else { return };
        let Some((x0, pair_w)) = self.pair_geometry(side, m) else { return };
        if pair_w <= 0.0 {
            return;
        }
        let current = self.panes.get_or(&panel, PaneDims::new(pane.default_split, false));
        let out = apply_pane_split_drag(
            current,
            pane.spec,
            side != rules.default_side,
            PaneDragInput { x0, pair_w, logical_x: x, min_ratio: m.min_split_ratio, max_ratio: m.max_split_ratio, min_first_px: pane.min_first_px },
        );
        self.panes.set(panel, PaneDims::new(out.split, out.collapsed));
    }
}

/// 配对视图的两个 slot 与中间分隔线的矩形(窗口坐标,已按镜像排好屏幕位置)。
/// 第一个 slot 收起时它宽 0、没有分隔线,第二个 slot 占满整个内容区。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PairRects {
    pub first: Rect,
    pub second: Rect,
    pub divider: Rect,
}

pub fn pair_rects(content: Rect, dims: PaneDims, spec: PaneSpec, mirrored: bool, m: &ChromeMetrics) -> PairRects {
    if dims.collapsed {
        return PairRects { first: Rect::new(content.x, content.y, 0.0, content.h), second: content, divider: Rect::new(content.x, content.y, 0.0, content.h) };
    }
    let pair_w = (content.w - m.divider_width).max(0.0);
    let first_w = pair_w * dims.split;
    let second_w = pair_w - first_w;
    let first_on_left = spec.default_first_first != mirrored;
    let (left_w, right_w) = if first_on_left { (first_w, second_w) } else { (second_w, first_w) };
    let left = Rect::new(content.x, content.y, left_w, content.h);
    let divider = Rect::new(left.right(), content.y, m.divider_width, content.h);
    let right = Rect::new(divider.right(), content.y, right_w, content.h);
    if first_on_left {
        PairRects { first: left, second: right, divider }
    } else {
        PairRects { first: right, second: left, divider }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    fn m() -> ChromeMetrics {
        ChromeMetrics::byteboy2077()
    }

    /// t.list 是左栏默认的配对面板(列表在前,可收起);t.agent 是右栏默认的配对面板(列表在后)。
    fn rules(p: &PanelId) -> Option<PanelRules> {
        match p.as_str() {
            "t.list" => Some(PanelRules {
                default_side: Side::Left,
                pane: Some(PaneRules { spec: PaneSpec::new(true, true), default_split: 0.4, min_first_px: 150.0 }),
            }),
            "t.agent" => Some(PanelRules {
                default_side: Side::Right,
                pane: Some(PaneRules { spec: PaneSpec::new(false, true), default_split: 0.4, min_first_px: 150.0 }),
            }),
            "t.plain" | "t.a" | "t.b" | "t.x" | "t.y" => Some(PanelRules { default_side: Side::Left, pane: None }),
            _ => None,
        }
    }

    fn model() -> ShellModel {
        ShellModel::new(
            [vec![id("t.list"), id("t.plain"), id("t.a")], vec![id("t.agent"), id("t.x")]],
            [id("t.list"), id("t.agent")],
            640.0,
            (1440.0, 900.0),
        )
        .unwrap()
    }

    fn run(model: &mut ShellModel, msg: ShellMsg) -> Outcome {
        let metrics = m();
        let r = |p: &PanelId| rules(p);
        model.update(msg, &Ctx { metrics: &metrics, rules: &r })
    }

    fn press(model: &mut ShellModel, p: &str) -> Outcome {
        run(model, ShellMsg::PanelPress { panel: id(p) })
    }

    // ---- 构造不变量 ----
    #[test]
    fn construction_rejects_broken_layouts() {
        let ok = || [vec![id("t.a")], vec![id("t.x")]];
        assert!(ShellModel::new(ok(), [id("t.a"), id("t.x")], 640.0, (1440.0, 900.0)).is_ok());
        assert_eq!(ShellModel::new([vec![], vec![id("t.x")]], [id("t.a"), id("t.x")], 640.0, (1.0, 1.0)), Err(ModelError::EmptyRail(Side::Left)));
        assert_eq!(ShellModel::new([vec![id("t.a")], vec![]], [id("t.a"), id("t.x")], 640.0, (1.0, 1.0)), Err(ModelError::EmptyRail(Side::Right)));
        assert_eq!(ShellModel::new(ok(), [id("t.b"), id("t.x")], 640.0, (1.0, 1.0)), Err(ModelError::SelectedNotInRail(Side::Left)));
        assert_eq!(ShellModel::new([vec![id("t.a")], vec![id("t.a")]], [id("t.a"), id("t.a")], 640.0, (1.0, 1.0)), Err(ModelError::Duplicate(id("t.a"))));
        assert_eq!(ShellModel::new([vec![id("t.a"), id("t.a")], vec![id("t.x")]], [id("t.a"), id("t.x")], 640.0, (1.0, 1.0)), Err(ModelError::Duplicate(id("t.a"))));
    }

    // ---- 图标栏点击 ----
    #[test]
    fn pressing_another_icon_selects_it_expands_the_side_and_reports_the_switch() {
        let mut s = model();
        run(&mut s, ShellMsg::Maximize(Side::Left));
        let out = press(&mut s, "t.plain");
        assert_eq!(s.selected(Side::Left), &id("t.plain"));
        assert!(!s.collapsed(Side::Left));
        assert_eq!(out, Outcome { persist: true, switched_in: Some(id("t.plain")) });
        assert_eq!(s.maximized(), None, "a rail click always leaves the maximized state");
    }

    #[test]
    fn pressing_the_active_icon_toggles_its_zone() {
        let mut s = model();
        let out = press(&mut s, "t.list");
        assert!(s.collapsed(Side::Left));
        assert_eq!(out.switched_in, None);
        press(&mut s, "t.list");
        assert!(!s.collapsed(Side::Left));
    }

    #[test]
    fn the_last_open_zone_cannot_be_collapsed() {
        let mut s = model();
        press(&mut s, "t.agent"); // right collapses
        assert!(s.collapsed(Side::Right));
        press(&mut s, "t.list"); // left is the only open one: stays open
        assert!(!s.collapsed(Side::Left));
    }

    #[test]
    fn pressing_an_unknown_panel_does_nothing() {
        let mut s = model();
        let before = s.clone();
        assert_eq!(press(&mut s, "t.nope"), Outcome::default());
        assert_eq!(s, before);
    }

    // ---- 图标栏拖拽 ----
    #[test]
    fn a_click_without_movement_does_not_reorder_but_still_persists_the_selection() {
        let mut s = model();
        press(&mut s, "t.plain");
        let out = run(&mut s, ShellMsg::PointerReleased);
        assert_eq!(out, Outcome::default(), "the release itself has nothing to save");
        assert_eq!(s.rail(Side::Left)[1], id("t.plain"));
        assert!(s.rail_drag().is_none());
    }

    #[test]
    fn dragging_within_a_rail_reorders_live_and_persists_on_release() {
        let mut s = model();
        press(&mut s, "t.list");
        run(&mut s, ShellMsg::RailDragMove { side: Side::Left, index: 2 });
        assert_eq!(s.rail(Side::Left), [id("t.plain"), id("t.a"), id("t.list")]);
        let out = run(&mut s, ShellMsg::PointerReleased);
        assert!(out.persist);
    }

    #[test]
    fn dragging_to_the_other_rail_moves_the_panel_on_release_and_selects_it_there() {
        let mut s = model();
        press(&mut s, "t.list"); // selected on the left; collapses it, then press again to keep it open
        press(&mut s, "t.list");
        assert!(!s.collapsed(Side::Left));
        run(&mut s, ShellMsg::RailDragMove { side: Side::Right, index: 1 });
        assert_eq!(s.rail(Side::Left).len(), 3, "nothing moves while hovering");
        let out = run(&mut s, ShellMsg::PointerReleased);
        assert!(out.persist);
        assert_eq!(s.rail(Side::Right), [id("t.agent"), id("t.list"), id("t.x")]);
        assert_eq!(s.selected(Side::Right), &id("t.list"));
        assert!(!s.collapsed(Side::Right));
        // the left side was showing the moved panel: it falls back to the first one left there
        assert_eq!(s.selected(Side::Left), &id("t.plain"));
    }

    #[test]
    fn a_cross_rail_drag_that_would_empty_the_source_is_refused() {
        let mut s = ShellModel::new([vec![id("t.a")], vec![id("t.x"), id("t.y")]], [id("t.a"), id("t.x")], 640.0, (1440.0, 900.0)).unwrap();
        press(&mut s, "t.a");
        run(&mut s, ShellMsg::RailDragMove { side: Side::Right, index: 0 });
        let out = run(&mut s, ShellMsg::PointerReleased);
        assert!(!out.persist);
        assert_eq!(s.rail(Side::Left), [id("t.a")]);
    }

    #[test]
    fn the_drag_is_confirmed_only_after_the_cursor_travels() {
        let mut s = model();
        run(&mut s, ShellMsg::CursorMoved { x: 20.0, y: 20.0 });
        run(&mut s, ShellMsg::PanelPress { panel: id("t.plain") });
        assert!(!s.rail_drag_confirmed());
        run(&mut s, ShellMsg::CursorMoved { x: 21.0, y: 20.0 });
        assert!(!s.rail_drag_confirmed());
        run(&mut s, ShellMsg::CursorMoved { x: 40.0, y: 20.0 });
        assert!(s.rail_drag_confirmed());
        assert_eq!(s.dragged_panel(), Some(id("t.plain")));
    }

    // ---- 程序化显示 ----
    #[test]
    fn show_panel_never_collapses_and_reports_only_real_switches() {
        let mut s = model();
        assert_eq!(run(&mut s, ShellMsg::ShowPanel(id("t.list"))), Outcome::default(), "already visible");
        press(&mut s, "t.list"); // collapse it
        let out = run(&mut s, ShellMsg::ShowPanel(id("t.list")));
        assert!(!s.collapsed(Side::Left));
        assert_eq!(out, Outcome { persist: true, switched_in: None });
        let out = run(&mut s, ShellMsg::ShowPanel(id("t.a")));
        assert_eq!(s.selected(Side::Left), &id("t.a"));
        assert_eq!(out.switched_in, Some(id("t.a")));
    }

    // ---- 放大 ----
    #[test]
    fn maximize_is_transient_and_ignored_for_a_collapsed_side() {
        let mut s = model();
        run(&mut s, ShellMsg::Maximize(Side::Right));
        assert_eq!(s.maximized(), Some(Side::Right));
        run(&mut s, ShellMsg::MaximizeClose);
        assert_eq!(s.maximized(), None);
        press(&mut s, "t.agent"); // collapse the right side
        run(&mut s, ShellMsg::Maximize(Side::Right));
        assert_eq!(s.maximized(), None, "a collapsed side cannot be maximized");
    }

    // ---- 聚焦区与缩放 ----
    #[test]
    fn a_click_sets_the_focused_zone_but_a_click_on_a_rail_keeps_it() {
        let mut s = model();
        let click = |s: &mut ShellModel, x: f32| {
            run(s, ShellMsg::CursorMoved { x, y: 300.0 });
            run(s, ShellMsg::ZoneClicked);
        };
        click(&mut s, 900.0);
        assert_eq!(s.active_zone(), Some(Side::Right));
        click(&mut s, 10.0);
        assert_eq!(s.active_zone(), Some(Side::Right));
        click(&mut s, 300.0);
        assert_eq!(s.active_zone(), Some(Side::Left));
    }

    #[test]
    fn resizing_never_rewrites_the_preferred_left_width() {
        let mut s = model();
        run(&mut s, ShellMsg::Resized { w: 700.0, h: 600.0 });
        assert_eq!(s.left_width(), 640.0);
        assert_eq!(s.effective_left_width(&m()), 320.0);
        run(&mut s, ShellMsg::Resized { w: 1440.0, h: 900.0 });
        assert_eq!(s.effective_left_width(&m()), 640.0, "widening the window restores the preference");
    }

    // ---- 动画 ----
    #[test]
    fn hover_and_slot_animations_run_on_ticks_and_settle() {
        let mut s = model();
        assert!(!s.animating());
        run(&mut s, ShellMsg::Hover { panel: id("t.a"), hovered: true });
        assert!(s.animating());
        for _ in 0..12 {
            run(&mut s, ShellMsg::Tick);
        }
        assert_eq!(s.hover_progress(&id("t.a")), 1.0);
        assert!(!s.animating());
        // reordering slides the neighbours instead of jumping
        press(&mut s, "t.list");
        run(&mut s, ShellMsg::Tick);
        run(&mut s, ShellMsg::RailDragMove { side: Side::Left, index: 2 });
        assert!(s.animating(), "the target slot changed, the animation has not caught up yet");
        run(&mut s, ShellMsg::Tick);
        let slid = s.slot_position(Side::Left, &id("t.plain"), 0);
        assert!(slid > 0.0 && slid < 1.0, "t.plain moved from slot 1 to 0 and is on its way: {slid}");
        assert!(s.animating());
        for _ in 0..12 {
            run(&mut s, ShellMsg::Tick);
        }
        assert_eq!(s.slot_position(Side::Left, &id("t.plain"), 0), 0.0);
        assert!(!s.animating());
    }

    // ---- 左右分隔线 ----
    #[test]
    fn dragging_the_zone_divider_sets_the_left_width_clamped_and_persists_on_release() {
        let mut s = model();
        run(&mut s, ShellMsg::DividerPress(Divider::Zones));
        run(&mut s, ShellMsg::CursorMoved { x: 544.0, y: 300.0 });
        assert_eq!(s.left_width(), 500.0, "x minus the rail width");
        run(&mut s, ShellMsg::CursorMoved { x: 5.0, y: 300.0 });
        assert_eq!(s.left_width(), 320.0, "never narrower than the minimum zone");
        run(&mut s, ShellMsg::CursorMoved { x: 9000.0, y: 300.0 });
        assert_eq!(s.left_width(), 1344.0 - 320.0);
        let out = run(&mut s, ShellMsg::PointerReleased);
        assert!(out.persist);
        // moving the cursor after release changes nothing
        run(&mut s, ShellMsg::CursorMoved { x: 600.0, y: 300.0 });
        assert_eq!(s.left_width(), 1024.0);
    }

    #[test]
    fn dragging_the_zone_divider_keeps_the_pair_list_width_in_pixels() {
        let mut s = model();
        let metrics = m();
        let before = s.pair_rects(Side::Left, &Ctx { metrics: &metrics, rules: &rules }).unwrap();
        run(&mut s, ShellMsg::DividerPress(Divider::Zones));
        run(&mut s, ShellMsg::CursorMoved { x: 44.0 + 800.0, y: 300.0 });
        let after = s.pair_rects(Side::Left, &Ctx { metrics: &metrics, rules: &rules }).unwrap();
        assert!((before.first.w - after.first.w).abs() < 0.5, "list stayed {} px, got {}", before.first.w, after.first.w);
        assert!(after.second.w > before.second.w, "the content side takes the extra space");
    }

    // ---- 配对分隔线 ----
    #[test]
    fn dragging_a_pair_divider_changes_the_split_and_collapses_when_dragged_narrow() {
        let mut s = model();
        run(&mut s, ShellMsg::DividerPress(Divider::Pair(Side::Left)));
        // left content: x 51, w 632 ⇒ pair_w 624, list first. Cursor at x0 + 0.5*pair_w.
        run(&mut s, ShellMsg::CursorMoved { x: 51.0 + 312.0, y: 300.0 });
        let d = s.panes().get(&id("t.list")).unwrap();
        assert!((d.split - 0.5).abs() < 1e-3 && !d.collapsed);
        // dragged almost to the left edge: below min_first_px ⇒ collapsed, ratio frozen
        run(&mut s, ShellMsg::CursorMoved { x: 60.0, y: 300.0 });
        let d = s.panes().get(&id("t.list")).unwrap();
        assert!(d.collapsed);
        assert!((d.split - 0.5).abs() < 1e-3);
        // dragged back out: expands again
        run(&mut s, ShellMsg::CursorMoved { x: 51.0 + 200.0, y: 300.0 });
        assert!(!s.panes().get(&id("t.list")).unwrap().collapsed);
        assert!(run(&mut s, ShellMsg::PointerReleased).persist);
    }

    #[test]
    fn a_panel_without_a_pair_ignores_pair_drags() {
        let mut s = model();
        press(&mut s, "t.plain");
        run(&mut s, ShellMsg::DividerPress(Divider::Pair(Side::Left)));
        run(&mut s, ShellMsg::CursorMoved { x: 300.0, y: 300.0 });
        assert!(s.panes().is_empty());
    }

    // ---- 切换配对收起 ----
    #[test]
    fn toggle_pane_flips_collapsed_keeps_the_split_and_asks_to_persist() {
        let mut s = model();
        // 还没有条目:第一次切换用 default_split(0.4)建一条已收起的
        let out = run(&mut s, ShellMsg::TogglePane(id("t.list")));
        assert!(out.persist);
        assert_eq!(s.panes().get(&id("t.list")), Some(PaneDims::new(0.4, true)));
        // 再切换一次展开
        assert!(run(&mut s, ShellMsg::TogglePane(id("t.list"))).persist);
        assert_eq!(s.panes().get(&id("t.list")), Some(PaneDims::new(0.4, false)));
        // 拖出一个 0.5 的比例,收起再展开后比例还在
        run(&mut s, ShellMsg::DividerPress(Divider::Pair(Side::Left)));
        run(&mut s, ShellMsg::CursorMoved { x: 51.0 + 312.0, y: 300.0 });
        run(&mut s, ShellMsg::PointerReleased);
        run(&mut s, ShellMsg::TogglePane(id("t.list")));
        let d = s.panes().get(&id("t.list")).unwrap();
        assert!(d.collapsed && (d.split - 0.5).abs() < 1e-3);
        run(&mut s, ShellMsg::TogglePane(id("t.list")));
        let d = s.panes().get(&id("t.list")).unwrap();
        assert!(!d.collapsed && (d.split - 0.5).abs() < 1e-3, "the dragged split survives a collapse/expand round trip");
    }

    #[test]
    fn toggle_pane_on_a_panel_without_a_pair_does_nothing() {
        let mut s = model();
        for p in ["t.plain", "t.nope"] {
            let out = run(&mut s, ShellMsg::TogglePane(id(p)));
            assert!(!out.persist, "{p}");
        }
        assert!(s.panes().is_empty());
    }

    #[test]
    fn a_collapsed_pair_expands_again_when_dragged_back_out() {
        let mut s = model();
        run(&mut s, ShellMsg::TogglePane(id("t.list")));
        assert!(s.panes().get(&id("t.list")).unwrap().collapsed);
        // 收起时没有分隔线可拖;直接模拟"仍在拖拽中"的光标回到宽处:展开
        run(&mut s, ShellMsg::DividerPress(Divider::Pair(Side::Left)));
        run(&mut s, ShellMsg::CursorMoved { x: 51.0 + 250.0, y: 300.0 });
        assert!(!s.panes().get(&id("t.list")).unwrap().collapsed);
    }

    // ---- 配对矩形 ----
    #[test]
    fn pair_rects_place_the_list_first_unless_the_panel_is_mirrored() {
        let content = Rect::new(51.0, 47.0, 632.0, 830.0);
        let d = PaneDims::new(0.4, false);
        let spec = PaneSpec::new(true, true);
        let p = pair_rects(content, d, spec, false, &m());
        // pair_w = 632 - 8 = 624; first = 249.6
        assert!((p.first.w - 249.6).abs() < 1e-3);
        assert_eq!(p.first.x, 51.0, "list on the left");
        assert!((p.divider.x - (51.0 + 249.6)).abs() < 1e-3);
        assert!((p.second.x - (p.divider.right())).abs() < 1e-3);
        assert!((p.first.w + p.divider.w + p.second.w - content.w).abs() < 1e-3);
        let mir = pair_rects(content, d, spec, true, &m());
        assert!(mir.second.x < mir.first.x, "mirrored: the list moves to the right");
        assert_eq!((mir.first.w, mir.second.w), (p.first.w, p.second.w), "widths do not depend on the mirror");
    }

    #[test]
    fn a_collapsed_pair_gives_the_whole_content_to_the_second_slot() {
        let content = Rect::new(51.0, 47.0, 632.0, 830.0);
        let p = pair_rects(content, PaneDims::new(0.4, true), PaneSpec::new(true, true), false, &m());
        assert_eq!(p.second, content);
        assert_eq!(p.first.w, 0.0);
        assert_eq!(p.divider.w, 0.0);
    }

    #[test]
    fn a_panel_whose_list_is_not_first_by_default_renders_the_list_on_the_right() {
        // t.agent: list (first slot) is rendered AFTER the content by default (default_first_first = false)
        let s = model();
        let metrics = m();
        let p = s.pair_rects(Side::Right, &Ctx { metrics: &metrics, rules: &rules }).unwrap();
        assert!(p.first.x > p.second.x, "list on the right by default");
    }
}
