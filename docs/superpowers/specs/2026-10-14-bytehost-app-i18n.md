# bytehost 应用 i18n 契约与产品接入规格（权威版）

> 日期：2026-10-14。**本文件是 bytehost 应用 i18n 契约的权威规格**，由 dozer 仓库评审后迁入（原评审稿：`dozer/docs/superpowers/specs/2026-10-14-bytehost-i18n-integration-design.md`）。
> 实施计划见 bytehost `docs/superpowers/plans/2026-10-14-bytehost-i18n-integration.md`（对应 dozer 侧 `2026-10-14-bytehost-i18n-integration.md`）。
> 旧 scaffold `docs/superpowers/plans/2026-10-14-bytehost-app-i18n-contract.md` 已被本规格取代，仅保留为历史指针与“已纠正的不准确表述”清单，不再作为实施依据。
> 两份仓库不得长期维护互相分叉的契约：本文件是唯一真相，dozer 侧只持有消费范围与链接。

## 1. 目标与边界

让支持此契约的应用在接入产品（如 Dozer）中显示本地化的外壳标题，并取得当前界面语言；声明支持动态更新的 Web 应用在语言切换后更新自己的界面，保留当前编辑和导航状态。语言设置仍由产品管理，bytehost 不增加全局语言设置文件或另一套 daemon revision。

本期覆盖已存在的 `EntrypointKind::Web`，包括 static_web 与 Node/Python HTTP 应用的页面。Node/Python 是服务端运行方式，不能据此归类为 External Surface。沿用现有安装、启动、gateway 与 WebView 安全路径。

本期不新增 Declarative、External、None 入口，不实现完整 V2 Plugin Manifest 的 Panel/Page/Tab/Command 注册系统，不翻译应用用户数据、日志、Agent 输出或第三方应用自身未提供的文案。不修改 `LANG`、`LC_ALL`、`navigator.language`，不通过语言切换重启后台应用。

原 spec 的 External `DOZER_UI_LOCALE` 约定留到 External 入口真正落地时实现；它属于产品适配，不作为 bytehost 通用环境变量。本期不凭空增加外部窗口启动协议。Digger 是兼容性验证对象，不要求本阶段完成它的语言设置产品功能。

## 2. 已核对的代码基线

2026-10-14 本地 bytehost HEAD 为 `bd84bef`（`v0.2.0` 后一条计划提交）；Dozer 的 bytehost 依赖固定在 `v0.2.0`。以下是本次设计依据，不假定本地 HEAD 就是已发布版本。

| 位置（bytehost 仓库内路径） | 当前事实 | 接入责任 |
| --- | --- | --- |
| `crates/bytehost-apps/src/manifest.rs` | schema v1；各层 `deny_unknown_fields`；`name` 与可选 `Entrypoint.title`；仅 Web 入口 | 可选 i18n 声明、标题 key/fallback 与校验 |
| `manager.rs`、`source.rs`、`digest.rs`、`registry.rs` | 安装计划、来源摘要、包复制、升级/回滚及持久化 | 资源校验与版本归属，不绕过计划批准 |
| `proto.rs`、`bytehost-client/src/lib.rs` | `AppSummary` 无本地化元数据；`LaunchUrl` 提供启动地址 | 增量传递经过校验的展示元数据 |
| `bytehost-webview/src/lib.rs` | origin、存储隔离、nonce IPC 白名单与初始化脚本 | 不含产品名称的语言桥纯策略 |
| `bytehost-core/src/surface.rs` | Surface 身份/generation、`PushState`、Application 导航完成就绪 | 语言推送遵循既有生命周期 |
| `bytehost-desktop/src/app_webview.rs` | 共享受限 builder 与 `evaluate_app` | 原生执行器接线 |
| Dozer `runtime.rs`、`app_webview.rs`、`host_adapters/app_host.rs` | 实际应用 WebView 构建及产品 Effect 接线 | 消费公共能力，不能复制安全策略 |

上游已有 `docs/superpowers/plans/2026-10-14-bytehost-app-i18n-contract.md` scaffold。本文明确其中未决项，并纠正“新字段可被旧严格解析器忽略”“非法资源不阻塞安装”“本期实现所有 Surface”的不准确表述。实施时以本次评审后的规格替换相应 scaffold 内容。

