//! 对话框宿主:共享生命周期控制器 + iced 运行时的原生父子绑定辅助。
//!
//! 控制器本身在 [`bytehost_settings::dialog`](纯状态);本模块只做运行时胶水:
//! iced 用公共 `window::run` 取得窗口的原生句柄,把设置窗挂成主窗的 AppKit
//! 子窗口,并把子窗口 frame 对齐到父窗口。**不创建第二条事件循环**,也不依赖
//! iced 私有结构。
//!
//! macOS 首批支持;其他平台返回 [`DialogBindingError::UnsupportedPlatform`],
//! 产品必须显式提示,不能把普通独立窗口当作统一宿主。

pub use bytehost_settings::dialog::{
    DestroyReason, DialogController, DialogEffect, DialogEvent, DialogFailure, DialogGeometry,
    DialogKey, DialogKind, LogicalRect, LogicalSize, Phase,
};

use std::fmt;

use iced::Task;
use iced::window;

/// 原生父子绑定失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogBindingError {
    /// 当前平台没有实现父所属 overlay。
    UnsupportedPlatform,
    /// 运行时没有给出窗口原生句柄。
    MissingWindowHandle,
    /// 句柄不是 AppKit 窗口。
    NotAppKit,
    /// 拿不到 NSWindow。
    MissingNativeWindow,
    /// 原生绑定只能在主线程执行。
    NotMainThread,
    /// 父窗口已经关闭,不能把新的子窗口挂回去。
    ParentClosed,
}

impl fmt::Display for DialogBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DialogBindingError::UnsupportedPlatform => {
                f.write_str("parent-owned dialogs are not supported on this platform")
            }
            DialogBindingError::MissingWindowHandle => {
                f.write_str("the runtime did not expose a native window handle")
            }
            DialogBindingError::NotAppKit => {
                f.write_str("the native window handle is not an AppKit window")
            }
            DialogBindingError::MissingNativeWindow => {
                f.write_str("the AppKit view is not attached to an NSWindow")
            }
            DialogBindingError::NotMainThread => {
                f.write_str("native window binding must run on the main thread")
            }
            DialogBindingError::ParentClosed => f.write_str("the parent window is already closed"),
        }
    }
}

impl std::error::Error for DialogBindingError {}

/// Build native and rendering resources as one recoverable operation. If rendering
/// fails, the partially-created native window is dropped before returning the error.
/// Factories are injectable so the production rollback path can be tested headlessly.
pub fn try_create_dialog_resources<W, G>(
    create_window: impl FnOnce() -> Result<W, String>,
    create_renderer: impl FnOnce(&W) -> Result<G, String>,
) -> Result<(W, G), String> {
    let window = create_window()?;
    let renderer = create_renderer(&window)?;
    Ok((window, renderer))
}

/// 当前平台是否支持父所属对话框。
pub const fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// 把 iced 创建的子窗口挂成父窗口的原生子窗口,并把 frame 对齐到父窗口客户区。
///
/// 返回的 `Task` 必须交给 iced 运行时;内部经 `window::run` 在主线程执行原生
/// 操作。父窗或子窗在动作执行前关闭时,回调不会运行(不会聚焦过时句柄)。
pub fn attach_child(parent: window::Id, child: window::Id) -> Task<Result<(), DialogBindingError>> {
    attach_child_impl(parent, child)
}

/// 父窗口移动 / 缩放 / 跨屏后重新对齐子窗口 frame。重复调用幂等。
pub fn sync_child(parent: window::Id, child: window::Id) -> Task<Result<(), DialogBindingError>> {
    attach_child_impl(parent, child)
}

/// 解除父子关系(关闭前可调用;窗口销毁本身也会解除)。
pub fn detach_child(parent: window::Id, child: window::Id) -> Task<Result<(), DialogBindingError>> {
    detach_child_impl(parent, child)
}

