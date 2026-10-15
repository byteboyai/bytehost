use iced_widget::core::{Border, Length, Padding};
use iced_widget::{column, container};

use super::{BodyArea, El, ShellHost};
use crate::geometry::Side;
use crate::metrics::ChromeMetrics;
use crate::model::ShellModel;

/// 一侧的面板区。收起时渲染成空元素;非放大态带整体外框(`left_zone` / `right_zone`),本区拿到焦点时描金边。
///
/// 左区在两侧都展开时宽度是 `Fixed(有效左宽)`,对侧收起时 `Fill`(独占);右区恒 `Fill`。
/// 这与几何函数 `left_zone_width` / `right_zone_width` 的规则一一对应——控件和几何不得各自推导。
pub(super) fn zone_area<'a, M, H>(model: &'a ShellModel, host: &'a H, side: Side, area: Option<BodyArea>, m: &ChromeMetrics) -> El<'a, M>
where
    M: Clone + 'static,
    H: ShellHost<M>,
{
    let other = side.other();
    if model.collapsed(side) {
        // 左收起:对侧也收起时占满(空),否则缩成零宽,让右区拿走全部空间。
        return if side == Side::Left && model.collapsed(other) { iced_widget::space::horizontal().into() } else { column![].into() };
    }
    let Some(area) = area else { return column![].into() };
    let inner = host.body(model.selected(side), &area);

    let region = match side {
        Side::Left => byteui::theme::region::left_zone(),
        Side::Right => byteui::theme::region::right_zone(),
    };
    let focused = model.active_zone() == Some(side);
    let base_border = region.border.unwrap_or_default();
    let zone_box = container(inner).width(Length::Fill).height(Length::Fill).padding(region.padding).clip(true).style(move |_t: &iced_widget::Theme| container::Style {
        background: region.background.map(Into::into),
        border: if focused { Border { color: byteui::theme::color::current().gold, width: 2.0, radius: base_border.radius } } else { base_border },
        ..container::Style::default()
    });

    let width = match side {
        Side::Left if !model.collapsed(Side::Right) => Length::Fixed(model.effective_left_width(m)),
        _ => Length::Fill,
    };
    let mg = region.margin;
    container(zone_box).width(width).height(Length::Fill).padding(Padding { top: mg.top, right: mg.right, bottom: mg.bottom, left: mg.left }).into()
}
