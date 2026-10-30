//! 安装来源、计划审批、运行时安装 / 卸载与错误 / 忙碌状态。

use bytehost_apps::registry::UninstallMode;
use bytehost_panel::install::{
    Flow, InstallFlowState as State, Message, PickTarget, SourceChoice, managed_runtime_label,
    plan_lines,
};
use byteui::theme::color;
use iced::widget::{Space, column, container, row, text};
use iced::{Element, Length};

use crate::apps::rows::review_view;
use crate::section;
use crate::text::Text;

pub(crate) fn flow_view<'a, M: Clone + 'a>(
    state: &'a State,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
) -> Element<'a, M> {
    let colors = color::current();
    match &state.flow {
        Flow::Idle => install_form(state, text_, map),
        Flow::Picking { target } => match target {
            PickTarget::Dir => section::dim(text_.get(
                "settings.apps.pick_dir_hint",
                "Choose the app directory in the dialog (it must contain manifest.toml)…",
            ))
            .into(),
            PickTarget::Archive => section::dim(text_.get(
                "settings.apps.pick_archive_hint",
                "Choose the archive in the dialog (.zip / .tar.gz / .tgz)…",
            ))
            .into(),
        },
        Flow::Planning { .. } => {
            section::dim(text_.get("settings.apps.checking_app", "Checking app…")).into()
        }
        Flow::Installing { .. } => {
            section::dim(text_.get("settings.apps.installing", "Installing…")).into()
        }
        Flow::Failed { message } => column![
            text(message.as_str())
                .size(byteui::theme::font::body())
                .color(colors.red),
            section::action_button(
                text_.get("settings.apps.got_it", "Got it"),
                map(Message::FlowDismissed),
                colors.dim
            ),
        ]
        .spacing(8)
        .into(),
        Flow::ConfirmUninstall { name, .. } => column![
            text(format!(
                "{} \"{name}\"?",
                text_.get("settings.apps.uninstall_title", "Uninstall app")
            ))
            .size(byteui::theme::font::body())
            .color(colors.cream),
            section::dim(text_.get(
                "settings.apps.uninstall_body",
                "\"Keep data\" removes only the program, so data remains after reinstall; \"Delete data too\" cannot be undone."
            )),
            row![
                section::action_button(
                    text_.get("common.cancel", "Cancel"),
                    map(Message::FlowDismissed),
                    colors.dim
                ),
                section::action_button(
                    text_.get("settings.apps.keep_data", "Keep data"),
                    map(Message::UninstallConfirmed(UninstallMode::Program)),
                    colors.gold
                ),
                section::action_button(
                    text_.get("settings.apps.delete_data_too", "Delete data too"),
                    map(Message::UninstallConfirmed(UninstallMode::ProgramAndData)),
                    colors.red
                ),
            ]
            .spacing(8),
        ]
        .spacing(8)
        .into(),
        Flow::ConfirmRollback { .. } => Space::new().height(Length::Fixed(0.0)).into(),
        Flow::Reviewing { plan, .. } => {
            let running = state.observed_running(plan.app_id.as_str());
            review_view(plan, running, text_, map)
        }
        Flow::RuntimePlanning { runtime } => section::dim(format!(
            "Preparing the install plan for the {} runtime…",
            managed_runtime_label(*runtime)
        ))
        .into(),
        Flow::RuntimeReviewing { plan } => runtime_review_view(plan, text_, map),
        Flow::ConfirmRuntimeUninstall { runtime, version } => column![
            text(format!(
                "Uninstall managed runtime {} {version}?",
                managed_runtime_label(*runtime)
            ))
            .size(byteui::theme::font::body())
            .color(colors.cream),
            section::dim(text_.get(
                "settings.apps.runtime_uninstall_body",
                "Apps depending on it will fail to start next time because the runtime is missing; reinstalling can restore it."
            )),
            row![
                section::action_button(
                    text_.get("common.cancel", "Cancel"),
                    map(Message::FlowDismissed),
                    colors.dim
                ),
                section::action_button(
                    text_.get("settings.apps.uninstall", "Uninstall"),
                    map(Message::RuntimeUninstallConfirmed),
                    colors.red
                ),
            ]
            .spacing(8),
        ]
        .spacing(8)
        .into(),
    }
}

