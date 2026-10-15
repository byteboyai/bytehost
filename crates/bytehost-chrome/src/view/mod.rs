//! 骨架的 iced 控件。产品实现 [`ShellHost`],骨架负责 rail / 分隔线 / 区域外框 / 放大浮层 / 顶栏外壳 / 底栏槽。
//!
//! 控件返回 `Element<'a, M, iced_widget::Theme, iced_renderer::Renderer>`(与 byteui 的签名一致),
//! 不额外开渲染器 feature。产品用 [`ShellHost::lift`] 把 [`ShellMsg`] 包进自己的消息枚举。

mod divider;
mod maximize;
mod pair;
mod rail;
mod topbar;
mod zone;

use bytehost_core::id::PanelId;
use byteui::interaction::icons::IconKind;
use iced_widget::core::{Element, Length};
use iced_widget::{column, container, row, stack};

use crate::geometry::{Rect, Side};
use crate::metrics::ChromeMetrics;
use crate::model::{Divider, PairRects, PanelRules, ShellModel, ShellMsg};

pub use divider::divider_bar;
pub use pair::pair_view;
pub use topbar::TopBarSlots;

pub type El<'a, M> = Element<'a, M, iced_widget::Theme, iced_renderer::Renderer>;

/// 一个面板在图标栏上的呈现。
#[derive(Debug, Clone, PartialEq)]
pub struct PanelMeta {
    pub title: String,
    pub icon: IconKind,
}

/// 面板 body 被放进哪个区、占多大。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyArea {
    pub side: Side,
    /// 面板内容矩形(窗口坐标);放大态下就是放大盒子。
    pub rect: Rect,
    /// 面板是配对面板时的两个 slot 矩形;宽度由骨架按 split 与镜像算好。
    pub pair: Option<PairRects>,
    pub mirrored: bool,
    /// 配对的 split(第一个 slot 的占比);收起时 `collapsed`。
    pub split: f32,
    pub collapsed: bool,
}

pub trait ShellHost<M: Clone + 'static> {
    /// 把骨架消息包进产品消息。
    fn lift(msg: ShellMsg) -> M
    where
        Self: Sized;
    fn meta(&self, panel: &PanelId) -> Option<PanelMeta>;
    fn rules(&self, panel: &PanelId) -> Option<PanelRules>;
    /// 渲染某面板的 body。配对面板用 [`pair_view`] 排版自己的两个 slot。
    fn body<'a>(&'a self, panel: &PanelId, area: &BodyArea) -> El<'a, M>;
    fn top_bar<'a>(&'a self) -> TopBarSlots<'a, M>;
    fn footbar<'a>(&'a self) -> El<'a, M>;
    /// 双击顶栏空白处(原生标题栏没了,缩放窗口的手势要自己补)。
    fn on_top_bar_double_click(&self) -> M;
}

fn body_area<M: Clone + 'static, H: ShellHost<M>>(model: &ShellModel, host: &H, side: Side, rects: &crate::geometry::ShellRects, m: &ChromeMetrics) -> Option<BodyArea> {
    use bytehost_desktop::PaneDims;
    let rect = rects.content(side)?;
    let panel = model.selected(side);
    let rules = host.rules(panel);
    let pane = rules.and_then(|r| r.pane.map(|p| (r, p)));
    let (pair, mirrored, split, collapsed) = match pane {
        Some((r, p)) => {
            let dims = model.panes().get_or(panel, PaneDims::new(p.default_split, false));
            let mirrored = side != r.default_side;
            (Some(crate::model::pair_rects(rect, dims, p.spec, mirrored, m)), mirrored, dims.split, dims.collapsed)
        }
        None => (None, false, 0.5, false),
    };
    Some(BodyArea { side, rect, pair, mirrored, split, collapsed })
}

