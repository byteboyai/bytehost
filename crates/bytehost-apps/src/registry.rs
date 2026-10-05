//! 注册表与磁盘布局:`<root>/apps/<app-id>/{manifest.toml, state.json, package/<ver>/, data/, cache/, logs/}`。
//! 程序包、用户数据、缓存、日志分开,卸载"程序"与删除"数据"是两个操作。纯同步文件 I/O,无异步、无网络。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::id::{AppId, Version};
use crate::permissions::Permissions;
use crate::state::{DesiredState, ObservedState};

/// `state.json` 的格式版本。读到不认识的版本就拒绝(不猜),升级格式时递增并写迁移。
pub const RECORD_FORMAT_VERSION: u32 = 1;

/// 一个应用在磁盘上的各个位置。
#[derive(Debug, Clone)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// `root` 是宿主为 bytehost 分配的数据目录。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn apps_dir(&self) -> PathBuf {
        self.root.join("apps")
    }

    pub fn app_dir(&self, id: &AppId) -> PathBuf {
        self.apps_dir().join(id.as_str())
    }

    pub fn manifest_path(&self, id: &AppId) -> PathBuf {
        self.app_dir(id).join("manifest.toml")
    }

    pub fn state_path(&self, id: &AppId) -> PathBuf {
        self.app_dir(id).join("state.json")
    }

    /// 某个版本的不可变程序内容目录。
    pub fn package_dir(&self, id: &AppId, version: &Version) -> PathBuf {
        self.app_dir(id).join("package").join(version.to_string())
    }

    pub fn data_dir(&self, id: &AppId) -> PathBuf {
        self.app_dir(id).join("data")
    }

    pub fn cache_dir(&self, id: &AppId) -> PathBuf {
        self.app_dir(id).join("cache")
    }

    pub fn logs_dir(&self, id: &AppId) -> PathBuf {
        self.app_dir(id).join("logs")
    }
}

/// 一次安装留下的版本记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub version: Version,
    pub manifest_digest: String,
    pub source_digest: String,
    pub installed_ms: u64,
}

/// 持久化的应用记录(`state.json`)。**不含密钥**——密钥只存引用,由宿主的凭据服务注入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRecord {
    /// 见 [`RECORD_FORMAT_VERSION`]。
    pub format_version: u32,
    pub id: AppId,
    pub desired: DesiredState,
    /// 最后一次观察到的状态(supervisor 持久化;重启后经 `recover_after_supervisor_restart` 修正)。
    pub observed: ObservedState,
    /// 当前生效的版本(`versions` 里最后一次安装的那个)。
    pub current_version: Version,
    /// 用户**实际授予**的权限(与 manifest 的"申请"分开存)。
    pub grants: Permissions,
    pub versions: Vec<VersionRecord>,
    /// 每应用 WebView 数据存储标识(32 位十六进制),跨重启、跨卸载重装稳定。
    pub data_store_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UninstallMode {
    /// 只删程序:保留 `data/` 与 `logs/`,重装后数据还在。
    Program,
    /// 连数据一起删:整个应用目录。
    ProgramAndData,
}

/// `list` 的结果:读得出来的记录,加上读不出来的(损坏的 `state.json` 不应遮住别的应用)。
#[derive(Debug, Default)]
pub struct Listing {
    pub apps: Vec<AppRecord>,
    /// (目录名, 问题描述)。
    pub problems: Vec<(String, String)>,
}

pub struct Registry {
    paths: AppPaths,
}

impl Registry {
    /// 打开(必要时创建)`<root>/apps/`。
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let paths = AppPaths::new(root);
        fs::create_dir_all(paths.apps_dir())?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// 全部已登记应用,按 id 排序。没有 `state.json` 的目录(如只保留了数据的残留目录)不算已安装应用。
    pub fn list(&self) -> io::Result<Listing> {
        let mut listing = Listing::default();
        let mut dirs: Vec<_> = fs::read_dir(self.paths.apps_dir())?.collect::<io::Result<_>>()?;
        dirs.sort_by_key(|e| e.file_name());
        for entry in dirs {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let state = entry.path().join("state.json");
            if !state.exists() {
                continue;
            }
            match read_record(&state, &name) {
                Ok(record) => listing.apps.push(record),
                Err(problem) => listing.problems.push((name, problem)),
            }
        }
        Ok(listing)
    }

