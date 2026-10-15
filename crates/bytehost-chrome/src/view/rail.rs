use bytehost_core::id::PanelId;
use byteui::interaction::icons;
use iced_widget::core::{Border, Length, Padding, alignment, mouse};
use iced_widget::{MouseArea, Stack, container};

use super::{El, ShellHost};
use crate::geometry::Side;
use crate::metrics::ChromeMetrics;
use crate::model::{ShellModel, ShellMsg};

/// 图标栏:按 `model.rail(side)` 的顺序渲染。左右两条栏共用这一份实现,差异(区域样式、选中态取哪一侧)
/// 通过 `side` 分派。每个按钮占一个绝对定位的 `Stack` 图层,纵向偏移按动画槽位号换算,换位时相邻按钮平滑让位。
pub(super) fn icon_rail<'a, M, H>(
    model: &'a ShellModel,
    host: &'a H,
    side: Side,
    m: &ChromeMetrics,
) -> El<'a, M>
where
    M: Clone + 'static,
    H: ShellHost<M>,
{
    let region = match side {
        Side::Left => byteui::theme::region::left_icon_rail(),
        Side::Right => byteui::theme::region::right_icon_rail(),
    };
    // 视觉"选中" = 该侧选中它 **且**面板区展开。点已选中的图标会收起面板区,此时图标退回未选中态。
    let active = model.selected(side).clone();
    let open = !model.collapsed(side);
    let button_size = byteui::theme::geometry::rail_button_size();
    let step = button_size + region.gap;
    let confirmed = model.rail_drag_confirmed();
    let dragged = model.dragged_panel();

    let mut layers: Vec<El<'a, M>> = Vec::new();
    for (idx, panel) in model.rail(side).iter().enumerate() {
        let Some(meta) = host.meta(panel) else {
            continue;
        };
        let press = H::lift(ShellMsg::PanelPress {
            panel: panel.clone(),
        });
        let hover_panel = panel.clone();
        // `interactive: false`:选中走外层 `rail_drag_surface` 的 `MouseArea::on_press`(按下瞬间触发),
        // 不走内层 `button::on_press`(松手才触发,武装拖拽的时机就晚了)。
        let base: El<'a, M> = icons::icon_button_entry(
            meta.icon,
            byteui::theme::icon_size::rail(),
            *panel == active && open,
            confirmed && dragged.as_ref() == Some(panel),
            model.hover_progress(panel),
            true,
            button_size,
            false,
            press.clone(),
            move |hovered| {
                H::lift(ShellMsg::Hover {
                    panel: hover_panel.clone(),
                    hovered,
                })
            },
            meta.title,
        );
        let y = region.padding.top + model.slot_position(side, panel, idx) * step;
        layers.push(positioned(
            drag_surface::<M, H>(base, side, idx, confirmed, press),
            y,
            &region,
        ));
    }

    // 补一块"末尾"命中区:每个按钮的 `on_move` 只挂在按钮自己的矩形上,悬停能上报的下标上限永远是
    // len - 1,跨栏拖拽永远到不了真正的末位;这块透明区从最后一个按钮往下铺满,上报下标固定为 len。
    let tail_index = model.rail(side).len();
    let tail = MouseArea::new(
        iced_widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_move(move |_| {
        H::lift(ShellMsg::RailDragMove {
            side,
            index: tail_index,
        })
    });
    layers.push(positioned(
        tail.into(),
        region.padding.top + tail_index as f32 * step,
        &region,
    ));

    // 拖拽悬停期间在预计插入点画一条金色插入线(同栏重排 = 已生效的顺序;跨栏悬停 = 将要落的位置),只在越过阈值后画。
    let insertion = model
        .rail_drag()
        .filter(|_| confirmed)
        .and_then(|d| match d.pending_cross {
            Some((cross_side, idx)) => (cross_side == side).then_some(idx),
            None => (d.source_side == side).then_some(d.source_index),
        });
    if let Some(idx) = insertion {
        let bar_h = 3.0;
        let y = (region.padding.top + idx as f32 * step - region.gap / 2.0 - bar_h / 2.0).max(0.0);
        let marker = container(iced_widget::Space::new())
            .width(Length::Fixed(button_size))
            .height(Length::Fixed(bar_h))
            .style(|_t: &iced_widget::Theme| container::Style {
                background: Some(byteui::theme::color::current().gold.into()),
                ..container::Style::default()
            });
        layers.push(positioned(marker.into(), y, &region));
    }

    let _ = m;
    container(
        Stack::with_children(layers)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fixed(byteui::theme::geometry::icon_rail_width()))
    .height(Length::Fill)
    .style(move |_t: &iced_widget::Theme| container::Style {
        background: region.background.map(Into::into),
        border: region.border.unwrap_or_default(),
        ..container::Style::default()
    })
    .into()
}

