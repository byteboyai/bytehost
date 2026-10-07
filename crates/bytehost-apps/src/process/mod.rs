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
