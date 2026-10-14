# bytehost:应用 i18n 契约(manifest `[i18n]` + 标题 key/fallback + Surface 语言协商)Implementation Plan

> **说明(2026-10-14,scaffold):** 本计划是 **dozer i18n 实施计划 Task 14** 的上游部分,归属**本仓库**。
> Dozer 侧只保留消费适配、依赖 tag 与样例验收记录(见 dozer 仓库
> `docs/superpowers/plans/2026-10-12-dozer-i18n-implementation.md` Task 14、
> `docs/superpowers/specs/2026-10-12-dozer-i18n-design.md` §9)。
> 本文件**只搭计划,尚未改任何 bytehost 代码**。执行时按 checkbox 逐任务推进。
> 依据:`docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`(manifest §4.1、安装计划 §5、
> 受管运行时、origin gateway);dozer i18n 设计规格 §9 的 `[i18n]` 契约与 Surface/External 口径。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 bytehost 应用能在 manifest 里**可选声明** i18n(`default_locale` / `supported_locales` / `strings_dir`),
并让 Panel/Page/Tab/Command 的标题以 `title_key` + 必填 `title_fallback` 表达;宿主按 Surface 类型对
Declarative / WebView / External / None 做**语言协商**:初始 locale 下发 + 协商后变化通知。资源装载限制在
已安装包内、不改宿主可信确认/权限文案。未声明 i18n 的应用与旧 manifest 行为**完全不变**(直接用 fallback / `title`)。

**Architecture:** manifest v1 追加可选 `[i18n]` 段与 entrypoint 的 `title_key`/`title_fallback`(与现有 `title` 并存,
`deny_unknown_fields` 下新字段必须先于校验规则落地,否则旧解析器会拒;wire 只追加)。新增 `i18n` 子模块负责:
①资源路径归属校验(包内、拒绝绝对/`..`/符号链接逃逸、限大小、限格式);②按协商顺序
(`当前 locale` → `supported` 内的 fallback 语言 → `default_locale` → `title_fallback`)选标题;
③Surface 语言下发(`ui_locale` + `locale_revision`)。Surface/宿主协议在现有 envelope 上**只追加**字段。

**Tech Stack:** Rust(`bytehost-apps`,serde,`deny_unknown_fields`,默认 feature 仍只依赖 serde 家族);
桌面外壳 `bytehost-desktop`/`bytehost-panel` 消费端按 tag;静态资源经 origin gateway(不改 CSP)。

## 上游未就绪的边界(为什么要单独成阶段)

dozer i18n 规格 §9 明确:**涉及安装包 schema、资源路径校验或跨产品公共桌面契约的改动,必须在 bytehost 独立仓库
追加兼容能力并发 tag,Dozer 只写消费适配;未经该能力接入的第三方应用仅保证外壳标题显示 fallback,
不声称其内部 UI 已本地化。** 因此本阶段**完成前**:

- Dozer 不得在自身仓库内另造 manifest 运行时或资源装载器;
- Dozer 不得把第三方应用宣传为"已本地化",最多显示 `title_fallback`;
- 本仓库完成后,dozer 侧工作 = 依赖 tag 升级 + 消费适配 + 样例验收记录。

## 用户待裁决的行为(执行前请确认)

1. **`title` 与 `title_key` 的关系:** 现有 `Entrypoint.title: Option<String>` 保留为**兜底显示名**;
   新增 `title_key: Option<String>` + `title_fallback: Option<String>`(声明 key 时 fallback 必填)。
   无 `[i18n]` 或未声明 key 时行为不变。**待确认:是否允许同时声明 `title` 与 `title_key`(以 `title_key` 优先)?**
2. **`[i18n]` 段字段:** `default_locale`(必填,声明该段时)、`supported_locales`(必填,非空、
   含 `default_locale`)、`strings_dir`(可选,相对包根,默认 `locales`)。**待确认默认目录名与是否强制在包内。**
