//! Task 02 探针:验证 host 层的三件事,均为**编译期/运行期证据**,不是文档声称。
//!
//! 1. 纯描述层(`core`)不含 iced/wry,只放 Surface 描述与布局 DTO。
//! 2. `Product<M>` / `HostMessage<M>` 对**任意**产品消息类型成立:
//!    两个不同的产品消息类型(模拟 Dozer 与 Digger)实例化同一接口。
//! 3. 同一 `SurfaceSpec`/`SurfacePayload` 能表达两类不同推送形状:
//!    Usage(时序聚合)与 CodeHealth(诊断列表)。
//!
//! 真实窗口入口(iced 循环 + wry `build_as_child` + thread_local 非 Send 句柄)
//! 在 `main.rs`;本文件只做接口契约与纯逻辑,`cargo test` 不需要显示环境。

use serde::{Deserialize, Serialize};

/// 纯描述层:与产品、界面框架、平台 webview 库无关。
/// (生产对应 `bytehost-core`,仅依赖 serde。)
pub mod core {
    use super::{Deserialize, Serialize};

    /// 一个 Surface 在窗口里的稳定槽位。webview 实例与之 1:1,重启面板不换槽。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SurfaceSlot(pub u32);

    /// 面板/文档绑定:URL/归属校验用,三个维度与 Dozer 的 `proj/panel/tab/doc` 同形。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Binding {
        pub project: String,
        pub panel: String,
        pub document: String,
    }

    /// Surface 的内容来源。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum SurfaceContent {
        /// 直接给一段 HTML(隔离 host)。
        Html(String),
        /// 给一个已授权的 URL。
        Url(String),
        /// 占位(尚未加载)。
        Empty,
    }

    /// 数据推送的形状变体(不是业务模型)。core 只认形状,不为任一业务加字段。
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub enum SurfacePayload {
        /// 行列表:每行若干命名列。Usage 与 CodeHealth 都归这个形状。
        Rows {
            columns: Vec<String>,
            rows: Vec<Vec<String>>,
        },
        /// 单值汇总(如总 token/总成本)。
        Summary { label: String, value: String },
    }

    /// Surface 描述:core 唯一知道的东西。不含业务字段。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SurfaceSpec {
        pub slot: SurfaceSlot,
        pub binding: Binding,
        pub content: SurfaceContent,
    }

    /// 顶层 iced 条(顶栏/恢复条/冲突条)的高度汇总。webview 恒在 iced 之上,
    /// 任何盖在它上面的条都必须把它**下推**这个值。
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Insets {
        pub top: f32,
    }

    /// 逻辑坐标矩形(DPI 由 wry 处理,调用方只给逻辑值)。
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct LogicalRect {
        pub x: f64,
        pub y: f64,
        pub width: f64,
        pub height: f64,
    }

    /// 窗口逻辑尺寸 + 顶部条高 → webview 子视图矩形。
    /// 纯函数,便于单测;生产里 `sync_bounds` 用它。
    pub fn host_rect(win_w: f32, win_h: f32, insets: Insets) -> LogicalRect {
        LogicalRect {
            x: 0.0,
            y: insets.top as f64,
            width: win_w as f64,
            height: (win_h - insets.top).max(0.0) as f64,
        }
    }
}

/// 泛型契约层:只描述形状,不绑定具体产品消息类型。
pub mod contract {
    use super::core::{SurfacePayload, SurfaceSlot};
    use iced::Element;
    use iced::Task;

    /// 宿主发给产品/从产品收到的通用事件。
    #[derive(Clone, Debug, PartialEq)]
    pub enum SurfaceEvent {
        /// 窗口尺寸变化,需重算 bounds。
        Resized {
            slot: SurfaceSlot,
            width: f32,
            height: f32,
        },
        /// 数据到达(从 webview 或后台)。形状由 payload 表达。
        Data {
            slot: SurfaceSlot,
            payload: SurfacePayload,
        },
        /// webview 被隐藏(切换面板)。
        Hidden { slot: SurfaceSlot },
        /// webview 恢复显示。
        Resumed { slot: SurfaceSlot },
        /// webview 销毁/面板关闭。
        Destroyed { slot: SurfaceSlot },
        /// webview 上行 IPC 原始串(已由宿主校验归属)。
        Ipc { slot: SurfaceSlot, body: String },
    }

    /// 同一 Host 抽象对**任意**产品消息 `M` 成立。
    #[derive(Clone, Debug, PartialEq)]
    pub enum HostMessage<M> {
        /// 产品自己的消息。
        Product(M),
        /// 宿主通用事件。
        Surface(SurfaceEvent),
    }

    impl<M> HostMessage<M> {
        /// 宿主层只关心 Surface 事件;产品的交回产品处理。
        pub fn surface(&self) -> Option<&SurfaceEvent> {
            match self {
                HostMessage::Surface(e) => Some(e),
                HostMessage::Product(_) => None,
            }
        }
    }

    /// 产品侧:一个 iced 应用(update/view + 把产品消息包装给宿主)。
    /// 注意 `view` 借用 `self`(`Element<'_, M>`),与 iced 0.14 一致;
    /// 不在 view 内建 wry,也不持非 Send 句柄。
    pub trait Product<M: Clone + 'static> {
        fn update(&mut self, msg: HostMessage<M>) -> Task<M>;
        fn view(&self) -> Element<'_, M>;
    }

    /// 宿主通用事件 → 让产品实现者决定是否转发。
    pub fn route<M: Clone + 'static, P: Product<M>>(p: &mut P, msg: HostMessage<M>) -> Task<M> {
        p.update(msg)
    }
}