    pub fn load(&self, id: &AppId) -> io::Result<Option<AppRecord>> {
        let path = self.paths.state_path(id);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let record = parse_record(&text, id.as_str()).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                })?;
                Ok(Some(record))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 原子地写 `state.json`(先写临时文件再改名),并确保 `data/`、`cache/`、`logs/` 存在。
    pub fn save(&self, record: &AppRecord) -> io::Result<()> {
        let id = &record.id;
        for dir in [
            self.paths.data_dir(id),
            self.paths.cache_dir(id),
            self.paths.logs_dir(id),
        ] {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(record).map_err(io::Error::other)?;
        write_atomic(&self.paths.state_path(id), json.as_bytes())
    }

    /// 原子地写 `manifest.toml`(当前版本的申请原文)。
    pub fn save_manifest(&self, id: &AppId, text: &str) -> io::Result<()> {
        fs::create_dir_all(self.paths.app_dir(id))?;
        write_atomic(&self.paths.manifest_path(id), text.as_bytes())
    }

    /// 卸载。应用不存在不算错误(幂等)。
    pub fn uninstall(&self, id: &AppId, mode: UninstallMode) -> io::Result<()> {
        let dir = self.paths.app_dir(id);
        match mode {
            UninstallMode::ProgramAndData => remove_dir_if_exists(&dir),
            UninstallMode::Program => {
                remove_dir_if_exists(&dir.join("package"))?;
                remove_dir_if_exists(&dir.join("cache"))?;
                remove_file_if_exists(&self.paths.manifest_path(id))?;
                // state.json 最后删:它一删,应用就不再出现在 `list` 里
                remove_file_if_exists(&self.paths.state_path(id))
            }
        }
    }
}

fn read_record(path: &Path, dir_name: &str) -> Result<AppRecord, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_record(&text, dir_name)
}

