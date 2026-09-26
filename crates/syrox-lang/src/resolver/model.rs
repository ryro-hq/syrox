#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModuleId(pub(super) u32);

impl ModuleId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemId(pub(super) u32);

impl ItemId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalId(pub(super) u32);

impl LocalId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A local identity and the declaration that introduced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedLocal {
    pub(super) id: LocalId,
    pub(super) span: Span,
}

impl ResolvedLocal {
    pub const fn id(self) -> LocalId {
        self.id
    }

    pub const fn span(self) -> Span {
        self.span
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct CanonicalPath {
    pub(super) segments: Vec<String>,
}

impl CanonicalPath {
    pub fn segments(&self) -> &[String] {
        &self.segments
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedItemKind {
    TypeAlias,
    Struct,
    Enum,
    Resource,
    Value,
    Function,
    OutputValue,
    OutputType,
    OutputFunction,
}

impl ResolvedItemKind {
    pub(super) const fn is_type(self) -> bool {
        matches!(
            self,
            Self::TypeAlias
                | Self::Struct
                | Self::Enum
                | Self::Resource
                | Self::Value
                | Self::OutputType
        )
    }

    pub(super) const fn description(self) -> &'static str {
        match self {
            Self::TypeAlias => "type alias",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Resource => "resource",
            Self::Value => "value",
            Self::Function => "function",
            Self::OutputValue => "output value",
            Self::OutputType => "exported type",
            Self::OutputFunction => "exported function",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedModule {
    pub(super) id: ModuleId,
    pub(super) domain: SourceDomainId,
    pub(super) path: CanonicalPath,
}

impl ResolvedModule {
    pub const fn id(&self) -> ModuleId {
        self.id
    }

    pub const fn path(&self) -> &CanonicalPath {
        &self.path
    }

    pub const fn domain(&self) -> SourceDomainId {
        self.domain
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedItem {
    pub(super) id: ItemId,
    pub(super) domain: SourceDomainId,
    pub(super) module: ModuleId,
    pub(super) path: CanonicalPath,
    pub(super) kind: ResolvedItemKind,
    pub(super) span: Span,
}

impl ResolvedItem {
    pub const fn id(&self) -> ItemId {
        self.id
    }

    pub const fn module(&self) -> ModuleId {
        self.module
    }

    pub const fn domain(&self) -> SourceDomainId {
        self.domain
    }

    pub const fn path(&self) -> &CanonicalPath {
        &self.path
    }

    pub const fn kind(&self) -> ResolvedItemKind {
        self.kind
    }

    pub const fn span(&self) -> Span {
        self.span
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedTarget {
    Module(ModuleId),
    Item(ItemId),
    Local(LocalId),
    EnumVariant {
        enumeration: ItemId,
        index: u32,
    },
    /// A bare pattern/value that the checker must resolve against an expected
    /// enum type, or reject if no matching variant exists.
    ContextualEnumVariant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    Import,
    Type,
    Function,
    Value,
    StructConstructor,
    Refinement,
    Pattern,
    Interpolation,
}

#[derive(Clone, Debug)]
pub struct ResolvedReference {
    pub(super) span: Span,
    pub(super) kind: ReferenceKind,
    pub(super) target: ResolvedTarget,
}

impl ResolvedReference {
    pub const fn span(&self) -> Span {
        self.span
    }

    pub const fn kind(&self) -> ReferenceKind {
        self.kind
    }

    pub const fn target(&self) -> &ResolvedTarget {
        &self.target
    }
}

/// A parsed program together with successful structural name resolution.
///
/// Its fields are private so callers cannot claim resolution for an arbitrary
/// syntax tree. Bare enum variants remain explicit contextual obligations that
/// a type checker must discharge. Construct it with [`super::resolve`].
#[derive(Clone, Debug)]
pub struct ResolvedProgram {
    pub(super) parsed: ParsedSources,
    pub(super) modules: Vec<ResolvedModule>,
    pub(super) items: Vec<ResolvedItem>,
    pub(super) references: Vec<ResolvedReference>,
    pub(super) locals: Vec<ResolvedLocal>,
}

impl ResolvedProgram {
    pub const fn parsed(&self) -> &ParsedSources {
        &self.parsed
    }

    pub fn modules(&self) -> impl ExactSizeIterator<Item = &ResolvedModule> {
        self.modules.iter()
    }

    pub fn items(&self) -> impl ExactSizeIterator<Item = &ResolvedItem> {
        self.items.iter()
    }

    pub fn references(&self) -> impl ExactSizeIterator<Item = &ResolvedReference> {
        self.references.iter()
    }

    pub fn locals(&self) -> impl ExactSizeIterator<Item = ResolvedLocal> + '_ {
        self.locals.iter().copied()
    }

    /// References whose final enum variant identity requires an expected type.
    pub fn contextual_obligations(&self) -> impl Iterator<Item = &ResolvedReference> {
        self.references
            .iter()
            .filter(|reference| reference.target == ResolvedTarget::ContextualEnumVariant)
    }
}
use crate::{ParsedSources, SourceDomainId, Span};
