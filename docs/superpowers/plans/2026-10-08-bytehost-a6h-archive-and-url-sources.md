# bytehost A6h:压缩包与 https URL 安装来源 Implementation Plan

> **归档说明(2026-10-08,A7 之后搬家):** 本计划**已在 dozer 仓库执行完成**(所有 Task 均 `[x]`,除两处需要 GUI 的手动验收 Step 保持未勾),随后随 A7 把 `crates/bytehost-*` 抽到本仓库。文档原样搬来作为执行记录,因此正文里的路径是**执行当时**的布局:
> - `crates/bytehost-apps/**`(`archive.rs`/`source.rs`/下载缓存/计划披露)现在在**本仓库**;
> - `crates/dozerd/**`、`crates/dozer-client/**`、`crates/dozer-app/**`(错误映射、来源选择 UI、审批卡披露)仍留在 **dozer 仓库**;
> - 门禁脚本已改名:本仓库是 `scripts/check-deps.sh`,dozer 保留 `scripts/check-log-scope.sh`;
> - 正文里的 commit 短哈希属于 **dozer 仓库历史**(搬运时 `git filter-repo` 保留了历史,哈希与提取前的 dozer 源码一致)。
>
> 未来同类改造:核心逻辑改在**本仓库**,消费者接线改在 **dozer**。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 除"本机目录"外,应用还能从**本机压缩包**(`.zip`/`.tar.gz`/`.tgz`)和 **https URL 压缩包**安装;网络来源按不可信处理(只允许静态应用),所有来源的信任级别由**服务端**按来源推导,不再信任客户端自报。

**Architecture:** `AppSource` 追加 `Archive{path}`、`Url{url, sha256}`。新增 `archive.rs`:**进程内**逐条目校验并解压到 staging(绝不交给系统 `tar`/`unzip`——输入不可信),随后走现有的"staging 副本重算摘要 + `verify`"流程,所以安装决定仍只基于 staging 里的内容。URL 来源经已有的 `CurlFetcher`(系统 curl,只信 https)下载;**出计划时下载并缓存字节**,安装时按计划里的归档 sha256 复用缓存(缓存缺失才重下并要求 sha256 一致),避免审批后换内容和重复下载。`Provenance`/`TrustLevel` 在服务端由来源推导。

**Tech Stack:** Rust(`bytehost-apps` 的 `server` feature 新增 `zip`、`flate2`、`tar`)、系统 `/usr/bin/curl`、serde(wire 只追加)、iced 纯状态机。

**Spec:** `docs/superpowers/specs/2026-10-04-bytehost-app-host-design.md`(§5 安装计划/审批;`AppSource` 注释"压缩包/仓库以后再加")。前序:`plans/2026-10-07-bytehost-a6d-runtime-install.md`(`Fetcher`/`CurlFetcher`/下载与校验的做法)。A6f/A6g 与本计划互相独立,构造函数参数的衔接见 Task 3 Step 0。

## 用户已裁决的行为(2026-10-08)

1. 新增来源:**本机压缩包 + https URL 压缩包**(Git 仓库不做)。
2. 网络(URL)来源**只允许静态应用**(`static_web`);Node/Python 仍只能从本机目录或本机压缩包安装,等沙箱落地后再放开。
3. URL 完整性:**允许填期望 sha256;填了必须匹配,不匹配拒绝;不填则审批卡如实展示实际算出的 sha256**。

## 本计划自己做的决定(请评审时确认或推翻)

- **本机压缩包 = 受信(`Local`/`Trusted`),可装进程型应用。** 理由:用户亲手选的本机文件,与本机目录同等;但这意味着从网上下载后手动选中的压缩包也享受同样待遇——审批卡对任何进程型应用的"将运行什么命令"披露(已有)是唯一防线。
- **服务端推导信任,忽略客户端自报:** `LocalDir`/`Archive` → `(Local, Trusted)`;`Url` → `(ThirdParty, Untrusted)`。`Plan` 请求里的 `provenance`/`trust` 字段保留(wire 兼容),但服务端**取更严格者**(客户端声称 `Trusted` 的 URL 来源仍按 `Untrusted` 处理),计划里展示的是生效值。
- **解压硬限制:** 压缩包 ≤ 200 MiB;解压后总量 ≤ 1 GiB;条目数 ≤ 20,000;单文件 ≤ 512 MiB;路径深度 ≤ 32、单段 ≤ 255 字节;压缩比超过 100:1 且解压量 > 64 MiB 视为压缩炸弹。超限一律拒绝并说清楚是哪一条。
- **归档形态:** 条目只允许普通文件与目录;**符号链接、硬链接、设备、FIFO 一律拒绝**(与 `copy_tree`/`digest_tree` 同口径);拒绝绝对路径、含 `..`/空段/`.` 的路径、反斜杠、非 UTF-8 名字、NUL;同一规范化路径出现两次(含 macOS 默认大小写不敏感下仅大小写不同)拒绝;若所有条目共享唯一顶层目录且根上没有 `manifest.toml`,自动剥掉该层(GitHub 风格 zip),否则 `manifest.toml` 必须在根。
- **URL 约束:** 只接受 `https://`;拒绝用户名/密码(`userinfo`)、空主机、控制字符、长度 > 2048;URL **整串不写入日志**(查询串里常带令牌),日志只记主机名。

## Global Constraints

- `bytehost-apps` 默认 feature 依赖只能是 serde 家族;新依赖(`zip`、`flate2`、`tar`)只能挂在 `server` feature 下(`dep:`);`scripts/check-bytehost-apps-deps.sh`、`scripts/check-log-scope.sh` 必须通过。**新增第三方依赖在评审时请单独确认**(`tar` 目前不在 `Cargo.lock` 里)。
- wire 只追加:`AppSource` 只加变体;`InstallPlan` 新字段 `#[serde(default)]`;旧形状 JSON 必须能解析。
- `ApprovedInstallPlan::verify` 的"整份计划逐字段核对"原则不变——新增的来源披露字段(见 Task 2)**参与核对**。
- 下载只经 `CurlFetcher`(https-only、TLS≥1.2、限长、可取消);**不得**新开一条下载路径。
- URL 与下载内容不写 dozerd 日志(只记主机名与字节数);临时文件在失败/取消/成功三条路径都要清理。
- 持久状态不进 Toast;设置页内联错误留在原位(沿用 A4c 的做法)。
- 日志统一走 `dozer_core::log_*!`;提交前 `git diff --cached --stat` 看全貌,只 `git add` 指定路径;不动 `Cargo.lock` 里与本计划无关的改动(新增依赖导致的 `Cargo.lock` 变化属于本计划,单独列出)。
- 变异验证:每条新校验的测试都要临时去掉实现确认会失败,再恢复。

