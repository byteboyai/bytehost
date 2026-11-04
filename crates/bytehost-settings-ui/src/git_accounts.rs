//! 共享 Git 账户页:三 provider 行、遮盖输入、连接 / 取消 / 断开、忙碌、
//! 失败 / 部分提交提示与重试只有这一份视图。
//!
//! 视图不访问 Keychain / 文件 / 网络;所有动作都经 [`GitIntent`] 交给产品。

use bytehost_git_accounts::{GitAccountError, GitProvider};
use bytehost_settings::git::{GitIntent, GitPageState};
use byteui::theme::{color, font};
use iced::widget::{Space, button, column, container, row, text};
use iced::{Alignment, Element, Length};

use crate::section;
use crate::text::Text;

/// 页面渲染参数。
pub struct View<'a, M> {
    pub state: &'a GitPageState,
    /// 输入框内容变化。
    pub on_token: &'a dyn Fn(GitProvider, String) -> M,
    /// 页面动作(连接 / 提交 / 取消 / 断开 / 重试 / 打开 PAT 页)。
    pub on_intent: &'a dyn Fn(GitIntent) -> M,
    pub text: Text<'a>,
}

/// 渲染完整 Git 页。
pub fn view<'a, M: Clone + 'a>(view: View<'a, M>) -> Element<'a, M> {
    let colors = color::current();
    let t = &view.text;
    let mut body = column![section::heading(
        t.get("settings.git.title", "Git accounts")
    )]
    .spacing(12);

    if view.state.is_loading() {
        body = body.push(
            text(t.get("settings.git.loading", "Loading accounts…"))
                .size(font::label())
                .color(colors.dim),
        );
    }

    if let Some(error) = view.state.query_error() {
        body = body.push(error_line(error, t));
        body = body.push(section::action_button(
            t.get("settings.git.retry", "Retry"),
            (view.on_intent)(GitIntent::RetryQuery),
            colors.cream,
        ));
    }

    for provider in GitProvider::ALL {
        body = body.push(provider_row(provider, &view, colors));
    }

    if let Some(notice) = view.state.notice() {
        body = body.push(error_line(notice, t));
    }

    body.into()
}

fn error_line<'a, M: 'a>(error: &GitAccountError, resolver: &Text<'a>) -> Element<'a, M> {
    let colors = color::current();
    let detail = error.stable();
    let label = resolver.get(
        "settings.git.error",
        "The account operation did not finish. Check the details and try again.",
    );
    column![
        section::warning(label, colors.red),
        iced::widget::text(detail)
            .size(font::caption())
            .color(colors.dim),
    ]
    .spacing(2)
    .into()
}

fn provider_row<'a, M: Clone + 'a>(
    provider: GitProvider,
    view: &View<'a, M>,
    colors: byteui::theme::color::ColorTokens,
) -> Element<'a, M> {
    let t = &view.text;
    let title = text(provider.display_name())
        .size(font::body())
        .color(colors.cream);

    if let Some(account) = view.state.account(provider) {
        let status = text(format!(
            "{}: {}",
            t.get("settings.git.connected", "Connected"),
            account.username
        ))
        .size(font::label())
        .color(colors.gold);
        let disconnect =
            button(text(t.get("settings.git.disconnect", "Disconnect")).size(font::body()))
                .padding([6, 14])
                .style(byteui::feedback::dialog::action_button_style(colors.dim));
        let disconnect = if view.state.is_busy(provider) {
            disconnect
        } else {
            disconnect.on_press((view.on_intent)(GitIntent::Disconnect { provider }))
        };
        return row![title, status, Space::new().width(Length::Fill), disconnect]
            .spacing(10)
            .align_y(Alignment::Center)
            .into();
    }

    if view.state.needs_reconciliation(provider) && !view.state.is_editing(provider) {
        let reconnect = button(text(t.get("settings.git.reconnect", "Reconnect")))
            .padding([6, 14])
            .on_press((view.on_intent)(GitIntent::Connect { provider }));
        let disconnect =
            button(text(t.get("settings.git.disconnect", "Disconnect"))).padding([6, 14]);
        let disconnect = if view.state.is_busy(provider) {
            disconnect
        } else {
            disconnect.on_press((view.on_intent)(GitIntent::Disconnect { provider }))
        };
        let mut body = column![
            row![
                title,
                Space::new().width(Length::Fill),
                reconnect,
                disconnect
            ]
            .spacing(10),
            text(t.get(
                "settings.git.unreconciled",
                "Credential saved; account details need recovery. Reconnect or disconnect."
            ))
            .size(font::caption())
            .color(colors.gold),
        ]
        .spacing(6);
        if let Some(error) = view.state.error(provider) {
            body = body.push(error_line(error, t));
        }
        return body.into();
    }

    if view.state.is_editing(provider) {
        let placeholder = t.get("settings.git.pat_placeholder", "Personal Access Token");
        let input = byteui::form::input_text::view_on_bg(
            &placeholder,
            view.state.token(provider),
            true,
            None,
            false,
            None,
            false,
            {
                let on_token = view.on_token;
                move |value| on_token(provider, value)
            },
        );
        let generate = container(
            text(t.get("settings.git.generate_pat", "No PAT? Generate one"))
                .size(font::label())
                .color(colors.gold),
        );
        let generate = iced::widget::MouseArea::new(generate)
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press((view.on_intent)(GitIntent::OpenTokenPage { provider }));

        let cancel = button(text(t.get("settings.git.cancel", "Cancel")).size(font::body()))
            .padding([6, 14])
            .on_press((view.on_intent)(GitIntent::Cancel { provider }))
            .style(byteui::feedback::dialog::action_button_style(colors.dim));

        let busy = view.state.is_busy(provider);
        let confirm_label = if busy {
            t.get("settings.git.validating", "Validating…")
        } else {
            t.get("settings.git.confirm", "Confirm")
        };
        let confirm = button(text(confirm_label).size(font::body()))
            .padding([6, 14])
            .style(byteui::feedback::dialog::action_button_style(colors.cream));
        let confirm = if busy {
            confirm
        } else {
            confirm.on_press((view.on_intent)(GitIntent::Submit { provider }))
        };

        let mut card = column![title, input, row![generate].spacing(8)].spacing(6);
        if let Some(error) = view.state.error(provider) {
            card = card.push(error_line(error, t));
        }
        card = card.push(
            row![Space::new().width(Length::Fill), cancel, confirm]
                .spacing(8)
                .align_y(Alignment::Center),
        );
        return card.into();
    }

    let status = text(t.get("settings.git.not_connected", "Not connected"))
        .size(font::label())
        .color(colors.dim);
    let connect = button(text(t.get("settings.git.connect", "Connect")).size(font::body()))
        .padding([6, 14])
        .on_press((view.on_intent)(GitIntent::Connect { provider }))
        .style(byteui::feedback::dialog::action_button_style(colors.cream));
    row![title, status, Space::new().width(Length::Fill), connect]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}
