# bytehost A6g 验收报告(升级、试用期自动回滚、手动回滚)

> 状态:**部分完成**。§1 的全部自动化项已实跑通过并给出逐条证据;**需要真实 GUI 的项(§2)尚未执行**——
> 执行会话无法操作 Dozer 窗口,这些项写成了待办清单,**不得据此把 A6g 标成"已完成"**。
> 对应计划:`docs/superpowers/plans/2026-10-08-bytehost-a6g-upgrade-and-rollback.md`。

## 0. 环境与对象

- 平台:macOS 26(Darwin arm64)。
- 运行时:`python3` = Python 3.13.1(系统解析;本切片的 live 用例只用到 python)。
- 任务提交:Task 1 `af2b87bd`、Task 2 `f8592f9a`、Task 3 `0a4f1600`、Task 4 `d1d3bc12`。
- 本切片新增/改动的关键文件:
  - `crates/bytehost-apps/src/registry.rs`:`AppRecord.{previous_version, probation, last_rollback}`、`RollbackNote`、`prune_versions`。
  - `crates/bytehost-apps/src/event.rs`:`AppEvent::{Upgraded, RolledBack}`。
  - `crates/bytehost-apps/src/manager.rs`:`install_staged` 的运行中升级(停→换→再启);`Core::rollback`/`rollback_locked`;`spawn_auto_rollback`;试用期钩子(`ready` 清 `probation`,`failed`/`down(None)`/`install_failed` 触发)。
  - `crates/bytehost-apps/src/proto.rs`:`AppRequest::Rollback`、`AppSummary.{previous_version, rollback_note}`(wire 只追加)。
  - `crates/dozerd/src/app_service.rs`、`crates/dozer-client/src/lib.rs`:承接/发起 `Rollback`。
  - `crates/dozer-app/src/extensions/settings_apps.rs`:行内版本、「回滚到 X」按钮与二次确认、`rollback_note` 常驻行、升级审批提示。
  - `crates/dozer-app/src/extensions/app_host.rs`:版本号变化触发面板重新加载。

## 1. 自动化结果——已验证

### 1.1 端到端 live 回归(Task 5 Step 1)

命令:

```
cargo test -p dozerd --test process_apps_live -- --ignored --nocapture
```

结果:**4 passed, 0 failed**(两进程型用例并行),其中新增用例单跑连跑 3 遍稳定(8.47s / 8.46s / 8.47s)。四个用例:

| 用例 | 覆盖 | 实测 |
|---|---|---|
| `python_sample_end_to_end` | A6e 的 11 步(升级改动不得回退其行为) | 通过 |
| `node_sample_end_to_end` | 同上(node) | 通过 |
| `a_failing_dependency_install_surfaces_a_dependency_issue` | A6f Task 3 | 通过 |
| `python_sample_upgrade_and_rollback` | **A6g**:运行中升级、手动回滚、试用期自动回滚 | 通过,~8.4s |

`python_sample_upgrade_and_rollback` 的步骤(全部经真实 gateway + 真 `python3` 子进程):

1. 装 1.0.0 → 启动 → 页面显示 `APPVERSION:1.0.0`(版本标记由测试注入),`GET /pid` 为纯数字 pid,`POST /hit` 后 `counter.txt` 计 1。
2. 升级到 1.1.0(运行中):**自动停→换→再启**;页面显示 `APPVERSION:1.1.0`、pid 与 1.0.0 不同(确认真换了进程)、计数仍为 1(数据跨升级保留);`List` 的 `previous_version == 1.0.0`。
3. 手动 `Rollback` → 1.0.0:页面显示 `APPVERSION:1.0.0`、计数仍为 1;`rollback_note.automatic == false` 且 `from == 1.1.0`/`to == 1.0.0`;`previous_version` 被消耗;再 `Rollback` → 错误类别 `NotFound`。
4. 升级到起不来的 1.2.0(`server.py` 改为启动即 `sys.exit(1)`):数秒内**自动回滚**回 1.0.0,页面显示 `APPVERSION:1.0.0`、计数仍为 1;`rollback_note.automatic == true` 且 `from == 1.2.0`/`to == 1.0.0`;再 `Rollback` 同样 `NotFound`。
5. 磁盘:`apps/<id>/package/` 下只剩 `1.0.0`——1.1.0 在装 1.2.0 时被"当前+上一版"清理,1.2.0 在自动回滚时清掉。

