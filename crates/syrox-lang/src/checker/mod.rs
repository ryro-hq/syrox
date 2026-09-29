//! Type, policy and affine ownership checking for resolved Syrox programs.

mod bodies;
mod collections;
mod display;
mod editor;
mod enums;
mod expressions;
mod functions;
mod index;
mod inference;
mod model;
mod ownership;
mod policy;
mod relocation;
mod types;

pub use bodies::{BodyBudget, BodyEffects, BodyReplayState};
pub(crate) use bodies::{BodyPublication, BodyPublications};
pub use editor::FieldInfo;
pub(crate) use editor::{NominalShape, RepresentationAuthority, nominal_id, substitute_type};
pub use model::*;
pub use policy::*;
pub(crate) use relocation::FactMapper;
pub use relocation::OwnerCheckedFacts;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    Block, Diagnostic, Enum, Expression, ExpressionKind, Function, Item, ItemId, ItemKind, Literal,
    LocalId, MAX_DIAGNOSTICS, MatchArm, ModuleId, OutputKind, ParsedSource, Path, Pattern,
    Primitive, PrimitiveDeclaration, ReferenceKind, RefinementKind, ResolvedItem, ResolvedItemKind,
    ResolvedProgram, ResolvedTarget, SourceDomainId, Span, StatementKind, StringLiteral,
    StringPart, Struct, StructField, Type, TypeAlias, TypeKind,
};
use bodies::BodyState;

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

#[derive(Clone)]
struct AliasInfo<'a> {
    declaration: &'a TypeAlias,
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
    Function {
        parameters: Vec<RawTy>,
        result: Box<RawTy>,
        once: bool,
    },
}

#[derive(Clone)]
struct Binding {
    ty: Ty,
    affine: bool,
    moved: Option<Span>,
    declaration: Span,
    conditional: bool,
    status: TypeStatus,
    ownership_unknown: bool,
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
    collection_metadata_units: usize,
    references: BTreeMap<(u32, u32, u32), ResolvedTarget>,
    local_at: BTreeMap<(u32, u32, u32), LocalId>,
    item_at: BTreeMap<(u32, u32, u32), ItemId>,
    module_by_path: BTreeMap<SourceDomainId, BTreeMap<&'a [String], ModuleId>>,
    item_context: BTreeMap<ItemId, Context>,
    aliases: BTreeMap<ItemId, AliasInfo<'a>>,
    structs: BTreeMap<ItemId, StructInfo<'a>>,
    enums: BTreeMap<ItemId, &'a Enum>,
    primitives: BTreeMap<ItemId, PrimitiveInfo<'a>>,
    functions: BTreeMap<ItemId, FunctionInfo<'a>>,
    output_values: BTreeMap<ItemId, &'a Type>,
    carriers: BTreeSet<ItemId>,
    generic_instances: BTreeSet<Ty>,
    struct_fields: BTreeMap<ItemId, Vec<(String, RawTy, bool)>>,
    function_types: BTreeMap<ItemId, (Vec<Ty>, Ty)>,
    body: BodyState,
    prior_diagnostics: usize,
    prior_metadata: usize,
    #[cfg(test)]
    unpartitioned_bodies: bool,
    #[cfg(test)]
    reuse: bodies::reuse::ReuseHarness,
    incomplete_bodies: BTreeSet<(u32, u32, u32)>,
    cancellation: Option<&'a crate::AnalysisCancellation>,
    editor: EditorFacts,
    publications: BodyPublications,
    effect_retention: bodies::EffectRetention,
    reference_inventory: Option<std::sync::Arc<[Span]>>,
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
            collection_metadata_units: 0,
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
            output_values: BTreeMap::new(),
            carriers: BTreeSet::new(),
            generic_instances: BTreeSet::new(),
            struct_fields: BTreeMap::new(),
            function_types: BTreeMap::new(),
            body: BodyState::default(),
            prior_diagnostics: 0,
            prior_metadata: 0,
            #[cfg(test)]
            unpartitioned_bodies: false,
            #[cfg(test)]
            reuse: bodies::reuse::ReuseHarness::default(),
            incomplete_bodies: BTreeSet::new(),
            cancellation: None,
            editor: EditorFacts::default(),
            publications: BodyPublications::default(),
            effect_retention: bodies::EffectRetention::default(),
            reference_inventory: None,
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
        self.collect_editor_types();
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

pub(crate) struct PartialCheck {
    pub bodies: BodyPublications,
    pub expressions: Vec<CheckedExpression>,
    pub functions: BTreeMap<ItemId, Ty>,
    pub diagnostics: Vec<Diagnostic>,
    pub editor: EditorFacts,
}

pub(crate) fn check_partial(
    resolved: &ResolvedProgram,
    policy: &CheckPolicy,
    incomplete_bodies: BTreeSet<(u32, u32, u32)>,
    cancellation: &crate::AnalysisCancellation,
    limits: CheckLimits,
) -> PartialCheck {
    let mut checker = Checker::new(resolved, policy, limits);
    checker.incomplete_bodies = incomplete_bodies;
    checker.cancellation = Some(cancellation);
    checker.run();
    checker.editor.patterns = std::mem::take(&mut checker.patterns);
    PartialCheck {
        bodies: checker.publications,
        expressions: checker.expressions,
        functions: checker
            .function_types
            .into_iter()
            .map(|(id, (parameters, result))| {
                (
                    id,
                    Ty::Function {
                        parameters,
                        result: Box::new(result),
                        once: false,
                    },
                )
            })
            .collect(),
        diagnostics: checker.diagnostics,
        editor: checker.editor,
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