3. **资源格式:** 首版只支持**每语言一个 JSON 文件**(`<strings_dir>/<locale>.json`,扁平 string→string),
   无嵌套、无复数/ICU。**待确认是否接受此最简格式**(与 dozer §6 口径一致,不引入 Fluent)。
4. **协商顺序:** `当前 UI locale` → `supported` 中的语言族回退(如 `zh-Hans-CN`→`zh-Hans`→`zh`)
   → `default_locale` → `title_fallback`。**待确认是否要语言族回退**,还是仅精确匹配 + `default_locale`。
5. **WebView 应用语言来源:** 经 URL query(`ui_locale`)还是经既有宿主→页面消息通道下发?
   **待确认**,默认倾向 **URL query(只追加,不改 payload 语义)**,复用现有 gateway,不改 CSP。
6. **External 应用:** 只传 `DOZER_UI_LOCALE`(不改 `LANG`/`LC_ALL`);未协商热更新能力的,下次启动生效,
   不强制重启。**待确认是否需要在 manifest 声明"支持热更新"。**

## 本计划自己做的决定(请评审时确认或推翻)

- **不破坏旧应用:** `[i18n]` 与 `title_key`/`title_fallback` 全部 `Option` + `#[serde(default)]`;
  旧 manifest 解析、安装、启动**逐字节行为不变**;新增字段非法时**明确报错**(不静默忽略)。
- **资源装载硬约束:** 只读**已安装包目录内**的文件;拒绝绝对路径、含 `..`/空段/`.` 的路径、反斜杠、
  非 UTF-8 名、NUL、符号链接逃逸;单语言文件 ≤ 512 KiB,`strings_dir` 整包 ≤ 8 MiB;
  非 JSON / JSON 顶层非对象 / 值非字符串 / 空翻译 → 该校验失败并说明是哪一条。
- **owner 隔离:** manifest 只能声明**应用自身**资源;语言资源**不得**影响宿主可信确认/权限/错误文案
  或其它应用/面板的资源。运行期资源损坏 → 用 `title_fallback`,**不拖垮 Host**。
- **不进入任何已安装包外:** 资源路径解析后必须 `canonicalize` 且以包根为前缀,否则拒绝。
- **wire 只追加:** 宿主↔外壳的请求/回复只加可选字段,旧形状 JSON 必须仍能解析;未知字段按现状处理。
- **Surface 语言下发幂等:** 每次 locale 变化推进该 Surface 的 `locale_revision`(外壳本地显示代次,
  **不与 daemon revision 比大小**);ready、重建、隐藏后恢复都发送最新完整快照,不以"语言事件已发过"漏推。

## Global Constraints(沿用本仓库既有门禁)

- `bytehost-apps` 默认 feature 依赖只能是 serde 家族;新增依赖只挂 `server` feature(`dep:`);
  `scripts/check-bytehost-apps-deps.sh`、`scripts/check-log-scope.sh` 必须通过。
- 日志统一走 `tracing`(bytehost 仓库口径,来源如 `bytehost::service`,**不是** `dozer::module::*`);
  **资源路径、URL、用户内容不写日志**;只记应用 id / locale / 字节数等安全字段。
- 持久状态不进 Toast(外壳侧口径);资源声明非法在**安装校验**时报错,运行期损坏走 fallback。
- **不改 CSP、不放宽导航策略**(`AppOrigin::allows_navigation` 唯一真相);WebView 语言传递不得新开网络路径。
- 变异验证:每条新校验的测试都要临时去掉实现确认会失败,再恢复。
- 提交前 `git diff --cached --stat` 看全貌,只 `git add` 指定路径。

## Review Focus(最可能咬到真实用户的失败模式;每条在对应任务里有测试)

1. **资源路径穿越 / 符号链接逃逸:** manifest 声明 `strings_dir = "../../etc"` 或包内符号链接指向包外,
   必须拒绝,**包外零读取**(Task 1)。
