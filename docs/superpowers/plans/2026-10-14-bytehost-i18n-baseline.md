# bytehost i18n — T0 基线、接缝与决策记录

> T0 归属 bytehost；Dozer 仅补消费点记录。本文件是 T0 的工作产出：固定基线、合并规格（见权威规格 `../specs/2026-10-14-bytehost-app-i18n.md`）、`AppSummary` 接缝清单、WebView 注入 spike 结论、以及发布期命名/限额/版本决策。
> 状态:**T0、T1、T2、T3、T4、T5 完成**(bytehost 侧)。T1=manifest `[i18n]` 字段与纯 resolver,T2=资源装载与严格校验并接入 `read_package`+digest,T3=`AppSummary.display_i18n` 投影(声明标题投影、安装期 64 入口/128 KiB 预算、运行期损坏降级),T4=WebView 语言桥(`bytehost-webview` 纯策略:只读快照/脚本 + pending/skeleton 不变量)+ Surface 推送(泛型 `PushState` 原生满足首帧/重推/hidden 同步/仅语言变更也推 + `PushBackoff` 有界退避),T5=三个已交付样例应用(`scripts/samples/i18n/bilingual-live` live 完整双语、`scripts/samples/i18n/partial-initial` initial 部分翻译、`scripts/samples/i18n/legacy-sample` 无 i18n)+ 现有 `scripts/samples/node-notes` 就地改写复用桥 + 安装期拒绝安全测试夹具。后续 T6–T8 在 Dozer 消费与验收。

## T0-1 上游基线记录

| 项 | 值 | 来源 |
| --- | --- | --- |
| bytehost 发布 tag | `v0.2.0`（`github/main`） | `git tag` + HEAD 链 |
| 本地 HEAD | `bd84bef`（v0.2.0 后一条计划提交：`docs(plan): scaffold app i18n contract plan`） | `git log` |
| `HOST_VERSION`（capability 版本） | `id::Version::new(0, 1, 0)`，即 `0.1.0` | `crates/bytehost-apps/src/lib.rs:50` |
| `min_host_version` 比较逻辑 | `if self.min_host_version > *host_version { 拒绝 }` | `crates/bytehost-apps/src/manifest.rs:165-170` |
| `host_version` 流向 | `bytehost-client`/service 经 `AppManager::with_config(root, HOST_VERSION, gateway, config)`（service.rs:14,215）存入 `self.host_version`，在 `read_package`(453,896,964)、`plan`(1311)、`list`(1594,1791,1945) 与 `Manifest::from_toml` 处使用 | grep `host_version` in bytehost-apps |
| schema_version | `1`（`SCHEMA_VERSION`, manifest.rs:11） | manifest.rs:11 |
| 协议帧上限（传输层） | **无显式单帧字节上限**。AppSummary 经 `bytehost-client` 两路共享同一类型：InProcess 为内存 `Vec`，UDS 为 serde_json 行帧（`testutil.rs:71,118` `write_all` raw，无 cap）。 | 见下“128 KiB 预算” |
| 目前无 i18n 代码 | `i18n`、`title_key`、`title_fallback`、`strings_dir`、`supported_locales`、`default_locale`、`ui_locale`、`locale_revision` 仅在文档出现，源码无。 | grep 全仓 |

### 128 KiB 投影预算是否需要收紧

**结论：无需收紧传输帧；128 KiB 是投影层自加预算，需在 `manager.list()` 投影处强制并记日志。**

- 传输路径没有比 128 KiB 更小的硬上限，因此不存在“传输帧更小需统一收紧”的情形。
- 但 spec §5 要求“最多 64 入口、投影序列化后上限 128 KiB；新声明超限在安装时报错”。这一上限是**投影/序列化边界**的约束，应在构造 `AppSummary.display_i18n` 时计算序列化大小并拒绝超限包（安装期）或降级（运行期）。
- 实施时：在 `manager.rs:1959` 的 `list()` 投影点累加每个入口的标题描述 + 已校验语言值大小；超限直接让该包安装失败（明确错误），旧无 i18n 包不受新增入口数限制。

