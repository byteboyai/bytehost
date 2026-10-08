# bytehost A6h 验收报告(非本机来源:压缩包 / https URL)

> 状态:**部分完成**。§1 的全部自动化项已实跑通过并给出逐条证据;**需要真实 GUI 的项(§2)尚未执行**——
> 执行会话无法操作 Dozer 窗口,这些项写成了待办清单,**不得据此把 A6h 标成"已完成"**。
> 对应计划:`docs/superpowers/plans/2026-10-08-bytehost-a6h-archive-and-url-sources.md`。

## 0. 环境与对象

- 平台:macOS 26.6.2(Darwin arm64,`sw_vers` = 26.6.2 / 25G83)。
- 运行时:`python3` = Python 3.13.1;`node` = v24.14.0(均为系统解析)。
- 打包工具(仅测试用它造输入):`/usr/bin/tar`(bsdtar)、`/usr/bin/zip`。
- 任务提交:Task 1 `ecc8cce0` / `1ea5c7bd`、Task 2 `982083d4`、Task 3 `6f137743`、Task 4 `ba16457c`、
  review 修复 `4ec6f2b4`、Task 5 `fd9b6dac`;Task 6(本报告)随提交附上。
- 本切片新增/改动的关键文件:
  - `crates/bytehost-apps/src/archive.rs`:`ArchiveKind`/`Limits`/`extract`,**进程内**逐条目校验解压
    (拒绝绝对路径、`.`/`..`/空段/反斜杠/NUL 路径、符号/硬链接与设备/FIFO、重复与大小写折叠重复,
    单文件/总量/条目数/深度/压缩比上限;超限即中止并清理)。压缩包字节可选(默认 feature 不含
    `zip`/`flate2`/`tar`,只 `server` feature 打开)。
  - `crates/bytehost-apps/src/source.rs`:`policy_for`/`SourcePolicy`/`effective`,**服务端按来源推导信任**
    (本机目录/压缩包 = `Local/Trusted`,URL = `ThirdParty/Untrusted`);`validate_url`(仅 https、拒凭据)、
    `normalize_sha256`;`stage_source`(目录/压缩包/URL 三态落地成含 `manifest.toml` 的目录)。
  - `crates/bytehost-apps/src/proto.rs`:`AppSource::{Archive, Url}`、`SourceInfo`(wire 只追加)。
  - `crates/bytehost-apps/src/manager.rs`:URL 出计划时下载并缓存到 `<root>/downloads/<sha256>.bin`,
    安装阶段复用缓存并**重算**校验;`InstallPlan.source_info` 参与 `verify`;`SourceNotAllowed` 等错误类别。
  - `crates/dozerd/src/app_service.rs`、`crates/dozer-client`、`crates/dozer-core::protocol`:贯通新来源。
  - `crates/dozer-app/src/extensions/settings_apps.rs`:来源切换(Dir/Archive/Url)、URL/sha256 输入、
    审批卡"来源"披露区块;`platform/window_events.rs`:原生选目录/选文件对话框按当前来源分派。

## 1. 自动化结果——已验证

### 1.1 端到端 live 回归(Task 6 Step 1)

命令:

```
cargo test -p dozerd --test process_apps_live -- --ignored --nocapture
```

- 并行跑:每轮 `7 passed`(偶发 1 例既有相关并行掉队,见 §3);**单线程连跑 3 遍稳定**:
  `19.69s / 19.70s / 19.72s`(`--test-threads=1`,8.58s→19.7s 的差别来自用例串行而非新增用例)。
- 本切片新增 3 个 `#[ignore]` 用例:

| 用例 | 覆盖 | 实测 |
|---|---|---|
| `archive_sourced_python_app_end_to_end` | 真 python 样例**现场**打 zip 与 tar.gz(临时目录,不提交二进制)→ `Plan(Archive)`→`Install`→`Start`,验证 A6e 里与来源无关的核心几步 | 通过(zip 与 tar.gz 两形态各一遍) |
| `hostile_archives_are_refused_with_no_files_outside_the_root` | 恶意压缩包(zip-slip `../evil.txt`、符号链接)经 `AppService` 被拒,且**数据根目录之外没有新文件** | 通过(对测试根父目录前后快照比较) |
| `url_sourced_static_app_runs_and_a_process_app_is_rejected` | 假 `Fetcher` 注入静态应用 zip → Plan→Install→Start→经 gateway 取到页面;同一假 fetcher 供 Node 应用 → 出计划即被拒 | 通过 |

