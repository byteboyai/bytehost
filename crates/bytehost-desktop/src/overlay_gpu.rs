//! 独立原生浮层窗口共用的 **wgpu 渲染管线 + 遮罩外壳**——产品中立,不含任何业务。
//!
//! C10 方案 C:在方案 B(建窗/定位几何已上收 [`crate::overlay`])之上,把
//! `OverlayGpu`(每扇浮层窗口自己的渲染资源 + `redraw`/`dispatch`/
//! `track_and_convert` 三段在多个模态卡片宿主之间逐字重复的胶水)与
//! `backdrop_card`/`backdrop_card_shrink`(整窗居中 + 半透明遮罩外壳)一并上收,
//! 使产品侧浮层文件不再自写"建窗/透明/穿透/alpha 模式"机制。
//!
//! **feature-gated**:本模块只在 `overlay` feature 打开时编译,届时 `bytehost-desktop`
//! 才会引入 `iced_wgpu`/`iced_winit`/`iced_widget`/`iced_renderer`。默认关闭时依赖树
//! 不变(见 `Cargo.toml` 注释与 R2 修复)。
//!
//! 产品的注入点只剩:内容 `Element`、遮罩色 `scrim`(宿主不认识主题 token,由调用方传入)。

use std::sync::Arc;

use iced_wgpu::graphics::{Shell, Viewport};
use iced_wgpu::{Engine, Renderer, wgpu};
use iced_winit::Clipboard;
use iced_winit::conversion;
use iced_winit::core::input_method::InputMethod;
use iced_winit::core::time::Instant;
use iced_winit::core::window;
use iced_winit::core::{Color, Element, Event, Font, Pixels, Size, mouse};
use iced_winit::runtime::user_interface::{self, UserInterface};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::WindowEvent;
use winit::keyboard::ModifiersState;
use winit::window::Window;

use crate::overlay::BACKDROP_CORNER_RADIUS;

/// 按 iced 本帧算出的 `mouse_interaction` 刷新 overlay 窗口的鼠标光标。
///
/// 这扇窗口是独立原生窗口,主窗口那套 `apply_mouse_cursor` 不覆盖它——这里只做
/// 最基本的一件事:把光标形状写进窗口。缺了这步,弹窗里的按钮悬停时不会变成手形。
///
/// `OverlayGpu::redraw`/`dispatch` 内部已自动调用;自己手写 `UserInterface::build`/
/// `update` 的消费方需在 `update` 拿到 `State::Updated` 后自行调用它。
pub fn apply_cursor(window: &Window, interaction: mouse::Interaction) {
    if let Some(icon) = conversion::mouse_interaction(interaction) {
        window.set_cursor(icon);
        window.set_cursor_visible(true);
    } else {
        window.set_cursor_visible(false);
    }
}

/// iced 本帧算出的输入法请求 → 候选条的锚点矩形(窗口逻辑坐标)。
///
/// 只有 `Enabled`(有 `text_input` 聚焦、要输入法)才有锚点,锚点就是 iced 算出的文本光标矩形;
/// 空输入框里的插入点宽度是 0,宽度至少取 1,否则部分输入法会把候选条放到窗口原点。
/// `Disabled` 返回 `None`:没有锚点可给,候选条位置不动。
pub fn ime_cursor_area(
    input_method: &InputMethod,
) -> Option<(LogicalPosition<f32>, LogicalSize<f32>)> {
    match input_method {
        InputMethod::Enabled { cursor, .. } => Some((
            LogicalPosition::new(cursor.x, cursor.y),
            LogicalSize::new(cursor.width.max(1.0), cursor.height),
        )),
        InputMethod::Disabled => None,
    }
}

/// 把本帧的输入法请求写进 overlay 窗口,让系统的输入法候选条跟着文本光标走。
///
/// 这扇窗口是独立原生窗口,主窗口那套"界面更新后设置候选条位置"的逻辑不覆盖它;缺了这步,
/// 候选条会落在系统默认位置(整个窗口左上角)。`OverlayGpu::redraw`/`dispatch` 内部已自动调用;
/// 自己手写 `UserInterface::build`/`update` 的消费方需在 `update` 拿到 `State::Updated` 后自行调用,
/// 与 [`apply_cursor`] 一起。窗口还需要 `set_ime_allowed(true)`(建窗时设)。
pub fn apply_input_method(window: &Window, input_method: &InputMethod) {
    if let Some((position, size)) = ime_cursor_area(input_method) {
        window.set_ime_cursor_area(position, size);
    }
}