> **基线实测补充（T0，2026-10-14）：** `Manifest` 与 `Entrypoint` 均 `#[serde(deny_unknown_fields)]`（manifest.rs:14,43）；`HOST_VERSION = id::Version::new(0,1,0)`（lib.rs:50）；`min_host_version` 比较逻辑为 `min_host_version > *host_version` 时拒绝（manifest.rs:165-170）。`AppSummary` 当前仅由 `manager.rs:1959` 的 `list()` 真实构造，读取已安装包目录的 manifest 取 `name`；其余均为测试 fixture。`AppSummary` 经 `bytehost-client` 的 InProcess/UDS 两路共享同一类型、零转换（bytehost-client/src/lib.rs:99-106, in_process.rs）。`AppSummary` 的传输路径（InProcess 内存 / UDS serde_json 行帧）**无显式单帧字节上限**，因此 128 KiB 是投影层自加的预算上限，不是传输帧限制；实施时需在 `manager.list()` 投影处强制 128 KiB 上限并记录，无需收紧传输协议帧。详见 T0 基线记录 `docs/superpowers/plans/2026-10-14-bytehost-i18n-baseline.md`。

## 3. Manifest 契约

维持 schema_version = 1，在新解析器中增加可选字段；未知字段仍拒绝。使用新字段的包要求 `min_host_version` 不低于实际发布此能力的 host capability version。此版本和仓库 tag 的映射须在发布记录中写明，不根据 crate 当前 package version 猜测。

示例是待实现格式，`min_host_version` 在样例发布时填真实能力版本：

```toml
[i18n]
default_locale = "en"
supported_locales = ["en", "zh-CN"]
strings_dir = "locales"

[presentation]
entrypoint = "main"
title_key = "app.title"
title_fallback = "Notes"

[entrypoints.main]
type = "web"
path = "/"
title = "Notes"
title_key = "entry.main.title"
title_fallback = "Notes"
```

### 3.1 字段及旧值优先关系

- `[i18n]` 可省略。声明时 `default_locale`、`supported_locales` 必填；后者非空、唯一且包含 default。`strings_dir` 默认 `locales`。
- `webview_locale` 取 `none | initial | live`，默认 `none`。标题本地化不隐含页面愿意接收语言。`initial` 仅在新文档初始化时提供语言；`live` 接收后续变化；不存在“未声明也自动注入”。
- `Presentation` 和 `Entrypoint` 追加可选 `title_key` / `title_fallback`。两者必须成对；只给 key 或只给 fallback 均拒绝；必须有 `[i18n]`。
- 原有 `Manifest.name` 和 `Entrypoint.title` 保留；允许与新字段共存。声明 key 时按资源解析，最后用 `title_fallback`；未声明 key 时沿用旧路径：应用名取 `name`，入口取非空 `title`，再取 `name`。
- 标题的 key/fallback 与解析后的译文沿用现有展示字符安全限制；fallback 非空、最多 64 个 Unicode 字符。标题不做 HTML/Markdown 解析，也不支持参数插值。既有 `name` 仍是稳定的原始显示名，不改 ID、入口键或应用注册记录。
- 宿主标题资源仅管理标题；页面可以复用该 JSON，也可以使用自己的内部 i18n 系统。bytehost 不解析应用整个前端的翻译框架。

### 3.2 Locale 与 fallback

本期接受明确的语言标签子集：ASCII language（2–8 字母），可选 script（4 字母）、region（2 字母或 3 数字）；不接受下划线、编码后缀、路径字符或扩展/private-use。规范化为 language 小写、Script 首字母大写、REGION 大写，规范化后查重。资源文件名使用规范标签，大小写不匹配在安装时报错。

bytehost 不把语言集硬编码为 en/zh-CN；Dozer 目前传这两个值。协商只做规范标签精确匹配，不做隐含语言族、简繁转换：当前 UI locale 被声明则采用，否则采用 `default_locale`。未来语言族回退需要单独版本化，不能悄悄改变现有选择。

标题解析顺序：当前语言中的 key → default 语言中的 key → `title_fallback`。缺非默认译文/key 是可接受的部分翻译；默认包必须包含所有声明的标题 key。安装后资源损坏时允许最终 fallback，不让应用列表或宿主崩溃。

