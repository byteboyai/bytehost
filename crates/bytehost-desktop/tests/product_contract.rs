//! 第二消费者契约测试:证明 desktop 壳不是专为一个产品建立的。
//!
//! 两个**不同**的产品消息类型与两个**不同**的服务上下文类型实例化同一
//! `Product<M>` / `HostMessage<M>` / `route_lifecycle`;两者都不引用 Dozer。
//! 产品自己的消息变体不进宿主 enum(业务命令回调留在产品里)。

use bytehost_core::{
    BackgroundExecutor, Emitter, Generation, PanelInstanceId, RequestKey, RequestTracker,
    ResultEnvelope, ScopeId, SurfaceSlot, run_scoped,
};
use bytehost_desktop::{HostMessage, HostSurfaceEvent, Product, route_lifecycle};

use iced::{Element, Task};
use std::sync::{Arc, Mutex};

// ---- 消费者 A:消息与服务类型都取自一个假想的"笔记"产品 ----

#[derive(Clone, Debug, PartialEq)]
enum NotesMsg {
    NewNote(String),
    Tick,
}

#[derive(Clone, Default)]
struct NotesServices {
    prefix: String,
}

#[derive(Default)]
struct NotesProduct {
    notes: Vec<String>,
    activations: usize,
}

impl Product<NotesMsg> for NotesProduct {
    fn update(&mut self, msg: HostMessage<NotesMsg>) -> Task<NotesMsg> {
        if let HostMessage::Product(NotesMsg::NewNote(n)) = msg {
            self.notes.push(n);
        }
        Task::none()
    }
    fn view(&self) -> Element<'_, NotesMsg> {
        iced::widget::text(self.notes.len()).into()
    }
    fn on_activated(&mut self, _slot: SurfaceSlot) {
        self.activations += 1;
    }
}

// ---- 消费者 B:消息与服务类型都取自一个假想的"地图"产品 ----

#[derive(Clone, Debug, PartialEq)]
enum MapMsg {
    PanBy { dx: i32, dy: i32 },
}

#[derive(Clone, Default)]
struct MapServices {
    tile_root: String,
}

#[derive(Default)]
struct MapProduct {
    pans: u32,
}

impl Product<MapMsg> for MapProduct {
    fn update(&mut self, msg: HostMessage<MapMsg>) -> Task<MapMsg> {
        if let HostMessage::Product(MapMsg::PanBy { .. }) = msg {
            self.pans += 1;
        }
        Task::none()
    }
    fn view(&self) -> Element<'_, MapMsg> {
        iced::widget::text(self.pans).into()
    }
}

#[test]
fn two_products_instance_same_host_interface() {
    let mut notes = NotesProduct::default();
    let mut map = MapProduct::default();

    let evt = HostSurfaceEvent::Created {
        slot: SurfaceSlot(0),
    };
    let _: Task<NotesMsg> = route_lifecycle(&mut notes, evt.clone());
    let _: Task<MapMsg> = route_lifecycle(&mut map, evt);

    let _: Task<NotesMsg> = Product::update(
        &mut notes,
        HostMessage::Product(NotesMsg::NewNote("a".into())),
    );
    let _: Task<MapMsg> = Product::update(
        &mut map,
        HostMessage::Product(MapMsg::PanBy { dx: 1, dy: 2 }),
    );

    assert_eq!(notes.notes, vec!["a".to_string()]);
    assert_eq!(notes.activations, 1);
    assert_eq!(map.pans, 1);
}

#[test]
fn two_service_contexts_run_through_same_executor_abstraction() {
    let tracker = RequestTracker::new(
        1,
        ScopeId::new("scope-a").unwrap(),
        PanelInstanceId::new(0, 0),
        Generation::new(0),
    );
    let exec = InlineExecutor;

    // 消费者 A:笔记服务。
    let notes_out: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let notes_emitter = collector(notes_out.clone());
    let token = tracker.begin(RequestKey::new("notes"));
    run_scoped(
        &tracker,
        token,
        NotesServices {
            prefix: "n-".into(),
        },
        &exec,
        &notes_emitter,
        |s| format!("{}note", s.prefix),
    );
    assert_eq!(&*notes_out.lock().unwrap(), &["n-note".to_string()]);

    // 消费者 B:地图服务(不同上下文类型,同一执行抽象)。
    let map_out: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let map_emitter = collector(map_out.clone());
    let token = tracker.begin(RequestKey::new("map"));
    run_scoped(
        &tracker,
        token,
        MapServices {
            tile_root: "/tiles".into(),
        },
        &exec,
        &map_emitter,
        |s| format!("{}/0/0.png", s.tile_root),
    );
    assert_eq!(&*map_out.lock().unwrap(), &["/tiles/0/0.png".to_string()]);
}

#[test]
fn business_message_variants_do_not_enter_host_enum() {
    // 宿主 enum 只有两类:Product(M) 与 Surface(事件)。
    // 业务命令(NewNote/PanBy)只能经 Product 包装进入,证明未混入宿主。
    let m: HostMessage<NotesMsg> = HostMessage::Product(NotesMsg::Tick);
    assert!(m.clone().into_product().is_some());
    assert!(m.surface().is_none());
}

// ---- 测试用执行器/发射器(不依赖任何产品) ----

struct InlineExecutor;
impl BackgroundExecutor for InlineExecutor {
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        task();
    }
}

fn collector(out: Arc<Mutex<Vec<String>>>) -> impl Emitter<ResultEnvelope<String>> + Clone {
    #[derive(Clone)]
    struct C(Arc<Mutex<Vec<String>>>);
    impl Emitter<ResultEnvelope<String>> for C {
        fn emit(&self, message: ResultEnvelope<String>) {
            self.0.lock().unwrap().push(message.payload);
        }
    }
    C(out)
}
