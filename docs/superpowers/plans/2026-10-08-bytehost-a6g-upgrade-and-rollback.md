# bytehost A6g:应用升级与回滚 Implementation Plan

> **归档说明(2026-10-08,A7 之后搬家):** 本计划**已在 dozer 仓库执行完成**(所有 Task 均 `[x]`,提交见各 Task 的 Step 5/6),随后随 A7 把 `crates/bytehost-*` 抽到本仓库。文档原样搬来作为执行记录,因此正文里的路径是**执行当时**的布局:
> - `crates/bytehost-apps/**`(升级/回滚/试用期的核心逻辑)现在在**本仓库**;
> - `crates/dozerd/**`、`crates/dozer-client/**`、`crates/dozer-app/**`(`app_service.rs` 承接、`Client::app_rollback`、设置页与面板重载)仍留在 **dozer 仓库**;
> - 门禁脚本已改名:本仓库是 `scripts/check-deps.sh`,dozer 保留 `scripts/check-log-scope.sh`;
> - 正文里的 commit 短哈希属于 **dozer 仓库历史**(搬运时 `git filter-repo` 保留了历史,哈希与提取前的 dozer 源码一致)。
>
> 未来同类改造:核心逻辑改在**本仓库**,消费者接线改在 **dozer**。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 已安装的应用能安全升级:运行中也能升(自动停→换→再启)、新版本起不来时自动回到上一版、用户随时可手动回滚,旧版本包只保留"当前 + 上一版"。

**Architecture:** 底层升级已存在(`install_staged`:同版本拒绝、权限对已授予做 diff、包目录不可变、`current_version` 最后才写)。本计划补三块:(1) `AppRecord` 追加 `previous_version`/`probation`/`last_rollback`,升级时记下上一版并清理更早的版本;(2) "试用期"——升级后的首次启动成功(`ready`)才算站稳,试用期内进入 `Failed` 则由**独立线程**自动回滚(绝不在监管线程回调里做,会和 `cancel_supervision` 的 join 自锁);(3) 手动回滚 `AppRequest::Rollback`。权限规则保守:**回滚不得提升权限**。

**Tech Stack:** Rust(`bytehost-apps`、`dozerd`、`dozer-client`、`dozer-app`)、serde(记录与 wire 只追加)、iced 纯状态机。

**Spec:** `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`(§5 安装计划/审批、`ManifestChanged` 事件、`current_version` 最后才写的升级中断语义)。前序:`plans/2026-10-08-bytehost-a6e-process-app-experience.md`;A6f(`plans/2026-10-08-bytehost-a6f-live-observation.md`)与本计划**互相独立**,但都改 `AppTransitions`,见 Task 2 Step 0。

## 用户已裁决的行为(2026-10-08)

1. 升级运行中的应用:**自动停 → 升 → 再启**,不要求用户先手动停止。
2. 升级后新版本起不来:**失败自动回滚 + 随时可手动回滚**。
3. 旧版本保留:**当前 + 上一版**,更早的清理。

## 本计划自己做的决定(请评审时确认或推翻)

- **回滚不得提升权限。** 目标版本的权限 = 它自己清单里的 `permissions`(安装时 `grants` 就等于申请的);若相对**当前已授予**有任何 `escalation`,则**手动回滚被拒绝**(`Conflict`,提示"回滚会提升权限,请重新安装该版本并审批"),**自动回滚被跳过**(应用保持 `Failed`,`last_rollback` 里写明原因)。这样不需要新的审批流程。
- **应用数据不回滚。** `data/` 在版本间共享;旧版本遇到新版本写过的数据格式可能出错。回滚确认文案必须如实说明,不做数据快照(已知局限)。
- **回滚消耗"上一版":** 回滚后 `previous_version = None`,被换下的那个版本的包目录和版本记录一并清理(用户要它可以重新安装并审批)。

## Global Constraints

