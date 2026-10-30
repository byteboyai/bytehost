//! 主题页:只读当前方案、输出选择事件;不读写文件、不调用 `set_scheme`。

use byteui::theme::color::{ColorScheme, current_scheme};
use iced::widget::{MouseArea, column, row, text};
use iced::{Alignment, Element, Length};

use crate::text::Text;

/// 暗色主题。
pub const DARK: ThemeOption = ThemeOption {
    scheme: ColorScheme::Dark,
    key: "settings.theme.dark",
    fallback: "Dark · ByteBoy2077",
};

/// 亮色主题。
pub const LIGHT: ThemeOption = ThemeOption {
    scheme: ColorScheme::Light,
    key: "settings.theme.light",
    fallback: "Light · ByteBoy2077-Light",
};

pub const ALL: [ThemeOption; 2] = [DARK, LIGHT];

/// 一个可选项。产品可以按自己的能力裁剪集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeOption {
    pub scheme: ColorScheme,
    pub key: &'static str,
    pub fallback: &'static str,
}

/// 主题页视图。
///
/// `current` 由产品传入(通常是 `byteui::theme::color::current_scheme()`,但产品也可以
/// 传自己的权威值);共享层不自行读取全局状态。
pub fn view<'a, M: Clone + 'a>(
    current: ColorScheme,
    options: &[ThemeOption],
    text_: Text<'a>,
    on_select: &'a dyn Fn(ColorScheme) -> M,
    extra: Vec<Element<'a, M>>,
) -> Element<'a, M> {
    let colors = byteui::theme::color::current();
    let mut col = column![
        text(text_.get("settings.theme.title", "Theme"))
            .size(byteui::theme::font::subtitle())
            .color(colors.cream)
    ]
    .spacing(14);

    for option in options {
        col = col.push(option_row(
            text_.get(option.key, option.fallback),
            option.scheme == current,
            (on_select)(option.scheme),
            colors,
        ));
    }
    for element in extra {
        col = col.push(element);
    }
    col.into()
}

/// 默认使用 `byteui` 当前方案。
pub fn view_current<'a, M: Clone + 'a>(
    options: &[ThemeOption],
    text_: Text<'a>,
    on_select: &'a dyn Fn(ColorScheme) -> M,
) -> Element<'a, M> {
    view(current_scheme(), options, text_, on_select, Vec::new())
}

fn option_row<'a, M: Clone + 'a>(
    label: String,
    selected: bool,
    message: M,
    colors: byteui::theme::color::ColorTokens,
) -> Element<'a, M> {
    let mark = text(if selected { "✓" } else { " " })
        .size(byteui::theme::font::body())
        .color(if selected { colors.gold } else { colors.body });
    let label = text(label)
        .size(byteui::theme::font::body())
        .color(if selected { colors.gold } else { colors.body });
    let content = row![mark, label, iced::widget::Space::new().width(Length::Fill)]
        .spacing(8)
        .align_y(Alignment::Center);
    MouseArea::new(content)
        .on_press(message)
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}
