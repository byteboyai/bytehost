//! 「应用」页的状态机(bytehost A4c/A6h):运行时探测、已安装应用的停止/卸载/回滚、安装流程
//! (选来源 → 出安装计划 → **展示给用户审批** → 安装)。**纯状态机**:`update` 只返回 [`Effect`],
//! 由宿主去执行(发请求、弹通知),所以不开窗口、不连后端也能测。
//!
//! 要点:
//! - **审批的就是展示的**:用户看到的 `InstallPlan` 原封不动地 `approve` 并交给服务端;服务端对 staging
//!   副本重新计算并核对(摘要/整份计划),不一致就拒绝——这里不重算也不改写计划;
//! - **强制等级原样展示**(`Enforced`/`Advisory`/`Unsupported`),不替权限"美化":一条只是声明、没被强制的权限
//!   必须看得出来;
//! - 流程里的校验类失败(清单不合法、版本已装、审批后源码变了)**留在对话流程内**(同"移动对话框内的校验
//!   错误"的先例),不弹通知;"刚发生的一件事"(安装成功、停止/卸载失败)走通知;
//! - 来源有三种(A6h):本机目录、本机压缩包(`.zip`/`.tar.gz`/`.tgz`)、https URL 压缩包。「信任」由
//!   **服务端按来源推导**(客户端自报只能更严不能更松),审批卡如实披露来源、哈希与"是否来自网络";
//!   URL 与 sha256 两个输入框的原始字符串由本状态机持有,客户端先行的格式校验只是体验,真正的校验在服务端。

use std::path::PathBuf;

use bytehost_apps::permissions::{Enforcement, PermissionKey};
use bytehost_apps::plan::{Approval, ApprovedInstallPlan, InstallPlan, Provenance, TrustLevel};
use bytehost_apps::proto::{
    AppSource, AppSummary, ManagedRuntime, RuntimeAvailability, RuntimeInstallPlan, RuntimeProbe,
    SourceInfo,
};

use bytehost_apps::registry::{RollbackNote, UninstallMode};
use bytehost_apps::state::ObservedState;

use crate::logs::{LogsState, LogsView};
use crate::notice::NoticeLevel;
use bytehost_client::AppApiError as Failure;

/// 批准记录里写的"谁批准的"(审计用,不防伪——见 `ApprovedInstallPlan` 文档)。
pub const APPROVER: &str = "dozer-gui";

/// 设置页「安装应用…」的来源切换(A6h)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceChoice {
    /// 本机目录(目录里要有 `manifest.toml`)。
    #[default]
    Dir,
    /// 本机压缩包(`.zip`/`.tar.gz`/`.tgz`)。
    Archive,
    /// https URL 指向的压缩包。
    Url,
}

/// 弹原生选择对话框时选的是文件还是目录(Url 不走对话框)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickTarget {
    Dir,
    Archive,
}

impl SourceChoice {
    /// 这条来源要不要弹"选文件"对话框;`None` = 不弹(Url 直接发计划)。
    fn pick_target(self) -> Option<PickTarget> {
        match self {
            SourceChoice::Dir => Some(PickTarget::Dir),
            SourceChoice::Archive => Some(PickTarget::Archive),
            SourceChoice::Url => None,
        }
    }
}

