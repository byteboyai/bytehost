//! 稳定身份:`PanelId`、`ScopeId`、`PanelInstanceId`、`SurfaceSlot`。
//!
//! 全部是**纯描述**:不依赖 GUI、不依赖任何产品 crate。产品组合根把业务枚举映射到
//! 这些值(规格 §5.1);Host 不解释 `ScopeId` 的业务含义。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 面板 id:带命名空间的稳定字符串,如 `digger.outline`、`shared.usage`、`app:excalidraw`。
///
/// 形状为 `<namespace>.<name>` 或 `<namespace>:<name>`(应用面板用 `app:<validated-id>`,
/// 其中 `<validated-id>` 已由产品侧的 `AppId` 校验)。名字段与命名空间都限小写字母、
/// 数字、`-`、`_`,以字母开头,保证它既是稳定键、也安全可读。**例外**:`app:`
/// 命名空间的名字段承载已校验的 `AppId`,与 `AppId` 的形状一致——允许数字开头
/// (如 `app:0day`)、不含 `_`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PanelId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelIdError {
    Empty,
    TooLong,
    /// 缺少 `<namespace>` 与 `<name>` 之间的分隔符(`.` 或 `:`)。
    MissingSeparator,
    /// 命名空间或名字段为空。
    EmptySegment,
    /// 出现了非法字符。
    BadChar(char),
    /// 段以非字母开头。
    SegmentMustStartWithLetter(char),
}

impl fmt::Display for PanelIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "面板 id 不能为空"),
            Self::TooLong => write!(f, "面板 id 最长 128 个字符"),
            Self::MissingSeparator => {
                write!(f, "面板 id 必须形如 <命名空间>.<名字> 或 <命名空间>:<名字>")
            }
            Self::EmptySegment => write!(f, "面板 id 的命名空间与名字都不能为空"),
            Self::BadChar(c) => write!(f, "面板 id 只能含小写字母、数字、'-' 和 '_',发现 {c:?}"),
            Self::SegmentMustStartWithLetter(c) => {
                write!(f, "面板 id 的每段都要以字母开头,发现 {c:?}")
            }
        }
    }
}

impl std::error::Error for PanelIdError {}

fn valid_segment(seg: &str) -> Result<(), PanelIdError> {
    let Some(first) = seg.chars().next() else {
        return Err(PanelIdError::EmptySegment);
    };
    if !first.is_ascii_lowercase() {
        return Err(PanelIdError::SegmentMustStartWithLetter(first));
    }
    if let Some(c) = seg
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
    {
        return Err(PanelIdError::BadChar(c));
    }
    Ok(())
}

/// `app:` 命名空间的名字段校验:允许数字开头(与已校验的 `AppId` 形状一致——
/// `AppId` 允许 `0day` 这类数字开头的 id)。字符集与 `AppId` 相同(`[a-z0-9-]`,
/// 不含 `_`,不以 `-` 开头/结尾);名字段已由产品侧 `AppId` 校验过,这里只做
/// 形状自洽检查,避免 `app:123-notes` 这类合法应用在接入 rail/注册表时表示不出来。
fn valid_app_name_segment(seg: &str) -> Result<(), PanelIdError> {
    let Some(first) = seg.chars().next() else {
        return Err(PanelIdError::EmptySegment);
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(PanelIdError::SegmentMustStartWithLetter(first));
    }
    if let Some(c) = seg
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(PanelIdError::BadChar(c));
    }
    Ok(())
}

impl PanelId {
    pub fn new(s: impl Into<String>) -> Result<Self, PanelIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(PanelIdError::Empty);
        }
        if s.len() > 128 {
            return Err(PanelIdError::TooLong);
        }
        // 找到第一个 `.` 或 `:`,作为命名空间与名字的分隔(先出现的优先)。
        let sep_pos = s.find(['.', ':']).ok_or(PanelIdError::MissingSeparator)?;
        let (ns, rest) = s.split_at(sep_pos);
        let name = &rest[1..];
        valid_segment(ns)?;
        // `app:` 命名空间的名字段是已校验的 `AppId`,允许数字开头。
        if ns == "app" {
            valid_app_name_segment(name)?;
        } else {
            valid_segment(name)?;
        }
        Ok(Self(s))
    }

    /// 用**已校验**的应用 id 构造 `app:<id>` 面板 id。调用方负责先经 `AppId` 校验;
    /// 这里只做 `app:` 前缀包装,再走一遍本类型自己的形状校验(与 `AppId` 形状一致,
    /// 允许数字开头,如 `app:0day`)。
    pub fn for_app(validated_app_id: &str) -> Result<Self, PanelIdError> {
        Self::new(format!("app:{validated_app_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 命名空间(分隔符之前的部分)。
    pub fn namespace(&self) -> &str {
        let pos = self.0.find(['.', ':']).expect("构造时已保证有分隔符");
        &self.0[..pos]
    }
}

impl TryFrom<String> for PanelId {
    type Error = PanelIdError;
    fn try_from(s: String) -> Result<Self, PanelIdError> {
        Self::new(s)
    }
}

impl From<PanelId> for String {
    fn from(id: PanelId) -> String {
        id.0
    }
}

impl fmt::Display for PanelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 作用域 id:产品提供的**不透明**值。Host 不理解其业务含义(不解释 Project 数据)。
/// 只要求非空、长度受限,并原样回传。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeIdError {
    Empty,
    TooLong,
}

impl fmt::Display for ScopeIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "作用域 id 不能为空"),
            Self::TooLong => write!(f, "作用域 id 最长 256 个字符"),
        }
    }
}

