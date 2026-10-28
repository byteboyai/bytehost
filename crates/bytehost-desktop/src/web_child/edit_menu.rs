//! 标准 Edit 菜单(Cut / Copy / Paste / Select All)的**数据**。安装(objc2)在 `apple.rs`。
//!
//! 为什么需要:wry 把子 webview 的 `performKeyEquivalent:` 覆盖成恒返回 `NO`,于是 ⌘A/⌘C/⌘V/⌘X 不会被
//! WKWebView 当作编辑命令执行——探针 S2 实测,没有 Edit 菜单时页面里这些快捷键**全都不能用**,装了之后
//! `copy`/`paste`/`cut` 事件正常触发、⌘A 能全选,而且 iced 输入框的快捷键不受影响。目标为空(走响应链),
//! first responder 是 WKWebView 时它响应;是 winit 的视图时没有人响应 `copy:`,菜单项被禁用,按键照常落到 `keyDown`。

/// 一个菜单项:标题、动作选择器(带冒号)、快捷键字符(⌘ 修饰由系统默认补上)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditItem {
    pub title: &'static str,
    pub selector: &'static str,
    pub key: &'static str,
}

pub fn standard_edit_menu_items() -> Vec<EditItem> {
    vec![
        EditItem {
            title: "Cut",
            selector: "cut:",
            key: "x",
        },
        EditItem {
            title: "Copy",
            selector: "copy:",
            key: "c",
        },
        EditItem {
            title: "Paste",
            selector: "paste:",
            key: "v",
        },
        EditItem {
            title: "Select All",
            selector: "selectAll:",
            key: "a",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_edit_menu_has_the_four_editing_commands() {
        let items = standard_edit_menu_items();
        let got: Vec<(&str, &str, &str)> =
            items.iter().map(|i| (i.title, i.selector, i.key)).collect();
        assert_eq!(
            got,
            vec![
                ("Cut", "cut:", "x"),
                ("Copy", "copy:", "c"),
                ("Paste", "paste:", "v"),
                ("Select All", "selectAll:", "a"),
            ]
        );
    }

    #[test]
    fn every_item_targets_the_responder_chain_with_a_single_lowercase_key() {
        // 目标为空(走响应链)才能让 first responder(WKWebView 或 iced 的视图)去响应;
        // 选择器必须带冒号(接收 sender),快捷键是单个小写字母(⌘ 修饰由系统默认补上)。
        for item in standard_edit_menu_items() {
            assert!(item.selector.ends_with(':'), "{}", item.selector);
            assert_eq!(item.key.chars().count(), 1);
            assert!(item.key.chars().all(|c| c.is_ascii_lowercase()));
        }
        let mut selectors: Vec<_> = standard_edit_menu_items()
            .iter()
            .map(|i| i.selector)
            .collect();
        selectors.sort();
        selectors.dedup();
        assert_eq!(selectors.len(), 4, "选择器不得重复");
    }
}
