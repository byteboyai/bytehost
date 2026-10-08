# bytehost 应用宿主(App Host)设计

> 状态:**草案,待用户审阅**。日期:2026-10-04。
> 来源:群聊"关于引入容器、Python、Nodejs 虚拟环境的讨论"(2026-10-04,dozer.db `chat_groups.id=2`,共 12 条;codex/claude 与用户)。本文把讨论结论固化,并用 `spike/origin-gateway` 的实测结果回答其中**未裁决**的 gateway 方案问题。
> 与现有 bytehost 文档的关系:`2026-10-03-bytehost-boundary-design.md`(E1/E2/E3 边界)、`2026-10-04-bytehost-panel-hooks-and-registry-design.md`(面板钩子与清单)讲的是**面板宿主**;本文讲的是**应用宿主**,两者是 bytehost 的两层,互不绑定。
> **标注约定:【群聊已定】= 讨论中用户与两位 agent 已达成一致;【本文建议】= 本文新增、需要用户确认;【待裁决】= 需要用户拍板。**

## 1. 目标与非目标

**目标。** 让 Dozer、Digger 等宿主产品能"安装"第三方 Web 应用(如 Excalidraw)或 Agent 现场生成的 Python/Node 程序,装好后在 left/right rail 上出现图标,点开是一个承载该应用的浏览器面板——产品因此成为用户可以自我塑造的应用。【群聊已定】

**非目标(一期)。**
- 不做应用商店/远程分发/多用户/远程访问鉴权;单机、单用户、显式授权。
- 不实现容器和 Python/Node runtime(只定义 adapter 接口与能力探测)。
- 不在 bytehost 里决定 Rail 的摆放与确认页的样式——那是产品层。【群聊已定】
- 不把 Dozer 的"治理与验收"身份写进平台;"Agent 生成的应用必须经确认才能启动"是 Dozer 在平台数据之上的**产品选择**,Digger 可以更宽松。【群聊已定】

## 2. 已定原则(群聊)

1. 机制归 bytehost,策略与呈现归产品。Rail 属于产品层;bytehost 只提供应用名称、图标、入口、状态和事件。
2. venv/Node 项目目录只解决依赖隔离,**不是安全边界**;未知代码(第三方、Agent 生成)应使用容器或同等级沙箱。macOS 上非容器层没有轻量进程沙箱,权限页必须如实标注"不隔离,仅限可信来源"。
3. 每个应用一个独立、**跨重启稳定**的 origin;不共享 `/apps/<id>/`,也不每次启动随机更换。
4. 程序包、用户数据、缓存、日志从第一版就分开;卸载"程序"与删除"数据"是两个操作。
5. 安装是两阶段:`install_plan → 审批 → install`。bytehost 产出可审查的计划,**是否需要确认由产品决定**。
6. 审批绑定内容:`ApprovedInstallPlan` 绑定 manifest 摘要与源码摘要(防审批后被替换,TOCTOU)。
7. manifest 里的是**权限申请**;用户**实际授予**的权限单独存储(否则做不出升级时的权限 diff,也分不清"批准过"与"自己声明的")。每条权限标注强制等级:`enforced` / `advisory` / `unsupported`(例如静态 Web 的出站网络限制可由 CSP 强制;Node/Python 的 `network: none` 在 macOS 上只能是 `advisory`)。
8. 安装计划包含:来源(provenance)、信任级别、将执行的命令、将访问的目录、网络能力、权限 diff。
9. 耗时操作(`prepare`、`start`)是异步任务:有进度、可取消、结构化错误;进度走事件。
10. Agent 开发循环需要 `AppSource::LocalDir`(开发模式、原地运行、不产生版本);发布/升级再生成不可变、可校验的版本。
11. manifest 对安全相关字段 `deny_unknown_fields`;声明 `min_host_version`——新版权限被旧宿主静默忽略是安全问题。
12. bytehost 的运行时只做**探测、可选启用**,不内置 Node/Python/容器(沿用 Dozer 的"核心不依赖 Node/Python"口径,但要在 bytehost 自己的规格里重新声明,不靠引用 Dozer 的 CLAUDE.md)。

## 3. 分层与模块

```text
产品 UI(Dozer / Digger:Rail 摆放、确认页、默认信任策略)
   ↓
app_service  (对外薄封装:list / inspect / install_plan / install / start / stop / uninstall;事件流)
   ↓
app/manager ──→ runtime(adapter:static_web | process{node,python} | container)
   │                  ↓
   │              gateway(独立 origin、Host 校验、代理、WebSocket)
   ↓
app/registry + app/storage + app/model(manifest、授权、状态、事件)
```

