//! 旧布局迁移:把**产品历史的**布局 JSON 翻译成 `bytehost-core::Layout` 的
//! 稳定 id 形态(规格 §5.2;Task 10)。
//!
//! 分工(计划 clause 157):产品负责"枚举值 → 稳定 `PanelId` 的映射"与默认布局;
//! 宿主负责与产品无关的迁移流程——识别**更高的 schema 版本并拒绝覆盖**、
//! 把未知/已下线 id 保留进 `retained_unknown`(不丢、不阻塞开窗)、尺寸钳制、
//! 以及**原子落盘 + 迁移前备份**。
//!
//! 产品通过实现 [`LegacyPanelAdapter`] 提供两件事:
//! 1. `rails_from_legacy`:从历史 JSON(对象)切出各栏的**原始 token 序列**
//!    (如 Dozer 的 `rail_layout.left/right`,元素是 `"Files"` / `"app:<id>"`);
//! 2. `map_token`:把一个历史 token 映射到稳定 `PanelId`(`None` = 已下线,
//!    保留进 `retained_unknown`)。
//!
//! 宿主不解释任何产品 token——不认识就交给产品的 `map_token` 决定。

use std::path::Path;

use bytehost_core::registry::PanelCatalog;
use bytehost_core::{LAYOUT_SCHEMA_VERSION, Layout, LayoutError, PanelId, Rail};

/// 产品侧的历史布局适配器。实现者只做"切栏 + 映射 token",其余流程归宿主。
pub trait LegacyPanelAdapter {
    /// 从历史布局 JSON 切出各栏的原始 token 序列(顺序即展示顺序)。
    /// 返回空 `Vec` 表示无历史栏信息(全新安装 / 文件缺字段)。
    fn rails_from_legacy(&self, legacy: &serde_json::Value) -> Vec<Vec<String>>;

    /// 把一个历史 token 映射到稳定 `PanelId`。`None` = 该 id 已从产品下线,
    /// 会被保留进 `retained_unknown` 而不是静默丢弃。
    fn map_token(&self, token: &str) -> Option<PanelId>;

    /// 每栏的初始宽度(逻辑点)。历史 JSON 不带宽度,由产品给默认值。
    fn default_rail_width(&self) -> f32;
}

/// 迁移结果:翻译出的布局 + 发生了什么(供调用方决定是否落盘 / 提示用户)。
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationOutcome {
    pub layout: Layout,
    /// 连稳定 `PanelId` 形状都构不出来的历史 token(产品已不认识),保留供提示。
    /// 仍是合法 `PanelId`、但当前目录未注册的,走 `layout.retained_unknown`。
    pub retained: Vec<String>,
    /// 历史 JSON 里的 schema 版本(缺省视为 [`LAYOUT_SCHEMA_VERSION`])。
    pub found_schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    /// 历史文件声明的 schema 版本高于本宿主支持 → **拒绝覆盖**(调用方不得
    /// 改写原文件)。这是防降级破坏的关键闸门。
    UnsupportedNewerVersion { found: u32, supported: u32 },
    /// 历史文件不是 JSON 对象(无法解释)。
    NotAnObject,
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedNewerVersion { found, supported } => write!(
                f,
                "历史布局 schema 版本 {found} 高于宿主支持的 {supported},拒绝覆盖"
            ),
            Self::NotAnObject => write!(f, "历史布局不是 JSON 对象,无法迁移"),
        }
    }
}

impl std::error::Error for MigrateError {}

/// 从历史布局 JSON 迁移到 [`Layout`]。
///
/// - 版本闸门:`schema_version` 高于支持 → [`MigrateError::UnsupportedNewerVersion`]。
/// - 栏:交产品 `rails_from_legacy` 切出;token 经 `map_token` 翻译。
/// - 未知 token 保留进 `retained_unknown`(不丢弃)。
/// - 宽度用产品的 `default_rail_width`(历史 JSON 不含宽度)。
pub fn migrate_legacy_layout<A: LegacyPanelAdapter>(
    legacy: &serde_json::Value,
    adapter: &A,
) -> Result<MigrationOutcome, MigrateError> {
    let obj = legacy.as_object().ok_or(MigrateError::NotAnObject)?;
    let found_schema_version = obj
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(LAYOUT_SCHEMA_VERSION);
    if found_schema_version > LAYOUT_SCHEMA_VERSION {
        return Err(MigrateError::UnsupportedNewerVersion {
            found: found_schema_version,
            supported: LAYOUT_SCHEMA_VERSION,
        });
    }

    let width = adapter.default_rail_width();
    let mut retained: Vec<String> = Vec::new();
    let mut rails: Vec<Rail> = Vec::new();
    for raw in adapter.rails_from_legacy(legacy) {
        let mut items = Vec::with_capacity(raw.len());
        for token in raw {
            match adapter.map_token(&token) {
                Some(id) => items.push(id),
                None => {
                    // 连合法 `PanelId` 形状都不是 → 产品已彻底不认识,保留供提示。
                    if !retained.contains(&token) {
                        retained.push(token);
                    }
                }
            }
        }
        rails.push(Rail::new(items, width));
    }

    // 仍是合法 `PanelId`、但当前目录未注册的,交给 core 的 `load` 摘进
    // `retained_unknown`(这里 `catalog` 由调用方在真实读盘时提供;迁移本身
    // 只产出 id 序列,不做目录过滤——由 `reconcile_with_catalog` 完成)。
    let layout = Layout {
        schema_version: LAYOUT_SCHEMA_VERSION,
        rails,
        retained_unknown: Vec::new(),
    };
    Ok(MigrationOutcome {
        layout,
        retained,
        found_schema_version,
    })
}

/// 从磁盘读历史 JSON 并迁移。文件缺失/损坏 → 交给调用方给默认布局
/// (这里返回 `Ok(None)` 表示"没有可迁移的历史")。
pub fn migrate_legacy_file<A: LegacyPanelAdapter>(
    path: &Path,
    adapter: &A,
) -> Result<Option<MigrationOutcome>, MigrateError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        // 损坏的历史文件:当作没有可迁移内容(不覆盖,交由产品决定)。
        return Ok(None);
    };
    migrate_legacy_layout(&value, adapter).map(Some)
}

/// 原子落盘:写临时文件再 `rename`,避免半截写坏布局。迁移前若已有旧文件,
/// 先备份一份 `<path>.bak`(只备份一次,已存在则不覆盖)。
pub fn atomic_save_layout(path: &Path, layout: &Layout) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let backup = path.with_extension("json.bak");
    if path.exists() && !backup.exists() {
        std::fs::copy(path, &backup)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, layout.to_json())?;
    std::fs::rename(&tmp, path)
}

/// 仅供迁移与测试:把一个没有产品目录约束的 `Layout` 与目录对齐
/// (未知 id 摘进 `retained_unknown`)。产品实际读盘走 `Layout::load`。
pub fn reconcile_with_catalog(
    layout: Layout,
    catalog: &PanelCatalog,
) -> Result<Layout, LayoutError> {
    layout_with_catalog(layout, catalog)
}

fn layout_with_catalog(layout: Layout, catalog: &PanelCatalog) -> Result<Layout, LayoutError> {
    // 复用 core 的 `load` 语义:序列化再 `load` 会做钳制 + 未知摘取 + 版本闸门。
    Layout::load(&layout.to_json(), catalog)
}