/// 整个窗口的根视图:顶栏 / 两条图标栏 / 两个面板区 / 分隔线 / 底栏,以及放大浮层与拖拽幽灵图标。
pub fn view<'a, M, H>(model: &'a ShellModel, host: &'a H, m: &ChromeMetrics) -> El<'a, M>
where
    M: Clone + 'static,
    H: ShellHost<M>,
{
    let rects = model.rects(m);
    let top = topbar::top_bar(host.top_bar(), host.on_top_bar_double_click(), m);
    let left = zone::zone_area::<M, H>(model, host, Side::Left, body_area(model, host, Side::Left, &rects, m), m);
    let right = zone::zone_area::<M, H>(model, host, Side::Right, body_area(model, host, Side::Right, &rects, m), m);
    let body = row![
        rail::icon_rail::<M, H>(model, host, Side::Left, m),
        column![
            row![left, divider_bar(Divider::Zones, byteui::theme::color::current().bg, byteui::theme::color::current().bg, H::lift(ShellMsg::DividerPress(Divider::Zones))), right].height(Length::Fill),
            container(host.footbar()).width(Length::Fill).height(Length::Fixed(m.footbar_height)),
        ]
        .width(Length::Fill),
        rail::icon_rail::<M, H>(model, host, Side::Right, m),
    ];
    // 与 Dozer 一致:窗口根容器铺窗口底色,区域外边距 / 分隔线两侧的留白露出的就是这个颜色(而不是 iced 默认主题的底色)。
    let base: El<'a, M> = container(column![top, body]).width(Length::Fill).height(Length::Fill).style(root_style).into();

    let mut layers: Vec<El<'a, M>> = vec![base];
    if let Some(side) = model.maximized() {
        if let Some(area) = body_area(model, host, side, &rects, m) {
            layers.push(maximize::maximize_overlay::<M, H>(host, model.selected(side), &area, m));
        }
    }
    if model.rail_drag_confirmed() {
        layers.push(rail::rail_drag_ghost::<M, H>(model, host));
    }
    if layers.len() == 1 {
        return layers.pop().expect("one layer");
    }
    stack(layers).width(Length::Fill).height(Length::Fill).into()
}

/// 窗口根容器的底色:byteui 区域样式里的 `window_background`。
fn root_style(_: &iced_widget::Theme) -> container::Style {
    container::Style { background: Some(byteui::theme::region::window_background().into()), ..container::Style::default() }
}

