//! wry 执行器共用的两件小工具:**主线程句柄表**与**受限 builder**。
//!
//! 四个执行器(`panel`、`app_webview`、`native_child`、`web_child::pool`)各有一份
//! thread_local 的 `WebView` 句柄表(句柄不是 `Send`,只能在创建它的主线程访问),
//! 也都必须显式反着写 wry 0.55 的**非安全默认值**(下载默认放行、debug 下开发者工具默认开、
//! `window.open`/新窗口默认放行)。本模块把这两件事收成一份。
//!
//! **不替调用方决定内容是否可信**:导航白名单、IPC/nonce、数据存储标识、标题与加载回调、
//! 是否允许下载等策略差异仍由各执行器自己追加。`panel` 的面板 surface 是可信内部 HTML,
//! 刻意保持 wry 默认(它不用 [`apply_restricted`]),这也是行为等价的一部分。

use std::cell::RefCell;
use std::collections::BTreeMap;

/// 主线程句柄表。
///
/// 每个执行器持有自己的实例(不同模型的句柄表**永不共用**:应用 webview、可信原生子视图、
/// 不可信浏览器池、内部面板 surface 混在一起会让"销毁哪个"说不清);这里统一的是
/// 「`RefCell<BTreeMap>` + 借出访问器」这段样板,不是表本身。
pub(crate) struct HandleRegistry<K, V> {
    map: RefCell<BTreeMap<K, V>>,
}

impl<K: Ord, V> HandleRegistry<K, V> {
    /// 供 `thread_local!` 的 `const` 初始化用。
    pub(crate) const fn new() -> Self {
        Self {
            map: RefCell::new(BTreeMap::new()),
        }
    }

    /// 在主线程**可变**借出整张表。借用期间不得再进入同一个表(闭包里不要再调 `with`)。
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut BTreeMap<K, V>) -> R) -> R {
        f(&mut self.map.borrow_mut())
    }

    /// 在主线程**只读**借出整张表。与 [`with`](Self::with) 分开是为了保持既有借用语义:
    /// 只读的调用点(例如遍历池判断焦点)不该因为表可写而和另一次只读借用冲突。
    pub(crate) fn with_ref<R>(&self, f: impl FnOnce(&BTreeMap<K, V>) -> R) -> R {
        f(&self.map.borrow())
    }

    /// 可变借用的**非 panicking**版本:回调可能在已持有借用的同步过程中被触发,
    /// 拿不到就返回 `None`(调用方按"这一轮跳过"处理)。见 `web_child::pool` 的重入注释。
    pub(crate) fn try_with<R>(&self, f: impl FnOnce(&mut BTreeMap<K, V>) -> R) -> Option<R> {
        self.map.try_borrow_mut().ok().map(|mut map| f(&mut map))
    }

    /// 只读借用的非 panicking 版本。
    pub(crate) fn try_with_ref<R>(&self, f: impl FnOnce(&BTreeMap<K, V>) -> R) -> Option<R> {
        self.map.try_borrow().ok().map(|map| f(&map))
    }
}

/// 把 wry 的**非安全默认**反过来:关开发者工具、拒绝下载、拒绝新窗口。
///
/// **必须在调用方追加完自己的配置之后调用**——产品经 `configure` 闭包传进来的设置
/// 不能覆盖宿主的固定策略(`native_child` 就是这样用)。需要自定义新窗口行为
/// (例如浏览器池允许通知宿主开新页签、但 wry 侧仍然拒绝)的调用方,在这之后
/// 再调一次 `with_new_window_req_handler` 覆盖即可:builder 后写覆盖前写。
pub(crate) fn apply_restricted<'a>(builder: wry::WebViewBuilder<'a>) -> wry::WebViewBuilder<'a> {
    builder
        .with_devtools(false)
        .with_download_started_handler(|_, _| false)
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
}

#[cfg(test)]
mod tests {
    /// 三份「受限执行器」的源码钉死合并成一份参数化测试(Tier3 Task2)。
    ///
    /// 没有廉价夹具能真的建 wry webview;这些是它们**必须**经 [`apply_restricted`] 拿到、
    /// 且 wry 默认值会反过来的配置。`panel` 是可信内部 HTML,不在名单里(刻意保留默认)。
    #[test]
    fn apply_restricted_reverses_the_unsafe_wry_defaults() {
        let src = include_str!("wry_common.rs");
        let prod = &src[..src.find("#[cfg(test)]").unwrap()];
        let at = prod.find("pub(crate) fn apply_restricted").unwrap();
        let body = &prod[at..];
        for required in [
            ".with_devtools(false)",
            ".with_download_started_handler(|_, _| false)",
            ".with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)",
        ] {
            assert!(body.contains(required), "apply_restricted 缺少 {required}");
        }
    }

    #[test]
    fn every_restricted_executor_goes_through_apply_restricted() {
        for src in [
            ("app_webview", include_str!("app_webview.rs")),
            ("native_child", include_str!("native_child.rs")),
            ("web_child/pool", include_str!("web_child/pool.rs")),
        ] {
            let (name, src) = src;
            let prod = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
            assert!(
                prod.contains("apply_restricted("),
                "{name} 必须经 wry_common::apply_restricted 收紧 wry 默认值"
            );
            for forbidden in [
                ".with_devtools(true)",
                "NewWindowResponse::Allow",
                "with_download_completed_handler",
            ] {
                assert!(!prod.contains(forbidden), "{name} 不得出现 {forbidden}");
            }
        }
    }
}
