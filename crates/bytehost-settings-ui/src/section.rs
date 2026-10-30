//! 共享小部件:标题、说明文字、操作按钮、分隔线与可滚动内容。

use iced::widget::{Space, button, container, scrollable, text};
use iced::{Element, Length};

use byteui::theme::{color, font};

/// 页面标题。
pub fn heading<'a, M: 'a>(label: impl Into<String>) -> Element<'a, M> {
    text(label.into())
        .size(font::subtitle())
        .color(color::current().cream)
        .into()
}

/// 次要说明文字。
pub fn dim<'a>(label: impl Into<String>) -> iced::widget::Text<'a> {
    text(label.into())
        .size(font::label())
        .color(color::current().dim)
}

/// 一行错误 / 警告文字。
pub fn warning<'a, M: 'a>(label: impl Into<String>, tint: iced::Color) -> Element<'a, M> {
    text(label.into()).size(font::label()).color(tint).into()
}

/// 弹窗风格的操作按钮。
pub fn action_button<'a, M: Clone + 'a>(
    label: impl Into<String>,
    message: M,
    tint: iced::Color,
) -> Element<'a, M> {
    button(text(label.into()).size(font::body()).color(tint))
        .padding([4, 12])
        .on_press(message)
        .style(byteui::feedback::dialog::action_button_style(tint))
        .into()
}

/// 右侧对齐的操作行(取消在左、确认在右)。
pub fn actions_row<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    container(content)
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .into()
}

/// 竖向 1px 分隔线。
pub fn separator<'a, M: 'a>() -> Element<'a, M> {
    container(Space::new())
        .width(Length::Fixed(1.0))
        .height(Length::Fill)
        .style(|_t: &iced::Theme| container::Style {
            background: Some(color::current().border.into()),
            ..container::Style::default()
        })
        .into()
}

/// 内容区滚动容器:统一滚动条样式,撑满可用空间。
pub fn scrollable_body<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    scrollable(content)
        .direction(scrollable::Direction::Vertical(
            byteui::interaction::scrollbar::scrollbar(),
        ))
        .style(|_t, _s| byteui::interaction::scrollbar::scrollbar_style())
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
