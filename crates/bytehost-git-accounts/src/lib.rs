//! 产品中立的 Git 账户能力。
//!
//! - [`model`]:provider / 账户 DTO、错误与敏感 Token 包装;
//! - [`providers`]:三家 provider 的 endpoint、鉴权与响应解析(唯一实现);
//! - [`stores`]:可注入的凭据 / 元数据后端与内存 / JSON 实现;
//! - [`transport`]:可注入 HTTP 接缝与 fake;
//! - [`service`]:查询 / 连接 / 断开 / 仓库列表与请求身份台账。
//!
//! 默认 feature 只有 serde 与纯逻辑;真实网络 / Keychain 分别由 `http` /
//! `keyring` 显式打开。共享层不提供任何默认产品路径或服务名。

pub mod model;
pub mod providers;
pub mod service;
pub mod stores;
pub mod transport;

pub use model::{
    AccountSnapshot, CommitOutcome, ConnectedAccount, GitAccountError, GitAccountsState,
    GitProvider, RemoteRepo, SecretToken,
};
pub use providers::{HttpRequest, HttpResponse, redact_secrets};
pub use service::{AccountService, Admission, RequestLedger, RequestTicket};
pub use stores::{
    CredentialStore, JsonFileMetadataStore, MemoryCredentialStore, MemoryMetadataStore,
    MetadataStore,
};
pub use transport::{FakeTransport, HttpTransport};

#[cfg(feature = "http")]
pub use transport::ReqwestTransport;

#[cfg(feature = "keyring")]
pub use stores::KeyringCredentialStore;

#[cfg(all(feature = "http", feature = "keyring"))]
pub use system::{SystemService, system_service, system_service_with_admission};

#[cfg(all(feature = "http", feature = "keyring"))]
mod system {
    use std::path::PathBuf;

    use crate::service::AccountService;
    use crate::stores::{JsonFileMetadataStore, KeyringCredentialStore};
    use crate::transport::ReqwestTransport;

    /// 真实后端组合:产品注入 Keychain 服务名与元数据路径。
    pub type SystemService =
        AccountService<KeyringCredentialStore, JsonFileMetadataStore, ReqwestTransport>;

    /// 默认实现不读取任何产品目录;调用方必须显式给出服务名与路径。
    pub fn system_service(
        keychain_service: impl Into<String>,
        metadata_path: impl Into<PathBuf>,
    ) -> SystemService {
        AccountService::new(
            KeyringCredentialStore::new(keychain_service),
            JsonFileMetadataStore::new(metadata_path),
            ReqwestTransport,
        )
    }

    /// 同一产品的多个请求共享提交准入;取消 / 同 provider 乱序由它裁决。
    pub fn system_service_with_admission(
        keychain_service: impl Into<String>,
        metadata_path: impl Into<PathBuf>,
        admission: std::sync::Arc<crate::service::Admission>,
    ) -> SystemService {
        AccountService::new_with_admission(
            KeyringCredentialStore::new(keychain_service),
            JsonFileMetadataStore::new(metadata_path),
            ReqwestTransport,
            admission,
        )
    }
}

/// 最小测试 / 示例装配:内存凭据 + 内存元数据 + fake HTTP。
pub type MemoryService = AccountService<MemoryCredentialStore, MemoryMetadataStore, FakeTransport>;

/// 默认装配,不触碰真实 Keychain 或网络。
pub fn memory_service(http: FakeTransport) -> MemoryService {
    AccountService::new(
        MemoryCredentialStore::default(),
        MemoryMetadataStore::default(),
        http,
    )
}
