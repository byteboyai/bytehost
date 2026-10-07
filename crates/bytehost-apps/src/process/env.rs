//! 给子进程的环境(纯函数)。**白名单,不是黑名单**:dozerd 的环境里可能有任何东西(Agent 令牌、云凭据、`DOZER_*`),
//! 第三方/Agent 生成的应用一概不该继承;只放行运行程序所必需的几个系统变量,再加宿主明确给的几个。

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::id::AppId;

/// 从父进程环境里放行的变量名(精确匹配)与前缀。
const PASS_EXACT: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "LANG", "TMPDIR", "TZ"];
const PASS_PREFIX: &[&str] = &["LC_"];

/// 宿主自己设的变量(`port_env` 不得与它们或系统关键变量重名)。
const RESERVED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "TZ",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
];

#[derive(Debug, PartialEq, Eq)]
pub enum PortEnvError {
    Empty,
    BadName(String),
    Reserved(String),
}

impl std::fmt::Display for PortEnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "port_env 不能为空"),
            Self::BadName(n) => write!(
                f,
                "port_env {n:?} 不是合法的环境变量名(只允许 A-Z、0-9、_,且不以数字开头)"
            ),
            Self::Reserved(n) => write!(f, "port_env {n:?} 是保留变量名"),
        }
    }
}

impl std::error::Error for PortEnvError {}

/// 校验 manifest 里的 `http.port_env`:大写字母/数字/下划线、不以数字开头,且不是系统关键变量或 `BYTEHOST_*`。
/// (否则一个恶意清单可以让端口变量覆盖 `PATH`/`NODE_OPTIONS`/`LD_PRELOAD`,在子进程里执行任意代码。)
pub fn validate_port_env(name: &str) -> Result<(), PortEnvError> {
    if name.is_empty() {
        return Err(PortEnvError::Empty);
    }
    let ok = name
        .bytes()
        .enumerate()
        .all(|(i, b)| b.is_ascii_uppercase() || b == b'_' || (i > 0 && b.is_ascii_digit()));
    if !ok {
        return Err(PortEnvError::BadName(name.to_owned()));
    }
    if RESERVED.contains(&name)
        || name.starts_with("BYTEHOST_")
        || name.starts_with("DYLD_")
        || name.starts_with("LD_")
    {
        return Err(PortEnvError::Reserved(name.to_owned()));
    }
    Ok(())
}

pub struct EnvSpec<'a> {
    pub app_id: &'a AppId,
    pub port: u16,
    pub port_env: &'a str,
    /// 应用自己的 `data/` 目录(绝对路径)。
    pub data_dir: &'a Path,
    /// 宿主**显式**追加的变量(如应用私有的 `npm_config_cache`/`UV_CACHE_DIR`)。它们不经过父环境白名单——
    /// 白名单只管"从 dozerd 继承什么",宿主自己给的东西不该被它挡掉。排在白名单变量之后、`BYTEHOST_*` 之前。
    pub extra: &'a [(OsString, OsString)],
}