## Review Focus

(最可能咬到真实用户的输入/失败模式,最危险的排前面;每条在对应任务里有测试。)

1. **zip-slip / 路径穿越**:`../../etc/x`、绝对路径、`a/../../b`、反斜杠 `..\..\x`,必须拒绝且**staging 之外零写入**(Task 1)。
2. **符号链接/硬链接条目**指向包外(如 `web/leak -> /etc/hosts`):拒绝,不得出现在 staging 里(Task 1)。
3. **压缩炸弹**(小文件解出 GB 级、百万条目):在超限的那一刻中止并清掉已写出的部分,不是解完再检查(Task 1)。
4. **客户端把 URL 来源自报成 `Trusted`**,企图装 Node/Python 应用:必须被拒(Task 2)。
5. **审批后服务器换了内容**(同 URL 两次下载字节不同)、**重定向到非 https 或另一个主机**:安装必须拒绝/披露,不得装上未审批的字节(Task 3)。
6. **用户填的 sha256 不匹配**(含大小写、前后空白、长度不对):明确拒绝且不留临时文件(Task 3)。
7. **URL 里带 `user:pass@` 或查询串令牌**:拒绝凭据;日志与错误信息里不得出现整串 URL(Task 3)。
8. **下载中途取消/磁盘写满/超过 400 MB**:不留半截文件,错误可读(Task 3)。
9. **GitHub 风格 zip(单一顶层目录)与根上直接放 `manifest.toml` 两种形态**都能装;含多个顶层目录且根上没有清单的包给出清楚的错误(Task 1、2)。

## File Structure

| 文件 | 变更 | 职责 |
|---|---|---|
| `crates/bytehost-apps/Cargo.toml` | 改 | `server` feature 增 `dep:zip`、`dep:flate2`、`dep:tar` |
| `crates/bytehost-apps/src/archive.rs` | **新**(`server`) | `extract(archive_path, kind, into, limits) -> Result<ExtractReport, ArchiveError>`:逐条目校验的 zip / tar.gz 解压 |
| `crates/bytehost-apps/src/source.rs` | **新**(`server`) | `SourcePolicy`:来源 → `(Provenance, TrustLevel, 是否只允许静态)`;URL 校验;`stage_source` |
| `crates/bytehost-apps/src/proto.rs` | 改 | `AppSource::{Archive, Url}`;`SourceInfo`(计划里的来源披露) |
| `crates/bytehost-apps/src/plan.rs` | 改 | `InstallPlan.source_info: SourceInfo`(`#[serde(default)]`,参与 `verify`) |
| `crates/bytehost-apps/src/runtime/managed/fetch.rs` | 改 | `CurlFetcher` 增 `fetch_with_meta`(最终 URL、字节数);`Fetcher::fetch` 委托它 |
| `crates/bytehost-apps/src/manager.rs` | 改 | `install_plan`/`install` 经 `stage_source`;下载缓存;`ManagerConfig` |
| `crates/dozerd/src/app_service.rs` | 改 | 新错误类别映射;其余透传 |
| `crates/dozer-app/src/extensions/settings_apps.rs`、`app/view.rs`、`app/app.rs` | 改 | 来源选择(目录/压缩包/URL + 可选 sha256)、审批卡来源披露 |
| `crates/dozerd/tests/process_apps_live.rs` | 改 | 本机压缩包端到端(真 python 样例打成 zip 安装) |
| `docs/superpowers/specs/2026-10-08-bytehost-a6h-acceptance-report.md` | **新** | 验收报告(只填实际运行结果) |

---

### Task 1: 安全解压(`archive.rs`,纯函数,无网络)

