use std::collections::BTreeMap;

use crate::{SourceDomainId, SourceId, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSources {
    pub(crate) sources: Vec<ParsedSource>,
    pub(crate) input_domains: BTreeMap<String, SourceDomainId>,
}

impl ParsedSources {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ParsedSource> {
        self.sources.iter()
    }

    pub fn get(&self, source_id: SourceId) -> Option<&ParsedProgram> {
        self.sources
            .get(source_id.index())
            .filter(|source| source.source_id == source_id)
            .map(|source| &source.program)
    }

    pub fn declaration_count(&self) -> usize {
        self.sources
            .iter()
            .map(|source| source.program.declaration_count())
            .sum()
    }

    pub(crate) fn input_domain(&self, alias: &str) -> Option<SourceDomainId> {
        self.input_domains.get(alias).copied()
    }

    pub(crate) fn is_input_domain(&self, domain: SourceDomainId) -> bool {
        self.input_domains
            .values()
            .any(|candidate| *candidate == domain)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSource {
    pub(crate) source_id: SourceId,
    pub(crate) domain: SourceDomainId,
    pub(crate) program: ParsedProgram,
}

impl ParsedSource {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub const fn domain(&self) -> SourceDomainId {
        self.domain
    }

    pub const fn program(&self) -> &ParsedProgram {
        &self.program
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedProgram {
    pub items: Vec<Item>,
}

impl ParsedProgram {
    pub fn declaration_count(&self) -> usize {
        self.items.len()
    }
}

pub type Analysis = ParsedProgram;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub text: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub segments: Vec<Ident>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Module(Module),
    Use(Use),
    Inputs(Inputs),
    Outputs(Outputs),
    TypeAlias(TypeAlias),
    Struct(Struct),
    Enum(Enum),
    Resource(PrimitiveDeclaration),
    Value(PrimitiveDeclaration),
    Function(Function),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    pub path: Path,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Use {
    pub path: Path,
    pub names: Option<Vec<Ident>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    pub entries: Vec<Input>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub name: Ident,
    pub value: StringLiteral,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outputs {
    pub entries: Vec<Output>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub kind: OutputKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Value {
        name: Ident,
        ty: Type,
        value: Expression,
    },
    Type {
        name: Ident,
        ty: Type,
    },
    Function {
        name: Ident,
        signature: Signature,
        function: Path,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub parameters: Vec<Type>,
    pub result: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeAlias {
    pub name: Ident,
    pub ty: Type,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Struct {
    pub name: Ident,
    pub opaque: bool,
    pub type_parameters: Vec<TypeParameter>,
    pub scopes: Vec<Ident>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeParameter {
    pub name: Ident,
    pub owner: bool,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: Ident,
    pub ty: Type,
    pub default: Option<Expression>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enum {
    pub name: Ident,
    pub variants: Vec<Ident>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primitive {
    Int,
    Str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimitiveDeclaration {
    pub name: Ident,
    pub primitive: Primitive,
    pub scope: Option<Ident>,
    pub refinements: Vec<Refinement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refinement {
    pub kind: RefinementKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefinementKind {
    Range {
        start: IntegerLiteral,
        end: IntegerLiteral,
        inclusive: bool,
    },
    Set(Vec<Literal>),
    Predicate(Path),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub name: Ident,
    pub parameters: Vec<Parameter>,
    pub result: Option<Type>,
    pub body: Block,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameter {
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub statements: Vec<Statement>,
    pub tail: Option<Box<Expression>>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Let { name: Ident, value: Expression },
    Expression(Expression),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Named { path: Path, arguments: Vec<Type> },
    List(Path),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expression {
    pub kind: ExpressionKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpressionKind {
    Integer(IntegerLiteral),
    String(StringLiteral),
    Path(Path),
    Call {
        callee: Path,
        arguments: Vec<Expression>,
    },
    Struct {
        path: Path,
        type_arguments: Vec<Type>,
        fields: Vec<StructField>,
    },
    List(Vec<Expression>),
    Erase {
        ty: Type,
        value: Box<Expression>,
    },
    Match {
        value: Box<Expression>,
        arms: Vec<MatchArm>,
    },
    Group(Box<Expression>),
    Field {
        value: Box<Expression>,
        fields: Vec<Ident>,
    },
    Concat(Vec<Expression>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructField {
    pub name: Ident,
    pub value: Expression,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub value: Expression,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Wildcard(Span),
    Path(Path),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    Integer(IntegerLiteral),
    String(StringLiteral),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegerLiteral {
    pub source: String,
    pub value: i64,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringLiteral {
    /// Exact source spelling, including quotes and escapes.
    pub source: String,
    pub parts: Vec<StringPart>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StringPart {
    /// Exact source spelling between interpolations, excluding the outer quotes.
    Text {
        source: String,
        span: Span,
    },
    Interpolation {
        path: Path,
        span: Span,
    },
}
