//! 纯几何:窗口尺寸 + 骨架状态 → 各区域矩形。无 iced、无窗口。
//!
//! 公式来自 Dozer(`app/state.rs` 的 `zones_width` / `clamp_left_width` / `left_zone_width` /
//! `right_zone_width`,`app/layout.rs` 的 `maximized_box_*`),但以 `Side` + 中立度量表达。

use crate::metrics::{ChromeMetrics, Insets};

/// 逻辑像素矩形,原点在窗口左上角。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// 向内缩 `insets`,宽高不会为负。
    pub fn inset(&self, insets: Insets) -> Rect {
        Rect::new(
            self.x + insets.left,
            self.y + insets.top,
            (self.w - insets.horizontal()).max(0.0),
            (self.h - insets.vertical()).max(0.0),
        )
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
        }
    }
}

/// 几何需要的骨架状态(`ShellModel` 提供)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneState {
    pub left_collapsed: bool,
    pub right_collapsed: bool,
    /// 持久化的左面板区外框宽度(未夹取)。
    pub left_width: f32,
    pub maximized: Option<Side>,
}

/// 窗口里所有区域的矩形。`None` = 这个区域此刻不存在 / 不可见。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellRects {
    pub top_bar: Rect,
    pub left_rail: Rect,
    pub right_rail: Rect,
    pub footbar: Rect,
    /// 左右面板区之间那条恒在的分隔线。
    pub divider: Rect,
    /// 面板区外框(含 margin)。收起的一侧是 `None`。
    pub left_zone: Option<Rect>,
    pub right_zone: Option<Rect>,
    /// 面板内容矩形(外框 − margin − padding;放大态是放大盒子本身)。
    /// 收起、或被另一侧放大盖住的一侧是 `None`。
    pub left_content: Option<Rect>,
    pub right_content: Option<Rect>,
    /// 放大态的金色描边盒子。
    pub maximize_box: Option<Rect>,
}

impl ShellRects {
    pub fn content(&self, side: Side) -> Option<Rect> {
        match side {
            Side::Left => self.left_content,
            Side::Right => self.right_content,
        }
    }

    pub fn zone(&self, side: Side) -> Option<Rect> {
        match side {
            Side::Left => self.left_zone,
            Side::Right => self.right_zone,
        }
    }
}

/// 两条图标栏与恒在的分隔线之外,留给左右面板区的总宽。
/// 分隔线无条件占位(收起一侧也保留拖拽手柄),所以恒扣。
pub fn zones_width(window_w: f32, m: &ChromeMetrics) -> f32 {
    (window_w - 2.0 * m.icon_rail_width - m.divider_width).max(0.0)
}

/// 把持久化的左宽夹进"当前窗口宽度下合法"的区间:下限 `min_zone_width`,上限给对侧也留够
/// `min_zone_width`。窗口窄到上限低于下限时用下限垫平,`clamp` 恒不 panic。
/// 这是**唯一**一处夹取:渲染、几何、拖拽都走它。
pub fn clamp_left_width(window_w: f32, left_width: f32, m: &ChromeMetrics) -> f32 {
    let upper = (zones_width(window_w, m) - m.min_zone_width).max(m.min_zone_width);
    left_width.clamp(m.min_zone_width, upper)
}

/// 左面板区外框当前实际宽度:收起 0;对侧收起时独占;否则夹取后的持久化宽。
pub fn left_zone_width(window_w: f32, s: &ZoneState, m: &ChromeMetrics) -> f32 {
    if s.left_collapsed {
        0.0
    } else if s.right_collapsed {
        zones_width(window_w, m)
    } else {
        clamp_left_width(window_w, s.left_width, m)
    }
}

/// 右面板区外框当前实际宽度:收起 0;否则是左区没占走的剩余。
pub fn right_zone_width(window_w: f32, s: &ZoneState, m: &ChromeMetrics) -> f32 {
    if s.right_collapsed {
        return 0.0;
    }
    (zones_width(window_w, m) - left_zone_width(window_w, s, m)).max(0.0)
}

/// 放大态盒子:两条图标栏之间、顶栏之下、直到窗口底边(遮罩也盖住底栏),四周留 `maximize_padding`。
pub fn maximize_box(window_w: f32, window_h: f32, m: &ChromeMetrics) -> Rect {
    let p = m.maximize_padding;
    Rect::new(
        m.icon_rail_width + p,
        m.top_bar_height + p,
        (window_w - 2.0 * m.icon_rail_width - 2.0 * p).max(0.0),
        (window_h - m.top_bar_height - 2.0 * p).max(0.0),
    )
}

