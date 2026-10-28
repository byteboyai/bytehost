//! macOS 原生胶水(仅 macOS):拒绝媒体捕获的 UI 委托代理、标准 Edit 菜单的安装。
//!
//! 都来自 Digger 浏览器面板 Task 0 的探针(`dozer/spike/browser-probe`),报告
//! `digger/docs/superpowers/reviews/2026-10-28-browser-panel-spikes.md`。

use super::edit_menu::standard_edit_menu_items;
use objc2::ClassType;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::NSObjectProtocol;
use objc2_web_kit::{
    WKFindConfiguration, WKFindResult, WKFrameInfo, WKPermissionDecision, WKSecurityOrigin,
    WKUIDelegate, WKWebView,
};
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

/// 保活句柄:`WKWebView` 对 `UIDelegate` 是 **weak** 引用,代理必须由我们自己活着(放进页签记录里,
/// 页签销毁时一起释放)。
pub type Guard = Option<Retained<AnyObject>>;

struct ProxyIvars {
    original: Retained<AnyObject>,
}

define_class!(
    /// wry 的 UI 委托(`WryWebViewUIDelegate`)对**所有来源、所有类型**的摄像头/麦克风请求无条件
    /// `Grant`,没有开关,类型私有。这个代理替换它:媒体捕获一律 `Deny`,其余方法(新窗口、文件选择等)
    /// 通过 `forwardingTargetForSelector:` 转发给 wry 的原委托。
    ///
    /// 目前页面调不到 `getUserMedia`(`navigator.mediaDevices` 在这个 WKWebView 里是 `undefined`,探针实测),
    /// 所以这是纵深防御:不依赖 wry/WebKit 的默认值,将来任何一方变化都不会让它突然生效。
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ProxyIvars]
    struct DenyMediaProxy;

    unsafe impl NSObjectProtocol for DenyMediaProxy {}

    unsafe impl WKUIDelegate for DenyMediaProxy {
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn request_media_capture_permission(
            &self,
            _webview: Option<&AnyObject>,
            _origin: Option<&WKSecurityOrigin>,
            _frame: Option<&WKFrameInfo>,
            _capture_type: isize,
            decision_handler: &block2::Block<dyn Fn(isize)>,
        ) {
            decision_handler.call((WKPermissionDecision::Deny.0,));
        }
    }

    impl DenyMediaProxy {
        /// WebKit 先问 `respondsToSelector:` 再调可选方法,所以既要转发调用,也要如实回答"能不能响应"。
        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target(&self, _sel: Sel) -> Option<&AnyObject> {
            let original: &AnyObject = &self.ivars().original;
            Some(original)
        }

        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, sel: Sel) -> Bool {
            let own: Bool = unsafe { msg_send![super(self), respondsToSelector: sel] };
            if own.as_bool() {
                return own;
            }
            unsafe { msg_send![&*self.ivars().original, respondsToSelector: sel] }
        }
    }
);

/// 给刚建好的 webview 装上"拒绝媒体捕获"的代理委托。必须在主线程(`window::run` 里)。
/// 返回保活句柄,调用方存进页签记录。拿不到主线程标记或原委托时返回 `None`(不装,不 panic)。
pub fn install_media_deny_proxy(view: &wry::WebView) -> Guard {
    use wry::WebViewExtMacOS;
    let mtm = MainThreadMarker::new()?;
    let webview = view.webview();
    let original: Option<Retained<AnyObject>> = unsafe { msg_send![&*webview, UIDelegate] };
    let original = original?;
    let proxy = mtm
        .alloc::<DenyMediaProxy>()
        .set_ivars(ProxyIvars { original });
    let proxy: Retained<DenyMediaProxy> = unsafe { msg_send![super(proxy), init] };
    let proxy: Retained<AnyObject> = unsafe { Retained::cast_unchecked(proxy) };
    unsafe {
        let _: () = msg_send![&*webview, setUIDelegate: &*proxy];
    }
    Some(proxy)
}

