# bytehost A6f 验收报告(状态推送、依赖安装失败页、实时日志)

> 状态:**部分完成**。§1 的全部自动化项已实跑通过并给出逐条证据;**需要真实 GUI 的项(§2)尚未执行**——
> 执行会话无法操作 Dozer 窗口,这些项写成了待办清单,**不得据此把 A6f 标成"已完成"**。
> 对应计划:`docs/superpowers/plans/2026-10-08-bytehost-a6f-live-observation.md`。

## 0. 环境与对象

- 平台:macOS 26(Darwin arm64)。
- 运行时:`python3` = Python 3.13.1、`node` = v24.14.0(同 A6e,系统解析)。
- 任务提交:Task 1 `41f7381f`、Task 2 `2e47f162`(计划勾选 `b43f7004`)、Task 3 `e842d187`、Task 4 `2154a4d5`。
- 本切片新增/改动的关键文件:
  - `crates/bytehost-apps/src/proto.rs`:`AppRequest::Subscribe`、`AppReply::{Subscribed, Changed{app}, Resync}`、`AppIssue::DependencyInstall { summary }`(wire 只追加)。
  - `crates/bytehost-apps/src/supervisor.rs` / `manager.rs`:`Transitions::install_failed`(先记 issue 再改状态)。
  - `crates/dozerd/src/app_service.rs` / `server.rs`:订阅连接(先应答、再流式推送失效信号)。
  - `crates/dozer-client/src/lib.rs`:`AppChange` / `app_subscribe()`。
  - `crates/dozer-app/src/extensions/app_logs.rs`:共享日志查看器状态机(`app_host` 与 `settings_apps` 共用)。
  - `crates/dozer-app/src/extensions/app_host.rs` / `settings_apps.rs` / `app/view.rs` / `app/app.rs` / `platform/window_events.rs`:订阅消费、问题页、日志查看器入口与自动滚底。

## 1. 自动化结果——已验证

### 1.1 端到端 live 回归(Task 5 Step 1)

命令:

```
cargo test -p dozerd --test process_apps_live -- --ignored --nocapture
```

结果:**3 passed, 0 failed**,连跑 3 遍稳定(4.18s / 4.18s / 4.19s;两用例并行)。三个用例:

| 用例 | 覆盖 | 实测 |
|---|---|---|
| `python_sample_end_to_end` | A6e 的 11 步(推送不得改变其行为) | 通过,耗时与 A6e 报告一致量级 |
| `node_sample_end_to_end` | 同上(node) | 通过 |
| `a_failing_dependency_install_surfaces_a_dependency_issue` | A6f Task 3:装一个依赖安装必然失败的进程型应用 → `Start` 后 `List` 里出现 `Failed` + `AppIssue::DependencyInstall`,`Logs` 含安装器错误输出 | 通过 |

**审查结论**:无"快得不可能"的步骤重新出现。A6e 曾因把网关错误页正文(`application is not reachable`)当成新 pid 而假通过,那次缺陷的守卫(新旧 pid 必须为纯数字)在本切片回归里仍在,未回退。

### 1.2 变体/单元测试

| 命令 | 结果 |
|---|---|
| `cargo test -p bytehost-apps --all-features` | **315 passed, 0 failed**(含新增 `logs` 用例:轮转中途读、ANSI 进度条 + 6000 字符单行清洗封顶) |
| `cargo test -p dozerd`(lib + 全部集成) | 全部 `ok`(lib **487 passed, 0 failed, 1 ignored**) |
| `cargo test -p dozer-client` | 全部 `ok`(**7 passed** 等) |
| `cargo test -p dozer-app` | **1967 passed, 1 failed, 2 ignored**;唯一失败是已知无关的 `extensions::files::tests::delete_confirm_spec_reflects_pending_target`(clean tree 亦失败,非本切片引入) |

Task 4 相关 filter 单跑(证据):
- `cargo test -p dozer-app app_logs` → **9 passed**(含 4 个滚动/跟随用例)。
- `cargo test -p dozer-app app_host` → **35 passed**(A6e 的 6 个日志用例零改动通过,迁移正确性证据)。
- `cargo test -p dozer-app settings_apps` → **35 passed**。
- `cargo test -p bytehost-apps --all-features logs` → **16 passed**。

### 1.3 门禁 / 格式 / lint

