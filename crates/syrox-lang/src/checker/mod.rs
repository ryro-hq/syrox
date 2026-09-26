//! Type, policy and affine ownership checking for resolved Syrox programs.

mod expressions;
mod index;
mod model;
mod ownership;
mod policy;
mod types;

pub use model::*;
pub use policy::*;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    Block, Diagnostic, Enum, Expression, ExpressionKind, Function, Item, ItemId, ItemKind, Literal,
    LocalId, MAX_DIAGNOSTICS, MatchArm, ModuleId, OutputKind, ParsedSource, Path, Pattern,
    Primitive, PrimitiveDeclaration, ReferenceKind, RefinementKind, ResolvedItem, ResolvedItemKind,
    ResolvedProgram, ResolvedTarget, SourceDomainId, Span, StatementKind, StringLiteral,
    StringPart, Struct, StructField, Type, TypeAlias, TypeKind,
};

#[derive(Clone, Copy)]
struct Context {
    domain: SourceDomainId,
    module: ModuleId,
}

#[derive(Clone)]
struct StructInfo<'a> {
    declaration: &'a Struct,
    context: Context,
    parameters: Vec<LocalId>,
}

#[derive(Clone, Copy)]
struct FunctionInfo<'a> {
    declaration: &'a Function,
    context: Context,
}

#[derive(Clone, Copy)]
struct PrimitiveInfo<'a> {
    declaration: &'a PrimitiveDeclaration,
}

#[derive(Clone, Debug)]
enum RawTy {
    Concrete(Ty),
    Parameter(LocalId),
    Specialization {
        template: ItemId,
        arguments: Vec<RawTy>,
    },
    List(Box<RawTy>),
}

#[derive(Clone)]
struct Binding {
    ty: Ty,
    affine: bool,
    moved: Option<Span>,
    declaration: Span,
}

struct Checker<'a> {
    program: &'a ResolvedProgram,
    policy: &'a CheckPolicy,
    limits: CheckLimits,
    work: usize,
    exhausted: bool,
    diagnostics: Vec<Diagnostic>,
    expressions: Vec<CheckedExpression>,
    patterns: Vec<CheckedPattern>,
    references: BTreeMap<(u32, u32, u32), ResolvedTarget>,
    local_at: BTreeMap<(u32, u32, u32), LocalId>,
    item_at: BTreeMap<(u32, u32, u32), ItemId>,
    module_by_path: BTreeMap<SourceDomainId, BTreeMap<&'a [String], ModuleId>>,
    item_context: BTreeMap<ItemId, Context>,
    aliases: BTreeMap<ItemId, &'a TypeAlias>,
    structs: BTreeMap<ItemId, StructInfo<'a>>,
    enums: BTreeMap<ItemId, &'a Enum>,
    primitives: BTreeMap<ItemId, PrimitiveInfo<'a>>,
    functions: BTreeMap<ItemId, FunctionInfo<'a>>,
    carriers: BTreeSet<ItemId>,
    generic_instances: BTreeSet<Ty>,
    struct_fields: BTreeMap<ItemId, Vec<(String, RawTy, bool)>>,
    function_types: BTreeMap<ItemId, (Vec<Ty>, Ty)>,
    bindings: BTreeMap<LocalId, Binding>,
    type_parameters: BTreeMap<LocalId, Ty>,
}

impl<'a> Checker<'a> {
    pub(super) fn new(
        program: &'a ResolvedProgram,
        policy: &'a CheckPolicy,
        limits: CheckLimits,
    ) -> Self {
        Self {
            program,
            policy,
            limits,
            work: 0,
            exhausted: false,
            diagnostics: Vec::new(),
            expressions: Vec::new(),
            patterns: Vec::new(),
            references: BTreeMap::new(),
            local_at: BTreeMap::new(),
            item_at: BTreeMap::new(),
            module_by_path: BTreeMap::new(),
            item_context: BTreeMap::new(),
            aliases: BTreeMap::new(),
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
            primitives: BTreeMap::new(),
            functions: BTreeMap::new(),
            carriers: BTreeSet::new(),
            generic_instances: BTreeSet::new(),
            struct_fields: BTreeMap::new(),
            function_types: BTreeMap::new(),
            bindings: BTreeMap::new(),
            type_parameters: BTreeMap::new(),
        }
    }

    pub(super) fn run(&mut self) {
        self.index_resolution_metadata();
        if self.exhausted {
            return;
        }
        self.index();
        if self.exhausted {
            return;
        }
        self.validate_declarations();
        if self.exhausted {
            return;
        }
        self.propagate_carriers();
        if self.exhausted {
            return;
        }
        self.collect_function_types();
        if self.exhausted {
            return;
        }
        self.check_defaults();
        if self.exhausted {
            return;
        }
        self.check_functions();
        if self.exhausted {
            return;
        }
        self.check_outputs();
    }
}

pub(super) fn span_key(span: Span) -> (u32, u32, u32) {
    (
        u32::try_from(span.source_id().index()).expect("source IDs are u32"),
        span.start(),
        span.end(),
    )
}

#[cfg(test)]
mod tests;