1 号用例逐步(经真实 gateway + 真 python 子进程):

1. 打成 `py-notes.zip` / `py-notes.tar.gz`(单一顶层目录形态)→ `Plan` 出带 `source_info.kind == "archive"`
   的计划 → `Install` → `Start` → `Running`。
2. 令牌门:带会话 Cookie `GET /` = 200;无 Cookie = 403,且计数不变。
3. SSE:1.5s 内收到 ≥3 条 tick。
4. 计数持久化:同源 `POST /hit` → `counter.txt` = 1;`GET /crash` → 监管重启换新 pid(`/pid` 为纯数字,
   且与旧值不同)→ 计数仍为 1。
5. `Uninstall(ProgramAndData)` 后应用目录消失。

2 号用例:

- **先证明**打的包确实含越界条目(`tar -tf` 输出含 `..`),避免"造包失败导致测试假通过"。
- `../evil.txt`(zip-slip)与指向 `/etc/passwd` 的符号链接两个包分别 `Plan` → 都被 `Rejected`。
- 两次拒绝后,根的父目录与运行前快照**逐条一致**(无 `evil.txt`、无残留 staging),证明解压失败清理干净、
  没有越界写入。

3 号用例:

- 静态应用 zip 经 URL 来源:出计划**下载一次**(假 fetcher `calls == 1`)→ 安装 → 启动 → `GET /` 正文含
  `hello-from-url`;`Uninstall(ProgramAndData)` 后目录消失。
- 进程型(Node)应用 zip 经 URL 来源:`Plan` 直接 `Rejected`,报文含"静态应用"。

### 1.2 组件/单元测试

| 命令 | 结果 |
|---|---|
| `cargo test -p bytehost-apps`(默认 feature) | **85 passed, 0 failed** |
| `cargo test -p bytehost-apps --all-features` | **380 passed, 0 failed**(含 Task 1/2/3 新增的压缩包/URL 用例) |
| `cargo test -p dozerd`(lib + 集成) | lib **492 passed, 0 failed, 1 ignored**;集成:除已知无关偶发外全绿(见 §3) |
| `cargo test -p dozer-client` | 全部 `ok`(含 URL/压缩包贯通用例) |
| `cargo test -p dozer-app settings_apps` | **47 passed** |
| `cargo test -p dozer-app`(全量) | **1982 passed, 1 failed**(唯一失败是已知无关的 `files::tests::delete_confirm_spec_reflects_pending_target`,见 §3) |

### 1.3 门禁 / 格式 / lint

| 检查 | 结果 |
|---|---|
| `scripts/check-bytehost-apps-deps.sh` | ok |
| `scripts/check-log-scope.sh` | ok |
| `cargo fmt --check` | ok |
| `cargo clippy -p dozerd --all-targets` | A6h 触碰的文件无新警告(唯一 warning 是既有的 `dozerd/src/preview_commands.rs:152` `let_underscore_future`,非本切片) |
| 默认 feature `cargo tree -p bytehost-apps` | 不含 `zip`/`flate2`/`tar` |

## 2. 待人工在真实 GUI 里完成(**未执行**)

> 执行会话无法操作 Dozer 窗口。以下各项**不得**据此标成完成。对应计划 Task 5 Step 5。
> 其中第 3 项含**一次对真实 https 服务器的下载**——自动化里只有假 `Fetcher`,这一项只能手工。

- [ ] 选本机目录安装仍正常(回归)。
- [ ] 选 `py-notes` 打成的 zip 安装并启动。
- [ ] 填一个真实 https 的静态应用 zip(例如自己托管的 Excalidraw 打包产物)→ 审批卡显示来源、哈希、
      "来自网络,不可信"标注 → 批准安装 → 能打开。
