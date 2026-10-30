//! 应用 webview 的**受限 builder**:把 `bytehost-webview` 的纯安全策略落成一个真实的 wry 子视图。
//!
//! 规格 §6.4:导航策略、数据存储 ID、nonce、IPC 白名单、数据清除队列复用 `bytehost-webview`;
//! "新 desktop 层统一实现 builder,产品只给策略配置与资源,不各自复制 wry 初始化代码"。
//! 因此**唯一一份**受限 wry 构造在这里,产品(Dozer / Digger)不各写一遍。
//!
//! 应用代码不可信(第三方或 agent 生成),与预览/浏览器那条可信路径的区别:
//! - 不注册任何自定义协议(预览 host/允许文件只在那条协议后面);
//! - IPC 只认 `focus`/`mouseup`/`zoom_*` 白名单消息,且必须带本 webview 的随机 nonce
//!   (页面伪造不了,见 `AppIpc::parse`/`app_init_script`),其余一律丢弃;
//! - 下载一律拒绝:wry 0.55 的**默认**是放行(默认 `download_started_handler` 返回 `true`,
//!   且下载不经导航策略),所以必须显式 `.with_download_started_handler(|_, _| false)`;
//!   开发者工具同理显式关闭(`.with_devtools(false)`,debug 下默认是开的);
//! - 导航只放行本应用 origin(`AppOrigin::allows_navigation`),`window.open`/新窗口一律拒绝;
//! - 每应用独立的 WKWebsiteDataStore(macOS 14+;更老的系统 wry 会退回默认存储);
//! - `url` 不是该形状的应用站点地址时**不创建**(`CreateFailed`,fail closed)。
//!
//! 无法在 wry 层强制的项:**摄像头/麦克风** —— wry 0.55 会无条件批准权限请求,这里没有 hook
//! 可拦;目前靠系统 TCC 挡着,且产品不声明用途(见规格 §6.4)。不虚报为已强制。

use std::collections::BTreeMap;

use bytehost_core::{LogicalRect, SurfaceSlot};
pub use bytehost_webview::i18n::{
    I18nSnapshot, build_i18n_snapshot, i18n_bridge_init_script, i18n_set_locale_script,
    should_install_bridge,
};
use bytehost_webview::{
    AppIpc, AppOrigin, StoreRemovalOutcome, app_init_script, data_store_identifier, new_ipc_nonce,
};
use iced::{Task, window};
use wry::WebViewBuilderExtDarwin;

use crate::wry_common::{HandleRegistry, apply_restricted};

/// 创建一个应用子视图所需的**全部**产品输入:slot、加载地址、逻辑矩形、可见性。
///
/// 产品只给这些;安全策略(origin 白名单、nonce、存储标识、下载/devtools/新窗口)全部由本模块
/// 固定,产品改不了也无需重复。
#[derive(Debug, Clone)]
pub struct AppSurfaceSpec {
    pub slot: SurfaceSlot,
    pub url: String,
    pub rect: LogicalRect,
    pub visible: bool,
}

/// 应用子视图的创建结果:成功交回 slot;失败带错误串(不 panic)。
#[derive(Debug, Clone)]
pub struct AppSurfaceBuilt {
    pub slot: SurfaceSlot,
    pub result: Result<(), String>,
}

/// 从应用 webview 上行、且**通过 nonce 白名单**的事件。宿主据此路由焦点/拖拽/缩放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSurfaceIpc {
    Focus,
    MouseUp,
    ZoomIn,
    ZoomOut,
    ZoomReset,
}

impl From<AppIpc> for AppSurfaceIpc {
    fn from(ipc: AppIpc) -> Self {
        match ipc {
            AppIpc::Focus => Self::Focus,
            AppIpc::MouseUp => Self::MouseUp,
            AppIpc::ZoomIn => Self::ZoomIn,
            AppIpc::ZoomOut => Self::ZoomOut,
            AppIpc::ZoomReset => Self::ZoomReset,
        }
    }
}

thread_local! {
    /// 非 Send 的应用 wry 句柄只在主线程持有;与 `panel::HANDLES` 分表,避免与内置面板混用。
    static APP_HANDLES: HandleRegistry<SurfaceSlot, wry::WebView> = const { HandleRegistry::new() };
}

