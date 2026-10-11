# desktop-contract spike（Task 02 探针）

一次性验证工程，证明 iced 0.14 × wry 0.55.1 桌面契约可落地（规格 §11 闸门 1）。
结论记在 `dozer/docs/dozer-v2/digger-bytehost-integration/api-decisions.md`。

**独立 `[workspace]`**：bytehost 的依赖门禁（`scripts/check-deps.sh`）禁止工作区内
出现 iced/wry，故放在工作区外，与 `digger/spike/editor-host` 同惯例。随时可删。

## 证据

| 证据 | 位置 | 命令 |
|---|---|---|
| 两产品消息类型共用同一 `Product<M>` / `HostMessage<M>` | `src/lib.rs` `two_products_share_one_host_interface` | `cargo test --lib` |
| 同一 Surface 描述表达 Usage 与 CodeHealth 两类推送 | `src/lib.rs` `one_surface_describes_usage_and_codehealth` | 同上 |
| 几何纯函数（webview 下推顶部条） | `src/lib.rs` `host_rect_pushes_down_top_insets` | 同上 |
| 真实窗口入口（iced 循环 + wry `build_as_child` + thread_local 句柄） | `src/main.rs` | `SELFTEST=1 cargo run`（退出 0） |

## 命令

```bash
cargo test --lib            # 3 个证据测试
SELFTEST=1 cargo run        # 开窗后立即退出，验证入口能启动
cargo run                   # 开真实窗口观察 webview 叠在 iced 之上
```

## 锁定版本

`iced = "0.14"`（feature `tokio`）、`wry = "0.55.1"`，与 Dozer `Cargo.lock`
（`iced_wgpu 0.14.0` / `iced_winit 0.14.1` / `wry 0.55.1`）及 byteui 0.4.1 一致。
