//! 共享 Git 账户领域模型与错误。仅 serde / std,不含网络或平台实现。

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// 支持的 Git 服务商。顺序即 UI 顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitProvider {
    GitHub,
    GitLab,
    Gitee,
}

impl GitProvider {
    pub const ALL: [GitProvider; 3] =
        [GitProvider::GitHub, GitProvider::GitLab, GitProvider::Gitee];

    /// 稳定存储键:旧 `dozer-git` Keychain 与 `git_accounts.json` 字段都依赖它。
    pub fn as_key(self) -> &'static str {
        match self {
            GitProvider::GitHub => "github",
            GitProvider::GitLab => "gitlab",
            GitProvider::Gitee => "gitee",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            GitProvider::GitHub => "GitHub",
            GitProvider::GitLab => "GitLab",
            GitProvider::Gitee => "Gitee",
        }
    }
}

/// 敏感 Token:Debug 永远打码、Drop 时清零,不进入普通产品消息的日志输出。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(***)")
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<String> for SecretToken {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretToken {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// 已连接账户的非敏感元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectedAccount {
    pub username: String,
}

/// 旧格式(以及新格式)兼容的三 provider 账户表。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitAccountsState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<ConnectedAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlab: Option<ConnectedAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitee: Option<ConnectedAccount>,
}

impl GitAccountsState {
    pub fn get(&self, provider: GitProvider) -> Option<&ConnectedAccount> {
        match provider {
            GitProvider::GitHub => self.github.as_ref(),
            GitProvider::GitLab => self.gitlab.as_ref(),
            GitProvider::Gitee => self.gitee.as_ref(),
        }
    }

    pub fn set(&mut self, provider: GitProvider, account: Option<ConnectedAccount>) {
        match provider {
            GitProvider::GitHub => self.github = account,
            GitProvider::GitLab => self.gitlab = account,
            GitProvider::Gitee => self.gitee = account,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.github.is_none() && self.gitlab.is_none() && self.gitee.is_none()
    }
}

/// 远端仓库列表项(项目创建 / 导入的消费者共用)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    pub full_name: String,
    pub clone_url: String,
}

/// 结构化错误:操作语义与原始诊断分离,呈现层负责本地化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitAccountError {
    /// 网络请求(发起 / 读取)失败。
    Network { detail: String },
    /// 服务商响应无法解析。
    ParseResponse { detail: String },
    /// 令牌无效或已过期。
    TokenInvalid,
    /// whoami 响应缺少 `login`。
    MissingLoginField,
    /// 仓库列表响应不是数组。
    NotArray,
    /// 拉取仓库列表时令牌失效。
    RepoListTokenExpired,
    /// 读取凭据失败。
    CredentialRead { detail: String },
    /// 写入凭据失败。
    CredentialWrite { detail: String },
    /// 删除凭据失败。
    CredentialDelete { detail: String },
    /// 读取本地账户元数据失败。
    MetadataRead { detail: String },
    /// 写入本地账户元数据失败。
    MetadataWrite { detail: String },
    /// 同一个对话框里的请求已被更新的请求取代。
    StaleRequest,
    /// 未知选项 / 能力不可用。
    Unsupported { detail: String },
}

impl GitAccountError {
    /// 语言无关的稳定诊断串(英文),用于日志与去重。
    pub fn stable(&self) -> String {
        match self {
            GitAccountError::Network { detail } => format!("Network request failed: {detail}"),
            GitAccountError::ParseResponse { detail } => {
                format!("Failed to parse the response: {detail}")
            }
            GitAccountError::TokenInvalid => "The token is invalid or has expired".to_string(),
            GitAccountError::MissingLoginField => {
                "The response is missing the login field".to_string()
            }
            GitAccountError::NotArray => "The response is not an array".to_string(),
            GitAccountError::RepoListTokenExpired => {
                "The repository list could not be loaded; reconnect the account".to_string()
            }
            GitAccountError::CredentialRead { detail } => {
                format!("Failed to read the credential: {detail}")
            }
            GitAccountError::CredentialWrite { detail } => {
                format!("Failed to write the credential: {detail}")
            }
            GitAccountError::CredentialDelete { detail } => {
                format!("Failed to delete the credential: {detail}")
            }
            GitAccountError::MetadataRead { detail } => {
                format!("Failed to read the local account record: {detail}")
            }
            GitAccountError::MetadataWrite { detail } => {
                format!("Failed to write the local account record: {detail}")
            }
            GitAccountError::StaleRequest => "The request was superseded".to_string(),
            GitAccountError::Unsupported { detail } => {
                format!("Unsupported operation: {detail}")
            }
        }
    }
}

impl fmt::Display for GitAccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.stable())
    }
}

impl std::error::Error for GitAccountError {}

/// 一次提交的部分结果:凭据与元数据不是原子事务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub provider: GitProvider,
    /// 凭据写入 / 删除是否完成。
    pub credential_committed: bool,
    /// 本地账户记录是否同步完成。
    pub metadata_committed: bool,
    /// 部分提交的诊断;`None` 表示完全成功。
    pub warning: Option<GitAccountError>,
}

impl CommitOutcome {
    pub fn complete(provider: GitProvider) -> Self {
        Self {
            provider,
            credential_committed: true,
            metadata_committed: true,
            warning: None,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.credential_committed && self.metadata_committed && self.warning.is_none()
    }
}

/// 账户查询结果:除状态外携带需要提示的部分失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub accounts: GitAccountsState,
    /// 元数据里存在、但凭据已缺失而被校正掉的 provider。
    pub stale_providers: Vec<GitProvider>,
    /// Credential exists, but metadata is absent. Expose recovery / disconnect,
    /// never report this persisted fact as an unconnected account.
    pub unreconciled_providers: Vec<GitProvider>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_token_debug_is_redacted_and_drop_clears() {
        let token = SecretToken::new("ghp_super_secret");
        assert_eq!(format!("{token:?}"), "SecretToken(***)");
        assert!(!format!("{token:?}").contains("super_secret"));
        assert_eq!(token.expose(), "ghp_super_secret");
    }

    #[test]
    fn accounts_state_routes_by_provider() {
        let mut state = GitAccountsState::default();
        assert!(state.is_empty());
        state.set(
            GitProvider::Gitee,
            Some(ConnectedAccount {
                username: "abc".into(),
            }),
        );
        assert_eq!(state.get(GitProvider::Gitee).unwrap().username, "abc");
        assert_eq!(state.get(GitProvider::GitHub), None);
        assert!(!state.is_empty());
    }

    #[test]
    fn legacy_json_shape_round_trips() {
        let json = r#"{"github":{"username":"octocat"},"gitlab":null,"gitee":{"username":"abc"}}"#;
        let state: GitAccountsState = serde_json::from_str(json).unwrap();
        assert_eq!(state.github.as_ref().unwrap().username, "octocat");
        assert_eq!(state.gitlab, None);
        assert_eq!(state.gitee.as_ref().unwrap().username, "abc");
        let back = serde_json::to_value(&state).unwrap();
        assert_eq!(back["github"]["username"], "octocat");
    }

    #[test]
    fn provider_keys_are_stable() {
        assert_eq!(GitProvider::GitHub.as_key(), "github");
        assert_eq!(GitProvider::GitLab.as_key(), "gitlab");
        assert_eq!(GitProvider::Gitee.as_key(), "gitee");
    }
}
