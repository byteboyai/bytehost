//! 最小产品装配示例:一个主窗口 owner + 共享 `DialogController` + fake Git
//! 服务,展示"引入库即可使用"所需的全部接线,不依赖 dozer-app / digger-desktop。
//!
//! 运行 `cargo run --example settings_product -p bytehost-settings-ui`:
//! 只构造状态、执行 fake 登录并打印生命周期结果,不创建窗口 / GPU / 网络。
//!
//! 真实产品只需替换两处:
//! - `create_service` 改为注入真实 Keychain / 元数据路径 / HTTP;
//! - `Create` / `Destroy` effect 交给对应运行时建窗 / 销毁窗口。

use std::sync::Arc;

use bytehost_git_accounts::{
    AccountService, Admission, FakeTransport, GitProvider, MemoryCredentialStore,
    MemoryMetadataStore, RequestTicket, SecretToken,
};
use bytehost_settings::git::GitIntent;
use bytehost_settings_ui::text::{Text, identity_resolver};

type FakeService = AccountService<MemoryCredentialStore, MemoryMetadataStore, FakeTransport>;

fn create_service(admission: Arc<Admission>) -> FakeService {
    // 真实产品在这里换成 KeyringCredentialStore + JsonFileMetadataStore +
    // ReqwestTransport,并注入自己的服务名 / 数据路径。
    let http = FakeTransport::new().push_json(
        "https://api.github.com/user",
        200,
        r#"{"login":"octocat"}"#,
    );
    AccountService::new_with_admission(
        MemoryCredentialStore::default(),
        MemoryMetadataStore::default(),
        http,
        admission,
    )
}

fn main() {
    // 1) 共享生命周期:打开 → 驱动回包 Opened → 关闭 → 驱动回包 Closed。
    let owner = 7;
    let mut dialog = bytehost_settings::dialog::DialogController::new(
        owner,
        bytehost_settings::dialog::DialogKind::Settings,
    );
    let effects = dialog.open();
    let key = match effects.as_slice() {
        [bytehost_settings::dialog::DialogEffect::Create(key)] => *key,
        other => panic!("expected Create, got {other:?}"),
    };
    assert_eq!(key.session, 1);
    dialog.handle(bytehost_settings::dialog::DialogEvent::Opened {
        key,
        window: key.session,
    });
    assert!(matches!(
        dialog.phase(),
        bytehost_settings::dialog::Phase::Open
    ));

    // 2) Git 页状态与产品服务:同一份共享视图 / 状态规则。
    let mut git = bytehost_settings::git::GitPageState::new(key.session);
    let admission = Arc::new(Admission::new());
    let service = create_service(Arc::clone(&admission));

    assert!(git.begin_edit(GitProvider::GitHub));
    assert!(git.set_token(GitProvider::GitHub, "SYNTHETIC-NOT-A-REAL-TOKEN"));
    let ticket: RequestTicket = git.begin_connect(GitProvider::GitHub).unwrap();
    admission.register(ticket);

    let token = SecretToken::new(git.token(GitProvider::GitHub));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(service.connect_with_account_at(
        GitProvider::GitHub,
        &token,
        Some(ticket),
    ));
    let (account, outcome) = result.expect("fake validation succeeds");
    assert!(outcome.is_complete());
    assert!(git.connect_succeeded(ticket, account, outcome));

    // 3) 共享视图只产出类型化意图;真实产品把它们路由到服务。
    let text = Text::new(identity_resolver());
    let on_token = |provider: GitProvider, value: String| {
        println!("on_token({provider:?}, <redacted {} bytes>)", value.len());
    };
    let on_intent = |intent: GitIntent| {
        println!("on_intent({intent:?})");
    };
    let page = bytehost_settings_ui::git_accounts_page(bytehost_settings_ui::GitAccountsPage {
        state: &git,
        on_token: &on_token,
        on_intent: &on_intent,
        text,
    });
    drop(page);

    // 4) 关窗:撤销该会话的全部提交准入后,任何迟到结果都不能再落盘。
    let username = account_username(&git);
    admission.cancel_owner(key.session);
    let close = dialog.close();
    assert!(matches!(
        close.as_slice(),
        [bytehost_settings::dialog::DialogEffect::Destroy { .. }]
    ));
    dialog.handle(bytehost_settings::dialog::DialogEvent::Closed {
        key,
        window: key.session,
    });
    assert_eq!(
        dialog.phase(),
        &bytehost_settings::dialog::Phase::Closed,
        "关闭回包必须由对应实例确认"
    );

    println!(
        "settings_product: session {}; github={}; admission_after_cancel={:?}",
        key.session,
        username,
        admission.admit(ticket),
    );
}

fn account_username(git: &bytehost_settings::git::GitPageState) -> String {
    git.account(GitProvider::GitHub)
        .map(|account| account.username.clone())
        .unwrap_or_else(|| "<none>".to_owned())
}
