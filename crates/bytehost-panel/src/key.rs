//! 面板状态机用来标识"哪个应用"的键:dozer 用进程内槽号,Digger 可以直接用 `AppId`。
//!
//! 状态机只认这个 trait 的两种能力:从 `AppId` 造键(映射不了就丢)、把键换回应用 id 字符串
//! (给请求/展示用)。键本身必须 `Copy + Eq + Hash + Debug + Send + 'static`,好塞进 `HashMap`/`HashSet`
//! 并随状态机一起跨线程。

use std::fmt::Debug;
use std::hash::Hash;

use bytehost_apps::id::AppId;

pub trait AppKey: Copy + Eq + Hash + Debug + Send + 'static {
    /// 从应用 id 造键;无法映射(如 dozer 的槽表满)返回 `None`,该应用会被跳过。
    fn from_app_id(id: &AppId) -> Option<Self>;

    /// 键对应的应用 id 字符串(请求、展示的兜底名)。
    fn app_id(&self) -> &str;
}
