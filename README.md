# bytehost

应用宿主平台库:把「装应用、跑应用、给应用一个隔离的 webview 站点」这套能力从具体产品里抽出来,供多个宿主共用。

**agent 中立、界面框架中立、平台 webview 库中立。** 不依赖任何产品 crate、`iced`、`wry`、`tauri`、`objc2`——由 `scripts/check-deps.sh` 门禁保证。

## 四个 crate

| crate | 一句话 | 依赖 |
|-------|--------|------|
| `bytehost-apps` | 应用宿主的无界面部分:应用模型、安装计划、生命周期状态、注册表、来源(本机目录/压缩包/URL)、受管运行时、进程监管、本机 gateway。 | 默认 feature 仅 serde 家族;`server` 才带 tokio/hyper/… |
| `bytehost-client` | 与传输无关的客户端 API:`AppHostApi` trait、统一失败类型、进程内实现 `InProcess`、以及「两种实现都必须通过」的契约测试 `conformance`。 | `bytehost-apps` |
| `bytehost-webview` | 应用 webview 的**纯安全策略**:origin 导航白名单(`AppOrigin`)、每应用存储标识(`data_store_identifier`)、IPC nonce 与注入脚本(`AppIpc`)、数据存储清除排队。不建 webview,只做决策。 | `bytehost-apps`、`url`、`uuid` |
| `bytehost-panel` | 应用面板 / 安装审批流程 / 日志查看器的**状态机**:与界面框架无关,以 `AppKey` 泛型标识应用,失败走 `Notice`(不弹具体 UI)。 | `bytehost-apps`、`bytehost-client` |

## 谁是消费者

- **Dozer**(wry + iced 宿主):已接入。`dozerd` 打开 `bytehost-apps/server` 跑 gateway 与监管;`dozer-app` 用 `bytehost-client`(UDS)+ `bytehost-panel` + `bytehost-webview`。
- **Digger**(Tauri 宿主,无守护进程):嵌入待做。用 `bytehost-client/in-process` 的 `InProcess` 直接在进程内驱动 `AppService`,`bytehost-webview` 的纯策略由 Tauri 侧建子 webview 时调用。

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
- **tag 即发布**:消费方用 git tag 引用(`bytehost-* = { git = "…/bytehost", tag = "vX.Y.Z" }`),同一仓库多个 crate 用同一个 tag。
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

样例应用在 `scripts/samples/`(`py-notes`、`node-notes`);Excalidraw 打包配方在 `scripts/excalidraw/`;受管运行时版本表由 `scripts/pin-runtimes.sh` 从官方源生成(不得手写哈希)。

## 文档

- 设计规格:`docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`
- 分片验收报告:`docs/superpowers/specs/*bytehost-a*-acceptance-report.md`
