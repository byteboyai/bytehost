//! 运行时行与应用行:动作集合是纯逻辑,渲染只读状态。

use bytehost_apps::plan::InstallPlan;
use bytehost_apps::proto::{AppSummary, ManagedRuntime, RuntimeProbe};
use bytehost_apps::state::ObservedState;
use bytehost_panel::install::{
    InstallFlowState as State, Message, managed_runtime_of, observed_label, probe_is_installing,
    progress_text, rollback_note_label, runtime_line,
};
use byteui::theme::color;
use iced::widget::{Space, column, row, text};
use iced::{Alignment, Element, Length};

use crate::section;
use crate::text::Text;

use super::AppRowExtra;

/// 运行时一行里此刻该显示的条目。顺序即显示顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeAction {
    Version(String),
    Processing(String),
    Uninstall(ManagedRuntime, String),
    Install(ManagedRuntime),
}

pub(crate) fn runtime_actions(
    probe: &RuntimeProbe,
    runtime: Option<ManagedRuntime>,
    uninstalling: &dyn Fn(ManagedRuntime, &str) -> bool,
) -> Vec<RuntimeAction> {
    let mut items = Vec::new();
    if !probe.managed.is_empty() {
        for version in &probe.managed {
            let Some(rt) = runtime else {
                items.push(RuntimeAction::Version(version.clone()));
                continue;
            };
            if uninstalling(rt, version) {
                items.push(RuntimeAction::Processing(version.clone()));
                continue;
            }
            items.push(RuntimeAction::Version(version.clone()));
            items.push(RuntimeAction::Uninstall(rt, version.clone()));
        }
    } else if probe.installable
        && let Some(rt) = runtime
    {
        items.push(RuntimeAction::Install(rt));
    }
    items
}

/// 应用行里此刻该显示的动作。顺序即显示顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowAction {
    Stop,
    Rollback(bytehost_apps::id::Version),
    Logs { open: bool },
    Uninstall,
}

pub(crate) fn row_actions(app: &AppSummary, acting: bool, log_open: bool) -> Vec<RowAction> {
    if acting {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if matches!(
        app.observed,
        ObservedState::Running | ObservedState::Starting
    ) {
        actions.push(RowAction::Stop);
    }
    if let Some(to) = app.previous_version {
        actions.push(RowAction::Rollback(to));
    }
    actions.push(RowAction::Logs { open: log_open });
    if !app.observed.is_transient() {
        actions.push(RowAction::Uninstall);
    }
    actions
}

pub(crate) fn runtime_row_view<'a, M: Clone + 'a>(
    state: &'a State,
    probe: &'a RuntimeProbe,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
) -> Element<'a, M> {
    let colors = color::current();
    let (name, status, _) = runtime_line(probe);
    let mut head = row![
        text(name.to_string())
            .size(byteui::theme::font::body())
            .color(colors.cream)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    if probe_is_installing(probe) {
        let progress = probe.job.as_ref().map(progress_text).unwrap_or_default();
        head = head.push(section::dim(progress));
        head = head.push(Space::new().width(Length::Fill));
        return head.into();
    }

    head = head.push(section::dim(status).width(Length::Fill));

    let runtime = managed_runtime_of(&probe.runtime);
    let actions = runtime_actions(probe, runtime, &|rt, v| state.runtime_uninstalling(rt, v));
    if actions.is_empty() {
        return head.into();
    }
    let mut buttons = row![].spacing(8).align_y(Alignment::Center);
    for action in actions {
        let item: Element<'a, M> = match action {
            RuntimeAction::Version(v) => section::dim(v).into(),
            RuntimeAction::Processing(v) => section::dim(format!("{v} (processing…)")).into(),
            RuntimeAction::Uninstall(rt, v) => section::action_button(
                text_.get("settings.apps.uninstall", "Uninstall"),
                map(Message::RuntimeUninstallClicked(rt, v)),
                colors.red,
            ),
            RuntimeAction::Install(rt) => section::action_button(
                text_.get("settings.apps.install_ellipsis", "Install…"),
                map(Message::RuntimeInstallClicked(rt)),
                colors.gold,
            ),
        };
        buttons = buttons.push(item);
    }
    column![Element::from(head), buttons.wrap().vertical_spacing(6)]
        .spacing(6)
        .width(Length::Fill)
        .into()
}