| 检查 | 结果 |
|---|---|
| `scripts/check-bytehost-apps-deps.sh` | ok |
| `scripts/check-log-scope.sh` | ok |
| `cargo fmt --check` | ok |
| `cargo clippy -p dozer-app -p dozer-client --all-targets` | A6f 触碰的文件(`app_logs.rs`/`settings_apps.rs`/`app_host.rs`/`view.rs`/`app.rs`/`window_events.rs`)**无新警告** |
| `cargo clippy --all-targets` | 审阅后全树无 error(`dozer-core` 测试初始化已补齐,见 §3 缺陷 1) |

## 2. 待人工在真实 GUI 里完成(**未执行**)

> 执行会话无法操作 Dozer 窗口。以下各项**不得**据此标成完成。

- [ ] (Task 2 Step 5)**推送即时性**:装 `py-notes` → 点它的 `/crash` → 面板应在 <1s 内进入"启动中…",而不是等下一次 2s 轮询;`kill -9 $(pgrep dozerd)` 后 2s 内显示 dozerd 不可用,重启 dozerd 后自动恢复订阅(日志里能看到重订阅)。
- [ ] (Task 4 Step 6)**实时日志**:`py-notes` 运行中,在设置→应用里点「日志」;另开终端 `curl` 触发应用打印,日志在 ~1s 内出现新行;关闭设置页后用 `lsof`/日志确认不再有 `Logs` 请求;上滚后新行到来不被拉回底部。
- [ ] (Task 3)**依赖安装失败页**:装一个依赖安装必然失败的应用(如 node 样例 `package-lock.json` 引用不存在的包)→ 面板显示"依赖安装失败"问题页,动作是「查看日志」+「重试」(没有"去设置安装");点「查看日志」能看到安装器输出。
- [ ] 崩溃页「查看日志」:把 `server.py` 改成启动即崩的版本重装 → Crashed 页 + 「查看日志」可见输出末尾。

## 3. 发现的缺陷与已知局限

0. **审阅发现并已修**:设置页日志 `Tick` 定时器每条消息多排一条(改为 `tick_armed` 单链);应用面板日志在面板不可见时仍每秒刷新(`tick_logs`/`any_logs_open` 按可见槽位过滤);"先记 issue 再改状态"补确定性测试(攥住 issue 表锁,断言状态此时还不是 `Failed`)。

1. **既有缺陷(A6d 引入;审阅后已修:测试初始化补齐三个字段,`cargo clippy --all-targets` 与 `cargo test -p dozer-core` 恢复)**:**`cargo clippy --all-targets` 因 `dozer-core`(lib test)编译失败而中断**。`crates/dozer-core/src/protocol.rs:1611` 的 `RuntimeProbe { runtime, availability }` 初始化缺 `installable` / `job` / `managed` 三个字段。字段由 A6d `adb85a58` 加入 `bytehost-apps::proto::RuntimeProbe`,但 `protocol.rs`(最后改动于更早的 `0fcc66b6`)的该测试未同步更新。**不在 A6f 范围,未顺手改**;新代码的 `clippy` 结论按 1.3 逐包核对(不依赖全树 `--all-targets` 通过)。
2. **已知无关的偶发/确定失败**(在计划"全量"命令里已列,逐个注明):`extensions::files::tests::delete_confirm_spec_reflects_pending_target`(dozer-app,clean tree 亦失败)。本次 `dozerd --lib` 并行未复现 `app_service::tests::the_first_run_is_unavailable_when_the_port_cannot_be_saved` / `summary_provider::tests::absolute_script_can_find_sibling_interpreter_with_minimal_parent_path`。
3. **已知局限(与计划"已知局限"一节一致,非缺陷)**:
   - 推送只是"失效信号":每次变化 GUI 仍要 `List` 一次(本机 UDS,开销可忽略);换来的是只有一份状态真相。
   - 日志刷新是整段重读:每秒读末尾 ≤256 KiB,未做偏移增量;日志很大且频繁刷新时会多读,但封顶有界。
   - 推送不带日志内容与安装输出,查看器只能靠自己的 `Logs` 请求。
   - 订阅按连接:一个 GUI 窗口一条订阅,多窗口各订各的。
   - 容器运行时、应用升级与回滚(A6g)、非本机来源(A6h)不在本切片。