thread_local! {
    static EDIT_MENU_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// 安装标准 Edit 菜单(见 [`standard_edit_menu_items`] 的文档):没有它,webview 里 ⌘A/⌘C/⌘V/⌘X 全都不能用。
/// 仅主线程;**幂等**(装过就不再装)。
///
/// **不替换主菜单**:winit 在 macOS 上默认装了带「退出 ⌘Q」「隐藏 ⌘H」的应用菜单,整个换掉会让这些快捷键失效。
/// 这里只在现有主菜单后面**追加** Edit;应用确实没有主菜单时才新建一个(带一个空的应用菜单占位)。
/// 已经有名为 `Edit` 的菜单(应用自己装过)就不再追加。返回是否这次真的装了。
pub fn install_standard_edit_menu() -> bool {
    use objc2::sel;
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    if EDIT_MENU_INSTALLED.with(|flag| flag.replace(true)) {
        return false;
    }
    let app = NSApplication::sharedApplication(mtm);
    let main = match app.mainMenu() {
        Some(existing) => existing,
        None => {
            let created = NSMenu::new(mtm);
            // 主菜单的第一项是应用菜单的位置,留一个空的占位。
            created.addItem(&NSMenuItem::new(mtm));
            app.setMainMenu(Some(&created));
            created
        }
    };
    let already_has_edit = main.itemArray().iter().any(|item| {
        item.submenu()
            .is_some_and(|submenu| submenu.title().to_string() == "Edit")
    });
    if already_has_edit {
        return false;
    }
    let edit_holder = NSMenuItem::new(mtm);
    main.addItem(&edit_holder);
    let edit = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Edit"));
    for item in standard_edit_menu_items() {
        let action = match item.selector {
            "cut:" => sel!(cut:),
            "copy:" => sel!(copy:),
            "paste:" => sel!(paste:),
            "selectAll:" => sel!(selectAll:),
            other => unreachable!("未知的 Edit 菜单选择器 {other}"),
        };
        let menu_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(item.title),
                Some(action),
                &NSString::from_str(item.key),
            )
        };
        edit.addItem(&menu_item);
    }
    edit_holder.setSubmenu(Some(&edit));
    true
}

/// 在页面里查找 `query`(`WKWebView.findString:withConfiguration:completionHandler:`):命中就选中并滚动到那里,
/// 下一次同样的查找从当前选区之后继续;`wraps` 恒开。`done(matched)` 在查找结束时回调(主线程)。
/// 拿不到主线程标记时直接回调 `done(false)`。
pub fn find_string(
    view: &wry::WebView,
    query: &str,
    backwards: bool,
    case_sensitive: bool,
    done: impl Fn(bool) + 'static,
) {
    use wry::WebViewExtMacOS;
    let Some(mtm) = MainThreadMarker::new() else {
        done(false);
        return;
    };
    let wk = view.webview();
    let query = objc2_foundation::NSString::from_str(query);
    unsafe {
        let config = WKFindConfiguration::new(mtm);
        config.setWraps(true);
        config.setBackwards(backwards);
        config.setCaseSensitive(case_sensitive);
        let handler = block2::RcBlock::new(move |result: NonNull<WKFindResult>| {
            done(result.as_ref().matchFound());
        });
        wk.findString_withConfiguration_completionHandler(&query, Some(&config), &handler);
    }
}

struct FindIvars {
    on_find: Box<dyn Fn()>,
}

define_class!(
    /// ⌘F 的菜单项目标。`WKWebView` **不**响应 `performTextFinderAction:`(实测 `responds(to:) == false`),所以
    /// 不能用响应链上的标准「查找」动作,要自己当目标。只在 first responder 是 `WKWebView` 时启用——
    /// 否则 ⌘F 会被这个菜单项吃掉,宿主自己的 ⌘F(Dozer 的预览查找)就收不到了。
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = FindIvars]
    struct FindTarget;

    unsafe impl NSObjectProtocol for FindTarget {}

    impl FindTarget {
        #[unsafe(method(browserFind:))]
        fn browser_find(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_find)();
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, _item: Option<&AnyObject>) -> Bool {
            Bool::new(first_responder_is_web_view())
        }
    }
);

/// 当前 key window 的 first responder 是不是 `WKWebView`(wry 的子类也算)。
fn first_responder_is_web_view() -> bool {
    use objc2_app_kit::NSApplication;
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.keyWindow()
        .and_then(|window| window.firstResponder())
        .is_some_and(|responder| responder.isKindOfClass(WKWebView::class()))
}

thread_local! {
    static FIND_INSTALLED: Cell<bool> = const { Cell::new(false) };
    // 菜单项对 target 是 weak 引用:target 必须由我们自己活着。
    static FIND_TARGET: RefCell<Option<Retained<FindTarget>>> = const { RefCell::new(None) };
}