- 依赖方向单向;**不现在就拆 crate**,但模块边界按此划,日后分拆不改接口。【群聊已定】
- 命名:避免与已有的 `PanelHost` 混淆,统一叫 `AppService`(对外)/`AppManager`(内部)。【本文建议】
- `app_service` 是 `app/manager` 之上的薄封装,只做类型转换与事件订阅;它不持有状态。【本文建议】

### 3.1 与现有"面板宿主"的接口:Rail 条目必须能容纳非枚举项

应用要在 rail 上出现,rail 条目就不能只是封闭枚举 `PanelKind` 的 12 个成员,而需要一个"面板 id"能表达 `app:<app-id>`;同时**每个应用有自己独立的入口和独立的 WebView 状态,不复用 Web(浏览器)面板**(用户 2026-10-04 确认:应用走 wry 显示,但不在浏览器面板里打开),而现在 `Workspace.browser` 只有一个实例。这对应面板宿主设计里的 **H7b 的最小版**(只让 rail/布局认识动态条目,不做 H8 的全量动态化),见 `docs/dozer-v2/bytehost-H8-evaluation.md`。落盘兼容:现有 rail 布局序列化为枚举名字符串(如 `"Files"`),动态条目用带前缀的字符串(如 `"app:excalidraw"`),旧文件原样可读(已有黄金测试保护)。【本文建议】

### 3.2 需要新建一个无界面的 bytehost crate

**为什么必须新建。** 应用管理要运行在 dozerd 里(§6.1),而 dozerd 不能依赖 iced/wry;`dozer-core::protocol` 是 Dozer 专有的(3085 行,含全部 Dozer 领域类型),应用模型若写进去,Digger 就复用不了;而 `dozer-app` 是带界面的 bin,更不能被 dozerd 依赖。所以应用宿主的**无界面部分**必须落在一个独立 crate 里,被 dozerd(服务端)、dozer-core/dozer-client/dozer-app(只要类型)依赖。【本文建议】

**形态:一个 crate,两层 cargo feature,不提前拆多个 crate。**(沿用群聊 10-03 的"一期只建一个 bytehost crate,不提前拆"口径;这里的区别只是它**不含 iced**。)

```text
crates/bytehost-apps/                      # 已定,见 A10
  features: (默认空)  = 只有类型与纯逻辑,依赖仅 serde/serde_json
            "digest"  = 摘要计算(sha2),dozerd 与需要校验摘要的 GUI 侧打开
            "server"  = manager / runtime adapter / gateway,依赖 tokio + HTTP 栈(隐含 digest);只有 dozerd 打开
  src/model/    manifest(解析 + deny_unknown_fields + 摘要)、权限与授权、InstallPlan、AppState、AppEvent
  src/proto/    GUI↔supervisor 的线上类型:AppRequest / AppResponse(serde)
  src/registry/ 安装记录与 state.json 读写(纯文件 I/O,无异步)
  src/manager/  [server] desired/observed 对账、任务句柄
  src/runtime/  [server] adapter trait + static_web(一期)+ process/container 骨架与 probe
  src/gateway/  [server] 本机 HTTP 服务:Host 校验、静态文件、反向代理、WebSocket 升级
```

**依赖方向(单向,且 bytehost-apps 不依赖任何 Dozer crate):**

```text
dozer-app (GUI,wry 显示、rail 入口、Settings、提示页) ──┐
dozer-client (UDS 客户端:新增 app_* 方法)  ─────────────┤── 依赖 bytehost-apps(无 feature:仅类型)
dozer-core (protocol::Request 里新增 App(AppRequest))  ──┘
dozerd (supervisor)  ── 依赖 bytehost-apps(features = ["server"]),在 server.rs 里把 Request::App 转给 AppManager
```

- **线上协议:** `bytehost-apps::proto` 定义 `AppRequest`/`AppResponse`/`AppEvent`;Dozer 的 `dozer-core::protocol::Request`/`Response` 各加一个 `App(..)` 变体把它们**原样带过去**(Dozer 自己的 socket、自己的版本),Digger 将来用同一组类型走它自己的 socket。这样应用宿主的类型不进 `dozer-core::protocol` 的 3085 行里,又不需要第二条 socket。
- **dozer-hook 与 dozer-mcp 的传递依赖:** 它们依赖 `dozer-core`,而 `dozer-core` 现在会依赖 `bytehost-apps`;因为 `bytehost-apps` 默认 feature 为空、只带 serde/serde_json(`dozer-core` 本来就依赖这两个),hook 二进制的依赖闭包**不增加新的 crate**。这是把 `sha2` 放进 `digest` feature、而不是默认依赖的原因。
- **新增依赖(需用户知悉):** `bytehost-apps` 默认无新依赖;`digest` feature 需要 `sha2`;`server` feature 需要 HTTP 服务/代理栈(倾向 `hyper` 1.x + `hyper-util` + `http-body-util`,WebSocket 复用已有的 `tokio-tungstenite`),以及将来解 Archive 的 `tar`/`zip`。这些依赖只进 dozerd,不进 GUI。
- **与面板宿主(`dozer-app` 里的 `panel_host`/`panel_registry`)的关系:** 暂时不动、不合并;面板宿主的 crate 化是 H8/O7 之后的事。两者靠 rail 的"条目 id"(§3.1)在 GUI 里接起来。
- **本规格范围外的约束:** 为了让 `bytehost-apps` 日后能被 Digger 使用,它**不得**出现 `dozer`/`Dozer` 字样的类型或路径,不得依赖 `dozer-core`;门禁可用 `cargo tree -p bytehost-apps` 检查。