- `bytehost-apps` 默认 feature 依赖只能是 serde 家族;`scripts/check-bytehost-apps-deps.sh`、`scripts/check-log-scope.sh` 必须通过。
- `AppRecord` 的新字段一律 `#[serde(default)]`,**不升 `RECORD_FORMAT_VERSION`**(`registry.rs:215` 对格式版本做严格相等校验);旧 `state.json` 必须照常读出。
- wire 只追加:`AppSummary` 新字段 `#[serde(default, skip_serializing_if = "Option::is_none")]`;`AppRequest`/`AppEvent` 只加变体。
- **监管线程回写状态必须持锁并核对取消标志、拿锁用尝试循环**(`Core::try_guard`);`AppTransitions` 里**不得**调用 `start_locked`/`stop_locked`/`cancel_supervision`(它们 join 监管线程,在监管线程自己里会自锁)。自动回滚一律交给独立线程。
- 持久状态(回滚记录、"可回滚到 X")留在页面原位,**不进 Toast**;一次性的"回滚已完成/被拒"可以 Toast。
- 日志统一走 `dozer_core::log_*!`;面板日志来源名规则同 CLAUDE.md。
- 提交前 `git diff --cached --stat` 看全貌,只 `git add` 指定路径;不动工作区里已有的 `Cargo.lock` 改动。
- 变异验证:每条新行为的测试都要临时去掉实现确认会失败,再恢复。

## Review Focus

(spec 与现有代码隐含、没有任务测试就会咬到真实用户的输入/失败模式,最可能的排前面;每条在对应任务里有测试。)

1. **升级到一个起不来的新版本时,dozerd 此刻被重启**:试用期标记必须落盘,重启后的启动对账仍能触发回滚,不能留下"新版本 Failed、没人回滚"(Task 2)。
2. **自动回滚与用户操作并发**:用户在回滚线程拿锁前手动停止/卸载/再次升级,回滚线程必须重新核对状态后放弃,不得把应用拉起来(Task 2)。
3. **回滚会提升权限**(新版本降了权限且失败,旧版本要得更多):不得悄悄放大授权(Task 2、3)。
4. **同一个应用连续升级三次**:更早的版本包目录和记录必须真的被清掉,且被清掉的版本号可以再装,保留的两个不能被覆盖(Task 1)。
5. **升级中途被打断**(包已改名落位、记录还没写):旧版本必须仍可启动,残骸不挡重试——现有 `debris_from_a_crashed_install...` 不得变红(Task 1)。
6. **升级后 GUI 面板还显示旧页面**:快速的停→启可能被 2s 轮询漏掉,面板必须因"版本号变了"重新加载(Task 4)。
7. **没有可回滚的上一版**(首次安装、或刚回滚过):不提供回滚入口,请求被明确拒绝而不是静默成功(Task 3、4)。

## File Structure

| 文件 | 变更 | 职责 |
|---|---|---|
| `crates/bytehost-apps/src/registry.rs` | 改 | `AppRecord` 追加 `previous_version`、`probation`、`last_rollback: Option<RollbackNote>`;`RollbackNote` 类型;`prune_versions` |
| `crates/bytehost-apps/src/event.rs` | 改 | `AppEvent::{Upgraded, RolledBack}` |
| `crates/bytehost-apps/src/manager.rs` | 改 | `install_staged` 支持运行中升级;试用期钩子;`rollback_locked`;`spawn_auto_rollback`;`Core::rollback` |
| `crates/bytehost-apps/src/proto.rs` | 改 | `AppRequest::Rollback{id}`;`AppSummary.{previous_version, rollback_note}`;`RollbackNote` 线上形状 |
| `crates/dozerd/src/app_service.rs` | 改 | 承接 `Rollback` |
| `crates/dozer-client/src/lib.rs` | 改 | `Client::app_rollback` |
| `crates/dozer-app/src/extensions/settings_apps.rs` | 改 | 行内版本/"回滚到 X"/回滚记录;升级审批卡提示 |
| `crates/dozer-app/src/extensions/app_host.rs`、`app/app.rs` | 改 | 版本号变化触发面板重载 |
| `crates/dozerd/tests/process_apps_live.rs` | 改 | 真实 python 的升级/回滚用例 |
| `docs/superpowers/specs/2026-10-08-bytehost-a6g-acceptance-report.md` | **新** | 验收报告(只填实际运行结果) |

