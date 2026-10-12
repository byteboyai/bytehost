//! Surface 路由:按 `(instance, slot, generation)` 显式归属,拒绝旧代数事件。
//!
//! 归属**不用** URL / 名称前缀猜测(规格 §5.4)。宿主维护一张 `SurfaceId -> 物理槽`
//! 的表;上行事件(IPC / ready / 脚本结果)必须带 `SurfaceId`,路由层校验:
//! 1. `SurfaceId` 在表中 → 否则丢弃(未知来源,视作伪造/过期);
//! 2. 表项 generation 与事件 generation 一致 → 否则丢弃(旧实例结果)。
//!
//! 第三方应用与内置面板走**不同策略**:
//! - 内置面板:等待 `Ready` 握手,才接受 IPC;
//! - 第三方应用:导航完成即 Ready,IPC 只认白名单(由产品层再校验)。

use std::collections::HashMap;

use bytehost_core::{Generation, SurfaceId};

/// 路由表的物理槽(交给 `crate::panel` 的 handle 句柄键)。
///
/// 用 `SurfaceSlot`(core 类型)以免再定义一层;这里只是别名表达"物理槽"。
pub type PhysicalSlot = bytehost_core::SurfaceSlot;

/// 一个已注册 Surface 的路由项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub id: SurfaceId,
    pub physical: PhysicalSlot,
}

/// 路由结果:接受(交回物理槽)或拒绝(带原因,便于日志)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// 事件归属有效,交回物理槽。
    Accepted(PhysicalSlot),
    /// 未知 SurfaceId(从未注册或已注销)。
    UnknownSurface,
    /// 已知槽但 generation 过期。
    StaleGeneration {
        expected: Generation,
        got: Generation,
    },
}

impl Routed {
    pub fn slot(self) -> Option<PhysicalSlot> {
        match self {
            Routed::Accepted(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_accepted(self) -> bool {
        matches!(self, Routed::Accepted(_))
    }
}

/// Surface 路由表:显式注册/注销 + 按身份校验事件。
#[derive(Debug, Default)]
pub struct SurfaceRouter {
    /// 物理槽 → 路由项(一个物理槽同一时刻至多一个 Surface)。
    by_physical: HashMap<PhysicalSlot, Route>,
    /// 身份 → 物理槽(反查用,避免遍历)。
    by_id: HashMap<SurfaceId, PhysicalSlot>,
}

impl SurfaceRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 Surface 的物理槽。若该物理槽已被别的 Surface 占用,
    /// 先注销旧的(代表重建:旧代数的在途事件从此被拒)。
    pub fn register(&mut self, id: SurfaceId, physical: PhysicalSlot) {
        if let Some(old) = self.by_physical.remove(&physical) {
            self.by_id.remove(&old.id);
        }
        self.by_id.insert(id, physical);
        self.by_physical.insert(physical, Route { id, physical });
    }

    /// 注销一个物理槽(销毁时调用)。返回被注销的路由项(若有)。
    pub fn unregister(&mut self, physical: PhysicalSlot) -> Option<Route> {
        let route = self.by_physical.remove(&physical)?;
        self.by_id.remove(&route.id);
        Some(route)
    }

    /// 按身份校验一次事件归属。
    pub fn route(&self, id: SurfaceId) -> Routed {
        if let Some(physical) = self.by_id.get(&id) {
            return Routed::Accepted(*physical);
        }
        // 未命中身份:可能同一 (instance, slot) 已换 generation → 明确报告过期。
        for route in self.by_physical.values() {
            if route.id.instance == id.instance && route.id.slot == id.slot {
                return Routed::StaleGeneration {
                    expected: route.id.generation,
                    got: id.generation,
                };
            }
        }
        Routed::UnknownSurface
    }

    /// 当前注册数量(诊断/测试)。
    pub fn len(&self) -> usize {
        self.by_physical.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_physical.is_empty()
    }

    /// 该身份是否已注册。
    pub fn contains(&self, id: SurfaceId) -> bool {
        self.by_id.contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_core::{PanelInstanceId, SurfaceSlot};

    fn sid(instance_gen: u32, slot: u32, generation: u64) -> SurfaceId {
        SurfaceId::new(
            PanelInstanceId::new(slot, instance_gen),
            SurfaceSlot(slot),
            Generation::new(generation),
        )
    }

    #[test]
    fn registered_surface_routes_to_its_physical_slot() {
        let mut r = SurfaceRouter::new();
        let id = sid(0, 3, 0);
        r.register(id, SurfaceSlot(3));
        assert_eq!(r.route(id), Routed::Accepted(SurfaceSlot(3)));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn unknown_surface_is_rejected() {
        let r = SurfaceRouter::new();
        assert_eq!(r.route(sid(0, 1, 0)), Routed::UnknownSurface);
    }

    #[test]
    fn stale_generation_is_reported_not_accepted() {
        let mut r = SurfaceRouter::new();
        let current = sid(0, 1, 1);
        r.register(current, SurfaceSlot(1));
        // 旧代数 (generation 0) 的事件。
        let stale = sid(0, 1, 0);
        assert_eq!(
            r.route(stale),
            Routed::StaleGeneration {
                expected: Generation::new(1),
                got: Generation::new(0),
            }
        );
        assert!(!r.route(stale).is_accepted());
    }

    #[test]
    fn re_register_same_physical_slot_evicts_previous_generation() {
        let mut r = SurfaceRouter::new();
        let old = sid(0, 1, 0);
        r.register(old, SurfaceSlot(1));
        let new = sid(0, 1, 1);
        r.register(new, SurfaceSlot(1));
        assert_eq!(r.len(), 1, "同一物理槽只有一个 Surface");
        assert!(!r.contains(old), "旧代数的身份被注销");
        assert!(r.contains(new));
        // 旧身份现在报过期(因为物理槽仍存在但 generation 不同)。
        assert!(matches!(r.route(old), Routed::StaleGeneration { .. }));
    }

    #[test]
    fn unregister_removes_identity_and_slot() {
        let mut r = SurfaceRouter::new();
        let id = sid(0, 2, 0);
        r.register(id, SurfaceSlot(2));
        let removed = r.unregister(SurfaceSlot(2)).unwrap();
        assert_eq!(removed.id, id);
        assert!(r.is_empty());
        assert_eq!(r.route(id), Routed::UnknownSurface, "注销后事件一律丢弃");
    }
}
