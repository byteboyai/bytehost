# bytehost：应用 i18n 契约 — scaffold（已取代）

> **状态（2026-10-14）：本 scaffold 已被权威规格 [2026-10-14-bytehost-app-i18n.md](../specs/2026-10-14-bytehost-app-i18n.md) 取代，不再作为实施依据。**
> 本文件保留为历史指针，记录当初未决项的裁决结果与“需要纠正的不准确表述”，便于追溯。
> 实施计划见 [2026-10-14-bytehost-i18n-integration.md](2026-10-14-bytehost-i18n-integration.md)（T0 起逐任务推进）。

## 权威入口

- **规格（唯一真相）：** `docs/superpowers/specs/2026-10-14-bytehost-app-i18n.md`
- **计划：** `docs/superpowers/plans/2026-10-14-bytehost-i18n-integration.md`
- 两份仓库不得长期维护互相分叉的契约：bytehost 持有契约真相，Dozer 侧只持消费范围与链接。

## 当初未决项的裁决结果（已在权威规格中定死）

1. **`title` 与 `title_key` 关系：** 保留 `Entrypoint.title` 作兜底显示名；新增 `title_key`/`title_fallback` 成对必填（声明 key 时 fallback 必填）。`title_key` 优先于 `title`，未声明 key 时沿用旧路径 `title → name`。
2. **`[i18n]` 段字段：** `default_locale`、`supported_locales`（必填、非空、含 default）、`strings_dir`（可选，默认 `locales`）。
3. **资源格式：** 每语言一个扁平 `string→string` JSON，无嵌套/ICU，不带 Fluent。
4. **协商顺序：** 仅规范标签**精确匹配** + `default_locale` + `title_fallback`；**不做**语言族隐式回退、不做简繁转换（未来族回退需单独版本化）。
5. **WebView 语言来源：** 由宿主在导航完成后经 `evaluate_app` 下发快照（非初始化脚本携带，因 init script 在创建时冻结，见规格 §6.1 注入接缝结论）；URL query 仍可用于 `initial` 的首次值，不改 CSP。
6. **External：** 本期不实现 External 入口；`DOZER_UI_LOCALE` 留待 External 真正落地。

## 需要纠正的不准确表述（原 scaffold 有误，权威规格已修正）

- ❌ “新字段可被旧严格解析器忽略” —— **错误**：`Manifest`/`Entrypoint` 均 `#[serde(deny_unknown_fields)]`，旧解析器遇 `[i18n]` 等新字段会**拒绝**；兼容靠 `min_host_version` 门禁，不是 serde default。仅 `AppSummary` 等 wire DTO 走 `#[serde(default, skip_serializing_if)]` 追加。
- ❌ “非法资源不阻塞安装” —— **错误/歧义**：manifest 声明阶段的非法的**声明**（字段组合、路径越界、超限、非 JSON）必须在**安装校验**期拒绝并给出字段级原因；只有**运行期**已安装资源损坏才走 `title_fallback`。两者语义分开。
- ❌ “本期实现所有 Surface” —— **错误**：本期仅 `EntrypointKind::Web`（static_web + Node/Python HTTP）；Declarative/External/None 不在本期，且 External 的 `DOZER_UI_LOCALE` 留待后续。

## 原始决定（被权威规格采纳，保留备查）

- `[i18n]` 与 `title_key`/`title_fallback` 全部 `Option + #[serde(default)]`；旧 manifest 解析/安装/启动逐字节不变。
- 资源装载硬约束：只读已安装包目录内；拒绝绝对路径、`..`/空段/`.`、反斜杠、非 UTF-8、NUL、符号链接逃逸；单文件 ≤ 512 KiB，`strings_dir` 整包 ≤ 8 MiB；非 JSON/非对象/值非字符串/空翻译 → 校验失败并说明是哪条。
- owner 隔离：应用只能声明自身资源，不得影响宿主可信确认/权限/错误文案或其它应用/面板资源。
- 不进入任何已安装包外（解析后 `canonicalize` 且以包根为前缀）。
- wire 只追加；`locale_revision` 是外壳本地显示代次，不与 daemon revision 比大小。
- Surface 语言下发幂等：ready/重建/隐藏后恢复都发最新完整快照，不以“语言事件已发过”漏推。

> 注：本 scaffold 原“File Structure / Task 1–4”的实现拆分已并入权威规格与实施计划，不再单独维护。