---

### Task 1: 记录模型、版本清理、运行中升级

**Files:** Modify `registry.rs`、`event.rs`、`manager.rs`;Test: 同文件 `mod tests`。

**Interfaces:**
- Produces(`registry.rs`):
  ```rust
  // AppRecord 追加(全部 #[serde(default)])
  pub previous_version: Option<Version>,   // 升级前的 current_version;首次安装/回滚后为 None
  pub probation: bool,                     // 升级后的首次启动尚未成功
  pub last_rollback: Option<RollbackNote>,
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct RollbackNote { pub from: Version, pub to: Version, pub reason: String, pub automatic: bool, pub at_ms: u64 }
  impl AppRecord {
      /// 只保留 `keep`(当前与上一版)的版本记录,返回被移除的版本号;包目录由调用方删。
      pub fn prune_versions(&mut self) -> Vec<Version>;
  }
  ```
- Produces(`event.rs`):`AppEvent::Upgraded { app, from: Version, to: Version }`、`AppEvent::RolledBack { app, from: Version, to: Version, reason: String, automatic: bool }`(`app()` 的 match 同步补)。
- `install_staged` 行为变化:
  - 现有状态门槛 `Installed | Stopped | Failed{..}` 放宽到**也接受 `Running | Starting | Preparing`**;`Stopping`(若存在)仍 `Busy`。
  - 升级(`existing.is_some()`)时:记 `was_running = desired == Running`;若当前在跑,**先 `stop_locked`**(持着 `install_staged` 已拿的锁;该函数 join 监管线程,不会自锁);包落位 + 写记录:`previous_version = Some(旧 current)`、`probation = was_running`(没在跑就没有"首次启动"可试,不设试用期)、`last_rollback = None`、`grants = 新申请`;然后 `prune_versions()` 并删除被清理版本的包目录;发 `Upgraded`;若 `was_running` 则 `start_locked`——启动失败(同步返回 `Err`)时的处理见 Task 2。
  - 全新安装:三个新字段取默认。