**Files:** Create `crates/bytehost-apps/src/archive.rs`;Modify `Cargo.toml`、`lib.rs`(`#[cfg(feature = "server")] pub mod archive;`);Test: `archive.rs` 的 `mod tests`。

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum ArchiveKind { Zip, TarGz }
  impl ArchiveKind { pub fn from_path(p: &Path) -> Option<Self> }   // .zip / .tar.gz / .tgz(大小写不敏感);其余 None
  #[derive(Debug, Clone, Copy)]
  pub struct Limits { pub max_archive: u64, pub max_total: u64, pub max_entries: usize, pub max_file: u64, pub max_depth: usize, pub max_ratio: u64, pub ratio_floor: u64 }
  impl Default for Limits { /* 200MiB / 1GiB / 20_000 / 512MiB / 32 / 100 / 64MiB */ }
  #[derive(Debug)]
  pub enum ArchiveError { Unsupported(String), TooLarge(&'static str), UnsafeEntry { name: String, why: &'static str }, Duplicate(String), Bomb, NoManifest, Io(io::Error), Corrupt(String) }
  pub struct ExtractReport { pub files: usize, pub bytes: u64, pub stripped_top_dir: Option<String> }
  /// 解压到 `into`(必须不存在或为空目录)。失败时 `into` 里已写出的内容由本函数清掉。
  pub fn extract(archive: &Path, kind: ArchiveKind, into: &Path, limits: &Limits) -> Result<ExtractReport, ArchiveError>;
  ```
- 实现要点:**先遍历全部条目元数据做校验**(路径规则、类型、重复键、条目数、声明的未压缩总量),再逐个写出;写出时用**实际读到的字节数**累计并随时对 `max_file`/`max_total`/压缩比设防(不信任头部声明的大小);目标路径用 `into.join(规范化相对路径)`,写之前再确认其父目录仍在 `into` 之下(`canonicalize` 比较);文件只以 `create_new` 打开;解压后权限统一为 `0o644`(目录 `0o755`),**不继承归档里的 setuid/可执行位**。顶层目录剥离在校验阶段决定。

- [x] **Step 1: 写失败测试**(用 `zip`/`tar` crate 在测试里**造恶意归档**,不要提交二进制 fixture)
  - 正常:根上 `manifest.toml` + `web/index.html` 的 zip 与 tar.gz 都能解出,内容一致;GitHub 风格(唯一顶层 `app-1.0/`)被剥层,`stripped_top_dir == Some("app-1.0")`;多个顶层目录且根上无清单 → `NoManifest`。
  - Review Focus 1:条目名 `../evil`、`a/../../evil`、`/abs/evil`、`C:\evil`、`web\..\..\evil`、`a//b`、`./a`、含 NUL → 各一个用例,全部 `UnsafeEntry`;**断言 `into` 的父目录(用 `tempdir` 套一层)里没有新文件**。
  - Review Focus 2:zip 里符号链接条目(`unix_permissions` 设 `S_IFLNK`)、tar 里 symlink / hardlink / 字符设备 / FIFO 条目 → `UnsafeEntry`,且 `into` 被清空。
  - Review Focus 3:① 一个声明 2 GiB 的条目(头部撒谎的变体:头部写 10 字节、实际流 100 MiB)→ 写到超限那一刻中止(断言**磁盘上已写出的字节数 < max_file + 一个块**);② 20,001 个空文件 → `TooLarge("entries")`;③ 高压缩比(1 MiB 全零 deflate 出 ~1 KiB,重复到解压量 > 64 MiB)→ `Bomb`。
  - 其它:同一路径出现两次 → `Duplicate`;仅大小写不同(`Web/a` 与 `web/a`)→ `Duplicate`;非 UTF-8 条目名(tar)→ `UnsafeEntry`;路径深度 33 → `TooLarge("depth")`;压缩包本身 > `max_archive` → `TooLarge("archive")`(用小的自定义 `Limits` 测,别真造 200 MiB);损坏的 zip/gz → `Corrupt`;扩展名不认识 → `ArchiveKind::from_path` 为 `None`;`into` 非空 → `Io`/拒绝。
  - 权限:归档里 `0o4755`(setuid)的文件解出后模式是 `0o644`。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p bytehost-apps --features server archive`;`cargo test -p bytehost-apps`(默认 feature)与 `scripts/check-bytehost-apps-deps.sh`;`cargo tree -p bytehost-apps` 默认 feature 下**不含** `zip`/`flate2`/`tar`)。变异:去掉路径含 `..` 的检查,Review Focus 1 的用例必须失败;把"实际字节累计"改成只信头部声明,炸弹用例失败;去掉 `Duplicate` 的小写折叠,大小写用例失败。
- [x] **Step 5: Commit**(`Cargo.toml`/`Cargo.lock` 的依赖变化与代码同一提交)— `feat(bytehost-apps): hardened zip and tar.gz extraction for untrusted archives (A6h task 1)`。

> **实现追记(2026-10-08,`ecc8cce0`)**:Step 1 的用例覆盖了全部 Review Focus,但与原文有个别手段差异——① zip 符号链接用 `ZipWriter::add_symlink` 造(用 `start_file` + `unix_permissions` 无法表达,`unix_permissions` 会把 mode 与 `0o777`,丢掉 `S_IFLNK` 位);② 重复路径用例直接测纯函数 `plan_entries`(zip/tar 写入器本身即拒绝重复名,无法造出重复名归档);③ "setuid 位"用例改为"归档给 `0o755`、解出恒为 `0o644`"(同因 `unix_permissions` 会丢弃 setuid 位),断言的是同一性质(不继承归档模式位);④ 声明总量超 `max_total` 现在会**提前拒绝**(复用 `declared` 字段),实际字节累计仍是主防线。`Cargo.lock` 未提交(本地 `[patch]` 路径覆盖会改其 source 行)。

---

### Task 2: 来源模型、服务端信任推导、本机压缩包安装

**Files:** Create `source.rs`;Modify `proto.rs`、`plan.rs`、`manager.rs`、`lib.rs`;Test: `source.rs`、`plan.rs`、`manager.rs` 的 `mod tests`。

**Interfaces:**
- Produces(`proto.rs`):
  ```rust
  // AppSource 追加
  Archive { path: PathBuf },                          // 绝对路径;类型按扩展名判断
  Url { url: String, sha256: Option<String> },        // sha256 为用户期望值(十六进制,大小写/空白在 `normalize_sha256` 里处理)
  // 计划里的来源披露(展示用,参与 verify)
  #[derive(Default, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct SourceInfo {
      pub kind: String,                    // "local_dir" | "archive" | "url"
      pub display: String,                 // 目录/文件路径;URL 为不含查询串的 `https://host/path`
      pub archive_sha256: Option<String>,  // 归档文件整体 sha256(archive/url 才有)
      pub archive_bytes: Option<u64>,
      pub effective_host: Option<String>,  // url:下载后最终落到的主机(Task 3 填)
      pub pinned: bool,                    // url:用户是否提供了期望 sha256 且已匹配
      pub stripped_top_dir: Option<String>,
  }
  ```
- Produces(`source.rs`):
  ```rust
  pub struct SourcePolicy { pub provenance: Provenance, pub trust: TrustLevel, pub static_only: bool }
  pub fn policy_for(source: &AppSource) -> SourcePolicy;                    // LocalDir/Archive → Local/Trusted/false;Url → ThirdParty/Untrusted/true
  pub fn effective(requested: (Provenance, TrustLevel), policy: &SourcePolicy) -> (Provenance, TrustLevel); // 取更严格者
  pub fn validate_url(url: &str) -> Result<ParsedUrl, SourceError>;         // https only、无 userinfo、非空主机、无控制字符、≤2048
  pub fn normalize_sha256(s: &str) -> Result<String, SourceError>;          // trim + 小写 + 必须 64 位十六进制
  ```
- `InstallPlan` 追加 `pub source_info: SourceInfo`(`#[serde(default)]`)。`ManagerError` 追加 `SourceNotAllowed(String)`、`Source(SourceError)`、`Archive(ArchiveError)`;`kind()` 映射:前两者与归档里的 `UnsafeEntry/Duplicate/Bomb/NoManifest/TooLarge/Corrupt/Unsupported` → `AppErrorKind::Rejected`,`ArchiveError::Io`/下载 I/O → `Internal`(**不新增 `AppErrorKind` 变体**,那是 wire 枚举)。
- `Core::install_plan`/`install` 改为:`policy_for(source)` → 生效的 `(provenance, trust)` 覆盖入参 → `stage_source(source, &staging)`(`LocalDir` = 现有 `copy_tree`;`Archive` = `archive::extract`;`Url` 留给 Task 3,本任务里先返回 `BadSource("URL 来源尚未启用")`)→ `read_package` → **若 `static_only` 且 `manifest.runtime` 不是 `StaticWeb` → `SourceNotAllowed("网络来源只能安装静态应用")`**(在出计划时就拒,不等到安装)。`SourceInfo` 在 `stage_source` 里填(`archive_sha256` 用 `digest::sha256_file`)。