/// 在 Edit 菜单里加一项「Find…」(⌘F),按下时调 `on_find`;只在 first responder 是 `WKWebView` 时启用。
/// 必须在 [`install_standard_edit_menu`] 之后调(要往已有的 Edit 菜单里加);主线程、幂等。返回是否这次真的装了。
pub fn install_find_shortcut(on_find: impl Fn() + 'static) -> bool {
    use objc2::sel;
    use objc2_app_kit::{NSApplication, NSMenuItem};
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    if FIND_INSTALLED.with(|flag| flag.get()) {
        return false;
    }
    let app = NSApplication::sharedApplication(mtm);
    let Some(main) = app.mainMenu() else {
        return false;
    };
    let edit = main.itemArray().iter().find_map(|item| {
        item.submenu()
            .filter(|submenu| submenu.title().to_string() == "Edit")
    });
    let Some(edit) = edit else {
        return false;
    };
    FIND_INSTALLED.with(|flag| flag.set(true));
    let target = mtm.alloc::<FindTarget>().set_ivars(FindIvars {
        on_find: Box::new(on_find),
    });
    let target: Retained<FindTarget> = unsafe { msg_send![super(target), init] };
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str("Find…"),
            Some(sel!(browserFind:)),
            &NSString::from_str("f"),
        )
    };
    unsafe { item.setTarget(Some(&target)) };
    edit.addItem(&item);
    FIND_TARGET.with(|slot| *slot.borrow_mut() = Some(target));
    true
}

#[cfg(test)]
mod tests {
    /// 代理的决定必须是 `Deny`,且整个文件里绝不能出现 `Grant`:这是整个代理存在的理由。
    #[test]
    fn the_proxy_can_only_ever_deny_media_capture() {
        let src = include_str!("apple.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(prod.contains("decision_handler.call((WKPermissionDecision::Deny.0,))"));
        assert!(!prod.contains("WKPermissionDecision::Grant"));
        assert!(!prod.contains("WKPermissionDecision::Prompt"));
    }

    /// 代理要把原委托接过来转发,并如实回答 `respondsToSelector:`;WKWebView 对委托是 weak 引用,
    /// 所以必须有保活(`Guard`)。这几处缺一个,新窗口 / 文件选择就悄悄坏掉,或者代理被释放后 Grant 又回来了。
    #[test]
    fn the_proxy_forwards_everything_else_and_is_kept_alive() {
        let src = include_str!("apple.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        for required in [
            "forwardingTargetForSelector:",
            "respondsToSelector:",
            "setUIDelegate:",
            "pub type Guard",
        ] {
            assert!(prod.contains(required), "缺少 {required}");
        }
    }

    /// 不得整个替换应用主菜单(会丢掉 winit 默认装的「退出 ⌘Q」「隐藏 ⌘H」);只有应用没有主菜单时才新建。
    #[test]
    fn the_edit_menu_is_appended_to_the_existing_main_menu_not_replacing_it() {
        let src = include_str!("apple.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        let install = &prod[prod.find("pub fn install_standard_edit_menu").unwrap()..];
        assert!(install.contains("app.mainMenu()"), "必须先读现有主菜单");
        assert_eq!(
            install.matches("setMainMenu(").count(),
            1,
            "setMainMenu 只能出现一次(新建分支里)"
        );
        let set_at = install.find("setMainMenu(").unwrap();
        let none_at = install.find("None =>").unwrap();
        assert!(
            set_at > none_at,
            "setMainMenu 只能在「没有主菜单」的分支里调用"
        );
        assert!(
            install.contains("already_has_edit"),
            "已有 Edit 菜单不得重复追加"
        );
    }

    /// ⌘F 菜单项只能在 first responder 是 `WKWebView` 时启用,否则会把 Dozer 自己的 ⌘F(预览查找)吃掉;
    /// 目标对象必须被我们保活(菜单项对 target 是 weak 引用)。
    #[test]
    fn the_find_shortcut_is_enabled_only_when_a_web_view_has_focus_and_its_target_is_kept_alive() {
        let src = include_str!("apple.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        for required in [
            "validateMenuItem:",
            "first_responder_is_web_view()",
            "isKindOfClass(WKWebView::class())",
            "FIND_TARGET.with(",
            "sel!(browserFind:)",
        ] {
            assert!(prod.contains(required), "缺少 {required}");
        }
        // 不得用标准的 performTextFinderAction:(WKWebView 不响应,菜单项会永远禁用)。
        assert!(!prod.contains("sel!(performTextFinderAction:)"));
        // 查找必须开 wraps、并回调 matchFound。
        assert!(prod.contains("config.setWraps(true)"));
        assert!(prod.contains("matchFound()"));
    }
}
