use bytehost_core::id::PanelId;
use iced_widget::core::Length;
use iced_widget::{MouseArea, column, container, row};

use super::{BodyArea, El, ShellHost};
use crate::metrics::ChromeMetrics;
use crate::model::ShellMsg;

/// 放大态浮层:两条图标栏之间、顶栏之下的整个内容区变暗,放大的那一侧内容占满并描金边。点变暗区域退出放大。
///
/// `bordered` 显式给 `Length::Fill`:iced 0.14 的 `Limits` 有 compression 传染机制,`Shrink` 容器包 `Fill` 子元素时
/// Fill 会退化成贴着内容收缩,金色描边就会贴着一小块内容而不是撑满放大区域。
pub(super) fn maximize_overlay<'a, M, H>(host: &'a H, panel: &PanelId, area: &BodyArea, m: &ChromeMetrics) -> El<'a, M>
where
    M: Clone + 'static,
    H: ShellHost<M>,
{
    let style = byteui::theme::region::maximize_overlay();
    let inner = host.body(panel, area);
    let bordered = container(inner).width(Length::Fill).height(Length::Fill).style(move |_t: &iced_widget::Theme| container::Style { border: style.border, ..container::Style::default() });
    // 放大内容自己再罩一层"吃掉点击 / 滚轮"的 MouseArea:iced 事件子先父后,内容里真正可交互的控件会先 capture,
    // 到得了这一层的正是"不消费点击的地方",不吃掉就会穿到外层遮罩的"退出放大",一点正文就退出。
    let noop = H::lift(ShellMsg::Noop);
    let scroll_noop = noop.clone();
    let guard = MouseArea::new(bordered).on_press(noop).on_scroll(move |_delta| scroll_noop.clone());
    let dim = MouseArea::new(container(guard).padding(style.scrim_padding).width(Length::Fill).height(Length::Fill).style(move |_t: &iced_widget::Theme| container::Style { background: Some(style.scrim_background.into()), ..container::Style::default() }))
        .on_press(H::lift(ShellMsg::MaximizeClose));
    // 顶部垫一条顶栏高的透明 Space,把遮罩钉在顶栏之下;两侧垫图标栏宽,让变暗区域精确对齐 body 的渲染范围。
    column![
        iced_widget::Space::new().height(Length::Fixed(m.top_bar_height)),
        row![iced_widget::Space::new().width(Length::Fixed(m.icon_rail_width)), dim, iced_widget::Space::new().width(Length::Fixed(m.icon_rail_width))],
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
