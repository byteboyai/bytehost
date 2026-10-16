# bytehost

应用宿主平台库:把「装应用、跑应用、给应用一个隔离的 webview 站点」这套能力从具体产品里抽出来,供多个宿主共用。

**agent 中立、界面框架中立、平台 webview 库中立。** 不依赖任何产品 crate、`iced`、`wry`、`tauri`、`objc2`——由 `scripts/check-deps.sh` 门禁保证。注意:这条只对下表前四个(应用宿主无界面部分)成立;`bytehost-desktop`、`bytehost-chrome` 是桌面宿主层,**允许**依赖 `iced`/`wry`/`byteui`,但仍不含任何产品业务 crate(`check-deps.sh` 按组分别校验)。

## 七个 crate

| crate | 一句话 | 依赖 |
|-------|--------|------|
| `bytehost-apps` | 应用宿主的无界面部分:应用模型、安装计划、生命周期状态、注册表、来源(本机目录/压缩包/URL)、受管运行时、进程监管、本机 gateway。 | 默认 feature 仅 serde 家族;`server` 才带 tokio/hyper/… |
| `bytehost-client` | 与传输无关的客户端 API:`AppHostApi` trait、统一失败类型、进程内实现 `InProcess`、以及「两种实现都必须通过」的契约测试 `conformance`。 | `bytehost-apps` |
| `bytehost-webview` | 应用 webview 的**纯安全策略**:origin 导航白名单(`AppOrigin`)、每应用存储标识(`data_store_identifier`)、IPC nonce 与注入脚本(`AppIpc`)、数据存储清除排队。不建 webview,只做决策。 | `bytehost-apps`、`url`、`uuid` |
| `bytehost-panel` | 应用面板 / 安装审批流程 / 日志查看器的**状态机**:与界面框架无关,以 `AppKey` 泛型标识应用,失败走 `Notice`(不弹具体 UI)。 | `bytehost-apps`、`bytehost-client` |
| `bytehost-core` | 桌面宿主的**纯契约层**:稳定身份(`PanelId` 等)、注册元数据、布局 DTO;产品组合根把它们映射到自己的业务枚举,Host 不解释业务含义。 | 仅 serde |
| `bytehost-desktop` | 共享桌面宿主:iced 窗口壳 + wry 子视图执行 + 产品泛型回调(Surface 生命周期、几何、浮层)。 | `bytehost-core`、`bytehost-webview`;允许 iced/wry |
| `bytehost-chrome` | 共享窗口骨架:顶栏、左右图标栏与面板区、分隔线、放大态、布局持久化;以 `PanelId` 为键,不理解具体面板。 | `bytehost-core`、`bytehost-desktop`、`byteui` |

## 谁是消费者

- **Dozer**(wry + iced 宿主):已接入。`dozerd` 打开 `bytehost-apps/server` 跑 gateway 与监管;`dozer-app` 用 `bytehost-client`(UDS)+ `bytehost-panel` + `bytehost-webview`。
- **Digger**(iced + wry 宿主,无守护进程):**进程内嵌入已实现**(2026-10-12,已随 tag 发布,`v0.2.0` 起)。用 `bytehost-client/in-process` 的 `InProcess` 直接在进程内驱动 `AppService`,`bytehost-webview` 的纯策略由 Digger 侧建子 webview 时调用。注意:Digger **不是** Tauri 宿主——Tauri 方案已撤回(见 Digger `docs/adr/ADR-001`、`ADR-011`),与 Dozer 同用 iced 0.14 + `byteui` + `wry` 子 webview。

## 独立嵌入(没有守护进程)

```rust
use bytehost_apps::gateway::GatewayConfig;
use bytehost_apps::service::AppService;
use bytehost_client::in_process::InProcess;

let svc = AppService::start_with(root, GatewayConfig { port: 0 }).await;
let host = InProcess::new(svc.clone());
// 出计划 → 批准 → 安装 → 启动,全程走 AppHostApi(与 UDS 实现同一套类型化方法)。
```

完整可运行示例见 `crates/bytehost-client/examples/embed.rs`。端到端验收(装并启动 `scripts/samples/py-notes`,经 gateway 带 Cookie 得 200、不带得 403、`shutdown()` 后端口关闭)见 `crates/bytehost-client/tests/embedded_host.rs`(需要真 `python3`,默认 `#[ignore]`)。

## 兼容性承诺

- **wire 协议只追加**:`AppRequest`/`AppReply`/`AppEvent` 的 JSON 形状只增字段、不改有字段;新字段解析方给默认值。
- **落盘格式带版本**:应用记录 `RECORD_FORMAT_VERSION`、应用目录布局 `apps/<id>/package/<version>`、`data/`、`logs/`、受管运行时目录、端口持久化文件均按版本演进。
- **tag 即发布**:消费方用 git tag 引用(`bytehost-* = { git = "…/bytehost", tag = "vX.Y.Z" }`),同一仓库多个 crate 用同一个 tag。各 crate 的 `Cargo.toml` 里 `version` 固定为 `0.1.0`、不随 tag 变化;**以 git tag 为准**,tag 版本与宿主能力版本(`HOST_VERSION`)也是两回事,不要用 crate 版本号判断依赖是否过期。
- **每应用数据存储标识算法不改**:`data_store_identifier(app_id)` 的输出一旦改动即破坏用户的 webview 数据归属。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
bash scripts/check-deps.sh
# 需要真 python3 / node 的进程型端到端:
cargo test -p bytehost-apps --all-features --test process_apps_live -- --ignored
cargo test -p bytehost-client --all-features --test embedded_host -- --ignored
```

样例应用在 `scripts/samples/`(`py-notes`、`node-notes`);i18n 样例在 `scripts/samples/i18n/`(`bilingual-live` 双语热切换 + 输入框/滚动区证明不丢状态、`partial-initial` 部分翻译回落 default、`legacy-sample` 无 i18n 字段走旧路径);`node-notes` 已就地加 `[i18n]` 复用语言桥,证明桥与运行时无关(static_web / Node HTTP 共用同一策略)。i18n 样例校验见 `crates/bytehost-apps/tests/i18n_samples.rs`(`cargo test -p bytehost-apps --features server --test i18n_samples`)。Excalidraw 打包配方在 `scripts/excalidraw/`;受管运行时版本表由 `scripts/pin-runtimes.sh` 从官方源生成(不得手写哈希)。

## 文档

- 设计规格:`docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`
- 分片验收报告:`docs/superpowers/specs/*bytehost-a*-acceptance-report.md`