/// 把 iced 事件映射成骨架消息(产品在自己的 `listen_with` 里调用,非捕获闭包即可)。
pub fn map_event(event: &iced_widget::core::Event) -> Option<ShellMsg> {
    use iced_widget::core::{Event, mouse, window};
    match event {
        Event::Mouse(mouse::Event::CursorMoved { position }) => Some(ShellMsg::CursorMoved { x: position.x, y: position.y }),
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => Some(ShellMsg::ZoneClicked),
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => Some(ShellMsg::PointerReleased),
        Event::Window(window::Event::Resized(size)) => Some(ShellMsg::Resized { w: size.width, h: size.height }),
        // 拖着图标 / 分隔线时窗口失去焦点,鼠标释放事件可能永远不会送达:当作松手,免得拖拽状态一直悬着,
        // 之后光标划过图标栏就把图标误重排。
        Event::Window(window::Event::Unfocused) => Some(ShellMsg::PointerReleased),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Ctx, PaneRules};
    use bytehost_desktop::PaneSpec;

    #[test]
    fn the_root_paints_the_window_background_so_margins_do_not_show_the_iced_default() {
        let style = root_style(&iced_widget::Theme::Light);
        assert_eq!(style.background, Some(byteui::theme::region::window_background().into()));
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    enum Msg {
        Shell(ShellMsg),
        Zoom,
    }

    struct Host;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    impl ShellHost<Msg> for Host {
        fn lift(msg: ShellMsg) -> Msg {
            Msg::Shell(msg)
        }
        fn meta(&self, panel: &PanelId) -> Option<PanelMeta> {
            Some(PanelMeta { title: panel.as_str().to_owned(), icon: IconKind::Home })
        }
        fn rules(&self, panel: &PanelId) -> Option<PanelRules> {
            (panel.as_str() == "t.list").then_some(PanelRules {
                default_side: Side::Left,
                pane: Some(PaneRules { spec: PaneSpec::new(true, true), default_split: 0.4, min_first_px: 150.0 }),
            })
        }
        fn body<'a>(&'a self, panel: &PanelId, area: &BodyArea) -> El<'a, Msg> {
            match area.pair {
                Some(_) => {
                    let bg = byteui::theme::color::current().bg;
                    pair_view(area, iced_widget::text("list").into(), iced_widget::text("content").into(), Self::lift(ShellMsg::DividerPress(Divider::Pair(area.side))), bg, bg)
                }
                None => iced_widget::text(panel.as_str().to_owned()).into(),
            }
        }
        fn top_bar<'a>(&'a self) -> TopBarSlots<'a, Msg> {
            TopBarSlots { title: iced_widget::text("T").into(), tabs: iced_widget::text("tabs").into(), right: iced_widget::text("R").into() }
        }
        fn footbar<'a>(&'a self) -> El<'a, Msg> {
            iced_widget::text("foot").into()
        }
        fn on_top_bar_double_click(&self) -> Msg {
            Msg::Zoom
        }
    }

    fn model() -> ShellModel {
        ShellModel::new([vec![id("t.list"), id("t.a")], vec![id("t.x"), id("t.y")]], [id("t.list"), id("t.x")], 640.0, (1440.0, 900.0)).unwrap()
    }

    fn step(s: &mut ShellModel, msg: ShellMsg) {
        let metrics = ChromeMetrics::byteboy2077();
        let rules = |p: &PanelId| Host.rules(p);
        s.update(msg, &Ctx { metrics: &metrics, rules: &rules });
    }

    /// Building the element tree must never panic, whatever state the shell is in.
    #[test]
    fn the_view_builds_in_every_shell_state() {
        let m = ChromeMetrics::byteboy2077();
        let host = Host;
        let mut s = model();
        let _ = view(&s, &host, &m);
        // collapsed sides
        step(&mut s, ShellMsg::PanelPress { panel: id("t.list") });
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::PanelPress { panel: id("t.list") });
        step(&mut s, ShellMsg::PanelPress { panel: id("t.x") });
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::PanelPress { panel: id("t.x") });
        // maximized, each side
        step(&mut s, ShellMsg::Maximize(Side::Left));
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::Maximize(Side::Right));
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::MaximizeClose);
        // a collapsed pair, a mirrored pair, a rail drag in progress
        step(&mut s, ShellMsg::DividerPress(Divider::Pair(Side::Left)));
        step(&mut s, ShellMsg::CursorMoved { x: 60.0, y: 300.0 });
        step(&mut s, ShellMsg::PointerReleased);
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::CursorMoved { x: 20.0, y: 80.0 });
        step(&mut s, ShellMsg::PanelPress { panel: id("t.a") });
        step(&mut s, ShellMsg::CursorMoved { x: 400.0, y: 400.0 });
        step(&mut s, ShellMsg::RailDragMove { side: Side::Right, index: 1 });
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::PointerReleased);
        let _ = view(&s, &host, &m);
        step(&mut s, ShellMsg::Resized { w: 300.0, h: 200.0 });
        let _ = view(&s, &host, &m);
    }

    #[test]
    fn events_map_to_shell_messages() {
        use iced_widget::core::{Event, Point, Size, mouse, window};
        assert_eq!(map_event(&Event::Mouse(mouse::Event::CursorMoved { position: Point::new(3.0, 4.0) })), Some(ShellMsg::CursorMoved { x: 3.0, y: 4.0 }));
        assert_eq!(map_event(&Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))), Some(ShellMsg::ZoneClicked));
        assert_eq!(map_event(&Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right))), None);
        assert_eq!(map_event(&Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))), Some(ShellMsg::PointerReleased));
        assert_eq!(map_event(&Event::Window(window::Event::Resized(Size::new(800.0, 600.0)))), Some(ShellMsg::Resized { w: 800.0, h: 600.0 }));
        assert_eq!(map_event(&Event::Window(window::Event::Focused)), None);
        assert_eq!(map_event(&Event::Window(window::Event::Unfocused)), Some(ShellMsg::PointerReleased), "losing focus mid-drag must end the drag");
    }
}