## 4. 应用模型

### 4.1 Manifest v1(最小)

> **格式已定(2026-10-04,用户):TOML。** 下面的示例保留 YAML 写法只是为了与群聊原文对照;实际的 `manifest.toml` 见 `crates/bytehost-apps/src/manifest.rs` 里的测试样例(`[presentation]`、`[entrypoints.main]`、`[runtime]`、`[permissions.network]`…)。

```yaml
schema_version: 1
min_host_version: "0.1.0"
id: excalidraw                 # 稳定、小写、[a-z0-9-];也是 origin 的来源
name: Excalidraw
version: 0.17.0
presentation:
  icon: assets/icon.svg
  surface_hint: browser          # 中性提示,不用 browser_panel 这类产品词
  entrypoint: main
entrypoints:
  main: { type: web, path: "/", title: Excalidraw }
runtime:
  kind: static_web               # 后续:process{node|python} / container
  source: web/
permissions:                      # 申请,不是授予
  network: { outbound: none }
  filesystem: { data: read_write }
  clipboard: read_write
  downloads: user_confirm
  popups: deny
health: { path: "/", timeout_ms: 3000 }
```

`runtime` 以外的部分对所有 runtime 通用;换 runtime 只替换 `runtime` 段(如 `kind: python` + `command` + `dependencies.lockfile` + `http.port_env`,或 `kind: container` + `image@sha256` + `http.container_port`)。

### 4.2 授权存储与强制等级

- `manifest.permissions`(申请)与 `state.grants`(授予)分开存;升级时 diff 的是"新申请 vs 当前授予"。
- 每条权限在安装计划里带 `enforcement: enforced | advisory | unsupported`,由 runtime adapter 在 `probe` 时给出;**UI 必须原样展示**,不得把 `advisory` 画成"已隔离"。
- 密钥不进 manifest/`state.json`,只存引用,由宿主的凭据服务注入。

### 4.3 安装流

```rust
trait AppService {
    fn list(&self) -> Result<Vec<AppSummary>>;
    fn inspect(&self, id: &AppId) -> Result<AppDetails>;
    async fn install_plan(&self, source: AppSource) -> Result<InstallPlan>;     // 含 manifest 摘要 + 源码摘要 + provenance + trust + enforcement + 权限 diff
    async fn install(&self, approved: ApprovedInstallPlan) -> Result<InstalledApp>; // 校验摘要未变
    fn start(&self, id: &AppId) -> TaskHandle<AppEndpoint>;
    fn stop(&self, id: &AppId) -> TaskHandle<()>;
    fn uninstall(&self, id: &AppId, mode: UninstallMode) -> TaskHandle<()>;     // 程序 / 程序+数据
    fn events(&self) -> Receiver<AppEvent>;
}
```

`AppSource::{LocalDir(path) /*开发模式*/, Archive(path|url + sha256), Registry(..) /*暂不做*/ }`。`TaskHandle` 提供进度、取消、结构化错误;进度作为 `AppEvent::Progress` 上报。

### 4.4 生命周期状态

不再用单一状态机,而是 **`desired_state` 与 `observed_state` 分开**,由 manager 持续对账:

```text
desired: Stopped | Running | Removed
observed: NotInstalled → Installed → Preparing → Starting → Running → Stopping → Stopped
          任意运行阶段 → Failed{reason, retryable}   (Failed 可经 start 重试,或经 stop 复位)
          Installed/Stopped → Updating → Installed
          Installed/Stopped → Uninstalling → NotInstalled
```

宿主重启后按持久化的 `desired_state` 对账(例如 `desired=Running` 而 `observed` 未知 → 探测进程/端口,健康则回到 `Running`,否则 `Failed`)。【本文建议,回应群聊 claude 指出的"Failed 没有出口、宿主崩溃语义缺失"】

### 4.5 事件

```rust
enum AppEvent {
    Installed(AppId), StateChanged(AppId, AppState), EndpointChanged(AppId, Option<AppEndpoint>),
    ManifestChanged(AppId, ManifestDiff), Progress(AppId, TaskId, Progress), LogAvailable(AppId),
    RuntimeUnavailable(AppId, RuntimeReason),
}
```

产品层据此更新 rail 图标状态;bytehost 不理解"左栏/右栏"。

