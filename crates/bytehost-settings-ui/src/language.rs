//! 语言页:通用 `SelectionState<T>` + 产品提供的选项。
//!
//! 共享层不解析 revision、不调用服务、不导入产品词表;保存中的重复提交在状态层被忽略。

use bytehost_settings::{SaveStatus, SelectionState};
use byteui::theme::color;
use iced::widget::{MouseArea, column, row, text};
use iced::{Alignment, Element, Length};

use crate::text::Text;

/// 一个语言选项。`value` 是产品自己的偏好类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageOption<T> {
    pub value: T,
    pub key: &'static str,
    pub fallback: &'static str,
}

impl<T> LanguageOption<T> {
    pub fn new(value: T, key: &'static str, fallback: &'static str) -> Self {
        Self {
            value,
            key,
            fallback,
        }
    }
}

/// 语言页视图。
///
/// - 已选态取 `state.shown()`(乐观值优先);
/// - `Saving` / `Unsupported` 时选项禁用;
/// - 失败 / 冲突 / 不支持在选项下方显示一行说明,失败保留待选值可重试。
pub fn view<'a, M: Clone + 'a, T: Clone + PartialEq + 'a>(
    state: &SelectionState<T>,
    options: &[LanguageOption<T>],
    text_: Text<'a>,
    on_select: &'a dyn Fn(T) -> M,
    extra: Vec<Element<'a, M>>,
) -> Element<'a, M> {
    let colors = color::current();
    let enabled = state.can_select();
    let shown = state.shown().clone();

    let mut col = column![
        text(text_.get("settings.language.title", "Language"))
            .size(byteui::theme::font::subtitle())
            .color(colors.cream)
    ]
    .spacing(14);

    let mut options_col = column![].spacing(6);
    for option in options {
        let selected = option.value == shown;
        let label = text_.get(option.key, option.fallback);
        options_col = options_col.push(option_row(
            label,
            selected,
            enabled,
            if enabled {
                Some((on_select)(option.value.clone()))
            } else {
                None
            },
            colors,
        ));
    }
    col = col.push(options_col);

    if let Some((line, tint)) = status_line(state.status(), text_, colors) {
        col = col.push(text(line).size(byteui::theme::font::caption()).color(tint));
    }
    for element in extra {
        col = col.push(element);
    }
    col.into()
}

fn status_line<'a>(
    status: &SaveStatus,
    text_: Text<'a>,
    colors: color::ColorTokens,
) -> Option<(String, iced::Color)> {
    match status {
        SaveStatus::Idle => None,
        SaveStatus::Saving => Some((text_.get("settings.language.saving", "Saving…"), colors.dim)),
        SaveStatus::Failed(_) => Some((
            text_.get("settings.language.save_failed", "Couldn't save. Try again."),
            colors.gold,
        )),
        SaveStatus::Conflict => Some((
            text_.get(
                "settings.language.conflict",
                "Language changed elsewhere — review your choice.",
            ),
            colors.gold,
        )),
        SaveStatus::Unsupported => Some((
            text_.get(
                "settings.language.unsupported",
                "This service doesn't support language settings.",
            ),
            colors.dim,
        )),
    }
}

fn option_row<'a, M: Clone + 'a>(
    label: String,
    selected: bool,
    enabled: bool,
    message: Option<M>,
    colors: color::ColorTokens,
) -> Element<'a, M> {
    let tint = if !enabled {
        colors.dim
    } else if selected {
        colors.gold
    } else {
        colors.body
    };
    let mark = text(if selected { "✓" } else { " " })
        .size(byteui::theme::font::body())
        .color(tint);
    let label = text(label).size(byteui::theme::font::body()).color(tint);
    let content = row![mark, label, iced::widget::Space::new().width(Length::Fill)]
        .spacing(8)
        .align_y(Alignment::Center);
    match message {
        Some(message) => MouseArea::new(content)
            .on_press(message)
            .interaction(iced::mouse::Interaction::Pointer)
            .into(),
        None => content.into(),
    }
}
