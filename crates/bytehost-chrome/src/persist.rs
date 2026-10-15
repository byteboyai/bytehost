//! 骨架状态的落盘与读盘消毒。
//!
//! 文件是应用级偏好 JSON,字段全部 `#[serde(default)]`:缺字段补默认,而不是整份读失败把用户攒下来的
//! 宽度 / 比例全部重置。读盘后统一消毒——磁盘上的值不一定是本程序写的(手改过、别的版本写的、写了一半)。
//! 更高 schema 版本的文件**拒绝覆盖**:内存里用默认值,调用方据 [`Loaded::Newer`] 不要回写。

use std::io;
use std::path::Path;

use bytehost_core::id::PanelId;
use bytehost_desktop::{PaneDims, PaneStore};
use serde::{Deserialize, Serialize};

use crate::geometry::Side;
use crate::metrics::ChromeMetrics;
use crate::model::ShellModel;
use crate::rail::Rails;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellFile {
    pub schema_version: u32,
    pub window_width: f32,
    pub window_height: f32,
    /// 两条图标栏,面板稳定 id 的顺序。读盘时按字符串读,逐个校验后再转成 `PanelId`。
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub selected_left: Option<String>,
    pub selected_right: Option<String>,
    pub left_collapsed: bool,
    pub right_collapsed: bool,
    pub left_width: f32,
    pub panes: PaneStore,
}

impl Default for ShellFile {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            window_width: 0.0,
            window_height: 0.0,
            left: Vec::new(),
            right: Vec::new(),
            selected_left: None,
            selected_right: None,
            left_collapsed: false,
            right_collapsed: false,
            left_width: 0.0,
            panes: PaneStore::new(),
        }
    }
}

/// 产品给的默认值(全新安装 / 文件不可用时用)。
#[derive(Debug, Clone, PartialEq)]
pub struct Defaults {
    pub rails: Rails,
    pub selected: [PanelId; 2],
    pub left_width: f32,
    pub window: (f32, f32),
    /// 窗口尺寸下限;读到更小的值视为损坏,回落默认。
    pub min_window: (f32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    /// 没有文件、解析失败或布局无法修复:用默认值,可正常回写。
    Fresh(ShellModel),
    /// 从文件恢复(可能经过消毒)。
    Restored(ShellModel),
    /// 文件的 schema 版本高于本程序支持的:内存用默认值,**不得回写**原文件。
    Newer { model: ShellModel, found: u32 },
}

impl Loaded {
    pub fn model(&self) -> &ShellModel {
        match self {
            Loaded::Fresh(m) | Loaded::Restored(m) | Loaded::Newer { model: m, .. } => m,
        }
    }

    pub fn into_model(self) -> ShellModel {
        match self {
            Loaded::Fresh(m) | Loaded::Restored(m) | Loaded::Newer { model: m, .. } => m,
        }
    }