#[cfg(target_os = "macos")]
fn attach_child_impl(
    parent: window::Id,
    child: window::Id,
) -> Task<Result<(), DialogBindingError>> {
    // 第一个回调只把父窗口放进主线程注册表并**保留所有权**;第二个回调再从
    // 注册表取出。绝不让裸地址跨回调传递:两个 `window::run` 之间父窗口可能
    // 已经关闭 / 释放,保留对象和主线程校验保证绑定期间指针始终有效。
    iced::window::run(parent, move |handle| register_parent(parent, handle)).then(
        move |registered| match registered {
            Ok(()) => iced::window::run(child, move |handle| attach_natively(parent, handle)),
            Err(error) => Task::done(Err(error)),
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn attach_child_impl(
    _parent: window::Id,
    _child: window::Id,
) -> Task<Result<(), DialogBindingError>> {
    Task::done(Err(DialogBindingError::UnsupportedPlatform))
}

#[cfg(target_os = "macos")]
fn detach_child_impl(
    parent: window::Id,
    child: window::Id,
) -> Task<Result<(), DialogBindingError>> {
    iced::window::run(parent, move |handle| register_parent(parent, handle)).then(
        move |registered| match registered {
            Ok(()) => iced::window::run(child, move |handle| detach_natively(parent, handle)),
            Err(error) => Task::done(Err(error)),
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn detach_child_impl(
    _parent: window::Id,
    _child: window::Id,
) -> Task<Result<(), DialogBindingError>> {
    Task::done(Err(DialogBindingError::UnsupportedPlatform))
}

#[cfg(target_os = "macos")]
thread_local! {
    /// 主线程的父窗口注册表:值持有 `Retained<NSWindow>`,在第一个
    /// `window::run` 回调里写入、第二个回调取出。保留所有权是安全的边界,
    /// 而不是依赖两个回调之间"原地址仍然有效"。
    static PARENT_WINDOWS: std::cell::RefCell<
        std::collections::HashMap<window::Id, objc2::rc::Retained<objc2_app_kit::NSWindow>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(target_os = "macos")]
fn ensure_main_thread() -> Result<(), DialogBindingError> {
    if objc2::MainThreadMarker::new().is_some() {
        Ok(())
    } else {
        Err(DialogBindingError::NotMainThread)
    }
}

#[cfg(target_os = "macos")]
fn native_window(
    handle: &dyn window::Window,
) -> Result<objc2::rc::Retained<objc2_app_kit::NSWindow>, DialogBindingError> {
    use iced::window::raw_window_handle::RawWindowHandle;
    use objc2_app_kit::NSView;

    let handle = handle
        .window_handle()
        .map_err(|_| DialogBindingError::MissingWindowHandle)?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return Err(DialogBindingError::NotAppKit);
    };
    // 安全:`ns_view` 来自 `window::run` 回调内仍存活的窗口;`window()`
    // 返回的 `Retained` 会把 NSWindow 的存活范围延长到本次操作结束。
    let view: &NSView = unsafe { &*(appkit.ns_view.as_ptr() as *mut NSView) };
    view.window().ok_or(DialogBindingError::MissingNativeWindow)
}

#[cfg(target_os = "macos")]
fn register_parent(
    parent: window::Id,
    handle: &dyn window::Window,
) -> Result<(), DialogBindingError> {
    ensure_main_thread()?;
    let window = native_window(handle)?;
    PARENT_WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        // 剪掉已关闭的旧父窗,避免注册表无限保留已释放的窗口对象。
        windows.retain(|_, window| window.isVisible() && window.windowNumber() != 0);
        windows.insert(parent, window);
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn take_parent(
    parent: window::Id,
) -> Result<objc2::rc::Retained<objc2_app_kit::NSWindow>, DialogBindingError> {
    PARENT_WINDOWS
        .with(|windows| windows.borrow_mut().remove(&parent))
        .ok_or(DialogBindingError::ParentClosed)
}

#[cfg(target_os = "macos")]
fn attach_natively(
    parent: window::Id,
    handle: &dyn window::Window,
) -> Result<(), DialogBindingError> {
    use objc2_app_kit::{NSColor, NSWindowOrderingMode};

    ensure_main_thread()?;
    let parent = take_parent(parent)?;
    // 保留不等于仍然打开:父窗可能已在两个回调之间关闭。关闭的父窗不能
    // 重新挂回子窗,否则会得到不可见 / 无主的原生窗口。
    if !parent.isVisible() || parent.windowNumber() == 0 {
        return Err(DialogBindingError::ParentClosed);
    }
    let child = native_window(handle)?;
    child.setFrame_display(parent.frame(), true);
    child.setOpaque(false);
    child.setHasShadow(false);
    child.setBackgroundColor(Some(&NSColor::clearColor()));
    if child.parentWindow().is_none() {
        // 安全:两个对象都持有强引用,且回调在主线程执行。
        unsafe {
            parent.addChildWindow_ordered(&child, NSWindowOrderingMode::Above);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn detach_natively(
    parent: window::Id,
    handle: &dyn window::Window,
) -> Result<(), DialogBindingError> {
    ensure_main_thread()?;
    // 父窗可能已经关闭:取不到就只清掉子窗自己的父子关系。
    let _parent = take_parent(parent);
    let child = native_window(handle)?;
    if let Some(current_parent) = child.parentWindow() {
        current_parent.removeChildWindow(&child);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_is_re_exported_and_works_through_the_desktop_path() {
        let mut controller = DialogController::new(1, DialogKind::Settings);
        let effects = controller.open();
        assert!(matches!(effects.as_slice(), [DialogEffect::Create(_)]));
    }

    #[test]
    fn platform_support_matches_the_compiled_target() {
        assert_eq!(supported(), cfg!(target_os = "macos"));
    }

    #[test]
    fn geometry_is_shared() {
        let rect = DialogGeometry::default().card_rect(LogicalSize::new(1200.0, 800.0));
        assert_eq!(rect.width, 560.0);
    }
    #[test]
    fn rendering_failure_releases_the_partial_window_and_window_failure_skips_rendering() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct WindowResource(Arc<AtomicUsize>);
        impl Drop for WindowResource {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let result = try_create_dialog_resources(
            || Ok(WindowResource(dropped.clone())),
            |_| Err::<(), _>("synthetic surface failure".into()),
        );
        assert!(result.is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let result = try_create_dialog_resources(
            || Err::<WindowResource, _>("synthetic window failure".into()),
            |_| -> Result<(), String> {
                panic!("renderer must not run when window creation failed")
            },
        );
        assert!(result.is_err());
    }
}
