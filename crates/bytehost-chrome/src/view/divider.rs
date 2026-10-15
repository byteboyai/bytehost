use iced_widget::core::{Color, Length, mouse};
use iced_widget::{MouseArea, container, row};

use super::El;
use crate::model::Divider;

/// 分隔线:命中区 `divider_width` 宽、`Fill` 高,中间一条 2px 边框色竖线。
///
/// 配对视图内部:`left_bg` / `right_bg` 是分隔线两侧紧贴的 pane 底色,命中区左右两半分别填上这两色,只留中间
/// 2px 竖线——否则 8px 命中区是透明的,会露出 zone 的底色,在两块 pane 之间顶出一条浅色"沟"。
/// `Divider::Zones` 不画竖线也不填色:它两侧各自套着整体外框,这条缝是故意空出来给两侧圆角边框收边的。
///
/// `on_press` 只发起拖拽;持续追踪交给全局的 `CursorMoved` / `PointerReleased`(`MouseArea` 的 `on_move` 要求光标
/// 不离开这条窄带,快速拖拽会断)。
pub fn divider_bar<'a, M: Clone + 'a>(kind: Divider, left_bg: Color, right_bg: Color, on_press: M) -> El<'a, M> {
    let width = byteui::theme::geometry::divider_width();
    let body: El<'a, M> = if matches!(kind, Divider::Zones) {
        iced_widget::Space::new().width(Length::Fixed(width)).height(Length::Fill).into()
    } else {
        let line_w = 2.0_f32;
        let side_w = (width - line_w) / 2.0;
        let fill = |c: Color| {
            container(iced_widget::Space::new()).width(Length::Fixed(side_w)).height(Length::Fill).style(move |_t: &iced_widget::Theme| container::Style { background: Some(c.into()), ..container::Style::default() })
        };
        let line = container(iced_widget::Space::new())
            .width(Length::Fixed(line_w))
            .height(Length::Fill)
            .style(|_t: &iced_widget::Theme| container::Style { background: Some(byteui::theme::color::current().border.into()), ..container::Style::default() });
        row![fill(left_bg), line, fill(right_bg)].width(Length::Fixed(width)).height(Length::Fill).into()
    };
    MouseArea::new(body).interaction(mouse::Interaction::ResizingColumn).on_press(on_press).into()
}
