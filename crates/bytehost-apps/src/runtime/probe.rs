//! 运行时可用性探测:分层报告"没装 / 装了但当前不可用 / 可用",产品据此给出不同的提示
//! (没装 → 去 Settings 安装;装了没启动 → 启动它,如 Colima)。外部命令经 [`CommandRunner`] 执行,
//! 测试里换成假的,不依赖本机装了什么。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAvailability {
    /// 可用;`detail` 是版本之类的人类可读信息。
    Available {
        detail: String,
    },
    /// 装了但当前用不了(如 Docker 守护进程没启动)。
    Unavailable {
        detail: String,
    },
    NotInstalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// 找不到这个可执行文件。
    NotFound,
    Timeout,
    Failed(String),
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError>;
}

/// 真正执行外部命令,带超时(超时就杀掉子进程)。
pub struct SystemRunner {
    pub timeout: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
        }
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CommandError::NotFound
                } else {
                    CommandError::Failed(e.to_string())
                }
            })?;
        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let read = |pipe: Option<&mut dyn Read>| {
                        let mut s = String::new();
                        if let Some(p) = pipe {
                            let _ = p.read_to_string(&mut s);
                        }
                        s
                    };
                    let stdout = read(child.stdout.as_mut().map(|p| p as &mut dyn Read));
                    let stderr = read(child.stderr.as_mut().map(|p| p as &mut dyn Read));
                    return Ok(CommandOutput {
                        success: status.success(),
                        stdout,
                        stderr,
                    });
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CommandError::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(CommandError::Failed(e.to_string())),
            }
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn unavailable(
    program: &str,
    err: CommandError,
    out: Option<CommandOutput>,
) -> RuntimeAvailability {
    let detail = match (err, out) {
        (CommandError::Timeout, _) => format!("{program} 执行超时"),
        (CommandError::Failed(e), _) => e,
        (CommandError::NotFound, _) => unreachable!("NotFound 在调用处单独处理"),
    };
    RuntimeAvailability::Unavailable { detail }
}

/// Docker:先看 `docker` 命令在不在,再用 `docker info` 看守护进程连得上连不上(Colima 没启动时命令在、守护进程不可用)。
pub fn probe_docker(runner: &dyn CommandRunner) -> RuntimeAvailability {
    match runner.run("docker", &["--version"]) {
        Err(CommandError::NotFound) => return RuntimeAvailability::NotInstalled,
        Err(e) => return unavailable("docker", e, None),
        Ok(out) if !out.success => {
            return RuntimeAvailability::Unavailable {
                detail: first_line(&out.stderr),
            };
        }
        Ok(_) => {}
    }
    match runner.run("docker", &["info", "--format", "{{.ServerVersion}}"]) {
        Ok(out) if out.success => RuntimeAvailability::Available {
            detail: format!("docker {}", first_line(&out.stdout)),
        },
        Ok(out) => {
            let reason = first_line(&out.stderr);
            RuntimeAvailability::Unavailable {
                detail: if reason.is_empty() {
                    "docker 守护进程不可用".to_string()
                } else {
                    reason
                },
            }
        }
        Err(CommandError::NotFound) => RuntimeAvailability::NotInstalled,
        Err(e) => unavailable("docker", e, None),
    }
}

/// Node:`node --version`。
pub fn probe_node(runner: &dyn CommandRunner) -> RuntimeAvailability {
    match runner.run("node", &["--version"]) {
        Err(CommandError::NotFound) => RuntimeAvailability::NotInstalled,
        Err(e) => unavailable("node", e, None),
        Ok(out) if out.success => RuntimeAvailability::Available {
            detail: format!("node {}", first_line(&out.stdout)),
        },
        Ok(out) => RuntimeAvailability::Unavailable {
            detail: first_line(&out.stderr),
        },
    }
}

/// Python:优先 `uv`(有 lockfile、能管 Python 版本),没有再退到 `python3`。
pub fn probe_python(runner: &dyn CommandRunner) -> RuntimeAvailability {
    match runner.run("uv", &["--version"]) {
        Ok(out) if out.success => {
            return RuntimeAvailability::Available {
                detail: first_line(&out.stdout),
            };
        }
        Ok(_) | Err(CommandError::NotFound) => {}
        Err(e) => return unavailable("uv", e, None),
    }
    match runner.run("python3", &["--version"]) {
        Err(CommandError::NotFound) => RuntimeAvailability::NotInstalled,
        Err(e) => unavailable("python3", e, None),
        // 老版本 python 把版本号打到 stderr
        Ok(out) if out.success => {
            let line = if out.stdout.trim().is_empty() {
                first_line(&out.stderr)
            } else {
                first_line(&out.stdout)
            };
            RuntimeAvailability::Available { detail: line }
        }
        Ok(out) => RuntimeAvailability::Unavailable {
            detail: first_line(&out.stderr),
        },
    }
}