2. **应用覆盖宿主文案:** 应用资源声明 key 撞宿主可信确认/权限 key,或跨 owner 引用,必须被隔离,不得生效(Task 1/3)。
3. **旧应用回归:** 无 `[i18n]` 的旧 manifest 在**新解析器**下安装/启动行为不变;`deny_unknown_fields`
   不得因新字段导致旧版本外壳解析失败(Task 2)。
4. **非法声明拖垮 Host:** 资源文件损坏 / 非 JSON / 超限时,宿主用 fallback 继续,**不 panic、不阻塞安装**,
   并能说清是哪条(安装期 vs 运行期)(Task 1/4)。
5. **Surface 漏推 / 旧包带回旧语言:** ready 前、隐藏后、重建、旧包晚到、**仅语言变化**都必须最终为新语言,
   不得以"语言事件已发过"漏推,不得让旧业务响应带回旧语言(Task 3)。
6. **External 误重启:** 未协商热更新的 External 应用不得被强制重启;不得改写其 `LANG`/`LC_ALL`(Task 3)。
7. **locale 协商空结果:** 当前 locale 不在 `supported`、`supported` 缺 `default_locale` 等情形必须确定性回退到
   `title_fallback`,不出现空标题(Task 2)。

## File Structure

- `crates/bytehost-apps/src/manifest.rs` — 追加 `[i18n]` 段、entrypoint `title_key`/`title_fallback` 及校验(Task 1/2)。
- `crates/bytehost-apps/src/i18n/mod.rs`(新)— 资源路径归属、装载、owner 隔离、标题协商(Task 1/3)。
- `crates/bytehost-apps/src/i18n/resources.rs`(新)— JSON 资源读取与形状校验(Task 1)。
- `crates/bytehost-apps/src/i18n/negotiate.rs`(新)— 语言协商顺序与标题选择(Task 2)。
- `crates/bytehost-apps/src/i18n/tests.rs`(新)— 上述单测(含变异确认)。
- `crates/bytehost-apps/src/proto.rs` / 外壳 envelope — Surface 语言字段追加(Task 3)。
- `crates/bytehost-panel/src/install.rs` — 安装/披露界面消费适配(Task 4)。
- 样例:`fixtures/i18n/` 下三个应用(英中、缺翻译、旧应用)(Task 4)。

## Task 1:资源归属校验与装载(`i18n/resources.rs`)

**Files:** Create `crates/bytehost-apps/src/i18n/mod.rs`、`i18n/resources.rs`、`i18n/tests.rs`;modify `lib.rs`(挂模块)。

- [ ] Step 0: 定义 `StringsBundle { locale, map: BTreeMap<String,String> }` 与错误类型(带"哪条"信息)。
- [ ] Step 1: `resolve_strings_path(pkg_root, strings_dir, locale)`:
  - 拒绝绝对路径、`..`/空段/`.`/反斜杠、非 UTF-8、NUL;
  - `canonicalize` 后必须以 `pkg_root` 为前缀,否则拒绝(**符号链接逃逸**一并挡住);
  - 只允许 `<strings_dir>/<locale>.json` 形状。
- [ ] Step 2: 装载与形状校验:文件 ≤ 512 KiB、`strings_dir` 整包 ≤ 8 MiB;顶层必须是对象;key/值必须是非空字符串。
- [ ] Step 3: 测试(含**变异确认**):穿越路径拒绝、符号链接逃逸拒绝、超限拒绝、非 JSON/非对象/空值报错、正常读取成功。
- [ ] Step 4: 运行 `cargo test -p bytehost-apps i18n::` 与依赖/日志门禁,提交 `feat(i18n): pkg-scoped resource loading with escape/size/shape validation`。

## Task 2:manifest `[i18n]` 与 `title_key`/`title_fallback`(`manifest.rs` + `i18n/negotiate.rs`)

**Files:** Modify `crates/bytehost-apps/src/manifest.rs`;create `crates/bytehost-apps/src/i18n/negotiate.rs`;extend `i18n/tests.rs`。

- [ ] Step 0: 追加 `I18n { default_locale: String, supported_locales: Vec<String>, strings_dir: Option<String> }`
      (`#[serde(default)]`),entrypoint 追加 `title_key: Option<String>`、`title_fallback: Option<String>`。