页面收到 `ui_locale`（宿主期望）和 `app_locale`（按 supported/default 协商的结果）；页面用 `app_locale` 渲染并设置 `document.documentElement.lang`。两个字段不能混为一谈，例如宿主 zh-CN、应用只支持 en 时分别为 zh-CN / en。

## 4. 资源生命周期与信任边界

### 4.1 格式及限额

`<strings_dir>/<canonical-locale>.json` 为 UTF-8 扁平 string→string 对象，无嵌套/ICU。拒绝重复 JSON key、非字符串值、空白值、非法编码。key 为 1–128 个 ASCII `[A-Za-z0-9_.-]` 字符。普通值最多 4096 个 Unicode 字符；用作标题的值额外受 64 字符及现有控制字符/bidi 校验约束。

初始限额：最多 16 个声明语言，单文件 512 KiB，声明资源总计 8 MiB，每包文件最多 4096 个 key。采用限长读取（limit+1）而非读完整文件后才判断大小；限制在解析前生效。只枚举声明语言，不递归扫描任意目录；额外文件不装载为宿主词表。默认语言文件必须存在；可省略非默认文件以表示未提供翻译，但已存在的非法文件必须拒绝。

### 4.2 安装与运行期

安装计划生成时验证来源暂存包；正式 install/upgrade 时在受控 staging 内重验，资源必须进入来源摘要覆盖范围，随后才原子提交。批准后修改资源必须触发现有源变化/计划不匹配处理；不能只验证 manifest 然后读来源目录的实时文件。

`strings_dir` 必须是包根内的相对路径，拒绝绝对路径、`.`/`..`/空段、反斜杠、NUL；拒绝资源路径各组件的符号链接与非普通文件。校验后打开也必须保持包内约束，防止检查与读取之间替换路径；仅 `canonicalize` 再普通 open 不作为充分实现。复用上游安全 staging/文件访问机制，包外读取测试必须覆盖链接替换。源路径、暂存路径与安装路径分别验证，不能把尚未安装的来源称为“已安装包”。

运行期只从已安装的活动版本取经校验资源，绝不从原始 LocalDir、远端 URL 或用户 data 目录补读。缓存键至少含 app ID、活动包版本/摘要；升级、回滚、卸载必须失效。I/O/解析移到服务端阻塞工作池，UI 线程不读磁盘。运行期缺失/损坏降级为 manifest fallback，并记录有界诊断；安装期同类错误拒绝安装，两者语义明确分开。

owner 由宿主绑定为应用 ID，应用不能提供或改写 owner。应用词表不进入 Dozer host/panel 的资源注册表。可信权限、批准按钮、风险说明和宿主错误始终来自宿主资源；即使 key 同名也不能覆盖。安装审批继续显示原 `name`、app ID、版本、来源及权限，不以可翻译标题替换身份依据。

## 5. 上游到产品的数据接缝

在 `AppSummary` 追加 `display_i18n: Option<AppDisplayI18n>`，serde default + None 不序列化；DTO 类型命名可随上游评审调整，语义须保持。旧 `name` 不改义、不替换为某个进程的当前语言。

`AppDisplayI18n` 包括 default/supported、webview_locale、presentation 与各入口的标题描述及其已校验语言值。只投影 manifest 引用的标题 key，不把整份 8 MiB 词表、包文件路径或启动 token 带入 List。最多 64 个入口参与 i18n，投影序列化后上限 128 KiB；新声明超限在安装时报错，旧无 i18n 包不受新增入口数限制。已有传输帧上限若更小，实施前统一收紧并记录，禁止无上限加大帧。

纯 resolver 属于 bytehost，产品调用它选择标题；Dozer 只把自己的 active locale 作为参数。UDS 与 `InProcess` 返回同义 DTO，`AppHostApi` 不增加一个必须由所有产品实现的语言设置服务。`LaunchUrl` 的 URL/token 与业务语义不变，不把全局语言写进服务端共享 AppRecord。

无新元数据的新 GUI 继续用 `name/title`；非预期元数据形状降级为旧名，不重试风暴。此处不新增 wire enum variant，避免旧客户端因未知 variant 无法反序列化。新增 Rust struct 字段仍可能要求编译期调用方更新，不能用 JSON 兼容来声称 Rust 源码零变更。