- [x] **Step 1: 写失败测试**(`manager.rs`,沿用 `rig`/`write_app`/静态应用 fixture;进程型用 `write_py_app` + `python3`,缺 python3 则 `return`)
  - `an_upgrade_records_the_previous_version`:装 1.0.0 → 装 1.1.0 → `previous_version == Some(1.0.0)`、`current_version == 1.1.0`、`probation == false`(应用没在跑)。
  - `three_successive_upgrades_keep_only_current_and_previous`(Review Focus 4):装 1.0.0/1.1.0/1.2.0 → 1.0.0 的包目录与 `versions` 记录都没了,1.1.0 与 1.2.0 还在;再装 1.0.0 成功(不是 `AlreadyInstalled`)。(注:重装 1.0.0 后它成为 current,`prune_versions` 会把 1.1.0 清掉,所以不断言 1.1.0/1.2.0 仍是 `AlreadyInstalled`。)
  - `upgrading_a_running_static_app_stops_swaps_and_restarts_it`:装 1.0.0、`start`、经 gateway 请求拿到旧页面内容;装 1.1.0(不同页面内容)→ `observed == Running`、`desired == Running`、经 gateway 请求拿到**新**页面内容;事件顺序含 `Upgraded`。
  - `upgrading_a_running_process_app_sets_probation_until_ready`:python 应用升级后记录 `probation == true`,等到 `Running` 后 `probation == false`(`ready` 清除,见 Task 2 Step 3;本用例在 Task 2 才能全绿,Task 1 里先断言"升级后立刻 `probation == true`")。
  - `an_upgrade_interrupted_after_rename_keeps_the_old_version_startable`(Review Focus 5):复用现有 `debris_from_a_crashed_install_never_blocks_a_retry_or_an_upgrade` 的构造,断言 `previous_version`/`probation` 没被半写(记录要么全是旧的,要么全是新的)。
  - `registry.rs`:旧 `state.json`(无三个新字段)读出为 `previous_version: None, probation: false, last_rollback: None`;新字段 round-trip;`prune_versions` 在 `previous_version = None` 时只留 current,在有 previous 时留两个,返回被移除版本且不含 current/previous。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p bytehost-apps --all-features manager registry event`;`cargo test -p bytehost-apps`(默认 feature)与依赖门禁)。变异:去掉 `prune_versions` 调用,用例 `three_successive...` 失败;去掉运行中升级前的 `stop_locked`,静态应用用例失败。
- [x] **Step 5: Commit** — `feat(bytehost-apps): upgrade keeps current and previous versions and handles running apps (A6g task 1)`。

---

### Task 2: 试用期与自动回滚

**Files:** Modify `manager.rs`(`AppTransitions`、新 `spawn_auto_rollback`/`rollback_locked`)、`registry.rs`(无新类型);Test: `manager.rs` 的 `mod tests`、`crates/bytehost-apps/src/supervisor.rs` 若需要新增 `Transitions` 假实现。

**Interfaces:**
- Produces:
  ```rust
  impl Core {
      /// 回滚到 `previous_version`(调用方已持锁)。`automatic` 只影响 `RollbackNote`/事件。
      /// 目标版本权限相对当前授予有 escalation → `Err(ManagerError::RollbackEscalates)`(不改任何状态)。
      fn rollback_locked(&self, id: &AppId, reason: String, automatic: bool, now_ms: u64) -> Result<(), ManagerError>;
      /// 试用期内应用进入 Failed 时,从**新线程**调用;线程里 `guard()` 取锁(阻塞即可,它不是监管线程),
      /// 重新核对后才回滚。
      fn spawn_auto_rollback(&self, id: AppId, failed_version: Version);
  }
  // ManagerError 追加
  RollbackEscalates, NoPreviousVersion(AppId)    // 映射:Conflict / NotFound
  ```
- `rollback_locked` 步骤(锁内):`stop_locked` → 校验 escalation(`diff_permissions(&record.grants, &prev_manifest.permissions).iter().any(|c| c.escalation)`)→ `grants = prev_manifest.permissions` → `current_version = previous`、`previous_version = None`、`probation = false`、`last_rollback = Some(note)` → 清理被换下版本的包目录与版本记录 → `observed = Stopped`(若原本是 `Failed` 先复位)→ 发 `RolledBack` → 若 `desired` 原本是 Running(`stop_locked` 会把 desired 改成 Stopped,**所以进来前先记下**)则 `start_locked`。
- 触发点:
  1. `AppTransitions::ready`:在已有逻辑里,若 `probation` 则置 `false` 并存盘(试用期成功)。
  2. `AppTransitions::failed(..)` 与 `AppTransitions::down(.., None)`(放弃重启):在写完 `Failed` 后,若记录 `probation == true`,调 `self.core.spawn_auto_rollback(id, 当前版本)`。`restart_in = Some(_)` 的 `down` **不触发**(还在重启策略的重试里)。
  3. `start_locked` 的同步错误路径(`RuntimeUnavailable`/`RuntimeVersion`/`MissingSource` 等):`install_staged` 里的"升级后再启"若 `Err` 且 `probation`,**直接在同一把锁里** `rollback_locked(.., automatic: true)`,把原错误作为 `reason`;`install` 本身仍返回 `Ok(())`(升级动作完成了,只是被回滚),结果经事件与 `last_rollback` 体现。
- `spawn_auto_rollback` 线程:`guard()` → 重新 `load_record` → 仅当 `probation && current_version == failed_version && observed 是 Failed{..} && !closed` 才调 `rollback_locked`;否则什么都不做(Review Focus 2)。`rollback_locked` 返回 `RollbackEscalates` 时:不动状态,把原因写进 `last_rollback`(`to == from`,`reason` 说明"回滚需要更高权限,未自动回滚"),`probation = false`,应用保持 `Failed`。
- **Step 0(A6f 先落地时):** A6f 的 `AppTransitions::install_failed`(依赖安装失败)同样要在写完 `Failed` 后走第 2 点的触发逻辑——抽成私有 `fn after_failed(&self)` 供 `failed`/`down(None)`/`install_failed` 共用。A6g 先落地则 A6f 实现该方法时照此接入。

- [x] **Step 1: 写失败测试**
  - `a_new_version_that_never_becomes_healthy_is_rolled_back_automatically`:python 应用 1.0.0 正常;1.1.0 的命令立即退出(`python3 -c "import sys; sys.exit(1)"`),用 `with_resolver_and_policy_for_test` 注入小退避让放弃在毫秒级完成 → 等到 `current_version == 1.0.0`、`observed == Running`、`last_rollback == Some{automatic: true, from: 1.1.0, to: 1.0.0}`、`probation == false`、`previous_version == None`、1.1.0 的包目录与记录已清;事件含 `RolledBack`。
  - `a_synchronous_start_failure_after_upgrade_rolls_back_in_the_same_call`:新版本声明 `python = ">=99"`(A6e 的版本校验同步失败)→ `install` 返回 `Ok`,返回时 `current_version` 已是旧版本、旧版本在跑。
  - `a_healthy_new_version_ends_probation_and_is_not_rolled_back`:新版本正常 → `ready` 后 `probation == false`,`previous_version` 仍是旧版(手动回滚还可用)。
  - `a_rollback_that_would_escalate_permissions_is_skipped`(Review Focus 3):1.0.0 申请 `network = lan`,1.1.0 申请 `network = none` 且起不来 → 不回滚,应用 `Failed`,`last_rollback.to == from` 且 `reason` 含"更高权限",`current_version` 仍是 1.1.0。
  - `rollback_is_abandoned_when_the_user_acted_first`(Review Focus 2):让测试线程先占住 `lock`,等 `spawn_auto_rollback` 的线程阻塞在 `guard()` 上,再在占锁期间把应用 `stop`(改 desired/observed)后放锁 → 线程核对后放弃,应用仍是 `Stopped`、`current_version` 不变。变异:去掉核对,用例失败。
  - `probation_survives_a_daemon_restart`(Review Focus 1):升级到起不来的新版本,**在监管线程放弃前**用新的 `AppManager` 实例(同一 root)模拟 dozerd 重启并 `reconcile()` → 新实例启动对账里 `probation == true` 的应用启动失败后仍触发回滚;最终 `current_version == 旧版本`。
  - 现有 `supervisor`/`manager` 测试(崩溃重启、give-up、`stop` 不死锁)全部保持通过。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(连跑 3 遍:`for i in 1 2 3; do cargo test -p bytehost-apps --all-features manager:: || break; done`——并发相关,不接受偶发)。变异:让 `failed` 里直接调 `rollback_locked`(不开线程),必须能复现自锁/死锁的失败(用例超时即视为失败);把 `ready` 里清 `probation` 去掉,"健康新版本"用例失败。
- [x] **Step 5: Commit** — `feat(bytehost-apps): probation after upgrade with automatic rollback on a separate thread (A6g task 2)`。

---

### Task 3: 手动回滚(wire / dozerd / client)

**Files:** Modify `proto.rs`、`manager.rs`(`Core::rollback`)、`app_service.rs`、`crates/dozer-client/src/lib.rs`;Test: `proto.rs`、`manager.rs`、`app_service.rs` 的 `mod tests`、`crates/dozerd/tests/app_requests.rs`。

**Interfaces:**
- Produces(`proto.rs`):
  ```rust
  // AppRequest 追加
  Rollback { id: AppId },                         // → AppReply::Done
  // AppSummary 追加
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub previous_version: Option<Version>,          // 有值才显示"回滚到 X"
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub rollback_note: Option<RollbackNote>,        // wire 上与 registry::RollbackNote 同形(复用同一类型,从 proto 再导出)
  ```
- `Core::rollback(&self, id) -> Result<(), ManagerError>`:`guard()` → `rollback_locked(.., reason: "用户手动回滚", automatic: false, ..)`;`previous_version == None` → `NoPreviousVersion`(`NotFound`);`RollbackEscalates` → `Conflict`;其余状态门槛同 `stop`(`Busy` 视为 `Conflict`)。
- `Client::app_rollback(&self, id: AppId) -> Result<()>`。

- [x] **Step 1: 写失败测试**
  - `proto.rs`:`Rollback` 序列化 `{"op":"rollback","id":"..."}`;`AppSummary` 新字段缺省时旧 JSON 照常解析、有值时形状固定。
  - `manager.rs`:装 1.0.0、1.1.0 → `rollback` 成功:`current_version == 1.0.0`、`previous_version == None`、`last_rollback.automatic == false`、1.1.0 包目录已清;**再次 `rollback`** → `NoPreviousVersion`(Review Focus 7);首次安装的应用 `rollback` → 同样;回滚会提升权限 → `RollbackEscalates` 且状态完全未变(逐字段比较前后记录);应用原本 `Running` → 回滚后仍 `Running`(新旧版本内容可区分);`list()` 的 `previous_version`/`rollback_note` 随之变化。
  - `app_service.rs`:`Rollback` 各错误类别映射(`NoPreviousVersion` → `NotFound`,`RollbackEscalates`/`Busy` → `Conflict`);`app_requests.rs`:经真实 UDS 走一遍 install → install → `Rollback` → `List`。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p bytehost-apps --all-features`、`cargo test -p dozerd app_service`、`cargo test -p dozerd --test app_requests`、`cargo test -p dozer-client`)。