fn positioned<'a, M: 'a>(
    content: El<'a, M>,
    y: f32,
    region: &byteui::theme::region::RegionStyle,
) -> El<'a, M> {
    container(content)
        .padding(Padding {
            top: y,
            left: region.padding.left,
            right: region.padding.right,
            bottom: 0.0,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Left)
        .align_y(alignment::Vertical::Top)
        .into()
}

/// 给一个图标栏按钮包上"拖拽换栏 / 换位"的感应层。这一层是按钮**唯一**的选中 / 拖拽入口:
/// `MouseArea::on_press`(按下瞬间触发)武装拖拽;`on_move`:光标移到这个按钮上时上报悬停下标,没在拖拽时是无害的 no-op。
fn drag_surface<'a, M, H>(
    content: El<'a, M>,
    side: Side,
    index: usize,
    armed: bool,
    on_press: M,
) -> El<'a, M>
where
    M: Clone + 'static,
    H: ShellHost<M>,
{
    let area = MouseArea::new(content)
        .on_press(on_press)
        .on_move(move |_| H::lift(ShellMsg::RailDragMove { side, index }));
    if armed {
        area.interaction(mouse::Interaction::Grabbing).into()
    } else {
        area.into()
    }
}

/// 图标栏拖拽期间跟随光标的幽灵图标:圆角方卡 + 金框,里面是被拖面板的图标,整体以光标为中心悬浮,纯展示。
pub(super) fn rail_drag_ghost<'a, M, H>(model: &'a ShellModel, host: &'a H) -> El<'a, M>
where
    M: Clone + 'static,
    H: ShellHost<M>,
{
    let empty = || iced_widget::column![].into();
    if !model.rail_drag_confirmed() {
        return empty();
    }
    let Some(panel): Option<PanelId> = model.dragged_panel() else {
        return empty();
    };
    let Some(meta) = host.meta(&panel) else {
        return empty();
    };
    let size = byteui::theme::geometry::rail_button_size();
    let colors = byteui::theme::color::current();
    let ghost = container(icons::view(
        meta.icon,
        byteui::theme::icon_size::rail(),
        colors.gold,
    ))
    .width(Length::Fixed(size))
    .height(Length::Fixed(size))
    .align_x(alignment::Horizontal::Center)
    .align_y(alignment::Vertical::Center)
    .style(move |_t: &iced_widget::Theme| container::Style {
        background: Some(colors.card.into()),
        border: Border {
            color: colors.gold,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    });
    // 以光标为中心,并钳制在窗口范围内,避免贴着窗口边缘拖拽时图标一半画到窗口外。
    let (cx, cy) = model.cursor();
    let (w, h) = model.window();
    let x = (cx - size / 2.0).clamp(0.0, (w - size).max(0.0));
    let y = (cy - size / 2.0).clamp(0.0, (h - size).max(0.0));
    container(ghost)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top: y,
            left: x,
            right: 0.0,
            bottom: 0.0,
        })
        .into()
}
