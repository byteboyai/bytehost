//! 独立原生浮层(overlay)窗口共用的**建窗 + 定位 + 鼠标穿透/圆角遮罩几何**——
//! 产品中立,不含任何业务。C10:从 dozer 的 `platform/overlay_window.rs` 上收
//! 那部分"不碰渲染器后端"的胶水;`iced_wgpu` 渲染管线(`OverlayGpu`)留在产品
//! 侧(bytehost-desktop 刻意不绑渲染器 feature,见 Cargo.toml 注释),经泛型
//! 工厂回调接入,故本模块**不依赖** `iced_wgpu`/`iced_winit`/`iced_renderer`。
//!
//! 覆盖的场景:模态卡片宿主(建一扇覆盖主窗口客户区的子窗口)+ Toast(右下角
//! 堆叠、整窗点击穿透且不聚焦)。产品的注入点只有:窗口标题 tag、GPU 工厂、
//! 以及(产品侧的)遮罩色与内容 `Element`。

use std::sync::Arc;

use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

/// 挂成主窗口子窗口后,在 macOS 上开启 `setAcceptsMouseMovedEvents:`。
///
/// **为什么必须做**:overlay 是无装饰(`with_decorations(false)`)+逐像素透明
/// (`with_transparent(true)`)的主窗口子 `NSWindow`。macOS 上这类窗口默认
/// **不会**收到 `mouseMoved` 事件,于是 winit 永远不会产生
/// `WindowEvent::CursorMoved`,导致 iced 内部的 `mouse::Cursor` 始终停在
/// `Unavailable` —— 这不仅让按钮 hover 样式失效,更会让 iced 无法对点击做
/// 命中测试。开启该项后 `CursorMoved` 才会正常派发,hover 与点击一并恢复。
///
/// 非 macOS 平台没有这套机制,winit 自带 `CursorMoved`,无需任何处理。
#[cfg(target_os = "macos")]
pub fn enable_overlay_mouse_moved_events(window: &Window) {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(ah) = handle.as_raw() else {
        return;
    };
    // 安全:`ns_view` 取自刚创建、仍存活的合法窗口,只借它拿 `window`,
    // 不持有/释放任何对象。
    let ns_view: &NSView = unsafe { &*(ah.ns_view.as_ptr() as *mut NSView) };
    let Some(ns_window) = ns_view.window() else {
        return;
    };
    // 安全:窗口存活期内调用一次即可(`open_child_window` 建窗后立即调),
    // 只翻转一个输入事件开关,不影响任何对象生命周期。
    ns_window.setAcceptsMouseMovedEvents(true);
}

/// 非 macOS 平台桩:无操作。
#[cfg(not(target_os = "macos"))]
pub fn enable_overlay_mouse_moved_events(_window: &Window) {}

/// 主窗口外框物理位置 + 物理尺寸 → overlay 窗口应放的物理位置与物理尺寸——
/// 覆盖整个主窗口客户区(不是只等于卡片大小;卡片由产品侧在内容层居中)。
pub fn full_window_overlay_bounds(
    main_outer_pos: PhysicalPosition<i32>,
    main_inner_size: PhysicalSize<u32>,
) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
    (main_outer_pos, main_inner_size)
}

/// 卡片在居中布局后,相对 overlay 窗口左上角的逻辑偏移。产品侧定位内嵌原生
/// 子视图(如 diff webview)时需要——它的坐标系是"整扇窗口",不是"卡片"。
pub fn centered_card_offset(
    window_logical_size: LogicalSize<f32>,
    card_logical_size: LogicalSize<f32>,
) -> LogicalPosition<f32> {
    LogicalPosition::new(
        (window_logical_size.width - card_logical_size.width) / 2.0,
        (window_logical_size.height - card_logical_size.height) / 2.0,
    )
}

/// 弹窗卡片默认宽度 = 主窗口逻辑宽度的 40%——模态弹窗统一宽度占比。
/// 高度逻辑各弹窗自行决定,这里只统一"宽度占比"这一项。
pub const POPUP_WIDTH_FRACTION: f32 = 0.4;

/// overlay 窗口本身是 borderless,macOS 不给它套系统圆角——遮罩若填满直角矩形
/// 会戳出主窗口可见圆角。给遮罩画成圆角补偿。取值须与 app 内容缩放脱钩
/// (匹配的是窗口服务端物理圆角),故为固定值。
pub const BACKDROP_CORNER_RADIUS: f32 = 10.0;

/// 取窗口逻辑尺寸:`inner_size()` 是物理像素,除以 `scale_factor` 折算成逻辑像素。
pub fn window_logical_size(window: &Window) -> LogicalSize<f32> {
    let scale = window.scale_factor();
    window.inner_size().to_logical::<f32>(scale)
}