- [x] **Step 5: Commit** — `feat(bytehost-apps,dozerd,dozer-client): manual rollback request (A6g task 3)`。

---

### Task 4: GUI——版本、回滚入口、升级审批提示、升级后重载

**Files:** Modify `settings_apps.rs`、`app_host.rs`、`app/app.rs`(副作用执行)、`app/view.rs`;Test: `settings_apps.rs`、`app_host.rs` 的 `mod tests`。

**Interfaces:**
- Consumes:`AppSummary.{version, previous_version, rollback_note}`、`Client::app_rollback`。
- Produces(`settings_apps`):`Message::{RollbackClicked(String), RollbackConfirmed(String), RollbackCancelled, RolledBack(String, Result<(), Failure>)}`、`Effect::Rollback(String)`;行内显示"版本 X",`previous_version` 有值时多一个「回滚到 Y」按钮(点击进入**行内二次确认**,不用 Toast;确认文案固定为:"回滚到 Y:应用数据不会一起回滚,旧版本可能无法读取新版本写过的数据。确定回滚?");`rollback_note` 有值时在该行下方持久显示一行(自动回滚:"已从 X 自动回滚到 Y:原因";手动:"已回滚到 Y")。回滚完成/失败的**一次性**结果走 `outbox` 的 Toast;`Conflict` 的"会提升权限"原因原样呈现。
- Produces(`app_host`):行变化时若同一应用的 `version` 与上一次 `List` 不同,且面板已加载过该应用 → 等价于"重新启动"——清掉旧的 launch URL 并重新 `FetchLaunchUrl`(webview 重载),**即使两次 `List` 之间看到的 `observed` 都是 `Running`**(Review Focus 6)。
- 升级审批卡(`plan_view`,已显示 `upgrading_from`)补两行:应用正在运行时"升级会短暂中断并自动重启";始终显示"新版本起不来会自动回到上一版"。运行状态来自设置页已有的 `List` 结果。