/// 解析并校验一份 `state.json`:格式版本必须认识,记录里的 id 必须与目录名一致。
fn parse_record(text: &str, dir_name: &str) -> Result<AppRecord, String> {
    let record: AppRecord = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if record.format_version != RECORD_FORMAT_VERSION {
        return Err(format!(
            "不支持的 state.json 格式版本 {}(本宿主认识 {RECORD_FORMAT_VERSION})",
            record.format_version
        ));
    }
    if record.id.as_str() != dir_name {
        return Err(format!(
            "记录里的 id {} 与目录名 {dir_name} 不一致",
            record.id
        ));
    }
    Ok(record)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn remove_dir_if_exists(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn remove_file_if_exists(file: &Path) -> io::Result<()> {
    match fs::remove_file(file) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Access;

    fn id(s: &str) -> AppId {
        AppId::new(s).unwrap()
    }

    fn record(app: &str) -> AppRecord {
        AppRecord {
            id: id(app),
            desired: DesiredState::Running,
            current_version: Version::new(0, 17, 0),
            grants: Permissions {
                clipboard: Access::Read,
                ..Permissions::default()
            },
            versions: vec![VersionRecord {
                version: Version::new(0, 17, 0),
                manifest_digest: "m".into(),
                source_digest: "s".into(),
                installed_ms: 5,
            }],
            data_store_id: "0123456789abcdef0123456789abcdef".into(),
            format_version: RECORD_FORMAT_VERSION,
            observed: ObservedState::Stopped,
        }
    }

    #[test]
    fn paths_follow_the_documented_layout() {
        let p = AppPaths::new("/r");
        let a = id("excalidraw");
        assert_eq!(p.app_dir(&a), PathBuf::from("/r/apps/excalidraw"));
        assert_eq!(
            p.manifest_path(&a),
            PathBuf::from("/r/apps/excalidraw/manifest.toml")
        );
        assert_eq!(
            p.state_path(&a),
            PathBuf::from("/r/apps/excalidraw/state.json")
        );
        assert_eq!(
            p.package_dir(&a, &Version::new(0, 17, 0)),
            PathBuf::from("/r/apps/excalidraw/package/0.17.0")
        );
        assert_eq!(p.data_dir(&a), PathBuf::from("/r/apps/excalidraw/data"));
        assert_eq!(p.cache_dir(&a), PathBuf::from("/r/apps/excalidraw/cache"));
        assert_eq!(p.logs_dir(&a), PathBuf::from("/r/apps/excalidraw/logs"));
    }

    #[test]
    fn saving_creates_the_data_cache_and_logs_dirs_and_round_trips_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let r = record("excalidraw");
        reg.save(&r).unwrap();
        for dir in [
            reg.paths().data_dir(&r.id),
            reg.paths().cache_dir(&r.id),
            reg.paths().logs_dir(&r.id),
        ] {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        assert_eq!(reg.load(&r.id).unwrap(), Some(r));
        assert_eq!(reg.load(&id("nope")).unwrap(), None);
    }

    #[test]
    fn saving_twice_replaces_the_record_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let mut r = record("excalidraw");
        reg.save(&r).unwrap();
        r.desired = DesiredState::Stopped;
        reg.save(&r).unwrap();
        assert_eq!(
            reg.load(&r.id).unwrap().unwrap().desired,
            DesiredState::Stopped
        );
        let leftovers: Vec<_> = fs::read_dir(reg.paths().app_dir(&r.id))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn state_json_has_exactly_the_documented_fields_and_no_secret_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        reg.save(&record("excalidraw")).unwrap();
        let text = fs::read_to_string(reg.paths().state_path(&id("excalidraw"))).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "current_version",
                "data_store_id",
                "desired",
                "format_version",
                "grants",
                "id",
                "observed",
                "versions"
            ]
        );
    }

    #[test]
    fn list_returns_apps_sorted_and_reports_corrupt_records_without_hiding_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        reg.save(&record("zeta")).unwrap();
        reg.save(&record("alpha")).unwrap();
        // 损坏的 state.json
        let bad = reg.paths().app_dir(&id("broken"));
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("state.json"), "{not json").unwrap();
        // 目录名与记录 id 不一致
        let liar = reg.paths().app_dir(&id("liar"));
        fs::create_dir_all(&liar).unwrap();
        fs::write(
            liar.join("state.json"),
            serde_json::to_string(&record("alpha")).unwrap(),
        )
        .unwrap();
        // 没有 state.json 的残留目录(如只保留了 data/)与普通文件:忽略
        fs::create_dir_all(reg.paths().app_dir(&id("leftover")).join("data")).unwrap();
        fs::write(reg.paths().apps_dir().join("stray.txt"), "x").unwrap();

        let listing = reg.list().unwrap();
        let ids: Vec<_> = listing
            .apps
            .iter()
            .map(|a| a.id.as_str().to_string())
            .collect();
        assert_eq!(ids, ["alpha", "zeta"]);
        let mut problem_dirs: Vec<_> = listing.problems.iter().map(|(d, _)| d.as_str()).collect();
        problem_dirs.sort();
        assert_eq!(problem_dirs, ["broken", "liar"]);
    }

    #[test]
    fn load_rejects_a_record_whose_id_does_not_match_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let liar = reg.paths().app_dir(&id("liar"));
        fs::create_dir_all(&liar).unwrap();
        fs::write(
            liar.join("state.json"),
            serde_json::to_string(&record("alpha")).unwrap(),
        )
        .unwrap();
        let err = reg.load(&id("liar")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    fn installed_with_everything(reg: &Registry, app: &str) -> AppId {
        let r = record(app);
        reg.save(&r).unwrap();
        reg.save_manifest(&r.id, "x = 1").unwrap();
        let pkg = reg.paths().package_dir(&r.id, &r.current_version);
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("index.html"), "hi").unwrap();
        fs::write(reg.paths().data_dir(&r.id).join("drawing.json"), "{}").unwrap();
        fs::write(reg.paths().cache_dir(&r.id).join("c"), "c").unwrap();
        fs::write(reg.paths().logs_dir(&r.id).join("l.log"), "l").unwrap();
        r.id
    }

    #[test]
    fn uninstalling_the_program_keeps_data_and_logs_and_a_reinstall_finds_them() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let app = installed_with_everything(&reg, "excalidraw");
        reg.uninstall(&app, UninstallMode::Program).unwrap();

        assert_eq!(reg.load(&app).unwrap(), None);
        assert!(reg.list().unwrap().apps.is_empty(), "不再是已安装应用");
        assert!(!reg.paths().app_dir(&app).join("package").exists());
        assert!(!reg.paths().cache_dir(&app).exists());
        assert!(!reg.paths().manifest_path(&app).exists());
        assert_eq!(
            fs::read_to_string(reg.paths().data_dir(&app).join("drawing.json")).unwrap(),
            "{}"
        );
        assert!(reg.paths().logs_dir(&app).join("l.log").exists());

        reg.save(&record("excalidraw")).unwrap();
        assert!(
            reg.paths().data_dir(&app).join("drawing.json").exists(),
            "重装后数据还在"
        );
    }

    #[test]
    fn uninstalling_with_data_removes_the_whole_app_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let app = installed_with_everything(&reg, "excalidraw");
        let other = installed_with_everything(&reg, "drawio");
        reg.uninstall(&app, UninstallMode::ProgramAndData).unwrap();
        assert!(!reg.paths().app_dir(&app).exists());
        assert!(
            reg.paths().data_dir(&other).join("drawing.json").exists(),
            "别的应用不受影响"
        );
    }

    #[test]
    fn uninstalling_something_that_is_not_installed_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        for mode in [UninstallMode::Program, UninstallMode::ProgramAndData] {
            reg.uninstall(&id("ghost"), mode).unwrap();
        }
    }

    #[test]
    fn the_observed_state_and_format_version_survive_a_save_load_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let mut r = record("excalidraw");
        r.observed = ObservedState::Failed {
            reason: "端口被占用".into(),
            retryable: true,
        };
        reg.save(&r).unwrap();
        let back = reg.load(&r.id).unwrap().unwrap();
        assert_eq!(back.observed, r.observed);
        assert_eq!(back.format_version, RECORD_FORMAT_VERSION);
    }

    #[test]
    fn a_state_json_with_an_unknown_format_version_is_refused_not_guessed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open(tmp.path()).unwrap();
        let mut r = record("excalidraw");
        r.format_version = RECORD_FORMAT_VERSION + 1;
        reg.save(&r).unwrap();
        let err = reg.load(&r.id).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let listing = reg.list().unwrap();
        assert!(listing.apps.is_empty());
        assert_eq!(listing.problems.len(), 1);
    }
}
