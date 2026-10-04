//! `AppManager`:安装、启动、停止、卸载、启动对账——把注册表、gateway 和生命周期纯函数接起来。
//!
//! 一期只实现 `static_web`;其他 runtime 在 `install_plan` 就被明确拒绝(`UnsupportedRuntime`)。
//! 所有会改状态的方法都在同一把锁里执行(**单写者**:规格里 supervisor 是唯一写者),
//! 每次状态变化都先落盘(`AppRecord.observed`)再发事件。耗时任务句柄/进度(规格 §4.3)留到
//! 有真正耗时的 runtime 时再加:静态应用的安装是瞬时的。
//!
//! 防 TOCTOU 的安装顺序(见 `digest` 模块文档):**先把包拷进 staging,对 staging 副本重新算摘要并
//! `verify`,再改名落位**;`current_version` 在最后才写。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::digest::{data_store_id_hex, digest_tree, sha256_hex};
use crate::event::AppEvent;
use crate::gateway::{Gateway, csp_for};
use crate::id::{AppId, Version};
use crate::manifest::{Manifest, ManifestError, Runtime};
use crate::plan::{
    ApprovedInstallPlan, InstallPlan, Installed, PlanInput, Provenance, TrustLevel, VerifyError,
};
use crate::registry::{AppRecord, RECORD_FORMAT_VERSION, Registry, UninstallMode, VersionRecord};
use crate::runtime::enforcement_for;
use crate::state::{
    Action, DesiredState, ObservedState, next_action, recover_after_supervisor_restart,
};

/// 应用从哪来。一期只有本地目录(目录里要有 `manifest.toml`);压缩包/仓库以后再加。
#[derive(Debug, Clone)]
pub enum AppSource {
    LocalDir(PathBuf),
}

#[derive(Debug)]
pub enum ManagerError {
    Io(io::Error),
    Manifest(ManifestError),
    Verify(VerifyError),
    /// 一期只支持 `static_web`。
    UnsupportedRuntime(String),
    /// 这个版本已经装过了(同版本不覆盖)。
    AlreadyInstalled(Version),
    /// 应用正在运行/过渡中,升级前要先停止。
    Busy(AppId),
    NotInstalled(AppId),
    /// 当前状态不允许这个操作。
    BadState {
        app: AppId,
        state: ObservedState,
    },
    /// 应用包里没有 `runtime.source` 指向的目录。
    MissingSource(PathBuf),
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O 错误: {e}"),
            Self::Manifest(e) => write!(f, "{e}"),
            Self::Verify(e) => write!(f, "安装被拒绝: {e}"),
            Self::UnsupportedRuntime(k) => {
                write!(f, "暂不支持 {k} 类型的应用(一期只支持 static_web)")
            }
            Self::AlreadyInstalled(v) => write!(f, "版本 {v} 已经安装"),
            Self::Busy(id) => write!(f, "应用 {id} 正在运行,请先停止再升级"),
            Self::NotInstalled(id) => write!(f, "应用 {id} 没有安装"),
            Self::BadState { app, state } => {
                write!(f, "应用 {app} 当前状态 {state:?} 不允许这个操作")
            }
            Self::MissingSource(p) => write!(f, "应用包里缺少站点目录: {}", p.display()),
        }
    }
}

impl std::error::Error for ManagerError {}

impl From<io::Error> for ManagerError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ManifestError> for ManagerError {
    fn from(e: ManifestError) -> Self {
        Self::Manifest(e)
    }
}

/// 列表里的一项。
#[derive(Debug, Clone, PartialEq)]
pub struct AppSummary {
    pub id: AppId,
    pub name: String,
    pub version: Version,
    pub desired: DesiredState,
    pub observed: ObservedState,
    /// 运行中才有:不含令牌的站点地址。
    pub url: Option<String>,
}

pub struct AppManager {
    registry: Registry,
    gateway: Arc<Gateway>,
    host_version: Version,
    events: broadcast::Sender<AppEvent>,
    /// 单写者锁:所有改状态的方法都先拿它。
    lock: Mutex<()>,
}

/// 一个已读入并校验过的应用包。
struct Package {
    manifest: Manifest,
    manifest_digest: String,
    source_digest: String,
}

fn read_package(dir: &Path, host_version: &Version) -> Result<Package, ManagerError> {
    let manifest_text = fs::read_to_string(dir.join("manifest.toml"))?;
    let manifest = Manifest::from_toml(&manifest_text, host_version)?;
    Ok(Package {
        manifest_digest: sha256_hex(manifest_text.as_bytes()),
        source_digest: digest_tree(dir)?,
        manifest,
    })
}

fn static_source(manifest: &Manifest) -> Result<&str, ManagerError> {
    match &manifest.runtime {
        Runtime::StaticWeb { source } => Ok(source),
        other => Err(ManagerError::UnsupportedRuntime(
            other.kind_name().to_string(),
        )),
    }
}

