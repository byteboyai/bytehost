//! 生命周期:`desired`(想要什么)与 `observed`(实际观察到什么)分开,由 manager 持续对账。
//! 这里只有**纯函数**:给定两者,下一步该做什么;宿主(supervisor)重启之后,持久化的观察态怎么修正。

use serde::{Deserialize, Serialize};

/// 用户/产品想要应用处于什么状态(持久化在 `AppRecord`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Stopped,
    Running,
    /// 要卸载(程序;是否连数据一起删由卸载模式决定)。
    Removed,
}

/// 实际观察到的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObservedState {
    NotInstalled,
    Installed,
    Preparing,
    Starting,
    Running,
    Stopping,
    Stopped,
    Updating,
    Uninstalling,
    Failed { reason: String, retryable: bool },
}

/// 对账给出的下一步动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 准备(装依赖/拉镜像等)并启动。
    Start,
    /// 停止(含取消进行中的准备/启动)。
    Stop,
    Uninstall,
}

impl ObservedState {
    /// 进行中的过渡态:动作已经发出,对账时应当等它结束而不是再发一次。
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Starting | Self::Stopping | Self::Updating | Self::Uninstalling
        )
    }
}

/// 想要 `desired` 而实际是 `observed` 时,下一步该做什么(**不含重试退避**:`Running` + 可重试的 `Failed` 会一直
/// 给 `Start`,manager 必须自己限制次数/间隔,否则会紧循环);`None` = 什么都不用做(已达成、在等进行中的动作、
/// 或不可恢复的失败需要人介入)。
pub fn next_action(desired: DesiredState, observed: &ObservedState) -> Option<Action> {
    use DesiredState as D;
    use ObservedState as O;
    match (desired, observed) {
        // 进行中的过渡态:等,但"想停/想卸"时仍要能取消正在准备/启动的
        (D::Stopped | D::Removed, O::Preparing | O::Starting) => Some(Action::Stop),
        (_, o) if o.is_transient() => None,

        (D::Running, O::Installed | O::Stopped) => Some(Action::Start),
        (
            D::Running,
            O::Failed {
                retryable: true, ..
            },
        ) => Some(Action::Start),
        (D::Running, _) => None,

        (D::Stopped, O::Running) => Some(Action::Stop),
        // 规格 §4.4:Failed 可经 stop 复位——失败的进程型应用可能还留着活进程/占着端口
        (D::Stopped, O::Failed { .. }) => Some(Action::Stop),
        (D::Stopped, _) => None,

        (D::Removed, O::Running) => Some(Action::Stop),
        (D::Removed, O::Installed | O::Stopped | O::Failed { .. }) => Some(Action::Uninstall),
        (D::Removed, _) => None,
    }
}