pub(crate) fn app_row<'a, M: Clone + 'a>(
    state: &'a State,
    app: &'a AppSummary,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
    notes: Option<&'a AppRowExtra<'a, M>>,
    leading: Option<&'a AppRowExtra<'a, M>>,
) -> Element<'a, M> {
    use bytehost_panel::install::Flow;
    let colors = color::current();
    let id = app.id.as_str().to_owned();
    let version_line = format!(
        "Version {} · {}",
        app.version,
        observed_label(&app.observed)
    );
    let title = app
        .display_i18n
        .as_ref()
        .and_then(|d| d.resolve_title(&bytehost_apps::proto::DisplayTitleScope::Presentation, "en"))
        .unwrap_or(app.name.as_str());
    let acting = state.is_acting(&id).is_some();

    let mut head = row![
        text(title.to_string())
            .size(byteui::theme::font::body())
            .color(colors.cream),
        section::dim(version_line),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if acting {
        head = head.push(section::dim(
            text_.get("settings.apps.processing", "Processing…"),
        ));
    }

    let mut col = column![Element::from(head)].spacing(6).width(Length::Fill);
    if let Some(notes) = notes {
        for note in notes(app) {
            col = col.push(note);
        }
    }

    let mut buttons = row![].spacing(8).align_y(Alignment::Center);
    if let Some(leading) = leading {
        for action in leading(app) {
            buttons = buttons.push(action);
        }
    }
    for action in row_actions(app, acting, state.is_log_open(&id)) {
        buttons = buttons.push(match action {
            RowAction::Stop => section::action_button(
                text_.get("settings.apps.stop", "Stop"),
                map(Message::StopClicked(id.clone())),
                colors.dim,
            ),
            RowAction::Rollback(to) => section::action_button(
                format!(
                    "{} {to}",
                    text_.get("settings.apps.rollback_to", "Roll back to")
                ),
                map(Message::RollbackClicked(id.clone())),
                colors.dim,
            ),
            RowAction::Logs { open: true } => section::action_button(
                text_.get("settings.apps.hide_logs", "Hide logs"),
                map(Message::HideLogs),
                colors.dim,
            ),
            RowAction::Logs { open: false } => section::action_button(
                text_.get("settings.apps.logs", "Logs"),
                map(Message::ShowLogs(id.clone())),
                colors.dim,
            ),
            RowAction::Uninstall => section::action_button(
                text_.get("settings.apps.uninstall", "Uninstall"),
                map(Message::UninstallClicked(id.clone())),
                colors.red,
            ),
        });
    }

    if !acting {
        col = col.push(buttons.wrap().vertical_spacing(6));
    }

    if let Flow::ConfirmRollback { id: pending, .. } = &state.flow
        && pending == &id
    {
        col = col.push(
            column![
                text(text_.get(
                    "settings.apps.rollback_confirm",
                    "Roll back: app data is not rolled back with it, and the old version may not read data written by the new version. Roll back?"
                ))
                .size(byteui::theme::font::body())
                .color(colors.cream),
                row![
                    section::action_button(
                        text_.get("common.cancel", "Cancel"),
                        map(Message::RollbackCancelled),
                        colors.dim
                    ),
                    section::action_button(
                        text_.get("settings.apps.confirm_rollback", "Confirm rollback"),
                        map(Message::RollbackConfirmed(id.clone())),
                        colors.gold,
                    ),
                ]
                .spacing(10),
            ]
            .spacing(6)
            .width(Length::Fill),
        );
    }
    if let Some(note) = &app.rollback_note {
        col = col.push(section::dim(rollback_note_label(note)));
    }
    col.into()
}

/// 审批卡:把 `plan_view` 的每一行如实画出来;升级权限用金色 `↑` 标注。
pub(crate) fn review_view<'a, M: Clone + 'a>(
    plan: &'a InstallPlan,
    running: bool,
    text_: Text<'a>,
    map: &'a dyn Fn(Message) -> M,
) -> Element<'a, M> {
    use bytehost_panel::install::plan_view;
    let colors = color::current();
    let view = plan_view(plan, running);
    let mut col = column![
        text(text_.get("settings.apps.install_title", "Install app"))
            .size(byteui::theme::font::body())
            .color(colors.cream),
    ]
    .spacing(6);
    for notice in view.notices {
        col = col.push(
            text(notice)
                .size(byteui::theme::font::label())
                .color(colors.gold),
        );
    }
    for (k, v) in view.facts {
        col = col.push(fact_row(k, v));
    }
    col = col.push(section::dim(
        text_.get("settings.apps.source_heading", "Source:"),
    ));
    if view.source.from_network {
        col = col.push(
            text(text_.get(
                "settings.apps.from_network_untrusted",
                "From the network, untrusted",
            ))
            .size(byteui::theme::font::label())
            .color(colors.gold),
        );
        col = col.push(section::dim(text_.get(
            "settings.apps.static_only_csp",
            "Static apps only, running under a strict CSP",
        )));
    }
    for (k, v) in view.source.rows {
        col = col.push(fact_row(k, v));
    }
    for warning in view.source.warnings {
        col = col.push(
            text(warning)
                .size(byteui::theme::font::label())
                .color(colors.gold),
        );
    }
    col = col.push(section::dim(text_.get(
        "settings.apps.will_run",
        "Installing and running will execute:",
    )));
    for line in view.will_run {
        col = col.push(
            text(line)
                .size(byteui::theme::font::label())
                .color(colors.cream),
        );
    }
    col = col.push(section::dim(text_.get(
        "settings.apps.permissions_heading",
        "Requested permissions:",
    )));
    if view.permissions.is_empty() {
        col = col.push(
            text(text_.get("settings.apps.no_permissions", "No permissions requested"))
                .size(byteui::theme::font::label())
                .color(colors.cream),
        );
    }
    for p in view.permissions {
        col = col.push(
            row![
                text(if p.escalation { "↑" } else { " " })
                    .size(byteui::theme::font::label())
                    .color(colors.gold),
                text(p.label)
                    .size(byteui::theme::font::label())
                    .color(colors.cream),
                text(p.change)
                    .size(byteui::theme::font::label())
                    .color(colors.cream),
                Space::new().width(Length::Fill),
                section::dim(p.enforcement),
            ]
            .spacing(10),
        );
    }
    col = col.push(
        row![
            section::action_button(
                text_.get("common.cancel", "Cancel"),
                map(Message::FlowDismissed),
                colors.dim,
            ),
            section::action_button(
                text_.get("settings.apps.approve_and_install", "Approve and install"),
                map(Message::ApproveClicked),
                colors.gold,
            ),
        ]
        .spacing(8),
    );
    iced::widget::container(col)
        .padding(10)
        .width(Length::Fill)
        .into()
}