## T0-3 `AppSummary` 接缝清单（最小改动清单）

### 当前结构（`crates/bytehost-apps/src/proto.rs:56-76`）

```rust
pub struct AppSummary {
    pub id: AppId,
    pub name: String,
    pub version: Version,
    pub desired: DesiredState,
    pub observed: ObservedState,
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub issue: Option<AppIssue>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub previous_version: Option<Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub rollback_note: Option<RollbackNote>,
}
```

### 构造点

| 位置 | 性质 | 是否需改 |
| --- | --- | --- |
| `crates/bytehost-apps/src/manager.rs:1959` | **真实 `list()`** 构造，读 `package_dir(id, current_version)` 的 manifest 取 `name`（1941-1947） | **是**——注入 `display_i18n` 与 128 KiB 预算 |
| `proto.rs:489` | 测试 fixture | 同步补字段 |
| `tests/process_apps_live.rs:142,157` | 测试 helper | 同步补字段 |
| `bytehost-panel/src/host.rs:826,840,862,1106` | 面板测试 fixture | 同步补字段 |
| `bytehost-panel/src/install.rs:1278,1530` | 面板测试 fixture | 同步补字段 |

### InProcess / UDS 接缝

- `bytehost-client/src/lib.rs:99-106` `app_list()` 直接 `request(AppRequest::List)` 取 `AppReply::Apps { apps }` 返回 `Vec<AppSummary>`，**无 DTO 变换**。
- `bytehost-client/src/in_process.rs` 包 `Arc<AppService>`，`request`/`subscribe` 直转；UDS 实现同型。两路**共享同一 `AppSummary` 类型、零转换** → 给 `AppSummary` 加 `#[serde(default, skip_serializing_if)]` 字段对两路同时可见且旧 JSON 仍可解析。
- `conformance.rs` 共享断言 `run(api, source, app_id)` 对两实现校验一致；新增字段须扩展断言（当前只查 `observed`/`id`/长度，不查 `name`/title）。

### 安装 / 回滚路径

- `registry.rs::AppPaths::package_dir(id, version)`（46-48）是**不可变已安装内容位置**；`manager.list()` 的 manifest 即从此读（1941）。i18n 资源随包版本进入 `package_dir`，**回滚自动恢复该版本的 locale 文件**，无需额外处理。
- `AppRecord.versions: Vec<VersionRecord>`（registry.rs:75-80, 97-123），`previous_version` 默认，`prune_versions` 仅保留 current+previous。
- 资源已自动进入摘要：`digest.rs::digest_tree(root)`（44-57）对**整个包根**（含 manifest/icons/runtime.source）哈希，`collect`（77-104）**拒绝符号链接**；i18n JSON 在包内即被覆盖。安装期来源校验在 `source.rs::stage_source`（211-258）。

### 最小改动清单（供 T1–T5 实施）

