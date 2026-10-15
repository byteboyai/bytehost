//! macOS 窗口装饰胶水(移植自 Dozer `platform/window.rs`):窗口用"透明标题栏 + 全尺寸内容视图",顶栏取代系统标题栏后,
//! 要自己处理两件事:
//! 1. 红绿灯垂直居中到顶栏里(系统仍按 28pt 旧标题栏排版,会贴着顶栏上沿);
//! 2. 原生全屏不绘制标题栏,红绿灯会跟着移出窗口:全屏期间把同一组原生按钮临时挂到内容视图。
//!
//! 与 Dozer 的区别:Dozer 自己持有 winit 窗口;这里经 iced 的 `window::run` 拿 `&dyn HasWindowHandle`。
//!
//! **没有移植 Dozer 的"顶栏拖窗保护"**(覆写 `mouseDownCanMoveWindow`):透明标题栏保留 ~28pt 的原生拖窗带,落在带内的
//! 按下事件在送到 iced 之前就被 AppKit 当拖窗,顶栏上可交互的页签 / 按钮会被吞。S1 的顶栏没有任何可交互控件
//! (标题只是展示,"＋"置灰),不需要它;顶栏有了项目页签(S5)时再移植,并在那时加真实窗口验证。

use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSView, NSWindow, NSWindowButton};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// 红绿灯按钮的 y(AppKit 坐标)。目标 y 直接由按钮父视图的**当前** bounds 算,不缓存首次看到的 frame:
/// 窗口从最大化恢复时 `Resized` 可能早于 AppKit 对标题栏按钮的重新布局,缓存旧 frame 会让红绿灯永久偏低。
/// 纯函数,可单测。
pub fn traffic_light_origin_y(
    bounds_origin_y: f64,
    bounds_height: f64,
    button_height: f64,
    top_bar_height: f64,
    flipped: bool,
) -> f64 {
    if flipped {
        bounds_origin_y + (top_bar_height - button_height) / 2.0
    } else {
        bounds_origin_y + bounds_height - (top_bar_height + button_height) / 2.0
    }
}

fn content_view(window: &dyn HasWindowHandle) -> Option<&NSView> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(ah) = handle.as_raw() else {
        return None;
    };
    // SAFETY: `ns_view` 是已装进窗口的合法 NSView 指针,窗口由主线程持有;这里只借只读引用,不持有 / 释放。
    Some(unsafe { &*(ah.ns_view.as_ptr() as *const NSView) })
}

fn window_buttons(window: &NSWindow) -> [Option<Retained<NSButton>>; 3] {
    [
        window.standardWindowButton(NSWindowButton::CloseButton),
        window.standardWindowButton(NSWindowButton::MiniaturizeButton),
        window.standardWindowButton(NSWindowButton::ZoomButton),
    ]
}

/// 把三个红绿灯在垂直方向居中到 `top_bar_height` 高的自绘顶栏里。放在 `Resized`(含首屏、全屏进出、拖拽缩放)里重设;幂等。
pub fn center_traffic_lights(window: &dyn HasWindowHandle, top_bar_height: f32) {
    let Some(content) = content_view(window) else {
        return;
    };
    let Some(ns_window) = content.window() else {
        return;
    };
    for button in window_buttons(&ns_window).into_iter().flatten() {
        // SAFETY: 读按钮的父视图(标题栏容器),只读几何。
        let Some(superview) = (unsafe { button.superview() }) else {
            continue;
        };
        let frame = button.frame();
        let bounds = superview.bounds();
        let mut origin = frame.origin;
        origin.y = traffic_light_origin_y(
            bounds.origin.y,
            bounds.size.height,
            frame.size.height,
            top_bar_height as f64,
            superview.isFlipped(),
        );
        button.setFrameOrigin(origin);
    }
}

// ---- 全屏期间的红绿灯 ----

#[derive(Clone, Copy)]
struct TrafficLightHome {
    parent: usize,
    frame: objc2_foundation::NSRect,
    content_frame: objc2_foundation::NSRect,
}

thread_local! {
    static HOMES: std::cell::RefCell<Option<[TrafficLightHome; 3]>> = const { std::cell::RefCell::new(None) };
}

