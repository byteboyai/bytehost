//! 纯 i18n 逻辑:语言标签子集校验/规范化、locale 协商、标题解析。
//!
//! 不依赖文件 I/O 或任何产品 crate(默认 feature 闭包只有 serde/serde_json)。
//! 资源装载(T2)把字符串注入 [`I18nBundle`],本模块只做纯计算。
//!
//! 设计见 `docs/superpowers/specs/2026-10-14-bytehost-app-i18n.md` §3。

use serde::{Deserialize, Serialize};

mod locale;
mod title;

#[cfg(feature = "server")]
pub mod resources;

pub use locale::{LocaleError, negotiate_locale, normalize_locale};
pub use title::{I18nBundle, TitleRef};

/// manifest `[i18n]` 字段。声明时 `default_locale` 与 `supported_locales` 必填。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I18n {
    /// 必须被 `supported_locales` 包含。
    pub default_locale: String,
    /// 非空、规范化后唯一、且包含 default。
    pub supported_locales: Vec<String>,
    /// 包内相对目录,默认 `locales`(资源装载器解析)。
    #[serde(default)]
    pub strings_dir: Option<String>,
    /// 语言桥能力,默认 `none`。
    #[serde(default)]
    pub webview_locale: WebViewLocale,
}

/// 应用 WebView 是否接收语言变化。对应 manifest `[i18n].webview_locale`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebViewLocale {
    /// 不安装语言桥。
    #[default]
    None,
    /// 仅新文档初始化时提供语言;不接收同文档内变化。
    Initial,
    /// 完整通知(初始化 + 后续变化)。
    Live,
}
