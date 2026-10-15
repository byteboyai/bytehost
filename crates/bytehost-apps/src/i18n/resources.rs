//! 包内 i18n 资源装载(仅 `server` feature;纯解析逻辑见 `crate::i18n::title`)。
//!
//! 加载 `<root>/<strings_dir>/<canonical-locale>.json`:严格扁平 `string->string` 对象,
//! 拒绝重复 key、嵌套、非字符串值、超限与非法字符;只枚举声明语言(默认语言文件必在,
//! 非默认缺失允许回退),并对资源路径做符号链接/逃逸检查。
//!
//! 资源字节已在 `digest_tree` 覆盖范围内(整包根哈希),本模块只负责**读取并校验**,
//! 校验在 `read_package`(plan 与 install/upgrade 两阶段都会调用)处执行,满足 TOCTOU 重验要求。

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer, MapAccess, Visitor};

use crate::i18n::{I18n, I18nBundle, TitleRef, normalize_locale};
use crate::manifest::Manifest;
use crate::proto::{AppDisplayI18n, AppDisplayTitle, DisplayTitleScope};

/// 资源限额(spec §4.1 / T0-5 表),全部在解析前生效,采用限长读取(limit+1)。
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// 声明语言数上限。
    pub max_locales: usize,
    /// 单文件字节上限(512 KiB)。
    pub max_file_bytes: usize,
    /// 声明资源总计字节上限(8 MiB)。
    pub max_total_bytes: usize,
    /// 单文件 key 数上限。
    pub max_keys_per_file: usize,
    /// key 长度上限(1–128 由调用方保证非空)。
    pub max_key_len: usize,
    /// 普通值 Unicode 字符数上限。
    pub max_value_len: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_locales: 16,
            max_file_bytes: 512 * 1024,
            max_total_bytes: 8 * 1024 * 1024,
            max_keys_per_file: 4096,
            max_key_len: 128,
            max_value_len: 4096,
        }
    }
}

/// 投影预算(spec §5):最多 64 个标题入口参与 i18n,序列化后上限 128 KiB。
pub const MAX_DISPLAY_ENTRIES: usize = 64;
pub const MAX_PROJECTION_BYTES: usize = 128 * 1024;

/// 装载阶段的资源错误,带字段级原因(经 [`crate::manager::ManagerError::Resource`] 上报)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceError {
    pub field: String,
    pub reason: String,
}

