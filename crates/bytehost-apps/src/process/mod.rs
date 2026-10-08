//! 进程型应用的**监管核心**(bytehost A6a):把一个 Node/Python 应用当作子进程跑起来、盯着、停掉——
//! 只有机制,**不接** `AppManager`、gateway、协议(那是 A6b/A6c)。拆成互相独立、可单测的小块:
//!
//! - `restart`:崩溃后的重启退避与放弃(纯函数);
//! - `env`:给子进程的**白名单环境**(不继承 dozerd 的密钥/令牌)与端口环境变量名校验(纯函数);
//! - `log`:有大小上限、会轮转的日志文件与"把子进程输出泵进去"的线程;
//! - `health`:阻塞式 HTTP 健康探测(进程活着 ≠ 服务就绪);
//! - `supervise`:启动(独立进程组)、存活检查、优雅停止(SIGTERM→宽限→SIGKILL,整组)与孤儿清理。

pub mod env;
pub mod health;
pub mod log;
pub mod restart;
#[cfg(unix)]
pub mod supervise;

/// 端到端:真起一个 Python 静态服务器,走 `build_env` 的端口变量 → 健康探测 → 停止,端口随之关闭。
/// 机器上没有 `python3` 时跳过(开发机都有;不为测试引入别的依赖)。
#[cfg(all(test, unix))]
mod e2e {
    use super::env::{EnvSpec, build_env};
    use super::health::{Health, probe_http, wait_healthy};
    use super::supervise::{ProcessSpec, Running, pick_free_port};
    use crate::id::AppId;
    use std::time::Duration;

    #[test]
    fn a_python_server_is_started_probed_healthy_and_stopped_with_its_port_closing() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("跳过:没有 python3");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hi</h1>").unwrap();
        let port = pick_free_port().unwrap();
        let id = AppId::new("pytest-app").unwrap();
        let data = dir.path().join("data");
        let mut env = build_env(
            std::env::vars_os(),
            &EnvSpec {
                app_id: &id,
                port,
                port_env: "APP_PORT",
                data_dir: &data,
                extra: &[],
            },
        );
        // 子进程要找得到 python3:父 PATH 已在白名单里。
        let _ = &mut env;
        let mut r = Running::spawn(&ProcessSpec {
            argv: vec![
                "python3".into(),
                "-c".into(),
                "import os,http.server,socketserver; \
                 socketserver.TCPServer.allow_reuse_address=True; \
                 socketserver.ThreadingTCPServer(('127.0.0.1',int(os.environ['APP_PORT'])),http.server.SimpleHTTPRequestHandler).serve_forever()"
                    .into(),
            ],
            cwd: dir.path().to_path_buf(),
            env,
            log_path: dir.path().join("logs/app.log"),
            log_max_bytes: 1 << 20,
            log_keep: 2,
        })
        .unwrap();
        let health = wait_healthy(
            port,
            "/",
            Duration::from_secs(15),
            Duration::from_millis(100),
            || r.try_exit().ok().flatten().is_none(),
        );
        assert_eq!(
            health,
            Health::Healthy,
            "应用没起来;日志: {:?}",
            std::fs::read_to_string(dir.path().join("logs/app.log"))
        );
        r.stop(Duration::from_secs(3)).unwrap();
        assert!(
            matches!(
                probe_http(port, "/", Duration::from_millis(500)),
                Health::Unhealthy(_)
            ),
            "停止后端口应已关闭"
        );
    }
}