**审查结论**:无"快得不可能"的步骤重新出现。守卫(A6e 起"新旧 pid 必须为纯数字")在本用例里继续使用(`is_pid`),未回退。

### 1.2 组件/单元测试

| 命令 | 结果 |
|---|---|
| `cargo test -p bytehost-apps --all-features` | **335 passed, 0 failed**(含 Task 1/2/3 新增的升级/清理/试用期/自动回滚/手动回滚用例) |
| `cargo test -p dozerd`(lib + 全部集成) | lib **487 passed, 1 failed, 1 ignored**;唯一失败是已知无关偶发 `memory::tests::list_orders_by_updated_ms_desc`(单跑通过,见 §3) |
| `cargo test -p dozer-client` | 全部 `ok`(7 + 14 + … passed) |
| `cargo test -p dozer-app settings_apps` | **40 passed** |
| `cargo test -p dozer-app app_host` | **38 passed** |

### 1.3 门禁 / 格式 / lint

| 检查 | 结果 |
|---|---|
| `scripts/check-bytehost-apps-deps.sh` | ok |
| `scripts/check-log-scope.sh` | ok |
| `cargo fmt --check` | ok |
| `cargo clippy -p dozerd -p bytehost-apps --all-targets` | A6g 触碰的文件无新警告(唯一 warning 是既有的 `dozerd/src/preview_commands.rs:152` `let_underscore_future`,非本切片) |

## 2. 待人工在真实 GUI 里完成(**未执行**)

> 执行会话无法操作 Dozer 窗口。以下各项**不得**据此标成完成。对应计划 Task 4 Step 5。

- [ ] 装 `py-notes` 1.0.0 并启动、点几次让计数增加 → 把样例 `manifest.toml` 版本改成 1.1.0 并改页面文字后再装 → 审批卡显示"正在运行,会短暂中断" → 批准后面板**自动**显示新页面、计数还在。
- [ ] 装一个起不来的 1.2.0 → 数秒内设置页该行出现"已从 1.2.0 自动回滚到 1.1.0",面板回到 1.1.0。
- [ ] 点「回滚到 1.0.0」→ 确认文案:"回滚到 1.0.0:应用数据不会一起回滚,旧版本可能无法读取新版本写过的数据。确定回滚?" → 面板回到 1.0.0。

## 3. 发现的缺陷与已知局限

0. **计划文本自相矛盾(已按实现修正计划,非代码缺陷)**:原 Task 5 Step 1 第 3/4 条把"手动回滚"排在自动回滚之后,期望自动回滚到 1.1.0 后还能手动回 1.0.0。这与两处已裁决行为冲突——Task 1「只保留当前+上一版」(装 1.2.0 时 1.0.0 已被清理)、Task 2「回滚消耗上一版」。二者不可能同时成立。已按实现把 live 用例改为"手动回滚先做、自动回滚回到当前版本",并在计划里留追记(用户已裁决:按实现改测试)。

1. **已知无关的偶发/确定失败**(计划"全量"命令里已列,逐个注明,均未顺手改):
   - `dozerd`:`memory::tests::list_orders_by_updated_ms_desc` 全量并行时偶发失败,单跑通过。
   - `dozer-app`:`extensions::files::tests::delete_confirm_spec_reflects_pending_target`(clean tree 亦失败)。
   - `bytehost-apps`:`manager::tests::a_python_app_installs_starts_runs_behind_the_gateway_and_stops` 既有偶发(并行负载下 `Running` 事件未及时出现,pre-A6g 即存在)。

2. **已知局限(与计划"已知局限"一节一致,非缺陷)**:
   - 应用数据(`data/`)不随版本回滚(版本间共享),也没有升级前的数据快照。
   - 只有"当前 + 上一版"两个回滚目标;要回到更早版本只能重新安装并审批。
   - 回滚会提升权限时不自动处理:手动被拒(`Conflict`)、自动跳过(应用保持 `Failed`,`last_rollback.to == from` 并说明原因)。
   - 试用期以"首次 `ready`"为界;新版本健康后崩溃走普通重启策略,不触发自动回滚。
   - 静态应用没有试用期(启动是同步的)。
   - 升级来源仍只有本机目录(非本机来源 = A6h);容器运行时仍未实现。
