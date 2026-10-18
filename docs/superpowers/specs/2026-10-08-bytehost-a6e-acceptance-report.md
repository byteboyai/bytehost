# bytehost A6e 验收报告(进程型应用体验:版本要求、问题页、日志、健康检查)

> 状态:**基本完成**(2026-10-18 对账)。§1 的全部自动化项已实跑通过并给出逐条证据;§2 需要真实 GUI 的项已于 2026-10-17/18 手工验收,
> **只剩一处未验**:`py-notes` 审批卡里"全部权限强制等级为仅声明,不强制"的标注(样例没有声明任何权限,审批卡写"不申请任何权限",看不到这个标注)。
> 初版写的"不得标成已完成"针对的是当时一项 GUI 都没做的状态,现已不成立;这一处未验补上之前不标"完成"。
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

## 2. 真实 GUI 手工验收(2026-10-17 / 2026-10-18)

> 初版由无法操作 Dozer 窗口的会话写成,当时全部未执行。2026-10-17、2026-10-18 已在真实 GUI 里手工验收,结果见 dozer `docs/superpowers/manual_tests/2026-10-16-bytehost-manual-acceptance.md`。下面勾选的是已验证的项;**未勾选的是仍未做或只验了一半的项**(逐项附有说明)。

- [ ] 设置→应用→安装应用…→选 `scripts/bytehost/samples/py-notes`:审批卡显示运行方式 `python`、命令行、"版本要求 >=3.9"、全部权限强制等级为"仅声明,不强制";批准并安装 → 图标栏出现 → 启动 → 面板里页面出现、计数、SSE 滚动、WebSocket 回显。
  **2026-10-17 手工验收:** `py-notes` 已做(**仍未验:权限标注**,见上方状态说明)。运行方式 `python`、命令行、版本要求 ✅;权限标注无法验(样例没有声明任何权限,审批卡写"不申请任何权限");页面、计数、SSE ✅。**WebSocket 原先在真实 WKWebView 里握手失败**(`close 1006`):网关追加了 `Connection: close`,WebKit 严格校验 101 的 `Connection` 头;bytehost `v0.4.5`(`14763a7`)修复,复测页面出现 `ws: hello` ✅。本报告当时的"WebSocket 同源升级成功"是用裸客户端测的,没经过 WKWebView。
- [x] 同上对 `node-notes`。
  **2026-10-18 手工验收:** 经"本机目录"装 `node-notes` 并启动:标题按 i18n 显示"Node 笔记"、`hits` 随刷新递增、`runtime: Node.js 24.14.0`(系统 node)、SSE tick 1–5、`ws: hello` WebSocket 回显 ✅,图标栏出现蓝色 "N" 首字母徽标。审批卡内容没有逐项记录。
- [x] 点页面上的「crash」→ 面板短暂变"启动中…"后自动恢复;点「hang」→ 约 45 秒后自动恢复(默认 15s×3)。
  **2026-10-17 手工验收:** crash 约 2 秒内恢复、hang 约 35 秒后恢复(这里写的"约 45 秒"偏保守);两条路径恢复后页面都会重载、计数保留。恢复期间面板显示"启动中…";hang 期间约 35 秒内面板仍显示崩溃前的旧页面、没有"无响应"提示——已裁决为有意行为。
- [x] 退出整个 Dozer → 重开 → 应用仍在运行(应用跟随 dozerd,GUI 退出不影响)→ 停 dozerd 后端口关闭 → 重启 dozerd 后自动恢复,计数没丢。
  **2026-10-17 手工验收:** dozerd 停止后端口关闭、重启后应用自动恢复、计数没丢 ✅。
  **2026-10-18 手工验收:** "退出整个 GUI"那一半用 `node-notes` 做完 ✅:⌘Q 退出后 GUI 进程消失,dozerd、node 进程与网关端口仍在且网关继续应答;重启 GUI 后连上的仍是原 dozerd、node 进程未重启、`gateway.json` 端口不变、应用直接 Running、计数续上(页面 `hits` 3,`counter.txt` 随后到 4,多出的 1 是之后又一次页面加载)。
- [x] 运行时缺失/版本不符的提示页(A6e Task 5 的三步手动验收:装 `python = ">=99"` 的副本看问题页与「去设置安装」;藏起 node 看"尚未安装";装好后回面板「重试」)。
  **2026-10-17 手工验收:** `python = ">=99"` 的副本 → 问题页"需要 Python / 要求 >=99,当前 3.13.1",带「去设置安装」「重试」✅。
  **2026-10-18 手工验收:** 藏起 node ✅:面板显示"需要 Node.js / 尚未安装,可在 设置 → 应用 里一键安装",按钮「去设置安装」「重试」;把独立的 node(v24.14.0)放进 dozerd 的 `PATH` 目录后点「重试」,面板恢复运行、页面显示 `Node.js 24.14.0`(此前系统 node 是 26.11.1,说明重试重新解析了运行时)。**构造方法:** 只改 `PATH` 藏不住 node,`resolve.rs` 总会追加 `/opt/homebrew/bin`、`/usr/local/bin`,所以用 `sandbox-exec` 在沙箱里拒绝访问 node 可执行文件,不改系统。
- [x] 把 `server.py` 改成启动即崩的版本重装 → Crashed 页 + 「查看日志」可见输出末尾。
  **2026-10-17 手工验收:** 约 40 秒后出现 Crashed 页(退避 1/2/4/8/16 秒重启 5 次才放弃,期间只显示"启动中…"——已裁决为有意行为),点「查看日志」可见输出末尾(6 行 = 1 次首启 + 5 次重启)。深色主题下日志文字原先几乎看不见,dozer `79737edd` 修复。
- [x] 卸载"保留数据"再装:计数还在;"含数据":计数归零。
  **2026-10-17 手工验收:** 保留数据:计数保留(卸载前后都是 4);含数据:数据目录被清除、webview 数据存储被清,重装启动后 `hits: 1`。
- [x] 受管运行时:设置里装 Node(A6d 手工项)后,`node-notes` 用的是受管版本(页面显示的 node 版本 = 受管版本)。
  **2026-10-18 手工验收:** 审批卡显示 Node 24.21.0、来源 nodejs.org 的 darwin-arm64 压缩包、SHA-256 `bed7eea5…6057`(与代码钉死的值、nodejs.org 官方 `SHASUMS256.txt` 三方一致)、安装位置 `runtimes/node/24.21.0`、"不修改系统环境与 shell 配置"。安装后 `bin/node -v` = v24.21.0(199M,无残留临时文件);启动 `node-notes`:页面 `runtime: Node.js 24.21.0`(同机系统 node 是 24.14.0 与 26.11.1,所以能区分),应用进程是 `runtimes/node/24.21.0/bin/node`,父进程是 dozerd ✅。

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