- [x] **Step 1: 写失败测试**
  - `source.rs`:`validate_url` 表驱动——`https://example.com/a.zip` 通过;`http://…`、`file:///…`、`ftp://…`、`https://u:p@host/…`、`https://` 无主机、含 `\n`/空格/控制字符、2049 字符、大写 `HTTPS://` 通过(scheme 大小写不敏感)、带端口的 `https://example.com:8443/a.zip` 与 IP 字面量 `https://192.168.1.5/a.zip` 也通过(**不做**本机/内网地址过滤:内网分发与本机测试是合法场景,见已知局限,审批卡展示主机即可)。`normalize_sha256` 表驱动——大写、前后空白、63/65 位、非十六进制各一例。`policy_for`/`effective` 表驱动:客户端自报 `(Local, Trusted)` 的 `Url` → 生效 `(ThirdParty, Untrusted)`;`LocalDir` 自报 `(ThirdParty, Untrusted)` → 保留更严格的 `Untrusted`(**只升不降**)。
  - `manager.rs`(Review Focus 4、9):`Archive` zip 装静态应用成功,`list()` 里有该应用,`plan.source_info.kind == "archive"` 且 `archive_sha256` 等于 `sha256_file(zip)`;GitHub 风格 zip 成功且 `stripped_top_dir` 有值;`Archive` tar.gz 装 python 进程应用成功(缺 `python3` 则 `return`)——本机压缩包可装进程型;多顶层无清单 → 清楚的错误;**同一个 zip 先后改一个字节再装** → 摘要不符(`verify` 拒绝),旧审批对新内容无效;未知扩展名 → `BadSource`;相对路径 → `BadSource`(同 `LocalDir`);解压失败(恶意归档)后 `apps/` 下**没有** `.staging-*` 残留。
  - 兼容:旧形状 `InstallPlan` JSON(无 `source_info`)可解析、`verify` 在 `source_info` 默认值之间照常工作;`LocalDir` 的计划 `source_info.kind == "local_dir"`;现有全部 `LocalDir` 测试不改一行仍通过。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p bytehost-apps --features server`;默认 feature 与依赖门禁)。变异:`effective` 改成"直接信客户端",Review Focus 4 的单测失败;去掉 `static_only` 检查,`Url`+python 的用例(Task 3 加入后)失败。
- [x] **Step 5: Commit** — `feat(bytehost-apps): archive sources with server-derived trust (A6h task 2)`。

> **实现追记(2026-10-08)**:① `SourceInfo` 加了容器级 `#[serde(default)]`,旧计划 JSON 缺字段也能解析(不止"整个字段缺失");② 相对路径仍映射为 `ManagerError::BadSource`(在 `manager.rs::ensure_absolute` 里对 `LocalDir`/`Archive` 先校验),`source.rs` 内部另有一份绝对路径校验兜底;③ `stage_source` 的错误经 `StageError::{Source,Archive,Io}` 映射到 `ManagerError::{Source,Archive,Io}`(Task 2 阶段 `Url` 返回 `SourceError::NotEnabled` → `Rejected`);④ `static_only` 检查放在 `install_staged`(出计划按来源落地时 `Url` 尚不可达,Task 3 起生效);⑤ zip 用的是 `CompressionMethod::Stored`(测试造包不依赖 deflate 编码器)。`Cargo.lock` 未提交。

---

### Task 3: https URL 来源(下载、缓存、sha256 钉死)

**Files:** Modify `runtime/managed/fetch.rs`、`source.rs`、`manager.rs`;Test: 同文件 `mod tests`。

**Step 0 — 构造函数衔接:** `AppManager` 现在靠一串 `with_*` 构造函数传 `resolver/policy/version_probe/monitor`(A6e),A6g 可能再加。本任务新增 `fetcher: Arc<dyn Fetcher>`,**同时**把它们收成具名字段结构体 `ManagerConfig { resolver, policy, version_probe, monitor, fetcher }`(`Default` 给生产值),`with_parts_full` 改为 `AppManager::with_config(root, host_version, gateway, config)`;保留 `with_resolver` 等公共入口作为薄封装,测试钩子 `with_*_for_test` 全部改为组装 `ManagerConfig`。**先看 `git log -5 -- crates/bytehost-apps/src/manager.rs`:若 A6g 已落地并引入了同名结构,则复用并追加 `fetcher` 字段,不要建第二个。**

**Interfaces:**
- Produces(`fetch.rs`):
  ```rust
  pub struct FetchMeta { pub effective_url: String, pub bytes: u64 }
  impl CurlFetcher { pub fn fetch_with_meta(&self, url: &str, dest: &Path, max_bytes: u64, on_progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool) -> io::Result<FetchMeta>; }
  // Fetcher trait 追加带默认实现的 `fn fetch_meta(&self, ..) -> io::Result<FetchMeta>`:默认调 `fetch` 后用 `dest` 的长度和传入 URL 填充;CurlFetcher 覆盖为真实值
  ```
  `fetch_with_meta` 在原有 curl 参数上加 `-w "%{url_effective}"`(stdout 写到管道读取),`--max-filesize` 改为入参(安装来源用 200 MiB,运行时下载仍 400 MB);**重定向后的最终 URL 必须仍是 https**(`--proto-redir =https` 已保证,测试里断言实际行为)。
