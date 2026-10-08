//! 状态机要宿主展示的"瞬时消息"等级:宿主把它映射到自己的一套提醒组件。
//! 描述"当前处于某状态"的持久状态不进这里,留在各自的视图里。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}