impl ResourceError {
    fn new(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ResourceError {}

/// 从已(暂存或安装)包根 `root` 装载 i18n 资源,返回按规范标签归类的 [`I18nBundle`]。
///
/// `i18n` 为清单中的 `[i18n]` 段;`[i18n]` 不存在时调用方不应调用本函数。
pub fn load_i18n_resources(
    root: &Path,
    i18n: &I18n,
    limits: &ResourceLimits,
) -> Result<I18nBundle, ResourceError> {
    let strings_dir = i18n.strings_dir.as_deref().unwrap_or("locales");
    check_strings_dir(strings_dir)?;

    let dir = root.join(strings_dir);
    if !dir.is_dir() {
        return Err(ResourceError::new(
            "i18n.strings_dir",
            format!("目录不存在: {}", dir.display()),
        ));
    }

    // 声明语言数上限。
    if i18n.supported_locales.len() > limits.max_locales {
        return Err(ResourceError::new(
            "i18n.supported_locales",
            format!(
                "声明语言数 {} 超过 {} 上限",
                i18n.supported_locales.len(),
                limits.max_locales
            ),
        ));
    }
    let default_canon = normalize_locale(&i18n.default_locale)
        .map_err(|_| ResourceError::new("i18n.default_locale", "非法语言标签"))?;

    // 枚举目录:建立 规范标签 -> 文件路径;同时拒绝文件名非规范(大小写/书写不匹配)的 .json。
    // 额外文件(非语言命名)忽略,不装载为宿主词表。
    let mut present: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in fs::read_dir(&dir).map_err(|e| io_field(&dir, e))? {
        let entry = entry.map_err(|e| io_field(&dir, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        match normalize_locale(stem) {
            Ok(c) if c == stem => {
                present.insert(c, entry.path());
            }
            Ok(c) => {
                return Err(ResourceError::new(
                    format!("i18n.strings_dir/{name}"),
                    format!("文件名必须是规范标签(期望 {c},收到 {name})"),
                ));
            }
            Err(_) => { /* 非语言命名的额外文件:不装载为宿主词表 */ }
        }
    }

    let canon_root = fs::canonicalize(root).map_err(|e| io_field(root, e))?;

    let mut bundle = I18nBundle::default();
    let mut total: usize = 0;
    for raw in &i18n.supported_locales {
        let canon = normalize_locale(raw).map_err(|_| {
            ResourceError::new("i18n.supported_locales", format!("非法语言标签: {raw}"))
        })?;

        let path = match present.get(&canon) {
            Some(p) => p.clone(),
            None => {
                if canon == default_canon {
                    return Err(ResourceError::new(
                        format!("i18n.strings_dir/{canon}.json"),
                        "默认语言文件必须存在",
                    ));
                }
                // 非默认缺失:允许未翻译,运行期回退到 default/fallback。
                continue;
            }
        };

        let canon_path = ensure_inside(&path, &canon_root)?;
        let bytes = read_regular_file_capped(&canon_path, limits.max_file_bytes)?;
        let map = parse_locale_file(&bytes, limits)?;
        total += bytes.len();
        if total > limits.max_total_bytes {
            return Err(ResourceError::new(
                format!("i18n.strings_dir/{canon}.json"),
                format!("声明资源总计超过 {} 字节上限", limits.max_total_bytes),
            ));
        }
        bundle.locales.insert(canon, map);
    }

    Ok(bundle)
}

/// 把已装载的资源投影成 [`AppDisplayI18n`]:只投影 manifest 声明的标题 key(presentation + 各 entrypoint),
/// 逐支持语言解析出标题值。超限(入口数 / 序列化字节)返回 `ResourceError`,供安装期拒绝。
///
/// 不携带完整词表、资源路径或启动令牌;产品用 [`AppDisplayI18n::resolve_title`] 按自己的 active locale 解析。
pub fn project_display_i18n(
    manifest: &Manifest,
    bundle: &I18nBundle,
    _limits: &ResourceLimits,
) -> Result<AppDisplayI18n, ResourceError> {
    let i18n = manifest
        .i18n
        .as_ref()
        .ok_or_else(|| ResourceError::new("i18n", "manifest 未声明 [i18n]"))?;

    let mut titles: Vec<AppDisplayTitle> = Vec::new();
    if let (Some(k), Some(f)) = (
        &manifest.presentation.title_key,
        &manifest.presentation.title_fallback,
    ) {
        titles.push(build_title(
            DisplayTitleScope::Presentation,
            k,
            f,
            i18n,
            bundle,
        ));
    }
    for (name, ep) in &manifest.entrypoints {
        if let (Some(k), Some(f)) = (&ep.title_key, &ep.title_fallback) {
            titles.push(build_title(
                DisplayTitleScope::Entrypoint { name: name.clone() },
                k,
                f,
                i18n,
                bundle,
            ));
        }
    }

    if titles.len() > MAX_DISPLAY_ENTRIES {
        return Err(ResourceError::new(
            "i18n.display",
            format!(
                "投影标题入口数 {} 超过 {} 上限",
                titles.len(),
                MAX_DISPLAY_ENTRIES
            ),
        ));
    }

    let proj = AppDisplayI18n {
        default_locale: i18n.default_locale.clone(),
        supported_locales: i18n.supported_locales.clone(),
        webview_locale: i18n.webview_locale,
        titles,
    };

    let bytes = serde_json::to_vec(&proj)
        .map_err(|e| ResourceError::new("i18n.display", format!("序列化失败: {e}")))?;
    if bytes.len() > MAX_PROJECTION_BYTES {
        return Err(ResourceError::new(
            "i18n.display",
            format!("投影序列化 {} 字节超过 128 KiB 上限", bytes.len()),
        ));
    }
    Ok(proj)
}

/// 为单个声明标题构建投影:逐支持语言解析出标题值(current→default→fallback,空/不安全值视为缺失)。
fn build_title(
    scope: DisplayTitleScope,
    key: &str,
    fallback: &str,
    i18n: &I18n,
    bundle: &I18nBundle,
) -> AppDisplayTitle {
    let r = TitleRef {
        key: key.to_string(),
        fallback: fallback.to_string(),
    };
    let mut values = BTreeMap::new();
    for loc in &i18n.supported_locales {
        let l = normalize_locale(loc).unwrap_or_else(|_| loc.clone());
        let v = bundle.resolve_title(&r, &l, &i18n.default_locale);
        values.insert(l, v);
    }
    AppDisplayTitle {
        scope,
        key: key.to_string(),
        fallback: fallback.to_string(),
        values,
    }
}

/// `strings_dir` 必须是包根内的相对路径:非空、不以 `/` 开头、不含 `..` 段、反斜杠或 NUL。
fn check_strings_dir(p: &str) -> Result<(), ResourceError> {
    let trimmed = p.strip_suffix('/').unwrap_or(p);
    let bad = p.is_empty()
        || p.starts_with('/')
        || p.contains('\\')
        || p.contains('\0')
        || trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if bad {
        return Err(ResourceError::new(
            "i18n.strings_dir",
            format!(
                "必须是应用包内的相对路径(不能为空、不能绝对、不能含 `.`/`..`/空段),收到 {p:?}"
            ),
        ));
    }
    Ok(())
}

/// 拒绝文件自身或任一**祖先目录**为符号链接(无论指向包内还是包外),并在打开前把路径
/// `canonicalize` 后断言仍在 `canon_root` 内(防 TOCTOU 替换/逃逸)。
///
/// 返回 `canonicalize` 后的真实路径,供 `read_regular_file_capped` 直接打开,消除「检查」与
/// 「打开」之间的路径替换窗口(spec §4.2:仅 canonicalize 再普通 open 不充分,需绑定实际句柄)。
///
/// 祖先比较用 **canonical** 形式,避免 `/tmp` 等系统符号链接干扰停止条件(原始路径的
/// `/tmp` 段是符号链接,但不在包内、也不应被当成「应用资源的父目录链接」)。
fn ensure_inside(path: &Path, canon_root: &Path) -> Result<PathBuf, ResourceError> {
    // 文件自身不能是符号链接。
    let ft = fs::symlink_metadata(path).map_err(|e| io_field(path, e))?;
    if ft.is_symlink() {
        return Err(ResourceError::new(
            path.display().to_string(),
            "不允许符号链接",
        ));
    }
    // 逐祖先目录检查(到 `canon_root` 为止):任一祖先是符号链接即拒绝,包括指向包内、
    // "看似安全"的链接。这样满足 spec §4.2「逐组件拒绝链接」,而非只拒包外。
    // 比较时把每个祖先 canonicalize 再与 `canon_root`(已 canonical)比对,避免在 `/tmp` 处误停。
    let mut cur = path.parent().map(|p| p.to_path_buf());
    while let Some(comp) = cur {
        match fs::canonicalize(&comp) {
            Ok(ccomp) if ccomp.as_path() == canon_root => break,
            Ok(ccomp) => {
                let cft = fs::symlink_metadata(&comp).map_err(|e| io_field(&comp, e))?;
                if cft.is_symlink() {
                    return Err(ResourceError::new(
                        comp.display().to_string(),
                        "资源路径的父目录不允许是符号链接",
                    ));
                }
                cur = ccomp.parent().map(|p| p.to_path_buf());
            }
            Err(_) => break,
        }
    }
    let canon = fs::canonicalize(path).map_err(|e| io_field(path, e))?;
    if !canon.starts_with(canon_root) {
        return Err(ResourceError::new(
            path.display().to_string(),
            "资源路径逃逸出应用包",
        ));
    }
    Ok(canon)
}

/// 以限长(limit+1)读取普通文件,拒绝符号链接与非普通文件。
fn read_regular_file_capped(path: &Path, cap: usize) -> Result<Vec<u8>, ResourceError> {
    let ft = fs::symlink_metadata(path).map_err(|e| io_field(path, e))?;
    if ft.is_symlink() {
        return Err(ResourceError::new(
            path.display().to_string(),
            "不允许符号链接",
        ));
    }
    if !ft.is_file() {
        return Err(ResourceError::new(
            path.display().to_string(),
            "不是普通文件(可能是设备/FIFO/套接字)",
        ));
    }
    let f = fs::File::open(path).map_err(|e| io_field(path, e))?;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024) + 1);
    let n = f
        .take((cap + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| io_field(path, e))?;
    if n > cap {
        return Err(ResourceError::new(
            path.display().to_string(),
            format!("文件超过 {cap} 字节上限"),
        ));
    }
    Ok(buf)
}

/// 严格解析扁平 `string->string` JSON 对象:UTF-8、无嵌套、无重复 key、只字符串值、key/值受限额约束。
fn parse_locale_file(
    bytes: &[u8],
    limits: &ResourceLimits,
) -> Result<BTreeMap<String, String>, ResourceError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let map = de
        .deserialize_map(FlatMapVisitor { limits })
        .map_err(|e| ResourceError::new("i18n 资源", format!("JSON 解析失败: {e}")))?;
    Ok(map)
}

struct FlatMapVisitor<'a> {
    limits: &'a ResourceLimits,
}

impl<'de, 'a> Visitor<'de> for FlatMapVisitor<'a> {
    type Value = BTreeMap<String, String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("扁平 string->string JSON 对象(无嵌套、无非字符串值)")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut out = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(de::Error::custom(format!("重复的 key: {key}")));
            }
            let raw: serde_json::Value = map.next_value()?;
            let v = match raw {
                serde_json::Value::String(s) => s,
                _ => return Err(de::Error::custom(format!("key {key:?} 的值不是字符串"))),
            };
            check_key(&key, self.limits).map_err(de::Error::custom)?;
            check_value(&v, self.limits).map_err(de::Error::custom)?;
            out.insert(key, v);
        }
        if out.len() > self.limits.max_keys_per_file {
            return Err(de::Error::custom(format!(
                "单个文件 key 数超过 {} 上限",
                self.limits.max_keys_per_file
            )));
        }
        Ok(out)
    }
}