### 4.6 磁盘布局

```text
bytehost/apps/<app-id>/
  manifest.yaml      # 当前版本的申请
  state.json         # desired_state、grants、版本记录、端口/存储标识(不含密钥)
  package/<ver>/     # 不可变、可校验的程序内容(LocalDir 开发模式不拷贝)
  data/              # 卸载时可选择保留
  cache/             # 可随时清理
  logs/              # 轮转,有配额
```

## 5. Gateway:每应用独立 origin(用 spike 实测回答群聊的未决项)

### 5.1 实测结论(完整表与未覆盖项见 `spike/origin-gateway/README.md`)

wry 0.55.1、WKWebView、macOS 26.6.2。三种方案的差异**不是实现细节**:

| 能力 | 自定义协议 | `<app>.localhost:端口` | `127.0.0.1:端口` |
|---|---|---|---|
| 同源 WebSocket | ❌ | ✅ | ✅ |
| 真流式(SSE/长连接) | ❌(**推断**:wry 应答类型是整块 `Vec`,未实测真流式) | ✅(分 3 条、间隔 100ms 的 SSE) | ✅(同左) |
| Cookie | ❌ | ✅ | ✅ |
| Service Worker | ❌(只允许 http/https) | ✅ | ✅ |
| localStorage / IndexedDB 跨重启 | ✅ | ✅ | ✅ |
| 不同 `data_store_identifier` 的存储隔离 | ✅ | ✅ | ✅ |

另有两条直接影响设计的发现:
- **origin 含端口。** 端口变了,localStorage/IndexedDB 就丢(实测);所以端口必须稳定。
- **Cookie 不区分端口。** 同一主机名下不同端口的应用共享 cookie(实测:换端口后 cookie 仍可读)。所以 `127.0.0.1:<不同端口>` 在 cookie 层并不隔离,而 `<app-id>.localhost` 因主机名不同天然隔离。

### 5.2 建议【用户已定(A1),2026-10-04:采纳 1–5,端口策略见 5.2 末】

1. **gateway 采用 `http://<app-id>.localhost:<单一固定端口>/`,Host 头路由。** 一个固定端口服务所有应用(按 Host 分发),避免每应用一个端口的分配、冲突与"端口变了丢数据"问题;`<app-id>` 稳定 ⇒ origin 稳定;主机名不同 ⇒ cookie/存储/Service Worker 作用域天然分离。该端口写进 registry 并持久化,被占用时**显式报错**而不是静默换端口(换端口 = 所有应用丢本地数据)。
2. **再叠一层 `data_store_identifier`(每应用一个)。** 实测不同标识即使 origin 完全相同也完全隔离 localStorage/IndexedDB/Cookie——这是比 origin 更硬的隔离杠杆。代价:`with_data_store_identifier` 需要 macOS 14+(低版本退回只靠 origin 隔离,并在安装计划里标注)。
3. **自定义协议不作为应用 gateway。** 它没有同源 WebSocket、没有真流式、没有 Cookie、没有 Service Worker,Excalidraw 这类应用会在细节上坏掉。它仍可用于 host 自己的静态页(如现有 `dozer://html/`)。
4. **Host 校验是硬要求。** gateway 只接受 `Host` 为已注册的 `<app-id>.localhost:<端口>` 的请求,其余一律拒绝;这是防御 DNS rebinding 的主要手段。另加每会话 token(放在 Cookie 或首次导航的一次性参数里,由宿主注入),防止本机其他网页直接探测应用。**本 spike 没有做攻击实验,这条是设计约束,不是实测结论。**
5. `AppEndpoint` 对产品暴露 `{ url, origin_id(跨重启稳定), capabilities }`,不暴露 gateway 的实现(以便日后换成别的承载)。
6. **端口策略【用户已定,2026-10-04】:** 首次启动从 **20000–32767**(低于 macOS 与 Linux 的临时端口段,避免被监听端口 0 的程序占走)随机选一个并持久化,跨重启不变(避开 3000/8080 等常用开发端口);选定后若被占用,**显式报错**并在 Settings 给出修改入口,**不静默换端口**——改端口会让所有应用丢失本地存储(origin 含端口),所以修改前必须明确确认。两个产品(Dozer/Digger)同机运行时各自持久化各自的端口,互不冲突。

### 5.2.1 显示与运行的分工【用户已确认走 wry】

