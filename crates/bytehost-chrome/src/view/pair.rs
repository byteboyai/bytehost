use iced_widget::core::Length;
use iced_widget::{container, row};

use super::{BodyArea, El};
use crate::model::Divider;

/// 配对视图的 `FillPortion` 权重。`FillPortion` 在扣掉中间固定宽的分隔线之后按权重分剩余空间,
/// 与几何里的 `pair_w = content.w - divider_width` 同源。
pub fn split_portions(split: f32) -> (u16, u16) {
    let first = (split * 10_000.0).round() as u16;
    let second = ((1.0 - split) * 10_000.0).round() as u16;
    (first, second)
}

/// 把配对面板的两个 slot 排成"列表 | 分隔线 | 内容"(或镜像)。第一个 slot 收起时只渲染第二个,占满整个内容区。
///
/// 两块的宽度由它们**自己的外层容器**声明成 `FillPortion`,不再套一层包装容器:`Limits::width(Fixed(w))` 会把子元素的
/// min/max 都钉成 `w`,父级的 `FillPortion` 只约束包装容器本身、传不进子元素,曾导致两块 pane 全部以 0 宽布局。
pub fn pair_view<'a, M: Clone + 'a>(
    area: &BodyArea,
    first: El<'a, M>,
    second: El<'a, M>,
    on_divider_press: M,
    first_bg: iced_widget::core::Color,
    second_bg: iced_widget::core::Color,
) -> El<'a, M> {
    if area.collapsed {
        return container(second)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    let (p_first, p_second) = split_portions(area.split);
    let first = container(first)
        .width(Length::FillPortion(p_first))
        .height(Length::Fill);
    let second = container(second)
        .width(Length::FillPortion(p_second))
        .height(Length::Fill);
    // 第一个 slot 此刻是否渲染在左边:几何里已经按镜像算好,直接读,不重复推导。
    let first_on_left = area.pair.map(|p| p.first.x <= p.second.x).unwrap_or(true);
    if first_on_left {
        row![
            first,
            super::divider_bar(
                Divider::Pair(area.side),
                first_bg,
                second_bg,
                on_divider_press
            ),
            second
        ]
        .height(Length::Fill)
        .into()
    } else {
        row![
            second,
            super::divider_bar(
                Divider::Pair(area.side),
                second_bg,
                first_bg,
                on_divider_press
            ),
            first
        ]
        .height(Length::Fill)
        .into()
    }
}