1. `manifest.rs`：给 `Manifest` 加 `i18n: Option<I18n>`（`#[serde(default)]`），给 `Entrypoint` 加 `title_key`/`title_fallback`；扩展 `validate`（157-223）。因 `deny_unknown_fields`，新 manifest 必须 bump `min_host_version`。
2. 新增 `bytehost-apps/src/i18n/{mod,resources,negotiate,tests}.rs`：资源路径归属/大小/形状校验（复用 `digest.rs` 逃逸规则）、标题协商（当前 locale→default→fallback，精确匹配，无族回退）。
3. `proto.rs`：给 `AppSummary` 加 `display_i18n: Option<AppDisplayI18n>`（`#[serde(default, skip_serializing_if)]`）。
4. `manager.rs:1959`：构造时跑 negotiate 注入 `display_i18n` 并强制 128 KiB 投影上限。
5. `bytehost-webview`：新增 i18n 桥快照序列化与“pending/skeleton 直到首帧”逻辑（见 T0-4）。
6. `bytehost-core/src/surface.rs` `PushState` + `bytehost-desktop/src/surface/lifecycle.rs` `Evaluate` + `routing.rs` generation 标记：复用既有“rebuild 强制重推”不变量下发 locale。
7. `bytehost-desktop/src/app_webview.rs`：导航完成后经 `evaluate_app`（163-172）下发 locale；扩展扫描钉死测试（246-311）。
8. 依赖门禁（`scripts/check-deps.sh`）：新增装载依赖挂 `server` feature；`bytehost-panel` 直接依赖限 `bytehost-apps`/`bytehost-client`。
9. 版本：i18n 发布时 bump `lib.rs:50` `HOST_VERSION`，并在发布记录写明 capability version ↔ tag 映射。

## T0-4 WebView 注入接缝 spike 结论

**Spike harness：** `crates/bytehost-desktop/tests/i18n_bridge_inject.rs`（`#[ignore]` 默认，需 GUI 会话运行 `cargo test -p bytehost-desktop --test i18n_bridge_inject -- --ignored --nocapture`）。它用 `app_webview` 的共享 builder 建一个 wry 子 webview，注入 init 脚本、同源二次导航、并通过 `evaluate_app` 模拟宿主首帧下发，断言：(a) init 脚本在页面脚本前执行；(b) 同源二次导航得到新 document epoch；(c) 语言快照**不**来自创建时冻结的 init 脚本，而来自导航后 `evaluate_app` 的首帧；(d) 首帧到达前桥处于 skeleton/未就绪，不返回已知过期值。

**架构级结论（已核实代码，作为 spike 的事实依据）：**

- 初始化脚本经 `app_webview.rs:104-117` 的 `with_initialization_script(app_init_script(&nonce))` 注入，**在 WebView 创建时一次性冻结**。wry 的 `add_initialization_script` 在每次文档加载时以**相同冻结内容**重跑（无 per-document 重注入路径）。因此 **init 脚本无法携带运行时变化的 locale 值**。
- 运行时下发通道是 `evaluate_app(slot, script)`（`app_webview.rs:163-172`），在 `PageLoadEvent::Finished`→`SurfaceKind::Application::Ready`（125-129）之后执行。这是唯一能给“新文档”带最新 locale 的路径。
- `SurfaceRouter`（`routing.rs:86-101`）按 `Generation` 拒绝旧实例；`PushState`（`surface.rs`）的 `reset_for_new_instance` 清空 `sent`、`take_pending` 仅当 `Ready` 且不同于 `sent` 才下发——这套既有生命周期正好支撑“仅语言变化也重推、旧文档回调不污染新文档”。

**采用路径（spec §6.1 允许的退化）：** 桥在宿主首帧快照到达前保持 **pending/skeleton**，绝不返回创建 WebView 时捕获的已知过期值；同源二次导航按新文档重新 bootstrap，首帧经由 `evaluate_app` 下发。该限制由 spike harness（d）+ 扫描钉死测试（app_webview.rs:246-311 扩展）提供验收证据。若真实 GUI 环境能证明 init 脚本可在新文档早期拿到最新值（与本架构结论相反），再考虑用 init 脚本携带；当前以 pending/skeleton 为准。

## T0-5 发布期命名 / 限额 / 版本决策

### 能力首发版本方案

- 当前 `HOST_VERSION = 0.1.0`（lib.rs:50）。i18n 能力发布时 bump 到下一语义版本（建议 `0.2.0`），并打对应 bytehost **release tag**；应用 manifest 的 `min_host_version` 必须 `>=` 该版本才能使用 `[i18n]`。
- capability version ↔ 仓库 tag 的映射写入发布记录（release notes + 兼容矩阵 + 样例版本 + 完整验证记录），**不**根据 crate 的 package version 猜测。
- 先发上游 tag，再让 Dozer 固定该 tag；联调 `[patch]` 不提交（见实施计划 T7）。

