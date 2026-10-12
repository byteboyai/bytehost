//! 产品中立的"可悬停元素"槽位词汇:面板内某个按钮/页签/行的**种类**,
//! 与具体面板无关(规格 E2:宿主公开类型里不出现业务类型)。
//!
//! 产品把"面板身份 + 槽位 + 可选键值"组合成自己的悬停键(如 Dozer 的
//! `HoverId::Panel(PanelKind, HoverSlot)`);宿主只提供这套通用词汇与
//! "具名兜底"(`Named`),新槽位优先用 `Named`,不再往这里加业务专属变体
//! ——变体表只许收敛(兼容桥 C9)。

/// 面板内一个可悬停元素的槽位。词汇通用,不含任何面板名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HoverSlot {
    /// 列表列折叠按钮。
    ListCollapse,
    /// 搜索框提交按钮。
    SearchSubmit,
    /// "更多…"翻页按钮。
    More,
    /// 页签标题(下标或稳定 id)。
    TabItem(u64),
    /// 页签关闭按钮。
    TabClose(u64),
    /// 页签溢出菜单按钮。
    TabOverflow,
    /// 列表行/卡片。
    Row(u64),
    /// 一组互斥选项里的一项。
    Choice(u64),
    /// 面板内一次性的具名按钮(同一面板内唯一)。
    Named(&'static str),
}
