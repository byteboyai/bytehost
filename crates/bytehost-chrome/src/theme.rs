//! 把 byteui 的运行时主题读成一份 [`ChromeMetrics`]。几何函数只读这份数,控件读的是同一批 byteui 来源。

use crate::metrics::{ChromeMetrics, Insets, ZoneInsets};

fn insets(p: iced_widget::core::Padding) -> Insets {
    Insets::new(p.top, p.right, p.bottom, p.left)
}

fn zone(style: byteui::theme::region::RegionStyle) -> ZoneInsets {
    ZoneInsets { margin: insets(style.margin), padding: insets(style.padding) }
}

/// 当前主题下的度量(已含全局 UI scale)。运行时每次需要几何时现读,这样 Ctrl +/- 缩放后几何与控件一起变。
pub fn metrics_from_theme() -> ChromeMetrics {
    use byteui::theme::{geometry as g, region as r};
    ChromeMetrics {
        top_bar_height: g::top_bar_height(),
        footbar_height: g::footbar_height(),
        icon_rail_width: g::icon_rail_width(),
        divider_width: g::divider_width(),
        min_zone_width: g::min_zone_width(),
        min_split_ratio: g::min_split_ratio(),
        max_split_ratio: g::max_split_ratio(),
        maximize_padding: g::maximize_overlay_padding(),
        left_zone: zone(r::left_zone()),
        right_zone: zone(r::right_zone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ChromeMetrics::byteboy2077()` 是几何测试用的数字;它必须与 byteui 默认主题读出来的一致,否则几何测试在测一个
    /// 运行时不存在的世界。(本 crate 的测试不改 byteui 的进程级全局,所以这里读到的是默认值。)
    #[test]
    fn the_test_constants_match_what_the_default_byteui_theme_reads() {
        assert_eq!(metrics_from_theme(), ChromeMetrics::byteboy2077());
    }
}