impl PickTarget {
    /// 用选中的路径拼出对应的 `AppSource`。
    fn to_source(self, path: PathBuf) -> AppSource {
        match self {
            PickTarget::Dir => AppSource::LocalDir { path },
            PickTarget::Archive => AppSource::Archive { path },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Load<T> {
    #[default]
    Loading,
    Loaded(T),
    Failed(String),
}

/// 安装/卸载对话流程(同一时刻只有一个)。
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Flow {
    #[default]
    Idle,
    /// 原生选目录/选压缩包对话框开着(由窗口层执行,结果经 `SourcePicked` 回来)。
    Picking {
        target: PickTarget,
    },
    /// 正在出安装计划(URL 来源已经先发过计划请求)。
    Planning {
        source: AppSource,
    },
    /// 计划已展示,等用户批准。
    Reviewing {
        plan: Box<InstallPlan>,
        source: AppSource,
    },
    Installing {
        plan: Box<InstallPlan>,
    },
    /// 流程内的失败(原因给人看);「知道了」回到 `Idle`。
    Failed {
        message: String,
    },
    /// 卸载确认:选"保留数据"还是"连数据一起删"。
    ConfirmUninstall {
        id: String,
        name: String,
    },
    /// 回滚确认:行内二次确认(不用 Notice)。
    ConfirmRollback {
        id: String,
        name: String,
        to: bytehost_apps::id::Version,
    },
    /// 正在出一份运行时安装计划(不下载任何东西)。
    RuntimePlanning {
        runtime: ManagedRuntime,
    },
    /// 运行时安装计划已展示,等用户批准。
    RuntimeReviewing {
        plan: Box<RuntimeInstallPlan>,
    },
    /// 卸载某个受管运行时版本的确认。
    ConfirmRuntimeUninstall {
        runtime: ManagedRuntime,
        version: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActKind {
    Stop,
    Uninstall,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// 切到「应用」页(或设置弹窗以它打开)。
    Opened,
    ProbesLoaded(Result<Vec<RuntimeProbe>, Failure>),
    ListLoaded(Result<Vec<AppSummary>, Failure>),
    InstallClicked,
    /// 切换安装来源(本机目录/本机压缩包/URL);切到 Url 不丢已填的输入,但会清掉上一次的计划。
    SourceChoiceChanged(SourceChoice),
    /// 点「安装应用…」(来源=本机压缩包):意图与 `InstallClicked` 同,只是弹的是"选文件"对话框。
    PickArchive,
    /// URL 输入框草稿变化(宿主 text input 每次给全量当前字符串)。
    UrlChanged(String),
    /// sha256 输入框草稿变化(同上;空 = 不钉死)。
    Sha256Changed(String),
    /// 点「获取计划」(来源=URL):客户端先行校验通过后才发 `Effect::Plan`。
    UrlPlanClicked,
    /// 选目录/选压缩包对话框的结果;`None` = 用户取消。
    SourcePicked(Option<PathBuf>),
    PlanLoaded(Result<Box<InstallPlan>, Failure>),
    ApproveClicked,
    /// 取消/关闭当前对话步骤(审批中取消、失败页「知道了」、卸载确认取消)。
    FlowDismissed,
    InstallDone(Result<(), Failure>),
    StopClicked(String),
    /// 点某应用行的「回滚到 Y」:进入行内二次确认。
    RollbackClicked(String),
    RollbackConfirmed(String),
    RollbackCancelled,
    RolledBack(String, Result<(), Failure>),
    UninstallClicked(String),
    UninstallConfirmed(UninstallMode),
    ActionDone(String, ActKind, Result<(), Failure>),
    /// 点「安装…」:为一个运行时出一份安装计划。
    RuntimeInstallClicked(ManagedRuntime),
    RuntimePlanLoaded(Result<Box<RuntimeInstallPlan>, Failure>),
    RuntimeApproveClicked,
    /// 安装请求已发出(成功表示服务端接受了任务,进度靠轮询)。
    RuntimeInstallStarted(Result<(), Failure>),
    RuntimeUninstallClicked(ManagedRuntime, String),
    RuntimeUninstallConfirmed,
    RuntimeUninstallDone(Result<(), Failure>),
    /// 安装进行中的定时刷新(由 `Effect::ProbeAfter` 转回)。
    PollProbes,
    /// 展开某应用的日志查看器(同一时刻只展开一个)。
    ShowLogs(String),
    /// 收起当前展开的日志查看器。
    HideLogs,
    /// 日志读取的结果。
    LogsLoaded(String, Result<(String, bool), Failure>),
    /// 用户滚动了日志查看器:`true` = 当前贴底。
    LogsScrolled(String, bool),
    /// 定时刷新展开着的日志查看器(由调用方按 [`crate::logs::REFRESH_INTERVAL`] 排期转回)。
    Tick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Probe,
    List,
    /// 窗口层弹原生选目录对话框(`window_events` 拦截 `InstallClicked` 执行,这里只是记录意图)。
    PickSource,
    /// 窗口层弹原生选文件对话框(过滤 `.zip`/`.tar.gz`/`.tgz`;`window_events` 拦截 `PickArchive` 执行)。
    PickArchive,
    Plan(AppSource),
    Install {
        approved: Box<ApprovedInstallPlan>,
        source: AppSource,
    },
    Stop(String),
    /// 把某应用回滚到上一版(A6g)。
    Rollback(String),
    Uninstall(String, UninstallMode),
    /// 为一个运行时出一份安装计划(不下载)。
    RuntimePlan(ManagedRuntime),
    /// 安装一份已批准的运行时计划。
    InstallRuntime(Box<RuntimeInstallPlan>),
    /// 卸载某个受管运行时版本。
    UninstallRuntime(ManagedRuntime, String),
    /// 延时后再探测一次(安装进行中的进度轮询)。
    ProbeAfter(std::time::Duration),
    /// 读某应用的日志末尾(`max_lines` 由状态机定)。
    FetchLogs(String, u32),
    /// 已安装集合/运行状态变了:通知主窗口的应用宿主立刻刷新列表(同步图标栏)。
    HostChanged,
    Notice {
        level: NoticeLevel,
        text: String,
        key: String,
    },
}

#[derive(Debug, Default)]
pub struct InstallFlowState {
    pub probes: Load<Vec<RuntimeProbe>>,
    pub apps: Load<Vec<AppSummary>>,
    pub flow: Flow,
    /// 当前选中的安装来源(A6h)。
    pub source_choice: SourceChoice,
    /// URL 输入框的原始字符串(切来源不清,失败后保留以便修改重试)。
    pub url_input: String,
    /// sha256 输入框的原始字符串(空 = 不钉死)。
    pub sha_input: String,
    /// 客户端先行的内联校验提示(URL/sha256 格式);真正的校验在服务端。
    pub url_error: Option<String>,
    acting: std::collections::HashMap<String, ActKind>,
    /// 已经报过结果(成功或失败 Notice)的运行时任务——避免每次轮询重复弹。
    reported_jobs: std::collections::HashSet<ManagedRuntime>,
    /// 已发出但还没回结果的受管运行时卸载(去重)。
    uninstalling: std::collections::HashSet<(ManagedRuntime, String)>,
    /// 每个应用的日志查看器(共享状态机);同一时刻只展开一个(`expanded_log`)。
    logs: std::collections::HashMap<String, LogsState>,
    expanded_log: Option<String>,
    /// 已经有一拍 `Tick` 定时器在路上:同一时刻只允许一条定时链,否则每条消息都多起一条、越积越多。
    tick_armed: bool,
}

impl InstallFlowState {
    pub fn update(&mut self, msg: Message, now_ms: u64) -> Vec<Effect> {
        match msg {
            Message::Opened => {
                self.probes = Load::Loading;
                self.apps = Load::Loading;
                vec![Effect::Probe, Effect::List]
            }
            Message::ProbesLoaded(result) => {
                self.probes = match result {
                    Ok(p) => Load::Loaded(p),
                    Err(f) => Load::Failed(f.text().to_owned()),
                };
                self.probe_side_effects()
            }
            Message::ListLoaded(result) => {
                self.apps = match result {
                    Ok(a) => Load::Loaded(a),
                    Err(f) => Load::Failed(f.text().to_owned()),
                };
                // 应用被卸载:清掉它的日志查看器(含当前展开的)。
                if let Load::Loaded(apps) = &self.apps {
                    let alive: std::collections::HashSet<&str> =
                        apps.iter().map(|a| a.id.as_str()).collect();
                    self.logs.retain(|id, _| alive.contains(id.as_str()));
                    if let Some(open) = self.expanded_log.as_deref()
                        && !alive.contains(open)
                    {
                        self.expanded_log = None;
                    }
                }
                Vec::new()
            }
            Message::SourceChoiceChanged(choice) => {
                self.source_choice = choice;
                self.url_error = None;
                // 切换来源会清掉上一次的计划/审批卡,避免"审批的是 A 来源、安装的是 B 来源";
                // 在途的规划/安装请求已经发出,不能撤,保持原样。已填的输入不清。
                if matches!(self.flow, Flow::Reviewing { .. } | Flow::Failed { .. }) {
                    self.flow = Flow::Idle;
                }
                Vec::new()
            }
            Message::InstallClicked | Message::PickArchive => {
                if self.flow != Flow::Idle {
                    return Vec::new();
                }
                let choice = self.source_choice;
                let Some(target) = choice.pick_target() else {
                    // 来源=URL 时不该走"弹对话框"这条路;按下即当"获取计划"处理。
                    return self.url_plan();
                };
                self.flow = Flow::Picking { target };
                vec![match target {
                    PickTarget::Dir => Effect::PickSource,
                    PickTarget::Archive => Effect::PickArchive,
                }]
            }
            Message::UrlChanged(s) => {
                self.url_input = s;
                // 用户开始改输入:清掉上一次的提示,等点「获取计划」再校验。
                self.url_error = None;
                Vec::new()
            }
            Message::Sha256Changed(s) => {
                self.url_error = sha_format_error(&s);
                self.sha_input = s;
                Vec::new()
            }
            Message::UrlPlanClicked => self.url_plan(),
            Message::SourcePicked(picked) => {
                let Flow::Picking { target } = self.flow else {
                    return Vec::new();
                };
                match picked {
                    None => {
                        self.flow = Flow::Idle;
                        Vec::new()
                    }
                    Some(path) => {
                        let source = target.to_source(path);
                        self.flow = Flow::Planning {
                            source: source.clone(),
                        };
                        vec![Effect::Plan(source)]
                    }
                }
            }
            Message::PlanLoaded(result) => {
                let Flow::Planning { source } = &self.flow else {
                    return Vec::new();
                };
                let source = source.clone();
                self.flow = match result {
                    Ok(plan) => Flow::Reviewing { plan, source },
                    Err(f) => Flow::Failed {
                        message: f.text().to_owned(),
                    },
                };
                Vec::new()
            }
            Message::ApproveClicked => {
                let Flow::Reviewing { plan, source } = &self.flow else {
                    return Vec::new();
                };
                let (plan, source) = (plan.clone(), source.clone());
                // 批准的就是展示的这一份:不重算、不改写。
                let approved = plan.as_ref().clone().approve(Approval {
                    approver: APPROVER.into(),
                    approved_ms: now_ms,
                });
                self.flow = Flow::Installing { plan };
                vec![Effect::Install {
                    approved: Box::new(approved),
                    source,
                }]
            }
            Message::FlowDismissed => {
                // 在途的规划/安装不能取消(请求已发出);其余步骤都可以关。
                if matches!(
                    self.flow,
                    Flow::Reviewing { .. }
                        | Flow::Failed { .. }
                        | Flow::ConfirmUninstall { .. }
                        | Flow::ConfirmRollback { .. }
                        | Flow::RuntimeReviewing { .. }
                        | Flow::ConfirmRuntimeUninstall { .. }
                ) {
                    self.flow = Flow::Idle;
                }
                Vec::new()
            }
            Message::InstallDone(result) => {
                let Flow::Installing { plan } = &self.flow else {
                    return Vec::new();
                };
                match result {
                    Ok(()) => {
                        let text = format!("已安装应用 {}", plan.name);
                        let key = format!("apps:install:{}", plan.app_id.as_str());
                        self.flow = Flow::Idle;
                        vec![
                            Effect::Notice {
                                level: NoticeLevel::Success,
                                text,
                                key,
                            },
                            Effect::List,
                            Effect::HostChanged,
                        ]
                    }
                    Err(f) => {
                        self.flow = Flow::Failed {
                            message: format!("安装失败:{}", f.text()),
                        };
                        Vec::new()
                    }
                }
            }
            Message::StopClicked(id) => {
                if self.acting.contains_key(&id) || self.flow != Flow::Idle {
                    return Vec::new();
                }
                self.acting.insert(id.clone(), ActKind::Stop);
                vec![Effect::Stop(id)]
            }
            Message::RollbackClicked(id) => {
                if self.acting.contains_key(&id) || self.flow != Flow::Idle {
                    return Vec::new();
                }
                let Some(to) = self.previous_version(&id) else {
                    return Vec::new();
                };
                let name = self.display_name(&id);
                self.flow = Flow::ConfirmRollback { id, name, to };
                Vec::new()
            }
            Message::RollbackCancelled => {
                if matches!(self.flow, Flow::ConfirmRollback { .. }) {
                    self.flow = Flow::Idle;
                }
                Vec::new()
            }
            Message::RollbackConfirmed(id) => {
                let Flow::ConfirmRollback { id: pending, .. } = &self.flow else {
                    return Vec::new();
                };
                if *pending != id {
                    return Vec::new();
                }
                self.flow = Flow::Idle;
                vec![Effect::Rollback(id)]
            }
            Message::RolledBack(id, result) => {
                let mut effects = Vec::new();
                if let Err(f) = result {
                    effects.push(Effect::Notice {
                        level: NoticeLevel::Error,
                        text: format!("回滚应用 {} 失败:{}", self.display_name(&id), f.text()),
                        key: format!("apps:rollback:{id}"),
                    });
                }
                // 成败都刷新:列表反映真相,图标栏同步。
                effects.push(Effect::List);
                effects.push(Effect::HostChanged);
                effects
            }
            Message::UninstallClicked(id) => {
                if self.acting.contains_key(&id) || self.flow != Flow::Idle {
                    return Vec::new();
                }
                let name = self.display_name(&id);
                self.flow = Flow::ConfirmUninstall { id, name };
                Vec::new()
            }
            Message::UninstallConfirmed(mode) => {
                let Flow::ConfirmUninstall { id, .. } = &self.flow else {
                    return Vec::new();
                };
                let id = id.clone();
                self.flow = Flow::Idle;
                self.acting.insert(id.clone(), ActKind::Uninstall);
                vec![Effect::Uninstall(id, mode)]
            }
            Message::ActionDone(id, kind, result) => {
                self.acting.remove(&id);
                let mut effects = Vec::new();
                if let Err(f) = result {
                    let verb = match kind {
                        ActKind::Stop => "停止",
                        ActKind::Uninstall => "卸载",
                    };
                    effects.push(Effect::Notice {
                        level: NoticeLevel::Error,
                        text: format!("{verb}应用 {} 失败:{}", self.display_name(&id), f.text()),
                        key: format!("apps:act:{id}"),
                    });
                }
                // 成败都刷新:列表反映真相,图标栏同步。
                effects.push(Effect::List);
                effects.push(Effect::HostChanged);
                effects
            }
            Message::RuntimeInstallClicked(runtime) => {
                if self.flow != Flow::Idle {
                    return Vec::new();
                }
                self.flow = Flow::RuntimePlanning { runtime };
                vec![Effect::RuntimePlan(runtime)]
            }
            Message::RuntimePlanLoaded(result) => {
                let Flow::RuntimePlanning { .. } = &self.flow else {
                    return Vec::new();
                };
                self.flow = match result {
                    Ok(plan) => Flow::RuntimeReviewing { plan },
                    // 流程内的失败:留在对话里,不弹 Notice(与安装计划一致)。
                    Err(f) => Flow::Failed {
                        message: f.text().to_owned(),
                    },
                };
                Vec::new()
            }
            Message::RuntimeApproveClicked => {
                let Flow::RuntimeReviewing { plan } = &self.flow else {
                    return Vec::new();
                };
                // 批准的就是展示的这一份:原封不动发回去,服务端会重算核对。
                let plan = plan.clone();
                self.flow = Flow::Idle;
                vec![
                    Effect::InstallRuntime(plan),
                    Effect::ProbeAfter(std::time::Duration::from_millis(500)),
                ]
            }
            Message::RuntimeInstallStarted(result) => match result {
                Ok(()) => Vec::new(),
                Err(f) => vec![Effect::Notice {
                    level: NoticeLevel::Error,
                    text: format!("安装运行时失败:{}", f.text()),
                    key: "runtimes:install".into(),
                }],
            },
            Message::RuntimeUninstallClicked(runtime, version) => {
                if self.flow != Flow::Idle
                    || self.uninstalling.contains(&(runtime, version.clone()))
                {
                    return Vec::new();
                }
                self.flow = Flow::ConfirmRuntimeUninstall { runtime, version };
                Vec::new()
            }
            Message::RuntimeUninstallConfirmed => {
                let Flow::ConfirmRuntimeUninstall { runtime, version } = &self.flow else {
                    return Vec::new();
                };
                let (runtime, version) = (*runtime, version.clone());
                self.flow = Flow::Idle;
                self.uninstalling.insert((runtime, version.clone()));
                vec![Effect::UninstallRuntime(runtime, version)]
            }
            Message::RuntimeUninstallDone(result) => {
                // 哪个版本已经不在状态里记了(可能同时多个);都清掉,要求刷新即可。
                for key in self.uninstalling.drain() {
                    self.reported_jobs.remove(&key.0);
                }
                let effects = vec![Effect::Probe];
                match result {
                    Ok(()) => effects,
                    Err(f) => {
                        let mut effects = effects;
                        effects.insert(
                            0,
                            Effect::Notice {
                                level: NoticeLevel::Error,
                                text: format!("卸载运行时失败:{}", f.text()),
                                key: "runtimes:uninstall".into(),
                            },
                        );
                        effects
                    }
                }
            }
            Message::PollProbes => vec![Effect::Probe],
            Message::ShowLogs(id) => {
                // 同一时刻只展开一个:先把别的收起。
                if let Some(prev) = self.expanded_log.take()
                    && prev != id
                {
                    self.logs.entry(prev).or_default().hide();
                }
                self.expanded_log = Some(id.clone());
                self.logs
                    .entry(id.clone())
                    .or_default()
                    .show(std::time::Instant::now())
                    .into_iter()
                    .map(|_| Effect::FetchLogs(id.clone(), crate::logs::FETCH_LINES))
                    .collect()
            }
            Message::HideLogs => {
                if let Some(prev) = self.expanded_log.take() {
                    self.logs.entry(prev).or_default().hide();
                }
                Vec::new()
            }
            Message::LogsLoaded(id, result) => {
                let mapped = result.map_err(|f| f.text().to_owned());
                self.logs
                    .entry(id)
                    .or_default()
                    .loaded(std::time::Instant::now(), mapped);
                Vec::new()
            }
            Message::LogsScrolled(id, at_bottom) => {
                self.logs.entry(id).or_default().on_scrolled(at_bottom);
                Vec::new()
            }
            Message::Tick => {
                self.tick_armed = false;
                self.tick_logs(std::time::Instant::now())
            }
        }
    }

    /// URL 来源点「获取计划」:客户端先校验格式,通过才发 `Effect::Plan`;失败把原因留在
    /// `url_error`(内联显示,不弹 Notice),并保留已填的输入供修改重试。
    fn url_plan(&mut self) -> Vec<Effect> {
        if self.flow != Flow::Idle {
            return Vec::new();
        }
        let url = self.url_input.trim().to_owned();
        if url.is_empty() {
            self.url_error = Some("请填写 https 压缩包地址".to_owned());
            return Vec::new();
        }
        if !url.starts_with("https://") {
            self.url_error = Some("地址必须以 https:// 开头(不接受 http)".to_owned());
            return Vec::new();
        }
        let sha256 = match self.sha_input.trim() {
            "" => None,
            s => match normalize_sha256_client(s) {
                Ok(v) => Some(v),
                Err(e) => {
                    self.url_error = Some(e);
                    return Vec::new();
                }
            },
        };
        self.url_error = None;
        let source = AppSource::Url { url, sha256 };
        self.flow = Flow::Planning {
            source: source.clone(),
        };
        vec![Effect::Plan(source)]
    }

    /// 取走"把滚动钉到底部"的一次性请求:返回待滚到底的应用 id(消费即复位)。
    pub fn take_log_scroll(&mut self) -> Option<String> {
        let id = self.expanded_log.clone()?;
        let want = self.logs.get_mut(&id).is_some_and(LogsState::take_scroll);
        want.then_some(id)
    }

    /// 有展开着的日志查看器吗(调用方据此决定要不要排下一次 `Tick`)。
    #[cfg(test)]
    pub fn log_tick_wanted(&self) -> bool {
        self.expanded_log.is_some()
    }

    /// 要不要现在排一拍 `Tick`:有展开的查看器、且还没有定时器在路上。返回 `true` 即视为已排
    /// (调用方必须真的排);`Tick` 到达时复位,再按需续排。
    pub fn arm_tick(&mut self) -> bool {
        if self.expanded_log.is_some() && !self.tick_armed {
            self.tick_armed = true;
            true
        } else {
            false
        }
    }

    /// 到点刷新展开着的日志查看器(设置页可见时才调用),返回要执行的 `FetchLogs`。
    pub fn tick_logs(&mut self, now: std::time::Instant) -> Vec<Effect> {
        let Some(id) = self.expanded_log.clone() else {
            return Vec::new();
        };
        self.logs
            .entry(id.clone())
            .or_default()
            .tick(now)
            .into_iter()
            .map(|_| Effect::FetchLogs(id.clone(), crate::logs::FETCH_LINES))
            .collect()
    }

    /// 某应用日志查看器的当前状态。
    pub fn logs_view(&self, id: &str) -> LogsView {
        self.logs
            .get(id)
            .map(|s| s.view())
            .unwrap_or(LogsView::Hidden)
    }

    /// 该应用是否正展开日志查看器。
    pub fn is_log_open(&self, id: &str) -> bool {
        self.expanded_log.as_deref() == Some(id)
    }

    /// `ProbesLoaded` 之后的副作用:安装进行中继续轮询;刚完成的任务报一次结果。
    fn probe_side_effects(&mut self) -> Vec<Effect> {
        let Load::Loaded(probes) = &self.probes else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        let mut any_running = false;
        for probe in probes {
            let Some(job) = &probe.job else { continue };
            if !job.finished {
                any_running = true;
                continue;
            }
            if self.reported_jobs.contains(&job.runtime) {
                continue;
            }
            self.reported_jobs.insert(job.runtime);
            let name = managed_runtime_label(job.runtime);
            match &job.failed {
                Some(why) => effects.push(Effect::Notice {
                    level: NoticeLevel::Error,
                    text: format!("{name} 运行时安装失败:{why}"),
                    key: format!("runtimes:job:{name}"),
                }),
                None => {
                    effects.push(Effect::Notice {
                        level: NoticeLevel::Success,
                        text: format!("{name} 运行时已安装"),
                        key: format!("runtimes:job:{name}"),
                    });
                    effects.push(Effect::HostChanged);
                }
            }
        }
        if any_running {
            effects.push(Effect::ProbeAfter(std::time::Duration::from_secs(1)));
        }
        effects
    }

    pub fn is_acting(&self, id: &str) -> Option<ActKind> {
        self.acting.get(id).copied()
    }

    fn display_name(&self, id: &str) -> String {
        match &self.apps {
            Load::Loaded(apps) => apps
                .iter()
                .find(|a| a.id.as_str() == id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| id.to_owned()),
            _ => id.to_owned(),
        }
    }

    /// 这个受管运行时的这个版本是否正在卸载(视图据此禁用按钮并显示"处理中…")。
    pub fn runtime_uninstalling(&self, runtime: ManagedRuntime, version: &str) -> bool {
        self.uninstalling.contains(&(runtime, version.to_owned()))
    }

    /// 某应用当前是否在运行(升级审批卡据此决定要不要提醒"会短暂中断")。
    pub fn observed_running(&self, id: &str) -> bool {
        match &self.apps {
            Load::Loaded(apps) => apps
                .iter()
                .find(|a| a.id.as_str() == id)
                .is_some_and(|a| a.observed == ObservedState::Running),
            _ => false,
        }
    }

    /// 某应用可回滚到的上一版;没有(全新安装 / 已回滚过)则 `None`。
    fn previous_version(&self, id: &str) -> Option<bytehost_apps::id::Version> {
        match &self.apps {
            Load::Loaded(apps) => apps
                .iter()
                .find(|a| a.id.as_str() == id)
                .and_then(|a| a.previous_version),
            _ => None,
        }
    }
}

/// 设置窗口已经关掉(状态没了)时到达的安装/停止/卸载结果:状态机已不存在,但事情照样发生了——
/// 仍要通知主窗口刷新应用列表(同步图标栏),并把失败/成功告诉用户(Notice)。其余消息在没有窗口时无意义,忽略。
pub fn orphan_result_effects(msg: &Message) -> Vec<Effect> {
    match msg {
        Message::InstallDone(Ok(())) => vec![
            Effect::Notice {
                level: NoticeLevel::Success,
                text: "应用已安装".into(),
                key: "apps:install".into(),
            },
            Effect::HostChanged,
        ],
        Message::InstallDone(Err(f)) => vec![
            Effect::Notice {
                level: NoticeLevel::Error,
                text: format!("安装失败:{}", f.text()),
                key: "apps:install".into(),
            },
            Effect::HostChanged,
        ],
        Message::ActionDone(id, kind, result) => {
            let mut effects = Vec::new();
            if let Err(f) = result {
                let verb = match kind {
                    ActKind::Stop => "停止",
                    ActKind::Uninstall => "卸载",
                };
                effects.push(Effect::Notice {
                    level: NoticeLevel::Error,
                    text: format!("{verb}应用 {id} 失败:{}", f.text()),
                    key: format!("apps:act:{id}"),
                });
            }
            effects.push(Effect::HostChanged);
            effects
        }
        Message::RolledBack(id, result) => {
            let mut effects = Vec::new();
            if let Err(f) = result {
                effects.push(Effect::Notice {
                    level: NoticeLevel::Error,
                    text: format!("回滚应用 {id} 失败:{}", f.text()),
                    key: format!("apps:rollback:{id}"),
                });
            }
            effects.push(Effect::HostChanged);
            effects
        }
        Message::RuntimeInstallStarted(Err(f)) => vec![Effect::Notice {
            level: NoticeLevel::Error,
            text: format!("安装运行时失败:{}", f.text()),
            key: "runtimes:install".into(),
        }],
        Message::RuntimeUninstallDone(Err(f)) => vec![Effect::Notice {
            level: NoticeLevel::Error,
            text: format!("卸载运行时失败:{}", f.text()),
            key: "runtimes:uninstall".into(),
        }],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// 展示用的纯函数(可测):计划 → 要给用户看的行;运行时探测 → 一行文案;观察态 → 文案。
// ---------------------------------------------------------------------------------------------

/// sha256 输入框的即时格式提示:空 = `None`(不钉死);非法 = 内联红字;合法 = `None`。
/// 只做格式检查(客户端先行校验只是体验),真正的匹配在服务端。
pub fn sha_format_error(input: &str) -> Option<String> {
    match input.trim() {
        "" => None,
        s => normalize_sha256_client(s).err(),
    }
}

/// 客户端版的 sha256 规范化(trim + 小写 + 64 hex 校验)。与 `bytehost_apps::source::normalize_sha256`
/// 同规则,但那份在 `server` feature 下,GUI 拿不到;这里只用于先行提示,真正的校验在服务端。
fn normalize_sha256_client(input: &str) -> Result<String, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s)
    } else {
        Err("sha256 需要是 64 位十六进制字符".to_owned())
    }
}

pub fn provenance_label(p: Provenance) -> &'static str {
    match p {
        Provenance::Local => "本地目录",
        Provenance::AgentGenerated => "Agent 生成",
        Provenance::ThirdParty => "第三方来源",
    }
}

pub fn trust_label(t: TrustLevel) -> &'static str {
    match t {
        TrustLevel::Trusted => "受信任",
        TrustLevel::Untrusted => "不受信任",
    }
}

pub fn enforcement_label(e: Option<Enforcement>) -> &'static str {
    match e {
        Some(Enforcement::Enforced) => "由宿主强制",
        Some(Enforcement::Advisory) => "仅声明,不强制",
        Some(Enforcement::Unsupported) => "此运行方式不支持",
        None => "未知(按不强制处理)",
    }
}

fn permission_label(key: PermissionKey) -> &'static str {
    match key {
        PermissionKey::NetworkOutbound => "出站网络",
        PermissionKey::FilesystemData => "应用数据目录",
        PermissionKey::Clipboard => "剪贴板",
        PermissionKey::Downloads => "下载",
        PermissionKey::Popups => "弹窗",
    }
}