/// 弹窗卡片尺寸:宽度恒为窗口宽度的 `POPUP_WIDTH_FRACTION`,高度由调用方给定。
pub fn popup_card_size(window: &Window, height: f32) -> LogicalSize<f32> {
    let width = window_logical_size(window).width * POPUP_WIDTH_FRACTION;
    LogicalSize::new(width, height)
}

/// 挂成主窗口子窗口的无装饰透明窗口。只建窗口本身,不建渲染管线(消费方按需建)。
///
/// **不去设 `WindowLevel::AlwaysOnTop`**:macOS 上它映射到跨 app 的全局悬浮层级,
/// 会让窗口飘在其他 app 之上。"盖过主窗口自己的 wry webview 子视图"靠
/// `with_parent_window` 触发的 `addChildWindow_ordered(NSWindowAbove)` 已保证。
pub fn open_child_window(
    main_window: &Window,
    pos: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    title: &str,
    el: &ActiveEventLoop,
) -> Arc<Window> {
    let window = build_child_window(main_window, pos, size, title, el, true);
    enable_overlay_mouse_moved_events(&window);
    window.focus_window();
    window
}

fn build_child_window(
    main_window: &Window,
    pos: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    title: &str,
    el: &ActiveEventLoop,
    active: bool,
) -> Arc<Window> {
    use winit::raw_window_handle::HasWindowHandle;

    let parent_handle = main_window
        .window_handle()
        .expect("main window handle")
        .as_raw();
    let attrs = Window::default_attributes()
        .with_title(title)
        .with_decorations(false)
        .with_transparent(true)
        .with_active(active)
        .with_position(pos)
        .with_inner_size(size);
    // Safety: `parent_handle` 取自仍存活的主窗口(`Ready` 持有的 `Arc<Window>`),
    // 本函数返回前主窗口不会被 drop。
    let attrs = unsafe { attrs.with_parent_window(Some(parent_handle)) };
    Arc::new(el.create_window(attrs).expect("create overlay window"))
}

/// 非模态提示窗口(Toast)用:建窗**不聚焦**(winit macOS 上 `with_active(false)`
/// 走 `orderFront` 而非 `makeKeyAndOrderFront`)且整窗**点击穿透**——点击落到
/// 下面的主窗口,这扇窗口永远不会成为 key window,不会抢终端键盘焦点。
/// 不调 `enable_overlay_mouse_moved_events`(穿透窗口收不到鼠标事件)。
pub fn open_child_window_unfocused(
    main_window: &Window,
    pos: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    title: &str,
    el: &ActiveEventLoop,
) -> Arc<Window> {
    let window = build_child_window(main_window, pos, size, title, el, false);
    let _ = window.set_cursor_hittest(false);
    window
}

/// `open` 方法体里"建覆盖主窗口的子窗口 + 建这扇窗口自己的渲染管线"那两步在多个
/// 模态卡片宿主之间逐字重复的部分。渲染管线由 `make_gpu` 工厂产出——本模块不认
/// 渲染器类型,只负责建窗并把 `window`/`size`/`scale` 交给工厂。
///
/// 返回 `(window, gpu)`,调用方按需对返回的 `window` 调 IME/原生菜单挂靠。
pub fn open_overlay<G>(
    main_window: &Arc<Window>,
    tag: &str,
    el: &ActiveEventLoop,
    make_gpu: impl FnOnce(&Arc<Window>, PhysicalSize<u32>, f64) -> G,
) -> (Arc<Window>, G) {
    let scale = main_window.scale_factor();
    let (pos, size) = full_window_overlay_bounds(
        main_window
            .outer_position()
            .unwrap_or(PhysicalPosition::new(0, 0)),
        main_window.inner_size(),
    );
    let window = open_child_window(main_window, pos, size, tag, el);
    let gpu = make_gpu(&window, size, scale);
    (window, gpu)
}

/// `reposition` 方法体在多个模态卡片宿主之间逐字重复的部分——主窗口移动
/// /resize 后跟着重新覆盖 + 重配置 surface。窗口大小只取决于主窗口。
pub fn reposition_overlay(
    window: &Window,
    main_outer_pos: PhysicalPosition<i32>,
    main_inner_size: PhysicalSize<u32>,
    mut reconfigure: impl FnMut(PhysicalSize<u32>, f64),
    scale: f64,
) {
    let (pos, size) = full_window_overlay_bounds(main_outer_pos, main_inner_size);
    window.set_outer_position(pos);
    if window.inner_size() != size {
        let _ = window.request_inner_size(size);
        reconfigure(size, scale);
    }
}

/// Toast 堆叠的几何常量(逻辑像素),由产品给定。用具名字段而非 5 个相邻 `f32`
/// 位置参数,避免顺序传错(见仓库约定:相邻同类型参数多时用参数结构体)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToastMetrics {
    /// 单条 Toast 的逻辑宽度。
    pub width: f32,
    /// 单条 Toast 的逻辑高度。
    pub height: f32,
    /// 相邻 Toast 之间的逻辑间距。
    pub gap: f32,
    /// 距主窗口右边缘的逻辑外边距。
    pub margin_right: f32,
    /// 距主窗口下边缘的逻辑外边距。
    pub margin_bottom: f32,
}