- [x] **Step 1: 写失败测试**
  - `settings_apps`:无 `previous_version` 的行没有回滚按钮(Review Focus 7);点击回滚 → 进入确认态且**不**发 `Effect::Rollback`;确认 → 发一次(在途时重复确认不重发);取消 → 回到普通态;回滚成功 → 刷新列表 + Toast;`Conflict` 失败 → Toast 带后端原文;`rollback_note` 有值时行内文案表驱动(自动/手动/权限提升跳过三种)。
  - `app_host`:`List` 两次中同一应用 `Running` 但 `version` 变了 → 产出 `FetchLaunchUrl(slot)`;版本不变的两次 `List` 不产出;应用第一次出现(没有上一次)不算变化。
  - 升级审批卡:`plan_view` 的行表驱动加两行(运行中/非运行中)。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p dozer-app app_host settings_apps`;`cargo clippy -p dozer-app --all-targets` 无新警告)。变异:去掉版本比较,重载用例失败。
- [ ] **Step 5: 手动验收**(需要 GUI,如实记入报告 §2,未做的不勾):装 `py-notes` 1.0.0 并启动、点几次让计数增加 → 把样例 `manifest.toml` 版本改成 1.1.0 并改页面文字后再装 → 审批卡显示"正在运行,会短暂中断"→ 批准后面板自动显示新页面、计数还在;装一个起不来的 1.2.0 → 数秒内设置页该行出现"已从 1.2.0 自动回滚到 1.1.0",面板回到 1.1.0;点「回滚到 1.0.0」→ 确认文案如上 → 面板回到 1.0.0。
- [x] **Step 6: Commit** — `feat(dozer-app): upgrade and rollback in the settings page; panel reloads when the version changes (A6g task 4)`。

---

### Task 5: 真实端到端验收与文档

**Files:** Modify `crates/dozerd/tests/process_apps_live.rs`;Create `docs/superpowers/specs/2026-10-08-bytehost-a6g-acceptance-report.md`;Modify `CLAUDE.md`、规格 §7 表。

- [x] **Step 1: live 用例**(`#[ignore]`,真 python3 + node 各一;沿用 A6e 的 `stage`/`install`/`launch`/`wait_*` 辅助。**所有"等新 pid / 等恢复"的断言必须要求纯数字 pid**——A6e 曾因接受网关错误页正文而假通过,任何"快得不可能"的耗时都要追到根因):
  1. 装样例 1.0.0 → 启动 → `POST /hit` 计数 1;
  2. 升级到 1.1.0(样例 `/` 页面带版本字符串,`stage` 里替换)→ 等到页面显示 1.1.0、pid 与升级前不同、计数仍为 1(`BYTEHOST_DATA_DIR` 持久);记录 `previous_version == 1.0.0`;
  3. 手动 `Rollback` → 1.0.0,页面显示 1.0.0,计数仍为 1;`previous_version` 被消耗;再 `Rollback` → `NotFound`。
     (追记:原第 3/4 条把"手动回滚"排在自动回滚之后,期望自动回滚到 1.1.0 后还能手动回 1.0.0——与 Task 1「只保留当前+上一版(装 1.2.0 时 1.0.0 已清)」和 Task 2「回滚消耗上一版」两处裁决自相矛盾,不可能成立。按实现改成:手动回滚先做,自动回滚用例回到手动回滚后的当前版本。)
  4. 升级到起不来的 1.2.0 → 自动回到当前(1.0.0),页面显示 1.0.0、`rollback_note.automatic == true` 且 `from == 1.2.0`、`to == 1.0.0`;再 `Rollback` 同样消耗上一版 → `NotFound`;
  5. 磁盘:`apps/<id>/package/` 下只剩当前版本的子目录(`Paths::package_dir`)——1.1.0 在装 1.2.0 时被"当前+上一版"清理,1.2.0 在自动回滚时清掉。