/// 窗口逻辑尺寸 + 骨架状态 → 全部区域矩形。
pub fn layout(window_w: f32, window_h: f32, s: &ZoneState, m: &ChromeMetrics) -> ShellRects {
    let rail_h = (window_h - m.top_bar_height).max(0.0);
    let body_y = m.top_bar_height;
    let body_h = (window_h - m.top_bar_height - m.footbar_height).max(0.0);
    let left_w = left_zone_width(window_w, s, m);
    let right_w = right_zone_width(window_w, s, m);
    let zones_x = m.icon_rail_width;

    // 左区(收起时宽 0)、分隔线、右区依次排开;一侧收起时另一侧独占 `zones_width`(见上面两个函数)。
    let left_outer = Rect::new(zones_x, body_y, left_w, body_h);
    let divider = Rect::new(zones_x + left_w, body_y, m.divider_width, body_h);
    let right_outer = Rect::new(divider.right(), body_y, right_w, body_h);

    let left_zone = (!s.left_collapsed).then_some(left_outer);
    let right_zone = (!s.right_collapsed).then_some(right_outer);

    let mut left_content = left_zone.map(|z| z.inset(m.left_zone.total()));
    let mut right_content = right_zone.map(|z| z.inset(m.right_zone.total()));
    let mut maximize = None;
    if let Some(side) = s.maximized {
        let b = maximize_box(window_w, window_h, m);
        maximize = Some(b);
        match side {
            Side::Left => {
                left_content = (!s.left_collapsed).then_some(b);
                right_content = None;
            }
            Side::Right => {
                right_content = (!s.right_collapsed).then_some(b);
                left_content = None;
            }
        }
    }
    ShellRects {
        top_bar: Rect::new(0.0, 0.0, window_w, m.top_bar_height),
        left_rail: Rect::new(0.0, body_y, m.icon_rail_width, rail_h),
        right_rail: Rect::new(window_w - m.icon_rail_width, body_y, m.icon_rail_width, rail_h),
        footbar: Rect::new(zones_x, window_h - m.footbar_height, (window_w - 2.0 * m.icon_rail_width).max(0.0), m.footbar_height),
        divider,
        left_zone,
        right_zone,
        left_content,
        right_content,
        maximize_box: maximize,
    }
}