- 下载缓存:`<root>/downloads/<archive_sha256>.part|.bin`。`install_plan`(Url)下载到 `downloads/<uuid>.part` → 算 sha256 → 若用户给了期望值且不匹配 → 删文件、`SourceError::Sha256Mismatch`;匹配/未给 → 改名 `<sha256>.bin`,计划里填 `archive_sha256/archive_bytes/effective_host/pinned`。`install(approved, Url)`:按 `approved.plan().source_info.archive_sha256` 找 `downloads/<sha>.bin`,**读出来重新算一遍 sha256**与文件名一致才用;缺失/损坏才重新下载,且下载结果 sha256 必须等于计划里的值,否则 `Verify(SourceChanged)`。安装成功/失败后都删对应缓存;`sweep_staging` 旁加 `sweep_downloads`(启动时清 `*.part` 与超过 1 小时的 `*.bin`)。
- URL 日志规则:`log_info!(LOG, host = %host, bytes, "下载应用包")`;错误文案里用 `display`(不含查询串)。

- [x] **Step 1: 写失败测试**(注入假 `Fetcher`:从测试准备好的归档字节"下载",可配置失败/变更内容/最终 URL)
  - 成功:假 fetcher 提供静态应用 zip → `install_plan(Url)` 得到 `source_info{kind:"url", archive_sha256, effective_host, pinned:false}`;`(Untrusted)`;`install` 成功**且假 fetcher 只被调用 1 次**(安装复用缓存,断言调用计数);成功后 `downloads/` 为空。
  - Review Focus 6:期望 sha256 写成大写带空白但值正确 → 通过且 `pinned == true`;错一位 → `Sha256Mismatch`、`downloads/` 为空、无 `.part`;长度 63/65、非十六进制 → 格式错误(未发起下载:断言 fetcher 调用 0 次)。
  - Review Focus 5:① 计划后把缓存文件改一个字节 → 安装拒绝(读出重算 sha 不符,不使用);② 删掉缓存且假 fetcher 第二次返回**不同字节** → `Verify(SourceChanged)`,不落位;③ 最终 URL 的主机与请求主机不同 → `source_info.effective_host` 如实是新主机(审批卡据此提示"已重定向到 X");④ fetcher 报告最终 URL 为 `http://…` → 拒绝(防御性:即便 curl 参数被改也守住)。
  - Review Focus 4(接 Task 2):`Url` 来源的 python/node 应用 → 出计划时 `SourceNotAllowed`;客户端自报 `Trusted` 的 `Url` 仍按 `Untrusted`。
  - Review Focus 7:`https://user:pass@host/x.zip` → 拒绝,错误文案与 `captured log`(用 `tracing` 测试订阅或让日志函数可注入)里**不含** `pass`;含查询串 `?token=SECRET` 的 URL → 成功,但日志与错误文案里不含 `SECRET`。
  - Review Focus 8:取消(`cancel` 置位)→ `Interrupted`、`downloads/` 无残留;fetcher 写一半返回 `Err` → 无残留;超过 `max_bytes` → 清楚的错误;磁盘写满用只读目录模拟 `Io` 错误 → 无残留。
  - `CurlFetcher` 本身:不发真实网络请求——参数拼装抽成纯函数 `curl_args(url, dest, max_bytes) -> Vec<OsString>` 单测(含 `--proto =https --proto-redir =https --tlsv1.2 --max-filesize <n> -w %{url_effective}`);A6d 原有的 fetch 测试不改一行仍通过。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p bytehost-apps --features server`)。变异:安装时不再重算缓存 sha,① 用例失败;`pinned` 比较改成区分大小写,大写用例失败。
- [x] **Step 5: Commit** — `feat(bytehost-apps): https URL sources with download cache, sha256 pinning and static-only policy (A6h task 3)`。

**实现追记(2026-10-08):**
- `ManagerConfig` 在 A6g 之后**尚未存在**,本任务新建;生产默认值 `fetcher = Arc::new(CurlFetcher)`。`new`/`with_resolver` 变成薄封装,`with_config` 是唯一真正构造点。
- **计划里"`Verify(SourceChanged)`"落地为 `SourceError::Sha256Mismatch`**:安装时缓存缺失/损坏就重新下载,下载结果与计划披露的 sha 不等即拒(与出计划时同一错误类型,语义更直白)。②用例据此断言 `Sha256Mismatch`。
- **①(缓存被改一个字节)落地为"不使用并重下"**:`extract_cached_archive` 重算 sha 不符即返回 `None`,安装重新下载正确字节后**安装成功**(缓存只剩数据价值,篡改不等于源被换)。用户可见行为:安装仍能成功;断言的是 fetcher 调用计数 +1、缓存最终清空。
- **格式错误 sha256 在 `validate_url`/`normalize_sha256` 阶段就拒**,不进入下载(断言 fetcher 调用为 0)。测试里累计计数含"错一位"那次下载,故成功路径总计数为 2。
- **`static_only` 的出计划检查放在 `Core::install_plan`**(读到 manifest 之后),命中即清掉刚下的缓存并回 `SourceNotAllowed`;`install_staged` 里保留第二道(纵深防御)。
- **`bytehost-apps` 无 logger**(不能依赖 `dozer_core`),计划中的 `log_info!("下载应用包")` **无法在此层写**;改为保证错误文案与 `display` 不含凭据/查询串(已测)。下载日志留待 dozerd 层(Task 4)。
- `SourceError::NotEnabled` 变体保留(公共类型,Task 2 引入),现已无构造点;不删以免动公共枚举。

---

### Task 4: dozerd 与 client 贯通

**Files:** Modify `crates/dozerd/src/app_service.rs`(错误映射)、`crates/dozer-client/src/lib.rs`(无新请求,仅确认 `Plan`/`Install` 透传新 `AppSource`);Test: `app_service.rs` 的 `mod tests`、`crates/dozerd/tests/app_requests.rs`。

- `AppSource::{Archive, Url}` 已随 `AppRequest::{Plan, Install}` 序列化,dozerd 无需新请求。要做的:`ManagerError::{SourceNotAllowed, Archive(..), Source(..)}` → `AppErrorKind`(用户可修正的输入问题映射到现有的 `Rejected`;I/O 类映射 `Internal`),错误文案**不带 URL 查询串**。生产的 `AppService` 用 `CurlFetcher`;测试钩子允许注入假 `Fetcher`(沿用 `finish_start_with` 的可选参数做法,不复制启动流程)。

- [x] **Step 1: 写失败测试**
  - `app_service.rs`:各新错误类别的映射表驱动;`Plan{ source: Archive }` 经 `handle` 得到带 `source_info` 的计划;`Plan{ source: Url }` + 注入的假 fetcher 得到 `ThirdParty/Untrusted`;客户端在请求里自报 `Local/Trusted` → 回来的计划是 `ThirdParty/Untrusted`(Review Focus 4 的端到端版本)。
  - `app_requests.rs`:经真实 UDS 走 `Plan(Archive)` → `Install` → `List`;旧客户端形状(只有 `LocalDir`)照常工作。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p dozerd app_service`、`cargo test -p dozerd --test app_requests`、`cargo test -p dozer-client`)。