- [x] **Step 2: 连跑 3 遍**,耗时如实记入报告 §1(实测:三遍全绿,单遍 8.4~8.5s)。
- [x] **Step 3: 报告**:§0 环境;§1 自动化(命令 + 结果 + 实测耗时);§2 GUI 手工项(Task 4 Step 5,未执行保持未勾选);§3 缺陷与已知局限(据实,不预填)。
- [x] **Step 4: CLAUDE.md** `bytehost-apps` 一行补:A6g 落地——升级保留"当前 + 上一版"(更早的清理)、运行中升级自动停→换→再启、升级后的首次启动有"试用期"(`probation` 落盘),失败由**独立线程**自动回滚(监管线程回调里绝不 `stop_locked`/`start_locked`)、回滚不得提升权限(手动被拒、自动跳过)、`AppRequest::Rollback`、应用数据不随版本回滚。规格 §7 表加 A6g 一行并链接本计划与报告。
- [x] **Step 5: 全量**:`cargo test -p bytehost-apps -p dozerd -p dozer-client -p dozer-app`(已知无关失败:`files::tests::delete_confirm_spec_reflects_pending_target`、`memory::tests::list_orders_by_updated_ms_desc`、`app_service::tests::the_first_run_is_unavailable_when_the_port_cannot_be_saved`——如仍存在逐个注明,不要顺手改)、`cargo clippy --all-targets`(A6g 触碰的文件无新警告)、`cargo fmt --check`、两个门禁脚本。(实跑:`bytehost-apps` 335 passed、`dozer-client` 全 ok、`dozerd` lib 487 passed / 1 failed(仅偶发 `memory::tests::list_orders_by_updated_ms_desc`,单跑通过)、`dozer-app` 1975 passed / 1 failed(仅既有 `delete_confirm_spec_reflects_pending_target`);两个门禁脚本 + `fmt --check` 通过;`clippy -p dozerd -p bytehost-apps --all-targets` A6g 文件无新警告。)
- [x] **Step 6: Commit** — 测试与报告各一个:`test(dozerd): real python and node apps upgrade, auto-rollback and manual rollback (A6g task 5)`(`89db2fae`)、`docs(bytehost): A6g landed`(`a355700c`)。