- **显示:** 每个应用一个 wry WebView,加载 `http://<app-id>.localhost:<固定端口>/`(不是自定义协议),带该应用自己的 `data_store_identifier`。应用有**独立入口**,不在 Web 面板里打开,也不复用它的地址栏/收藏夹/标签状态。
- **运行:** 应用进程(Node/Python)、容器(Docker)**不是 wry**,由宿主或 supervisor 启动的子进程/`docker run` 承载;gateway 是 Rust 进程里的本机 HTTP 服务(按 Host 路由、反向代理、转发 WebSocket 升级)。
- **入口形态【用户已定 A6,2026-10-04】:** 选 (a)——rail 图标打开**该应用自己的面板**(在 left/right 栏里,与其他面板同级)。因此一期前置需要 H7b 最小版(§3.1,A3);且受"webview 恒在 iced 之上"约束:应用面板上方的 iced 浮层必须显式下推/隐藏 webview 矩形。"弹出为独立窗口"((b),复用 `platform/overlay_window.rs`)作为以后的可选能力,本规格不做。

### 5.3 仍需验证(在落地前的 spike 清单)

- ~~**V1** 单一端口 + Host 路由 + 多应用同时打开时的实际表现~~ **已实测通过(2026-10-04,见 `spike/origin-gateway/README.md` 的 V1 一节):** 同端口、同存储标识下两个应用的 localStorage/IndexedDB/Cookie 完全互不可见;
- ~~**V2** 真实 Excalidraw 静态构建的端到端(base path、字体、剪贴板读写、下载、弹窗)~~ **已验证(2026-10-05,见 `specs/2026-10-05-bytehost-a5-acceptance-report.md`):** 原样构建在严格 CSP 下字体全坏(内联脚本被拦、资源路径未设),打包配方(`scripts/bytehost/excalidraw/`)后通过;剪贴板/下载在 wry 无用户手势下不能自动探测,留手工;
- **V3** 外部浏览器与 DNS rebinding 的攻击面(Host 校验 + token 的有效性);
- **V4** 不带 `data_store_identifier` 与 macOS 14 以下的行为;
- **V5** 非 macOS(WebView2/WebKitGTK)上 `*.localhost` 解析与数据目录隔离。

## 6. 运行时:进程所有权、来源与不可用时的呈现

### 6.1 进程所有权【用户已定 A2,2026-10-04】:跟随 dozerd

**应用的进程/容器由 dozerd 监督,dozerd 停止则应用停止。** 这等价于 S1(独立于 GUI 的 supervisor),supervisor 就是 Dozer 已有的会话守护进程:

- GUI 退出不影响应用(dozerd 本来就让会话在 GUI 退出后存活);dozerd 停止(用户在设置里停止、或崩溃)则应用随之停止。
- `app/manager`(§3)作为库链接进 dozerd;GUI 通过 dozerd 的 UDS 协议做客户端。生命周期状态 desired/observed(§4.4)由 dozerd 持久化与对账。
- **进程型应用:** dozerd 退出时杀整个进程组;dozerd **崩溃**时(macOS 没有 `PR_SET_PDEATHSIG`)会留下孤儿进程,所以启动时必须按 pidfile/进程组对账并回收孤儿。
- **容器:** 容器本来独立于 dozerd,所以"dozerd 停则停"要靠 dozerd 优雅退出时 `docker stop`;崩溃留下的孤儿容器靠 `bytehost.app=<id>` 标签在下次启动时找回并清理。
- **gateway 同样放进 dozerd**【用户已定,A7,2026-10-04】:它与应用同生命周期,`<app-id>.localhost:<端口>` 的 origin 在 GUI 重启时保持不变,静态 Web 应用也由它提供文件。GUI 里的 wry 只是客户端。
- **Digger 没有 dozerd。** 所以 supervisor 必须是 bytehost 定义的**接口/协议**,dozerd 是 Dozer 的实现;Digger 需要自己的实现(自带守护进程或进程内嵌入,见 A8)。接口按"client-style、desired/observed、任务句柄"设计,不假定背后是 dozerd。

### 6.2 运行时来源【用户已定,A9,2026-10-04:Settings 可装 uv/Python 与 Node,Docker 只探测】

- **先探测系统已有的**(`node`/`npm`/`pnpm`、`uv`/`python3`、`docker` 及当前 context),探测结果分层:没装 / 装了但不可用(如 Colima 没启动)/ 可用。
- **Settings 面板提供"运行时安装/管理"**:这是产品 UI;机制归 bytehost 的 `RuntimeManager`(探测、安装、列出版本、卸载),产品的 Settings 调它。
- **能装什么要区分:** uv(及其管理的 Python)、Node 可由 bytehost 下载到自己的目录(固定版本 + 校验和,不动系统环境,显式确认);**Docker/Colima 不能由 bytehost 安装**(需要系统权限与虚拟机),只做探测、状态说明与指引。一期(只做静态 Web)不需要任何运行时,这一整块属于二期 Node/Python runtime 的范围;一期 Settings 只展示探测结果。
- **下载是安全敏感操作:** 版本固定、校验和强制、来源可审、下载在安装计划/设置界面里明示;不静默下载。**已落地(A6d)**:`runtime::managed` 装到 `<root>/runtimes/<name>/<version>/`,固定版本表由 `scripts/bytehost/pin-runtimes.sh` 从官方源生成,下载走系统 `curl`/`tar`,Python 的哈希由 uv 内置校验(安装计划里如实披露),受管运行时经 `ChainResolver` 优先于系统,安装计划在服务端重算并逐字段核对;只有 macOS 有 pin。
- 本机现状(用户机器实测):docker 29.6(context=colima)、node 24.14、python 3.13、uv 均已可用。