impl std::error::Error for ScopeIdError {}

impl ScopeId {
    pub fn new(s: impl Into<String>) -> Result<Self, ScopeIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ScopeIdError::Empty);
        }
        if s.len() > 256 {
            return Err(ScopeIdError::TooLong);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ScopeId {
    type Error = ScopeIdError;
    fn try_from(s: String) -> Result<Self, ScopeIdError> {
        Self::new(s)
    }
}

impl From<ScopeId> for String {
    fn from(id: ScopeId) -> String {
        id.0
    }
}

impl fmt::Display for ScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 面板实例 id:区分**同一**面板的不同实例。
///
/// 每次销毁重建递增 `generation`,旧实例的在途任务不能写入新实例(规格 §5.1)。
/// `slot` 是产品在本次会话内分配的稳定序号,`generation` 只在重建时递增。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PanelInstanceId {
    pub slot: u32,
    pub generation: u32,
}

impl PanelInstanceId {
    pub const fn new(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    /// 同一 slot 的下一代实例(销毁重建时调用)。
    pub const fn next_generation(self) -> Self {
        Self {
            slot: self.slot,
            generation: self.generation.wrapping_add(1),
        }
    }
}

impl fmt::Display for PanelInstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.slot, self.generation)
    }
}

/// Surface 槽位:一个面板实例可以拥有的多个 Surface(主内容、侧栏等)的稳定编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SurfaceSlot(pub u32);

impl SurfaceSlot {
    pub const MAIN: Self = Self(0);

    pub const fn new(n: u32) -> Self {
        Self(n)
    }
}

impl fmt::Display for SurfaceSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "surface{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_id_accepts_dotted_and_colon_namespaced_names() {
        for ok in [
            "digger.outline",
            "shared.usage",
            "app:excalidraw",
            "a.b_c-1",
        ] {
            assert!(PanelId::new(ok).is_ok(), "{ok}");
        }
        assert_eq!(
            PanelId::new("digger.outline").unwrap().namespace(),
            "digger"
        );
        assert_eq!(PanelId::new("app:excalidraw").unwrap().namespace(), "app");
    }

    #[test]
    fn panel_id_rejects_bad_shapes() {
        assert_eq!(PanelId::new(""), Err(PanelIdError::Empty));
        assert_eq!(
            PanelId::new("noseparator"),
            Err(PanelIdError::MissingSeparator)
        );
        assert_eq!(PanelId::new(".name"), Err(PanelIdError::EmptySegment));
        assert_eq!(PanelId::new("ns."), Err(PanelIdError::EmptySegment));
        assert_eq!(
            PanelId::new("Ns.name"),
            Err(PanelIdError::SegmentMustStartWithLetter('N'))
        );
        assert_eq!(
            PanelId::new("ns.name/../x"),
            Err(PanelIdError::BadChar('/'))
        );
        assert!(matches!(
            PanelId::new("a".repeat(200)),
            Err(PanelIdError::TooLong)
        ));
    }

    #[test]
    fn panel_id_for_app_wraps_a_validated_id() {
        assert_eq!(
            PanelId::for_app("excalidraw").unwrap().as_str(),
            "app:excalidraw"
        );
        // 非法应用 id(带大写/斜杠)会被本类型自身再挡一次。
        assert!(PanelId::for_app("Excal").is_err());
        assert!(PanelId::for_app("../x").is_err());
    }

    #[test]
    fn panel_id_for_app_accepts_digit_leading_valid_app_ids() {
        // AppId 允许数字开头(`0day` 是 AppId 自己的测试样例);for_app 必须能表示它。
        assert_eq!(PanelId::for_app("0day").unwrap().as_str(), "app:0day");
        assert_eq!(
            PanelId::for_app("123-notes").unwrap().as_str(),
            "app:123-notes"
        );
        // 但仍拒绝与 AppId 形状不符的:下划线不在 AppId 字符集内。
        assert!(PanelId::for_app("a_b").is_err());
        // 普通命名空间的名字段不受此放宽影响,仍要求字母开头。
        assert_eq!(
            PanelId::new("ns.0day"),
            Err(PanelIdError::SegmentMustStartWithLetter('0'))
        );
    }

    #[test]
    fn panel_id_serde_validates_on_read() {
        let id = PanelId::new("shared.usage").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"shared.usage\"");
        assert_eq!(
            serde_json::from_str::<PanelId>("\"shared.usage\"").unwrap(),
            id
        );
        assert!(serde_json::from_str::<PanelId>("\"no separator\"").is_err());
    }

    #[test]
    fn scope_id_is_opaque_and_only_length_checked() {
        assert!(ScopeId::new("project/42").is_ok());
        assert!(ScopeId::new("任意产品值").is_ok());
        assert_eq!(ScopeId::new(""), Err(ScopeIdError::Empty));
        assert!(matches!(
            ScopeId::new("x".repeat(300)),
            Err(ScopeIdError::TooLong)
        ));
    }

    #[test]
    fn instance_generation_increments_on_rebuild() {
        let a = PanelInstanceId::new(3, 0);
        let b = a.next_generation();
        assert_eq!(a, PanelInstanceId::new(3, 0));
        assert_eq!(b, PanelInstanceId::new(3, 1));
        assert_ne!(a, b);
        assert_eq!(a.slot, b.slot);
    }
}