/// 原生全屏不绘制标题栏,标准红绿灯也随之移出窗口。进入全屏前保存按钮在内容视图中的等价位置;
/// 全屏完成后把同一组原生按钮临时挂到内容视图(外观、hover、点击仍由 AppKit 提供);退出全屏再交还原来的标题栏父视图。
fn save_home(content: &NSView) {
    let Some(window) = content.window() else {
        return;
    };
    let buttons = window_buttons(&window);
    HOMES.with(|saved| {
        let mut saved = saved.borrow_mut();
        if saved.is_some() {
            return;
        }
        *saved = Some(std::array::from_fn(|i| {
            let button = buttons[i].as_ref().expect("standard window button");
            // SAFETY: 读按钮父视图指针,只用于之后原样放回。
            let parent = unsafe { button.superview() }.expect("window button superview");
            TrafficLightHome {
                parent: std::ptr::from_ref(&*parent) as usize,
                frame: button.frame(),
                content_frame: content.convertRect_fromView(button.frame(), Some(&parent)),
            }
        }));
    });
}

fn move_to_content(content: &NSView) {
    save_home(content);
    let Some(window) = content.window() else {
        return;
    };
    let buttons = window_buttons(&window);
    HOMES.with(|saved| {
        let saved = saved.borrow();
        let homes = saved.as_ref().expect("homes saved above");
        for (button, home) in buttons.into_iter().zip(homes) {
            let Some(button) = button else { continue };
            button.removeFromSuperview();
            content.addSubview(&button);
            button.setFrame(home.content_frame);
            button.setHidden(false);
        }
    });
}

fn restore(content: &NSView) {
    let Some(window) = content.window() else {
        return;
    };
    let buttons = window_buttons(&window);
    HOMES.with(|saved| {
        let Some(homes) = saved.borrow_mut().take() else {
            return;
        };
        for (button, home) in buttons.into_iter().zip(homes) {
            let Some(button) = button else { continue };
            // SAFETY: 保存的父视图是按钮原来的标题栏视图,属于同一个存活的 NSWindow,全屏过渡期间由该视图层级持有。
            let parent = unsafe { &*(home.parent as *const NSView) };
            button.removeFromSuperview();
            parent.addSubview(&button);
            button.setFrame(home.frame);
            button.setHidden(false);
        }
    });
}

/// 在 AppKit 的全屏生命周期边界搬移 / 还原红绿灯(只靠 `Resized` 不可靠:最后一次 resize 可能发生在 AppKit 把标题栏隐藏之前)。
pub fn install_fullscreen_guard(window: &dyn HasWindowHandle) {
    use block2::RcBlock;
    use objc2_app_kit::{
        NSWindowDidEnterFullScreenNotification, NSWindowDidExitFullScreenNotification,
        NSWindowWillEnterFullScreenNotification, NSWindowWillExitFullScreenNotification,
    };
    use objc2_foundation::{NSNotificationCenter, NSOperationQueue};

    let Some(content) = content_view(window) else {
        return;
    };
    let content_ptr = content as *const NSView as usize;
    let Some(ns_window) = content.window() else {
        return;
    };
    let center = NSNotificationCenter::defaultCenter();
    let queue = NSOperationQueue::mainQueue();

    let install = |name, action: u8| {
        let block = RcBlock::new(move |_| {
            // SAFETY: 观察者的生命周期与进程相同,主窗口 / 内容视图比它活得久;AppKit 在主线程投递这些通知。
            let content = unsafe { &*(content_ptr as *const NSView) };
            match action {
                0 => save_home(content),
                1 => move_to_content(content),
                _ => restore(content),
            }
        });
        // SAFETY: `name` 是 AppKit 的通知名常量。
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                Some(&ns_window),
                Some(&queue),
                &block,
            )
        };
        // 主窗口活多久观察者就活多久;不为拆除再另设一个所有者。
        std::mem::forget(observer);
    };
    install(unsafe { NSWindowWillEnterFullScreenNotification }, 0);
    install(unsafe { NSWindowDidEnterFullScreenNotification }, 1);
    install(unsafe { NSWindowWillExitFullScreenNotification }, 2);
    install(unsafe { NSWindowDidExitFullScreenNotification }, 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_lights_are_centered_in_the_top_bar_in_both_coordinate_systems() {
        // unflipped (origin bottom-left): a 40pt bar at the top of a 28pt title bar region of height 28, 16pt buttons
        assert_eq!(
            traffic_light_origin_y(0.0, 28.0, 16.0, 40.0, false),
            0.0 + 28.0 - (40.0 + 16.0) / 2.0
        );
        assert_eq!(traffic_light_origin_y(4.0, 32.0, 16.0, 40.0, false), 8.0);
        // flipped (origin top-left)
        assert_eq!(traffic_light_origin_y(0.0, 28.0, 16.0, 40.0, true), 12.0);
        // repeated calls with the same bounds give the same answer (idempotent, no cached baseline)
        let a = traffic_light_origin_y(1.0, 30.0, 14.0, 40.0, false);
        assert_eq!(a, traffic_light_origin_y(1.0, 30.0, 14.0, 40.0, false));
    }
}