### 6.3 运行时/容器不可用时的呈现【用户已定,2026-10-04】:在该应用的 wry 窗口里提示

应用无法启动(运行时缺失、Colima 没启动、容器不可用……)时,**应用自己的面板(wry 窗口)里显示一张提示页**,而不是弹别处的通知:说明原因并给出可用的动作(启动 Colima、去 Settings 安装运行时、重试;对未知来源应用还可以"明确确认后以普通进程运行")。要点:
- **不得静默降级。** 未知来源应用在没有容器时不会悄悄变成裸跑进程;"以普通进程运行"必须是用户在提示页上的显式选择,并写入授权记录。
- **提示页由 host 提供,不由应用 origin 提供。** 否则它与应用共享 origin/存储,会被应用(或应用的残留状态)伪造;应放在 host 自己的页面承载上(与现有 `preview_fallback_page` 同一思路),动作经 host 的消息回到 `AppService`。

### 6.4 信任模型的两点说明(A2/A1 评审留下的,写下来而不是留作暗知识)

- **UDS 上的请求等于"同一用户"。** dozerd 的 socket 只有同一用户的进程能连——包括在 Dozer 的 PTY 里运行的 Agent。所以任何同用户进程都可以用 `Request::App` 自己出计划、自己批准(`Approval.approver` 与 `trust` 都是客户端自己填的)、自己安装并启动应用。这与 `CreateSession` 已经能运行任意命令是同一个信任级别,**没有引入新的权限**;但"甲方批准"(用户在界面上确认)只是**产品层的约定**,不是 dozerd 强制的——需要强制时,要由 supervisor 保存计划并只接受它自己签发的批准(留给有进程型 runtime 的切片)。
- **`LocalDir` 安装会把整个源目录永久拷进应用目录**(含 `.env` 之类的敏感文件),即使 gateway 只提供 `runtime.source` 指向的子目录。`path` 必须是绝对路径(相对路径会按 dozerd 的工作目录解析)。崩溃时遗留的 `.staging-*` 在 manager 下次启动时清扫。

- **应用 webview 的 wry 默认值不是安全默认。** wry 0.55 默认放行下载、debug 构建开开发者工具、**对摄像头/麦克风请求无条件批准**。前两项已在 `build_app_webview` 显式关闭;媒体权限 wry 没有钩子可改,目前只靠 macOS TCC(Dozer 没声明摄像头/麦克风用途)挡着——将来 Dozer 若加语音输入等功能,必须先给应用 webview 加授权闸门(对照 manifest 的 grants),否则所有应用会静默拿到。IPC 消息带每 webview 的随机 nonce 且只转发真实用户事件,页面伪造不了焦点/缩放。

## 7. 一期范围与验收

**一期目标:** 在 Dozer 里装一个静态 Web 应用(Excalidraw),它在 rail 上有自己的图标和面板,由 dozerd 里的 gateway 提供,GUI 退出重开后数据还在。Python/Node/容器只定义 adapter trait 与 `probe`,不实现。

**前置验证(进入对应切片前做完):** V1(单端口多应用 Host 路由)、V2(真实 Excalidraw 静态构建端到端)在切片 B 之前;V3(Host 校验 + token 的攻击面)在切片 B 内;V4/V5 不阻塞一期。

**切片(每片单独可合并、可测;顺序有依赖):**

