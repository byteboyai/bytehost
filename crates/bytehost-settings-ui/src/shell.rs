//! 共享设置容器:标题、左栏导航、右侧内容、分隔线与关闭操作。
//!
//! 容器只画共享骨架;当前页内容、导航消息与关闭消息全部由产品注入。

use bytehost_settings::{PageDescriptor, PageId};
use byteui::interaction::icons::{self, IconKind};
use byteui::theme::{color, font, icon_size};
use iced::widget::{MouseArea, Space, button, column, container, row, text};
use iced::{Alignment, Border, Element, Length, Padding};

use crate::section;
use crate::text::Text;

/// 斜体参数(全部由产品决定)。
pub struct Shell<'a, M> {
    /// 有序页面(注册顺序即导航顺序)。
    pub pages: &'a [PageDescriptor],
    pub current: Option<&'a PageId>,
    /// 右侧内容;`pages` 为空时忽略。
    pub content: Element<'a, M>,
    /// 点导航页时产生的产品消息。
    pub on_select: &'a dyn Fn(&PageId) -> M,
    /// 关闭消息(Esc / 关闭按钮共用)。
    pub on_close: M,
    /// 关闭图标 hover 进入 / 离开;不关心时传 `None`。
    pub on_close_hover: Option<&'a dyn Fn(bool) -> M>,
    pub close_hovered: bool,
    pub text: Text<'a>,
    /// 内容是否由容器包一层竖向滚动。自带滚动(例如应用页)时传 `false`。
    pub scroll_content: bool,
    /// 卡片宽度;`Length::Fill` 表示填满产品给的窗口。
    pub width: Length,
}

/// 渲染完整设置卡片。
pub fn view<'a, M: Clone + 'a>(shell: Shell<'a, M>) -> Element<'a, M> {
    let colors = color::current();
    let title = row![
        icons::view(IconKind::Settings, font::title() as f32, colors.cream),
        text(shell.text.get("settings.title", "Settings"))
            .size(font::title())
            .color(colors.cream),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let close = close_icon(
        shell.on_close.clone(),
        shell.on_close_hover,
        shell.close_hovered,
    );
    let header = row![title, Space::new().width(Length::Fill), close].align_y(Alignment::Center);

    let nav = column(
        shell
            .pages
            .iter()
            .map(|page| {
                let label = page
                    .text_key
                    .as_deref()
                    .map(|key| shell.text.get(key, &page.fallback))
                    .unwrap_or_else(|| page.fallback.clone());
                page_button(
                    label,
                    shell.current == Some(&page.id),
                    (shell.on_select)(&page.id),
                )
            })
            .collect::<Vec<_>>(),
    )
    .spacing(8)
    .width(Length::Fill);

    let content: Element<'a, M> = if shell.pages.is_empty() {
        section::dim(
            shell
                .text
                .get("settings.empty", "No settings pages are available."),
        )
        .into()
    } else if shell.scroll_content {
        section::scrollable_body(shell.content)
    } else {
        shell.content
    };

    let main: Element<'a, M> = if shell.pages.is_empty() {
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        row![
            container(nav).width(Length::Fixed(140.0)),
            section::separator(),
            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                    left: 16.0,
                }),
        ]
        .spacing(16)
        .align_y(Alignment::Start)
        .height(Length::Fill)
        .into()
    };

    let close_row = section::actions_row(section::action_button(
        shell.text.get("settings.close", "Close"),
        shell.on_close,
        colors.dim,
    ));

    let body = column![header, main, close_row].spacing(14);

    container(body)
        .padding(16)
        .width(shell.width)
        .height(Length::Fill)
        .style(byteui::feedback::dialog::card_style)
        .into()
}

fn close_icon<'a, M: Clone + 'a>(
    on_close: M,
    on_hover: Option<&'a dyn Fn(bool) -> M>,
    hovered: bool,
) -> Element<'a, M> {
    let colors = color::current();
    let icon = icons::view(IconKind::X, icon_size::row(), colors.dim);
    let btn = button(icon).padding(6).on_press(on_close).style(
        move |_t: &iced::Theme, status: button::Status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed if hovered => Some(
                    iced::Color {
                        a: 0.15,
                        ..colors.gold
                    }
                    .into(),
                ),
                _ => None,
            },
            border: Border::default(),
            ..button::Style::default()
        },
    );
    match on_hover {
        Some(map) => MouseArea::new(btn)
            .on_enter(map(true))
            .on_exit(map(false))
            .interaction(iced::mouse::Interaction::Pointer)
            .into(),
        None => btn.into(),
    }
}

fn page_button<'a, M: Clone + 'a>(label: String, active: bool, message: M) -> Element<'a, M> {
    let colors = color::current();
    let cell = container(text(label).size(font::caption()).color(if active {
        colors.cream
    } else {
        colors.dim
    }))
    .padding([8, 12])
    .width(Length::Fill)
    .style(move |_t: &iced::Theme| container::Style {
        background: Some(if active { colors.card } else { colors.bg }.into()),
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    });
    MouseArea::new(cell)
        .on_press(message)
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}