/// supervisor(dozerd)重启之后,把持久化下来的观察态修正为现实:应用随 supervisor 一起停止了
/// (`docs/.../app-host-design.md` §6.1),所以"曾在运行/过渡中"的都回到 `Stopped`;
/// 被打断的升级回到 `Stopped`(旧版本完整);被打断的卸载标成可重试的失败(再卸一次即可)。
pub fn recover_after_supervisor_restart(observed: ObservedState) -> ObservedState {
    match observed {
        ObservedState::Preparing
        | ObservedState::Starting
        | ObservedState::Running
        | ObservedState::Stopping => ObservedState::Stopped,
        // 每个版本的包目录不可变、`current_version` 最后才写:升级被打断时旧版本仍然完整,回到 Stopped
        ObservedState::Updating => ObservedState::Stopped,
        ObservedState::Uninstalling => ObservedState::Failed {
            reason: "卸载被打断".to_string(),
            retryable: true,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DesiredState as D;
    use ObservedState as O;

    fn failed(retryable: bool) -> O {
        O::Failed {
            reason: "x".into(),
            retryable,
        }
    }

    fn every_observed() -> Vec<O> {
        vec![
            O::NotInstalled,
            O::Installed,
            O::Preparing,
            O::Starting,
            O::Running,
            O::Stopping,
            O::Stopped,
            O::Updating,
            O::Uninstalling,
            failed(true),
            failed(false),
        ]
    }

    /// 完整真值表:3 种 desired × 11 种 observed,没有遗漏的格子。
    #[test]
    fn next_action_truth_table() {
        use Action::*;
        let table: [(D, Vec<Option<Action>>); 3] = [
            // NotInstalled, Installed, Preparing, Starting, Running, Stopping, Stopped, Updating, Uninstalling, Failed(retry), Failed(no)
            (
                D::Running,
                vec![
                    None,
                    Some(Start),
                    None,
                    None,
                    None,
                    None,
                    Some(Start),
                    None,
                    None,
                    Some(Start),
                    None,
                ],
            ),
            (
                D::Stopped,
                vec![
                    None,
                    None,
                    Some(Stop),
                    Some(Stop),
                    Some(Stop),
                    None,
                    None,
                    None,
                    None,
                    Some(Stop),
                    Some(Stop),
                ],
            ),
            (
                D::Removed,
                vec![
                    None,
                    Some(Uninstall),
                    Some(Stop),
                    Some(Stop),
                    Some(Stop),
                    None,
                    Some(Uninstall),
                    None,
                    None,
                    Some(Uninstall),
                    Some(Uninstall),
                ],
            ),
        ];
        for (desired, expected) in table {
            for (observed, want) in every_observed().iter().zip(expected) {
                assert_eq!(
                    next_action(desired, observed),
                    want,
                    "{desired:?} × {observed:?}"
                );
            }
        }
    }

    #[test]
    fn transient_states_are_exactly_the_in_flight_ones() {
        let transient: Vec<_> = every_observed()
            .into_iter()
            .filter(|o| o.is_transient())
            .collect();
        assert_eq!(
            transient,
            vec![
                O::Preparing,
                O::Starting,
                O::Stopping,
                O::Updating,
                O::Uninstalling
            ]
        );
    }

    #[test]
    fn a_non_retryable_failure_never_restarts_by_itself() {
        assert_eq!(next_action(D::Running, &failed(false)), None);
        assert_eq!(next_action(D::Running, &failed(true)), Some(Action::Start));
    }

    #[test]
    fn after_a_supervisor_restart_nothing_is_running_or_in_flight() {
        for (before, after) in [
            (O::Running, O::Stopped),
            (O::Starting, O::Stopped),
            (O::Preparing, O::Stopped),
            (O::Stopping, O::Stopped),
            (O::NotInstalled, O::NotInstalled),
            (O::Installed, O::Installed),
            (O::Stopped, O::Stopped),
        ] {
            assert_eq!(
                recover_after_supervisor_restart(before.clone()),
                after,
                "{before:?}"
            );
        }
        assert_eq!(
            recover_after_supervisor_restart(failed(false)),
            failed(false),
            "失败原样保留"
        );
        assert_eq!(
            recover_after_supervisor_restart(O::Updating),
            O::Stopped,
            "升级中断:每个版本的包目录不可变、current_version 最后才写,所以旧版本仍然完整,回到 Stopped"
        );
        assert!(matches!(
            recover_after_supervisor_restart(O::Uninstalling),
            O::Failed {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn recovery_never_leaves_a_transient_state_behind() {
        for o in every_observed() {
            assert!(
                !recover_after_supervisor_restart(o.clone()).is_transient(),
                "{o:?}"
            );
        }
    }

    #[test]
    fn a_desired_running_app_restarts_after_a_supervisor_restart() {
        let observed = recover_after_supervisor_restart(O::Running);
        assert_eq!(next_action(D::Running, &observed), Some(Action::Start));
    }

    #[test]
    fn states_serialize_with_a_stable_snake_case_vocabulary() {
        assert_eq!(serde_json::to_string(&D::Running).unwrap(), "\"running\"");
        assert_eq!(
            serde_json::to_string(&O::NotInstalled).unwrap(),
            "{\"state\":\"not_installed\"}"
        );
        let f = O::Failed {
            reason: "端口被占用".into(),
            retryable: true,
        };
        assert_eq!(
            serde_json::from_str::<O>(&serde_json::to_string(&f).unwrap()).unwrap(),
            f
        );
    }

    #[test]
    fn a_failed_app_that_should_be_stopped_is_stopped_to_clean_up_any_residue() {
        // 规格 §4.4:Failed 可经 start 重试,或经 stop 复位——失败的进程型应用可能还留着活进程/占着端口
        assert_eq!(next_action(D::Stopped, &failed(true)), Some(Action::Stop));
        assert_eq!(next_action(D::Stopped, &failed(false)), Some(Action::Stop));
        // 复位之后(Stopped)就不再有动作,不会循环
        assert_eq!(next_action(D::Stopped, &O::Stopped), None);
    }
}