| 片 | 内容 | 落在 | 依赖 |
|---|---|---|---|
| **A0** | 新建 `bytehost-apps` crate(仅类型与纯逻辑;摘要计算在 `digest` feature 下):manifest 解析与校验(`deny_unknown_fields`、`min_host_version`)、摘要、权限/授权/`InstallPlan`/`ApprovedInstallPlan`、`AppState`(desired/observed)与对账纯函数、`AppEvent`、registry/storage 的文件读写;全部有单测;门禁 `cargo tree -p bytehost-apps` 不含 `dozer*` | 新 crate | 无 **已完成(A0,`bytehost-a0`):`6ebec5fe`**——manifest 用 TOML(用户 2026-10-04 裁决,`toml` 0.8,在 `manifest-toml` feature 下);默认 feature 为空、依赖仅 serde/serde_json;门禁 `scripts/check-bytehost-apps-deps.sh` |
| **A1** | `server` feature:`AppManager`、`static_web` runtime、gateway(Host 校验、静态文件、固定端口);runtime adapter trait + 各 runtime 的 `probe`(docker/colima、node、uv 的分层探测) | 新 crate | A0、V1 **已完成(A1,`bytehost-a1`):`283ff056`**——`server` feature:`AppManager`、`static_web`、gateway(Host 校验 + 会话令牌 + 路径解析 + CSP)、端口持久化、docker/node/python 分层探测;V1 已实测通过 |
| **A2** | 接入 dozerd:`dozer-core::protocol` 加 `Request::App`/`Reply::App`/事件,`dozerd/server.rs` 转给 `AppManager`;`dozer-client` 加 `app_*` 方法;dozerd 启动对账、优雅退出停应用、孤儿清理 | dozer-core、dozerd、dozer-client | A1 **已完成(A2,`bytehost-a2`):`0fcc66b6`**——线上类型在 `bytehost-apps::proto`;`dozer-core::protocol` 加 `Request::App`/`Reply::App`;dozerd 的 `AppService`(启动永不失败、启动对账、`Shutdown` 时收尾且保留 `desired`);`dozer-client` 的 `app_*` 方法;依赖门禁新增 `dozer-hook` 闭包检查。**推送事件**(GUI 订阅状态变化)本切片没做,A4 之前 GUI 靠轮询 `List`;启动时的"首次绑定成功后才持久化端口"已在 A4a 修好 |
| **A3** | rail 动态条目最小版(H7b-min):条目 id 能表达 `app:<id>`、布局序列化向后兼容、按应用 id 存独立 WebView 状态 | dozer-app | 无(可与 A0–A2 并行)。**rail 条目已落地(A3,`2026-10-05-bytehost-a3-rail-app-entries.md`):`PanelKind::App(AppSlot)` + `app:<id>` 落盘 + `RailLayout::sync_apps`;`App::sync_installed_apps` 待 A4 接线。按应用 id 存独立 WebView 状态属 A4** |
| **A4** | GUI:应用面板(wry,加载 `http://<app-id>.localhost:端口/`,每应用 `data_store_identifier`)、安装计划/审批的最小界面、不可用时的提示页(§6.3,由 host 提供)、Settings 里的运行时探测展示 | dozer-app | A2、A3 **必须同时实现"禁止离开本 origin 的顶层导航/`window.open`"的 WebView 策略**——这是静态应用出站网络的强制等级能从 `Advisory` 升为 `Enforced` 的前提(CSP 挡不住导航与 WebRTC,见 A1 评审)。**A4 拆为 A4a(后端:类别化失败 + 首次端口落盘修正,已完成)/A4b(GUI 面板与导航策略)/A4c(安装审批界面 + Settings 探测),见 `plans/2026-10-05-bytehost-a4a-failure-kinds-and-port.md`**。**A4b 再拆为 A4b1(应用 webview 机制,已完成:`plans/2026-10-05-bytehost-a4b1-app-webview-mechanism.md`——`app_webview.rs` 的 origin 策略/id 段/每应用 `data_store_identifier`/独立受限构建路径 `build_app_webview`,以及几何、`preview_desired`、焦点路由接线;**URL 由谁供给属 A4b2**)与 A4b2(接线实际 URL + host 提示页)**。**A4b2 已完成:宿主逻辑(列表轮询 / 每应用面板状态机 / 启停),见 `plans/2026-10-05-bytehost-a4b2-app-host-logic.md`。**A4c 已完成:设置「应用」页(运行时探测 / 安装审批 / 停止 / 卸载),纯状态机在 `extensions/settings_apps.rs`,见 `plans/2026-10-05-bytehost-a4c-settings-apps-page.md`;**A4 全部完成**(Task 3 的手工 GUI 验收待人工执行)——下一步 A5(Excalidraw 端到端验收)** |
| **A5** | Excalidraw 端到端验收(下面的验收 1–8) | 全部 | A4、V2 **代码与自动验证已完成(`plans/2026-10-05-bytehost-a5-excalidraw-acceptance.md`):打包配方、V2 探针、卸载"含数据"清 WebView 存储;验收 8 一期重新表述为"非 static_web 在安装时被明确拒绝"。真实 GUI 手工验收尚未执行,清单见 `specs/2026-10-05-bytehost-a5-acceptance-report.md` §3——全部有结论前不得标完成** |
| **A6(二期)** | 进程型运行时(Python/Node),切分为 A6a 监管核心(`process` 模块:白名单环境、独立进程组、有上限的日志、健康探测、重启退避、孤儿清理,**已完成,未接线**)/A6b gateway 反向代理(含 WebSocket、SSE)/A6c 接入 AppManager+dozerd+GUI/A6d Settings 运行时安装/A6e 进程应用体验(清单版本要求严格校验、运行时缺失/版本不符问题页、日志查看、运行中周期健康检查、真实 Python/Node 示例端到端),见 `plans/2026-10-07-bytehost-a6a-process-supervisor.md` 与 `plans/2026-10-08-bytehost-a6e-process-app-experience.md`(**A6e 代码与自动端到端验证已完成,验收报告 `specs/2026-10-08-bytehost-a6e-acceptance-report.md`,真实 GUI 手工项待人工执行**) | bytehost-apps(A6a)| A5 |

