//! 最小真实窗口示例:注册一个 fake 面板,演示激活 / 异步完成 / 关闭 / 窗口退出。
//!
//! 运行(需显示环境):
//!   cargo run -p bytehost-desktop --example minimal
//! 无交互自检:
//!   SELFTEST=1 cargo run -p bytehost-desktop --example minimal
//!
//! 它证明 desktop 壳可被**任意产品**驱动:本示例的产品消息类型(`Msg`)与服务类型
//! (`FakeServices`)都不来自任何真实产品 crate。

use bytehost_core::{
    Generation, Insets, LogicalRect, PanelInstanceId, RequestKey, RequestTracker, ScopeId,
    SurfaceSlot, host_rect,
};
use bytehost_desktop::dispatch::TokioExecutor;
use bytehost_desktop::{HostMessage, Product, SurfaceEvent, panel};

use iced::{Element, Size, Subscription, Task, window};

#[derive(Debug, Clone)]
enum Msg {
    WindowReady(window::Id, Size),
    Load,
    Loaded(String),
    Close,
    HostBuilt(Result<(), String>),
    Quit,
}

/// 一个假的服务上下文(与任何产品无关)。
#[derive(Clone, Default)]
struct FakeServices {
    greeting: &'static str,
}

struct App {
    window: Option<window::Id>,
    size: Size,
    slot: SurfaceSlot,
    tracker: RequestTracker,
    greeting: Option<String>,
    insets: Insets,
    note: String,
    built: bool,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            size: app_size(),
            slot: SurfaceSlot::MAIN,
            tracker: RequestTracker::new(
                1,
                ScopeId::new("minimal").expect("合法 scope"),
                PanelInstanceId::new(0, 0),
                Generation::new(0),
            ),
            greeting: None,
            insets: Insets { top: 44.0 },
            note: "booting".into(),
            built: false,
        }
    }

    fn rect(&self) -> LogicalRect {
        host_rect(self.size.width, self.size.height, self.insets)
    }

    /// 演示:后台异步完成一条读取,经 core 请求身份校验后投回。
    ///
    /// 这里直接调用 core 的执行原语并把结果经 `Task::perform` 回灌;
    /// 生产里产品会用 `ProxyEmitter` + iced `Sender`(见 `dispatch` 模块)。
    fn start_load(&self) -> Task<Msg> {
        let token = self.tracker.begin(RequestKey::new("greeting"));
        let tracker = self.tracker.clone();
        Task::perform(
            async move {
                let services = FakeServices {
                    greeting: "hello from bytehost-desktop",
                };
                let exec = TokioExecutor::current();
                // 后台完成(用 Tokio 若可用,否则内联),只取当前有效结果。
                match exec {
                    Some(exec) => {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let emitter = ChEmitter(tx);
                        bytehost_core::run_scoped(
                            &tracker,
                            token,
                            services,
                            &exec,
                            &emitter,
                            |s| s.greeting.to_string(),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                        rx.try_recv()
                            .map(|env| env.payload)
                            .unwrap_or_else(|_| "<superseded>".into())
                    }
                    None => {
                        if tracker.accepts(&token) {
                            services.greeting.to_string()
                        } else {
                            "<superseded>".into()
                        }
                    }
                }
            },
            Msg::Loaded,
        )
    }
}

#[derive(Clone)]
struct ChEmitter(std::sync::mpsc::Sender<bytehost_core::ResultEnvelope<String>>);

impl bytehost_core::Emitter<bytehost_core::ResultEnvelope<String>> for ChEmitter {
    fn emit(&self, message: bytehost_core::ResultEnvelope<String>) {
        let _ = self.0.send(message);
    }
}

impl Product<Msg> for App {
    fn update(&mut self, msg: HostMessage<Msg>) -> Task<Msg> {
        if let HostMessage::Product(m) = msg {
            return self.update_product(m);
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Msg> {
        use iced::widget::{button, column, row, text};
        column![
            text(format!(
                "bytehost-desktop minimal — {}x{} — built={} — {}",
                self.size.width, self.size.height, self.built, self.note
            )),
            text(self.greeting.clone().unwrap_or_else(|| "<loading>".into())),
            row![
                button("Load").on_press(Msg::Load),
                button("Close panel").on_press(Msg::Close),
                button("Quit").on_press(Msg::Quit),
            ]
            .spacing(8),
        ]
        .spacing(12)
        .padding(16)
        .into()
    }

    fn on_activated(&mut self, _slot: SurfaceSlot) {
        self.note = "activated".into();
    }

    fn on_closed(&mut self, _slot: SurfaceSlot) {
        self.note = "closed".into();
    }
}

impl App {
    fn update_product(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::WindowReady(id, size) => {
                self.window = Some(id);
                self.size = size;
                self.build_panel(id)
            }
            Msg::Load => self.start_load(),
            Msg::Loaded(s) => {
                self.greeting = Some(s);
                Task::none()
            }
            Msg::Close => {
                panel::destroy(self.slot);
                self.built = false;
                self.note = "destroyed".into();
                self.on_closed(self.slot);
                Task::none()
            }
            Msg::HostBuilt(Ok(())) => {
                self.built = true;
                self.note = "webview built".into();
                self.on_activated(self.slot);
                // 顺带演示宿主事件也能走 HostMessage::Surface。
                let evt = SurfaceEvent::Created { slot: self.slot };
                let _ = bytehost_desktop::route_lifecycle(self, evt);
                Task::none()
            }
            Msg::HostBuilt(Err(e)) => {
                self.note = format!("build failed: {e}");
                Task::none()
            }
            Msg::Quit => iced::exit(),
        }
    }

    fn build_panel(&self, id: window::Id) -> Task<Msg> {
        let slot = self.slot;
        let html = "<html><body style='margin:0;background:#0a0e16;color:#FFE5B4;\
                    font-family:sans-serif'><h3>minimal fake panel</h3></body></html>";
        let spec = panel::ChildSpec::new(slot, html, self.rect());
        let build = panel::build_child(id, spec, |built| Msg::HostBuilt(built.result));
        if std::env::var("SELFTEST").is_ok() {
            Task::done(Msg::Quit)
        } else {
            build
        }
    }
}

fn boot() -> (App, Task<Msg>) {
    let app = App::new();
    let size = app.size;
    let t = panel::oldest_window().then(move |id| match id {
        Some(id) => panel::window_size(id).map(move |s| Msg::WindowReady(id, s.unwrap_or(size))),
        None => Task::none(),
    });
    (app, t)
}

fn app_size() -> Size {
    Size::new(820.0, 560.0)
}

fn view(app: &App) -> Element<'_, Msg> {
    Product::view(app)
}

fn subscription(_app: &App) -> Subscription<Msg> {
    iced::event::listen_with(|evt, _status, _id| match evt {
        iced::Event::Window(window::Event::CloseRequested) => Some(Msg::Quit),
        _ => None,
    })
}

fn update(app: &mut App, msg: Msg) -> Task<Msg> {
    // 窗口消息经宿主泛型包装交给产品;证明产品不直接处理宿主 enum。
    Product::update(app, HostMessage::Product(msg))
}

fn main() -> iced::Result {
    iced::application(boot, update, view)
        .title("bytehost-desktop minimal")
        .subscription(subscription)
        .window_size(app_size())
        .run()
}
