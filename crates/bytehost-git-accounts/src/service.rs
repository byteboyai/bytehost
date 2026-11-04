//! 账户查询 / 连接 / 断开 / 仓库列表的服务实现,以及请求身份台账。
//!
//! 服务不持有窗口、不弹 UI、不读产品路径;凭据 / 元数据 / HTTP 全部由产品注入。
//! 网络校验在持久化之前完成;一旦凭据写入即视为已发生事实,后续元数据失败会
//! 作为可诊断的部分结果返回,而不是假装整体失败。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::model::{
    AccountSnapshot, CommitOutcome, ConnectedAccount, GitAccountError, GitAccountsState,
    GitProvider, RemoteRepo, SecretToken,
};
use crate::providers::HttpRequest;
use crate::stores::{CredentialStore, MetadataStore};
use crate::transport::HttpTransport;

/// 一次账户请求的身份。同一 owner + provider 只有最新 ticket 的结果有效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTicket {
    pub owner: u64,
    pub provider: GitProvider,
    pub seq: u64,
}

/// 服务层提交准入:同一 owner + provider 只有最新请求可以进入持久化。
///
/// UI 的 `RequestLedger` 只决定“回包是否展示”;这里决定“事实是否允许落盘”。
/// 两者必须同时使用,否则取消 / 乱序请求仍会覆盖凭据。`Admission` 由产品
/// 在长寿命状态里持有一个 `Arc`,每个请求新建的 `AccountService` 共享它。
#[derive(Debug, Default)]
pub struct Admission {
    latest: Mutex<HashMap<GitProvider, (u64, u64)>>,
    cancelled: Mutex<HashSet<(u64, u64)>>,
    // Shared by every service instance for this product. Cancellation stays cheap;
    // durable mutations for one provider finish in order, including Keychain I/O.
    commit_locks: [Mutex<()>; 3],
}

impl Admission {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin_commit(
        &self,
        provider: GitProvider,
        ticket: Option<RequestTicket>,
    ) -> Result<std::sync::MutexGuard<'_, ()>, GitAccountError> {
        let slot = match provider {
            GitProvider::GitHub => 0,
            GitProvider::GitLab => 1,
            GitProvider::Gitee => 2,
        };
        let guard = self.commit_locks[slot]
            .lock()
            .expect("provider commit lock");
        // Recheck after waiting: an obsolete queued request must never start I/O.
        if let Some(ticket) = ticket
            && (ticket.provider != provider || !self.admit(ticket))
        {
            return Err(GitAccountError::StaleRequest);
        }
        Ok(guard)
    }

    /// 登记请求为当前最新;必须在派生异步任务之前调用。
    pub fn register(&self, ticket: RequestTicket) {
        self.latest
            .lock()
            .expect("admission lock")
            .insert(ticket.provider, (ticket.owner, ticket.seq));
    }

    pub fn admit(&self, ticket: RequestTicket) -> bool {
        let key = (ticket.owner, ticket.seq);
        if self
            .cancelled
            .lock()
            .expect("admission lock")
            .contains(&key)
        {
            return false;
        }
        self.latest
            .lock()
            .expect("admission lock")
            .get(&ticket.provider)
            .is_some_and(|current| *current == key)
    }

    pub fn cancel(&self, ticket: RequestTicket) {
        self.cancelled
            .lock()
            .expect("admission lock")
            .insert((ticket.owner, ticket.seq));
    }

    /// 关窗 / 换会话:该 owner 的所有在途请求都不能再进入持久化。
    pub fn cancel_owner(&self, owner: u64) {
        let mut latest = self.latest.lock().expect("admission lock");
        let mut cancelled = self.cancelled.lock().expect("admission lock");
        latest.retain(|_, current| {
            if current.0 == owner {
                cancelled.insert(*current);
                false
            } else {
                true
            }
        });
    }
}

/// 请求台账:产品在关闭 / 关窗 / 新请求时用它过滤乱序回包。
#[derive(Debug, Default)]
pub struct RequestLedger {
    next: u64,
    active: HashMap<(u64, GitProvider), u64>,
}

