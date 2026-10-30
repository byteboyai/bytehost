//! 共享「应用」页:直接画 `bytehost_panel::install::InstallFlowState`。
//!
//! 不持有状态、不复制安装状态机;所有按钮发出原始 `install::Message`,由产品映射到
//! 自己的消息并交给既有执行链。
//!
//! 产品可以:
//! - 通过 [`View::extra_top`] / [`View::extra_bottom`] 插入自己的状态、通知与审批卡;
//! - 提供自己的日志滚动 `Id` 生成器,保证与产品侧的"贴底跟随"逻辑一致。

mod flow;
mod rows;

use bytehost_panel::install::{InstallFlowState, Load, Message};
use iced::widget::{column, container, scrollable, text};
use iced::{Element, Length};

use crate::section;
use crate::text::Text;

/// 产品为单个应用行追加元素的钩子:入参是应用摘要,返回要插入的元素。
pub type AppRowExtra<'a, M> = dyn Fn(&bytehost_apps::proto::AppSummary) -> Vec<Element<'a, M>> + 'a;

/// 一次渲染所需的全部输入。
pub struct View<'a, M> {
    pub state: &'a InstallFlowState,
    pub text: Text<'a>,
    /// 把共享消息映射为产品消息(原始 `install::Message` 不变)。
    pub map: &'a dyn Fn(Message) -> M,
    /// 产品日志滚动容器的稳定 `Id`。
    pub log_scroll_id: &'a dyn Fn(&str) -> iced::widget::Id,
    /// 插在共享内容之前的区块(服务不可用、通知、清理状态等)。
    pub extra_top: Vec<Element<'a, M>>,
    /// 追加在安装流程之后的区块(产品附加说明、辅助操作等)。
    pub extra_bottom: Vec<Element<'a, M>>,
    /// 应用行里的产品附加说明(例如运行时问题),画在标题与动作之间。
    pub app_notes: Option<&'a AppRowExtra<'a, M>>,
    /// 应用行里的产品前置动作(例如「打开应用」),排在共享动作之前。
    pub app_leading_actions: Option<&'a AppRowExtra<'a, M>>,
}

impl<'a, M> View<'a, M> {
    pub fn new(
        state: &'a InstallFlowState,
        text: Text<'a>,
        map: &'a dyn Fn(Message) -> M,
        log_scroll_id: &'a dyn Fn(&str) -> iced::widget::Id,
    ) -> Self {
        Self {
            state,
            text,
            map,
            log_scroll_id,
            extra_top: Vec::new(),
            extra_bottom: Vec::new(),
            app_notes: None,
            app_leading_actions: None,
        }
    }
}

