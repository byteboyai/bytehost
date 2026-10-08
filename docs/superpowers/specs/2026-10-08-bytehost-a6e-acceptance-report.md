# bytehost A6e 验收报告(进程型应用体验:版本要求、问题页、日志、健康检查)

> 状态:**部分完成**。§1 的全部自动化项已实跑通过并给出逐条证据;**需要真实 GUI 的项(§2)尚未执行**——
> 执行会话无法操作 Dozer 窗口,这些项写成了待办清单,**不得据此把 A6e 标成"已完成"**。
> 对应计划:`docs/superpowers/plans/2026-10-08-bytehost-a6e-process-app-experience.md`。

## 0. 环境与对象

- 平台:macOS 26(Darwin arm64)。
- 运行时(系统解析到的解释器):`python3` = Python **3.13.1**、`node` = **v24.14.0**(`/usr/bin/python3` 为 3.9.6,但 PATH 上先命中 3.13.1)。
- 示例应用(仅标准库、无第三方依赖、无 lockfile):
  - `scripts/bytehost/samples/py-notes/`(`manifest.toml` + `server.py`,kind = `python`,command `["python3","server.py"]`,`python = ">=3.9"`,`port_env = "PORT"`,`[health] path = "/"`)。
  - `scripts/bytehost/samples/node-notes/`(`manifest.toml` + `server.js`,kind = `node`,command `["node","server.js"]`,`node = ">=18"`)。
  - 两个脚本语义一致:`GET /`(HTML,含计数与 `runtime: <name> <ver>`,内联 `EventSource('/events')` + `WebSocket` 脚本)、`POST /hit`(+1 返回 JSON)、`GET /events`(SSE,200ms 一条 `data: tick N`,共 5 条)、`GET /ws`(手写 RFC 6455 握手,文本帧回显)、`GET /pid`、`GET /hang`(此后所有请求卡死)、`GET /crash`(`os._exit(3)` / `process.exit(3)`);计数持久化在 `BYTEHOST_DATA_DIR/counter.txt`;启动打印 `listening on`。
- 语法检查:`python3 -m py_compile server.py` 通过;`node --check server.js` 通过。
- 测试文件:`crates/dozerd/tests/process_apps_live.rs`(两个 `#[ignore]` 测试,`#![cfg(unix)]`;缺 `python3`/`node` 会显式 `panic`,不静默跳过)。用真 `ChainResolver` + 真 gateway + 真监管线程,`AppService` 起在临时根目录;监视参数经测试钩子注入为 300ms 间隔 / 400ms 超时 ×3(默认 15s×3)、`RestartPolicy` 退避调小为 200ms;钩子与生产走同一个 `finish_start_with`。

## 1. 自动化端到端结果——已验证

命令:

```
cargo test -p dozerd --test process_apps_live -- --ignored --nocapture
```

结果:**2 passed, 0 failed**(修正后连跑 3 遍稳定:4.25s / 4.16s / 4.17s;两个用例并行)。逐条断言与实测耗时(py-notes / node-notes):

| # | 断言 | 证据(实测) |
|---|---|---|
| 1 | `Plan`→`Install`→`Start`→轮询到 `Running`(≤20s) | 通过;py +214ms、node +209ms |
| 2 | 带会话 Cookie 经 gateway 请求 `/` = 200 且正文含 `runtime:` + `hits: 0`;**不带 Cookie = 403 且应用侧计数不变** | 通过;+1~2ms |
| 3 | SSE:`/events` 在 1.5s 内收到 ≥3 条 `tick`(流式) | 通过;py 收到 3 tick 用 +409ms、node +604ms |
| 4 | WebSocket:同源升级成功并回显 `hello`;**跨源 `Origin` 升级 = 403** | 通过;+2ms |
| 5 | `POST /hit` 同源 `Origin` → 计数 1;**`Origin: http://evil.example` → 403 且计数不变** | 通过;+2ms |
| 6 | `GET /pid` 记 pid(须为纯数字)→ `GET /crash` → 监管重启 → 新 pid(同样须为纯数字、≠旧)且重 `Running`(≤20s) | 通过;py +517~529ms、node +516~625ms(含 200ms 退避 + 进程启动 + 健康等待) |
| 7 | 计数仍是 1(`BYTEHOST_DATA_DIR` 持久化,跨进程重启保留) | 通过;+0ms |
| 8 | `GET /hang`(前置:卡死前 pid 须为纯数字)→ 健康检查连续失败达阈值后杀掉并重启,观察到新的纯数字 pid | 通过;py +2603~2622ms、node +2536~2631ms(注入监视 300ms 间隔 / 400ms 超时 ×3) |
| 9 | `Stop` → `LaunchUrl` 不再给;经 gateway 请求该站点 = **404**(站点注销);`Uninstall(ProgramAndData)` 后整个应用目录消失 | 通过;py +220ms、node +233ms(含卸载的进程收尾) |
| 10 | `Logs` 返回文本含启动行 `listening on` | 通过;+0ms |
| 11 | 装一份声明 `>=99` 的副本 → `Start` 返回 `AppErrorKind::Unavailable`;`List` 里 `issue == Some(RuntimeVersion{..})` 且 `observed == Failed` | 通过;py +30ms、node +42ms |