- [x] **Step 5: Commit** — `feat(dozerd): carry archive and URL sources end to end (A6h task 4)`。

**实施追记(Task 4):**
1. **错误映射其实在 Task 2 就做完了**:`ManagerError::{SourceNotAllowed, Archive, Source}::kind()`(manager.rs:128-154)已把输入类问题映射 `Rejected`、`Io`/`Source::Io`/`Archive::Io` 映射 `Internal`。本 Task 只在此基线上补测试与下载器注入,未再改映射代码。
2. **新增下载器注入点**:把启动主体抽成 `AppService::finish_start_with_sources(root, gateway, runtime_manager, monitor, app_fetcher: Option<Arc<dyn Fetcher>>)`(`#[doc(hidden)]`),原 `finish_start_with` 只是转发 `None`;构造从 `with_resolver`/`with_monitor_for_test` 改为统一走 `AppManager::with_config`,注入的 fetcher 覆盖默认 `CurlFetcher`(不复制启动流程)。生产 `main` 仍走 `finish_start_with` → 真实 `CurlFetcher`。
3. **`dozer-client` 零改动**:`app_plan`/`app_install` 直接透传 `AppSource`,新变体自动随之。
4. **`process_apps_live.rs` 4 处 `let AppSource::LocalDir{..} = ..` 因 Task 2 扩枚举而编译失败**,本 Task 一并补 `else { panic!(..) }`(这些 helper 只处理本机目录)。
5. **dozerd 测试造压缩包不引新依赖**:复用 `/usr/bin/tar -czf <a> -C <src> manifest.toml web`(只 tar 具名条目,避免 `.`/`./` 段被解压器判为不安全)。`SourceFetcher`(内存字节→dest,可配 `effective_url`)实现 `Fetcher::fetch/fetch_meta`。
6. **变异验证**:①`source::effective` 直信客户端 trust → `a_url_source_is_untrusted_no_matter_what_the_client_claims` FAILED;②`manager` 两处 `policy.static_only` 检查短路 → `a_url_source_of_a_process_app_is_rejected_over_the_wire` FAILED。均已还原。
7. `cargo test -p dozerd` 全绿(492 lib + 集成);fmt/两门禁绿;clippy 仅剩既有 `preview_commands.rs:152` 警告。已知偶发:`memory::tests::list_orders_by_updated_ms_desc`、`a_python_app_runs_through_the_wire_and_dies_with_dozerd`(并行偶发,单跑通过)。

---

### Task 5: GUI——来源选择与审批卡披露

**Files:** Modify `settings_apps.rs`、`app/view.rs`、`app/app.rs`(原生文件对话框副作用);Test: `settings_apps.rs` 的 `mod tests`。

**Interfaces:**
- Produces:
  ```rust
  pub enum SourceChoice { Dir, Archive, Url }                       // 设置页「安装应用…」的来源切换
  // Message 追加
  SourceChoiceChanged(SourceChoice), PickArchive, UrlChanged(String), Sha256Changed(String), UrlPlanClicked,
  // Effect 追加
  PickArchive,                  // 与 `PickSource` 同样由窗口层拦截,弹原生选文件对话框(过滤 zip/tar.gz/tgz)
  ```
  URL 与 sha256 两个输入框沿用设置页已有的 `text_input` 接线方式(**先读 `settings.rs`/`settings_apps.rs` 里现有输入框是怎么接焦点与原生输入的,照做**;不得新发明一套);输入的原始字符串由状态机持有,点击「获取计划」时才发 `Effect::Plan(AppSource::Url{..})`。
- 审批卡(`plan_view`)追加"来源"区块:来源类型、`display`、`archive_sha256`(完整 64 位,可复制不截断)、`archive_bytes`;URL 来源额外:醒目的"来自网络,不可信"标注 + "只允许静态应用,运行在严格 CSP 下"说明;`effective_host` 与请求主机不同 → 金色警告"已重定向到 X";`pinned == false` → "未提供期望 sha256,以上哈希是本次下载实际算出的";`pinned == true` → "已匹配你提供的 sha256"。进程型应用从本机压缩包来:沿用现有"将运行的命令"披露,**另加一行**"来自压缩包,内容已解压校验"。
- `SourceNotAllowed`/`Sha256Mismatch`/格式错误等失败**留在安装区域内联显示**(不弹 Toast),并保留已填的 URL 与 sha256 以便修改重试。

- [x] **Step 1: 写失败测试**
  - 来源切换:切到 `Url` 不丢已填的输入;切换来源会清掉上一次的计划与审批卡(避免"审批的是 A 来源、安装的是 B 来源");`UrlPlanClicked` 在 URL 为空/非 https 时**不发 Effect**,内联提示原因(客户端先行校验只是体验,真正的校验在服务端)。
  - `Sha256Changed` 即时格式提示(空 = 不钉死;非法 = 内联红字;合法 = 无提示),不改变其它状态。
  - 审批卡表驱动:四种 `source_info` 组合(local_dir / archive / url 未钉死 / url 钉死且重定向)各自的行文案与警告;`archive_sha256` 完整显示。
  - `Plan` 失败(`SourceNotAllowed`)→ 内联错误且输入保留;随后改成本机压缩包重试成功。
  - 并发:计划在途时重复点击不重发(沿用现有 `in_flight` 守卫)。
