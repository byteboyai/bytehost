//! 几何 DTO:窗口/子视图的逻辑坐标矩形与顶部条下推计算。
//!
//! 纯函数、纯数据,不依赖任何界面框架(Task 02 §3:几何逻辑进 core 组)。
//! DPI 缩放由 wry/winit 处理,调用方只给逻辑值。
//!
//! "webview 恒在 iced 之上":任何盖在子视图上的 iced 条(顶栏、恢复条、冲突条)
//! 都必须把子视图**下推**其高度;`Insets.top` 汇总这些条高,`host_rect` 算出矩形。

/// 顶层 iced 条的高度汇总(逻辑点)。webview 子视图据此下推。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub top: f32,
}

/// 逻辑坐标矩形(x/y 为左上角,宽高为逻辑点)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl LogicalRect {
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };

    /// 是否为零面积(用于"隐藏子视图"——把矩形收到零)。
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// 窗口逻辑尺寸 + 顶部条高 → 子视图矩形。纯函数,便于单测。
///
/// 高度不足时钳到 0(不下推到负高)。
pub fn host_rect(win_w: f32, win_h: f32, insets: Insets) -> LogicalRect {
    LogicalRect {
        x: 0.0,
        y: insets.top as f64,
        width: win_w as f64,
        height: (win_h - insets.top).max(0.0) as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn host_rect_clamps_height_at_zero() {
        let r = host_rect(100.0, 30.0, Insets { top: 48.0 });
        assert_eq!(r.height, 0.0);
        assert!(r.is_empty());
    }

    #[test]
    fn zero_rect_is_empty() {
        assert!(LogicalRect::ZERO.is_empty());
    }
}
