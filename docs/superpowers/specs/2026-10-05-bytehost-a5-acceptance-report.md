# bytehost A5 验收报告(Excalidraw 端到端)

> 状态:**部分完成**。V2 与全部"能自动验证"的项已验证(下表给出命令与结果);**需要真实 GUI 的手工项尚未执行**——
> 执行会话无法操作 Dozer 窗口,这些项写成了待办清单,**不得据此把 A5 标成"已完成"**。
> 对应计划:`docs/superpowers/plans/2026-10-05-bytehost-a5-excalidraw-acceptance.md`。

## 1. V2(真实 Excalidraw 静态构建)——已验证

- 对象:`excalidraw/excalidraw:latest`,摘要 `sha256:f7ee194addd607bf831d2af0f0a34463dd4225e426cf35199ef0b12a803398e9`(2026-10-05 拉取)。
- 工具:`scripts/bytehost/excalidraw/package.sh`(打包)→ `scripts/bytehost/excalidraw/verify.py`(真实 gateway + 受限 wry webview)。
- 结果:

| 构建 | `verify.py` 退出码 | 说明 |
|---|---|---|
| 官方构建原样 | **1(失败)** | 内联脚本被 `script-src` 拦截;`EXCALIDRAW_ASSET_PATH` 未设;Excalifont/Virgil/Nunito/Assistant 等全部加载失败(`NetworkError`) |
| 打包后 | **0(`V2 OK`)** | 无脚本违规;资源路径 `["/"]`;画布出现;Excalifont、Virgil、Cascadia、Nunito、Comic Shanns、Assistant、Lilita One、Liberation Sans 均 `loaded`;`window.open` 被拒;Service Worker 已注册;localStorage 可写 |

- 已知噪音:`esm.sh` 字体回退源的 CSP 违规(字体已从本站加载)。
- **探针结论不了的两项**(wry 里拿不到用户手势):剪贴板读写、blob 下载——见 §3 手工清单。

## 2. 验收 1–8:自动化已验证的部分

| # | 已验证 | 证据 |
|---|---|---|
| 1 | 持久化机制:固定端口 + 固定存储标识时数据跨进程保留 | `spike/v2-excalidraw` 连跑:第一次 `lsAtStart=[]` → 第二次含 `excalidraw`、`excalidraw-state` 等键 |
| 2 | 篡改源码/清单后用旧批准安装被拒 | `manager::a_source_or_manifest_changed_after_approval_is_refused_and_leaves_nothing_behind`、`plan::verify_rejects_a_payload_edited_after_the_digests_were_taken` 通过 |
| 3 | 目录层:"仅程序"保留 `data/` 与日志;WebView 存储层:**清除机制**可用 | `manager::uninstalling_the_program_stops_serving_and_keeps_user_data`、`registry::uninstalling_the_program_keeps_data_and_logs_and_a_reinstall_finds_them` 通过;探针:`--purge --store N` 后 `lsAtStart` 回到 `[]`;队列逻辑 `app_webview::store_removal_*` 通过 |
| 4 | 不同应用 origin 隔离 | V1 spike(`spike/origin-gateway/README.md`,2026-10-04) |
| 5 | 伪造 `Host` 被拒 | `gateway::the_host_header_must_be_exactly_a_valid_app_dot_localhost_with_the_gateways_port` 通过 |
| 6 | 未知权限字段 → 计划失败 | `manifest::…::unknown_fields_are_a_parse_error_at_the_top_level_and_in_nested_tables` 通过;随包清单 `the_shipped_excalidraw_manifest_parses_and_asks_for_nothing` 通过 |
| 7 | dozerd 停止后应用 `Stopped`、重启按 `desired` 恢复;重启后沿用持久化端口 | `app_requests::a_shutdown_request_takes_the_apps_down_but_keeps_what_the_user_wanted`、`app_service::apps_wanted_running_come_back_after_a_dozerd_restart_and_stopped_ones_stay_stopped`、`start_uses_the_persisted_port_across_restarts` 通过 |
| 8(重新表述) | 声明非 `static_web` 的应用在安装计划阶段被明确拒绝 | `manager::only_static_web_is_supported_in_phase_one` 通过 |

## 3. 待人工在真实 GUI 里完成(**未执行**)

前置:`sh scripts/bytehost/excalidraw/package.sh ~/excalidraw-app`;先完成 A4b1 冒烟、A4b2 的 7 步、A4c 的 8 步(各自计划的"手工验收"一节)。

- [ ] 1:设置→应用→安装应用…→选 `~/excalidraw-app`;审批卡内容正确(来源"本地目录"、运行方式 `static_web`、权限全最严);批准并安装→图标栏出现→启动→画几笔→**退出整个 Dozer**→重开→进该应用:画还在(并确认 `gateway.json` 端口未变)。
- [ ] 2(GUI 侧):审批卡停留时改 `web/index.html` 再批准 → 流程内显示"应用源码在审批之后发生了变化"。
- [ ] 3:画一笔→卸载「保留数据」→重装→画还在;再画→卸载「连数据一起删」→重装→空白(**验证 Task 3 的 App 层接线**:设置窗口确认后立刻关闭也应清除)。
- [ ] 4:再装一个写 `localStorage.x=1` 的静态页,两个应用互相读不到。
- [ ] 5:`curl -i -H 'Host: evil.example' http://127.0.0.1:<端口>/` → 4xx。
- [ ] 6(GUI 侧):在 `manifest.toml` 里加 `[permissions] telepathy = "allow"` → 安装流程内红字失败。
- [ ] 7:运行中退出 Dozer → `curl -i -H 'Host: excalidraw.localhost:<端口>' http://127.0.0.1:<端口>/` 仍有响应(端口在服务);停 dozerd 后连接失败;重启 dozerd 后应用自动恢复。
- [ ] 8(GUI 侧):把清单 `[runtime]` 改成 `kind = "container"` → 安装流程内显示"暂不支持 container 类型的应用"。
- [ ] V2 手工补项:① `⌘C`/`⌘V` 复制粘贴图形;② 导出 PNG/保存(预期不可用,A4b1 拒绝下载);③ 切换主题、画英文字(手写体字形);④ 画中文(Xiaolai 按需加载)。

## 4. 发现的缺陷与已知局限

- (无新增缺陷)Task 3 的 App 层接线(意图记录、结果触发清除、窗口层调用 `remove_data_store`)没有 App 夹具可自动验证,依赖 §3 第 3 项手工确认。
- Excalidraw 的导出/保存在一期不可用(A4b1 拒绝所有下载)。
- 打包阶段联网下载 3 个字体,未校验摘要;镜像用 `latest`,以本报告记录的摘要为准。
- 清除 WebView 存储需要 macOS 14+。