- [x] **Step 2: 确认失败 → Step 3: 实现 → Step 4: 通过**(`cargo test -p dozer-app settings_apps`;`cargo clippy -p dozer-app --all-targets` 无新警告)。变异:不清上一次计划,切换用例失败。
  - 实施追记:
    1. `bytehost_apps::source`(含 `validate_url`/`normalize_sha256`)是 **`server` feature 门控**的,`dozer-app` 拿不到(它是 `dozerd` 才开的 feature)。客户端先行校验因此**在 GUI 内自带轻量实现**(`normalize_sha256_client`:trim + 小写 + 64 hex;URL 只查空/`https://` 前缀),真正的解析/校验仍在服务端 —— 与计划"客户端先行校验只是体验"的口径一致。
    2. `SourceChoice{ Dir,Archive,Url }` 落在 `State.source_choice`;`url_input`/`sha_input`/`url_error` 也落 `State`(切来源/失败都不清,满足"输入保留")。`Flow::Picking` 带 `PickTarget{ Dir,Archive }`(Url 不弹对话框);`Flow::{Planning,Reviewing}` 与 `Effect::{Plan,Install}` 的 `source` 从 `PathBuf` 改成 `AppSource`。
    3. 新增 `Message::{SourceChoiceChanged,PickArchive,UrlChanged,Sha256Changed,UrlPlanClicked}` + `Effect::PickArchive`。原生对话框拦截**发生在 Message 层**(与既有 `InstallClicked` 一致,不是拦截 Effect):`window_events` 现在同时拦 `InstallClicked`/`PickArchive`,按**状态机当前 `source_choice`** 决定弹"选目录"还是"选文件(过滤 zip/tar.gz/tgz)",结果仍经 `SourcePicked` 回状态机;`Effect::{PickSource,PickArchive}` 在 `settings.rs` 是 no-op。
    4. 审批卡新增 `PlanView.source: SourceDisclosure`(`source_disclosure(info, runtime_kind)` 纯函数):来源类型/位置/完整 64 位哈希/大小/剥掉的顶层目录;URL 来源给"来自网络,不可信" + "只允许静态应用,运行在严格 CSP 下",未钉死说"未提供期望 sha256,以上哈希是本次下载实际算出的",钉死说"已匹配你提供的 sha256",`effective_host` 与请求主机不同给金色"已重定向到 X";进程型应用从本机压缩包来另加"来自压缩包,内容已解压校验"。URL/sha 输入框沿用 `byteui::form::input_text::view`(真 `text_input`,同分类树改名框),未新发明接线。
    5. 变异验证:把 `SourceChoiceChanged` 里的"清旧计划"改成 `if false && …` → `switching_source_keeps_typed_input_but_clears_a_stale_plan` 与 `a_rejected_url_plan_stays_inline_and_keeps_input_then_archive_retries` 双双 FAILED;已 `cp` 还原。
    6. 结果:`cargo test -p dozer-app settings_apps` 47 passed;`settings`(全设置页)76 passed;`cargo test -p dozer-app` 1981 passed(仅既有 `files::tests::delete_confirm_spec_reflects_pending_target` 与偶发 `assets::tests::serves_vendored_asset_with_mime` 失败,与 A6h 无关);`cargo clippy -p dozer-app --all-targets` 触碰文件零警告;`cargo fmt` 干净;两个门禁脚本绿。
- [ ] **Step 5: 手动验收**(需要 GUI,如实记入报告 §2,未做的不勾):① 选本机目录安装仍正常;② 选 `py-notes` 打成的 zip 安装并启动;③ 填一个真实 https 的静态应用 zip(例如自己托管的 Excalidraw 打包产物)→ 审批卡显示来源、哈希、"不可信"标注 → 批准安装 → 能打开;④ 同一个 URL 填错 sha256 → 内联拒绝;⑤ 填一个 Node 应用的 URL → 内联"网络来源只能安装静态应用";⑥ 输入框里中文输入法(IME)与粘贴正常。
- [ ] **Step 6: Commit** — `feat(dozer-app): archive and URL sources in the install flow with source disclosure (A6h task 5)`。

---

### Task 6: 端到端验收与文档

**Files:** Modify `crates/dozerd/tests/process_apps_live.rs`;Create `docs/superpowers/specs/2026-10-08-bytehost-a6h-acceptance-report.md`;Modify `CLAUDE.md`、规格 §7 表。

- [x] **Step 1: live 用例**(`#[ignore]`;沿用 A6e 的辅助;**所有"等 pid/等恢复"的断言要求纯数字 pid**——A6e 曾因接受网关错误页正文而假通过,任何"快得不可能"的耗时都要追到根因):
  1. 把 `scripts/bytehost/samples/py-notes` 与 `node-notes` 在测试里**现场打成 zip 和 tar.gz**(临时目录,不提交二进制),经 `AppService` 走 `Plan(Archive)` → `Install` → `Start`,验证 A6e 的 11 步里与来源无关的核心几步(鉴权、SSE、计数持久化、崩溃重启)在压缩包来源下同样成立;
  2. 恶意压缩包(zip-slip、符号链接)经 `AppService` 被拒,且**数据根目录之外没有新文件**(用 `walkdir` 对测试根的父目录前后快照比较);
  3. `Url` 来源:用假 `Fetcher` 注入一个静态应用 zip,走完 Plan→Install→Start→经 gateway 取到页面;同一假 fetcher 供给 Node 应用 → 被拒。