impl RequestLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&mut self, owner: u64, provider: GitProvider) -> RequestTicket {
        self.next = self.next.saturating_add(1).max(1);
        self.active.insert((owner, provider), self.next);
        RequestTicket {
            owner,
            provider,
            seq: self.next,
        }
    }

    pub fn is_current(&self, ticket: RequestTicket) -> bool {
        self.active.get(&(ticket.owner, ticket.provider)) == Some(&ticket.seq)
    }

    /// 取消请求但不改变当前 ticket(结果仍可被识别为旧)。
    pub fn cancel(&mut self, ticket: RequestTicket) {
        if self.is_current(ticket) {
            self.active.remove(&(ticket.owner, ticket.provider));
        }
    }

    /// 关闭窗口 / 切换 owner 时让该 owner 的所有在途结果失效。
    pub fn cancel_owner(&mut self, owner: u64) {
        self.active.retain(|(key_owner, _), _| *key_owner != owner);
    }
}

/// 三后端组合的账户服务。
pub struct AccountService<C, M, H> {
    credentials: C,
    metadata: M,
    http: H,
    admission: Arc<Admission>,
}

impl<C, M, H> AccountService<C, M, H>
where
    C: CredentialStore,
    M: MetadataStore,
    H: HttpTransport,
{
    pub fn new(credentials: C, metadata: M, http: H) -> Self {
        Self::new_with_admission(credentials, metadata, http, Arc::new(Admission::new()))
    }

    /// 多个请求 / 多个 service 实例共享同一提交准入。
    pub fn new_with_admission(
        credentials: C,
        metadata: M,
        http: H,
        admission: Arc<Admission>,
    ) -> Self {
        Self {
            credentials,
            metadata,
            http,
            admission,
        }
    }

    pub fn admission(&self) -> &Arc<Admission> {
        &self.admission
    }

    /// 校验 PAT 并返回用户名。网络校验发生在任何持久化之前,可安全取消。
    pub async fn validate_token(
        &self,
        provider: GitProvider,
        token: &SecretToken,
    ) -> Result<ConnectedAccount, GitAccountError> {
        let response = self
            .http
            .send(provider.whoami_request(token.expose()))
            .await?;
        if !response.is_success() {
            return Err(GitAccountError::TokenInvalid);
        }
        provider.extract_username(&response.body)
    }

    /// 校验 → 写凭据 → 写元数据。凭据成功后元数据失败返回部分结果。
    pub async fn connect(
        &self,
        provider: GitProvider,
        token: &SecretToken,
    ) -> Result<CommitOutcome, GitAccountError> {
        self.connect_with_account(provider, token)
            .await
            .map(|(_, outcome)| outcome)
    }

    /// 同 [`Self::connect`],额外返回校验得到的账户名,供 UI 立即展示。
    pub async fn connect_with_account(
        &self,
        provider: GitProvider,
        token: &SecretToken,
    ) -> Result<(ConnectedAccount, CommitOutcome), GitAccountError> {
        self.connect_with_account_at(provider, token, None).await
    }

    /// 带请求身份的连接:网络校验后、写凭据前检查提交准入。
    /// 取消 / 被更新的同 provider 请求不会开始持久化;一旦开始写入即完成收尾。
    pub async fn connect_with_account_at(
        &self,
        provider: GitProvider,
        token: &SecretToken,
        ticket: Option<RequestTicket>,
    ) -> Result<(ConnectedAccount, CommitOutcome), GitAccountError> {
        let account = self.validate_token(provider, token).await?;

        let _commit = self.admission.begin_commit(provider, ticket)?;

        self.credentials.set(provider, token)?;

        let mut outcome = CommitOutcome {
            provider,
            credential_committed: true,
            metadata_committed: false,
            warning: None,
        };
        match self.metadata.update(&mut |state| {
            state.set(provider, Some(account.clone()));
        }) {
            Ok(_) => outcome.metadata_committed = true,
            Err(error) => outcome.warning = Some(error),
        }
        Ok((account, outcome))
    }

    /// 以凭据是否存在为权威事实校正元数据里残留的账户。
    pub fn query(&self) -> Result<AccountSnapshot, GitAccountError> {
        let mut accounts = self.metadata.load()?;
        let mut stale_providers = Vec::new();
        let mut unreconciled_providers = Vec::new();
        for provider in GitProvider::ALL {
            // Keychain may have committed while metadata did not. Inspect all
            // providers, including the ones missing from the metadata snapshot.
            match self.credentials.get(provider)? {
                Some(_) if accounts.get(provider).is_none() => {
                    unreconciled_providers.push(provider)
                }
                Some(_) => {}
                None if accounts.get(provider).is_some() => {
                    accounts.set(provider, None);
                    stale_providers.push(provider);
                }
                None => {}
            }
        }
        Ok(AccountSnapshot {
            accounts,
            stale_providers,
            unreconciled_providers,
        })
    }

    /// 显示用的元数据快照(不探测凭据);UI 重开时应优先调用 [`Self::query`]。
    pub fn load_metadata(&self) -> Result<GitAccountsState, GitAccountError> {
        self.metadata.load()
    }

    /// 断开:先删凭据(权威事实),再清元数据;元数据失败仍有部分结果。
    pub fn disconnect(&self, provider: GitProvider) -> Result<CommitOutcome, GitAccountError> {
        self.disconnect_at(provider, None)
    }

    /// 带请求身份的断开;旧请求不会删除新连接。
    pub fn disconnect_at(
        &self,
        provider: GitProvider,
        ticket: Option<RequestTicket>,
    ) -> Result<CommitOutcome, GitAccountError> {
        let _commit = self.admission.begin_commit(provider, ticket)?;
        self.credentials.delete(provider)?;

        let mut outcome = CommitOutcome {
            provider,
            credential_committed: true,
            metadata_committed: false,
            warning: None,
        };
        match self.metadata.update(&mut |state| state.set(provider, None)) {
            Ok(_) => outcome.metadata_committed = true,
            Err(error) => outcome.warning = Some(error),
        }
        Ok(outcome)
    }

    pub async fn list_repos(
        &self,
        provider: GitProvider,
        token: &SecretToken,
    ) -> Result<Vec<RemoteRepo>, GitAccountError> {
        let response = self
            .http
            .send(provider.list_repos_request(token.expose()))
            .await?;
        if !response.is_success() {
            return Err(GitAccountError::RepoListTokenExpired);
        }
        provider.parse_repo_list(&response.body)
    }

    /// 供测试 / 高级消费者直接发请求(仍走同一 provider 构造)。
    pub async fn send(
        &self,
        request: HttpRequest,
    ) -> Result<crate::providers::HttpResponse, GitAccountError> {
        self.http.send(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::HttpResponse;
    use crate::stores::{MemoryCredentialStore, MemoryMetadataStore};
    use crate::transport::FakeTransport;
    use std::sync::Mutex;

    fn service(
        http: FakeTransport,
    ) -> AccountService<MemoryCredentialStore, MemoryMetadataStore, FakeTransport> {
        AccountService::new(
            MemoryCredentialStore::default(),
            MemoryMetadataStore::default(),
            http,
        )
    }

    #[tokio::test]
    async fn connect_validates_then_persists() {
        let http = FakeTransport::new().push_json(
            "https://api.github.com/user",
            200,
            r#"{"login":"octocat"}"#,
        );
        let service = service(http);
        let outcome = service
            .connect(GitProvider::GitHub, &SecretToken::new("tok"))
            .await
            .unwrap();
        assert!(outcome.is_complete());
        assert_eq!(
            service.query().unwrap().accounts.github.unwrap().username,
            "octocat"
        );
        // 请求里带的是 GitHub 的 Bearer 头。
        let request = &service.http.requests()[0];
        assert!(
            request
                .headers
                .contains(&("Authorization".into(), "Bearer tok".into()))
        );
    }

    #[tokio::test]
    async fn invalid_token_never_writes_credentials() {
        let http = FakeTransport::new().push_json(
            "https://api.github.com/user",
            401,
            r#"{"message":"Bad credentials"}"#,
        );
        let service = service(http);
        let error = service
            .connect(GitProvider::GitHub, &SecretToken::new("bad"))
            .await
            .unwrap_err();
        assert_eq!(error, GitAccountError::TokenInvalid);
        assert_eq!(service.credentials.get(GitProvider::GitHub).unwrap(), None);
        assert!(service.query().unwrap().accounts.is_empty());
    }

    #[derive(Default)]
    struct FailingMetadata {
        inner: MemoryMetadataStore,
        fail_saves: Mutex<bool>,
    }

    impl MetadataStore for FailingMetadata {
        fn load(&self) -> Result<GitAccountsState, GitAccountError> {
            self.inner.load()
        }

        fn save(&self, state: &GitAccountsState) -> Result<(), GitAccountError> {
            if *self.fail_saves.lock().unwrap() {
                return Err(GitAccountError::MetadataWrite {
                    detail: "disk full".into(),
                });
            }
            self.inner.save(state)
        }
    }

    #[tokio::test]
    async fn metadata_failure_is_a_visible_partial_commit() {
        let http = FakeTransport::new().push_json(
            "https://api.github.com/user",
            200,
            r#"{"login":"octocat"}"#,
        );
        let metadata = FailingMetadata::default();
        *metadata.fail_saves.lock().unwrap() = true;
        let service = AccountService::new(MemoryCredentialStore::default(), metadata, http);

        let outcome = service
            .connect(GitProvider::GitHub, &SecretToken::new("tok"))
            .await
            .unwrap();
        assert!(!outcome.is_complete());
        assert!(outcome.credential_committed);
        assert!(!outcome.metadata_committed);
        assert!(matches!(
            outcome.warning,
            Some(GitAccountError::MetadataWrite { .. })
        ));
        // 凭据已是事实,查询不会假装未连接元数据之外的东西。
        assert_eq!(
            service.credentials.get(GitProvider::GitHub).unwrap(),
            Some(SecretToken::new("tok"))
        );
    }

    #[tokio::test]
    async fn query_drops_metadata_without_credentials() {
        let http = FakeTransport::new();
        let metadata = MemoryMetadataStore::default();
        let mut state = GitAccountsState::default();
        state.set(
            GitProvider::GitLab,
            Some(ConnectedAccount {
                username: "stale".into(),
            }),
        );
        metadata.save(&state).unwrap();
        let service = AccountService::new(MemoryCredentialStore::default(), metadata, http);

        let snapshot = service.query().unwrap();
        assert_eq!(snapshot.accounts.get(GitProvider::GitLab), None);
        assert_eq!(snapshot.stale_providers, vec![GitProvider::GitLab]);
    }

    #[tokio::test]
    async fn disconnect_removes_credential_and_metadata() {
        let http = FakeTransport::new().push_json(
            "https://api.github.com/user",
            200,
            r#"{"login":"octocat"}"#,
        );
        let service = service(http);
        service
            .connect(GitProvider::GitHub, &SecretToken::new("tok"))
            .await
            .unwrap();
        let outcome = service.disconnect(GitProvider::GitHub).unwrap();
        assert!(outcome.is_complete());
        assert_eq!(service.credentials.get(GitProvider::GitHub).unwrap(), None);
        assert!(service.query().unwrap().accounts.is_empty());
    }

    #[tokio::test]
    async fn disconnect_of_unknown_account_is_idempotent() {
        let service = service(FakeTransport::new());
        assert!(
            service
                .disconnect(GitProvider::Gitee)
                .unwrap()
                .is_complete()
        );
        assert!(
            service
                .disconnect(GitProvider::Gitee)
                .unwrap()
                .is_complete()
        );
    }

    #[tokio::test]
    async fn repo_list_rejects_expired_token() {
        let http = FakeTransport::new().push_json("https://api.github.com/user/repos", 403, "[]");
        let service = service(http);
        assert_eq!(
            service
                .list_repos(GitProvider::GitHub, &SecretToken::new("x"))
                .await,
            Err(GitAccountError::RepoListTokenExpired)
        );
    }

    #[tokio::test]
    async fn repo_list_parses_provider_shape() {
        let http = FakeTransport::new().push_json(
            "https://gitee.com/api/v5/user/repos",
            200,
            r#"[{"full_name":"a/demo","html_url":"https://gitee.com/a/demo"}]"#,
        );
        let service = service(http);
        let repos = service
            .list_repos(GitProvider::Gitee, &SecretToken::new("x"))
            .await
            .unwrap();
        assert_eq!(repos[0].full_name, "a/demo");
    }

    #[tokio::test]
    async fn cancelled_request_never_writes_credentials_or_metadata() {
        let http = FakeTransport::new().push_json(
            "https://api.github.com/user",
            200,
            r#"{"login":"octocat"}"#,
        );
        let service = service(http);
        let ticket = RequestTicket {
            owner: 1,
            provider: GitProvider::GitHub,
            seq: 7,
        };
        service.admission().register(ticket);
        service.admission().cancel(ticket);

        assert_eq!(
            service
                .connect_with_account_at(
                    GitProvider::GitHub,
                    &SecretToken::new("tok"),
                    Some(ticket)
                )
                .await,
            Err(GitAccountError::StaleRequest)
        );
        assert_eq!(service.credentials.get(GitProvider::GitHub).unwrap(), None);
        assert!(service.metadata.load().unwrap().is_empty());
    }

    /// 网络阶段可被打断的传输:用于证明取消发生在校验之后、持久化之前。
    struct HeldTransport {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl HttpTransport for HeldTransport {
        async fn send(&self, _request: HttpRequest) -> Result<HttpResponse, GitAccountError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(HttpResponse {
                status: 200,
                body: r#"{"login":"late"}"#.into(),
            })
        }
    }

    #[tokio::test]
    async fn cancelling_during_the_network_phase_never_starts_persisting() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let service = AccountService::new(
            MemoryCredentialStore::default(),
            MemoryMetadataStore::default(),
            HeldTransport {
                entered: entered.clone(),
                release: release.clone(),
            },
        );
        let ticket = RequestTicket {
            owner: 1,
            provider: GitProvider::GitHub,
            seq: 11,
        };
        service.admission().register(ticket);

        let token = SecretToken::new("tok");
        let connect = service.connect_with_account_at(GitProvider::GitHub, &token, Some(ticket));
        let cancel = async {
            entered.notified().await;
            service.admission().cancel(ticket);
            release.notify_one();
        };
        let (result, ()) = tokio::join!(connect, cancel);

        assert_eq!(result, Err(GitAccountError::StaleRequest));
        assert_eq!(service.credentials.get(GitProvider::GitHub).unwrap(), None);
        assert!(service.metadata.load().unwrap().is_empty());
    }

    #[tokio::test]
    async fn superseded_request_cannot_overwrite_the_newer_connection() {
        let http = FakeTransport::new().push_json(
            "https://api.github.com/user",
            200,
            r#"{"login":"newer"}"#,
        );
        let service = service(http);
        let older = RequestTicket {
            owner: 1,
            provider: GitProvider::GitHub,
            seq: 1,
        };
        let newer = RequestTicket {
            owner: 1,
            provider: GitProvider::GitHub,
            seq: 2,
        };
        service.admission().register(older);
        service.admission().register(newer);

        assert_eq!(
            service
                .connect_with_account_at(GitProvider::GitHub, &SecretToken::new("old"), Some(older))
                .await,
            Err(GitAccountError::StaleRequest)
        );
        service
            .connect_with_account_at(GitProvider::GitHub, &SecretToken::new("new"), Some(newer))
            .await
            .unwrap();
        assert_eq!(
            service
                .credentials
                .get(GitProvider::GitHub)
                .unwrap()
                .unwrap()
                .expose(),
            "new"
        );
        assert_eq!(
            service
                .metadata
                .load()
                .unwrap()
                .get(GitProvider::GitHub)
                .unwrap()
                .username,
            "newer"
        );
    }

    #[tokio::test]
    async fn concurrent_connects_to_two_providers_keep_both_accounts() {
        let http = FakeTransport::new()
            .push_json("https://api.github.com/user", 200, r#"{"login":"octocat"}"#)
            .push_json(
                "https://gitee.com/api/v5/user",
                200,
                r#"{"login":"gitee-user"}"#,
            );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git_accounts.json");
        let make = || {
            AccountService::new(
                MemoryCredentialStore::default(),
                crate::stores::JsonFileMetadataStore::new(&path),
                http.clone(),
            )
        };
        // 每个请求独立 service 实例,与产品实际创建方式一致;并发准入 / 元数据
        // 事务必须仍然完整。
        let (a, b) = futures_join(
            make().connect(GitProvider::GitHub, &SecretToken::new("a")),
            make().connect(GitProvider::Gitee, &SecretToken::new("b")),
        )
        .await;
        a.unwrap();
        b.unwrap();
        let state = crate::stores::JsonFileMetadataStore::new(&path)
            .load()
            .unwrap();
        assert_eq!(state.get(GitProvider::GitHub).unwrap().username, "octocat");
        assert_eq!(
            state.get(GitProvider::Gitee).unwrap().username,
            "gitee-user"
        );
    }

    async fn futures_join<A, B>(a: A, b: B) -> (A::Output, B::Output)
    where
        A: std::future::Future,
        B: std::future::Future,
    {
        tokio::join!(a, b)
    }

    #[test]
    fn request_ledger_accepts_only_the_latest_ticket() {
        let mut ledger = RequestLedger::new();
        let first = ledger.begin(1, GitProvider::GitHub);
        let second = ledger.begin(1, GitProvider::GitHub);
        assert!(!ledger.is_current(first));
        assert!(ledger.is_current(second));
        // 不同 provider 独立。
        let other = ledger.begin(1, GitProvider::GitLab);
        assert!(ledger.is_current(other));
        ledger.cancel_owner(1);
        assert!(!ledger.is_current(second));
        assert!(!ledger.is_current(other));
    }

    #[test]
    fn unused_http_response_helper_is_available() {
        assert!(
            HttpResponse {
                status: 200,
                body: String::new()
            }
            .is_success()
        );
    }
    #[test]
    fn a_provider_commit_finishes_before_its_newer_request_can_write() {
        use std::sync::{Arc, Mutex};
        struct HeldCredentials {
            values: Mutex<Option<SecretToken>>,
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl CredentialStore for Arc<HeldCredentials> {
            fn get(&self, _: GitProvider) -> Result<Option<SecretToken>, GitAccountError> {
                Ok(self.values.lock().unwrap().clone())
            }
            fn set(&self, _: GitProvider, token: &SecretToken) -> Result<(), GitAccountError> {
                if token.expose() == "SYNTHETIC-OLD" {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                }
                *self.values.lock().unwrap() = Some(token.clone());
                Ok(())
            }
            fn delete(&self, _: GitProvider) -> Result<(), GitAccountError> {
                *self.values.lock().unwrap() = None;
                Ok(())
            }
        }
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let credentials = Arc::new(HeldCredentials {
            values: Mutex::new(None),
            entered,
            release: Mutex::new(resume),
        });
        let dir = tempfile::tempdir().unwrap();
        let metadata_path = dir.path().join("accounts.json");
        let admission = Arc::new(Admission::new());
        let old = RequestTicket {
            owner: 1,
            provider: GitProvider::GitHub,
            seq: 1,
        };
        admission.register(old);
        let c = credentials.clone();
        let a = admission.clone();
        let old_path = metadata_path.clone();
        let old_thread = std::thread::spawn(move || {
            let service = AccountService::new_with_admission(
                c,
                crate::stores::JsonFileMetadataStore::new(old_path),
                FakeTransport::new().push_json("https://", 200, r#"{"login":"old-user"}"#),
                a,
            );
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(service.connect_with_account_at(
                    GitProvider::GitHub,
                    &SecretToken::new("SYNTHETIC-OLD"),
                    Some(old),
                ))
                .unwrap()
        });
        waiting.recv().unwrap(); // old credential mutation is in progress
        assert!(admission.commit_locks[0].try_lock().is_err());
        let new = RequestTicket { seq: 2, ..old };
        admission.register(new);
        let c = credentials.clone();
        let a = admission.clone();
        let new_path = metadata_path.clone();
        let new_thread = std::thread::spawn(move || {
            let service = AccountService::new_with_admission(
                c,
                crate::stores::JsonFileMetadataStore::new(new_path),
                FakeTransport::new().push_json("https://", 200, r#"{"login":"new-user"}"#),
                a,
            );
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(service.connect_with_account_at(
                    GitProvider::GitHub,
                    &SecretToken::new("SYNTHETIC-NEW"),
                    Some(new),
                ))
                .unwrap()
        });
        release.send(()).unwrap();
        assert!(old_thread.join().unwrap().1.is_complete());
        assert!(new_thread.join().unwrap().1.is_complete());
        assert_eq!(
            credentials
                .get(GitProvider::GitHub)
                .unwrap()
                .unwrap()
                .expose(),
            "SYNTHETIC-NEW"
        );
        let metadata = crate::stores::JsonFileMetadataStore::new(metadata_path)
            .load()
            .unwrap();
        assert_eq!(
            metadata.get(GitProvider::GitHub).unwrap().username,
            "new-user"
        );
    }

    #[tokio::test]
    async fn credential_without_metadata_is_visible_and_can_be_disconnected_after_reopening() {
        struct FailedMetadata;
        impl MetadataStore for FailedMetadata {
            fn load(&self) -> Result<GitAccountsState, GitAccountError> {
                Ok(GitAccountsState::default())
            }
            fn save(&self, _: &GitAccountsState) -> Result<(), GitAccountError> {
                Err(GitAccountError::MetadataWrite {
                    detail: "synthetic disk failure".into(),
                })
            }
        }
        let service = AccountService::new(
            MemoryCredentialStore::default(),
            FailedMetadata,
            FakeTransport::new().push_json("https://", 200, r#"{"login":"user"}"#),
        );
        let (_, outcome) = service
            .connect_with_account(GitProvider::GitHub, &SecretToken::new("SYNTHETIC"))
            .await
            .unwrap();
        assert!(outcome.credential_committed && !outcome.metadata_committed);
        let snapshot = service.query().unwrap();
        assert!(snapshot.accounts.get(GitProvider::GitHub).is_none());
        assert_eq!(snapshot.unreconciled_providers, vec![GitProvider::GitHub]);
        service.disconnect(GitProvider::GitHub).unwrap();
        assert!(service.query().unwrap().unreconciled_providers.is_empty());
    }
}
