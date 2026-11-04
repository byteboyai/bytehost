//! 共享 Git 账户页的状态与类型化意图。
//!
//! UI 只产生 [`GitIntent`];服务调用由产品适配器执行,结果经 [`GitPageState`]
//! 的回包方法写回。所有结果都带请求身份:旧请求 / 关窗后的迟到回包不会修改
//! 新会话的输入或账户展示。
//!
//! Token 用 [`SecretToken`] 保存:Debug 打码、关闭页面时清零。

use bytehost_git_accounts::{
    AccountSnapshot, CommitOutcome, ConnectedAccount, GitAccountError, GitAccountsState,
    GitProvider, RequestLedger, RequestTicket, SecretToken,
};

/// 页面产生的类型化意图。产品把它翻译成实际服务调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitIntent {
    /// 用户点“连接”:进入输入态。
    Connect { provider: GitProvider },
    /// 用户提交 Token:执行 校验 → 写凭据 → 写元数据。
    Submit { provider: GitProvider },
    /// 取消输入 / 取消在途请求。
    Cancel { provider: GitProvider },
    /// 断开已有账户。
    Disconnect { provider: GitProvider },
    /// 用系统浏览器打开 PAT 创建页。
    OpenTokenPage { provider: GitProvider },
    /// 重新查询权威账户事实。
    RetryQuery,
}

#[derive(Debug)]
struct ProviderRow {
    account: Option<ConnectedAccount>,
    unreconciled: bool,
    token: SecretToken,
    editing: bool,
    busy: bool,
    error: Option<GitAccountError>,
    ticket: Option<RequestTicket>,
}

impl ProviderRow {
    fn new() -> Self {
        Self {
            account: None,
            unreconciled: false,
            token: SecretToken::default(),
            editing: false,
            busy: false,
            error: None,
            ticket: None,
        }
    }

    fn clear_sensitive(&mut self) {
        self.token = SecretToken::default();
    }
}

/// Git 页共享状态。`owner` 用于隔离不同对话框 / 会话的请求身份。
#[derive(Debug)]
pub struct GitPageState {
    owner: u64,
    rows: [ProviderRow; 3],
    loading: bool,
    query_seq: u64,
    query_ticket: Option<RequestTicket>,
    query_error: Option<GitAccountError>,
    notice: Option<GitAccountError>,
    ledger: RequestLedger,
}

fn index(provider: GitProvider) -> usize {
    match provider {
        GitProvider::GitHub => 0,
        GitProvider::GitLab => 1,
        GitProvider::Gitee => 2,
    }
}

impl GitPageState {
    pub fn new(owner: u64) -> Self {
        Self {
            owner,
            rows: [ProviderRow::new(), ProviderRow::new(), ProviderRow::new()],
            loading: false,
            query_seq: 0,
            query_ticket: None,
            query_error: None,
            notice: None,
            ledger: RequestLedger::new(),
        }
    }

