//! 可注入的凭据、元数据与 HTTP 后端。
//!
//! 默认只提供 trait 与内存 fake;真实 Keychain / HTTP 由 feature 显式打开。
//! 产品必须注入自己的服务名 / 元数据路径,共享实现不得读取任何产品目录。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::model::{GitAccountError, GitAccountsState, GitProvider, SecretToken};

/// 凭据后端(Keychain / 测试 fake)。
pub trait CredentialStore: Send + Sync {
    fn get(&self, provider: GitProvider) -> Result<Option<SecretToken>, GitAccountError>;
    fn set(&self, provider: GitProvider, token: &SecretToken) -> Result<(), GitAccountError>;
    fn delete(&self, provider: GitProvider) -> Result<(), GitAccountError>;
}

/// 账户元数据后端(JSON 文件 / 测试 fake)。
///
/// `update` 是**读改写事务**:实现必须保证并发调用不会丢更新。连接 / 断开
/// 一律走它;`load` / `save` 保留给查询与兼容调用。
pub trait MetadataStore: Send + Sync {
    fn load(&self) -> Result<GitAccountsState, GitAccountError>;
    fn save(&self, state: &GitAccountsState) -> Result<(), GitAccountError>;

    fn update(
        &self,
        mutate: &mut dyn FnMut(&mut GitAccountsState),
    ) -> Result<GitAccountsState, GitAccountError> {
        let mut state = self.load()?;
        mutate(&mut state);
        self.save(&state)?;
        Ok(state)
    }
}

/// 内存凭据后端:仅用于测试与最小示例。
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    entries: Mutex<[Option<SecretToken>; 3]>,
}

fn slot_index(provider: GitProvider) -> usize {
    match provider {
        GitProvider::GitHub => 0,
        GitProvider::GitLab => 1,
        GitProvider::Gitee => 2,
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, provider: GitProvider) -> Result<Option<SecretToken>, GitAccountError> {
        let entries = self.entries.lock().expect("credential lock");
        Ok(entries[slot_index(provider)].clone())
    }

    fn set(&self, provider: GitProvider, token: &SecretToken) -> Result<(), GitAccountError> {
        let mut entries = self.entries.lock().expect("credential lock");
        entries[slot_index(provider)] = Some(token.clone());
        Ok(())
    }

    fn delete(&self, provider: GitProvider) -> Result<(), GitAccountError> {
        let mut entries = self.entries.lock().expect("credential lock");
        entries[slot_index(provider)] = None;
        Ok(())
    }
}

/// 内存元数据后端:仅用于测试与最小示例。
#[derive(Debug, Default)]
pub struct MemoryMetadataStore {
    state: Mutex<GitAccountsState>,
}

impl MetadataStore for MemoryMetadataStore {
    fn load(&self) -> Result<GitAccountsState, GitAccountError> {
        Ok(self.state.lock().expect("metadata lock").clone())
    }

    fn save(&self, state: &GitAccountsState) -> Result<(), GitAccountError> {
        *self.state.lock().expect("metadata lock") = state.clone();
        Ok(())
    }

    fn update(
        &self,
        mutate: &mut dyn FnMut(&mut GitAccountsState),
    ) -> Result<GitAccountsState, GitAccountError> {
        let mut state = self.state.lock().expect("metadata lock");
        mutate(&mut state);
        Ok(state.clone())
    }
}

/// 产品注入路径的 JSON 元数据后端;schema 与旧 `git_accounts.json` 完全一致。
#[derive(Debug, Clone)]
pub struct JsonFileMetadataStore {
    path: PathBuf,
}

impl JsonFileMetadataStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// 进程内按路径共享事务锁:不同 `JsonFileMetadataStore` 实例(每个请求新建)
/// 也必须在同一把锁上完成 load-modify-save,否则两个 provider 会互相覆盖。
fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().expect("metadata path lock registry");
    locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn read_state(path: &Path) -> Result<GitAccountsState, GitAccountError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| GitAccountError::MetadataRead {
            detail: e.to_string(),
        }),
        // 文件不存在 = 从未连接,与旧实现一致。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(GitAccountsState::default())
        }
        Err(error) => Err(GitAccountError::MetadataRead {
            detail: error.to_string(),
        }),
    }
}

fn write_state_atomically(path: &Path, state: &GitAccountsState) -> Result<(), GitAccountError> {
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| GitAccountError::MetadataWrite {
            detail: e.to_string(),
        })?;
    }
    let json = serde_json::to_string_pretty(state).map_err(|e| GitAccountError::MetadataWrite {
        detail: e.to_string(),
    })?;

    // 同目录临时文件 + 同步 + 原子替换:进程中止或并发读都不会看到半截 JSON。
    let temp = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("git_accounts.json"),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(GitAccountError::MetadataWrite {
            detail: error.to_string(),
        });
    }
    Ok(())
}

impl JsonFileMetadataStore {
    fn save_locked(&self, state: &GitAccountsState) -> Result<(), GitAccountError> {
        let lock = path_lock(&self.path);
        let _guard = lock.lock().expect("metadata path lock");
        write_state_atomically(&self.path, state)
    }
}

