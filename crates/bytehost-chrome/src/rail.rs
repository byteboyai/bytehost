//! 图标栏的纯逻辑:同栏重排、跨栏移动、拖拽阈值、"确保可见"。以 `PanelId` 为键。
//! 行为照 Dozer `chrome/rail.rs`(`rail_drag_move_into` / `rail_cross_apply` / `show_panel_in` /
//! `rail_drag_past_threshold`),不依赖 iced,可直接单测。

use bytehost_core::id::PanelId;

use crate::geometry::Side;

/// 两条图标栏,`[左, 右]`。
pub type Rails = [Vec<PanelId>; 2];

/// 正在进行的图标栏拖拽(同栏重排 / 跨栏移动)。
#[derive(Debug, Clone, PartialEq)]
pub struct RailDrag {
    pub source_side: Side,
    pub source_index: usize,
    /// 拖拽开始时 `source_index` 的原始值;同栏重排期间 `source_index` 会变,据此判断是否真的重排过。
    pub origin_index: usize,
    /// 悬停到另一栏时记录的目标位置;松手才提交,悬停期间不搬、不落盘。
    pub pending_cross: Option<(Side, usize)>,
    /// 按下瞬间的光标位置,用来区分"单击"与"真的在拖"。
    pub press_pos: (f32, f32),
}

/// 从"按下武装"到"算一次真的拖拽"所需的最小位移(逻辑像素),避免快速单击闪出拖拽视觉。
pub const DRAG_THRESHOLD_PX: f32 = 4.0;

pub fn side_of(rails: &Rails, panel: &PanelId) -> Option<Side> {
    if rails[0].contains(panel) {
        Some(Side::Left)
    } else if rails[1].contains(panel) {
        Some(Side::Right)
    } else {
        None
    }
}

/// 同栏:立即重排并更新 `source_index`,清掉跨栏悬停残留;跨栏:只记悬停目标,松手时再提交。
pub fn drag_move_into(rails: &mut Rails, drag: &mut RailDrag, side: Side, to: usize) {
    if drag.source_side == side {
        drag.pending_cross = None;
        let panels = &mut rails[side.index()];
        let from = drag.source_index;
        if from == to || from >= panels.len() || to >= panels.len() {
            return;
        }
        let panel = panels.remove(from);
        panels.insert(to, panel);
        drag.source_index = to;
    } else {
        drag.pending_cross = Some((side, to));
    }
}

/// 跨栏移动的落地:把 `source_side` 第 `source_index` 个面板搬到 `target_side` 第 `target_index` 位。
/// 源栏只剩这一个(搬走会清空栏)或下标越界时返回 `None` 且不改动;目标下标越界时取末尾。
pub fn cross_apply(
    rails: &mut Rails,
    source_side: Side,
    source_index: usize,
    target_side: Side,
    target_index: usize,
) -> Option<PanelId> {
    if source_side == target_side {
        return None;
    }
    let source = &rails[source_side.index()];
    if source.len() <= 1 || source_index >= source.len() {
        return None;
    }
    let panel = rails[source_side.index()].remove(source_index);
    let target = &mut rails[target_side.index()];
    let at = target_index.min(target.len());
    target.insert(at, panel.clone());
    Some(panel)
}

/// 此刻鼠标下真正拖着的那个面板(同栏重排会实时更新 `source_index`)。
pub fn dragged_panel(rails: &Rails, drag: Option<&RailDrag>) -> Option<PanelId> {
    let drag = drag?;
    rails[drag.source_side.index()]
        .get(drag.source_index)
        .cloned()
}

/// 自按下以来光标是否已越过阈值(越过才算拖拽,不是单击)。
pub fn past_threshold(drag: &RailDrag, cursor: (f32, f32)) -> bool {
    let dx = cursor.0 - drag.press_pos.0;
    let dy = cursor.1 - drag.press_pos.1;
    dx * dx + dy * dy > DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX
}