/// 在主线程用 `window::run` 创建一个**受限**应用子视图。
///
/// - `on_ipc`:收到一条通过 nonce 白名单的上行事件(在窗口线程调用,须自行把消息送回窗口)。
/// - `on_ready`:页面导航完成(用于把 `SurfaceKind::Application` 推到 `Ready`);宿主应在此后
///   经 `evaluate_app` 下发首帧语言快照(见 `i18n_set_locale_script`)。
/// - `install_i18n_bridge`:manifest 声明 `[i18n]` 且 `webview_locale != none` 时为 `true`,
///   初始化脚本里装入只读语言桥骨架(真实值由 `evaluate_app` 下发);`none` 不装,桥不存在。
/// - `wrap`:把创建结果包成产品消息。
///
/// 返回的 `Task` 完成时产出 `wrap(AppSurfaceBuilt)`。
pub fn build_app_child<M>(
    window_id: window::Id,
    spec: AppSurfaceSpec,
    on_ipc: impl Fn(SurfaceSlot, AppSurfaceIpc) + Send + 'static,
    on_ready: impl Fn(SurfaceSlot) + Send + 'static,
    install_i18n_bridge: bool,
    wrap: impl Fn(AppSurfaceBuilt) -> M + Send + 'static,
) -> Task<M>
where
    M: Send + 'static,
{
    iced::window::run(window_id, move |w| {
        // fail closed:地址不是 `<id>.localhost:<port>` 形状就不创建。
        let Some(origin) = AppOrigin::from_url(&spec.url) else {
            return AppSurfaceBuilt {
                slot: spec.slot,
                result: Err("应用地址不是应用站点形状,拒绝创建".into()),
            };
        };
        let slot = spec.slot;
        let nonce = new_ipc_nonce();
        let app_id = origin.app_id().to_owned();
        let built = apply_restricted(wry::WebViewBuilder::new())
            .with_url(&spec.url)
            .with_bounds(crate::panel::rect_to_wry(spec.rect))
            .with_visible(spec.visible)
            .with_allow_link_preview(false)
            // 剪贴板:保守关闭。wry 的 `clipboard` 只影响 Linux/Windows(macOS 由系统管辖),
            // 但显式写 false 让"不做声明即不放行"这条在跨平台上一致,而不是依赖默认值(默认恰好也是 false)。
            .with_clipboard(false)
            .with_data_store_identifier(data_store_identifier(&app_id))
            .with_initialization_script(app_init_script(&nonce, install_i18n_bridge))
            .with_ipc_handler(move |req| {
                if let Some(ipc) = AppIpc::parse(req.body().as_str(), &nonce) {
                    on_ipc(slot, ipc.into());
                }
            })
            .with_navigation_handler(move |url: String| origin.allows_navigation(&url))
            .with_on_page_load_handler(move |event, _url| {
                if matches!(event, wry::PageLoadEvent::Finished) {
                    on_ready(slot);
                }
            })
            .build_as_child(&w);
        match built {
            Ok(wv) => {
                with_app_handles(|h| {
                    h.insert(slot, wv);
                });
                AppSurfaceBuilt {
                    slot,
                    result: Ok(()),
                }
            }
            Err(e) => AppSurfaceBuilt {
                slot,
                result: Err(e.to_string()),
            },
        }
    })
    .map(wrap)
}

/// 重设某个应用子视图的 bounds(主线程)。
pub fn set_app_bounds(slot: SurfaceSlot, rect: LogicalRect) -> bool {
    with_app_handles(|h| {
        if let Some(wv) = h.get(&slot) {
            let _ = wv.set_bounds(crate::panel::rect_to_wry(rect));
            true
        } else {
            false
        }
    })
}

/// 向某个应用子视图注入脚本(主线程)。返回是否有该实例。
pub fn evaluate_app(slot: SurfaceSlot, script: &str) -> bool {
    with_app_handles(|h| {
        if let Some(wv) = h.get(&slot) {
            let _ = wv.evaluate_script(script);
            true
        } else {
            false
        }
    })
}

/// 销毁某个应用子视图(主线程)。返回是否确实销毁了一个实例。
pub fn destroy_app(slot: SurfaceSlot) -> bool {
    with_app_handles(|h| h.remove(&slot).is_some())
}