- [ ] Step 1: `validate` 追加规则:声明 `[i18n]` 时 `default_locale` 必填、`supported_locales` 非空且含 default、
      locale 形如 BCP-47 子集(执行时按 dozer §4 口径定死);声明 `title_key` 时 `title_fallback` 必填;
      `strings_dir` 必须通过 Task 1 的路径校验;`deny_unknown_fields` 下旧 manifest 仍可解析。
- [ ] Step 2: `negotiate.rs` 实现标题选择顺序:**当前 UI locale** → 语言族回退(若采纳)→ `default_locale`
      → `title_fallback`;空结果确定性回退,绝不空标题。
- [ ] Step 3: 测试:旧 manifest 解析/校验不变;新字段非法各报一条;`title_key` 无 fallback 拒绝;
      `supported` 不含 default 拒绝;协商各分支;变异确认。
- [ ] Step 4: `cargo test -p bytehost-apps` + 门禁,提交 `feat(i18n): optional manifest [i18n] and title_key with fallback negotiation`。

## Task 3:Surface 语言下发与协商(Task 3 = 下发机制)

**Files:** Modify 外壳 envelope / `crates/bytehost-apps/src/proto.rs`(只追加);
create 语言下发路径(WebView query 或消息通道,按裁决);extend `i18n/tests.rs`。

- [ ] Step 0: 在 Surface envelope 追加 `ui_locale: Option<String>`、`locale_revision: Option<u64>`(只追加,`#[serde(default)]`)。
- [ ] Step 1: **Declarative:** 宿主按 owner 查表返回标题(消费 Task 2 协商结果)。
- [ ] Step 2: **WebView:** 语言经裁决的路径下发(默认 URL query 只追加,复用 gateway,**不改 CSP**);
      ready/重建/隐藏后恢复发最新完整快照;把 locale 纳入 `last_sent` 比较,**仅语言变化也推送**。
- [ ] Step 3: **External:** 仅设 `DOZER_UI_LOCALE`(**不动 `LANG`/`LC_ALL`**);未声明热更新者不强制重启,下次启动生效。
- [ ] Step 4: **None:** 不因全局切换重启后台任务。
- [ ] Step 5: 测试:ready 前/隐藏后/重建/旧包晚到/仅语言变化的推送去重与重发;External 环境变量正确且不重启;
      旧包照现有 revision 丢弃,不因新增字段误判;变异确认。
- [ ] Step 6: `cargo test -p bytehost-apps` + 门禁,提交 `feat(i18n): surface locale delivery and negotiation`。

## Task 4:样例应用与验收记录(Dozer 侧消费 + 本仓库样例)

**Files:** Create `fixtures/i18n/{bilingual,missing,legacy}/`;Dozer 仓库消费适配(独立提交,见 dozer Task 14)。

- [ ] Step 0: 样例一 `bilingual`:en/zh-CN 资源齐全的应用;样例二 `missing`:缺当前语言翻译(走 fallback);
      样例三 `legacy`:无 `[i18n]` 的旧应用。
- [ ] Step 1: 本仓库验收:三个样例安装/校验/启动;旧应用行为不变;非法声明各报一条。
- [ ] Step 2: 发布 tag(授权后);Dozer 侧升级依赖 tag + 消费适配 + 样例验收记录(安装管理界面与应用内部界面**分开**验收)。
- [ ] Step 3: 记录 I4 完整性:支持/未覆盖边界、与内置双语版本的独立性;不阻塞 dozer 内置双语发布。

## 验收

- I4 单独报告**完整性**(支持的 Surface 类型、资源格式、协商能力)与**兼容性**(旧 manifest/旧应用),
  **不阻塞**已完成的内置双语版本。
- 本阶段完成前,Dozer 不得宣称第三方应用 UI 已本地化,不得在自身仓库另造 manifest 运行时。
- 联调 `[patch]` 不提交;正式引用实际发布 tag。