- [x] **Step 2: 连跑 3 遍**,耗时如实记入报告 §1。
- [x] **Step 3: 报告**:§0 环境(含新增依赖版本);§1 自动化(命令 + 结果 + 实测耗时);§2 GUI 手工项(Task 5 Step 5,**含一次对真实 https 服务器的下载**——自动化测试里没有真实网络,这一项只能手工,未执行保持未勾选);§3 缺陷与已知局限(据实,不预填)。
- [x] **Step 4: CLAUDE.md** `bytehost-apps` 一行补:A6h 落地——`AppSource::{Archive, Url}`;压缩包**进程内**逐条目校验解压(拒符号/硬链接、`..`/绝对路径、重复/大小写折叠重复、炸弹,限额见 `archive::Limits`),不交给系统 `tar`;**信任由服务端按来源推导**(本机目录/压缩包 = `Local/Trusted`,URL = `ThirdParty/Untrusted`,客户端自报只能更严不能更松);**网络来源只允许静态应用**;URL 只认 https、拒凭据、整串不入日志;出计划时下载并缓存字节(`<root>/downloads/<sha256>.bin`),安装复用并重算校验;`InstallPlan.source_info` 参与 `verify`。规格 §7 表加 A6h 一行并链接计划与报告;注明 Git 仓库来源与"网络来源放开进程型应用(依赖沙箱)"仍未做。
- [x] **Step 5: 全量**:`cargo test -p bytehost-apps -p dozerd -p dozer-client -p dozer-app`(已知无关失败:`files::tests::delete_confirm_spec_reflects_pending_target`、`memory::tests::list_orders_by_updated_ms_desc`、`app_service::tests::the_first_run_is_unavailable_when_the_port_cannot_be_saved`——如仍存在逐个注明,不要顺手改)、`cargo clippy --all-targets`(A6h 触碰的文件无新警告)、`cargo fmt --check`、两个门禁脚本。
- [x] **Step 6: Commit** — 测试与报告各一个:`test(dozerd): archive-sourced apps end to end and hostile archives refused (A6h task 6)`、`docs(bytehost): A6h landed`。

**实施追记(Task 6)**:

- **用 `snapshot_tree` 替代 `walkdir`**:仓库没有任何 crate 依赖 `walkdir`,新增依赖会污染不提交的 `Cargo.lock`。改用 `process_apps_live.rs` 内的本地递归 `snapshot_tree`(相对路径→类型的有序集合),语义等价。属等价手段替换,已记入报告 §3.2。
- **打包形态**:macOS bsdtar 的 AppleDouble(`._*`)资源叉条目会在归档根部多出条目、破坏"唯一顶层目录"判定 → `NoManifest`。测试打包加 `--no-mac-metadata`(`zip` 加 `COPYFILE_DISABLE=1`)打干净形态;是否在解压层忽略 `._*` 属后续跟进。
- **`tar -C src .` 形态被拒**:解压器按设计拒绝含 `.` 段的路径,`./manifest.toml` 这类常见归档会被拒。测试采用"单一顶层目录"形态;`./` 形态的支持属后续跟进(已记入报告 §3.3 与 CLAUDE.md 已知偏严项)。
- **假 `Fetcher` 注入**:跨 crate 用 `AppService::finish_start_with_sources(root, gateway, rm, None, Some(fetcher))`(`#[doc(hidden)] pub`);`Fetcher`/`FetchMeta` 公有。
- **静态应用清单**必须带 `[runtime] kind = "static_web"` / `source = "web/"`,否则 `missing field runtime`(首轮踩到,已修)。
- **URL 用例的静态页**经真 gateway 取到 `GET /` 正文含 `hello-from-url`;假 fetcher `calls == 1`(证明只下载一次、安装复用缓存)。
- **耗时**:单线程连跑 3 遍 `19.69s / 19.70s / 19.72s`;并行跑 7 个用例时偶发 1 例既有并行掉队(见报告 §3.1)。


---

## 已知局限(写在这里,不是缺陷)

- **不拦截本机/内网地址的 URL**(`https://localhost/…`、`https://10.x/…`):内网分发与本机测试是合法场景;审批卡展示主机即可,不做 SSRF 过滤。
- **网络来源的静态应用仍是 `Advisory` 级别的出站网络**(CSP 挡不住 WebRTC 等,见规格 A1 评审)。
- **本机压缩包享受 `Trusted`**:用户手动选中一个从网上下载的压缩包,与选目录一样受信;防线是审批卡对进程型应用命令的披露。
- **没有签名校验**:sha256 只防"字节被换",不证明发布者身份。
- **没有 Git 仓库来源**(用户裁决不做);**网络来源放开进程型应用**要等沙箱。
- **缓存只保留 1 小时**,计划与安装间隔更久会重新下载并要求 sha256 一致。
- **只支持 `.zip`、`.tar.gz`、`.tgz`**;`.tar.xz`/`.7z`/`.rar` 不支持。

## Self-Review(写计划时已核对)

- **范围覆盖**:用户三项裁决——本机压缩包 + https URL(Task 1–3、5)、网络来源只允许静态应用(Task 2 的 `static_only`,Task 3 的端到端用例)、sha256 可选钉死(Task 3);四个自己的决定(本机压缩包受信、服务端推导信任、解压限额、URL 日志规则)明列待评审。
- **占位符**:无;验收报告 §1/§3 要求执行时据实填写。
- **类型一致**:`ArchiveKind/Limits/ArchiveError/ExtractReport/extract`(Task 1)、`SourcePolicy/policy_for/effective/validate_url/normalize_sha256/SourceInfo/AppSource::{Archive,Url}/ManagerError::{SourceNotAllowed,Archive}`(Task 2)、`FetchMeta/fetch_with_meta/ManagerConfig/downloads 缓存`(Task 3)在定义它们的任务里给出签名,后续任务按同名使用;Task 2 里 `Url` 分支先返回占位错误、Task 3 才启用,Task 2 的测试不得依赖 `Url` 成功路径。
- **已核实的前提**:`Install` 把 `source` 再传一遍并由服务端对 staging 重算后 `verify`(所以远程来源天然不能被审批后换内容骗过);`Provenance/TrustLevel` 目前只记录展示、由客户端在 `Plan` 请求里自报(所以必须在服务端推导才有意义);`digest_tree` 按路径与内容计算、与时间戳无关(同一归档内容的不同打包字节不影响树摘要);`Cargo.lock` 里已有 `zip` 8.6 与 `flate2`、没有 `tar`;`CurlFetcher` 已是 https-only、限长、可取消。