    pub fn may_save(&self) -> bool {
        !matches!(self, Loaded::Newer { .. })
    }
}

fn default_model(d: &Defaults) -> ShellModel {
    ShellModel::new(d.rails.clone(), d.selected.clone(), d.left_width, d.window)
        .expect("产品给的默认布局必须合法")
}

fn parse_ids(raw: &[String], known: &[PanelId], seen: &mut Vec<PanelId>) -> Vec<PanelId> {
    let mut out = Vec::new();
    for s in raw {
        let Ok(id) = PanelId::new(s.clone()) else {
            continue;
        };
        if known.contains(&id) && !seen.contains(&id) {
            seen.push(id.clone());
            out.push(id);
        }
    }
    out
}

fn pick_selected(rail: &[PanelId], want: &Option<String>) -> PanelId {
    want.as_ref()
        .and_then(|w| rail.iter().find(|p| p.as_str() == w))
        .unwrap_or(&rail[0])
        .clone()
}

/// 逐字段解析:某个字段类型不对只让这个字段回到默认,不连累其余偏好(栏位顺序、选中、收起、窗口大小……)。
fn lenient_file(raw: &serde_json::Value) -> ShellFile {
    fn field<T: serde::de::DeserializeOwned>(raw: &serde_json::Value, key: &str, fallback: T) -> T {
        raw.get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(fallback)
    }
    let d = ShellFile::default();
    ShellFile {
        schema_version: field(raw, "schema_version", d.schema_version),
        window_width: field(raw, "window_width", d.window_width),
        window_height: field(raw, "window_height", d.window_height),
        left: field(raw, "left", d.left),
        right: field(raw, "right", d.right),
        selected_left: field(raw, "selected_left", d.selected_left),
        selected_right: field(raw, "selected_right", d.selected_right),
        left_collapsed: field(raw, "left_collapsed", d.left_collapsed),
        right_collapsed: field(raw, "right_collapsed", d.right_collapsed),
        left_width: field(raw, "left_width", d.left_width),
        panes: field(raw, "panes", d.panes),
    }
}

/// 读盘:`json` 为 `None` = 没有文件。`known` 是产品面板清单里的全部面板。
pub fn load(json: Option<&str>, known: &[PanelId], d: &Defaults, m: &ChromeMetrics) -> Loaded {
    let Some(json) = json else {
        return Loaded::Fresh(default_model(d));
    };
    // 顶层必须是对象:serde 会把 `[1,2]` 这样的数组按"位置序列"塞进结构体,把损坏文件当成已恢复。
    let Some(raw) = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .filter(serde_json::Value::is_object)
    else {
        return Loaded::Fresh(default_model(d));
    };
    // 版本号先从原始 JSON 里读:更新版本可能改了字段形状,本版本按类型解析会失败,但仍必须认出它更新、不得回写。
    if let Some(found) = raw
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .filter(|v| *v > u64::from(SCHEMA_VERSION))
    {
        return Loaded::Newer {
            model: default_model(d),
            found: u32::try_from(found).unwrap_or(u32::MAX),
        };
    }
    let file = lenient_file(&raw);

    // 1. 栏位:丢未知 / 重复,清单里有而文件里没有的面板补回它的默认栏末尾。
    let mut seen = Vec::new();
    let mut rails: Rails = [
        parse_ids(&file.left, known, &mut seen),
        parse_ids(&file.right, known, &mut seen),
    ];
    for p in known {
        if !seen.contains(p) {
            let side = if d.rails[1].contains(p) {
                Side::Right
            } else {
                Side::Left
            };
            rails[side.index()].push(p.clone());
        }
    }
    // 2. 任一栏为空 = 无法修复,整体回落默认。
    if rails[0].is_empty() || rails[1].is_empty() {
        return Loaded::Fresh(default_model(d));
    }
    let selected = [
        pick_selected(&rails[0], &file.selected_left),
        pick_selected(&rails[1], &file.selected_right),
    ];

    // 3. 尺寸:窗口小于下限或非有限 → 默认;左宽非有限或非正 → 默认(夹取在渲染时做,不回写)。
    let ok = |v: f32, min: f32| v.is_finite() && v >= min;
    let window = if ok(file.window_width, d.min_window.0) && ok(file.window_height, d.min_window.1)
    {
        (file.window_width, file.window_height)
    } else {
        d.window
    };
    let left_width = if file.left_width.is_finite() && file.left_width > 0.0 {
        file.left_width
    } else {
        d.left_width
    };

    // 4. 分割比例:0 / 1 / 非有限的会让配对里的一块彻底消失(FillPortion(0)),整条丢弃回默认;
    //    只保留清单里的面板。
    let mut panes = PaneStore::new();
    for p in known {
        if let Some(PaneDims { split, collapsed }) = file.panes.get(p)
            && split.is_finite()
            && split >= m.min_split_ratio
            && split <= m.max_split_ratio
        {
            panes.set(p.clone(), PaneDims::new(split, collapsed));
        }
    }
    // 5. 两侧都收起 = 界面上什么都没有,展开左侧。
    let mut collapsed = [file.left_collapsed, file.right_collapsed];
    if collapsed[0] && collapsed[1] {
        collapsed[0] = false;
    }

    let Ok(mut model) = ShellModel::new(rails, selected, left_width, window) else {
        return Loaded::Fresh(default_model(d));
    };
    model.restore(collapsed, left_width, panes);
    Loaded::Restored(model)
}

pub fn to_file(model: &ShellModel) -> ShellFile {
    let ids = |side: Side| {
        model
            .rail(side)
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect()
    };
    ShellFile {
        schema_version: SCHEMA_VERSION,
        window_width: model.window().0,
        window_height: model.window().1,
        left: ids(Side::Left),
        right: ids(Side::Right),
        selected_left: Some(model.selected(Side::Left).as_str().to_owned()),
        selected_right: Some(model.selected(Side::Right).as_str().to_owned()),
        left_collapsed: model.collapsed(Side::Left),
        right_collapsed: model.collapsed(Side::Right),
        left_width: model.left_width(),
        panes: model.panes().clone(),
    }
}

pub fn to_json(model: &ShellModel) -> String {
    serde_json::to_string_pretty(&to_file(model)).expect("骨架状态可序列化")
}

/// 原子落盘:先写同目录临时文件,再 rename 覆盖;中途崩溃不会留下半截文件。
pub fn save_atomic(path: &Path, model: &ShellModel) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, to_json(model))?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PanelId {
        PanelId::new(s).unwrap()
    }

    fn m() -> ChromeMetrics {
        ChromeMetrics::byteboy2077()
    }

    fn known() -> Vec<PanelId> {
        ["t.a", "t.b", "t.c", "t.x", "t.y"]
            .iter()
            .map(|s| id(s))
            .collect()
    }

    fn defaults() -> Defaults {
        Defaults {
            rails: [
                vec![id("t.a"), id("t.b"), id("t.c")],
                vec![id("t.x"), id("t.y")],
            ],
            selected: [id("t.a"), id("t.x")],
            left_width: 640.0,
            window: (1440.0, 900.0),
            min_window: (800.0, 480.0),
        }
    }

    fn load_str(json: &str) -> Loaded {
        load(Some(json), &known(), &defaults(), &m())
    }

    fn default_json() -> String {
        to_json(&default_model(&defaults()))
    }

    #[test]
    fn no_file_gives_the_defaults() {
        let l = load(None, &known(), &defaults(), &m());
        assert!(matches!(l, Loaded::Fresh(_)));
        assert!(l.may_save());
        assert_eq!(l.model().left_width(), 640.0);
        assert_eq!(l.model().window(), (1440.0, 900.0));
    }

    #[test]
    fn a_saved_model_round_trips_exactly() {
        let mut model = default_model(&defaults());
        let metrics = m();
        let r = |_: &PanelId| None;
        let ctx = crate::model::Ctx {
            metrics: &metrics,
            rules: &r,
        };
        model.update(
            crate::model::ShellMsg::PanelPress { panel: id("t.b") },
            &ctx,
        );
        model.update(
            crate::model::ShellMsg::RailDragMove {
                side: Side::Left,
                index: 2,
            },
            &ctx,
        );
        model.update(crate::model::ShellMsg::PointerReleased, &ctx);
        model.restore([false, true], 512.0, {
            let mut p = PaneStore::new();
            p.set(id("t.b"), PaneDims::new(0.35, true));
            p
        });
        let back = load_str(&to_json(&model)).into_model();
        assert_eq!(back.rails(), model.rails());
        assert_eq!(back.selected(Side::Left), model.selected(Side::Left));
        assert!(back.collapsed(Side::Right));
        assert_eq!(back.left_width(), 512.0);
        assert_eq!(back.panes(), model.panes());
        assert_eq!(back.window(), model.window());
    }

    #[test]
    fn corrupt_json_gives_the_defaults() {
        assert!(matches!(load_str("not json"), Loaded::Fresh(_)));
        assert!(matches!(load_str(""), Loaded::Fresh(_)));
        assert!(matches!(load_str("[1,2]"), Loaded::Fresh(_)));
    }

    #[test]
    fn a_newer_schema_is_never_overwritten() {
        let l = load_str(r#"{ "schema_version": 999, "left": ["t.a"], "right": ["t.x"] }"#);
        assert!(matches!(l, Loaded::Newer { found: 999, .. }));
        assert!(!l.may_save());
        assert_eq!(
            l.model().rails(),
            &defaults().rails,
            "the in-memory layout is the default one"
        );
    }

    #[test]
    fn missing_fields_fall_back_to_defaults_instead_of_failing_the_whole_file() {
        let l = load_str(
            r#"{ "left": ["t.a","t.b","t.c"], "right": ["t.x","t.y"], "left_width": 500.0 }"#,
        );
        let model = l.model();
        assert_eq!(model.left_width(), 500.0);
        assert_eq!(
            model.window(),
            (1440.0, 900.0),
            "no window size in the file"
        );
        assert_eq!(model.selected(Side::Left), &id("t.a"));
    }

    #[test]
    fn unknown_and_duplicate_ids_are_dropped_and_missing_panels_come_back_on_their_default_side() {
        // t.c and t.y are missing, t.zzz is unknown, t.a appears twice, "Bad Id" is not even a PanelId
        let l = load_str(r#"{ "left": ["t.a","t.zzz","Bad Id","t.a"], "right": ["t.b","t.x"] }"#);
        let model = l.model();
        assert_eq!(
            model.rail(Side::Left),
            [id("t.a"), id("t.c")],
            "t.c returns to the left, where it lives by default"
        );
        assert_eq!(
            model.rail(Side::Right),
            [id("t.b"), id("t.x"), id("t.y")],
            "t.y returns to the right; the user's t.b stays right"
        );
    }

    #[test]
    fn an_unrepairable_layout_uses_the_defaults_and_unusable_ids_leave_nothing_behind() {
        // everything is on the left and the right rail is empty: no half-repairs, the whole default layout is used
        let l = load_str(r#"{ "left": ["t.a","t.b","t.c","t.x","t.y"], "right": [] }"#);
        assert!(matches!(l, Loaded::Fresh(_)));
        assert_eq!(l.model().rails(), &defaults().rails);
        // no usable ids at all: every panel comes back on its default side, which is the default layout again
        let l = load_str(r#"{ "left": ["nope"], "right": ["nada"] }"#);
        assert_eq!(l.model().rails(), &defaults().rails);
    }

    #[test]
    fn a_selected_panel_that_is_not_in_its_rail_becomes_the_first_one() {
        let l = load_str(
            r#"{ "left": ["t.a","t.b","t.c"], "right": ["t.x","t.y"], "selected_left": "t.x", "selected_right": "t.zzz" }"#,
        );
        assert_eq!(l.model().selected(Side::Left), &id("t.a"));
        assert_eq!(l.model().selected(Side::Right), &id("t.x"));
    }

    /// 评审 Important 1:一个字段类型不对,不能让整份偏好(栏位顺序、选中、收起、其余字段)被重置。
    #[test]
    fn one_mistyped_field_does_not_reset_the_rest_of_the_file() {
        let l = load_str(
            r#"{ "left": ["t.b","t.a","t.c"], "right": ["t.y","t.x"], "selected_left": "t.c", "left_collapsed": "yes", "left_width": null, "window_width": 1000.0, "window_height": 700.0 }"#,
        );
        assert!(
            matches!(l, Loaded::Restored(_)),
            "the file is repaired, not discarded"
        );
        let model = l.model();
        assert_eq!(
            model.rail(Side::Left),
            [id("t.b"), id("t.a"), id("t.c")],
            "the user's rail order survives"
        );
        assert_eq!(model.rail(Side::Right), [id("t.y"), id("t.x")]);
        assert_eq!(model.selected(Side::Left), &id("t.c"));
        assert_eq!(model.window(), (1000.0, 700.0));
        assert_eq!(
            model.left_width(),
            640.0,
            "only the broken field falls back to its default"
        );
        assert!(!model.collapsed(Side::Left));
    }

    /// 评审 Important 2:未来版本改了字段形状(这里 `left` 变成对象数组)时,本版本解析失败也必须认出它更新,不得回写。
    #[test]
    fn a_newer_schema_is_recognised_even_when_its_shape_no_longer_parses() {
        let l = load_str(
            r#"{ "schema_version": 2, "left": [{ "id": "t.a", "pinned": true }], "right": [{ "id": "t.x" }] }"#,
        );
        assert!(matches!(l, Loaded::Newer { found: 2, .. }));
        assert!(!l.may_save());
    }

    #[test]
    fn bad_numbers_are_sanitized() {
        let l = load_str(
            r#"{ "left": ["t.a","t.b","t.c"], "right": ["t.x","t.y"], "window_width": 0.0, "window_height": 10.0, "left_width": -5.0 }"#,
        );
        assert_eq!(l.model().window(), (1440.0, 900.0));
        assert_eq!(l.model().left_width(), 640.0);
        let l = load_str(
            r#"{ "left": ["t.a","t.b","t.c"], "right": ["t.x","t.y"], "window_width": 1000.0, "window_height": 700.0 }"#,
        );
        assert_eq!(l.model().window(), (1000.0, 700.0));
    }

    #[test]
    fn split_ratios_outside_the_legal_range_are_dropped_so_no_pane_vanishes() {
        let mut json: serde_json::Value = serde_json::from_str(&default_json()).unwrap();
        json["panes"] = serde_json::json!({ "t.a": {"split": 0.0, "collapsed": false}, "t.b": {"split": 1.0, "collapsed": false}, "t.c": {"split": 0.5, "collapsed": true}, "t.zzz": {"split": 0.5, "collapsed": false} });
        let l = load_str(&json.to_string());
        let panes = l.model().panes();
        assert!(
            panes.get(&id("t.a")).is_none() && panes.get(&id("t.b")).is_none(),
            "0.0 and 1.0 would make one side zero-width"
        );
        assert_eq!(panes.get(&id("t.c")), Some(PaneDims::new(0.5, true)));
        assert_eq!(panes.len(), 1, "unknown panels are not kept");
    }

    #[test]
    fn both_zones_collapsed_in_the_file_is_repaired_by_opening_the_left_one() {
        let l = load_str(
            r#"{ "left": ["t.a","t.b","t.c"], "right": ["t.x","t.y"], "left_collapsed": true, "right_collapsed": true }"#,
        );
        assert!(!l.model().collapsed(Side::Left) && l.model().collapsed(Side::Right));
    }

    #[test]
    fn a_stale_old_field_in_the_file_is_ignored() {
        let l = load_str(
            r#"{ "left": ["t.a","t.b","t.c"], "right": ["t.x","t.y"], "files_split": 0.0, "rail_layout": {} }"#,
        );
        assert!(matches!(l, Loaded::Restored(_)));
    }

    #[test]
    fn saving_is_atomic_and_creates_the_directory() {
        let dir = std::env::temp_dir().join(format!("chrome-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("shell.json");
        save_atomic(&path, &default_model(&defaults())).unwrap();
        assert!(path.exists());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed away"
        );
        let back = load(
            Some(&std::fs::read_to_string(&path).unwrap()),
            &known(),
            &defaults(),
            &m(),
        );
        assert!(matches!(back, Loaded::Restored(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
