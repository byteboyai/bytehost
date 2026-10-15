use iced_widget::core::{Alignment, Length};
use iced_widget::{MouseArea, container, row, stack};

use super::El;
use crate::metrics::ChromeMetrics;

/// 顶栏三个槽位:左侧标题(恒在最左,不参与收窄)、中间页签行(占满剩余空间)、右侧按钮。
pub struct TopBarSlots<'a, M> {
    pub title: El<'a, M>,
    pub tabs: El<'a, M>,
    pub right: El<'a, M>,
}

/// 顶栏外壳:固定 `top_bar_height` 高,区域样式 `top_bar`(左内边距 78 给红绿灯让位)。
/// 双击顶栏空白处缩放窗口:背景 `MouseArea` 放在 `bar` 下面一层,真正的按钮 / 页签会先吃掉落在自己身上的点击,
/// 双击只有落在没有任何控件的空白处才会穿透到这层背景。
pub(super) fn top_bar<'a, M: Clone + 'a>(slots: TopBarSlots<'a, M>, on_double_click: M, m: &ChromeMetrics) -> El<'a, M> {
    let region = byteui::theme::region::top_bar();
    let tabs = container(slots.tabs).width(Length::Fill);
    let bar = row![slots.title, tabs, slots.right].spacing(region.gap).padding(region.padding).height(Length::Fixed(m.top_bar_height)).align_y(Alignment::Center);
    let background = MouseArea::new(container(iced_widget::Space::new()).width(Length::Fill).height(Length::Fill)).on_double_click(on_double_click);
    container(stack![background, bar])
        .width(Length::Fill)
        .height(Length::Fixed(m.top_bar_height))
        .style(move |_t: &iced_widget::Theme| container::Style { background: region.background.map(Into::into), border: region.border.unwrap_or_default(), ..container::Style::default() })
        .into()
}