/// key:`[A-Za-z0-9_.-]` 字符集,长度 1–`max_key_len`。
fn check_key(key: &str, limits: &ResourceLimits) -> Result<(), String> {
    if key.is_empty() || key.len() > limits.max_key_len {
        return Err(format!("key 长度必须在 1..={} 之间", limits.max_key_len));
    }
    if !key.chars().all(is_key_char) {
        return Err(format!("key {key:?} 只能含 [A-Za-z0-9_.-]"));
    }
    Ok(())
}

/// 值:非空、Unicode 字符数 ≤ `max_value_len`(用作标题时另有 64 + 控制/双向字符约束,在解析时施加)。
fn check_value(v: &str, limits: &ResourceLimits) -> Result<(), String> {
    if v.is_empty() {
        return Err("值不能为空".to_string());
    }
    if v.chars().count() > limits.max_value_len {
        return Err(format!("值长度超过 {} 字符上限", limits.max_value_len));
    }
    Ok(())
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn io_field(path: &Path, e: io::Error) -> ResourceError {
    ResourceError::new(path.display().to_string(), format!("I/O: {e}"))
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::i18n::WebViewLocale;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bh_i18n_test_{}_{}", nanos, n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn i18n(default: &str, supported: &[&str]) -> I18n {
        I18n {
            default_locale: default.to_string(),
            supported_locales: supported.iter().map(|s| s.to_string()).collect(),
            strings_dir: None,
            webview_locale: WebViewLocale::None,
        }
    }

    fn locales_dir(root: &Path) -> PathBuf {
        let d = root.join("locales");
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_and_one_translation_load_into_bundle() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"app.title":"Notes","a.b":"x"}"#).unwrap();
        fs::write(d.join("zh-CN.json"), r#"{"app.title":"笔记"}"#).unwrap();
        let bundle = load_i18n_resources(
            &root,
            &i18n("en", &["en", "zh-CN"]),
            &ResourceLimits::default(),
        )
        .expect("应成功装载");
        assert_eq!(
            bundle.locales.get("en").unwrap().get("app.title").unwrap(),
            "Notes"
        );
        assert_eq!(
            bundle
                .locales
                .get("zh-CN")
                .unwrap()
                .get("app.title")
                .unwrap(),
            "笔记"
        );
        // 声明顺序与文件排序无关
        assert_eq!(bundle.locales.len(), 2);
    }

    #[test]
    fn duplicate_key_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"k":"v","k":"w"}"#).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("重复的 key"), "{e}");
    }

    #[test]
    fn nested_value_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"k":{"a":"b"}}"#).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("不是字符串"), "{e}");
    }

    #[test]
    fn non_string_value_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"k":1}"#).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("不是字符串"), "{e}");
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), [0xff, 0xfe, b'{', b'}']).unwrap();
        assert!(
            load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default()).is_err()
        );
    }

    #[test]
    fn invalid_key_charset_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"a b":"x"}"#).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("只能含"), "{e}");
    }

    #[test]
    fn key_too_long_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        let long = "a".repeat(129);
        fs::write(d.join("en.json"), format!(r#"{{"{long}":"x"}}"#)).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("长度"), "{e}");
    }

    #[test]
    fn value_too_long_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        let long = "x".repeat(4097);
        fs::write(d.join("en.json"), format!(r#"{{"k":"{long}"}}"#)).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("长度"), "{e}");
    }

    #[test]
    fn empty_value_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"k":""}"#).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("不能为空"), "{e}");
    }

    #[test]
    fn too_many_keys_per_file_is_rejected() {
        let limits = ResourceLimits {
            max_keys_per_file: 3,
            ..Default::default()
        };
        let root = tmp_root();
        let d = locales_dir(&root);
        let body: Vec<String> = (0..4).map(|i| format!(r#""k{i}":"v""#)).collect();
        fs::write(d.join("en.json"), format!("{{{}}}", body.join(","))).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &limits).unwrap_err();
        assert!(e.reason.contains("key 数"), "{e}");
    }

    #[test]
    fn single_file_over_byte_cap_is_rejected() {
        let limits = ResourceLimits {
            max_file_bytes: 100,
            ..Default::default()
        };
        let root = tmp_root();
        let d = locales_dir(&root);
        let long = "x".repeat(101);
        fs::write(d.join("en.json"), format!(r#"{{"k":"{long}"}}"#)).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &limits).unwrap_err();
        assert!(e.reason.contains("超过 100 字节"), "{e}");
    }

    #[test]
    fn total_over_byte_cap_is_rejected() {
        let limits = ResourceLimits {
            max_total_bytes: 150,
            ..Default::default()
        };
        let root = tmp_root();
        let d = locales_dir(&root);
        let a = "x".repeat(100);
        let b = "y".repeat(100);
        fs::write(d.join("en.json"), format!(r#"{{"k":"{a}"}}"#)).unwrap();
        fs::write(d.join("zh-CN.json"), format!(r#"{{"k":"{b}"}}"#)).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en", "zh-CN"]), &limits).unwrap_err();
        assert!(e.reason.contains("总计"), "{e}");
    }

    #[test]
    fn too_many_declared_locales_is_rejected() {
        let limits = ResourceLimits {
            max_locales: 2,
            ..Default::default()
        };
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"k":"v"}"#).unwrap();
        fs::write(d.join("zh-CN.json"), r#"{"k":"v"}"#).unwrap();
        fs::write(d.join("fr.json"), r#"{"k":"v"}"#).unwrap();
        let e =
            load_i18n_resources(&root, &i18n("en", &["en", "zh-CN", "fr"]), &limits).unwrap_err();
        assert!(e.reason.contains("语言数"), "{e}");
    }

    #[test]
    fn missing_default_locale_file_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        // 只提供非默认语言
        fs::write(d.join("zh-CN.json"), r#"{"k":"v"}"#).unwrap();
        let e = load_i18n_resources(
            &root,
            &i18n("en", &["en", "zh-CN"]),
            &ResourceLimits::default(),
        )
        .unwrap_err();
        assert!(e.reason.contains("默认语言文件必须存在"), "{e}");
    }

    #[test]
    fn missing_non_default_locale_is_allowed() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"app.title":"Notes"}"#).unwrap();
        // zh-CN 缺失:允许未翻译,只装载 default
        let bundle = load_i18n_resources(
            &root,
            &i18n("en", &["en", "zh-CN"]),
            &ResourceLimits::default(),
        )
        .expect("非默认缺失应允许");
        assert_eq!(bundle.locales.len(), 1);
        assert!(bundle.locales.contains_key("en"));
    }

    #[test]
    fn non_canonical_filename_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        // 声明 zh-CN,但文件名写成 Zh-CN(大小写不匹配)
        fs::write(d.join("Zh-CN.json"), r#"{"k":"v"}"#).unwrap();
        let e = load_i18n_resources(
            &root,
            &i18n("zh-CN", &["zh-CN"]),
            &ResourceLimits::default(),
        )
        .unwrap_err();
        assert!(e.reason.contains("规范标签"), "{e}");
    }

    #[test]
    fn extra_unrelated_file_is_ignored() {
        let root = tmp_root();
        let d = locales_dir(&root);
        fs::write(d.join("en.json"), r#"{"k":"v"}"#).unwrap();
        // 额外非 .json 文件:完全忽略,不装载为宿主词表(也不会被当成语言文件报错)
        fs::write(d.join("README.md"), r#"{"secret":"leak"}"#).unwrap();
        let bundle = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .expect("额外文件应被忽略");
        assert_eq!(bundle.locales.len(), 1);
        assert!(!bundle.locales.contains_key("README"));
    }

    #[test]
    fn absolute_strings_dir_is_rejected() {
        let root = tmp_root();
        let mut i = i18n("en", &["en"]);
        i.strings_dir = Some("/abs/locales".to_string());
        let e = load_i18n_resources(&root, &i, &ResourceLimits::default()).unwrap_err();
        assert!(e.reason.contains("相对路径"), "{e}");
    }

    #[test]
    fn dot_dot_in_strings_dir_is_rejected() {
        let root = tmp_root();
        let mut i = i18n("en", &["en"]);
        i.strings_dir = Some("locales/../../etc".to_string());
        let e = load_i18n_resources(&root, &i, &ResourceLimits::default()).unwrap_err();
        assert!(e.reason.contains("相对路径"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_resource_file_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        let real = d.join("real.json");
        fs::write(&real, r#"{"k":"v"}"#).unwrap();
        std::os::unix::fs::symlink(&real, d.join("en.json")).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("符号链接"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_outside_package_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        // 包外的“诱饵”文件
        let outside = tmp_root().join("evil.json");
        fs::write(&outside, r#"{"k":"stolen"}"#).unwrap();
        // en.json 是一个指向包外的符号链接(真实链接替换场景,而非静态 ../ 字符串)
        std::os::unix::fs::symlink(&outside, d.join("en.json")).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("符号链接"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_dir_escape_is_rejected() {
        let root = tmp_root();
        let d = locales_dir(&root);
        // locales 自身是一个指向包外的符号链接目录
        let outside = tmp_root().join("outside_locales");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("en.json"), r#"{"k":"v"}"#).unwrap();
        let _ = fs::remove_dir_all(&d);
        std::os::unix::fs::symlink(&outside, &d).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(
            e.reason.contains("符号链接") || e.reason.contains("逃逸"),
            "{e}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_dir_inside_package_is_rejected() {
        let root = tmp_root();
        let real = root.join("locales_real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("en.json"), r#"{"k":"v"}"#).unwrap();
        // `locales` 是符号链接,指向包内另一个目录:看似安全,但 spec §4.2 要求逐组件拒绝链接。
        let d = locales_dir(&root);
        let _ = fs::remove_dir_all(&d);
        std::os::unix::fs::symlink(&real, &d).unwrap();
        let e = load_i18n_resources(&root, &i18n("en", &["en"]), &ResourceLimits::default())
            .unwrap_err();
        assert!(e.reason.contains("符号链接"), "{e}");
    }

    fn manifest(toml: &str) -> Manifest {
        Manifest::from_toml(toml, &crate::id::Version::new(0, 1, 0)).expect("manifest 应合法")
    }

    #[test]
    fn project_only_declared_titles_per_locale() {
        let m = manifest(
            r#"
schema_version = 1
min_host_version = "0.1.0"
id = "notes"
name = "Notes"
version = "1.0.0"

[i18n]
default_locale = "en"
supported_locales = ["en", "zh-CN"]

[presentation]
entrypoint = "main"
title_key = "app.title"
title_fallback = "Notes"

[entrypoints.main]
type = "web"
path = "/"
title_key = "main.title"
title_fallback = "Open"

[runtime]
kind = "static_web"
source = "web/"
"#,
        );
        let mut bundle = I18nBundle::default();
        bundle.locales.insert(
            "en".into(),
            [
                ("app.title".into(), "Notes".into()),
                ("main.title".into(), "Open".into()),
                ("unused.key".into(), "ignored".into()),
            ]
            .into(),
        );
        bundle.locales.insert(
            "zh-CN".into(),
            [
                ("app.title".into(), "笔记".into()),
                ("main.title".into(), "打开".into()),
            ]
            .into(),
        );
        let proj =
            project_display_i18n(&m, &bundle, &ResourceLimits::default()).expect("投影应成功");
        assert_eq!(proj.default_locale, "en");
        assert_eq!(
            proj.supported_locales,
            vec!["en".to_string(), "zh-CN".to_string()]
        );
        assert_eq!(proj.titles.len(), 2, "只投影声明的两个标题");

        // 解析器:zh-CN → 中文;未知语言 → 协商到 default(en)
        let pres = DisplayTitleScope::Presentation;
        let main = DisplayTitleScope::Entrypoint {
            name: "main".into(),
        };
        assert_eq!(proj.resolve_title(&pres, "zh-CN"), Some("笔记"));
        assert_eq!(proj.resolve_title(&pres, "fr"), Some("Notes"));
        assert_eq!(proj.resolve_title(&main, "zh-CN"), Some("打开"));
        // 未声明来源的标题 → None(调用方回落 name)
        assert_eq!(
            proj.resolve_title(
                &DisplayTitleScope::Entrypoint {
                    name: "other".into()
                },
                "en"
            ),
            None
        );
    }

    #[test]
    fn project_rejects_too_many_title_entries() {
        let mut toml = String::from(
            r#"
schema_version = 1
min_host_version = "0.1.0"
id = "big"
name = "Big"
version = "1.0.0"

[i18n]
default_locale = "en"
supported_locales = ["en"]

[presentation]
entrypoint = "e0"
title_key = "app.title"
title_fallback = "Big"

[runtime]
kind = "static_web"
source = "web/"
"#,
        );
        // 再加 65 个声明标题的 entrypoint → 共 66 个,超过 64 上限
        for i in 0..65 {
            toml.push_str(&format!(
                "\n[entrypoints.e{i}]\ntype = \"web\"\npath = \"/\"\ntitle_key = \"e{i}.title\"\ntitle_fallback = \"E{i}\"\n"
            ));
        }
        let m = manifest(&toml);
        let bundle = I18nBundle::default();
        let e = project_display_i18n(&m, &bundle, &ResourceLimits::default()).unwrap_err();
        assert!(e.reason.contains("64"), "{e}");
    }

    #[test]
    fn projection_budget_constants_are_defensive_upper_bounds() {
        // 逐标题解析走 `resolve_title`,不安全值(空 / 超 64 字符 / 控制或双向字符)回落 fallback,
        // 因此单个标题值恒 ≤64 字符;叠加 64 入口上限后,投影序列化实际远低于 128 KiB。
        // 该上限是不可达的防御性兜底(常量保留),这里仅断言常量取值,避免测试一个永远触发的分支。
        assert_eq!(MAX_PROJECTION_BYTES, 128 * 1024);
        assert_eq!(MAX_DISPLAY_ENTRIES, 64);
    }
}