### 规范 locale 子集

- 接受：**ASCII language（2–8 字母）** + 可选 **script（4 字母）** + 可选 **region（2 字母 或 3 数字）**。
- 规范化：language 小写、Script 首字母大写、REGION 大写；规范化后查重。
- **拒绝**：下划线、编码后缀、路径字符、扩展/private-use。
- **不**做隐含语言族回退、**不**做简繁互转。当前 UI locale 被 `supported_locales` 声明则采用，否则 `default_locale`。未来族回退需单独版本化。
- 资源文件名使用规范标签；大小写不匹配安装期报错。

### 限额（spec §4.1，实施时强制）

| 限额 | 值 | 实施点 |
| --- | --- | --- |
| 声明语言数 | ≤ 16 | `i18n/resources.rs` 枚举声明语言时计数 |
| 单文件大小 | ≤ 512 KiB | 限长读取（limit+1），解析前生效 |
| 声明资源总计 | ≤ 8 MiB | staging/安装两阶段重验 |
| 每文件 key 数 | ≤ 4096 | 解析时计数 |
| key 长度/字符集 | 1–128 个 `[A-Za-z0-9_.-]` | 校验 |
| 普通值长度 | ≤ 4096 Unicode | 校验 |
| 标题值长度 | ≤ 64 Unicode + 现有控制字符/bidi 校验 | 复用 `check_display` |
| 投影序列化 | ≤ 128 KiB（≤64 入口） | `manager.list()` 投影处强制 |

### bridge API 最终命名

- 挂载点：`window.bytehost.i18n`（仅当 manifest 声明且宿主桥版本满足时安装；`none` 不安装）。
- 方法：`getSnapshot() -> Snapshot`（独立只读副本）、`subscribe(cb) -> unsubscribe()`（注册立即回调当前快照，返回幂等注销函数）。
- `Snapshot = { version: 1, ui_locale: string, app_locale: string, locale_revision: string(十进制) }`。`locale_revision` 为外壳本地显示代次（**不**与 daemon revision 比大小，不跨窗口比较）；页面用受约束 BigInt 或十进制比较。
- 行为：`initial` 仅首次快照 + 一次立即回调，不接收同文档变化；`live` 完整通知；页面篡改自身语言显示不反向修改产品语言，不暴露任何宿主命令能力。

## T5 样例应用与安装期安全夹具（spec §8）

> T5 在 bytehost 仓库实现并交付真实样例；验收证据由 `crates/bytehost-apps/src/manager.rs` 的 `manager::tests` 提供（随 `--features server` 运行）。

### 交付样例（`scripts/samples/i18n/`）

| 样例 | `[i18n]` | `webview_locale` | locales | 页面是否消费桥 |
| --- | --- | --- | --- | --- |
| `bilingual-live` | 有（en/zh-CN 完整） | `live` | en + zh-CN 全量四 key | **是**——`web/index.html` 经 `window.bytehost.i18n.getSnapshot()`/`subscribe` 实时渲染，`document.documentElement.lang` 设为 `app_locale` |
| `partial-initial` | 有（en 完整，zh-CN 仅译 `app.title`） | `initial` | en 全量；zh-CN 部分 | **是**——只取首帧，`main.title`/`greeting`/`hint` 在 zh-CN 缺失 → 回落 default(en) |
| `legacy-sample` | 无 | （无桥） | 无 | 否——仅 `name` + `Entrypoint.title`，旧路径 |

