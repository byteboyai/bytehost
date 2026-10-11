//! 真实窗口入口探针:iced 循环 + wry 子视图,证明 Task 02 的桌面契约可编译并可跑。
//!
//! 模式:
//!   cargo run                 → 开真实窗口(需显示环境;webview 在 iced 之上)
//!   SELFTEST=1 cargo run      → 建窗后立刻退出(无交互,验证入口能启动)
//!
//! 关键点(与 D/spike/editor-host 同构):
//! - 唯一 iced 事件循环 `iced::application(...).run()`;
//! - 窗口 id 由 `window::oldest()` 在 boot 里拿到(不是 `Id::MAIN`);
//! - wry 子视图在**主线程**、用 `window::run(id, ..)` + `build_as_child` 创建;
//! - 非 Send 的 `WebView` 放 `thread_local`;
//! - 几何用逻辑坐标,顶部条下推由 lib 的 `host_rect` 算出。

use desktop_contract_spike::contract::{HostMessage, Product};
use desktop_contract_spike::core::{
    Binding, Insets, SurfaceContent, SurfaceSlot, SurfaceSpec, host_rect,
};

use iced::{Element, Size, Subscription, Task, window};

thread_local! {
    /// 非 Send 的 wry 句柄只在主线程持有;所有操作都经 `HOST.with(..)`。
    static HOST: std::cell::RefCell<Option<wry::WebView>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Debug, Clone)]
enum Msg {
    Ready(window::Id, Size),
    HostBuilt(Result<(), String>),
    Quit,
}

struct Probe {
    id: Option<window::Id>,
    size: Size,
    built: bool,
    note: String,
    insets: Insets,
}

fn boot() -> (Probe, Task<Msg>) {
    let probe = Probe {
        id: None,
        size: Size::new(900.0, 640.0),
        built: false,
        note: "booting".into(),
        insets: Insets { top: 44.0 },
    };
    let t = window::oldest().then(|id| match id {
        Some(id) => window::size(id).map(move |s| Msg::Ready(id, s)),
        None => Task::none(),
    });
    (probe, t)
}

fn spec() -> SurfaceSpec {
    SurfaceSpec {
        slot: SurfaceSlot(0),
        binding: Binding {
            project: "spike".into(),
            panel: "desktop-contract".into(),
            document: "probe".into(),
        },
        content: SurfaceContent::Html(
            "<html><body style='margin:0;background:#0a0e16;color:#FFE5B4;\
             font-family:sans-serif'><h3>desktop-contract probe</h3></body></html>"
                .into(),
        ),
    }
}

fn update(app: &mut Probe, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Ready(id, size) => {
            app.id = Some(id);
            app.size = size;
            let s = spec();
            let html = match s.content {
                SurfaceContent::Html(h) => h,
                _ => String::new(),
            };
            let rect = host_rect(size.width, size.height, app.insets);
            let build = window::run(id, move |w| {
                let webview = wry::WebViewBuilder::new()
                    .with_html(html)
                    .with_bounds(wry::Rect {
                        position: wry::dpi::LogicalPosition::new(rect.x, rect.y).into(),
                        size: wry::dpi::LogicalSize::new(rect.width, rect.height).into(),
                    })
                    .build_as_child(&w)
                    .map_err(|e| e.to_string())?;
                HOST.with(|h| *h.borrow_mut() = Some(webview));
                Ok(())
            })
            .map(Msg::HostBuilt);
            let exit = std::env::var("SELFTEST").is_ok().then_some(Msg::Quit);
            return match exit {
                Some(q) => Task::done(q),
                None => build,
            };
        }
        Msg::HostBuilt(Ok(())) => {
            app.built = true;
            app.note = "webview child built".into();
        }
        Msg::HostBuilt(Err(e)) => {
            app.note = format!("build failed: {e}");
        }
        Msg::Quit => {
            HOST.with(|h| drop(h.borrow_mut().take()));
            return iced::exit();
        }
    }
    Task::none()
}

fn view(app: &Probe) -> Element<'_, Msg> {
    iced::widget::column![iced::widget::text(format!(
        "{}x{} — built={} — {} (top inset {})",
        app.size.width, app.size.height, app.built, app.note, app.insets.top
    ))]
    .into()
}

fn subscription(_app: &Probe) -> Subscription<Msg> {
    use iced::event;
    event::listen_with(|evt, _status, _id| match evt {
        iced::Event::Window(window::Event::CloseRequested) => Some(Msg::Quit),
        _ => None,
    })
}

/// 复用 lib 里的泛型产品接口,证明 main 侧也能装配一个 `Product<Msg>`。
struct ProbeProduct {
    last: Option<String>,
}

impl Product<Msg> for ProbeProduct {
    fn update(&mut self, msg: HostMessage<Msg>) -> Task<Msg> {
        match msg {
            HostMessage::Surface(e) => self.last = Some(format!("{e:?}")),
            HostMessage::Product(_) => {}
        }
        Task::none()
    }
    fn view(&self) -> Element<'_, Msg> {
        iced::widget::text(self.last.clone().unwrap_or_default()).into()
    }
}

fn main() -> iced::Result {
    // 装配一次,证明 main 侧也能把产品接入同一泛型接口(值本身不参与运行)。
    let _product = ProbeProduct { last: None };
    iced::application(boot, update, view)
        .title("desktop-contract probe")
        .subscription(subscription)
        .window_size(Size::new(900.0, 640.0))
        .run()
}