/// 独立原生窗口自己的一份渲染资源——不含 `Device`/`Queue`/`Adapter`/`Instance`
/// (全部从主窗口 `Ready` 借来的共享句柄,`Device`/`Queue` 便宜 `Clone`),
/// `iced_wgpu` 的 `Renderer` 内部持有 `Engine`,不能跨窗口共享,每扇窗口必须自己一份。
/// 字段 `pub`:消费方需要在各自的 `redraw`/`handle_input` 里对单个字段做细粒度可变
/// 借用(比如同时借 `&mut renderer` 和 `&viewport`),包一层 getter 反而更啰嗦。
pub struct OverlayGpu {
    pub surface: wgpu::Surface<'static>,
    pub format: wgpu::TextureFormat,
    pub renderer: Renderer,
    pub cache: user_interface::Cache,
    pub viewport: Viewport,
    pub clipboard: Clipboard,
}

impl OverlayGpu {
    /// 从共享的 `Instance`/`Adapter`/`Device`/`Queue` 为 `window` 建一份独立渲染管线,
    /// `size`/`scale` 是这扇窗口当前的物理尺寸/缩放。
    pub fn open(
        window: &Arc<Window>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: winit::dpi::PhysicalSize<u32>,
        scale: f64,
    ) -> OverlayGpu {
        let surface = instance
            .create_surface(window.clone())
            .expect("create overlay surface");
        let capabilities = surface.get_capabilities(adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .expect("get overlay surface format");
        surface.configure(
            device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                // `Auto` 在 Metal 后端会落到 `Opaque`(能力表只有 `[Opaque, PostMultiplied]`,
                // `Auto` 走 wgpu-core 的兜底顺序优先选 `Opaque`),这会让
                // `CAMetalLayer.opaque = true`,合成时完全无视 alpha 通道——即使把内容
                // 清成 `Color::TRANSPARENT` 也只会显示纯黑而不是透明。显式指定
                // `PostMultiplied` 才能让窗口真正透明。
                alpha_mode: wgpu::CompositeAlphaMode::PostMultiplied,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );

        let engine = Engine::new(
            adapter,
            device.clone(),
            queue.clone(),
            format,
            None,
            Shell::headless(),
        );
        let renderer = Renderer::new(engine, Font::default(), Pixels::from(16));
        let viewport =
            Viewport::with_physical_size(Size::new(size.width, size.height), scale as f32);
        let clipboard = Clipboard::connect(window.clone());

        OverlayGpu {
            surface,
            format,
            renderer,
            cache: user_interface::Cache::new(),
            viewport,
            clipboard,
        }
    }

    /// resize 后重配置 surface + viewport(不重建 `Engine`/`Renderer`)。
    /// 调用方负责判断"要不要重配"。
    pub fn reconfigure(
        &mut self,
        device: &wgpu::Device,
        size: winit::dpi::PhysicalSize<u32>,
        scale: f64,
    ) {
        self.viewport =
            Viewport::with_physical_size(Size::new(size.width, size.height), scale as f32);
        self.surface.configure(
            device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                // 同 `open()`——`reconfigure` 必须用一样的 `alpha_mode`,否则 resize
                // 之后又退回不透明。
                alpha_mode: wgpu::CompositeAlphaMode::PostMultiplied,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );
    }

    /// "已经拿到这一帧的 `content`,要把它画出来"那段在多个模态卡片宿主之间逐字
    /// 重复的部分:建 `UserInterface` →空事件 `update`(只重算布局,不产生消息)
    /// →`draw`→存 cache→取帧→present。调用方自己的 `redraw` 只需拼出 `content`
    /// (产品侧拼好,可先经 [`backdrop_card`] 套遮罩)再转调本方法。
    pub fn redraw<Message>(
        &mut self,
        window: &Window,
        cursor: mouse::Cursor,
        content: Element<'_, Message, iced_widget::Theme, iced_renderer::Renderer>,
    ) {
        let mut interface = UserInterface::build(
            content,
            self.viewport.logical_size(),
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        // 必须喂一个 `RedrawRequested` 事件而非空切片:iced 的 `button`/`text_input`
        // 只在处理这个事件时才把当帧算出的 hover/focused `Status` 提交进 `draw()`
        // 读的缓存字段。
        let (state, _) = interface.update(
            &[Event::Window(
                window::Event::RedrawRequested(Instant::now()),
            )],
            cursor,
            &mut self.renderer,
            &mut self.clipboard,
            &mut Vec::new(),
        );
        if let user_interface::State::Updated {
            mouse_interaction,
            input_method,
            ..
        } = state
        {
            apply_cursor(window, mouse_interaction);
            apply_input_method(window, &input_method);
        }
        interface.draw(
            &mut self.renderer,
            &iced_winit::core::Theme::Dark,
            &iced_winit::core::renderer::Style::default(),
            cursor,
        );
        self.cache = interface.into_cache();

        let Ok(frame) = self.surface.get_current_texture() else {
            window.request_redraw();
            return;
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // `Some(Color::TRANSPARENT)` 显式清空 surface——若这一帧的内容没有画满整扇
        // 窗口(如卡片高度 shrink-fit),未画到的区域会露出上一帧/未初始化纹理的
        // 内容,在透明弹窗窗口上表现为黑块。清成透明色让内容之外始终透出主窗口。
        self.renderer.present(
            Some(Color::TRANSPARENT),
            frame.texture.format(),
            &view,
            &self.viewport,
        );
        frame.present();
    }

    /// "已经拿到转换后的 iced 事件,要建 `UserInterface` 派发"那段在多个模态卡片
    /// 宿主之间逐字重复的部分:建界面→带事件 `update`(收集消息)→存 cache
    /// →请求重绘→返回消息。
    pub fn dispatch<Message>(
        &mut self,
        window: &Window,
        cursor: mouse::Cursor,
        content: Element<'_, Message, iced_widget::Theme, iced_renderer::Renderer>,
        iced_event: Event,
    ) -> Vec<Message> {
        let events: [Event; 1] = [iced_event];
        let mut interface = UserInterface::build(
            content,
            self.viewport.logical_size(),
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let mut messages = Vec::new();
        let (state, _) = interface.update(
            &events,
            cursor,
            &mut self.renderer,
            &mut self.clipboard,
            &mut messages,
        );
        if let user_interface::State::Updated {
            mouse_interaction,
            input_method,
            ..
        } = state
        {
            apply_cursor(window, mouse_interaction);
            apply_input_method(window, &input_method);
        }
        self.cache = interface.into_cache();
        window.request_redraw();
        messages
    }

    /// `handle_input` 开头"追踪 modifiers/cursor、把 winit 事件转换成 iced 事件"
    /// 那段在多个模态卡片宿主之间逐字重复的部分(不含 Esc 分支——各宿主的取消消息
    /// 不同,留给调用方自己处理)。返回 `None` 表示这个 winit 事件 iced 不关心
    /// (如未映射的按键),调用方应直接返回空消息列表,不需要再建 `UserInterface`。
    pub fn track_and_convert(
        &self,
        cursor: &mut mouse::Cursor,
        modifiers: &mut ModifiersState,
        event: &WindowEvent,
    ) -> Option<Event> {
        if let WindowEvent::ModifiersChanged(new_modifiers) = event {
            *modifiers = new_modifiers.state();
        }
        if let WindowEvent::CursorMoved { position, .. } = event {
            *cursor = mouse::Cursor::Available(conversion::cursor_position(
                *position,
                self.viewport.scale_factor(),
            ));
        }
        conversion::window_event(event.clone(), self.viewport.scale_factor(), *modifiers)
    }
}

/// 给卡片元素套一层"整窗口居中 + 半透明背景遮罩"的外壳。遮罩色由调用方传入
/// (宿主不认识产品主题 token);遮罩容器没有 `on_press`:点击穿不透到主窗体,
/// 但也不产生任何消息去关弹窗。遮罩圆角见 [`BACKDROP_CORNER_RADIUS`]。
pub fn backdrop_card<'a, Message: 'a>(
    card: Element<'a, Message, iced_widget::Theme, iced_renderer::Renderer>,
    card_logical: iced_winit::core::Size,
    scrim: Color,
) -> Element<'a, Message, iced_widget::Theme, iced_renderer::Renderer> {
    use iced_widget::container;
    use iced_widget::core::{Length, alignment};

    container(
        container(card)
            .width(Length::Fixed(card_logical.width))
            .height(Length::Fixed(card_logical.height)),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(alignment::Horizontal::Center)
    .align_y(alignment::Vertical::Center)
    .style(move |_theme: &iced_widget::Theme| container::Style {
        background: Some(scrim.into()),
        border: iced_widget::core::Border {
            radius: BACKDROP_CORNER_RADIUS.into(),
            ..Default::default()
        },
        ..container::Style::default()
    })
    .into()
}

/// 同 [`backdrop_card`],但卡片**高度自适应内容**(`Length::Shrink`)——内容行数
/// 不定时不会被裁切。宽度仍由 `card_width` 固定;高度交给 iced 按真实排版撑开。
pub fn backdrop_card_shrink<'a, Message: 'a>(
    card: Element<'a, Message, iced_widget::Theme, iced_renderer::Renderer>,
    card_width: f32,
    scrim: Color,
) -> Element<'a, Message, iced_widget::Theme, iced_renderer::Renderer> {
    use iced_widget::container;
    use iced_widget::core::{Length, alignment};

    container(
        container(card)
            .width(Length::Fixed(card_width))
            .height(Length::Shrink),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(alignment::Horizontal::Center)
    .align_y(alignment::Vertical::Center)
    .style(move |_theme: &iced_widget::Theme| container::Style {
        background: Some(scrim.into()),
        border: iced_widget::core::Border {
            radius: BACKDROP_CORNER_RADIUS.into(),
            ..Default::default()
        },
        ..container::Style::default()
    })
    .into()
}

#[cfg(test)]
mod ime_tests {
    use super::*;
    use iced_winit::core::input_method::{InputMethod, Purpose};
    use iced_winit::core::{Point, Rectangle};

    /// 有 text_input 聚焦要输入法时,候选条锚点就是 iced 算出的文本光标矩形(逻辑坐标)。
    #[test]
    fn an_enabled_input_method_anchors_the_candidate_bar_at_the_text_cursor() {
        let im = InputMethod::Enabled {
            cursor: Rectangle::new(Point::new(120.0, 340.0), Size::new(2.0, 18.0)),
            purpose: Purpose::Normal,
            preedit: None,
        };
        let (pos, size) = ime_cursor_area(&im).expect("Enabled 必须给出锚点");
        assert_eq!((pos.x, pos.y), (120.0, 340.0));
        assert_eq!((size.width, size.height), (2.0, 18.0));
    }

    /// iced 的光标矩形宽度可以是 0(空输入框里的插入点):给系统一个宽度为 0 的矩形,
    /// 部分输入法会把候选条放到窗口原点,所以宽度至少取 1。
    #[test]
    fn a_zero_width_cursor_gets_a_one_pixel_wide_anchor() {
        let im = InputMethod::Enabled {
            cursor: Rectangle::new(Point::new(10.0, 20.0), Size::new(0.0, 18.0)),
            purpose: Purpose::Normal,
            preedit: None,
        };
        let (_, size) = ime_cursor_area(&im).unwrap();
        assert_eq!(size.width, 1.0);
        assert_eq!(size.height, 18.0);
    }

    /// 没有任何 text_input 要输入法时不动候选条位置(没有锚点可给)。
    #[test]
    fn a_disabled_input_method_gives_no_anchor() {
        assert!(ime_cursor_area(&InputMethod::Disabled).is_none());
    }
}