/// 主窗口外框位置 + 内区尺寸 + 缩放 + 条数 + Toast 几何常量 → 右下角 Toast 堆叠
/// 窗口的物理位置与尺寸。窗口尺寸钳到主窗口内且 ≥1(主窗口被缩得比 Toast 还小也
/// 不出 0/负尺寸,否则 wgpu surface 配置会失败)。`count == 0` 按 1 处理。定位约定
/// 与 [`full_window_overlay_bounds`] 一致:以 `outer_position` 为原点、`inner_size`
/// 为范围。
pub fn stack_bounds(
    main_outer_pos: PhysicalPosition<i32>,
    main_inner_size: PhysicalSize<u32>,
    scale: f64,
    count: usize,
    metrics: ToastMetrics,
) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
    let ToastMetrics {
        width: toast_width,
        height: toast_height,
        gap,
        margin_right,
        margin_bottom,
    } = metrics;
    let n = count.max(1);
    let px = |logical: f32| (f64::from(logical) * scale).round() as i64;
    let main_w = i64::from(main_inner_size.width);
    let main_h = i64::from(main_inner_size.height);
    let w = px(toast_width).min(main_w).max(1);
    let h = px(toast_height * n as f32 + gap * (n - 1) as f32)
        .min(main_h)
        .max(1);
    let x = i64::from(main_outer_pos.x) + (main_w - w - px(margin_right)).max(0);
    let y = i64::from(main_outer_pos.y) + (main_h - h - px(margin_bottom)).max(0);
    (
        PhysicalPosition::new(x as i32, y as i32),
        PhysicalSize::new(w as u32, h as u32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用固定 Toast 几何(与产品默认一致)。
    fn metrics() -> ToastMetrics {
        ToastMetrics {
            width: 380.0,
            height: 56.0,
            gap: 8.0,
            margin_right: 16.0,
            margin_bottom: 48.0,
        }
    }

    #[test]
    fn full_window_overlay_bounds_matches_main_window_exactly() {
        let (pos, size) = full_window_overlay_bounds(
            PhysicalPosition::new(100, 50),
            PhysicalSize::new(1200, 800),
        );
        assert_eq!(pos, PhysicalPosition::new(100, 50));
        assert_eq!(size, PhysicalSize::new(1200, 800));
    }

    #[test]
    fn centered_card_offset_centers_within_window() {
        let offset = centered_card_offset(
            LogicalSize::new(1200.0, 800.0),
            LogicalSize::new(400.0, 640.0),
        );
        assert_eq!(offset.x, 400.0);
        assert_eq!(offset.y, 80.0);
    }

    #[test]
    fn centered_card_offset_zero_when_card_fills_window() {
        let offset = centered_card_offset(
            LogicalSize::new(600.0, 640.0),
            LogicalSize::new(600.0, 640.0),
        );
        assert_eq!(offset.x, 0.0);
        assert_eq!(offset.y, 0.0);
    }

    #[test]
    fn one_toast_anchors_bottom_right_of_main_window() {
        let (pos, size) = stack_bounds(
            PhysicalPosition::new(100, 50),
            PhysicalSize::new(1200, 800),
            1.0,
            1,
            metrics(),
        );
        assert_eq!(size, PhysicalSize::new(380, 56));
        assert_eq!(pos, PhysicalPosition::new(904, 746));
    }

    #[test]
    fn three_toasts_grow_upward_with_gaps() {
        let (pos, size) = stack_bounds(
            PhysicalPosition::new(100, 50),
            PhysicalSize::new(1200, 800),
            1.0,
            3,
            metrics(),
        );
        // 3*56 + 2*8 = 184
        assert_eq!(size, PhysicalSize::new(380, 184));
        assert_eq!(pos, PhysicalPosition::new(904, 618));
    }

    #[test]
    fn toast_tiny_main_window_clamps_and_never_goes_zero() {
        let (pos, size) = stack_bounds(
            PhysicalPosition::new(100, 50),
            PhysicalSize::new(200, 30),
            1.0,
            1,
            metrics(),
        );
        assert_eq!(size, PhysicalSize::new(200, 30));
        assert_eq!(pos, PhysicalPosition::new(100, 50));

        let (_, size) = stack_bounds(
            PhysicalPosition::new(0, 0),
            PhysicalSize::new(0, 0),
            1.0,
            1,
            metrics(),
        );
        assert!(size.width >= 1 && size.height >= 1);
    }

    #[test]
    fn toast_zero_count_is_treated_as_one_so_size_is_never_zero() {
        let (_, size) = stack_bounds(
            PhysicalPosition::new(0, 0),
            PhysicalSize::new(1200, 800),
            1.0,
            0,
            metrics(),
        );
        assert_eq!(size, PhysicalSize::new(380, 56));
    }
}