fn fact_row<'a, M: 'a>(key: impl Into<String>, value: impl Into<String>) -> Element<'a, M> {
    let colors = byteui::theme::color::current();
    row![
        section::dim(key.into()),
        text(value.into())
            .size(byteui::theme::font::label())
            .color(colors.cream)
    ]
    .spacing(10)
    .into()
}

#[cfg(test)]
mod tests {
    use super::{RowAction, RuntimeAction, row_actions, runtime_actions};
    use bytehost_apps::id::{AppId, Version};
    use bytehost_apps::proto::{AppSummary, ManagedRuntime, RuntimeAvailability, RuntimeProbe};
    use bytehost_apps::state::{DesiredState, ObservedState};

    fn summary(observed: ObservedState, previous: Option<Version>) -> AppSummary {
        AppSummary {
            id: AppId::new("demo").unwrap(),
            name: "Demo".into(),
            version: Version::new(1, 1, 0),
            desired: DesiredState::Running,
            observed,
            url: None,
            issue: None,
            previous_version: previous,
            rollback_note: None,
            display_i18n: None,
        }
    }

    #[test]
    fn a_running_app_with_a_previous_version_offers_stop_rollback_logs_uninstall_in_order() {
        let prev = Version::new(1, 0, 0);
        let app = summary(ObservedState::Running, Some(prev));
        assert_eq!(
            row_actions(&app, false, false),
            vec![
                RowAction::Stop,
                RowAction::Rollback(prev),
                RowAction::Logs { open: false },
                RowAction::Uninstall,
            ]
        );
    }

    #[test]
    fn acting_apps_show_no_actions() {
        let app = summary(ObservedState::Running, Some(Version::new(1, 0, 0)));
        assert!(row_actions(&app, true, false).is_empty());
    }

    #[test]
    fn transient_apps_cannot_be_uninstalled() {
        let app = summary(ObservedState::Updating, None);
        assert!(!row_actions(&app, false, false).contains(&RowAction::Uninstall));
    }

    fn probe(runtime: &str, managed: &[&str], installable: bool) -> RuntimeProbe {
        RuntimeProbe {
            runtime: runtime.into(),
            availability: RuntimeAvailability::Available {
                detail: "uv 0.8.2".into(),
            },
            managed: managed.iter().map(|s| s.to_string()).collect(),
            installable,
            job: None,
        }
    }

    #[test]
    fn installable_runtime_without_a_managed_version_offers_install() {
        let p = probe("python", &[], true);
        assert_eq!(
            runtime_actions(&p, Some(ManagedRuntime::Python), &|_, _| false),
            vec![RuntimeAction::Install(ManagedRuntime::Python)]
        );
    }

    #[test]
    fn uninstalling_versions_show_processing_without_a_button() {
        let p = probe("node", &["22.1.0", "20.5.0"], true);
        assert_eq!(
            runtime_actions(&p, Some(ManagedRuntime::Node), &|_, v| v == "20.5.0"),
            vec![
                RuntimeAction::Version("22.1.0".into()),
                RuntimeAction::Uninstall(ManagedRuntime::Node, "22.1.0".into()),
                RuntimeAction::Processing("20.5.0".into()),
            ]
        );
    }
}