/// 组装子进程环境:父环境里的白名单变量 + 宿主给的 `BYTEHOST_APP_ID`/`BYTEHOST_DATA_DIR`/`BYTEHOST_HOST` + 端口变量。
/// `port_env` 必须已通过 [`validate_port_env`](这里不再检查,避免两处各写一份)。
pub fn build_env<I>(parent: I, spec: &EnvSpec<'_>) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut out: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(k, _)| {
            let k = k.to_string_lossy();
            PASS_EXACT.contains(&k.as_ref()) || PASS_PREFIX.iter().any(|p| k.starts_with(p))
        })
        .collect();
    out.extend(spec.extra.iter().cloned());
    let mut set = |k: &str, v: &OsStr| out.push((OsString::from(k), v.to_owned()));
    set("BYTEHOST_APP_ID", OsStr::new(spec.app_id.as_str()));
    set("BYTEHOST_DATA_DIR", spec.data_dir.as_os_str());
    set("BYTEHOST_HOST", OsStr::new("127.0.0.1"));
    set(spec.port_env, OsStr::new(&spec.port.to_string()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn only_allow_listed_parent_variables_reach_the_child() {
        let parent = [
            ("PATH", "/usr/bin"),
            ("HOME", "/Users/u"),
            ("LC_ALL", "en_US.UTF-8"),
            ("DOZER_SOCKET", "/x"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("GITHUB_TOKEN", "t"),
        ]
        .map(|(k, v)| (os(k), os(v)));
        let id = AppId::new("demo").unwrap();
        let data = PathBuf::from("/apps/demo/data");
        let env = build_env(
            parent,
            &EnvSpec {
                app_id: &id,
                port: 24001,
                port_env: "PORT",
                data_dir: &data,
                extra: &[],
            },
        );
        let keys: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for kept in [
            "PATH",
            "HOME",
            "LC_ALL",
            "BYTEHOST_APP_ID",
            "BYTEHOST_DATA_DIR",
            "BYTEHOST_HOST",
            "PORT",
        ] {
            assert!(keys.contains(&kept.to_string()), "{kept}: {keys:?}");
        }
        for dropped in [
            "DOZER_SOCKET",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "GITHUB_TOKEN",
        ] {
            assert!(!keys.contains(&dropped.to_string()), "{dropped} 不应继承");
        }
    }

    #[test]
    fn host_supplied_extras_reach_the_child_but_inherited_lookalikes_do_not() {
        let id = AppId::new("demo").unwrap();
        let data = PathBuf::from("/apps/demo/data");
        let extra = [(os("npm_config_cache"), os("/apps/demo/cache/npm"))];
        let env = build_env(
            [
                (os("npm_config_cache"), os("/home/u/.npm")),
                (os("UV_CACHE_DIR"), os("/home/u/.cache/uv")),
            ],
            &EnvSpec {
                app_id: &id,
                port: 1,
                port_env: "PORT",
                data_dir: &data,
                extra: &extra,
            },
        );
        let all: Vec<_> = env
            .iter()
            .filter(|(k, _)| k == "npm_config_cache")
            .collect();
        assert_eq!(all.len(), 1, "继承来的同名变量不放行,只有宿主给的那个");
        assert_eq!(all[0].1, os("/apps/demo/cache/npm"));
        assert!(
            !env.iter().any(|(k, _)| k == "UV_CACHE_DIR"),
            "没给的就不该出现"
        );
    }

    #[test]
    fn the_host_variables_carry_the_right_values_and_win_over_the_parent() {
        let id = AppId::new("demo").unwrap();
        let data = PathBuf::from("/apps/demo/data");
        // 父环境里就有同名变量时,宿主给的排在后面(后设置的覆盖先设置的)。
        let env = build_env(
            [(os("PATH"), os("/bin"))],
            &EnvSpec {
                app_id: &id,
                port: 31337,
                port_env: "APP_PORT",
                data_dir: &data,
                extra: &[],
            },
        );
        let get = |k: &str| {
            env.iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("APP_PORT").as_deref(), Some("31337"));
        assert_eq!(get("BYTEHOST_APP_ID").as_deref(), Some("demo"));
        assert_eq!(get("BYTEHOST_DATA_DIR").as_deref(), Some("/apps/demo/data"));
        assert_eq!(get("BYTEHOST_HOST").as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn port_env_names_are_validated() {
        for ok in ["PORT", "APP_PORT", "HTTP_PORT_1", "_X"] {
            assert_eq!(validate_port_env(ok), Ok(()), "{ok}");
        }
        assert_eq!(validate_port_env(""), Err(PortEnvError::Empty));
        for bad in ["port", "1PORT", "A-B", "A B", "PORT=1", "P\u{e9}RT", "A.B"] {
            assert!(
                matches!(validate_port_env(bad), Err(PortEnvError::BadName(_))),
                "{bad}"
            );
        }
    }

    /// 端口变量名不能成为覆盖关键变量的后门(`NODE_OPTIONS=--require /evil.js` 就是任意代码执行)。
    #[test]
    fn port_env_cannot_shadow_security_critical_variables() {
        for reserved in [
            "PATH",
            "HOME",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BYTEHOST_APP_ID",
            "BYTEHOST_X",
            "LD_ANYTHING",
            "DYLD_ANYTHING",
        ] {
            assert!(
                matches!(validate_port_env(reserved), Err(PortEnvError::Reserved(_))),
                "{reserved}"
            );
        }
    }
}