fn install_form<'a, M: Clone + 'a>(
    state: &'a State,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
) -> Element<'a, M> {
    let colors = color::current();
    let mut col = column![source_choice_row(state, text_, map)]
        .spacing(8)
        .width(Length::Fill);
    match state.source_choice {
        SourceChoice::Dir => {
            col = col.push(section::action_button(
                text_.get("settings.apps.install_app_ellipsis", "Install app…"),
                map(Message::InstallClicked),
                colors.gold,
            ));
        }
        SourceChoice::Archive => {
            col = col.push(section::action_button(
                text_.get("settings.apps.install_app_ellipsis", "Install app…"),
                map(Message::PickArchive),
                colors.gold,
            ));
        }
        SourceChoice::Url => {
            col = col.push(section::dim(text_.get(
                "settings.apps.url_hint",
                "https archive URL (static apps only)",
            )));
            col = col.push(byteui::form::input_text::view(
                "https://example.com/app.zip",
                state.url_input.as_str(),
                false,
                None,
                false,
                None,
                false,
                move |v| map(Message::UrlChanged(v)),
            ));
            col = col.push(section::dim(text_.get(
                "settings.apps.sha_hint",
                "Expected sha256 (optional; if set it is pinned, otherwise the approval card shows the actual computed hash)",
            )));
            let sha_placeholder =
                text_.get("settings.apps.sha_placeholder", "Leave empty = not pinned");
            col = col.push(byteui::form::input_text::view(
                &sha_placeholder,
                state.sha_input.as_str(),
                false,
                None,
                false,
                None,
                false,
                move |v| map(Message::Sha256Changed(v)),
            ));
            if let Some(err) = &state.url_error {
                col = col.push(
                    text(err.as_str())
                        .size(byteui::theme::font::label())
                        .color(colors.red),
                );
            }
            col = col.push(section::action_button(
                text_.get("settings.apps.fetch_plan", "Fetch plan"),
                map(Message::UrlPlanClicked),
                colors.gold,
            ));
        }
    }
    container(col).width(Length::Fill).into()
}

fn source_choice_row<'a, M: Clone + 'a>(
    state: &'a State,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
) -> Element<'a, M> {
    let colors = color::current();
    let current = state.source_choice;
    let mut row_ = row![].spacing(8);
    for (choice, key, fallback) in [
        (
            SourceChoice::Dir,
            "settings.apps.source_dir",
            "Local directory",
        ),
        (
            SourceChoice::Archive,
            "settings.apps.source_archive",
            "Local archive",
        ),
        (SourceChoice::Url, "settings.apps.source_url", "Network URL"),
    ] {
        let selected = current == choice;
        row_ = row_.push(section::action_button(
            text_.get(key, fallback),
            map(Message::SourceChoiceChanged(choice)),
            if selected { colors.gold } else { colors.dim },
        ));
    }
    row_.into()
}

/// 运行时安装审批卡:逐行展示 `plan_lines`(来源 URL、固定版本、完整 SHA-256、目标目录)。
fn runtime_review_view<'a, M: Clone + 'a>(
    plan: &'a bytehost_apps::proto::RuntimeInstallPlan,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
) -> Element<'a, M> {
    let colors = color::current();
    let mut col = column![
        text(format!(
            "{} {}",
            text_.get("settings.apps.install_runtime_title", "Install the runtime"),
            managed_runtime_label(plan.runtime)
        ))
        .size(byteui::theme::font::body())
        .color(colors.cream),
    ]
    .spacing(6);
    for (k, v) in plan_lines(plan) {
        col = col.push(flow_fact_row(k, v));
    }
    col = col.push(section::dim(
        text_.get("settings.apps.will_do", "What will happen:"),
    ));
    for line in &plan.will_do {
        col = col.push(
            text(line.clone())
                .size(byteui::theme::font::label())
                .color(colors.cream),
        );
    }
    col = col.push(
        row![
            section::action_button(
                text_.get("common.cancel", "Cancel"),
                map(Message::FlowDismissed),
                colors.dim
            ),
            section::action_button(
                text_.get("settings.apps.download_and_install", "Download and install"),
                map(Message::RuntimeApproveClicked),
                colors.gold
            ),
        ]
        .spacing(8),
    );
    container(col).padding(10).width(Length::Fill).into()
}

fn flow_fact_row<'a, M: 'a>(key: impl Into<String>, value: impl Into<String>) -> Element<'a, M> {
    let colors = color::current();
    row![
        section::dim(key.into()),
        text(value.into())
            .size(byteui::theme::font::label())
            .color(colors.cream)
    ]
    .spacing(10)
    .into()
}