/// 当前已创建的应用子视图数量(测试/诊断用)。
pub fn app_handle_count() -> usize {
    with_app_handles(|h| h.len())
}

/// 当前 macOS 主版本号(纯策略 crate `bytehost-webview` 不碰 objc2,版本判断留给宿主层)。
pub fn host_os_major() -> isize {
    objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion
}

/// 清除一个应用的 WKWebsiteDataStore(卸载"连数据一起删"的浏览器数据部分)。
///
/// 规格 §6.4:数据清除队列复用 `bytehost-webview`;系统版本门槛也由它给出
/// ([`bytehost_webview::supports_store_removal`])——wry **不检查**系统版本,macOS 12/13 上直接调
/// `remove_data_store` 会因未知选择器让进程崩掉,所以这里先判断。
///
/// 结果**异步**回:`remove_data_store` 的完成回调由 WebKit 在事件循环里触发,所以这里通过
/// `on_result` 把 [`StoreRemovalOutcome`] 送回产品(产品自行发消息/触发重试)。返回的 `Task` 只表示
/// "已提交到主线程";`on_result` 一定被调用一次(不支持的系统版本上同步调 `Unsupported`)。
/// **调用前该应用的 webview 必须已离开池**(wry 要求),由调用方保证。
pub fn remove_app_data_store<M>(
    window_id: window::Id,
    app_id: String,
    store: [u8; 16],
    on_result: impl Fn(String, StoreRemovalOutcome) + Send + Sync + 'static,
    wrap: impl FnOnce() -> M + Send + 'static,
) -> Task<M>
where
    M: Send + 'static,
{
    if !bytehost_webview::supports_store_removal(host_os_major()) {
        on_result(app_id, StoreRemovalOutcome::Unsupported);
        return Task::done(wrap());
    }
    window::run(window_id, move |_w| {
        use wry::WebViewExtDarwin;
        let id = app_id.clone();
        let done = on_result;
        wry::WebView::remove_data_store(&store, move |result| {
            done(
                id,
                match result {
                    Ok(()) => StoreRemovalOutcome::Done,
                    Err(wry::Error::DataStoreInUse) => StoreRemovalOutcome::InUse,
                    Err(e) => StoreRemovalOutcome::Failed(e.to_string()),
                },
            );
        });
        wrap()
    })
}