/// 递归拷贝目录。只拷普通文件和目录:符号链接/FIFO/设备一律拒绝(与 `digest_tree` 同口径)。
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &to)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("应用包里只允许普通文件和目录: {}", entry.path().display()),
            ));
        }
    }
    Ok(())
}

impl AppManager {
    pub fn new(
        root: impl Into<PathBuf>,
        host_version: Version,
        gateway: Arc<Gateway>,
    ) -> io::Result<Self> {
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            registry: Registry::open(root)?,
            gateway,
            host_version,
            events,
            lock: Mutex::new(()),
        })
    }

    pub fn events(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: AppEvent) {
        // 没有订阅者时 send 会返回错误——不是问题
        let _ = self.events.send(event);
    }

    /// 首次导航用的地址(带一次性换 Cookie 的令牌)。**秘密,不要写日志、不要广播。**
    pub fn launch_url(&self, id: &AppId) -> String {
        self.gateway.launch_url(id)
    }

    pub fn install_plan(
        &self,
        source: &AppSource,
        provenance: Provenance,
        trust: TrustLevel,
    ) -> Result<InstallPlan, ManagerError> {
        let AppSource::LocalDir(dir) = source;
        let package = read_package(dir, &self.host_version)?;
        self.plan_for(&package, provenance, trust)
    }

    fn plan_for(
        &self,
        package: &Package,
        provenance: Provenance,
        trust: TrustLevel,
    ) -> Result<InstallPlan, ManagerError> {
        static_source(&package.manifest)?;
        let record = self.registry.load(&package.manifest.id)?;
        let installed = record.as_ref().map(|r| Installed {
            version: &r.current_version,
            grants: &r.grants,
        });
        Ok(InstallPlan::build(PlanInput {
            manifest: &package.manifest,
            manifest_digest: package.manifest_digest.clone(),
            source_digest: package.source_digest.clone(),
            provenance,
            trust,
            enforcement: enforcement_for(&package.manifest.runtime),
            installed,
        }))
    }

    /// 安装一份**已批准**的计划。**一切决定都只基于 staging 里的副本**:源目录只被"拷贝"这一个动作读取,
    /// 拷完之后它再怎么变都与本次安装无关;对 staging 副本重新计算并 `verify`,通过才落位。
    pub fn install(
        &self,
        approved: &ApprovedInstallPlan,
        source: &AppSource,
        now_ms: u64,
    ) -> Result<(), ManagerError> {
        let _guard = self.lock.lock().expect("manager 锁");
        let AppSource::LocalDir(dir) = source;
        let staging = self
            .registry
            .paths()
            .apps_dir()
            .join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
        let result = self.install_staged(approved, dir, &staging, now_ms);
        // 成功时 staging 已经改名走了,这里是空操作;失败时清掉
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    fn install_staged(
        &self,
        approved: &ApprovedInstallPlan,
        dir: &Path,
        staging: &Path,
        now_ms: u64,
    ) -> Result<(), ManagerError> {
        copy_tree(dir, staging)?;
        let package = read_package(staging, &self.host_version)?;
        let id = package.manifest.id.clone();
        let version = package.manifest.version;
        let plan = self.plan_for(&package, approved.plan().provenance, approved.plan().trust)?;
        let verified = approved.verify(plan).map_err(ManagerError::Verify)?;

        let existing = self.registry.load(&id)?;
        if let Some(r) = &existing {
            if r.versions.iter().any(|v| v.version == version) {
                return Err(ManagerError::AlreadyInstalled(version));
            }
            if !matches!(
                r.observed,
                ObservedState::Installed | ObservedState::Stopped | ObservedState::Failed { .. }
            ) {
                return Err(ManagerError::Busy(id));
            }
        }

        let final_dir = self.registry.paths().package_dir(&id, &version);
        fs::create_dir_all(final_dir.parent().expect("package_dir 有父目录"))?;
        // 没有版本记录却已经存在的版本目录,是上一次崩溃留下的残骸:清掉,不能让它挡住重试
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)?;
        }
        fs::rename(staging, &final_dir)?;
        // 落位之后的任何一步失败都要把包撤回,否则会留下"有包目录、没有记录"的状态
        let upgrading = existing.is_some();
        let recorded = (|| -> Result<(), ManagerError> {
            let mut record = existing.unwrap_or_else(|| AppRecord {
                format_version: RECORD_FORMAT_VERSION,
                id: id.clone(),
                desired: DesiredState::Stopped,
                observed: ObservedState::Installed,
                current_version: version,
                grants: verified.requested,
                versions: Vec::new(),
                data_store_id: data_store_id_hex(&id),
            });
            record.grants = verified.requested;
            record.versions.push(VersionRecord {
                version,
                manifest_digest: verified.manifest_digest.clone(),
                source_digest: verified.source_digest.clone(),
                installed_ms: now_ms,
            });
            if upgrading && matches!(record.observed, ObservedState::Failed { .. }) {
                record.observed = ObservedState::Installed;
            }
            // current_version 最后才写;它之前的任何一步失败,调用方会撤回刚落位的包,旧版本不受影响
            record.current_version = version;
            self.registry.save(&record)?;
            Ok(())
        })();
        if let Err(e) = recorded {
            let _ = fs::remove_dir_all(&final_dir);
            return Err(e);
        }

        self.emit(AppEvent::Installed { app: id.clone() });
        if upgrading && !verified.permission_diff.is_empty() {
            self.emit(AppEvent::ManifestChanged {
                app: id,
                permission_changes: verified.permission_diff,
            });
        }
        Ok(())
    }

    fn load_record(&self, id: &AppId) -> Result<AppRecord, ManagerError> {
        self.registry
            .load(id)?
            .ok_or_else(|| ManagerError::NotInstalled(id.clone()))
    }

    fn set_observed(
        &self,
        record: &mut AppRecord,
        observed: ObservedState,
    ) -> Result<(), ManagerError> {
        record.observed = observed.clone();
        self.registry.save(record)?;
        self.emit(AppEvent::StateChanged {
            app: record.id.clone(),
            state: observed,
        });
        Ok(())
    }

    /// 启动:把应用的静态站点注册进 gateway。返回不含令牌的站点地址。
    pub fn start(&self, id: &AppId) -> Result<String, ManagerError> {
        let _guard = self.lock.lock().expect("manager 锁");
        self.start_locked(id)
    }

    fn start_locked(&self, id: &AppId) -> Result<String, ManagerError> {
        let mut record = self.load_record(id)?;
        match &record.observed {
            ObservedState::Running => return Ok(self.gateway.site_url(id)),
            ObservedState::Installed | ObservedState::Stopped => {}
            ObservedState::Failed {
                retryable: true, ..
            } => {}
            other => {
                return Err(ManagerError::BadState {
                    app: id.clone(),
                    state: other.clone(),
                });
            }
        }
        let text = fs::read_to_string(
            self.registry
                .paths()
                .package_dir(id, &record.current_version)
                .join("manifest.toml"),
        )?;
        let manifest = Manifest::from_toml(&text, &self.host_version)?;
        let source = static_source(&manifest)?.to_string();
        let root = self
            .registry
            .paths()
            .package_dir(id, &record.current_version)
            .join(&source);
        if !root.is_dir() {
            let reason = format!("应用包里缺少站点目录 {source}");
            self.set_observed(
                &mut record,
                ObservedState::Failed {
                    reason,
                    retryable: false,
                },
            )?;
            return Err(ManagerError::MissingSource(root));
        }
        self.gateway.add_site(id, root, csp_for(&record.grants));
        record.desired = DesiredState::Running;
        self.set_observed(&mut record, ObservedState::Running)?;
        let url = self.gateway.site_url(id);
        self.emit(AppEvent::EndpointChanged {
            app: id.clone(),
            url: Some(url.clone()),
        });
        Ok(url)
    }

    /// 停止(也用来把 `Failed` 复位成 `Stopped`)。对已经停止的应用是空操作。
    pub fn stop(&self, id: &AppId) -> Result<(), ManagerError> {
        let _guard = self.lock.lock().expect("manager 锁");
        self.stop_locked(id)
    }

    fn stop_locked(&self, id: &AppId) -> Result<(), ManagerError> {
        let mut record = self.load_record(id)?;
        let was_serving = self.gateway.remove_site(id);
        record.desired = DesiredState::Stopped;
        match record.observed {
            ObservedState::Running | ObservedState::Failed { .. } => {
                self.set_observed(&mut record, ObservedState::Stopped)?;
            }
            _ => self.registry.save(&record)?,
        }
        if was_serving {
            self.emit(AppEvent::EndpointChanged {
                app: id.clone(),
                url: None,
            });
        }
        Ok(())
    }

    /// 卸载(不存在不算错误)。运行中先停。
    pub fn uninstall(&self, id: &AppId, mode: UninstallMode) -> Result<(), ManagerError> {
        let _guard = self.lock.lock().expect("manager 锁");
        if self.registry.load(id)?.is_none() {
            self.registry.uninstall(id, mode)?;
            return Ok(());
        }
        self.stop_locked(id)?;
        self.registry.uninstall(id, mode)?;
        self.emit(AppEvent::StateChanged {
            app: id.clone(),
            state: ObservedState::NotInstalled,
        });
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<AppSummary>, ManagerError> {
        let listing = self.registry.list()?;
        Ok(listing
            .apps
            .into_iter()
            .map(|r| {
                let name = fs::read_to_string(
                    self.registry
                        .paths()
                        .package_dir(&r.id, &r.current_version)
                        .join("manifest.toml"),
                )
                .ok()
                .and_then(|t| Manifest::from_toml(&t, &self.host_version).ok())
                .map(|m| m.name)
                .unwrap_or_else(|| r.id.to_string());
                let url = matches!(r.observed, ObservedState::Running)
                    .then(|| self.gateway.site_url(&r.id));
                AppSummary {
                    name,
                    version: r.current_version,
                    desired: r.desired,
                    observed: r.observed,
                    url,
                    id: r.id,
                }
            })
            .collect())
    }

    /// supervisor 启动时调用:把持久化的观察态修正为现实(应用随 supervisor 一起停了),再按 `desired` 对账。
    /// 返回失败的应用与原因,一个应用失败不影响其他应用。
    pub fn reconcile(&self) -> Vec<(AppId, ManagerError)> {
        let _guard = self.lock.lock().expect("manager 锁");
        let mut failures = Vec::new();
        let listing = match self.registry.list() {
            Ok(l) => l,
            Err(e) => return vec![(AppId::new("unknown").expect("合法"), ManagerError::Io(e))],
        };
        for mut record in listing.apps {
            let id = record.id.clone();
            let recovered = recover_after_supervisor_restart(record.observed.clone());
            if recovered != record.observed
                && let Err(e) = self.set_observed(&mut record, recovered)
            {
                failures.push((id, e));
                continue;
            }
            let result = match next_action(record.desired, &record.observed) {
                Some(Action::Start) => self.start_locked(&id).map(|_| ()),
                Some(Action::Stop) => self.stop_locked(&id),
                // 卸载只删程序(数据由用户显式要求才删)
                Some(Action::Uninstall) => self
                    .registry
                    .uninstall(&id, UninstallMode::Program)
                    .map_err(Into::into),
                None => Ok(()),
            };
            if let Err(e) = result {
                failures.push((id, e));
            }
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayConfig;
    use crate::permissions::{Access, Enforcement, PermissionKey};
    use crate::plan::Approval;
    use crate::testutil::{Req, Resp, write_files};

    const HOST: Version = Version::new(0, 1, 0);

    fn manifest_toml(id: &str, version: &str, extra: &str) -> String {
        format!(
            r#"schema_version = 1
min_host_version = "0.1.0"
id = "{id}"
name = "{id} app"
version = "{version}"

[presentation]
entrypoint = "main"

[entrypoints.main]
type = "web"
path = "/"

[runtime]
kind = "static_web"
source = "web/"
{extra}"#
        )
    }

    /// 在 `dir` 下写一个静态应用(manifest.toml + web/index.html),返回来源。
    fn write_app(dir: &Path, id: &str, version: &str, extra: &str, body: &str) -> AppSource {
        write_files(
            dir,
            &[
                ("manifest.toml", &manifest_toml(id, version, extra)),
                ("web/index.html", body),
            ],
        );
        AppSource::LocalDir(dir.to_path_buf())
    }

    struct Rig {
        tmp: tempfile::TempDir,
        gateway: Arc<Gateway>,
        manager: AppManager,
    }

    async fn rig() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let gateway = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let manager = AppManager::new(tmp.path().join("bytehost"), HOST, gateway.clone()).unwrap();
        Rig {
            tmp,
            gateway,
            manager,
        }
    }

    impl Rig {
        fn src_dir(&self, name: &str) -> PathBuf {
            self.tmp.path().join("src").join(name)
        }

        fn approve(&self, source: &AppSource) -> ApprovedInstallPlan {
            self.manager
                .install_plan(source, Provenance::Local, TrustLevel::Trusted)
                .unwrap()
                .approve(Approval {
                    approver: "test".into(),
                    approved_ms: 1,
                })
        }

        fn install(&self, source: &AppSource) -> Result<(), ManagerError> {
            self.manager.install(&self.approve(source), source, 100)
        }

        /// 带着有效 Cookie 取应用的某个路径。
        fn fetch(&self, id: &AppId, target: &str) -> Resp {
            let url = self.manager.launch_url(id);
            let token = url.split("bh_token=").nth(1).unwrap().to_string();
            let port = self.gateway.port();
            Req::get(port, &format!("{id}.localhost:{port}"), target)
                .cookie(&format!("bh_session={token}"))
                .send()
        }
    }

    fn id(s: &str) -> AppId {
        AppId::new(s).unwrap()
    }

    /// 安装失败之后,`apps/` 下不能留下 staging 目录,也不能凭空多出应用目录。
    fn assert_no_leftovers(rig: &Rig) {
        let apps = rig.manager.registry.paths().apps_dir();
        let names: Vec<String> = fs::read_dir(&apps)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.starts_with(".staging-")),
            "{names:?}"
        );
    }

    fn drain(rx: &mut broadcast::Receiver<AppEvent>) -> Vec<AppEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_plan_describes_the_app_and_is_honest_about_static_web_enforcement() {
        let rig = rig().await;
        let src = write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "0.17.0",
            "[permissions]\nclipboard = \"read_write\"\n",
            "<h1>x</h1>",
        );
        let plan = rig
            .manager
            .install_plan(&src, Provenance::ThirdParty, TrustLevel::Untrusted)
            .unwrap();
        assert_eq!(plan.app_id, id("excalidraw"));
        assert_eq!(plan.runtime_kind, "static_web");
        assert_eq!(plan.provenance, Provenance::ThirdParty);
        assert_eq!(plan.permission_diff.len(), 1);
        assert_eq!(plan.permission_diff[0].key, PermissionKey::Clipboard);
        let net = plan
            .enforcement
            .iter()
            .find(|e| e.key == PermissionKey::NetworkOutbound)
            .unwrap();
        assert_eq!(net.enforcement, Enforcement::Advisory);
        assert!(plan.will_run[0].contains("不执行任何命令"));
        assert_eq!(plan.manifest_digest.len(), 64);
        assert_eq!(plan.source_digest.len(), 64);
        assert!(
            rig.manager.list().unwrap().is_empty(),
            "出计划不会安装任何东西"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_start_serve_and_stop_round_trip_with_events() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let src = write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "0.17.0",
            "",
            "<h1>excalidraw</h1>",
        );
        rig.install(&src).unwrap();
        let app = id("excalidraw");

        let listed = rig.manager.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "excalidraw app");
        assert_eq!(listed[0].observed, ObservedState::Installed);
        assert_eq!(listed[0].desired, DesiredState::Stopped);
        assert_eq!(listed[0].url, None);
        assert_eq!(rig.fetch(&app, "/").status, 404, "没启动:站点没注册");

        let url = rig.manager.start(&app).unwrap();
        assert_eq!(
            url,
            format!("http://excalidraw.localhost:{}/", rig.gateway.port())
        );
        let r = rig.fetch(&app, "/");
        assert_eq!(r.status, 200);
        assert!(r.text().contains("excalidraw"));
        assert!(
            r.header("content-security-policy").is_some(),
            "默认出站网络为 none → 带 CSP"
        );
        let listed = rig.manager.list().unwrap();
        assert_eq!(
            (listed[0].observed.clone(), listed[0].url.clone()),
            (ObservedState::Running, Some(url.clone()))
        );
        assert_eq!(rig.manager.start(&app).unwrap(), url, "重复启动是幂等的");

        rig.manager.stop(&app).unwrap();
        assert_eq!(rig.fetch(&app, "/").status, 404);
        assert_eq!(
            rig.manager.list().unwrap()[0].observed,
            ObservedState::Stopped
        );
        rig.manager.stop(&app).unwrap();

        let events = drain(&mut rx);
        assert_eq!(
            events,
            vec![
                AppEvent::Installed { app: app.clone() },
                AppEvent::StateChanged {
                    app: app.clone(),
                    state: ObservedState::Running
                },
                AppEvent::EndpointChanged {
                    app: app.clone(),
                    url: Some(url)
                },
                AppEvent::StateChanged {
                    app: app.clone(),
                    state: ObservedState::Stopped
                },
                AppEvent::EndpointChanged {
                    app: app.clone(),
                    url: None
                },
            ]
        );
        assert!(
            !format!("{events:?}").contains("bh_token"),
            "事件里不能出现令牌"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn granting_outbound_network_removes_the_csp() {
        let rig = rig().await;
        let src = write_app(
            &rig.src_dir("a"),
            "online",
            "1.0.0",
            "[permissions.network]\noutbound = \"any\"\n",
            "hi",
        );
        rig.install(&src).unwrap();
        rig.manager.start(&id("online")).unwrap();
        let r = rig.fetch(&id("online"), "/");
        assert_eq!(r.status, 200);
        assert!(r.header("content-security-policy").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_or_manifest_changed_after_approval_is_refused_and_leaves_nothing_behind() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        let src = write_app(&dir, "excalidraw", "0.17.0", "", "<h1>good</h1>");
        let approved = rig.approve(&src);

        fs::write(dir.join("web/index.html"), "<h1>evil</h1>").unwrap();
        assert!(matches!(
            rig.manager.install(&approved, &src, 1),
            Err(ManagerError::Verify(VerifyError::SourceChanged))
        ));

        fs::write(dir.join("web/index.html"), "<h1>good</h1>").unwrap();
        fs::write(
            dir.join("manifest.toml"),
            manifest_toml(
                "excalidraw",
                "0.17.0",
                "[permissions]\nclipboard = \"read_write\"\n",
            ),
        )
        .unwrap();
        assert!(matches!(
            rig.manager.install(&approved, &src, 1),
            Err(ManagerError::Verify(VerifyError::ManifestChanged))
        ));

        assert!(rig.manager.list().unwrap().is_empty());
        assert_no_leftovers(&rig);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_approved_payload_with_widened_permissions_is_refused() {
        let rig = rig().await;
        let src = write_app(&rig.src_dir("a"), "excalidraw", "0.17.0", "", "hi");
        let approved = rig.approve(&src);
        let mut json: serde_json::Value = serde_json::to_value(&approved).unwrap();
        json["plan"]["requested"]["clipboard"] = serde_json::json!("read_write");
        let forged: ApprovedInstallPlan = serde_json::from_value(json).unwrap();
        assert!(matches!(
            rig.manager.install(&forged, &src, 1),
            Err(ManagerError::Verify(VerifyError::PlanChanged))
        ));
        assert!(rig.manager.list().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_static_web_is_supported_in_phase_one() {
        let rig = rig().await;
        let dir = rig.src_dir("py");
        write_files(
            &dir,
            &[(
                "manifest.toml",
                &manifest_toml("pyapp", "1.0.0", "").replace(
                    "kind = \"static_web\"\nsource = \"web/\"",
                    "kind = \"python\"\ncommand = [\"python\", \"-m\", \"app\"]\n[runtime.http]\nport_env = \"PORT\"",
                ),
            )],
        );
        match rig.manager.install_plan(
            &AppSource::LocalDir(dir),
            Provenance::Local,
            TrustLevel::Trusted,
        ) {
            Err(ManagerError::UnsupportedRuntime(k)) => assert_eq!(k, "python"),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_package_with_a_symlink_is_refused() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        let src = write_app(&dir, "excalidraw", "0.17.0", "", "hi");
        std::os::unix::fs::symlink("/etc/hosts", dir.join("web/leak")).unwrap();
        assert!(matches!(
            rig.manager
                .install_plan(&src, Provenance::Local, TrustLevel::Trusted),
            Err(ManagerError::Io(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_same_version_is_never_overwritten_and_an_upgrade_records_new_grants() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let app = id("excalidraw");
        let v1 = write_app(
            &rig.src_dir("v1"),
            "excalidraw",
            "1.0.0",
            "",
            "<h1>one</h1>",
        );
        rig.install(&v1).unwrap();
        assert!(
            matches!(rig.install(&v1), Err(ManagerError::AlreadyInstalled(v)) if v == Version::new(1, 0, 0))
        );
        drain(&mut rx);

        let v2 = write_app(
            &rig.src_dir("v2"),
            "excalidraw",
            "1.1.0",
            "[permissions]\nclipboard = \"read\"\n",
            "<h1>two</h1>",
        );
        let plan = rig
            .manager
            .install_plan(&v2, Provenance::Local, TrustLevel::Trusted)
            .unwrap();
        assert_eq!(plan.upgrading_from, Some(Version::new(1, 0, 0)));
        assert_eq!(plan.permission_diff.len(), 1, "升级差异是相对当前授予的");
        rig.install(&v2).unwrap();

        let events = drain(&mut rx);
        assert!(matches!(&events[0], AppEvent::Installed { .. }));
        assert!(
            matches!(&events[1], AppEvent::ManifestChanged { permission_changes, .. } if permission_changes.len() == 1)
        );
        let record = rig.manager.registry.load(&app).unwrap().unwrap();
        assert_eq!(record.current_version, Version::new(1, 1, 0));
        assert_eq!(record.versions.len(), 2);
        assert_eq!(record.grants.clipboard, Access::Read);
        let pkg = rig.manager.registry.paths();
        assert!(
            pkg.package_dir(&app, &Version::new(1, 0, 0)).is_dir(),
            "旧版本目录保留"
        );
        rig.manager.start(&app).unwrap();
        assert!(
            rig.fetch(&app, "/").text().contains("two"),
            "启动的是当前版本"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrading_a_running_app_is_refused_until_it_is_stopped() {
        let rig = rig().await;
        let app = id("excalidraw");
        rig.install(&write_app(
            &rig.src_dir("v1"),
            "excalidraw",
            "1.0.0",
            "",
            "one",
        ))
        .unwrap();
        rig.manager.start(&app).unwrap();
        let v2 = write_app(&rig.src_dir("v2"), "excalidraw", "1.1.0", "", "two");
        assert!(matches!(rig.install(&v2), Err(ManagerError::Busy(_))));
        rig.manager.stop(&app).unwrap();
        rig.install(&v2).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_package_without_its_site_directory_fails_to_start_cleanly_and_stop_resets_it() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        write_files(
            &dir,
            &[("manifest.toml", &manifest_toml("hollow", "1.0.0", ""))],
        );
        let src = AppSource::LocalDir(dir);
        rig.install(&src).unwrap();
        let app = id("hollow");
        assert!(matches!(
            rig.manager.start(&app),
            Err(ManagerError::MissingSource(_))
        ));
        assert!(matches!(
            rig.manager.list().unwrap()[0].observed,
            ObservedState::Failed {
                retryable: false,
                ..
            }
        ));
        assert!(
            matches!(rig.manager.start(&app), Err(ManagerError::BadState { .. })),
            "不可重试的失败不自动重来"
        );
        rig.manager.stop(&app).unwrap();
        assert_eq!(
            rig.manager.list().unwrap()[0].observed,
            ObservedState::Stopped
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uninstalling_the_program_stops_serving_and_keeps_user_data() {
        let rig = rig().await;
        let mut rx = rig.manager.events();
        let app = id("excalidraw");
        rig.install(&write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "1.0.0",
            "",
            "hi",
        ))
        .unwrap();
        rig.manager.start(&app).unwrap();
        let data = rig.manager.registry.paths().data_dir(&app);
        fs::write(data.join("drawing.json"), "{}").unwrap();
        drain(&mut rx);

        rig.manager.uninstall(&app, UninstallMode::Program).unwrap();
        assert!(rig.manager.list().unwrap().is_empty());
        assert!(!rig.gateway.has_site(&app));
        assert!(data.join("drawing.json").exists());
        let events = drain(&mut rx);
        assert!(events.contains(&AppEvent::StateChanged {
            app: app.clone(),
            state: ObservedState::NotInstalled
        }));

        rig.manager.uninstall(&app, UninstallMode::Program).unwrap();
        rig.manager
            .uninstall(&app, UninstallMode::ProgramAndData)
            .unwrap();
        assert!(!rig.manager.registry.paths().app_dir(&app).exists());
        rig.manager
            .uninstall(&id("never-installed"), UninstallMode::ProgramAndData)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_a_supervisor_restart_reconcile_restarts_apps_that_wanted_to_run() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bytehost");
        let (run, idle) = (id("runner"), id("idler"));
        {
            let gw = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
            let m = AppManager::new(&root, HOST, gw.clone()).unwrap();
            for (app, dir) in [(&run, "runner"), (&idle, "idler")] {
                let src = write_app(
                    &tmp.path().join("src").join(dir),
                    app.as_str(),
                    "1.0.0",
                    "",
                    "hello",
                );
                let plan = m
                    .install_plan(&src, Provenance::Local, TrustLevel::Trusted)
                    .unwrap();
                m.install(
                    &plan.approve(Approval {
                        approver: "t".into(),
                        approved_ms: 1,
                    }),
                    &src,
                    1,
                )
                .unwrap();
            }
            m.start(&run).unwrap();
            // "崩溃":不做任何清理,直接丢掉 gateway 与 manager
            drop(m);
            Arc::try_unwrap(gw).ok().unwrap().shutdown().await;
        }
        let gw2 = Arc::new(Gateway::start(GatewayConfig { port: 0 }).await.unwrap());
        let m2 = AppManager::new(&root, HOST, gw2.clone()).unwrap();
        let before = m2.list().unwrap();
        let stale = before.iter().find(|a| a.id == run).unwrap();
        assert_eq!(
            stale.observed,
            ObservedState::Running,
            "磁盘上是崩溃前的陈旧观察态"
        );
        assert!(!gw2.has_site(&run), "但新的 gateway 里没有站点");

        assert!(m2.reconcile().is_empty());
        assert!(gw2.has_site(&run), "desired=Running 的应用被重新启动");
        assert!(!gw2.has_site(&idle), "desired=Stopped 的保持停止");
        let after = m2.list().unwrap();
        assert_eq!(
            after.iter().find(|a| a.id == run).unwrap().observed,
            ObservedState::Running
        );
        assert_eq!(
            after.iter().find(|a| a.id == idle).unwrap().observed,
            ObservedState::Installed
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_reports_a_broken_app_without_blocking_the_others() {
        let rig = rig().await;
        let (good, bad) = (id("good"), id("bad"));
        rig.install(&write_app(&rig.src_dir("good"), "good", "1.0.0", "", "ok"))
            .unwrap();
        rig.install(&write_app(&rig.src_dir("bad"), "bad", "1.0.0", "", "ok"))
            .unwrap();
        rig.manager.start(&good).unwrap();
        rig.manager.start(&bad).unwrap();
        // 模拟崩溃后:坏应用的站点目录被删,两个应用都停在"想运行"
        rig.gateway.remove_site(&good);
        rig.gateway.remove_site(&bad);
        let pkg = rig
            .manager
            .registry
            .paths()
            .package_dir(&bad, &Version::new(1, 0, 0));
        fs::remove_dir_all(pkg.join("web")).unwrap();

        let failures = rig.manager.reconcile();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, bad);
        assert!(matches!(failures[0].1, ManagerError::MissingSource(_)));
        assert!(rig.gateway.has_site(&good));
        assert!(!rig.gateway.has_site(&bad));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listing_falls_back_to_the_id_when_the_manifest_cannot_be_read() {
        let rig = rig().await;
        let app = id("excalidraw");
        rig.install(&write_app(
            &rig.src_dir("a"),
            "excalidraw",
            "1.0.0",
            "",
            "hi",
        ))
        .unwrap();
        // 清单从**当前版本的包**里读(包里本来就拷着 manifest.toml),不再依赖一份共享的应用级文件
        fs::write(
            rig.manager
                .registry
                .paths()
                .package_dir(&app, &Version::new(1, 0, 0))
                .join("manifest.toml"),
            "not toml {{{",
        )
        .unwrap();
        assert_eq!(rig.manager.list().unwrap()[0].name, "excalidraw");
    }

    /// 安装的每个失败出口(摘要对不上、同版本、应用在运行)都不能留下 staging;而且保存下来的 manifest
    /// 是 staging 副本里被 verify 过的那份。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_failed_install_cleans_up_and_the_saved_manifest_is_the_staged_one() {
        let rig = rig().await;
        let dir = rig.src_dir("a");
        let src = write_app(&dir, "excalidraw", "1.0.0", "", "one");
        rig.install(&src).unwrap();
        // 清单就在落位的包里(staging 副本被 verify 过的那份);不再另存一份应用级的 manifest.toml
        let saved = fs::read_to_string(
            rig.manager
                .registry
                .paths()
                .package_dir(&id("excalidraw"), &Version::new(1, 0, 0))
                .join("manifest.toml"),
        )
        .unwrap();
        assert!(
            !rig.manager
                .registry
                .paths()
                .manifest_path(&id("excalidraw"))
                .exists()
        );
        assert_eq!(
            saved,
            fs::read_to_string(dir.join("manifest.toml")).unwrap()
        );

        assert!(matches!(
            rig.install(&src),
            Err(ManagerError::AlreadyInstalled(_))
        ));
        assert_no_leftovers(&rig);

        rig.manager.start(&id("excalidraw")).unwrap();
        let v2 = write_app(&rig.src_dir("v2"), "excalidraw", "1.1.0", "", "two");
        assert!(matches!(rig.install(&v2), Err(ManagerError::Busy(_))));
        assert_no_leftovers(&rig);

        let unsupported = rig.src_dir("py");
        write_files(
            &unsupported,
            &[(
                "manifest.toml",
                &manifest_toml("py", "1.0.0", "").replace(
                    "kind = \"static_web\"\nsource = \"web/\"",
                    "kind = \"python\"\ncommand = [\"p\"]\n[runtime.http]\nport_env = \"PORT\"",
                ),
            )],
        );
        assert!(
            rig.manager
                .install(&rig.approve(&src), &AppSource::LocalDir(unsupported), 1)
                .is_err()
        );
        assert_no_leftovers(&rig);
    }

    /// 落位(rename)之后的任何一步失败,都要把刚落位的包目录撤回——否则重试永远撞上"目录已存在"。
    /// 这里用"`data` 被一个同名文件占着"让 `registry.save` 在 rename 之后失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failure_after_the_rename_rolls_the_package_back_so_a_retry_works() {
        let rig = rig().await;
        let app = id("excalidraw");
        let src = write_app(&rig.src_dir("a"), "excalidraw", "1.0.0", "", "hi");
        let blocker = rig.manager.registry.paths().data_dir(&app);
        fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        fs::write(&blocker, "i am a file, not a directory").unwrap();

        assert!(matches!(rig.install(&src), Err(ManagerError::Io(_))));
        let pkg = rig
            .manager
            .registry
            .paths()
            .package_dir(&app, &Version::new(1, 0, 0));
        assert!(!pkg.exists(), "失败后包目录必须撤回");
        assert!(!rig.manager.registry.paths().state_path(&app).exists());
        assert_no_leftovers(&rig);

        fs::remove_file(&blocker).unwrap();
        rig.install(&src).expect("障碍移走后重试必须成功");
        assert_eq!(rig.manager.list().unwrap().len(), 1);
    }

    /// 上一次崩溃留下的"包目录存在但没有版本记录"的残骸,不能挡住重装。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debris_from_a_crashed_install_never_blocks_a_retry_or_an_upgrade() {
        let rig = rig().await;
        let app = id("excalidraw");
        // 全新安装:残骸目录已经在了
        let debris = rig
            .manager
            .registry
            .paths()
            .package_dir(&app, &Version::new(1, 0, 0));
        write_files(&debris, &[("junk.txt", "left over")]);
        rig.install(&write_app(
            &rig.src_dir("v1"),
            "excalidraw",
            "1.0.0",
            "",
            "one",
        ))
        .unwrap();
        assert!(!debris.join("junk.txt").exists(), "残骸被清掉,落位的是新包");
        assert!(debris.join("web/index.html").is_file());

        // 升级:新版本目录的残骸
        let debris2 = rig
            .manager
            .registry
            .paths()
            .package_dir(&app, &Version::new(1, 1, 0));
        write_files(&debris2, &[("junk.txt", "left over")]);
        rig.install(&write_app(
            &rig.src_dir("v2"),
            "excalidraw",
            "1.1.0",
            "",
            "two",
        ))
        .unwrap();
        assert!(!debris2.join("junk.txt").exists());
        rig.manager.start(&app).unwrap();
        assert!(rig.fetch(&app, "/").text().contains("two"));
    }
}
