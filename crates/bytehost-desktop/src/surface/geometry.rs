//! Surface 几何:逻辑坐标 → 原生子视图 bounds 的统一换算。
//!
//! 唯一真相在窗口线程;本模块是**纯函数**,便于单测(不碰 wry)。
//! DPI 缩放由 wry/winit 承担(此处只做逻辑运算);本模块额外处理窗口缩放、
//! 零面积、水平分割(split)与裁剪(clip)。
//!
//! 顶栏/工具条等窗口内浮层遮挡由宿主负责把 Surface 矩形**下推**
//! (`Insets::top`);这里只按已给定的 insets 换算,不再重复判断遮挡。

use bytehost_core::{Insets, LogicalRect};

/// 一个 Surface 相对窗口的位置描述:分割比例 + 顶部占用。
///
/// - `split`:Surface 占可用宽度的比例(`0.0..=1.0`);`None` 表示占满。
/// - `insets`:窗口顶部占用(顶栏等),换算时扣掉。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub split: Option<f32>,
    pub insets: Insets,
}

impl Placement {
    /// 占满窗口(仅扣 insets)。
    pub fn full(insets: Insets) -> Self {
        Self {
            split: None,
            insets,
        }
    }

    /// 左侧分割,占 `fraction` 宽度。
    pub fn left_split(fraction: f32, insets: Insets) -> Self {
        Self {
            split: Some(fraction.clamp(0.0, 1.0)),
            insets,
        }
    }
}

/// 把窗口逻辑尺寸 + 放置描述换算成一个非负的 Surface 逻辑矩形。
///
/// 语义:
/// - 扣掉 `insets.top` 后再算可用高度;
/// - `split` 取可用宽度的一部分(左侧);`None` 占满;
/// - 任何负尺寸裁剪到 0(零面积 Surface 不 panic,交给上层决定跳过);
/// - 超出窗口的部分不产生负宽高。
pub fn surface_rect(window_w: f32, window_h: f32, placement: Placement) -> LogicalRect {
    let avail_w = window_w.max(0.0) as f64;
    let avail_h = (window_h - placement.insets.top).max(0.0) as f64;
    let width = match placement.split {
        Some(fraction) => (avail_w * fraction as f64).clamp(0.0, avail_w),
        None => avail_w,
    };
    LogicalRect {
        x: 0.0,
        y: placement.insets.top.max(0.0) as f64,
        width: width.max(0.0),
        height: avail_h.max(0.0),
    }
}

/// 计算一个**窗口内浮层**隐藏/收缩时要还原的 Surface 矩形。
///
/// 浮层打开时,被遮挡的 Surface 要么隐藏(返回空矩形)要么下推/缩小(返回新矩形);
/// 关闭时用 `reserve_top = -base_shift` 还原。`reserve_top` 是浮层占用的顶部高度
/// (正值下推,负值恢复)。
pub fn rect_after_overlay(base: LogicalRect, reserve_top: f32) -> LogicalRect {
    let push = reserve_top as f64;
    let new_y = base.y + push;
    let new_h = (base.height - push).max(0.0);
    LogicalRect {
        x: base.x,
        y: new_y,
        width: base.width,
        height: new_h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insets_top(t: f32) -> Insets {
        Insets { top: t }
    }

    #[test]
    fn full_placement_subtracts_top_inset() {
        let r = surface_rect(800.0, 600.0, Placement::full(insets_top(44.0)));
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 44.0);
        assert_eq!(r.width, 800.0);
        assert_eq!(r.height, 556.0);
    }

    #[test]
    fn left_split_takes_fraction_of_available_width() {
        let r = surface_rect(800.0, 600.0, Placement::left_split(0.25, insets_top(44.0)));
        assert_eq!(r.width, 200.0);
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 44.0);
    }

    #[test]
    fn split_is_clamped_to_unit_range() {
        let r = surface_rect(
            800.0,
            600.0,
            Placement::left_split(2.0, Insets { top: 0.0 }),
        );
        assert_eq!(r.width, 800.0);
        let r = surface_rect(
            800.0,
            600.0,
            Placement::left_split(-1.0, Insets { top: 0.0 }),
        );
        assert_eq!(r.width, 0.0);
    }

    #[test]
    fn zero_or_tiny_window_yields_zero_area_not_negative() {
        let r = surface_rect(10.0, 10.0, Placement::full(insets_top(44.0)));
        assert_eq!(r.height, 0.0, "窗口比顶栏还矮时高度裁剪为 0");
        assert!(r.width >= 0.0);
        assert!(r.height >= 0.0);
        assert!(r.is_empty());
    }

    #[test]
    fn negative_dimensions_are_clamped() {
        let r = surface_rect(-100.0, -100.0, Placement::full(insets_top(44.0)));
        assert_eq!(r.width, 0.0);
        assert_eq!(r.height, 0.0);
    }

    #[test]
    fn overlay_push_shrinks_top_and_restore_is_exact() {
        let base = surface_rect(800.0, 600.0, Placement::full(insets_top(44.0)));
        let pushed = rect_after_overlay(base, 40.0);
        assert_eq!(pushed.y, 84.0);
        assert_eq!(pushed.height, 516.0);
        // 还原即原矩形。
        let restored = rect_after_overlay(pushed, -40.0);
        assert_eq!(restored, base);
    }
}