合计单用例墙钟 ≈ 3.7~4.0s(两用例并行)。

**退出码**:`0`。

## 2. 待人工在真实 GUI 里完成(**未执行**)

> 执行会话无法操作 Dozer 窗口。以下各项**不得**据此标成完成。

- [ ] 设置→应用→安装应用…→选 `scripts/bytehost/samples/py-notes`:审批卡显示运行方式 `python`、命令行、"版本要求 >=3.9"、全部权限强制等级为"仅声明,不强制";批准并安装 → 图标栏出现 → 启动 → 面板里页面出现、计数、SSE 滚动、WebSocket 回显。
- [ ] 同上对 `node-notes`。
- [ ] 点页面上的「crash」→ 面板短暂变"启动中…"后自动恢复;点「hang」→ 约 45 秒后自动恢复(默认 15s×3)。
- [ ] 退出整个 Dozer → 重开 → 应用仍在运行(应用跟随 dozerd,GUI 退出不影响)→ 停 dozerd 后端口关闭 → 重启 dozerd 后自动恢复,计数没丢。
- [ ] 运行时缺失/版本不符的提示页(A6e Task 5 的三步手动验收:装 `python = ">=99"` 的副本看问题页与「去设置安装」;藏起 node 看"尚未安装";装好后回面板「重试」)。
- [ ] 把 `server.py` 改成启动即崩的版本重装 → Crashed 页 + 「查看日志」可见输出末尾。
- [ ] 卸载"保留数据"再装:计数还在;"含数据":计数归零。
- [ ] 受管运行时:设置里装 Node(A6d 手工项)后,`node-notes` 用的是受管版本(页面显示的 node 版本 = 受管版本)。

## 3. 发现的缺陷与已知局限

- **测试缺陷(审核发现,已修)**:本报告的初版把 6、7、8 记为通过,**那是假通过**。`wait_new_pid_async` 把任何非空的 `/pid` 正文当成"新 pid";应用刚崩溃、监管还没重启时,网关对已死上游返回的错误页正文 `application is not reachable` 被当成了新 pid(实测 `old="71594" new="application is not reachable"`),步骤 8 的"卡死前 pid"也是同一段错误文本。初版记的 `+3~5ms`、`+515ms` 在 200ms 退避 / 200ms 监管轮询的条件下本就不可能是真实重启,当时把它解释成"重启已完成才轮到轮询"是错的。修正:新旧 pid 都必须是纯数字,步骤 6、8 的前置 pid 也加了断言。**变异验证**:把监视失败阈值注入为 0(关闭检查)后,步骤 8 超时失败。修正后 A6 的产品逻辑(崩溃重启、卡死检测)经真实重启验证成立,无产品缺陷。
- 首次实跑时还发现测试自身的断言写错(把 gateway 端口当作应用端口,断言"停止后端口关闭"——实际 gateway 仍在监听,只是站点注销),已改为断言经 gateway 请求得到 404;这是**测试的错**,不是 A6 的缺陷。
- 已知局限(与计划"已知局限"一节一致,非缺陷):
  - `uv run` 管理的 Python 版本不校验(解释器由 uv 运行时挑选,宿主拿不到)。
  - 版本检查是启动时一次性的;解释器之后被卸载/升级,只在下次启动时发现。
  - 健康检查只看 HTTP 状态行(与启动期同判据);"能应答但业务坏了"不在范围。
  - 日志是只读快照(点一次读一次,无实时 tail);超过 256 KiB 取末尾。
  - 问题页只覆盖"运行时缺失/版本不符";依赖安装失败仍显示为 `Crashed` + 日志。
  - 容器运行时仍未实现;规格验收 8 的"容器不可用提示页"继续用"安装时明确拒绝"代替。
  - 进程型应用出站网络强制等级仍为 `Advisory`;没有沙箱。
