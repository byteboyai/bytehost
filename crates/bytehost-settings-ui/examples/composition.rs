//! 最小消费示例:同一共享框架注册主题页、产品自定义页,并在主题页尾部插入附加区块。
//!
//! 运行 `cargo run --example composition -p bytehost-settings-ui` 只构造元素并打印注册结果,
//! 不需要窗口 / GPU——用于冻结公共签名与验证两种扩展方式。

use bytehost_settings::{PageDescriptor, PageId, PageRegistry, SelectionState, SettingsState};
use bytehost_settings_ui::language::LanguageOption;
use bytehost_settings_ui::text::{Text, identity_resolver};
use bytehost_settings_ui::{AppsView, Shell, theme};
use byteui::theme::color::ColorScheme;
use iced::Element;

#[derive(Clone, Debug)]
#[allow(dead_code)] // 示例只构造消息,不执行 update。
enum Message {
    Close,
    PageSelected(PageId),
    ThemeSelected(ColorScheme),
    LanguageSelected(&'static str),
    Apps(bytehost_panel::install::Message),
    EditorToggled(bool),
}

fn custom_page(raw: &str, fallback: &str) -> PageDescriptor {
    PageDescriptor::custom(PageId::new(raw).unwrap(), format!("page.{raw}"), fallback)
}

fn main() {
    // 1) 产品注册:共享主题页 + 产品自己的 `digger.editor` 页。
    let registry = PageRegistry::new(vec![
        PageDescriptor::built_in(PageId::new(bytehost_settings::THEME_ID).unwrap(), "Theme"),
        custom_page("digger.editor", "Editor"),
    ])
    .expect("valid registry");

    let mut settings = SettingsState::new();
    settings.open(&registry, Some(&PageId::new("digger.editor").unwrap()));
    assert_eq!(settings.current().unwrap().as_str(), "digger.editor");

    // 2) 未知目标回退到主题页。
    settings.open(&registry, Some(&PageId::new("missing.page").unwrap()));
    assert_eq!(
        settings.current().unwrap().as_str(),
        bytehost_settings::THEME_ID
    );

    // 3) 语言选择模型:保存中忽略重复提交。
    let mut language = SelectionState::new("en");
    assert!(language.select("zh"));
    assert!(!language.select("fr"));

    let resolve = identity_resolver();
    let text = Text::new(resolve);
    let select_page = |id: &PageId| Message::PageSelected(id.clone());
    let select_theme = |scheme: ColorScheme| Message::ThemeSelected(scheme);
    let select_language = |value: &'static str| Message::LanguageSelected(value);
    let map_apps = Message::Apps;
    let theme_extra: Vec<Element<'_, Message>> =
        vec![iced::widget::text("Product-specific note under the shared theme options").into()];
    let _theme_page: Element<'_, Message> = theme::view(
        ColorScheme::Dark,
        &theme::ALL,
        text,
        &select_theme,
        theme_extra,
    );

    let options = [
        LanguageOption::new("system", "settings.language.follow_system", "Follow System"),
        LanguageOption::new("en", "settings.language.english", "English"),
    ];
    let _language_page: Element<'_, Message> = bytehost_settings_ui::language_page(
        &language,
        &options,
        text,
        &select_language,
        Vec::new(),
    );

    // 4) 自定义页直接作为容器内容;共享容器只负责导航 / 标题 / 关闭。
    let editor_page: Element<'_, Message> = iced::widget::checkbox(true)
        .label("Enable line numbers (product-owned state)")
        .on_toggle(Message::EditorToggled)
        .into();
    let _shell: Element<'_, Message> = bytehost_settings_ui::settings_shell(Shell {
        pages: registry.pages(),
        current: settings.current(),
        content: editor_page,
        on_select: &select_page,
        on_close: Message::Close,
        on_close_hover: None,
        close_hovered: false,
        text,
        scroll_content: true,
        width: iced::Length::Fill,
    });

    // 5) 应用页只借用共享安装状态,消息映射到产品自己的路由。
    let apps = bytehost_panel::install::InstallFlowState::default();
    let _apps_page: Element<'_, Message> =
        bytehost_settings_ui::apps::view(AppsView::new(&apps, text, &map_apps, &|id| {
            iced::widget::Id::from(format!("example.logs.{id}"))
        }));

    println!(
        "registered {} pages; current = {}",
        registry.len(),
        settings.current().map(|p| p.as_str()).unwrap_or("<none>")
    );
}