/// 渲染完整「应用」页(自带竖向滚动)。
pub fn view<'a, M: Clone + 'a>(spec: View<'a, M>) -> Element<'a, M> {
    let View {
        state,
        text,
        map,
        log_scroll_id,
        extra_top,
        extra_bottom,
        app_notes,
        app_leading_actions,
    } = spec;

    let mut body = column![section::heading(text.get("settings.apps.title", "Apps"))]
        .spacing(12)
        .width(Length::Fill);

    for element in extra_top {
        body = body.push(element);
    }

    body = body.push(section::dim(text.get(
        "settings.apps.runtime_note",
        "Runtimes (managed versions ship with the daemon and take priority over system versions)",
    )));

    let probes: Element<'a, M> = match &state.probes {
        Load::Loading => section::dim(text.get("settings.apps.detecting", "Detecting…")).into(),
        Load::Failed(why) => section::dim(format!(
            "{}: {why}",
            text.get("settings.apps.probe_failed_label", "Probe failed")
        ))
        .into(),
        Load::Loaded(probes) => {
            let mut col = column![].spacing(4);
            for probe in probes {
                col = col.push(rows::runtime_row_view(state, probe, text, map));
            }
            col.into()
        }
    };
    body = body.push(probes);

    body = body.push(section::heading(
        text.get("settings.apps.installed_heading", "Installed"),
    ));

    let apps: Element<'a, M> = match &state.apps {
        Load::Loading => section::dim(text.get("settings.apps.reading", "Reading…")).into(),
        Load::Failed(why) => section::dim(format!(
            "{}: {why}",
            text.get("settings.apps.read_failed_label", "Read failed")
        ))
        .into(),
        Load::Loaded(apps) if apps.is_empty() => {
            section::dim(text.get("settings.apps.no_apps", "No apps installed yet")).into()
        }
        Load::Loaded(apps) => {
            let mut col = column![].spacing(6);
            for app in apps {
                col = col.push(rows::app_row(
                    state,
                    app,
                    text,
                    map,
                    app_notes,
                    app_leading_actions,
                ));
                if state.is_log_open(app.id.as_str()) {
                    col = col.push(log_viewer(
                        state.logs_view(app.id.as_str()),
                        app.id.as_str(),
                        text,
                        map,
                        log_scroll_id,
                    ));
                }
            }
            col.into()
        }
    };
    body = body.push(apps);

    body = body.push(flow::flow_view(state, text, map));

    for element in extra_bottom {
        body = body.push(element);
    }

    scrollable(body)
        .direction(scrollable::Direction::Vertical(
            byteui::interaction::scrollbar::scrollbar(),
        ))
        .style(|_t, _s| byteui::interaction::scrollbar::scrollbar_style())
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn log_viewer<'a, M: Clone + 'a>(
    view: bytehost_panel::logs::LogsView,
    app_id: &str,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
    log_scroll_id: &'a dyn Fn(&str) -> iced::widget::Id,
) -> Element<'a, M> {
    use bytehost_panel::logs::LogsView;
    let colors = byteui::theme::color::current();
    let mut col = column![].spacing(4).width(Length::Fill);
    match view {
        LogsView::Hidden => {}
        LogsView::Loading => {
            col = col.push(section::dim(
                text_.get("settings.apps.reading_logs", "Reading logs…"),
            ))
        }
        LogsView::Failed(reason) => {
            col = col.push(
                text(reason)
                    .size(byteui::theme::font::body())
                    .color(colors.red),
            )
        }
        LogsView::Loaded {
            text: log_text,
            truncated,
            stale,
        } => {
            if truncated {
                col = col.push(section::dim(text_.get(
                    "settings.apps.logs_truncated",
                    "Only the last lines are shown",
                )));
            }
            if stale {
                col = col.push(section::dim(text_.get(
                    "settings.apps.logs_stale",
                    "Refresh failed; showing the previous content",
                )));
            }
            let scrolled_id = app_id.to_owned();
            let body = text_widget(log_text, colors.cream);
            col = col.push(
                container(
                    scrollable(body)
                        .id(log_scroll_id(app_id))
                        .direction(scrollable::Direction::Vertical(
                            byteui::interaction::scrollbar::scrollbar(),
                        ))
                        .style(|_t, _s| byteui::interaction::scrollbar::scrollbar_style())
                        .width(Length::Fill)
                        .on_scroll(move |viewport| {
                            map(Message::LogsScrolled(
                                scrolled_id.clone(),
                                at_bottom(&viewport),
                            ))
                        }),
                )
                .height(Length::Fixed(200.0))
                .width(Length::Fill)
                .padding(6),
            );
        }
    }
    container(col).width(Length::Fill).padding(6).into()
}

fn text_widget<'a>(log_text: String, tint: iced::Color) -> iced::widget::Text<'a> {
    text(log_text)
        .size(byteui::theme::font::body())
        .color(tint)
        .shaping(iced::widget::text::Shaping::Advanced)
        .width(Length::Fill)
}

/// 视口是否贴底(留 4px 容差)。
fn at_bottom(viewport: &scrollable::Viewport) -> bool {
    let abs = viewport.absolute_offset();
    let bottom = abs.y + viewport.bounds().height;
    bottom >= viewport.content_bounds().height - 4.0
}