- `bilingual-live` 与 `partial-initial` 的 `web/index.html` 自备字符串表、用桥给的 `app_locale`（**不是** `ui_locale`）选语言，符合 spec §3.1「页面可用自己内部 i18n 系统」；首帧前 `getSnapshot()` 返回 `null`，不渲染过期值（pending/skeleton 不变量）。
- **应用 UI 接入成功标记**：`bilingual-live`、`partial-initial` 的 `web/index.html` 真正消费桥并完成双语/部分双语渲染，可标「应用 UI 接入成功」；`legacy-sample` 不消费桥，仅作基线对照（外壳标题沿用 `name`）。

### 安装期拒绝安全夹具（`expect_install_rejected` 经 `install_plan` 断言拒绝 + 无 staging 残留）

| 夹具 | 触发层 | 拒绝原因 |
| --- | --- | --- |
| `security_fixture_rejects_path_traversal_strings_dir` | `load_i18n_resources` → `check_strings_dir` | `strings_dir = "../../etc"` 非包内相对路径 |
| `security_fixture_rejects_illegal_locale_label` | `manifest.validate` → `normalize_locale` | `supported_locales = ["en_US"]` 含下划线，非法标签 |
| `security_fixture_rejects_missing_default_locale_file` | `load_i18n_resources` | 默认语言 `en` 文件缺失 |
| `security_fixture_rejects_duplicate_key_in_locale_file` | `load_i18n_resources` → `FlatMapVisitor` | JSON 重复 key |
| `security_fixture_rejects_title_key_without_fallback` | `manifest.validate` → `check_display_title` | `title_key` 缺 `title_fallback` |
| `security_fixture_rejects_unknown_webview_locale_value` | `Manifest::from_toml` → `toml::from_str` | `webview_locale = "always"` 非 `none/initial/live` |
| `security_fixture_rejects_symlinked_resource_escape`（仅 unix） | `load_i18n_resources` → `ensure_inside` | `en.json` 为指向包外的符号链接 |

- 所有夹具均经管理器安装入口 `install_plan` 拒绝（安装期，而非仅资源解析单测），且 `apps/` 下不留 `.staging-*` 残留（`assert_no_leftovers`），对应 spec §8 B02/B03。
- 资源装载层的更细粒度拒绝（嵌套/非字符串值/超限/非规范文件名/符号链接父目录等）由 `i18n/resources.rs` 单测覆盖（T2），本处只补「经安装入口端到端拒绝」证据。

### B 系列验收映射（bytehost 侧可证部分）

| ID | 证据位置 |
| --- | --- |
| B01 | `legacy_sample_installs_without_any_i18n_bridge` + manifest 单测 `old_manifest_without_i18n_still_validates` |
| B02 | 三个样例安装投影一致 + 6 个非法夹具安装期拒绝 |
| B03 | `security_fixture_rejects_path_traversal_strings_dir` / `..._symlinked_resource_escape` + resources 单测 `symlink_escape_outside_package_is_rejected` |
| B05 | `bilingual_live_*`（精确/en/fallback）、`partial_initial_*`（部分翻译回落） |
| B09 | `legacy-sample`（`none` 无桥）+ `partial-initial`（`initial` 仅首帧）对照 |
| B10 | bridge 单测 `init_script_installs_readonly_bridge_skeleton`（无 `__host.dispatch` / `postMessage`） |
| B11 | `PushBackoff` 单测（`push_backoff_is_bounded_and_resets_on_success`，T4） |

> B04/B06/B07/B08/B12 中涉及运行时 Surface 推送/热切换/Dozer UDS 协同的部分在 T6–T8（Dozer 侧）验收。

### T5 收尾说明（样例路径 / node-notes 桥复用 / 测试与门禁）