impl MetadataStore for JsonFileMetadataStore {
    fn load(&self) -> Result<GitAccountsState, GitAccountError> {
        read_state(&self.path)
    }

    fn save(&self, state: &GitAccountsState) -> Result<(), GitAccountError> {
        self.save_locked(state)
    }

    fn update(
        &self,
        mutate: &mut dyn FnMut(&mut GitAccountsState),
    ) -> Result<GitAccountsState, GitAccountError> {
        let lock = path_lock(&self.path);
        let _guard = lock.lock().expect("metadata path lock");
        let mut state = read_state(&self.path)?;
        mutate(&mut state);
        write_state_atomically(&self.path, &state)?;
        Ok(state)
    }
}

/// macOS Keychain 凭据后端:服务名由产品注入(`dozer-git` / `digger-git`)。
#[cfg(feature = "keyring")]
#[derive(Debug, Clone)]
pub struct KeyringCredentialStore {
    service: String,
}

#[cfg(feature = "keyring")]
impl KeyringCredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, provider: GitProvider) -> Result<keyring::Entry, GitAccountError> {
        keyring::Entry::new(&self.service, provider.as_key()).map_err(|e| {
            GitAccountError::CredentialRead {
                detail: e.to_string(),
            }
        })
    }
}

#[cfg(feature = "keyring")]
impl CredentialStore for KeyringCredentialStore {
    fn get(&self, provider: GitProvider) -> Result<Option<SecretToken>, GitAccountError> {
        match self.entry(provider)?.get_password() {
            Ok(token) => Ok(Some(SecretToken::new(token))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(GitAccountError::CredentialRead {
                detail: error.to_string(),
            }),
        }
    }

    fn set(&self, provider: GitProvider, token: &SecretToken) -> Result<(), GitAccountError> {
        self.entry(provider)?
            .set_password(token.expose())
            .map_err(|e| GitAccountError::CredentialWrite {
                detail: e.to_string(),
            })
    }

    fn delete(&self, provider: GitProvider) -> Result<(), GitAccountError> {
        match self.entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(GitAccountError::CredentialDelete {
                detail: error.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConnectedAccount;

    #[test]
    fn memory_credential_store_round_trips_and_deletes() {
        let store = MemoryCredentialStore::default();
        assert_eq!(store.get(GitProvider::GitHub).unwrap(), None);
        store
            .set(GitProvider::GitHub, &SecretToken::new("tok"))
            .unwrap();
        assert_eq!(
            store.get(GitProvider::GitHub).unwrap().unwrap().expose(),
            "tok"
        );
        store.delete(GitProvider::GitHub).unwrap();
        assert_eq!(store.get(GitProvider::GitHub).unwrap(), None);
    }

    #[test]
    fn json_metadata_missing_file_is_empty_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileMetadataStore::new(dir.path().join("git_accounts.json"));
        assert!(store.load().unwrap().is_empty());
        let mut state = GitAccountsState::default();
        state.set(
            GitProvider::GitHub,
            Some(ConnectedAccount {
                username: "octocat".into(),
            }),
        );
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
    }

    #[test]
    fn concurrent_json_updates_keep_every_provider() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git_accounts.json");
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for provider in [GitProvider::GitHub, GitProvider::Gitee] {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let store = JsonFileMetadataStore::new(path);
                barrier.wait();
                store
                    .update(&mut |state| {
                        state.set(
                            provider,
                            Some(ConnectedAccount {
                                username: provider.as_key().to_owned(),
                            }),
                        );
                    })
                    .unwrap();
            }));
        }
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let state = JsonFileMetadataStore::new(&path).load().unwrap();
        assert_eq!(state.get(GitProvider::GitHub).unwrap().username, "github");
        assert_eq!(state.get(GitProvider::Gitee).unwrap().username, "gitee");
    }

    #[cfg(unix)]
    #[test]
    fn failed_publish_keeps_the_previous_file_intact() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git_accounts.json");
        let store = JsonFileMetadataStore::new(&path);
        let mut initial = GitAccountsState::default();
        initial.set(
            GitProvider::GitHub,
            Some(ConnectedAccount {
                username: "keep-me".into(),
            }),
        );
        store.save(&initial).unwrap();

        // 目录不可写:临时文件 / rename 必然失败,原文件不能被截断。
        let mode = dir.path().metadata().unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = store.update(&mut |state| {
            state.set(
                GitProvider::Gitee,
                Some(ConnectedAccount {
                    username: "new".into(),
                }),
            );
        });
        std::fs::set_permissions(dir.path(), mode).unwrap();

        assert!(matches!(result, Err(GitAccountError::MetadataWrite { .. })));
        assert_eq!(
            store.load().unwrap().github.unwrap().username,
            "keep-me",
            "发布失败不能损坏上一份元数据"
        );
        assert!(store.load().unwrap().gitee.is_none());
    }

    #[test]
    fn json_metadata_corrupt_file_is_reported_not_silently_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git_accounts.json");
        std::fs::write(&path, "not json").unwrap();
        let store = JsonFileMetadataStore::new(path);
        assert!(matches!(
            store.load(),
            Err(GitAccountError::MetadataRead { .. })
        ));
    }
}
