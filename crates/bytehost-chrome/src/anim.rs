//! 图标栏的两种动画状态机(照 Dozer `HoverAnim` / `RailSlotAnim`):悬停颜色渐变、换位时相邻图标滑动让位。
//! 都是"每拍残余 50% 的指数逼近,足够接近就 snap",由产品的 ~60fps 定时器驱动 `Tick`。

use crate::geometry::Side;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HoverAnim {
    progress: f32,
    target: f32,
}

impl HoverAnim {
    pub fn set(&mut self, hovered: bool) {
        self.target = if hovered { 1.0 } else { 0.0 };
    }
    pub fn advance(&mut self) {
        let next = self.progress + (self.target - self.progress) * 0.5;
        self.progress = if (next - self.target).abs() < 0.01 {
            self.target
        } else {
            next
        };
    }
    pub fn active(&self) -> bool {
        (self.progress - self.target).abs() > 0.001
    }
    pub fn t(&self) -> f32 {
        self.progress
    }
}

/// 图标按钮的槽位动画:`current` 是本帧渲染用的浮点槽位号,逼近 `target`;`side` 记录上次逼近所在的栏——
/// 同栏重排时平滑滑动,跨栏时两条栏是完全不同的物理列,直接 snap。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotAnim {
    current: f32,
    side: Side,
}

impl SlotAnim {
    pub fn new(side: Side, target: f32) -> Self {
        Self {
            current: target,
            side,
        }
    }
    pub fn retarget(&mut self, side: Side, target: f32) {
        if self.side != side {
            self.side = side;
            self.current = target;
            return;
        }
        let next = self.current + (target - self.current) * 0.5;
        self.current = if (next - target).abs() < 0.02 {
            target
        } else {
            next
        };
    }
    pub fn active(&self, side: Side, target: f32) -> bool {
        self.side == side && (self.current - target).abs() > 0.005
    }
    pub fn current(&self, side: Side) -> Option<f32> {
        (self.side == side).then_some(self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_converges_in_a_few_ticks_and_reports_activity() {
        let mut h = HoverAnim::default();
        assert!(!h.active());
        h.set(true);
        assert!(h.active());
        let mut ticks = 0;
        while h.active() {
            h.advance();
            ticks += 1;
            assert!(ticks < 20, "must converge");
        }
        assert_eq!(h.t(), 1.0);
        h.set(false);
        h.advance();
        assert!(h.t() < 1.0 && h.t() > 0.0, "eases out, does not jump");
    }

    #[test]
    fn a_slot_slides_within_a_rail_and_snaps_across_rails() {
        let mut a = SlotAnim::new(Side::Left, 0.0);
        a.retarget(Side::Left, 2.0);
        let first = a.current(Side::Left).unwrap();
        assert!(first > 0.0 && first < 2.0, "slides, does not jump: {first}");
        for _ in 0..12 {
            a.retarget(Side::Left, 2.0);
        }
        assert_eq!(a.current(Side::Left), Some(2.0));
        assert!(!a.active(Side::Left, 2.0));
        a.retarget(Side::Right, 1.0);
        assert_eq!(a.current(Side::Right), Some(1.0), "a cross-rail move snaps");
        assert_eq!(
            a.current(Side::Left),
            None,
            "the old side no longer owns the animation"
        );
    }
}