    pub fn owner(&self) -> u64 {
        self.owner
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn query_error(&self) -> Option<&GitAccountError> {
        self.query_error.as_ref()
    }

    pub fn notice(&self) -> Option<&GitAccountError> {
        self.notice.as_ref()
    }

    pub fn account(&self, provider: GitProvider) -> Option<&ConnectedAccount> {
        self.rows[index(provider)].account.as_ref()
    }

    pub fn needs_reconciliation(&self, provider: GitProvider) -> bool {
        self.rows[index(provider)].unreconciled
    }

    pub fn is_busy(&self, provider: GitProvider) -> bool {
        self.rows[index(provider)].busy
    }

    pub fn is_editing(&self, provider: GitProvider) -> bool {
        self.rows[index(provider)].editing
    }

    pub fn error(&self, provider: GitProvider) -> Option<&GitAccountError> {
        self.rows[index(provider)].error.as_ref()
    }

    /// 供受控文本输入读取当前值;不要写入日志 / Debug。
    pub fn token(&self, provider: GitProvider) -> &str {
        self.rows[index(provider)].token.expose()
    }

    pub fn accounts(&self) -> GitAccountsState {
        let mut state = GitAccountsState::default();
        for provider in GitProvider::ALL {
            state.set(provider, self.account(provider).cloned());
        }
        state
    }

    // -- 查询 -------------------------------------------------------------

    /// 打开设置 / 重开页面时先查权威账户事实。返回本次查询的请求身份。
    pub fn begin_query(&mut self) -> RequestTicket {
        self.loading = true;
        self.query_error = None;
        self.query_seq = self.query_seq.saturating_add(1).max(1);
        let ticket = RequestTicket {
            owner: self.owner,
            provider: GitProvider::GitHub,
            seq: self.query_seq,
        };
        self.query_ticket = Some(ticket);
        ticket
    }

    pub fn apply_query(&mut self, ticket: RequestTicket, snapshot: AccountSnapshot) -> bool {
        if !self.accepts_query(ticket) {
            return false;
        }
        self.query_ticket = None;
        self.loading = false;
        self.query_error = None;
        for provider in GitProvider::ALL {
            let row = &mut self.rows[index(provider)];
            row.account = snapshot.accounts.get(provider).cloned();
            row.unreconciled = snapshot.unreconciled_providers.contains(&provider);
            if row.account.is_some() {
                row.clear_sensitive();
            }
        }
        // 残留元数据已按凭据事实校正,不当作操作失败提示。
        true
    }

    pub fn query_failed(&mut self, ticket: RequestTicket, error: GitAccountError) -> bool {
        if !self.accepts_query(ticket) {
            return false;
        }
        self.query_ticket = None;
        self.loading = false;
        self.query_error = Some(error);
        true
    }

    // -- 输入 / 连接 -------------------------------------------------------

    /// 进入输入态。已连接 / 忙碌 / 正在输入时返回 `false`。
    pub fn begin_edit(&mut self, provider: GitProvider) -> bool {
        let row = &mut self.rows[index(provider)];
        if row.busy || row.account.is_some() || row.editing {
            return false;
        }
        row.error = None;
        row.token = SecretToken::default();
        row.editing = true;
        true
    }

    /// 更新输入。忙碌中或已连接时忽略,避免旧回包覆盖新输入。
    pub fn set_token(&mut self, provider: GitProvider, token: impl Into<String>) -> bool {
        let row = &mut self.rows[index(provider)];
        if row.busy || row.account.is_some() {
            return false;
        }
        row.token = SecretToken::new(token);
        row.error = None;
        true
    }

    /// 提交连接:参数校验通过后返回请求身份;空 Token / 已连接 / 忙碌返回 `None`。
    pub fn begin_connect(&mut self, provider: GitProvider) -> Option<RequestTicket> {
        let row = &mut self.rows[index(provider)];
        if row.busy || row.account.is_some() || row.token.is_empty() {
            return None;
        }
        row.busy = true;
        row.error = None;
        let ticket = self.ledger.begin(self.owner, provider);
        row.ticket = Some(ticket);
        Some(ticket)
    }

    /// 连接成功。`outcome` 的部分提交告警写入 notice,账户本身仍然生效。
    pub fn connect_succeeded(
        &mut self,
        ticket: RequestTicket,
        account: ConnectedAccount,
        outcome: CommitOutcome,
    ) -> bool {
        if !self.accepts(ticket) {
            return false;
        }
        let row = &mut self.rows[index(ticket.provider)];
        row.account = Some(account);
        row.unreconciled = !outcome.metadata_committed;
        row.busy = false;
        row.editing = false;
        row.error = None;
        row.clear_sensitive();
        row.ticket = None;
        self.notice = outcome.warning;
        self.ledger.cancel(ticket);
        true
    }

    pub fn connect_failed(&mut self, ticket: RequestTicket, error: GitAccountError) -> bool {
        if !self.accepts(ticket) {
            return false;
        }
        let row = &mut self.rows[index(ticket.provider)];
        row.busy = false;
        row.error = Some(error);
        row.ticket = None;
        self.ledger.cancel(ticket);
        true
    }

    /// 取消输入或在途请求;立即清掉敏感输入并让迟到回包失效。
    /// 返回被取消的请求身份,供产品同步撤销服务层的提交准入。
    pub fn cancel(&mut self, provider: GitProvider) -> Option<RequestTicket> {
        let row = &mut self.rows[index(provider)];
        let ticket = row.ticket.take();
        if let Some(ticket) = ticket {
            self.ledger.cancel(ticket);
        }
        row.busy = false;
        row.editing = false;
        row.error = None;
        row.clear_sensitive();
        ticket
    }

    /// 已被取消 / 被取代的请求如果仍然完成了持久化,把部分提交告警保留为
    /// 页面事实提示。调用方应随后触发一次权威查询,不能静默丢弃已发生的事实。
    pub fn observe_orphan_commit(&mut self, outcome: &CommitOutcome) {
        if outcome.credential_committed {
            self.notice = outcome.warning.clone();
        }
    }

    // -- 断开 -------------------------------------------------------------

    /// 开始断开;已断开 / 忙碌时返回 `false`。
    pub fn begin_disconnect(&mut self, provider: GitProvider) -> Option<RequestTicket> {
        let row = &mut self.rows[index(provider)];
        if row.busy || (row.account.is_none() && !row.unreconciled) {
            return None;
        }
        row.busy = true;
        row.error = None;
        let ticket = self.ledger.begin(self.owner, provider);
        row.ticket = Some(ticket);
        Some(ticket)
    }

    pub fn disconnect_succeeded(&mut self, ticket: RequestTicket, outcome: CommitOutcome) -> bool {
        if !self.accepts(ticket) {
            return false;
        }
        let row = &mut self.rows[index(ticket.provider)];
        row.account = None;
        row.unreconciled = false;
        row.busy = false;
        row.editing = false;
        row.error = None;
        row.clear_sensitive();
        row.ticket = None;
        self.notice = outcome.warning;
        self.ledger.cancel(ticket);
        true
    }

    pub fn disconnect_failed(&mut self, ticket: RequestTicket, error: GitAccountError) -> bool {
        if !self.accepts(ticket) {
            return false;
        }
        let row = &mut self.rows[index(ticket.provider)];
        row.busy = false;
        row.error = Some(error);
        row.ticket = None;
        self.ledger.cancel(ticket);
        true
    }

    /// 页面关闭:取消所有在途请求、清零输入;账户事实保留,重开时重新查询。
    pub fn close(&mut self) {
        self.ledger.cancel_owner(self.owner);
        self.query_ticket = None;
        self.query_error = None;
        self.notice = None;
        self.loading = false;
        for row in &mut self.rows {
            row.busy = false;
            row.editing = false;
            row.ticket = None;
            row.clear_sensitive();
        }
    }

    fn accepts_query(&self, ticket: RequestTicket) -> bool {
        self.query_ticket == Some(ticket)
            && ticket.owner == self.owner
            && ticket.seq == self.query_seq
    }

    fn accepts(&self, ticket: RequestTicket) -> bool {
        self.ledger.is_current(ticket)
            && self
                .rows
                .get(index(ticket.provider))
                .is_some_and(|row| row.ticket == Some(ticket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> GitPageState {
        GitPageState::new(9)
    }

    fn complete(provider: GitProvider) -> CommitOutcome {
        CommitOutcome::complete(provider)
    }

    #[test]
    fn connect_flow_clears_sensitive_input_and_sets_account() {
        let mut state = state();
        assert!(state.begin_edit(GitProvider::GitHub));
        assert!(state.set_token(GitProvider::GitHub, "ghp_secret"));
        assert!(state.is_editing(GitProvider::GitHub));
        let ticket = state.begin_connect(GitProvider::GitHub).unwrap();
        assert!(state.is_busy(GitProvider::GitHub));

        assert!(state.connect_succeeded(
            ticket,
            ConnectedAccount {
                username: "octocat".into()
            },
            complete(GitProvider::GitHub)
        ));
        assert_eq!(
            state.account(GitProvider::GitHub).unwrap().username,
            "octocat"
        );
        assert_eq!(state.token(GitProvider::GitHub), "");
        assert!(!state.is_busy(GitProvider::GitHub));
    }

    #[test]
    fn blank_token_and_connected_rows_cannot_connect() {
        let mut state = state();
        assert!(state.begin_connect(GitProvider::GitHub).is_none());
        assert!(state.set_token(GitProvider::GitHub, "  "));
        assert!(state.begin_connect(GitProvider::GitHub).is_none());

        assert!(state.begin_edit(GitProvider::GitLab));
        state.set_token(GitProvider::GitLab, "tok");
        let ticket = state.begin_connect(GitProvider::GitLab).unwrap();
        state.connect_succeeded(
            ticket,
            ConnectedAccount {
                username: "u".into(),
            },
            complete(GitProvider::GitLab),
        );
        assert!(state.begin_connect(GitProvider::GitLab).is_none());
    }

    #[test]
    fn stale_requests_cannot_modify_new_state() {
        let mut state = state();
        state.begin_edit(GitProvider::GitHub);
        state.set_token(GitProvider::GitHub, "tok");
        let first = state.begin_connect(GitProvider::GitHub).unwrap();
        // 失败后重新输入并提交,产生同一 provider 的新 ticket。
        state.connect_failed(
            first,
            GitAccountError::Network {
                detail: "timeout".into(),
            },
        );
        state.set_token(GitProvider::GitHub, "tok2");
        let second = state.begin_connect(GitProvider::GitHub).unwrap();
        assert_ne!(first.seq, second.seq);

        assert!(!state.connect_succeeded(
            first,
            ConnectedAccount {
                username: "old".into()
            },
            complete(GitProvider::GitHub)
        ));
        assert_eq!(state.account(GitProvider::GitHub), None);

        assert!(state.connect_failed(second, GitAccountError::TokenInvalid));
        assert_eq!(
            state.error(GitProvider::GitHub),
            Some(&GitAccountError::TokenInvalid)
        );
    }

    #[test]
    fn cancel_clears_input_and_invalidates_in_flight_request() {
        let mut state = state();
        state.begin_edit(GitProvider::GitHub);
        state.set_token(GitProvider::GitHub, "tok");
        let ticket = state.begin_connect(GitProvider::GitHub).unwrap();
        state.cancel(GitProvider::GitHub);
        assert_eq!(state.token(GitProvider::GitHub), "");
        assert!(!state.is_busy(GitProvider::GitHub));
        assert!(!state.connect_succeeded(
            ticket,
            ConnectedAccount {
                username: "late".into()
            },
            complete(GitProvider::GitHub)
        ));
        assert_eq!(state.account(GitProvider::GitHub), None);
    }

    #[test]
    fn close_clears_sensitive_input_and_freezes_results() {
        let mut state = state();
        state.begin_edit(GitProvider::GitHub);
        state.set_token(GitProvider::GitHub, "tok");
        let ticket = state.begin_connect(GitProvider::GitHub).unwrap();
        state.close();
        assert_eq!(state.token(GitProvider::GitHub), "");
        assert!(!state.is_busy(GitProvider::GitHub));
        assert!(!state.connect_succeeded(
            ticket,
            ConnectedAccount {
                username: "late".into()
            },
            complete(GitProvider::GitHub)
        ));
    }

    #[test]
    fn query_is_owner_scoped_and_ignores_late_results() {
        let mut state = state();
        let first = state.begin_query();
        assert!(state.is_loading());
        let second = state.begin_query();
        assert!(!state.apply_query(
            first,
            AccountSnapshot {
                accounts: GitAccountsState::default(),
                stale_providers: vec![],
                unreconciled_providers: vec![],
            }
        ));
        assert!(state.apply_query(
            second,
            AccountSnapshot {
                accounts: {
                    let mut accounts = GitAccountsState::default();
                    accounts.set(
                        GitProvider::Gitee,
                        Some(ConnectedAccount {
                            username: "abc".into(),
                        }),
                    );
                    accounts
                },
                stale_providers: vec![],
                unreconciled_providers: vec![],
            }
        ));
        assert_eq!(state.account(GitProvider::Gitee).unwrap().username, "abc");
        assert!(!state.is_loading());
    }

    #[test]
    fn partial_metadata_warning_is_visible_after_connect() {
        let mut state = state();
        state.begin_edit(GitProvider::GitHub);
        state.set_token(GitProvider::GitHub, "tok");
        let ticket = state.begin_connect(GitProvider::GitHub).unwrap();
        let mut outcome = complete(GitProvider::GitHub);
        outcome.metadata_committed = false;
        outcome.warning = Some(GitAccountError::MetadataWrite {
            detail: "disk full".into(),
        });
        assert!(state.connect_succeeded(
            ticket,
            ConnectedAccount {
                username: "octocat".into()
            },
            outcome
        ));
        assert!(matches!(
            state.notice(),
            Some(GitAccountError::MetadataWrite { .. })
        ));
        // 凭据已成功,UI 仍展示已连接。
        assert_eq!(
            state.account(GitProvider::GitHub).unwrap().username,
            "octocat"
        );
    }

    #[test]
    fn disconnect_flow_removes_account() {
        let mut state = state();
        state.begin_edit(GitProvider::Gitee);
        state.set_token(GitProvider::Gitee, "tok");
        let connect = state.begin_connect(GitProvider::Gitee).unwrap();
        state.connect_succeeded(
            connect,
            ConnectedAccount {
                username: "abc".into(),
            },
            complete(GitProvider::Gitee),
        );

        let disconnect = state.begin_disconnect(GitProvider::Gitee).unwrap();
        assert!(state.disconnect_succeeded(disconnect, complete(GitProvider::Gitee)));
        assert_eq!(state.account(GitProvider::Gitee), None);
    }
    #[test]
    fn reopened_partial_commit_is_recoverable_and_disconnect_is_not_disabled() {
        let mut page = state();
        let ticket = page.begin_query();
        page.apply_query(
            ticket,
            AccountSnapshot {
                accounts: GitAccountsState::default(),
                stale_providers: vec![],
                unreconciled_providers: vec![GitProvider::GitHub],
            },
        );
        assert!(page.needs_reconciliation(GitProvider::GitHub));
        let ticket = page
            .begin_disconnect(GitProvider::GitHub)
            .expect("saved credential can be removed");
        page.disconnect_succeeded(ticket, complete(GitProvider::GitHub));
        assert!(!page.needs_reconciliation(GitProvider::GitHub));
    }
}