**验收:**
1. Excalidraw 静态构建:`install_plan` 展示摘要/来源/强制等级 → 审批 → 安装 → rail 出现图标 → 打开 → 画一笔 → 退出 **GUI** 重开 → 画还在;
2. 篡改源码后用旧的 `ApprovedInstallPlan` 安装被拒(摘要不符);
3. 卸载"仅程序"保留数据,重装后画还在;卸载"含数据"后清空;
4. 第二个应用(任意静态页)与 Excalidraw 的 localStorage/Cookie 互不可见;
5. 对一个伪造 `Host: evil.example` 的请求,gateway 返回拒绝;
6. manifest 里出现未知权限字段时安装计划失败(`deny_unknown_fields`);
7. **GUI 退出后 gateway 仍在服务**(用外部 `curl` 带正确 Host 能取到 `/`);**dozerd 停止后**该端口不再服务、应用状态变为 `Stopped`;重启 dozerd 后按 `desired_state` 自动恢复;
8. 一个 runtime 不可用的应用(例如声明了 `kind: container` 而 Colima 未启动)打开后,其面板里出现提示页,不会静默降级。

## 8. 与其他计划的关系

- **Digger:** `../digger` 目前只有文档。Digger 启动后是 bytehost 的第二个消费者,但它不能把 bytehost 当 crate 依赖(host 仍在 `dozer-app`)。应用宿主按模块边界在 bytehost 内部演进,Digger 的真实差异出现后再抽象。【群聊已定:"等 Digger 接入后,根据真实差异再抽象"】
- **PlantUML/UML 面板:** 若走"应用宿主",PlantUML server 是 Java 服务,属于进程/容器 runtime,晚于一期;短期更现实的做法是作为 Preview 的 `.puml` 渲染类型(只渲染、不编辑,符合核心原则)。**待用户裁决形态。**

## 9. 待裁决清单

| # | 问题 | 本文倾向 |
|---|---|---|
| A1 | gateway 方案 | **已定(2026-10-04,用户):`<app-id>.localhost` + 单一固定端口 + Host 头路由 + 每应用 `data_store_identifier`;自定义协议不作应用 gateway**(§5.2) |
| A2 | 进程所有权 | **已定(2026-10-04,用户):跟随 dozerd,dozerd 停止则应用停止**(§6.1) |
| A3 | 一期切片是否需要 rail 动态条目最小版(H7b-min) | **需要**(A6 选 (a) 已定);作为本规格的前置小计划,要点:rail/布局的条目 id 能表达 `app:<id>`、按应用 id 存独立 WebView 状态、落盘兼容 |
| A4 | 规格存放位置 | 暂放 dozer 的 `docs/superpowers/specs/`(与其他 bytehost 文档同处);`bytehost-apps` 建在 dozer 仓库的 `crates/` 下,将来随 bytehost 一起拆出时再迁移 |
| A6 | 应用的"独立入口"是什么形态 | **已定(2026-10-04,用户):(a) rail 图标 → 该应用自己的面板**;(b) 独立窗口留作以后的可选能力 |
| A7 | gateway 放在哪个进程 | **已定(2026-10-04,用户):放进 dozerd**,与应用同生命周期;GUI 里的 wry 只是客户端 |
| A8 | Digger 的 supervisor 怎么实现 | bytehost 只定义接口/协议;Digger 自带守护进程或进程内嵌入。**待 Digger 启动时裁决** |
| A9 | Settings 里运行时安装的范围 | **已定(2026-10-04,用户):可装 uv/Python、Node(固定版本+校验和,装到 bytehost 自己的目录,显式确认);Docker/Colima 只探测与指引**;属二期,一期 Settings 只展示探测结果。**已落地(A6d,2026-10-07)** |
| A12 | gateway 固定端口策略 | **已定(2026-10-04,用户):首次启动从高端口段随机选一个并持久化;被占用显式报错,不静默换**(§5.2 第 6 条) |
| A10 | 新 crate 的名字与位置 | **已定(2026-10-04,用户):`crates/bytehost-apps`**(无界面;与将来可能的 iced 侧 `bytehost` 区分) |
| A11 | `server` feature 的 HTTP 栈 | **已确认(2026-10-04,用户):`hyper` 1.x + `hyper-util` + `http-body-util`,WebSocket 复用 `tokio-tungstenite`**(新增依赖只进 dozerd) |
| A5 | UML 面板的形态 | Preview 的 `.puml` 类型,而非独立面板/应用 |