fn with_app_handles<R>(f: impl FnOnce(&mut BTreeMap<SurfaceSlot, wry::WebView>) -> R) -> R {
    APP_HANDLES.with(|h| h.with(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// builder 是创建应用 wry webview 的唯一路径,没有廉价夹具能真的建一个;这些是它**必须**写出、
    /// 且 wry 默认值会反过来的配置,用源码扫描钉住(wry 0.55 默认:下载放行、debug 下开发者工具开、
    /// 媒体权限自动批准)。与 Dozer `app_webview.rs::build_app_webview_pins_the_restrictive_settings` 同型,
    /// 但这里的 builder 是共享层唯一一份。
    #[test]
    fn build_app_child_pins_the_restrictive_settings() {
        let src = include_str!("app_webview.rs");
        // 只看生产代码:本文件的测试模块自身含这些字面量(在断言里),扫进去会自我命中。
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        let start = prod.find("pub fn build_app_child<M>(").unwrap();
        let body = &prod[start..];
        // wry 默认值反转(devtools/下载/新窗口)统一在 wry_common::apply_restricted,
        // 由那里的参数化测试钉住;这里只钉应用 webview 特有的策略。
        for required in [
            "apply_restricted(",
            ".with_clipboard(false)",
            ".with_navigation_handler(",
            ".with_data_store_identifier(",
            ".with_initialization_script(app_init_script(",
            "AppIpc::parse(",
            "AppOrigin::from_url(",
        ] {
            assert!(body.contains(required), "缺少 {required}");
        }
        for forbidden in [
            "with_custom_protocol",
            "with_download_completed_handler",
            ".with_devtools(true)",
        ] {
            assert!(!body.contains(forbidden), "不得出现 {forbidden}");
        }
    }

    /// 策略与存储标识只在 `bytehost-webview` 有一份:desktop builder 不得再抄
    /// `allows_navigation` / `data_store_identifier` 的实现(只能 import)。
    #[test]
    fn no_second_copy_of_the_navigation_policy() {
        let src = include_str!("app_webview.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(
            !prod.contains("fn allows_navigation"),
            "导航策略的第二份实现;唯一真相在 bytehost-webview"
        );
        assert!(
            !prod.contains("fn data_store_identifier"),
            "每应用存储标识的第二份实现;唯一真相在 bytehost-webview"
        );
    }

    /// 数据存储清除必须**先过系统版本门槛**:wry 不检查系统版本,macOS 12/13 上直接调
    /// `remove_data_store` 会因未知选择器崩掉。纯策略 crate 给出 `supports_store_removal`,
    /// 宿主层必须真的先问它——用源码扫描钉住这条顺序(无廉价夹具能跑真实 wry 调用)。
    #[test]
    fn remove_app_data_store_gates_on_the_os_version() {
        let src = include_str!("app_webview.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        let start = prod.find("pub fn remove_app_data_store<M>(").unwrap();
        let body = &prod[start..];
        assert!(
            body.contains("supports_store_removal(host_os_major())"),
            "必须先过 supports_store_removal(host_os_major()) 再调 wry"
        );
        assert!(
            body.contains("wry::WebView::remove_data_store("),
            "经 wry 的 remove_data_store 清除"
        );
        assert!(
            !body.contains("fn data_store_identifier"),
            "存储标识唯一真相在 bytehost-webview,不得另抄一份"
        );
    }

    /// T0 spike（i18n 注入接缝）的源码钉死：语言快照**必须**经导航后的 `evaluate_app` 下发，
    /// **不得**靠创建时一次性冻结的初始化脚本携带。据此保证“语言变化后新文档不会暴露旧快照”。
    /// 这是 spec §6.1 pending/skeleton 路径的验收依据；T4 实现桥时不得违反。
    #[test]
    fn i18n_locale_must_be_pushed_post_navigation_not_via_init_script() {
        let src = include_str!("app_webview.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];

        // 初始化脚本只在创建时注入一次（冻结），没有按文档重注入的循环/二次调用路径。
        assert_eq!(
            prod.matches("with_initialization_script(").count(),
            1,
            "初始化脚本只能注入一次(创建时冻结),不能按文档更新 locale"
        );
        assert!(
            prod.contains("with_initialization_script(app_init_script(&nonce,"),
            "初始化脚本必须是 app_init_script(&nonce, install_i18n_bridge),不得内嵌 locale 值"
        );

        // 运行时语言下发唯一通道是 evaluate_app -> wv.evaluate_script（导航完成后）。
        let eval = prod.find("pub fn evaluate_app(").unwrap();
        let eval_body = &prod[eval..];
        assert!(
            eval_body.contains("wv.evaluate_script("),
            "运行时下发必须走 evaluate_app -> wv.evaluate_script"
        );

        // ready 时机是导航完成(PageLoadEvent::Finished),语言首帧在此之后下发。
        assert!(
            prod.contains("wry::PageLoadEvent::Finished"),
            "应用 Surface ready 须为导航完成"
        );

        // 不引入新的 IPC 动词或 dozer:// 通道。
        assert!(!prod.contains("dozer://"), "不得注册 dozer:// 通道");
        assert!(
            !prod.contains("with_custom_protocol"),
            "不得为应用注册自定义协议"
        );
    }

    #[test]
    fn app_surface_ipc_maps_every_whitelisted_verb() {
        assert_eq!(AppSurfaceIpc::from(AppIpc::Focus), AppSurfaceIpc::Focus);
        assert_eq!(AppSurfaceIpc::from(AppIpc::MouseUp), AppSurfaceIpc::MouseUp);
        assert_eq!(AppSurfaceIpc::from(AppIpc::ZoomIn), AppSurfaceIpc::ZoomIn);
        assert_eq!(AppSurfaceIpc::from(AppIpc::ZoomOut), AppSurfaceIpc::ZoomOut);
        assert_eq!(
            AppSurfaceIpc::from(AppIpc::ZoomReset),
            AppSurfaceIpc::ZoomReset
        );
    }
}
