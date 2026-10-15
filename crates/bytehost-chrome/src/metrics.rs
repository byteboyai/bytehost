//! 外壳度量:几何函数只读这一份数,控件也读它,不允许各自写常量。
//! 纯数据,不依赖 byteui / iced,所以几何可以在没有窗口的测试里验证。

/// 四边距,字段顺序与 iced `Padding` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Insets {
    pub const ZERO: Self = Self { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };

    pub const fn new(top: f32, right: f32, bottom: f32, left: f32) -> Self {
        Self { top, right, bottom, left }
    }

    pub const fn uniform(v: f32) -> Self {
        Self::new(v, v, v, v)
    }

    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }

    pub fn plus(&self, other: Insets) -> Insets {
        Insets::new(
            self.top + other.top,
            self.right + other.right,
            self.bottom + other.bottom,
            self.left + other.left,
        )
    }
}

/// 一个面板区外框的留白:`margin` 在外框之外(悬浮留白),`padding` 在外框之内。
/// 面板内容矩形 = 外框 − margin − padding。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneInsets {
    pub margin: Insets,
    pub padding: Insets,
}

impl ZoneInsets {
    pub fn total(&self) -> Insets {
        self.margin.plus(self.padding)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeMetrics {
    pub top_bar_height: f32,
    pub footbar_height: f32,
    pub icon_rail_width: f32,
    pub divider_width: f32,
    pub min_zone_width: f32,
    pub min_split_ratio: f32,
    pub max_split_ratio: f32,
    /// 放大态:遮罩到金色描边盒子的内边距。
    pub maximize_padding: f32,
    pub left_zone: ZoneInsets,
    pub right_zone: ZoneInsets,
}

impl ChromeMetrics {
    /// ByteBoy2077 主题下的数值,与 `byteui::theme::geometry::GeometryTokens::byteboy2077()` 和
    /// Dozer `assets/theme/workspace.json` 的 `left_zone` / `right_zone` 一致。仅供测试与文档对照;
    /// 运行时一律从 byteui 读(见 `theme::metrics_from_byteui`)。
    pub const fn byteboy2077() -> Self {
        Self {
            top_bar_height: 40.0,
            footbar_height: 22.0,
            icon_rail_width: 44.0,
            divider_width: 8.0,
            min_zone_width: 320.0,
            min_split_ratio: 0.2,
            max_split_ratio: 0.8,
            maximize_padding: 40.0,
            left_zone: ZoneInsets { margin: Insets::new(6.0, 0.0, 0.0, 6.0), padding: Insets::uniform(1.0) },
            right_zone: ZoneInsets { margin: Insets::new(6.0, 6.0, 0.0, 0.0), padding: Insets::uniform(1.0) },
        }
    }
}
