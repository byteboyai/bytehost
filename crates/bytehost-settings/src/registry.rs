//! 有序页面注册表:校验空 / 重复 / 保留命名空间 / 内置种类一致性。

use std::collections::HashMap;
use std::fmt;

use crate::page::{PageDescriptor, PageId, PageKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// 内置页以外的 ID 使用了 `bytehost.*`。
    ReservedNamespace(PageId),
    /// 内置种类但 ID 不在已知内置集合中。
    UnknownBuiltIn(PageId),
    /// 已知内置 ID 却声明成自定义页。
    BuiltInDeclaredCustom(PageId),
    /// 自定义页声明成内置种类。
    CustomDeclaredBuiltIn(PageId),
    /// 没有英文回退标题的页面无法导航。
    EmptyTitle(PageId),
    /// 重复 ID;不静默覆盖。
    Duplicate(PageId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::ReservedNamespace(id) => {
                write!(f, "page id uses the reserved bytehost namespace: {id}")
            }
            RegistryError::UnknownBuiltIn(id) => write!(f, "unknown built-in page: {id}"),
            RegistryError::BuiltInDeclaredCustom(id) => {
                write!(f, "built-in page must be registered as built-in: {id}")
            }
            RegistryError::CustomDeclaredBuiltIn(id) => {
                write!(f, "custom page must not be registered as built-in: {id}")
            }
            RegistryError::EmptyTitle(id) => {
                write!(f, "page must provide an English fallback title: {id}")
            }
            RegistryError::Duplicate(id) => write!(f, "duplicate page id: {id}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// 有序页面集合。注册顺序即导航顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRegistry {
    pages: Vec<PageDescriptor>,
    index: HashMap<PageId, usize>,
}

impl PageRegistry {
    pub fn new(pages: Vec<PageDescriptor>) -> Result<Self, RegistryError> {
        let mut index = HashMap::with_capacity(pages.len());
        for (position, page) in pages.iter().enumerate() {
            validate(page)?;
            if index.insert(page.id.clone(), position).is_some() {
                return Err(RegistryError::Duplicate(page.id.clone()));
            }
        }
        Ok(Self { pages, index })
    }

    pub fn empty() -> Self {
        Self {
            pages: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub fn pages(&self) -> &[PageDescriptor] {
        &self.pages
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn contains(&self, id: &PageId) -> bool {
        self.index.contains_key(id)
    }

    pub fn get(&self, id: &PageId) -> Option<&PageDescriptor> {
        self.index.get(id).map(|position| &self.pages[*position])
    }

    /// 默认页:优先主题页,否则首个注册页。
    pub fn default_page(&self) -> Option<&PageId> {
        let theme = PageId::new(crate::THEME_ID).expect("static built-in id");
        if self.contains(&theme) {
            return self.pages.iter().find(|p| p.id == theme).map(|p| &p.id);
        }
        self.pages.first().map(|p| &p.id)
    }

    /// 请求页可用则返回它;否则按 "主题 → 首个" 回退。
    pub fn resolve(&self, requested: Option<&PageId>) -> Option<&PageId> {
        requested
            .and_then(|id| self.get(id))
            .map(|page| &page.id)
            .or_else(|| self.default_page())
    }
}

fn validate(page: &PageDescriptor) -> Result<(), RegistryError> {
    match page.kind {
        PageKind::BuiltIn => {
            if !page.id.is_known_builtin() {
                return Err(RegistryError::UnknownBuiltIn(page.id.clone()));
            }
        }
        PageKind::Custom => {
            if page.id.is_known_builtin() {
                return Err(RegistryError::BuiltInDeclaredCustom(page.id.clone()));
            }
            if page.id.is_reserved() {
                return Err(RegistryError::ReservedNamespace(page.id.clone()));
            }
        }
    }
    if page.fallback.trim().is_empty() {
        return Err(RegistryError::EmptyTitle(page.id.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PageDescriptor, PageId};

    fn custom(raw: &str, fallback: &str) -> PageDescriptor {
        PageDescriptor::custom(PageId::new(raw).unwrap(), format!("page.{raw}"), fallback)
    }

    fn builtin(raw: &str, fallback: &str) -> PageDescriptor {
        PageDescriptor::built_in(PageId::new(raw).unwrap(), fallback)
    }

    #[test]
    fn preserves_registration_order_and_looks_pages_up() {
        let registry = PageRegistry::new(vec![
            custom("dozer.git", "Git"),
            builtin(crate::THEME_ID, "Theme"),
        ])
        .unwrap();
        let ids: Vec<_> = registry.pages().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["dozer.git", "bytehost.theme"]);
        assert_eq!(
            registry
                .get(&PageId::new("dozer.git").unwrap())
                .unwrap()
                .fallback,
            "Git"
        );
    }

    #[test]
    fn rejects_duplicate_ids() {
        let err = PageRegistry::new(vec![
            custom("dozer.git", "Git"),
            custom("dozer.git", "Git again"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            RegistryError::Duplicate(PageId::new("dozer.git").unwrap())
        );
    }

    #[test]
    fn products_cannot_use_the_reserved_namespace() {
        let err = PageRegistry::new(vec![custom("bytehost.git", "Not allowed")]).unwrap_err();
        assert_eq!(
            err,
            RegistryError::ReservedNamespace(PageId::new("bytehost.git").unwrap())
        );
    }

    #[test]
    fn builtin_ids_must_match_their_kind() {
        let err = PageRegistry::new(vec![builtin("bytehost.custom", "Nope")]).unwrap_err();
        assert_eq!(
            err,
            RegistryError::UnknownBuiltIn(PageId::new("bytehost.custom").unwrap())
        );
        let err = PageRegistry::new(vec![builtin(crate::APPS_ID, "Apps")]).unwrap();
        assert!(err.contains(&PageId::new(crate::APPS_ID).unwrap()));
    }

    #[test]
    fn known_builtin_cannot_be_registered_as_custom() {
        let err = PageRegistry::new(vec![PageDescriptor {
            id: PageId::new(crate::THEME_ID).unwrap(),
            kind: PageKind::Custom,
            text_key: None,
            fallback: "Theme".into(),
        }])
        .unwrap_err();
        assert_eq!(
            err,
            RegistryError::BuiltInDeclaredCustom(PageId::new(crate::THEME_ID).unwrap())
        );
    }

    #[test]
    fn default_and_fallback_prefer_theme_then_first() {
        let theme = PageId::new(crate::THEME_ID).unwrap();
        let git = PageId::new("dozer.git").unwrap();
        let with_theme = PageRegistry::new(vec![
            custom("dozer.git", "Git"),
            builtin(crate::THEME_ID, "Theme"),
        ])
        .unwrap();
        assert_eq!(with_theme.default_page(), Some(&theme));
        assert_eq!(with_theme.resolve(Some(&git)), Some(&git));
        assert_eq!(
            with_theme.resolve(Some(&PageId::new("missing.page").unwrap())),
            Some(&theme)
        );

        let without_theme = PageRegistry::new(vec![custom("dozer.git", "Git")]).unwrap();
        assert_eq!(without_theme.default_page(), Some(&git));
        assert_eq!(without_theme.resolve(None), Some(&git));
        assert_eq!(PageRegistry::empty().resolve(None), None);
    }
}