- [ ] 同一个 URL 填错 sha256 → 内联拒绝(不弹 Toast,保留已填 URL 与 sha256)。
- [ ] 填一个 Node 应用的 URL → 内联"网络来源只能安装静态应用"。
- [ ] 输入框里中文输入法(IME)与粘贴正常。

## 3. 发现的缺陷与已知局限

1. **既有无关的偶发/确定失败**(计划"全量"命令里已列,逐个注明,均未顺手改):
   - `dozerd`:`memory::tests::list_orders_by_updated_ms_desc`、`app_service::tests::a_python_app_runs_through_the_wire_and_dies_with_dozerd`
     全量并行时偶发失败,单跑通过。
   - `dozerd --test process_apps_live` 并行跑偶发 1 例掉队(`python_sample_end_to_end` 在 `List` 处
     因并行端口争用),单线程连跑 3 遍全绿。
   - `dozer-app`:`extensions::files::tests::delete_confirm_spec_reflects_pending_target`(clean tree 亦失败);
     偶发 `assets::tests::serves_vendored_asset_with_mime`。
   - `bytehost-apps`:`manager::tests::a_python_app_installs_starts_runs_behind_the_gateway_and_stops` 既有偶发。

2. **计划文本与实现的一处偏差(手段而非目标,已改为等效实现)**:计划 Task 6 Step 1 第 2 条要求用
   `walkdir` 对测试根父目录前后快照比较。本仓库没有任何 crate 依赖 `walkdir`,新增依赖会改 `Cargo.lock`
   (联调 patch 会污染 lock 的 `source` 行,且本切片约定不提交 lock)。改用 `process_apps_live.rs` 内的
   一个本地递归 `snapshot_tree`(相对路径→类型的有序集合),语义等价(能发现越界新建的文件/目录/符号链接),
   也无需新依赖。**这是等价替换,不是遗漏。**

3. **已知局限(与计划"已知局限"一节一致,非缺陷;此处补两条执行中实测到的)**:
   - **不拦截本机/内网地址的 URL**(`https://localhost/…`、`https://10.x/…`):内网分发与本机测试是合法场景。
   - **网络来源的静态应用仍是 `Advisory` 级别的出站网络**(CSP 挡不住 WebRTC 等)。
   - **本机压缩包享受 `Trusted`**:用户手动选中一个从网上下载的压缩包,与选目录一样受信。
   - **没有签名校验**:sha256 只防"字节被换",不证明发布者身份。
   - **没有 Git 仓库来源**;**网络来源放开进程型应用**要等沙箱。
   - **缓存只保留 1 小时**,计划与安装间隔更久会重新下载并要求 sha256 一致。
   - **只支持 `.zip`、`.tar.gz`、`.tgz`**;`.tar.xz`/`.7z`/`.rar` 不支持。
   - **实测补充(1):含 `.` 段的归档条目被拒**。解压器按设计拒绝 `./`、`./file` 这类含 `.` 段的路径
     (`archive.rs::safe_segments`)。因此**用 `tar czf app.tar.gz -C app .` 打出来的、条目形如 `./manifest.toml`
     的常见归档会被拒**,需改打成**单一顶层目录**形态(`tar czf app.tar.gz -C parent app`,条目形如
     `app/manifest.toml`)或"根目录直接放 `manifest.toml`"形态。Task 6 的 live 用例即采用顶层目录形态。
     这是一个**可用性上的已知限制**(偏严),不是越权/越界,故未在本切片放宽——若要支持 `./` 形态,
     应在 `safe_segments` 里先剥掉开头的 `./` 段,属后续 A6h 跟进或用户裁决事项。
   - **实测补充(2):macOS bsdtar 的 AppleDouble(`._*`)伴随文件会破坏"唯一顶层目录"判定**。当源目录带
     扩展属性(如从网络下载/仓库检出)时,`tar` 会在归档根部额外写入 `._<name>` 条目,导致顶层段不唯一、
     剥层失败 → `NoManifest`。Task 6 的测试用 `--no-mac-metadata` 打成干净形态以聚焦来源流程本身。
     真实用户在 macOS 上用默认 `tar` 打包可能踩到;是否在解压层忽略 `._*` 属后续跟进(同样偏严而非越权)。