/// 一次探测全部(Settings 展示用)。`static_web` 不需要任何外部运行时,不在其中。
pub fn probe_all(runner: &dyn CommandRunner) -> Vec<(&'static str, RuntimeAvailability)> {
    vec![
        ("docker", probe_docker(runner)),
        ("node", probe_node(runner)),
        ("python", probe_python(runner)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 按 "program arg1 arg2" 查表的假执行器。
    struct Fake(HashMap<String, Result<CommandOutput, CommandError>>);

    impl Fake {
        fn new(entries: Vec<(&str, Result<CommandOutput, CommandError>)>) -> Self {
            Self(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            )
        }
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.0
                .get(&key)
                .cloned()
                .unwrap_or(Err(CommandError::NotFound))
        }
    }

    fn ok(stdout: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    fn fail(stderr: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }

    const INFO: &str = "docker info --format {{.ServerVersion}}";

    #[test]
    fn docker_is_reported_in_three_distinct_layers() {
        assert_eq!(
            probe_docker(&Fake::new(vec![])),
            RuntimeAvailability::NotInstalled
        );

        let daemon_down = Fake::new(vec![
            ("docker --version", ok("Docker version 29.6.2")),
            (
                INFO,
                fail(
                    "Cannot connect to the Docker daemon at unix:///Users/x/.colima/default/docker.sock\nIs the docker daemon running?",
                ),
            ),
        ]);
        assert_eq!(
            probe_docker(&daemon_down),
            RuntimeAvailability::Unavailable {
                detail: "Cannot connect to the Docker daemon at unix:///Users/x/.colima/default/docker.sock".into()
            }
        );

        let up = Fake::new(vec![
            ("docker --version", ok("Docker version 29.6.2")),
            (INFO, ok("29.6.2\n")),
        ]);
        assert_eq!(
            probe_docker(&up),
            RuntimeAvailability::Available {
                detail: "docker 29.6.2".into()
            }
        );
    }

    #[test]
    fn a_silent_daemon_failure_still_gets_a_readable_reason_and_timeouts_are_unavailable() {
        let quiet = Fake::new(vec![
            ("docker --version", ok("Docker version 1")),
            (INFO, fail("")),
        ]);
        assert_eq!(
            probe_docker(&quiet),
            RuntimeAvailability::Unavailable {
                detail: "docker 守护进程不可用".into()
            }
        );
        let slow = Fake::new(vec![
            ("docker --version", ok("Docker version 1")),
            (INFO, Err(CommandError::Timeout)),
        ]);
        assert!(
            matches!(probe_docker(&slow), RuntimeAvailability::Unavailable { detail } if detail.contains("超时"))
        );
    }

    #[test]
    fn node_is_not_installed_available_or_broken() {
        assert_eq!(
            probe_node(&Fake::new(vec![])),
            RuntimeAvailability::NotInstalled
        );
        assert_eq!(
            probe_node(&Fake::new(vec![("node --version", ok("v24.14.0\n"))])),
            RuntimeAvailability::Available {
                detail: "node v24.14.0".into()
            }
        );
        assert_eq!(
            probe_node(&Fake::new(vec![(
                "node --version",
                fail("dyld: Library not loaded")
            )])),
            RuntimeAvailability::Unavailable {
                detail: "dyld: Library not loaded".into()
            }
        );
    }

    #[test]
    fn python_prefers_uv_then_python3_and_reads_old_pythons_stderr_version() {
        let both = Fake::new(vec![
            ("uv --version", ok("uv 0.9.1")),
            ("python3 --version", ok("Python 3.13.1")),
        ]);
        assert_eq!(
            probe_python(&both),
            RuntimeAvailability::Available {
                detail: "uv 0.9.1".into()
            }
        );

        let only_py = Fake::new(vec![("python3 --version", ok("Python 3.13.1\n"))]);
        assert_eq!(
            probe_python(&only_py),
            RuntimeAvailability::Available {
                detail: "Python 3.13.1".into()
            }
        );

        let old = Fake::new(vec![(
            "python3 --version",
            Ok(CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: "Python 2.7.18\n".into(),
            }),
        )]);
        assert_eq!(
            probe_python(&old),
            RuntimeAvailability::Available {
                detail: "Python 2.7.18".into()
            }
        );

        assert_eq!(
            probe_python(&Fake::new(vec![])),
            RuntimeAvailability::NotInstalled
        );
        let broken_uv = Fake::new(vec![
            ("uv --version", fail("boom")),
            ("python3 --version", ok("Python 3.12.0")),
        ]);
        assert_eq!(
            probe_python(&broken_uv),
            RuntimeAvailability::Available {
                detail: "Python 3.12.0".into()
            },
            "uv 坏了就退到 python3"
        );
    }

    #[test]
    fn probe_all_covers_the_three_external_runtimes_in_a_fixed_order() {
        let names: Vec<_> = probe_all(&Fake::new(vec![]))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["docker", "node", "python"]);
    }

    #[test]
    fn the_system_runner_reports_missing_programs_and_real_output() {
        let r = SystemRunner::default();
        assert_eq!(
            r.run("definitely-not-a-real-program-xyz", &[]),
            Err(CommandError::NotFound)
        );
        let out = r.run("sh", &["-c", "echo hello"]).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout.trim(), "hello");
        let out = r.run("sh", &["-c", "echo oops >&2; exit 3"]).unwrap();
        assert!(!out.success);
        assert_eq!(out.stderr.trim(), "oops");
    }

    #[test]
    fn the_system_runner_kills_commands_that_exceed_the_timeout() {
        let r = SystemRunner {
            timeout: Duration::from_millis(150),
        };
        let started = Instant::now();
        assert_eq!(r.run("sleep", &["5"]), Err(CommandError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(3), "超时后立即返回");
    }
}