---

## 已知局限(写在这里,不是缺陷)

- **应用数据不回滚**,也没有升级前的数据快照;旧版本读不了新数据时只能由应用自己处理。
- **只有一个回滚目标**(上一版);要回到更早的版本只能重新安装并审批。
- **回滚会提升权限时不自动处理**:手动被拒、自动跳过,都需要用户重新安装目标版本。
- **试用期以"首次 `ready`"为界**:新版本健康了但之后才崩,不会触发自动回滚(那是普通的崩溃重启策略;用户可手动回滚)。
- **静态应用没有试用期**:启动是同步的,成功就算站稳,失败当场回滚。
- 升级的来源仍只有本机目录(非本机来源 = A6h);容器运行时仍未实现。

## Self-Review(写计划时已核对)

- **范围覆盖**:用户三项裁决——运行中自动停升再启(Task 1)、失败自动回滚 + 手动回滚(Task 2、3、4)、保留当前 + 上一版(Task 1)——各有任务;权限与数据两个未被问到的点已作为"自己的决定"明列待评审。
- **占位符**:无;验收报告 §1/§3 要求执行时据实填写。
- **类型一致**:`previous_version`/`probation`/`last_rollback`/`RollbackNote`/`prune_versions`(Task 1)、`rollback_locked`/`spawn_auto_rollback`/`RollbackEscalates`/`NoPreviousVersion`(Task 2)、`AppRequest::Rollback`/`AppSummary.{previous_version,rollback_note}`/`Client::app_rollback`/`Core::rollback`(Task 3)、`Message::Rollback*`/`Effect::Rollback`(Task 4)在定义它们的任务里给出签名,后续任务按同名使用。
- **已核实的前提**:`registry.rs:215` 对 `format_version` 严格相等 → 新字段只能用 `#[serde(default)]` 而不升版本;`install_staged` 现有 `Busy` 门槛与"同版本拒绝";`stop_locked` 会把 `desired` 改成 `Stopped`(回滚要先记下原值);`settings_apps.rs:584` 已显示 `upgrading_from`;`diff_permissions(...).escalation` 已存在,回滚的权限判定直接复用。