/// 产品消息类型 A:模拟 Dozer 的顶层消息(只取两个变体即可证明泛型成立)。
#[derive(Clone, Debug, PartialEq)]
pub enum DozerMsg {
    OpenPanel(core::SurfaceSlot),
    Tick,
}

/// 产品消息类型 B:模拟 Digger 的产品消息(与 A 形状不同)。
#[derive(Clone, Debug, PartialEq)]
pub enum DiggerMsg {
    LoadDocument(String),
    Surface(contract::SurfaceEvent),
}

/// 一个最小产品实现 A,证明 `Product<DozerMsg>` 可编译。
#[derive(Default)]
pub struct DozerProduct {
    pub last: Option<String>,
}

impl contract::Product<DozerMsg> for DozerProduct {
    fn update(&mut self, msg: contract::HostMessage<DozerMsg>) -> iced::Task<DozerMsg> {
        match msg {
            contract::HostMessage::Product(DozerMsg::Tick) => self.last = Some("tick".into()),
            contract::HostMessage::Product(DozerMsg::OpenPanel(_)) => {}
            contract::HostMessage::Surface(contract::SurfaceEvent::Resized {
                width,
                height,
                ..
            }) => {
                self.last = Some(format!("resize {width}x{height}"));
            }
            contract::HostMessage::Surface(_) => {}
        }
        iced::Task::none()
    }

    fn view(&self) -> iced::Element<'_, DozerMsg> {
        iced::widget::text(self.last.clone().unwrap_or_default()).into()
    }
}

/// 一个最小产品实现 B,证明 `Product<DiggerMsg>` 可编译(同一接口)。
#[derive(Default)]
pub struct DiggerProduct {
    pub doc: Option<String>,
}

impl contract::Product<DiggerMsg> for DiggerProduct {
    fn update(&mut self, msg: contract::HostMessage<DiggerMsg>) -> iced::Task<DiggerMsg> {
        match msg {
            contract::HostMessage::Product(DiggerMsg::LoadDocument(d)) => self.doc = Some(d),
            contract::HostMessage::Product(DiggerMsg::Surface(_)) => {}
            contract::HostMessage::Surface(contract::SurfaceEvent::Data { .. }) => {}
            contract::HostMessage::Surface(_) => {}
        }
        iced::Task::none()
    }

    fn view(&self) -> iced::Element<'_, DiggerMsg> {
        iced::widget::text(self.doc.clone().unwrap_or_default()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::contract::{HostMessage, Product, SurfaceEvent};
    use super::core::SurfaceSpec;
    use super::core::{Binding, Insets, LogicalRect, SurfaceContent, SurfaceSlot, host_rect};
    use super::{DiggerMsg, DiggerProduct, DozerMsg, DozerProduct, core::SurfacePayload};

    /// 证据 1:两个不同产品消息类型实例化**同一** `Product<M>` / `HostMessage<M>`。
    #[test]
    fn two_products_share_one_host_interface() {
        let mut a = DozerProduct::default();
        let mut b = DiggerProduct::default();
        let evt = SurfaceEvent::Resized {
            slot: SurfaceSlot(1),
            width: 800.0,
            height: 600.0,
        };
        let _: iced::Task<DozerMsg> = a.update(HostMessage::Surface(evt.clone()));
        let _: iced::Task<DiggerMsg> = b.update(HostMessage::Surface(evt));
        assert_eq!(a.last.as_deref(), Some("resize 800x600"));
        let _: iced::Task<DozerMsg> = a.update(HostMessage::Product(DozerMsg::Tick));
        let _: iced::Task<DiggerMsg> =
            b.update(HostMessage::Product(DiggerMsg::LoadDocument("x".into())));
        assert_eq!(a.last.as_deref(), Some("tick"));
        assert_eq!(b.doc.as_deref(), Some("x"));
    }

    /// 证据 2:同一 Surface 描述 + 两类 payload 表达 Usage 与 CodeHealth 两种推送形状。
    #[test]
    fn one_surface_describes_usage_and_codehealth() {
        let spec = SurfaceSpec {
            slot: SurfaceSlot(7),
            binding: Binding {
                project: "p".into(),
                panel: "usage".into(),
                document: "usage.html".into(),
            },
            content: SurfaceContent::Html("<div id=app></div>".into()),
        };

        let usage = SurfacePayload::Rows {
            columns: vec!["day".into(), "tokens".into(), "cost".into()],
            rows: vec![vec!["2026-10-11".into(), "12000".into(), "0.34".into()]],
        };
        let usage_total = SurfacePayload::Summary {
            label: "total".into(),
            value: "$0.34".into(),
        };
        let health = SurfacePayload::Rows {
            columns: vec!["file".into(), "severity".into(), "message".into()],
            rows: vec![vec!["a.rs".into(), "warn".into(), "unused".into()]],
        };

        for payload in [usage, usage_total, health] {
            let evt = SurfaceEvent::Data {
                slot: spec.slot,
                payload,
            };
            assert!(matches!(evt, SurfaceEvent::Data { .. }));
        }
    }

    /// 证据 3:几何纯函数(webview 下推顶部条)。
    #[test]
    fn host_rect_pushes_down_top_insets() {
        let r = host_rect(1000.0, 700.0, Insets { top: 48.0 });
        assert_eq!(
            r,
            LogicalRect {
                x: 0.0,
                y: 48.0,
                width: 1000.0,
                height: 652.0
            }
        );
        let r = host_rect(100.0, 30.0, Insets { top: 48.0 });
        assert_eq!(r.height, 0.0);
    }
}