`bytehost-panel` 的共享状态保存元数据、不保存已格式化的最终标题。应用 list/rail/tab 在视图阶段按产品语言解析，语言变化无需重拉启动地址、不改变 slot 或 Surface ID。公共状态机既有 Notice/错误文本的全面结构化迁移不是本期应用语言契约的前置条件；出现阻塞验收的外壳文案问题按独立 bug 处理。

## 6. WebView 语言桥 v1

### 6.1 能力与接口

能力由经过校验的 manifest 声明和宿主桥版本共同决定。v1 不增加页面→原生的语言握手/设置权限，不把内置面板 `__host.dispatch` 暴露给应用。Application 仍以导航完成作为 Surface ready，语言监听器是否已注册不影响应用启动。

建议 JS 公共接口如下（待实现）：

```javascript
const snapshot = window.bytehost?.i18n?.getSnapshot();
const unsubscribe = window.bytehost?.i18n?.subscribe(snapshot => {
  // 应用使用 snapshot.app_locale 更新自身 i18n 状态。
});
```

快照包含 `version: 1`、`ui_locale`、`app_locale`、`locale_revision`。`locale_revision` 在线上为十进制字符串，避免 Rust u64 超出 JS 安全整数；页面比较使用受约束的 BigInt 或等价十进制比较。它是本 GUI 的显示代，不是 daemon 持久化 revision，也不跨窗口比较。

`getSnapshot()` 返回独立只读副本；`subscribe()` 注册后立即回调当前快照，返回幂等注销函数。无声明不安装该 API；`initial` 安装只读初始快照，订阅立即返回一次值，不接收同文档内变化；`live` 支持完整通知。宿主写入入口由上游封装，不能给页面可调用的宿主命令能力。页面篡改自己的语言显示不被当作权限边界，不能反向修改产品语言。

> **注入接缝结论（T0 spike，2026-10-14）：** 初始化脚本（`bytehost-webview::app_init_script`，经 `app_webview.rs:104-117` 的 `with_initialization_script` 注入）是**在 WebView 创建时一次性冻结**的，wry 的 `add_initialization_script` 在每次文档加载时以**相同冻结内容**重跑，无法携带运行时变化的 locale 值。因此新的语言快照**不能**依赖初始化脚本携带；每文档的最新值必须由宿主在导航完成后经 `evaluate_app`（app_webview.rs:163-172）下发。结论：桥在首个完整快照到达前处于**未就绪（pending/skeleton）**状态，绝不可返回创建 WebView 时捕获的已知过期值。这是 spec 明确允许的退化路径，必须有测试与验收证据（见 `2026-10-14-bytehost-i18n-baseline.md` 的 T0-4 记录及 spike harness）。

### 6.2 状态与更新顺序

- 主机绑定使用既有 Surface instance/slot/generation；每次同源文档导航再建立 document epoch。旧实例/旧文档的完成回调不得更新新文档的 sent 状态。
- 首次 ready 发送最新完整快照；`live` 的 desired/sent 比较包含两个 locale 与 revision。仅语言变更也必须推送。
- 隐藏时保留最新 desired，恢复后推最新值；隐藏不意味着销毁，也不能把未执行的脚本标为已发送。
- 重建、刷新、同源导航均清空 sent，按新文档重新 bootstrap。`initial` 在新文档也采用最新语言，但不为了语言主动导航。
- 同一文档内旧 revision 丢弃，相等 revision 幂等，不同文档可重新建立基线。语言推送独立于业务 payload，不让业务响应覆盖较新语言。
- 脚本执行失败不标记 sent；采用有界退避（建议 250ms/1s/2s，三次后等下一次 ready/恢复/语言变化），不在 about_to_wait 零间隔循环。执行成功仅表示桥接收快照，不宣称第三方框架已完成渲染。

热切换不改 URL/query、token、history、焦点、滚动、编辑草稿，不触发 WebView reload、runtime restart 或重新登录。`initial` 应用在下一次自然导航/打开时使用新语言；宿主若需要说明，使用已有状态说明区域，不强制弹窗。

### 6.3 安全接线