- **样例路径落实**：三个样例原在仓库根 `samples/`，按实施计划 §T5 迁至 `scripts/samples/i18n/`（保留 `bilingual-live` / `partial-initial` / `legacy-sample` 三个子目录）；README 已更新指向新路径。
- **bilingual-live 状态保存**：`web/index.html` 含 `<input id="note">` 与可滚动 `<div id="log">`（60 行）；`render(snap)` 只改 `greeting`/`hint`/`meta` 文本与 `document.documentElement.lang`，**绝不读写 input 值或 scrollTop**，证明语言热切换保留用户输入/滚动位置（spec §8「应用 UI 接入成功」标志）。
- **partial-initial 干净部分翻译**：`zh-CN` 仅译 `app.title`，`pick(loc,key)` 缺失 key 回落 `STRINGS.en`；已删除原先 `STRINGS["zh-CN"].hint = undefined` 的 hack，改为真实省略。
- **node-notes 就地复用桥**（用户决策：就地改 node-notes，不新建变体、不并改 py-notes）：`scripts/samples/node-notes` 增加 `[i18n]`（`default_locale="en"`、`supported_locales=["en","zh-CN"]`、`strings_dir="locales"`、`webview_locale="live"`）、`[presentation]`+`title_key`/`title_fallback`、`[entrypoints.main]`+`title_key`/`title_fallback`，新增 `locales/en.json` 与 `locales/zh-CN.json`（**两个文件都必须存在**——`[i18n]` 声明触发安装期 `load_i18n_resources`，缺默认 en 文件会导致 `node_sample_end_to_end` 的 ignored 真机测试安装失败）。`server.js` 的 `indexHtml()` 在保留既有 EventSource/WebSocket/count 逻辑（含 `runtime:`/`hits: 0` 子串供 `process_apps_live.rs` 断言）前提下，新增 `<h1 id="greeting">`/`<p id="meta">` 与一个注入桥的 `<script>`：用 `window.bytehost.i18n.getSnapshot()`+`subscribe()` 设 `document.documentElement.lang` 与问候/元信息，并在无桥时回落静态 `STRINGS`。这同时满足 spec §8「验证一个现有 Node/Python HTTP 样例复用桥」且**不把 server 进程 env 当作页面语言来源**（页面语言由桥快照的 `app_locale` 决定）。
- **static_web 复用桥**：`bilingual-live`/`partial-initial` 本身是 static_web 样例，`web/index.html` 直接消费桥——已覆盖 spec §8「验证一个 static_web 样例复用桥」。
- **验证测试**：新增 `crates/bytehost-apps/tests/i18n_samples.rs`（`#![cfg(feature = "server")]`），4 个用例全部通过（`cargo test -p bytehost-apps --features server --test i18n_samples` → 4 passed / 0 failed）：`bilingual_live_is_live_and_resolves_all_locales`、`partial_initial_is_initial_and_falls_back_for_missing_translation`、`legacy_sample_has_no_i18n_section`、`node_notes_reuses_the_bridge`。
- **门禁**：T5-8 跑 `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo test --workspace --all-features` / `bash scripts/check-deps.sh`，结果如实记录（不掩盖基线既有失败）。

### B 系列验收映射补充（T5 新增证据）

| ID | 证据位置 |
| --- | --- |
| B05（HTTP 复用桥） | `crates/bytehost-apps/tests/i18n_samples.rs::node_notes_reuses_the_bridge`（zh-CN→"Node 笔记"/"打开演示"；fr→"Node Notes" default 回落） |
| 样例状态保存 | `scripts/samples/i18n/bilingual-live/web/index.html`（input/log 不随语言切换丢失） |
| 样例路径 | README + `scripts/samples/i18n/` 目录 |

## T0 验收对照

- 消费入口与版本映射明确：HOST_VERSION / min_host_version 比较逻辑已记录；capability↔tag 方案已定。
- B06/B07 的实现接缝有实测依据：init 脚本冻结、evaluate_app 下发、Surface 生命周期复用——架构已核实，spike harness 提供可运行验证。
- 没有新增 External 或通用 Plugin Manifest 工作：本期仅 `EntrypointKind::Web`，External/Declarative/None 留待后续（已在 scaffold 纠正项中声明）。