/// `show_panel_in` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ShowPanel {
    pub selected: PanelId,
    pub collapsed: bool,
    pub maximized: Option<Side>,
    /// 该侧原本显示的是别的面板(真切换)。
    pub switched: bool,
    /// `false` = 面板本来就看得见,什么都不用做。
    pub changed: bool,
}

/// "把某面板显示出来"的纯逻辑——与图标栏点击的区别:**绝不收起**。已经看得见就什么都不做;被收起就展开;
/// 显示的是别的就切过去;被另一侧的放大态盖住就退出放大。
pub fn show_panel_in(
    side: Side,
    panel: &PanelId,
    selected: &PanelId,
    collapsed: bool,
    maximized: Option<Side>,
) -> ShowPanel {
    let visible = selected == panel && !collapsed;
    let covered = maximized.is_some_and(|m| m != side);
    if visible && !covered {
        return ShowPanel {
            selected: selected.clone(),
            collapsed,
            maximized,
            switched: false,
            changed: false,
        };
    }
    ShowPanel {
        selected: panel.clone(),
        collapsed: false,
        maximized: None,
        switched: selected != panel,
        changed: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    fn rails() -> Rails {
        [
            vec![id("t.a"), id("t.b"), id("t.c")],
            vec![id("t.x"), id("t.y")],
        ]
    }

    fn drag(side: Side, index: usize) -> RailDrag {
        RailDrag {
            source_side: side,
            source_index: index,
            origin_index: index,
            pending_cross: None,
            press_pos: (0.0, 0.0),
        }
    }

    fn names(r: &Rails, side: Side) -> Vec<&str> {
        r[side.index()].iter().map(|p| p.as_str()).collect()
    }

    #[test]
    fn same_side_reorder_moves_the_panel_and_follows_it() {
        let mut r = rails();
        let mut d = drag(Side::Left, 0);
        drag_move_into(&mut r, &mut d, Side::Left, 2);
        assert_eq!(names(&r, Side::Left), ["t.b", "t.c", "t.a"]);
        assert_eq!(d.source_index, 2);
        assert_eq!(
            d.origin_index, 0,
            "origin stays so the end can tell a reorder happened"
        );
        assert_eq!(dragged_panel(&r, Some(&d)), Some(id("t.a")));
    }

    #[test]
    fn same_side_reorder_to_the_same_or_a_bad_index_is_a_no_op() {
        let mut r = rails();
        let mut d = drag(Side::Left, 1);
        drag_move_into(&mut r, &mut d, Side::Left, 1);
        drag_move_into(&mut r, &mut d, Side::Left, 9);
        assert_eq!(names(&r, Side::Left), ["t.a", "t.b", "t.c"]);
        assert_eq!(d.source_index, 1);
    }

    #[test]
    fn hovering_the_other_side_only_records_a_pending_target() {
        let mut r = rails();
        let mut d = drag(Side::Left, 1);
        drag_move_into(&mut r, &mut d, Side::Right, 1);
        assert_eq!(d.pending_cross, Some((Side::Right, 1)));
        assert_eq!(
            names(&r, Side::Left),
            ["t.a", "t.b", "t.c"],
            "nothing moves until the button is released"
        );
        // hovering back cancels the pending move
        drag_move_into(&mut r, &mut d, Side::Left, 1);
        assert_eq!(d.pending_cross, None);
    }

    #[test]
    fn cross_side_move_relocates_and_clamps_the_target_index() {
        let mut r = rails();
        assert_eq!(
            cross_apply(&mut r, Side::Left, 1, Side::Right, 1),
            Some(id("t.b"))
        );
        assert_eq!(names(&r, Side::Left), ["t.a", "t.c"]);
        assert_eq!(names(&r, Side::Right), ["t.x", "t.b", "t.y"]);
        // an index past the end lands the panel last
        assert_eq!(
            cross_apply(&mut r, Side::Left, 0, Side::Right, 99),
            Some(id("t.a"))
        );
        assert_eq!(names(&r, Side::Right), ["t.x", "t.b", "t.y", "t.a"]);
    }

    #[test]
    fn a_side_is_never_emptied_by_a_cross_side_move() {
        let mut r: Rails = [vec![id("t.only")], vec![id("t.x")]];
        assert_eq!(cross_apply(&mut r, Side::Left, 0, Side::Right, 0), None);
        assert_eq!(names(&r, Side::Left), ["t.only"]);
        assert_eq!(names(&r, Side::Right), ["t.x"]);
    }

    #[test]
    fn a_stale_source_index_or_same_side_target_changes_nothing() {
        let mut r = rails();
        assert_eq!(cross_apply(&mut r, Side::Left, 7, Side::Right, 0), None);
        assert_eq!(cross_apply(&mut r, Side::Left, 0, Side::Left, 1), None);
        assert_eq!(r, rails());
    }

    #[test]
    fn dragged_panel_is_none_without_a_drag_or_with_a_bad_index() {
        let r = rails();
        assert_eq!(dragged_panel(&r, None), None);
        assert_eq!(dragged_panel(&r, Some(&drag(Side::Right, 9))), None);
        assert_eq!(
            dragged_panel(&r, Some(&drag(Side::Right, 1))),
            Some(id("t.y"))
        );
    }

    #[test]
    fn the_drag_threshold_separates_a_click_from_a_drag() {
        let mut d = drag(Side::Left, 0);
        d.press_pos = (10.0, 10.0);
        assert!(!past_threshold(&d, (10.0, 10.0)));
        assert!(
            !past_threshold(&d, (14.0, 10.0)),
            "exactly at the threshold is still a click"
        );
        assert!(past_threshold(&d, (14.1, 10.0)));
        assert!(past_threshold(&d, (13.0, 13.0)), "diagonal distance counts");
    }

    #[test]
    fn show_panel_does_nothing_when_it_is_already_visible() {
        let out = show_panel_in(Side::Left, &id("t.a"), &id("t.a"), false, None);
        assert!(!out.changed && !out.switched);
    }

    #[test]
    fn show_panel_never_collapses_an_open_panel_and_keeps_its_own_maximize() {
        let out = show_panel_in(Side::Left, &id("t.a"), &id("t.a"), false, Some(Side::Left));
        assert!(!out.changed);
        assert_eq!(out.maximized, Some(Side::Left));
    }

    #[test]
    fn show_panel_expands_a_collapsed_active_panel_without_a_switch() {
        let out = show_panel_in(Side::Left, &id("t.a"), &id("t.a"), true, None);
        assert!(out.changed && !out.switched && !out.collapsed);
    }

    #[test]
    fn show_panel_switches_when_the_side_shows_another_panel() {
        let out = show_panel_in(Side::Left, &id("t.b"), &id("t.a"), false, None);
        assert!(out.changed && out.switched);
        assert_eq!(out.selected, id("t.b"));
    }

    #[test]
    fn show_panel_exits_a_maximize_that_covers_it_and_a_switch_always_exits_it() {
        // the other side is maximized and covers us: leave the maximize so we can be seen
        let out = show_panel_in(Side::Left, &id("t.a"), &id("t.a"), false, Some(Side::Right));
        assert!(out.changed);
        assert_eq!(out.maximized, None);
        // a switch always drops the maximize
        let out = show_panel_in(Side::Left, &id("t.b"), &id("t.a"), false, Some(Side::Left));
        assert_eq!(out.maximized, None);
    }

    #[test]
    fn side_of_finds_each_panel() {
        let r = rails();
        assert_eq!(side_of(&r, &id("t.b")), Some(Side::Left));
        assert_eq!(side_of(&r, &id("t.y")), Some(Side::Right));
        assert_eq!(side_of(&r, &id("t.nope")), None);
    }
}