bytehost-webview 提供序列化与脚本/快照的纯策略；bytehost-core 管生命周期语义；bytehost-desktop 执行原生动作；Dozer 当前独立 builder 也调用同一公共策略。本期不借机强制重构整个产品 builder。

locale 数据由 JSON 序列化生成，不能把应用 key/标题直接拼入可执行 JS。桥只装在符合现有 app origin 策略的主文档，不能传给跨源 iframe 或外部浏览器。导航 origin 检查、CSP、权限、下载/devtools/new-window 限制和每应用存储标识保持不变；既有私有 nonce 不得经 getter、事件、日志暴露。新增语言通道不接受任意 IPC 动词、不注册 `dozer://`。

## 7. 兼容与发布

| 组合 | 预期 |
| --- | --- |
| 新 bytehost + 旧无 i18n manifest | 安装/启动/展示路径不变，无桥 |
| 旧 bytehost + 新字段 manifest | 严格解析器可能在版本检查前拒绝；不承诺可安装，也不全局放宽 unknown fields |
| 新 GUI + 旧 daemon | 缺 `display_i18n`，用旧名，不启用应用桥；内置 UI 语言功能照常 |
| 旧 GUI + 新 daemon | 忽略新增可选 summary 字段，旧功能继续；需真实旧 JSON decoder fixture 验证 |
| 新 GUI + 新 daemon + 部分翻译应用 | 标题精确语言/default/fallback；页面得到协商后的 app_locale |
| 新宿主升级应用后回滚 | 资源与桥能力随活动包回滚，缓存不串版本；不改浏览器存储归属 |

正式发布用 bytehost 同一实际 tag 更新所有相关 crate，联调 path patch 不提交。先发布上游再更新 Dozer；不写一个尚不存在的版本号进依赖。新 GUI/旧 daemon 的兼容验证通过后才允许独立更新。回退旧宿主不保证能读取新 manifest，应保留旧应用包或先按受支持流程回滚应用，禁止通过剥掉新字段绕过审批摘要。

## 8. 验收与完成定义

| ID | 场景 | 必须观察到的结果 |
| --- | --- | --- |
| B01 | 旧应用、旧 wire fixture | 新宿主兼容旧应用；旧客户端兼容新增 DTO 字段 |
| B02 | manifest 组合、locale、重复 key/非法资源 | 合法组合通过；非法声明安装期拒绝，给出字段级原因 |
| B03 | 路径穿越、链接/替换、超限 | 包外零读取；限长读取；不扩大可信文案范围 |
| B04 | 批准后改资源、升级/回滚/卸载 | 摘要重验；活动版本切换；缓存正确失效 |
| B05 | zh-CN/en、缺译文、未知宿主语言 | 标题非空，精确/default/fallback 与页面 app_locale 一致 |
| B06 | 首开、订阅晚注册、语言变更后再导航 | 最新快照可取得，不漏首次值、不沿用旧文档快照 |
| B07 | 隐藏/恢复、重建、旧脚本晚回、同值重推 | 最新状态收敛；不跨 generation/epoch 污染 |
| B08 | live 热切换 | 无 reload/restart；草稿、滚动、登录状态保留 |
| B09 | initial / none、两个窗口不同显示代 | initial 不热改；none 无桥；窗口 revision 不互相覆盖 |
| B10 | origin/CSP/nonce/权限隔离 | 既有安全测试通过；无新宿主命令权限或 nonce 泄漏 |
| B11 | 执行失败、运行期资源损坏 | 有界重试与 fallback，UI 无忙循环、崩溃或 Toast 风暴 |
| B12 | Dozer UDS + bytehost InProcess | 同一 DTO/协商行为；打包后不依赖开发仓库资源路径 |

交付至少含 bilingual-live、partial-initial、legacy 三个应用样例，以及拒绝安装的安全测试夹具。外壳标题和应用内部 UI 分开记录；只有样例真正消费桥并完成双语渲染才标“应用 UI 接入成功”。

I4 本期完成 = 上游代码/契约/测试 + 已发布 tag + Dozer 适配 + B01–B12 证据。报告明确“现有 Web 应用契约已完成”，不宣称所有未来 V2 Surface 或任意第三方应用已本地化。