/// 点击落在哪一侧面板区(整区)。落在图标栏、或点在不存在的那一侧时是 `None`,调用方保持原聚焦。
/// 放大态:整个内容区就是放大的那一侧。
pub fn zone_at_x(x: f32, window_w: f32, s: &ZoneState, m: &ChromeMetrics) -> Option<Side> {
    if x < m.icon_rail_width || x > window_w - m.icon_rail_width {
        return None;
    }
    if let Some(side) = s.maximized {
        return Some(side);
    }
    match (s.left_collapsed, s.right_collapsed) {
        (true, true) => None,
        (true, false) => Some(Side::Right),
        (false, true) => Some(Side::Left),
        (false, false) => {
            let boundary = m.icon_rail_width + left_zone_width(window_w, s, m);
            Some(if x < boundary { Side::Left } else { Side::Right })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1440.0;
    const H: f32 = 900.0;

    fn m() -> ChromeMetrics {
        ChromeMetrics::byteboy2077()
    }

    fn open(left_width: f32) -> ZoneState {
        ZoneState { left_collapsed: false, right_collapsed: false, left_width, maximized: None }
    }

    #[test]
    fn both_zones_open_split_the_space_between_the_rails() {
        let r = layout(W, H, &open(640.0), &m());
        assert_eq!(zones_width(W, &m()), 1344.0);
        assert_eq!(r.left_zone, Some(Rect::new(44.0, 40.0, 640.0, 838.0)));
        assert_eq!(r.divider, Rect::new(684.0, 40.0, 8.0, 838.0));
        assert_eq!(r.right_zone, Some(Rect::new(692.0, 40.0, 704.0, 838.0)));
        // 右区右缘正好贴着右图标栏
        assert_eq!(r.right_zone.unwrap().right(), W - 44.0);
        assert_eq!(r.right_rail, Rect::new(1396.0, 40.0, 44.0, 860.0));
        assert_eq!(r.top_bar, Rect::new(0.0, 0.0, W, 40.0));
        assert_eq!(r.footbar, Rect::new(44.0, 878.0, 1352.0, 22.0));
    }

    #[test]
    fn content_rects_subtract_the_zone_margin_and_padding() {
        let r = layout(W, H, &open(640.0), &m());
        // 左: margin top 6 left 6, padding 1 ⇒ x 44+6+1, y 40+6+1, w 640-6-0-2, h 838-6-0-2
        assert_eq!(r.left_content, Some(Rect::new(51.0, 47.0, 632.0, 830.0)));
        // 右: margin top 6 right 6 ⇒ x 692+0+1, w 704-0-6-2
        assert_eq!(r.right_content, Some(Rect::new(693.0, 47.0, 696.0, 830.0)));
    }

    #[test]
    fn a_narrow_window_clamps_the_left_width_and_never_starves_the_right_zone() {
        // 700 宽:zones = 700-96 = 604;上限 = max(604-320, 320) = 320
        assert_eq!(clamp_left_width(700.0, 640.0, &m()), 320.0);
        let r = layout(700.0, H, &open(640.0), &m());
        assert_eq!(r.left_zone.unwrap().w, 320.0);
        assert_eq!(r.right_zone.unwrap().w, 284.0);
        // 极窄:上限被下限垫平,不 panic
        assert_eq!(clamp_left_width(300.0, 640.0, &m()), 320.0);
        // 宽窗口:持久化宽原样保留(夹取只发生在渲染 / 几何时刻,不回写)
        assert_eq!(clamp_left_width(2000.0, 640.0, &m()), 640.0);
        // 太小的持久化宽被抬到下限
        assert_eq!(clamp_left_width(W, 12.0, &m()), 320.0);
    }

    #[test]
    fn collapsing_one_side_gives_the_other_the_whole_space() {
        let s = ZoneState { left_collapsed: true, ..open(640.0) };
        let r = layout(W, H, &s, &m());
        assert_eq!(r.left_zone, None);
        assert_eq!(r.left_content, None);
        // 分隔线仍在图标栏之后,右区从分隔线之后起,独占 zones_width
        assert_eq!(r.divider, Rect::new(44.0, 40.0, 8.0, 838.0));
        assert_eq!(r.right_zone, Some(Rect::new(52.0, 40.0, 1344.0, 838.0)));

        let s = ZoneState { right_collapsed: true, ..open(640.0) };
        let r = layout(W, H, &s, &m());
        assert_eq!(r.right_zone, None);
        assert_eq!(r.left_zone, Some(Rect::new(44.0, 40.0, 1344.0, 838.0)));
        assert_eq!(r.divider.x, 1388.0);
        assert_eq!(r.divider.right(), W - 44.0);
    }

    #[test]
    fn both_collapsed_leaves_no_zone() {
        let s = ZoneState { left_collapsed: true, right_collapsed: true, ..open(640.0) };
        let r = layout(W, H, &s, &m());
        assert_eq!((r.left_zone, r.right_zone, r.left_content, r.right_content), (None, None, None, None));
    }

    #[test]
    fn maximizing_one_side_hides_the_other_and_fills_the_box() {
        let s = ZoneState { maximized: Some(Side::Left), ..open(640.0) };
        let r = layout(W, H, &s, &m());
        let b = Rect::new(84.0, 80.0, 1272.0, 780.0);
        assert_eq!(r.maximize_box, Some(b));
        assert_eq!(r.left_content, Some(b));
        assert_eq!(r.right_content, None, "the covered side must report no content so its native view is hidden");
        let s = ZoneState { maximized: Some(Side::Right), ..open(640.0) };
        let r = layout(W, H, &s, &m());
        assert_eq!(r.right_content, Some(b));
        assert_eq!(r.left_content, None);
    }

    #[test]
    fn tiny_windows_never_produce_negative_sizes() {
        let r = layout(50.0, 50.0, &open(640.0), &m());
        for rect in [r.top_bar, r.left_rail, r.right_rail, r.footbar, r.divider].into_iter().chain([r.left_zone, r.right_zone, r.left_content, r.right_content].into_iter().flatten()) {
            assert!(rect.w >= 0.0 && rect.h >= 0.0, "{rect:?}");
        }
        assert!(maximize_box(50.0, 50.0, &m()).w >= 0.0);
    }

    #[test]
    fn a_click_lands_in_the_zone_under_it() {
        let s = open(640.0);
        assert_eq!(zone_at_x(10.0, W, &s, &m()), None, "on the left rail");
        assert_eq!(zone_at_x(W - 10.0, W, &s, &m()), None, "on the right rail");
        assert_eq!(zone_at_x(300.0, W, &s, &m()), Some(Side::Left));
        assert_eq!(zone_at_x(900.0, W, &s, &m()), Some(Side::Right));
        assert_eq!(zone_at_x(683.9, W, &s, &m()), Some(Side::Left));
        assert_eq!(zone_at_x(684.0, W, &s, &m()), Some(Side::Right));
        let s = ZoneState { left_collapsed: true, ..open(640.0) };
        assert_eq!(zone_at_x(100.0, W, &s, &m()), Some(Side::Right));
        let s = ZoneState { left_collapsed: true, right_collapsed: true, ..open(640.0) };
        assert_eq!(zone_at_x(100.0, W, &s, &m()), None);
        let s = ZoneState { maximized: Some(Side::Right), ..open(640.0) };
        assert_eq!(zone_at_x(100.0, W, &s, &m()), Some(Side::Right));
    }
}
