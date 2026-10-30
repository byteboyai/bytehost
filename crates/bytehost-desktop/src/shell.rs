//! 窗口壳:一个 iced 应用把**产品**接进宿主,产品持有自己的业务状态。
//!
//! 设计约束(Task 02 已编译签名 + 计划 Task 07):
//! - 产品消息类型 `M` 是泛型的;宿主只定义 `HostMessage<M>` 的包装与通用事件,
//!   产品自己的消息变体**不进入**宿主 enum。
//! - `Product` trait 由产品实现:`update` / `view` / 生命周期回调(激活/关闭)。
//! - 窗口生命周期与 wry 子视图创建走同一 iced 循环(见 `crate::panel`)。

use iced::{Element, Task};

use bytehost_core::SurfaceSlot;

/// 宿主通用事件:与业务无关的窗口/子视图事件。
///
/// 名字带 `Host` 是为了和 `bytehost_core::SurfaceEvent`(Surface 状态机的输入事件:
/// `Created`/`Ready`/`Hidden`…)区分——两者都在 crate 根导出,同名会让 glob import 歧义。
///
/// 这些是宿主唯一"认识"的事件形状;业务命令回调(打开项目、刷新列表等)
/// 由产品在自己的消息类型 `M` 里表达,不放进本 enum。
#[derive(Debug, Clone, PartialEq)]
pub enum HostSurfaceEvent {
    /// 窗口或子视图尺寸变化,需重算 bounds。
    Resized {
        slot: SurfaceSlot,
        width: f32,
        height: f32,
    },
    /// 子视图被隐藏(切换面板/标签)。
    Hidden { slot: SurfaceSlot },
    /// 子视图恢复显示。
    Resumed { slot: SurfaceSlot },
    /// 子视图销毁(面板关闭)。
    Destroyed { slot: SurfaceSlot },
    /// 子视图上行 IPC 原始串(归属已由宿主按 slot 校验)。
    Ipc { slot: SurfaceSlot, body: String },
    /// 子视图创建成功(拿到原生句柄)。
    Created { slot: SurfaceSlot },
    /// 子视图创建失败;上层应转为面板可见错误,不 panic。
    CreateFailed { slot: SurfaceSlot, error: String },
}

impl HostSurfaceEvent {
    pub fn slot(&self) -> SurfaceSlot {
        match self {
            HostSurfaceEvent::Resized { slot, .. }
            | HostSurfaceEvent::Hidden { slot }
            | HostSurfaceEvent::Resumed { slot }
            | HostSurfaceEvent::Destroyed { slot }
            | HostSurfaceEvent::Ipc { slot, .. }
            | HostSurfaceEvent::Created { slot }
            | HostSurfaceEvent::CreateFailed { slot, .. } => *slot,
        }
    }
}

/// 宿主发给产品的消息:要么是产品自己的消息,要么是宿主通用事件。
#[derive(Debug, Clone, PartialEq)]
pub enum HostMessage<M> {
    /// 产品自己的消息。
    Product(M),
    /// 宿主通用事件。
    Surface(HostSurfaceEvent),
}

impl<M> HostMessage<M> {
    /// 宿主层只关心 Surface 事件;产品消息交回产品。
    pub fn surface(&self) -> Option<&HostSurfaceEvent> {
        match self {
            HostMessage::Surface(e) => Some(e),
            HostMessage::Product(_) => None,
        }
    }

    pub fn into_product(self) -> Option<M> {
        match self {
            HostMessage::Product(m) => Some(m),
            HostMessage::Surface(_) => None,
        }
    }
}

/// 产品接入宿主:实现这个 trait 即可被 `crate::shell` 驱动。
///
/// 注意 `view` 借用 `self`(`Element<'_, M>`),与 iced 0.14 一致;
/// 不在 `view` 内创建 wry,也不持非 `Send` 句柄。
pub trait Product<M: Clone + 'static> {
    /// 处理一条消息(产品消息或宿主事件),返回要执行的 iced `Task`。
    fn update(&mut self, msg: HostMessage<M>) -> Task<M>;

    /// 渲染窗口内容。
    fn view(&self) -> Element<'_, M>;

    /// 面板/子视图被激活(创建或恢复)。默认无操作。
    fn on_activated(&mut self, _slot: SurfaceSlot) {}

    /// 面板/子视图被关闭(销毁)。默认无操作。
    fn on_closed(&mut self, _slot: SurfaceSlot) {}
}

/// 一个产品的窗口标题(默认实现返回固定标题;产品可覆盖)。
pub trait ProductTitle {
    fn title(&self) -> String;
}

/// 把宿主通用事件路由到产品的生命周期回调,其余交给 `update`。
/// 产品不需要自己复制这段 match。
pub fn route_lifecycle<M, P>(product: &mut P, event: HostSurfaceEvent) -> Task<M>
where
    M: Clone + 'static,
    P: Product<M>,
{
    match &event {
        HostSurfaceEvent::Created { slot } | HostSurfaceEvent::Resumed { slot } => {
            product.on_activated(*slot)
        }
        HostSurfaceEvent::Destroyed { slot } => product.on_closed(*slot),
        _ => {}
    }
    product.update(HostMessage::Surface(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::{Task, widget};

    #[derive(Clone, Debug, PartialEq)]
    enum Msg {
        Tick,
    }

    #[derive(Default)]
    struct Fake {
        ticks: u32,
        activated: Vec<SurfaceSlot>,
        closed: Vec<SurfaceSlot>,
    }

    impl Product<Msg> for Fake {
        fn update(&mut self, msg: HostMessage<Msg>) -> Task<Msg> {
            if let HostMessage::Product(Msg::Tick) = msg {
                self.ticks += 1;
            }
            Task::none()
        }
        fn view(&self) -> Element<'_, Msg> {
            widget::text(self.ticks).into()
        }
        fn on_activated(&mut self, slot: SurfaceSlot) {
            self.activated.push(slot);
        }
        fn on_closed(&mut self, slot: SurfaceSlot) {
            self.closed.push(slot);
        }
    }

    #[test]
    fn product_message_reaches_update() {
        let mut p = Fake::default();
        let _: Task<Msg> = p.update(HostMessage::Product(Msg::Tick));
        assert_eq!(p.ticks, 1);
    }

    #[test]
    fn lifecycle_routes_to_callbacks_then_update() {
        let mut p = Fake::default();
        let _ = route_lifecycle(
            &mut p,
            HostSurfaceEvent::Created {
                slot: SurfaceSlot(3),
            },
        );
        let _ = route_lifecycle(
            &mut p,
            HostSurfaceEvent::Destroyed {
                slot: SurfaceSlot(3),
            },
        );
        assert_eq!(p.activated, vec![SurfaceSlot(3)]);
        assert_eq!(p.closed, vec![SurfaceSlot(3)]);
    }

    #[test]
    fn surface_and_product_are_distinguishable() {
        let m: HostMessage<Msg> = HostMessage::Surface(HostSurfaceEvent::Hidden {
            slot: SurfaceSlot(1),
        });
        assert!(m.surface().is_some());
        assert!(m.into_product().is_none());
        let m: HostMessage<Msg> = HostMessage::Product(Msg::Tick);
        assert!(m.surface().is_none());
        assert!(m.into_product().is_some());
    }
}