/// 权限取值的中文(来自 `Permissions::level` 的稳定英文标签;未知标签原样显示,不吞)。
fn level_label(raw: &str) -> String {
    match raw {
        "none" => "无",
        "any" => "任意",
        "read" => "只读",
        "read_write" => "读写",
        "deny" => "禁止",
        "user_confirm" => "需确认",
        "allow" => "允许",
        other => return other.to_owned(),
    }
    .to_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermLine {
    pub label: &'static str,
    /// 例如 `无 → 任意`。
    pub change: String,
    /// 申请比当前授予更多(升级时需要重新确认)。
    pub escalation: bool,
    pub enforcement: &'static str,
}

/// 审批卡"来源"区块(A6h):`(项, 值)` 事实行 + 金色警告(网络/重定向/未钉死哈希)。
#[derive(Debug, Clone, PartialEq)]
pub struct SourceDisclosure {
    pub rows: Vec<(&'static str, String)>,
    pub warnings: Vec<String>,
    /// 来自网络(URL 来源):用醒目颜色标"不可信"。
    pub from_network: bool,
}

/// 安装计划里要展示给用户的全部内容。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanView {
    pub title: String,
    /// `(项, 值)` 事实行:来源、信任、运行方式、版本变化。
    pub facts: Vec<(&'static str, String)>,
    /// 升级时要额外说清的提示(A6g):自动重启、起不来会回滚。
    pub notices: Vec<String>,
    /// 安装/运行时会执行什么(原样展示)。
    pub will_run: Vec<String>,
    pub permissions: Vec<PermLine>,
    /// 来源披露(A6h)。
    pub source: SourceDisclosure,
}

/// 把 `plan.source_info` 映射成审批卡"来源"区块(纯函数,可测)。
/// `runtime_kind` 用来判断进程型应用从本机压缩包来时要加"内容已解压校验"一行。
pub fn source_disclosure(info: &SourceInfo, runtime_kind: &str) -> SourceDisclosure {
    let mut rows: Vec<(&'static str, String)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let kind_label = match info.kind.as_str() {
        "local_dir" => "本机目录",
        "archive" => "本机压缩包",
        "url" => "网络(https)",
        _ => "未知",
    };
    rows.push(("来源", kind_label.to_owned()));
    if !info.display.is_empty() {
        rows.push(("位置", info.display.clone()));
    }
    if let Some(sha) = &info.archive_sha256 {
        // 完整 64 位,不截断(可复制)。
        rows.push(("压缩包 SHA-256", sha.clone()));
    }
    if let Some(bytes) = info.archive_bytes {
        rows.push(("压缩包大小", format_bytes(bytes)));
    }
    if let Some(top) = &info.stripped_top_dir {
        rows.push(("顶层目录", format!("已剥掉 {top}")));
    }
    let from_network = info.kind == "url";
    if from_network {
        rows.push((
            "完整性",
            if info.pinned {
                "已匹配你提供的 sha256".to_owned()
            } else {
                "未提供期望 sha256".to_owned()
            },
        ));
        if info.pinned {
            warnings.push("已匹配你提供的 sha256".to_owned());
        } else {
            warnings.push("未提供期望 sha256,以上哈希是本次下载实际算出的".to_owned());
        }
        // effective_host 与请求主机不同 → 提示被重定向。
        if let Some(req_host) = host_of_display(&info.display)
            && let Some(eff) = &info.effective_host
            && !eff.eq_ignore_ascii_case(&req_host)
        {
            warnings.push(format!("已重定向到 {eff}"));
        }
    }
    // 进程型应用从本机压缩包来:命令披露之外,另加一行说明内容已解压校验。
    if info.kind == "archive" && runtime_kind != "static_web" {
        warnings.push("来自压缩包,内容已解压校验".to_owned());
    }
    SourceDisclosure {
        rows,
        warnings,
        from_network,
    }
}

/// 取 `https://host/path` 里的 host(用于跟 effective_host 比较)。
fn host_of_display(display: &str) -> Option<String> {
    let rest = display
        .strip_prefix("https://")
        .or_else(|| display.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // 去掉端口与 userinfo(这里 display 已由服务端剥掉查询串,保守再处理一次)。
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// 人类可读的字节数(1 位小数)。
fn format_bytes(bytes: u64) -> String {
    const UNIT: f64 = 1024.0;
    let b = bytes as f64;
    if b < UNIT {
        format!("{bytes} B")
    } else if b < UNIT * UNIT {
        format!("{:.1} KiB", b / UNIT)
    } else if b < UNIT * UNIT * UNIT {
        format!("{:.1} MiB", b / (UNIT * UNIT))
    } else {
        format!("{:.1} GiB", b / (UNIT * UNIT * UNIT))
    }
}

/// `running` = 被升级/安装的应用当前是否在运行(决定要不要提醒"会短暂中断")。
pub fn plan_view(plan: &InstallPlan, running: bool) -> PlanView {
    let version = match &plan.upgrading_from {
        Some(from) => format!("{from} → {}", plan.version),
        None => plan.version.to_string(),
    };
    let mut notices = Vec::new();
    if plan.upgrading_from.is_some() {
        if running {
            notices.push("升级会短暂中断并自动重启。".to_owned());
        }
        notices.push("新版本起不来会自动回到上一版。".to_owned());
    }
    let permissions = plan
        .permission_diff
        .iter()
        .map(|c| PermLine {
            label: permission_label(c.key),
            change: format!("{} → {}", level_label(&c.from), level_label(&c.to)),
            escalation: c.escalation,
            enforcement: enforcement_label(
                plan.enforcement
                    .iter()
                    .find(|e| e.key == c.key)
                    .map(|e| e.enforcement),
            ),
        })
        .collect();
    PlanView {
        title: format!("{}({})", plan.name, plan.app_id.as_str()),
        facts: vec![
            ("版本", version),
            ("来源", provenance_label(plan.provenance).to_owned()),
            ("信任", trust_label(plan.trust).to_owned()),
            ("运行方式", plan.runtime_kind.clone()),
        ],
        notices,
        will_run: plan.will_run.clone(),
        permissions,
        source: source_disclosure(&plan.source_info, &plan.runtime_kind),
    }
}

/// 一行运行时探测文案 `(名称, 状态文字, 是否可用)`。
pub fn runtime_line(probe: &RuntimeProbe) -> (String, String, bool) {
    let name = match probe.runtime.as_str() {
        "docker" => "Docker",
        "node" => "Node.js",
        "python" => "Python",
        other => other,
    }
    .to_owned();
    match &probe.availability {
        RuntimeAvailability::Available { detail } => (name, format!("可用 · {detail}"), true),
        RuntimeAvailability::Unavailable { detail } => {
            (name, format!("已安装但当前不可用 · {detail}"), false)
        }
        RuntimeAvailability::NotInstalled => (name, "未安装".to_owned(), false),
    }
}

pub fn observed_label(state: &ObservedState) -> String {
    match state {
        ObservedState::Running => "运行中".into(),
        ObservedState::Stopped | ObservedState::Installed => "已停止".into(),
        ObservedState::Preparing | ObservedState::Starting | ObservedState::Updating => {
            "启动中".into()
        }
        ObservedState::Stopping | ObservedState::Uninstalling => "停止中".into(),
        ObservedState::NotInstalled => "未安装".into(),
        ObservedState::Failed { reason, .. } => format!("已退出 · {reason}"),
    }
}

/// 一行"回滚过"的持久说明(A6g):自动回滚点出原因,手动回滚只说落到哪版。
pub fn rollback_note_label(note: &RollbackNote) -> String {
    // 因权限提升而跳过的自动回滚:`from == to`,没有发生回滚,别说"回滚到"。
    if note.from == note.to {
        return format!("未自动回滚:{}", note.reason);
    }
    if note.automatic {
        format!("已从 {} 自动回滚到 {}:{}", note.from, note.to, note.reason)
    } else {
        format!("已回滚到 {}", note.to)
    }
}

/// 受管运行时的中文名(给按钮/通知用)。
pub fn managed_runtime_label(runtime: ManagedRuntime) -> &'static str {
    match runtime {
        ManagedRuntime::Node => "Node.js",
        ManagedRuntime::Python => "Python",
    }
}

/// 探测的 `runtime` 键 → 受管运行时(仅 node/python;其他返回 `None`)。
pub fn managed_runtime_of(key: &str) -> Option<ManagedRuntime> {
    match key {
        "node" => Some(ManagedRuntime::Node),
        "python" => Some(ManagedRuntime::Python),
        _ => None,
    }
}

/// 运行时行:一个运行时一行(名称、探测状态、受管版本、安装/卸载按钮、进度)。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeRow {
    pub probe_runtime: String,
    pub name: String,
    /// 探测到的可用性文案(如 `可用 · v20` / `未安装`)。
    pub status: String,
    /// 已装的受管版本(新→旧),带 `uninstall` 的按钮文案所需。
    pub managed: Vec<String>,
    /// 此平台可自动安装(有固定版本)。
    pub installable: bool,
    /// 显示的进度文案(有任务且未完成时),如 `下载 node · 45%` 或 `下载 node · 12.3 MB`。
    pub progress: Option<String>,
}

/// 把探测结果映射成运行时行。每个探测一行;未受管不支持的运行时(docker)`installable=false`。
#[cfg(test)]
pub fn runtime_rows(probes: &[RuntimeProbe]) -> Vec<RuntimeRow> {
    probes
        .iter()
        .map(|probe| {
            let (name, status, _) = runtime_line(probe);
            RuntimeRow {
                probe_runtime: probe.runtime.clone(),
                name,
                status,
                managed: probe.managed.clone(),
                installable: probe.installable,
                progress: probe
                    .job
                    .as_ref()
                    .filter(|j| !j.finished)
                    .map(progress_text),
            }
        })
        .collect()
}

/// 一个进行中的任务 → 进度文案;`total` 未知时只显示已下载 MB。
pub fn progress_text(job: &bytehost_apps::proto::RuntimeJob) -> String {
    match job.total {
        Some(total) if total > 0 => {
            let pct = (job.done.saturating_mul(100) / total).min(100);
            format!("{} · {}%", job.phase, pct)
        }
        _ => format!("{} · {:.1} MB", job.phase, job.done as f64 / 1_048_576.0),
    }
}

/// 判断某探测对应的运行时是否正在安装(未完成的任务)。
pub fn probe_is_installing(probe: &RuntimeProbe) -> bool {
    probe.job.as_ref().is_some_and(|j| !j.finished)
}

/// 审批页逐行内容:`(标签, 值)`——来源 / 固定版本 / SHA-256 / 安装位置 / 将要做的事。
pub fn plan_lines(plan: &RuntimeInstallPlan) -> Vec<(String, String)> {
    let mut lines: Vec<(String, String)> = Vec::new();
    lines.push((
        "运行时".into(),
        managed_runtime_label(plan.runtime).to_owned(),
    ));
    for (name, version) in &plan.versions {
        lines.push((format!("版本({name})"), version.clone()));
    }
    for d in &plan.downloads {
        lines.push((format!("来源({})", d.what), d.url.clone()));
        let sha = match &d.sha256 {
            Some(s) => s.clone(),
            None => "由 uv 内置哈希校验".to_owned(),
        };
        lines.push((format!("SHA-256({})", d.what), sha));
        if !d.note.is_empty() {
            lines.push((format!("说明({})", d.what), d.note.clone()));
        }
    }
    lines.push(("安装位置".into(), plan.dest.clone()));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_apps::id::{AppId, Version};
    use bytehost_apps::permissions::{Access, Gate, Outbound, Permissions, diff_permissions};
    use bytehost_apps::plan::EnforcementEntry;
    use bytehost_apps::proto::{AppErrorKind, AppFailure, RuntimeDownload, RuntimeJob};
    use bytehost_apps::state::DesiredState;

    const NOW: u64 = 1_700_000_000_000;

    fn plan() -> InstallPlan {
        let requested = Permissions {
            network: bytehost_apps::permissions::NetworkPerm {
                outbound: Outbound::Any,
            },
            downloads: Gate::UserConfirm,
            ..Permissions::default()
        };
        InstallPlan {
            app_id: AppId::new("excalidraw").unwrap(),
            name: "Excalidraw".into(),
            version: Version::new(0, 17, 0),
            upgrading_from: None,
            provenance: Provenance::Local,
            trust: TrustLevel::Trusted,
            runtime_kind: "static_web".into(),
            manifest_digest: "m".into(),
            source_digest: "s".into(),
            requested,
            enforcement: vec![
                EnforcementEntry {
                    key: PermissionKey::NetworkOutbound,
                    enforcement: Enforcement::Advisory,
                },
                EnforcementEntry {
                    key: PermissionKey::Downloads,
                    enforcement: Enforcement::Unsupported,
                },
            ],
            permission_diff: diff_permissions(&Permissions::default(), &requested),
            will_run: vec!["不执行任何命令".into()],
            source_info: Default::default(),
        }
    }

    fn fail(kind: AppErrorKind, msg: &str) -> Failure {
        Failure::Host(AppFailure::new(kind, msg))
    }

    fn summary(id: &str, observed: ObservedState) -> AppSummary {
        AppSummary {
            id: AppId::new(id).unwrap(),
            name: format!("name-{id}"),
            version: Version::new(1, 0, 0),
            desired: DesiredState::Running,
            observed,
            url: None,
            issue: None,
            previous_version: None,
            rollback_note: None,
        }
    }

    fn reviewing() -> InstallFlowState {
        let mut s = InstallFlowState::default();
        s.update(Message::InstallClicked, NOW);
        s.update(Message::SourcePicked(Some("/src/app".into())), NOW);
        s.update(Message::PlanLoaded(Ok(Box::new(plan()))), NOW);
        assert!(matches!(s.flow, Flow::Reviewing { .. }));
        s
    }

    #[test]
    fn opening_the_page_probes_runtimes_and_lists_apps() {
        let mut s = InstallFlowState::default();
        assert_eq!(
            s.update(Message::Opened, NOW),
            vec![Effect::Probe, Effect::List]
        );
        assert_eq!(s.probes, Load::Loading);
    }

    #[test]
    fn the_install_flow_goes_pick_plan_review_and_cancelling_the_picker_returns_to_idle() {
        let mut s = InstallFlowState::default();
        assert_eq!(
            s.update(Message::InstallClicked, NOW),
            vec![Effect::PickSource]
        );
        assert_eq!(
            s.flow,
            Flow::Picking {
                target: PickTarget::Dir
            }
        );
        assert!(
            s.update(Message::InstallClicked, NOW).is_empty(),
            "流程中不接受第二次安装"
        );
        s.update(Message::SourcePicked(None), NOW);
        assert_eq!(s.flow, Flow::Idle);
        s.update(Message::InstallClicked, NOW);
        let effects = s.update(Message::SourcePicked(Some("/src/app".into())), NOW);
        assert_eq!(
            effects,
            vec![Effect::Plan(AppSource::LocalDir {
                path: "/src/app".into()
            })]
        );
        s.update(Message::PlanLoaded(Ok(Box::new(plan()))), NOW);
        assert!(matches!(s.flow, Flow::Reviewing { .. }));
    }

    #[test]
    fn a_source_picked_outside_the_picking_step_is_ignored() {
        let mut s = InstallFlowState::default();
        assert!(
            s.update(Message::SourcePicked(Some("/x".into())), NOW)
                .is_empty()
        );
        assert_eq!(s.flow, Flow::Idle);
    }

    #[test]
    fn a_rejected_plan_stays_inside_the_flow_as_an_inline_failure() {
        let mut s = InstallFlowState::default();
        s.update(Message::InstallClicked, NOW);
        s.update(Message::SourcePicked(Some("/src/app".into())), NOW);
        let effects = s.update(
            Message::PlanLoaded(Err(fail(AppErrorKind::Rejected, "manifest 缺少 id"))),
            NOW,
        );
        assert!(effects.is_empty(), "校验错误不弹 Notice");
        assert_eq!(
            s.flow,
            Flow::Failed {
                message: "manifest 缺少 id".into()
            }
        );
        s.update(Message::FlowDismissed, NOW);
        assert_eq!(s.flow, Flow::Idle);
    }

    /// 批准的必须**就是展示的那份**:不重算、不改写,批准人/时间如实记录。
    #[test]
    fn approving_sends_exactly_the_plan_that_was_shown() {
        let mut s = reviewing();
        let effects = s.update(Message::ApproveClicked, NOW);
        assert_eq!(effects.len(), 1);
        let Effect::Install { approved, source } = &effects[0] else {
            panic!("{effects:?}")
        };
        assert_eq!(approved.plan(), &plan());
        assert_eq!(approved.approval().approver, APPROVER);
        assert_eq!(approved.approval().approved_ms, NOW);
        assert_eq!(
            source,
            &AppSource::LocalDir {
                path: "/src/app".into()
            }
        );
        assert!(matches!(s.flow, Flow::Installing { .. }));
        assert!(
            s.update(Message::ApproveClicked, NOW).is_empty(),
            "不能重复批准"
        );
    }

    #[test]
    fn approving_is_only_possible_while_reviewing() {
        let mut s = InstallFlowState::default();
        assert!(s.update(Message::ApproveClicked, NOW).is_empty());
        s.update(Message::InstallClicked, NOW);
        assert!(s.update(Message::ApproveClicked, NOW).is_empty());
    }

    #[test]
    fn dismissing_reviews_cancels_but_in_flight_steps_cannot_be_cancelled() {
        let mut s = reviewing();
        s.update(Message::FlowDismissed, NOW);
        assert_eq!(s.flow, Flow::Idle);
        let mut s = reviewing();
        s.update(Message::ApproveClicked, NOW);
        s.update(Message::FlowDismissed, NOW);
        assert!(
            matches!(s.flow, Flow::Installing { .. }),
            "安装请求已发出,不能取消"
        );
    }

    #[test]
    fn a_successful_install_notices_refreshes_and_tells_the_host() {
        let mut s = reviewing();
        s.update(Message::ApproveClicked, NOW);
        let effects = s.update(Message::InstallDone(Ok(())), NOW);
        assert_eq!(s.flow, Flow::Idle);
        assert!(
            matches!(&effects[0], Effect::Notice { level: NoticeLevel::Success, text, key }
            if text.contains("Excalidraw") && key == "apps:install:excalidraw")
        );
        assert!(effects.contains(&Effect::List));
        assert!(effects.contains(&Effect::HostChanged));
    }

    #[test]
    fn a_failed_install_is_shown_in_the_flow_and_does_not_touch_the_host() {
        let mut s = reviewing();
        s.update(Message::ApproveClicked, NOW);
        let effects = s.update(
            Message::InstallDone(Err(fail(
                AppErrorKind::Rejected,
                "安装被拒绝: 应用源码在审批之后发生了变化",
            ))),
            NOW,
        );
        assert!(effects.is_empty());
        match &s.flow {
            Flow::Failed { message } => assert!(message.contains("源码在审批之后"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stop_is_deduplicated_and_always_refreshes_with_a_notice_only_on_failure() {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Ok(vec![summary("stop-a", ObservedState::Running)])),
            NOW,
        );
        assert_eq!(
            s.update(Message::StopClicked("stop-a".into()), NOW),
            vec![Effect::Stop("stop-a".into())]
        );
        assert!(
            s.update(Message::StopClicked("stop-a".into()), NOW)
                .is_empty()
        );
        assert_eq!(s.is_acting("stop-a"), Some(ActKind::Stop));
        let ok = s.update(
            Message::ActionDone("stop-a".into(), ActKind::Stop, Ok(())),
            NOW,
        );
        assert_eq!(ok, vec![Effect::List, Effect::HostChanged]);
        s.update(Message::StopClicked("stop-a".into()), NOW);
        let bad = s.update(
            Message::ActionDone(
                "stop-a".into(),
                ActKind::Stop,
                Err(fail(AppErrorKind::Conflict, "状态冲突")),
            ),
            NOW,
        );
        assert!(
            matches!(&bad[0], Effect::Notice { level: NoticeLevel::Error, text, .. }
            if text.contains("停止") && text.contains("name-stop-a") && text.contains("状态冲突"))
        );
        assert_eq!(&bad[1..], &[Effect::List, Effect::HostChanged]);
    }

    #[test]
    fn uninstall_asks_which_mode_first_and_can_be_cancelled() {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Ok(vec![summary("un-a", ObservedState::Stopped)])),
            NOW,
        );
        assert!(
            s.update(Message::UninstallClicked("un-a".into()), NOW)
                .is_empty()
        );
        assert_eq!(
            s.flow,
            Flow::ConfirmUninstall {
                id: "un-a".into(),
                name: "name-un-a".into()
            }
        );
        s.update(Message::FlowDismissed, NOW);
        assert_eq!(s.flow, Flow::Idle);
        s.update(Message::UninstallClicked("un-a".into()), NOW);
        let effects = s.update(Message::UninstallConfirmed(UninstallMode::Program), NOW);
        assert_eq!(
            effects,
            vec![Effect::Uninstall("un-a".into(), UninstallMode::Program)]
        );
        assert_eq!(s.flow, Flow::Idle);
        assert_eq!(s.is_acting("un-a"), Some(ActKind::Uninstall));
    }

    #[test]
    fn uninstall_confirmation_is_ignored_outside_its_step() {
        let mut s = InstallFlowState::default();
        assert!(
            s.update(
                Message::UninstallConfirmed(UninstallMode::ProgramAndData),
                NOW
            )
            .is_empty()
        );
    }

    fn summary_prev(id: &str, version: (u32, u32, u32), prev: (u32, u32, u32)) -> AppSummary {
        AppSummary {
            version: Version::new(version.0, version.1, version.2),
            previous_version: Some(Version::new(prev.0, prev.1, prev.2)),
            ..summary(id, ObservedState::Running)
        }
    }

    #[test]
    fn rollback_goes_through_an_inline_confirmation_then_fires_the_effect() {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Ok(vec![summary_prev("rb-a", (2, 0, 0), (1, 0, 0))])),
            NOW,
        );
        // 点「回滚到 1.0.0」进入行内确认,不发请求。
        assert!(
            s.update(Message::RollbackClicked("rb-a".into()), NOW)
                .is_empty()
        );
        assert_eq!(
            s.flow,
            Flow::ConfirmRollback {
                id: "rb-a".into(),
                name: "name-rb-a".into(),
                to: Version::new(1, 0, 0),
            }
        );
        // 取消:回 Idle,无副作用。
        assert!(s.update(Message::RollbackCancelled, NOW).is_empty());
        assert_eq!(s.flow, Flow::Idle);
        // 再来一次并确认。
        s.update(Message::RollbackClicked("rb-a".into()), NOW);
        assert_eq!(
            s.update(Message::RollbackConfirmed("rb-a".into()), NOW),
            vec![Effect::Rollback("rb-a".into())]
        );
        assert_eq!(s.flow, Flow::Idle);
    }

    #[test]
    fn rollback_click_is_ignored_when_there_is_no_previous_version() {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Ok(vec![summary("fresh-a", ObservedState::Running)])),
            NOW,
        );
        assert!(
            s.update(Message::RollbackClicked("fresh-a".into()), NOW)
                .is_empty()
        );
        assert_eq!(s.flow, Flow::Idle);
    }

    #[test]
    fn a_failed_rollback_notices_the_conflict_text_and_still_refreshes() {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Ok(vec![summary_prev("rb-a", (2, 0, 0), (1, 0, 0))])),
            NOW,
        );
        let effects = s.update(
            Message::RolledBack(
                "rb-a".into(),
                Err(fail(AppErrorKind::Conflict, "回滚会提升权限,已拒绝")),
            ),
            NOW,
        );
        assert!(
            matches!(&effects[0], Effect::Notice { level: NoticeLevel::Error, text, .. }
                if text.contains("回滚") && text.contains("name-rb-a") && text.contains("回滚会提升权限"))
        );
        assert_eq!(&effects[1..], &[Effect::List, Effect::HostChanged]);
    }

    #[test]
    fn rollback_note_labels_automatic_and_manual_differently() {
        let auto = RollbackNote {
            from: Version::new(2, 0, 0),
            to: Version::new(1, 0, 0),
            reason: "启动失败".into(),
            automatic: true,
            at_ms: 0,
        };
        assert_eq!(
            rollback_note_label(&auto),
            "已从 2.0.0 自动回滚到 1.0.0:启动失败"
        );
        let manual = RollbackNote {
            automatic: false,
            ..auto.clone()
        };
        assert_eq!(rollback_note_label(&manual), "已回滚到 1.0.0");
        // 权限提升导致的跳过:from == to,不得写成"回滚到同一版本"。
        let skipped = RollbackNote {
            from: Version::new(2, 0, 0),
            to: Version::new(2, 0, 0),
            reason: "回滚到上一版需要更高权限".into(),
            automatic: true,
            at_ms: 0,
        };
        assert_eq!(
            rollback_note_label(&skipped),
            "未自动回滚:回滚到上一版需要更高权限"
        );
    }

    #[test]
    fn load_failures_are_kept_as_text_not_raised() {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Err(fail(AppErrorKind::Unavailable, "端口被占用"))),
            NOW,
        );
        assert_eq!(s.apps, Load::Failed("端口被占用".into()));
        s.update(
            Message::ProbesLoaded(Err(Failure::Transport("连不上".into()))),
            NOW,
        );
        assert_eq!(s.probes, Load::Failed("连不上".into()));
    }

    #[test]
    fn the_plan_view_shows_every_requested_change_with_its_real_enforcement() {
        let v = plan_view(&plan(), false);
        assert!(v.notices.is_empty(), "全新安装不该有升级提示");
        assert_eq!(v.title, "Excalidraw(excalidraw)");
        assert!(v.facts.contains(&("版本", "0.17.0".to_string())));
        assert!(v.facts.contains(&("来源", "本地目录".to_string())));
        assert!(v.facts.contains(&("信任", "受信任".to_string())));
        assert_eq!(v.will_run, vec!["不执行任何命令".to_string()]);
        assert_eq!(
            v.permissions,
            vec![
                PermLine {
                    label: "出站网络",
                    change: "无 → 任意".into(),
                    escalation: true,
                    enforcement: "仅声明,不强制"
                },
                PermLine {
                    label: "下载",
                    change: "禁止 → 需确认".into(),
                    escalation: true,
                    enforcement: "此运行方式不支持"
                },
            ]
        );
    }

    #[test]
    fn the_plan_view_marks_a_missing_enforcement_entry_as_unknown_and_shows_upgrades() {
        let mut p = plan();
        p.enforcement.clear();
        p.upgrading_from = Some(Version::new(0, 16, 0));
        p.permission_diff = diff_permissions(
            &Permissions {
                clipboard: Access::ReadWrite,
                ..Permissions::default()
            },
            &Permissions {
                clipboard: Access::Read,
                ..Permissions::default()
            },
        );
        let v = plan_view(&p, true);
        assert!(v.facts.contains(&("版本", "0.16.0 → 0.17.0".to_string())));
        assert_eq!(
            v.notices,
            vec![
                "升级会短暂中断并自动重启。".to_string(),
                "新版本起不来会自动回到上一版。".to_string(),
            ]
        );
        assert_eq!(
            v.permissions,
            vec![PermLine {
                label: "剪贴板",
                change: "读写 → 只读".into(),
                escalation: false,
                enforcement: "未知(按不强制处理)"
            }]
        );
    }

    #[test]
    fn runtime_lines_distinguish_available_unavailable_and_missing() {
        let probe = |runtime: &str, availability| RuntimeProbe {
            runtime: runtime.into(),
            availability,
            managed: Vec::new(),
            installable: false,
            job: None,
        };
        assert_eq!(
            runtime_line(&probe(
                "node",
                RuntimeAvailability::Available {
                    detail: "v20.1.0".into()
                }
            )),
            ("Node.js".into(), "可用 · v20.1.0".into(), true)
        );
        assert_eq!(
            runtime_line(&probe(
                "docker",
                RuntimeAvailability::Unavailable {
                    detail: "守护进程未启动".into()
                }
            )),
            (
                "Docker".into(),
                "已安装但当前不可用 · 守护进程未启动".into(),
                false
            )
        );
        assert_eq!(
            runtime_line(&probe("python", RuntimeAvailability::NotInstalled)),
            ("Python".into(), "未安装".into(), false)
        );
    }

    #[test]
    fn observed_labels_cover_every_state() {
        assert_eq!(observed_label(&ObservedState::Running), "运行中");
        assert_eq!(observed_label(&ObservedState::Stopped), "已停止");
        assert_eq!(observed_label(&ObservedState::Starting), "启动中");
        assert_eq!(observed_label(&ObservedState::Stopping), "停止中");
        assert_eq!(
            observed_label(&ObservedState::Failed {
                reason: "崩了".into(),
                retryable: false
            }),
            "已退出 · 崩了"
        );
    }

    // ---- 设置窗口已关闭时到达的结果 ----

    #[test]
    fn results_that_arrive_after_the_settings_window_closed_still_refresh_the_host_and_notice() {
        let ok = orphan_result_effects(&Message::InstallDone(Ok(())));
        assert!(ok.contains(&Effect::HostChanged));
        assert!(ok.iter().any(|e| matches!(
            e,
            Effect::Notice {
                level: NoticeLevel::Success,
                ..
            }
        )));

        let bad = orphan_result_effects(&Message::InstallDone(Err(fail(
            AppErrorKind::Rejected,
            "应用源码在审批之后发生了变化",
        ))));
        assert!(bad.iter().any(|e| matches!(e,
            Effect::Notice { level: NoticeLevel::Error, text, .. } if text.contains("源码在审批之后"))));
        assert!(
            bad.contains(&Effect::HostChanged),
            "失败也刷新,列表反映真相"
        );

        let stop_ok =
            orphan_result_effects(&Message::ActionDone("x-a".into(), ActKind::Stop, Ok(())));
        assert_eq!(stop_ok, vec![Effect::HostChanged]);
        let un_bad = orphan_result_effects(&Message::ActionDone(
            "x-a".into(),
            ActKind::Uninstall,
            Err(fail(AppErrorKind::Conflict, "状态冲突")),
        ));
        assert!(un_bad.iter().any(|e| matches!(e,
            Effect::Notice { level: NoticeLevel::Error, text, .. } if text.contains("卸载") && text.contains("x-a"))));
        assert!(un_bad.contains(&Effect::HostChanged));
    }

    #[test]
    fn only_install_and_action_results_are_handled_without_a_window() {
        for msg in [
            Message::Opened,
            Message::InstallClicked,
            Message::ApproveClicked,
            Message::FlowDismissed,
            Message::ListLoaded(Ok(vec![])),
            Message::ProbesLoaded(Ok(vec![])),
        ] {
            assert!(orphan_result_effects(&msg).is_empty(), "{msg:?}");
        }
    }

    // ---- A6d Task 5:运行时安装审批 / 进度 / 卸载 ----

    fn rt_plan(runtime: ManagedRuntime, sha: Option<&str>) -> RuntimeInstallPlan {
        RuntimeInstallPlan {
            runtime,
            versions: vec![("node".into(), "24.21.0".into())],
            downloads: vec![RuntimeDownload {
                what: "node".into(),
                url: "https://nodejs.org/dist/v24.21.0/node.tar.gz".into(),
                sha256: sha.map(str::to_owned),
                note: String::new(),
            }],
            dest: "/Users/x/bytehost/runtimes/node/24.21.0".into(),
            will_do: vec!["下载并解压到目标目录".into(), "校验 SHA-256".into()],
        }
    }

    fn rt_probe(
        runtime: &str,
        managed: Vec<&str>,
        installable: bool,
        job: Option<RuntimeJob>,
    ) -> RuntimeProbe {
        RuntimeProbe {
            runtime: runtime.into(),
            availability: RuntimeAvailability::NotInstalled,
            managed: managed.into_iter().map(String::from).collect(),
            installable,
            job,
        }
    }

    fn job(runtime: ManagedRuntime, finished: bool, failed: Option<&str>) -> RuntimeJob {
        RuntimeJob {
            runtime,
            phase: "下载".into(),
            done: 5,
            total: Some(10),
            failed: failed.map(str::to_owned),
            finished,
        }
    }

    #[test]
    fn clicking_install_plans_then_reviewing_shows_every_download_with_its_checksum() {
        let mut s = InstallFlowState::default();
        assert_eq!(
            s.update(Message::RuntimeInstallClicked(ManagedRuntime::Node), NOW),
            vec![Effect::RuntimePlan(ManagedRuntime::Node)]
        );
        assert_eq!(
            s.flow,
            Flow::RuntimePlanning {
                runtime: ManagedRuntime::Node
            }
        );
        s.update(
            Message::RuntimePlanLoaded(Ok(Box::new(rt_plan(
                ManagedRuntime::Node,
                Some(&"a".repeat(64)),
            )))),
            NOW,
        );
        assert!(matches!(s.flow, Flow::RuntimeReviewing { .. }));

        let plan = rt_plan(ManagedRuntime::Node, Some(&"a".repeat(64)));
        let lines = plan_lines(&plan);
        assert!(lines.iter().any(|(_, v)| v.contains("nodejs.org")));
        assert!(
            lines
                .iter()
                .any(|(k, v)| k.starts_with("版本") && v == "24.21.0")
        );
        assert!(
            lines
                .iter()
                .any(|(k, v)| k.starts_with("SHA-256") && v == &"a".repeat(64))
        );
        assert!(
            lines
                .iter()
                .any(|(_, v)| v.contains("/runtimes/node/24.21.0"))
        );

        // sha256 == None → 说明由 uv 校验。
        let uv = RuntimeInstallPlan {
            runtime: ManagedRuntime::Python,
            versions: vec![("python".into(), "3.13".into())],
            downloads: vec![RuntimeDownload {
                what: "python".into(),
                url: "uv:python-install".into(),
                sha256: None,
                note: "由 uv 下载".into(),
            }],
            dest: "/x/runtimes/python".into(),
            will_do: vec![],
        };
        assert!(
            plan_lines(&uv)
                .iter()
                .any(|(k, v)| k.starts_with("SHA-256") && v == "由 uv 内置哈希校验")
        );
    }

    #[test]
    fn approving_sends_back_exactly_the_plan_that_was_shown_and_starts_polling() {
        let mut s = InstallFlowState::default();
        s.update(Message::RuntimeInstallClicked(ManagedRuntime::Node), NOW);
        let plan = rt_plan(ManagedRuntime::Node, Some(&"b".repeat(64)));
        s.update(Message::RuntimePlanLoaded(Ok(Box::new(plan.clone()))), NOW);
        let effects = s.update(Message::RuntimeApproveClicked, NOW);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0], Effect::InstallRuntime(Box::new(plan)));
        assert!(matches!(effects[1], Effect::ProbeAfter(_)));
        assert_eq!(s.flow, Flow::Idle, "批准后回到 Idle");
    }

    #[test]
    fn approve_outside_reviewing_does_nothing() {
        let mut s = InstallFlowState::default();
        assert!(s.update(Message::RuntimeApproveClicked, NOW).is_empty());
        s.update(Message::RuntimeInstallClicked(ManagedRuntime::Node), NOW);
        assert!(s.update(Message::RuntimeApproveClicked, NOW).is_empty());
    }

    #[test]
    fn polling_continues_only_while_a_job_is_unfinished_and_stops_after() {
        let mut s = InstallFlowState::default();
        // 未完成 → 继续轮询。
        let effects = s.update(
            Message::ProbesLoaded(Ok(vec![rt_probe(
                "node",
                vec![],
                true,
                Some(job(ManagedRuntime::Node, false, None)),
            )])),
            NOW,
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::ProbeAfter(_))));

        // 已完成且成功 → 一次成功 Notice + HostChanged,且不再轮询。
        let effects = s.update(
            Message::ProbesLoaded(Ok(vec![rt_probe(
                "node",
                vec!["24.21.0"],
                true,
                Some(job(ManagedRuntime::Node, true, None)),
            )])),
            NOW,
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Notice {
                level: NoticeLevel::Success,
                ..
            }
        )));
        assert!(effects.contains(&Effect::HostChanged));
        assert!(!effects.iter().any(|e| matches!(e, Effect::ProbeAfter(_))));
    }

    #[test]
    fn a_finished_job_notices_once_and_a_failed_one_notices_its_reason_once() {
        let mut s = InstallFlowState::default();
        let probes_ok = || {
            vec![rt_probe(
                "node",
                vec!["24.21.0"],
                true,
                Some(job(ManagedRuntime::Node, true, None)),
            )]
        };
        let first = s.update(Message::ProbesLoaded(Ok(probes_ok())), NOW);
        assert_eq!(
            first
                .iter()
                .filter(|e| matches!(e, Effect::Notice { .. }))
                .count(),
            1
        );
        let second = s.update(Message::ProbesLoaded(Ok(probes_ok())), NOW);
        assert_eq!(
            second
                .iter()
                .filter(|e| matches!(e, Effect::Notice { .. }))
                .count(),
            0,
            "同一任务只报一次"
        );

        // 失败:报一次原因;不同运行时各报各的。
        let mut s = InstallFlowState::default();
        let f = s.update(
            Message::ProbesLoaded(Ok(vec![rt_probe(
                "python",
                vec![],
                true,
                Some(job(ManagedRuntime::Python, true, Some("下载超时"))),
            )])),
            NOW,
        );
        assert!(f.iter().any(|e| matches!(e,
            Effect::Notice { level: NoticeLevel::Error, text, .. } if text.contains("下载超时"))));
    }

    #[test]
    fn docker_never_offers_install() {
        let probes = vec![rt_probe("docker", vec![], false, None)];
        let rows = runtime_rows(&probes);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].installable);
        assert!(rows[0].managed.is_empty());
        assert!(managed_runtime_of("docker").is_none());
    }

    #[test]
    fn an_installed_managed_version_offers_uninstall_with_the_warning() {
        let probes = vec![rt_probe("node", vec!["24.21.0"], true, None)];
        let rows = runtime_rows(&probes);
        assert_eq!(rows[0].managed, vec!["24.21.0".to_string()]);

        // 确认页文案含"运行时缺失而失败"的警告。
        let mut s = InstallFlowState::default();
        s.update(Message::ProbesLoaded(Ok(probes.clone())), NOW);
        let effects = s.update(
            Message::RuntimeUninstallClicked(ManagedRuntime::Node, "24.21.0".into()),
            NOW,
        );
        assert!(effects.is_empty(), "先确认");
        assert!(matches!(
            s.flow,
            Flow::ConfirmRuntimeUninstall { ref runtime, .. } if *runtime == ManagedRuntime::Node
        ));
        let effects = s.update(Message::RuntimeUninstallConfirmed, NOW);
        assert_eq!(
            effects,
            vec![Effect::UninstallRuntime(
                ManagedRuntime::Node,
                "24.21.0".into()
            )]
        );
    }

    #[test]
    fn a_plan_failure_stays_in_the_flow_not_a_notice() {
        let mut s = InstallFlowState::default();
        s.update(Message::RuntimeInstallClicked(ManagedRuntime::Node), NOW);
        let effects = s.update(
            Message::RuntimePlanLoaded(Err(fail(AppErrorKind::Unsupported, "此平台不支持"))),
            NOW,
        );
        assert!(effects.is_empty(), "流程内失败不弹 Notice");
        assert_eq!(
            s.flow,
            Flow::Failed {
                message: "此平台不支持".into()
            }
        );
    }

    #[test]
    fn runtime_rows_show_progress_percent_or_megabytes() {
        // total 已知 → 百分比。
        let pct = runtime_rows(&[rt_probe(
            "node",
            vec![],
            true,
            Some(RuntimeJob {
                runtime: ManagedRuntime::Node,
                phase: "下载".into(),
                done: 3,
                total: Some(4),
                failed: None,
                finished: false,
            }),
        )]);
        assert_eq!(pct[0].progress.as_deref(), Some("下载 · 75%"));

        // total 未知 → 只显示 MB。
        let mb = runtime_rows(&[rt_probe(
            "node",
            vec![],
            true,
            Some(RuntimeJob {
                runtime: ManagedRuntime::Node,
                phase: "下载".into(),
                done: 2_097_152,
                total: None,
                failed: None,
                finished: false,
            }),
        )]);
        assert_eq!(mb[0].progress.as_deref(), Some("下载 · 2.0 MB"));
    }

    #[test]
    fn a_finished_uninstall_clears_dedup_and_refreshes() {
        let mut s = InstallFlowState::default();
        s.update(Message::ProbesLoaded(Ok(vec![])), NOW);
        s.update(
            Message::RuntimeUninstallClicked(ManagedRuntime::Node, "24.21.0".into()),
            NOW,
        );
        s.update(Message::RuntimeUninstallConfirmed, NOW);
        let effects = s.update(Message::RuntimeUninstallDone(Ok(())), NOW);
        assert_eq!(effects, vec![Effect::Probe]);
        // 失败版带 Notice。
        s.update(
            Message::RuntimeUninstallClicked(ManagedRuntime::Node, "24.21.0".into()),
            NOW,
        );
        s.update(Message::RuntimeUninstallConfirmed, NOW);
        let bad = s.update(
            Message::RuntimeUninstallDone(Err(fail(AppErrorKind::Conflict, "仍在运行"))),
            NOW,
        );
        assert!(bad.iter().any(|e| matches!(e,
            Effect::Notice { level: NoticeLevel::Error, text, .. } if text.contains("仍在运行"))));
    }

    #[test]
    fn poll_probes_outside_the_settings_window_does_nothing_bad() {
        // PollProbes 在有窗口时只返回一个 Probe 意图;窗口没了时不产生任何孤儿副作用。
        assert!(orphan_result_effects(&Message::PollProbes).is_empty());
    }

    fn with_app(id: &str) -> InstallFlowState {
        let mut s = InstallFlowState::default();
        s.update(
            Message::ListLoaded(Ok(vec![summary(id, ObservedState::Running)])),
            NOW,
        );
        s
    }

    #[test]
    fn showing_logs_fetches_once_and_only_one_viewer_is_open() {
        let mut s = with_app("alpha");
        let fx = s.update(Message::ShowLogs("alpha".into()), NOW);
        assert_eq!(
            fx,
            vec![Effect::FetchLogs("alpha".into(), crate::logs::FETCH_LINES)]
        );
        assert!(s.is_log_open("alpha"));
        assert_eq!(s.logs_view("alpha"), LogsView::Loading);

        // 展开另一个:前一个自动收起,只保留一个。
        let fx = s.update(Message::ShowLogs("beta".into()), NOW);
        assert_eq!(
            fx,
            vec![Effect::FetchLogs("beta".into(), crate::logs::FETCH_LINES)]
        );
        assert!(s.is_log_open("beta"));
        assert!(!s.is_log_open("alpha"));
        assert_eq!(s.logs_view("alpha"), LogsView::Hidden);
    }

    #[test]
    fn hiding_logs_closes_the_open_viewer() {
        let mut s = with_app("alpha");
        s.update(Message::ShowLogs("alpha".into()), NOW);
        assert!(s.update(Message::HideLogs, NOW).is_empty());
        assert!(!s.is_log_open("alpha"));
    }

    #[test]
    fn a_tick_without_an_open_viewer_does_nothing() {
        let mut s = with_app("alpha");
        assert!(s.update(Message::Tick, NOW).is_empty());
        assert!(!s.log_tick_wanted());

        // 收起后 Tick 同样不发任何请求。
        s.update(Message::ShowLogs("alpha".into()), NOW);
        s.update(Message::HideLogs, NOW);
        assert!(s.update(Message::Tick, NOW).is_empty());
        assert!(!s.log_tick_wanted());
    }

    #[test]
    fn showing_logs_asks_for_the_shared_fetch_line_budget() {
        let mut s = with_app("alpha");
        let fx = s.update(Message::ShowLogs("alpha".into()), NOW);
        assert_eq!(
            fx,
            vec![Effect::FetchLogs("alpha".into(), crate::logs::FETCH_LINES)]
        );
        assert!(s.log_tick_wanted());
    }

    /// 任何消息(滚动、加载结果…)都不能多排定时器:只有一条在路上的 `Tick` 链。
    #[test]
    fn only_one_tick_is_ever_armed_until_it_fires() {
        let mut s = with_app("alpha");
        assert!(!s.arm_tick(), "没有展开的查看器不排");
        s.update(Message::ShowLogs("alpha".into()), NOW);
        assert!(s.arm_tick());
        for _ in 0..20 {
            s.update(Message::LogsScrolled("alpha".into(), false), NOW);
            s.update(
                Message::LogsLoaded("alpha".into(), Ok(("x".into(), false))),
                NOW,
            );
            assert!(!s.arm_tick(), "已有一拍在路上");
        }
        // 切换到另一个应用的查看器也不多排。
        s.update(Message::ShowLogs("beta".into()), NOW);
        assert!(!s.arm_tick());
        // Tick 到达后复位,可以续排一拍;收起后不再排。
        s.update(Message::Tick, NOW);
        assert!(s.arm_tick());
        s.update(Message::HideLogs, NOW);
        s.update(Message::Tick, NOW);
        assert!(!s.arm_tick());
    }

    #[test]
    fn unloading_an_app_closes_its_log_viewer() {
        let mut s = with_app("alpha");
        s.update(Message::ShowLogs("alpha".into()), NOW);
        assert!(s.is_log_open("alpha"));
        s.update(Message::ListLoaded(Ok(Vec::new())), NOW);
        assert!(!s.is_log_open("alpha"));
        assert_eq!(s.logs_view("alpha"), LogsView::Hidden);
    }

    // ---- A6h Task 5:来源选择、审批卡披露、内联校验 -------------------------------------------

    fn source_info(kind: &str, display: &str) -> SourceInfo {
        SourceInfo {
            kind: kind.to_owned(),
            display: display.to_owned(),
            ..SourceInfo::default()
        }
    }

    /// 切到 URL 不丢已填的输入;但从「审批中」切来源会清掉上一次的计划(避免审批 A 装 B)。
    #[test]
    fn switching_source_keeps_typed_input_but_clears_a_stale_plan() {
        let mut s = InstallFlowState::default();
        s.update(Message::SourceChoiceChanged(SourceChoice::Url), NOW);
        s.update(Message::UrlChanged("https://example.com/a.zip".into()), NOW);
        s.update(Message::Sha256Changed("AB".into()), NOW);
        // 切回目录再切回 URL:输入还在。
        s.update(Message::SourceChoiceChanged(SourceChoice::Dir), NOW);
        s.update(Message::SourceChoiceChanged(SourceChoice::Url), NOW);
        assert_eq!(s.url_input, "https://example.com/a.zip");
        assert_eq!(s.sha_input, "AB");
        assert_eq!(s.source_choice, SourceChoice::Url);

        // 从审批中切来源 → 计划/审批卡被清掉,Flow 回 Idle。
        let mut s = reviewing();
        s.update(Message::SourceChoiceChanged(SourceChoice::Archive), NOW);
        assert_eq!(s.flow, Flow::Idle);
        assert_eq!(s.source_choice, SourceChoice::Archive);
    }

    /// 空/非 https URL 点「获取计划」:不发任何 Effect,内联提示原因。
    #[test]
    fn url_plan_clicked_validates_locally_without_sending_an_effect() {
        let mut s = InstallFlowState::default();
        s.update(Message::SourceChoiceChanged(SourceChoice::Url), NOW);

        assert!(s.update(Message::UrlPlanClicked, NOW).is_empty());
        assert!(s.url_error.is_some(), "空 URL 要内联提示");
        assert_eq!(s.flow, Flow::Idle);

        s.update(Message::UrlChanged("http://example.com/a.zip".into()), NOW);
        assert!(s.update(Message::UrlPlanClicked, NOW).is_empty());
        assert!(
            s.url_error.as_deref().is_some_and(|e| e.contains("https")),
            "非 https 要内联提示:{:?}",
            s.url_error
        );

        // 合法 https:发 `Effect::Plan(AppSource::Url{..})`,并进入 Planning。
        s.update(Message::UrlChanged("https://example.com/a.zip".into()), NOW);
        let fx = s.update(Message::UrlPlanClicked, NOW);
        assert_eq!(
            fx,
            vec![Effect::Plan(AppSource::Url {
                url: "https://example.com/a.zip".into(),
                sha256: None,
            })]
        );
        assert!(matches!(s.flow, Flow::Planning { .. }));
        assert!(s.url_error.is_none());
    }

    /// URL 合法但 sha256 非法:同样不发 Effect,内联提示。
    #[test]
    fn a_bad_sha256_blocks_the_url_plan_with_an_inline_hint() {
        let mut s = InstallFlowState::default();
        s.update(Message::SourceChoiceChanged(SourceChoice::Url), NOW);
        s.update(Message::UrlChanged("https://example.com/a.zip".into()), NOW);
        s.update(Message::Sha256Changed("not-hex".into()), NOW);
        assert!(s.update(Message::UrlPlanClicked, NOW).is_empty());
        assert!(s.url_error.as_deref().is_some_and(|e| e.contains("sha256")));
        assert_eq!(s.flow, Flow::Idle);
    }

    /// sha256 输入框即时格式提示:空=不钉死;非法=红字;合法=无提示;不改其它状态。
    #[test]
    fn sha256_input_shows_an_inline_format_hint_only() {
        let mut s = InstallFlowState::default();
        s.update(Message::Sha256Changed("".into()), NOW);
        assert!(s.url_error.is_none(), "空 = 不钉死,无提示");
        s.update(Message::Sha256Changed("xyz".into()), NOW);
        assert!(s.url_error.is_some(), "非法 = 红字");
        s.update(Message::Sha256Changed("AB".into()), NOW);
        assert!(s.url_error.is_some(), "仍非法(太短)");
        let good = "a".repeat(64);
        s.update(Message::Sha256Changed(good.clone()), NOW);
        assert!(s.url_error.is_none(), "合法 = 无提示");
        assert_eq!(s.sha_input, good);
        assert_eq!(s.source_choice, SourceChoice::Dir, "不改来源");
        assert_eq!(s.flow, Flow::Idle, "不改流程");
    }

    /// 审批卡来源区块表驱动:四种组合各自的行文案与警告。
    #[test]
    fn the_review_card_discloses_each_source_shape() {
        // ① 本机目录:只有来源/位置,无警告、非网络。
        let d = source_disclosure(&source_info("local_dir", "/src/app"), "static_web");
        assert_eq!(d.rows[0], ("来源", "本机目录".to_owned()));
        assert_eq!(d.rows[1], ("位置", "/src/app".to_owned()));
        assert!(!d.from_network);
        assert!(d.warnings.is_empty());

        // ② 本机压缩包(进程型应用):来源/位置/完整哈希/大小 + "内容已解压校验"。
        let mut info = source_info("archive", "/tmp/app.zip");
        info.archive_sha256 = Some("f".repeat(64));
        info.archive_bytes = Some(2_500_000);
        let d = source_disclosure(&info, "node");
        assert_eq!(d.rows[0], ("来源", "本机压缩包".to_owned()));
        let sha_row = d
            .rows
            .iter()
            .find(|(k, _)| *k == "压缩包 SHA-256")
            .expect("要有完整哈希行");
        assert_eq!(sha_row.1.len(), 64, "哈希完整显示,不截断");
        assert!(d.rows.iter().any(|(k, _)| *k == "压缩包大小"));
        assert!(!d.from_network);
        assert!(
            d.warnings.iter().any(|w| w.contains("内容已解压校验")),
            "进程型应用从压缩包来要说明:{:?}",
            d.warnings
        );

        // ③ URL 未钉死:来自网络 + "未提供期望 sha256,实际算出的"。
        let mut info = source_info("url", "https://example.com/app.zip");
        info.archive_sha256 = Some("a".repeat(64));
        info.pinned = false;
        let d = source_disclosure(&info, "static_web");
        assert!(d.from_network);
        assert!(d.warnings.iter().any(|w| w.contains("未提供期望 sha256")));

        // ④ URL 钉死且重定向:来源含请求主机,effective_host 不同 → 金色"已重定向到 X"。
        let mut info = source_info("url", "https://origin.example/app.zip");
        info.archive_sha256 = Some("b".repeat(64));
        info.pinned = true;
        info.effective_host = Some("cdn.example".to_owned());
        let d = source_disclosure(&info, "static_web");
        assert!(d.from_network);
        assert!(
            d.warnings
                .iter()
                .any(|w| w.contains("已匹配你提供的 sha256"))
        );
        assert!(
            d.warnings.iter().any(|w| w == "已重定向到 cdn.example"),
            "重定向要金色警告:{:?}",
            d.warnings
        );
    }

    /// URL 计划被服务端拒(`SourceNotAllowed`)→ 内联错误且输入保留;随后改成本机压缩包重试成功。
    #[test]
    fn a_rejected_url_plan_stays_inline_and_keeps_input_then_archive_retries() {
        let mut s = InstallFlowState::default();
        s.update(Message::SourceChoiceChanged(SourceChoice::Url), NOW);
        s.update(
            Message::UrlChanged("https://example.com/node.zip".into()),
            NOW,
        );
        s.update(Message::Sha256Changed("A".repeat(64).as_str().into()), NOW);
        s.update(Message::UrlPlanClicked, NOW);
        let fx = s.update(
            Message::PlanLoaded(Err(fail(
                AppErrorKind::Rejected,
                "网络来源只能安装静态应用",
            ))),
            NOW,
        );
        assert!(fx.is_empty(), "校验类失败不弹 Notice");
        assert!(
            matches!(&s.flow, Flow::Failed { message } if message.contains("静态应用")),
            "{:?}",
            s.flow
        );
        // 输入保留。
        assert_eq!(s.url_input, "https://example.com/node.zip");
        assert_eq!(s.sha_input, "A".repeat(64));

        // 切到本机压缩包并重试:成功。
        s.update(Message::SourceChoiceChanged(SourceChoice::Archive), NOW);
        assert_eq!(s.flow, Flow::Idle);
        let fx = s.update(Message::PickArchive, NOW);
        assert_eq!(fx, vec![Effect::PickArchive]);
        let fx = s.update(Message::SourcePicked(Some("/tmp/app.zip".into())), NOW);
        assert_eq!(
            fx,
            vec![Effect::Plan(AppSource::Archive {
                path: "/tmp/app.zip".into()
            })]
        );
        s.update(Message::PlanLoaded(Ok(Box::new(plan()))), NOW);
        assert!(matches!(s.flow, Flow::Reviewing { .. }));
        // 让服务端推导的信任/来源如实进审批卡(服务端会覆盖自报值)。
    }

    /// 计划在途时重复点「获取计划」不重发(沿用 flow != Idle 守卫)。
    #[test]
    fn a_second_url_plan_click_while_in_flight_is_ignored() {
        let mut s = InstallFlowState::default();
        s.update(Message::SourceChoiceChanged(SourceChoice::Url), NOW);
        s.update(Message::UrlChanged("https://example.com/a.zip".into()), NOW);
        assert_eq!(s.update(Message::UrlPlanClicked, NOW).len(), 1);
        assert!(matches!(s.flow, Flow::Planning { .. }));
        assert!(
            s.update(Message::UrlPlanClicked, NOW).is_empty(),
            "在途不重发"
        );
    }
}
