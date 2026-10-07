//! 进程型 runtime 的清单校验与依赖安装命令。纯函数:不起进程、不碰网络。

use std::path::Path;

use crate::manifest::Runtime;
use crate::process::env::validate_port_env;

const NODE_PROGRAMS: &[&str] = &["node", "npm", "npx"];
const PYTHON_PROGRAMS: &[&str] = &["python", "python3", "uv"];

pub fn lockfile_of(runtime: &Runtime) -> Option<&str> {
    match runtime {
        Runtime::Node { lockfile, .. } | Runtime::Python { lockfile, .. } => lockfile.as_deref(),
        _ => None,
    }
}

pub fn check_process_runtime(runtime: &Runtime, package_dir: &Path) -> Result<(), String> {
    let (command, lockfile, http, allowed, known_lock) = match runtime {
        Runtime::Node {
            command,
            lockfile,
            http,
            ..
        } => (command, lockfile, http, NODE_PROGRAMS, "package-lock.json"),
        Runtime::Python {
            command,
            lockfile,
            http,
            ..
        } => (command, lockfile, http, PYTHON_PROGRAMS, "uv.lock"),
        _ => return Err("不是进程型应用".to_string()),
    };
    validate_port_env(&http.port_env).map_err(|e| format!("runtime.http.port_env 不合法: {e}"))?;
    let program = command.first().map(String::as_str).unwrap_or("");
    if !allowed.contains(&program) {
        return Err(format!(
            "runtime.command 必须以 {} 之一开头(得到 {program:?})",
            allowed.join("/")
        ));
    }
    if let Some(lock) = lockfile {
        if lock != known_lock {
            return Err(format!(
                "不支持的 lockfile {lock:?}(这种应用只认 {known_lock})"
            ));
        }
        if !package_dir.join(lock).is_file() {
            return Err(format!("包里没有声明的 lockfile {lock}"));
        }
    }
    Ok(())
}

pub fn install_argv(runtime: &Runtime) -> Option<Vec<String>> {
    let to = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
    match (runtime, lockfile_of(runtime)) {
        (Runtime::Node { .. }, Some(_)) => Some(to(&["npm", "ci"])),
        (Runtime::Python { .. }, Some(_)) => Some(to(&["uv", "sync", "--frozen"])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ProcessHttp;

    fn node(command: &[&str], lockfile: Option<&str>, port_env: &str) -> Runtime {
        Runtime::Node {
            command: command.iter().map(|s| s.to_string()).collect(),
            lockfile: lockfile.map(String::from),
            node: None,
            http: ProcessHttp {
                port_env: port_env.into(),
            },
        }
    }

    fn python(command: &[&str], lockfile: Option<&str>) -> Runtime {
        Runtime::Python {
            command: command.iter().map(|s| s.to_string()).collect(),
            lockfile: lockfile.map(String::from),
            python: None,
            http: ProcessHttp {
                port_env: "PORT".into(),
            },
        }
    }

    fn pkg(files: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for f in files {
            std::fs::write(d.path().join(f), "x").unwrap();
        }
        d
    }

    #[test]
    fn reserved_port_variable_names_are_refused() {
        let d = pkg(&[]);
        for bad in [
            "NODE_OPTIONS",
            "PATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BYTEHOST_HOST",
        ] {
            let err =
                check_process_runtime(&node(&["node", "a.js"], None, bad), d.path()).unwrap_err();
            assert!(err.contains(bad), "{err}");
        }
        assert!(check_process_runtime(&node(&["node", "a.js"], None, "PORT"), d.path()).is_ok());
    }

    #[test]
    fn the_command_must_start_with_the_runtimes_own_interpreter() {
        let d = pkg(&[]);
        for ok in [vec!["node", "a.js"], vec!["npm", "start"], vec!["npx", "x"]] {
            assert!(
                check_process_runtime(&node(&ok, None, "PORT"), d.path()).is_ok(),
                "{ok:?}"
            );
        }
        for bad in [
            vec!["sh", "-c", "x"],
            vec!["./server"],
            vec!["/usr/bin/node", "a.js"],
            vec!["python3", "a.py"],
        ] {
            assert!(
                check_process_runtime(&node(&bad, None, "PORT"), d.path()).is_err(),
                "{bad:?}"
            );
        }
        for ok in [
            vec!["python", "a.py"],
            vec!["python3", "-m", "app"],
            vec!["uv", "run", "a.py"],
        ] {
            assert!(
                check_process_runtime(&python(&ok, None), d.path()).is_ok(),
                "{ok:?}"
            );
        }
        assert!(check_process_runtime(&python(&["node", "a.js"], None), d.path()).is_err());
    }

    #[test]
    fn only_known_lockfiles_that_exist_in_the_package_are_accepted() {
        let d = pkg(&["package-lock.json", "uv.lock", "yarn.lock"]);
        assert!(
            check_process_runtime(
                &node(&["node", "a.js"], Some("package-lock.json"), "PORT"),
                d.path()
            )
            .is_ok()
        );
        assert!(
            check_process_runtime(&python(&["uv", "run", "a.py"], Some("uv.lock")), d.path())
                .is_ok()
        );
        // 不认识的格式、跨运行时、不存在
        assert!(
            check_process_runtime(
                &node(&["node", "a.js"], Some("yarn.lock"), "PORT"),
                d.path()
            )
            .is_err()
        );
        assert!(
            check_process_runtime(&node(&["node", "a.js"], Some("uv.lock"), "PORT"), d.path())
                .is_err()
        );
        let empty = pkg(&[]);
        assert!(
            check_process_runtime(
                &node(&["node", "a.js"], Some("package-lock.json"), "PORT"),
                empty.path()
            )
            .is_err()
        );
    }

    #[test]
    fn install_commands_follow_the_lockfile() {
        assert_eq!(
            install_argv(&node(
                &["node", "a.js"],
                Some("package-lock.json"),
                "PORT"
            )),
            Some(vec!["npm".into(), "ci".into()])
        );
        assert_eq!(
            install_argv(&python(&["uv", "run", "a.py"], Some("uv.lock"))),
            Some(vec!["uv".into(), "sync".into(), "--frozen".into()])
        );
        assert_eq!(install_argv(&python(&["python3", "a.py"], None)), None);
        assert_eq!(
            install_argv(&Runtime::StaticWeb {
                source: "web/".into()
            }),
            None
        );
    }
}
