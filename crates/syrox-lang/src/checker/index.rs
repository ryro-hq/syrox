use super::{
    BTreeMap, BTreeSet, Checker, Context, Diagnostic, FunctionInfo, Item, ItemId, ItemKind,
    Literal, LocalId, MAX_DIAGNOSTICS, ModuleId, OutputKind, ParsedSource, Path, Primitive,
    PrimitiveDeclaration, PrimitiveInfo, PrimitiveType, RawTy, ReferenceKind, RefinementKind,
    ResolvedItemKind, ResolvedTarget, Span, StructInfo, span_key,
};

impl<'a> Checker<'a> {
    pub(super) fn index_resolution_metadata(&mut self) {
        for reference in self.program.references() {
            if reference.kind() == ReferenceKind::Import {
                continue;
            }
            if !self.charge(reference.span()) {
                return;
            }
            self.references
                .insert(span_key(reference.span()), reference.target().clone());
        }
        for local in self.program.locals() {
            if !self.charge(local.span()) {
                return;
            }
            self.local_at.insert(span_key(local.span()), local.id());
        }
        for item in self.program.items() {
            if !self.charge(item.span()) {
                return;
            }
            self.item_at.insert(span_key(item.span()), item.id());
        }
        let program = self.program;
        let span = program
            .parsed()
            .iter()
            .flat_map(|source| source.program().items.iter())
            .next()
            .map_or_else(
                || Span::new(crate::SourceId::SINGLE, 0, 0),
                |item| item.span,
            );
        for module in program.modules() {
            if !self.charge(span) {
                return;
            }
            self.module_by_path
                .entry(module.domain())
                .or_default()
                .insert(module.path().segments(), module.id());
        }
    }

    pub(super) fn charge(&mut self, span: Span) -> bool {
        self.observe_effect(span, super::bodies::EffectOperation::WorkProbe(1));
        if self
            .cancellation
            .is_some_and(|cancellation| cancellation.check().is_err())
        {
            self.exhausted = true;
            return false;
        }
        if self.exhausted {
            return false;
        }
        self.work = self.work.saturating_add(1);
        if self.work <= self.limits.max_work {
            return true;
        }
        self.exhausted = true;
        self.error("type checking work limit reached", span);
        false
    }

    pub(super) fn error(&mut self, message: impl Into<String>, span: Span) {
        self.push_diagnostic(Diagnostic::error(message, span));
    }

    pub(super) fn push_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.observe_effect(diagnostic.span, super::bodies::EffectOperation::Diagnostic);
        self.body.fact_errors = self.body.fact_errors.saturating_add(1);
        if self.prior_diagnostics + self.diagnostics.len() < MAX_DIAGNOSTICS {
            self.diagnostics
                .push(diagnostic.in_phase(crate::DiagnosticCode::TypeCheck));
        }
    }

    pub(super) fn reserve_metadata(&mut self, span: Span) -> bool {
        self.observe_effect(span, super::bodies::EffectOperation::Metadata);
        if self
            .expressions
            .len()
            .saturating_add(self.patterns.len())
            .saturating_add(self.prior_metadata)
            .saturating_add(self.collection_metadata_units)
            >= self.limits.max_metadata_units
        {
            if !self.exhausted {
                self.exhausted = true;
                self.error("checked metadata limit reached", span);
            }
            false
        } else {
            true
        }
    }

    pub(super) fn index(&mut self) {
        let program = self.program;
        for source in program.parsed().iter() {
            self.index_source(source);
            if self.exhausted {
                return;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn index_source(&mut self, source: &'a ParsedSource) {
        let root = self
            .module_by_path
            .get(&source.domain())
            .and_then(|modules| modules.get(source.module()))
            .expect("resolver records each source domain root")
            .to_owned();
        let mut stack: Vec<(&Item, Vec<String>, ModuleId)> = source
            .program()
            .items
            .iter()
            .rev()
            .map(|item| (item, source.module().to_vec(), root))
            .collect();
        while let Some((item, path, module)) = stack.pop() {
            if !self.charge(item.span) {
                return;
            }
            if let ItemKind::Module(declaration) = &item.kind {
                let mut child_path = path;
                child_path.extend(
                    declaration
                        .path
                        .segments
                        .iter()
                        .map(|segment| segment.text.clone()),
                );
                let child = self
                    .module_by_path
                    .get(&source.domain())
                    .and_then(|modules| modules.get(child_path.as_slice()))
                    .expect("resolved module")
                    .to_owned();
                for nested in declaration.items.iter().rev() {
                    stack.push((nested, child_path.clone(), child));
                }
                continue;
            }
            let context = Context {
                domain: source.domain(),
                module,
            };
            match &item.kind {
                ItemKind::TypeAlias(declaration) => {
                    if let Some(id) = self.declaration_id(declaration.name.span) {
                        self.item_context.insert(id, context);
                        let parameters = declaration
                            .type_parameters
                            .iter()
                            .filter_map(|parameter| self.local_id(parameter.name.span))
                            .collect();
                        self.aliases.insert(
                            id,
                            super::AliasInfo {
                                declaration,
                                parameters,
                            },
                        );
                    }
                }
                ItemKind::Struct(declaration) => {
                    if let Some(id) = self.declaration_id(declaration.name.span) {
                        let parameters = declaration
                            .type_parameters
                            .iter()
                            .filter_map(|parameter| self.local_id(parameter.name.span))
                            .collect();
                        self.item_context.insert(id, context);
                        self.structs.insert(
                            id,
                            StructInfo {
                                declaration,
                                context,
                                parameters,
                            },
                        );
                    }
                }
                ItemKind::Enum(declaration) => {
                    if let Some(id) = self.declaration_id(declaration.name.span) {
                        self.item_context.insert(id, context);
                        self.enums.insert(id, declaration);
                    }
                }
                ItemKind::Resource(declaration) | ItemKind::Value(declaration) => {
                    if let Some(id) = self.declaration_id(declaration.name.span) {
                        self.item_context.insert(id, context);
                        self.primitives.insert(id, PrimitiveInfo { declaration });
                    }
                }
                ItemKind::Function(declaration) => {
                    if let Some(id) = self.declaration_id(declaration.name.span) {
                        self.item_context.insert(id, context);
                        self.functions.insert(
                            id,
                            FunctionInfo {
                                declaration,
                                context,
                            },
                        );
                    }
                }
                ItemKind::Outputs(outputs) => {
                    for output in &outputs.entries {
                        if !self.charge(output.span) {
                            return;
                        }
                        if let OutputKind::Value { ty, .. } = &output.kind
                            && let Some(id) = self.declaration_id(output.span)
                        {
                            self.output_values.insert(id, ty);
                        }
                    }
                }
                ItemKind::Module(_) | ItemKind::Use(_) | ItemKind::Inputs(_) => {}
            }
        }
    }

    pub(super) fn declaration_id(&self, span: Span) -> Option<ItemId> {
        self.item_at.get(&span_key(span)).copied()
    }

    pub(super) fn local_id(&self, span: Span) -> Option<LocalId> {
        self.local_at.get(&span_key(span)).copied()
    }

    pub(super) fn target(&self, span: Span) -> Option<&ResolvedTarget> {
        self.references.get(&span_key(span))
    }

    pub(super) fn validate_declarations(&mut self) {
        let aliases: Vec<_> = self.aliases.values().cloned().collect();
        for alias in aliases {
            let params = alias
                .parameters
                .iter()
                .copied()
                .map(|id| (id, RawTy::Parameter(id)))
                .collect();
            let ty = self.resolve_raw_type(&alias.declaration.ty, &params, 0);
            if let Some(concrete) = self.concretize(ty, &BTreeMap::new(), alias.declaration.ty.span)
            {
                self.track_generic_instances(&concrete, alias.declaration.ty.span);
            }
        }
        let structures: Vec<_> = self
            .structs
            .iter()
            .map(|(&id, info)| (id, info.clone()))
            .collect();
        for (id, info) in structures {
            self.validate_struct(id, &info);
        }
        self.validate_enums();
        let primitives: Vec<_> = self
            .primitives
            .iter()
            .map(|(&id, info)| (id, *info))
            .collect();
        for (id, info) in primitives {
            self.validate_primitive(id, info.declaration);
        }
    }

    pub(super) fn validate_struct(&mut self, id: ItemId, info: &StructInfo<'_>) {
        let declaration = &info.declaration;
        let mut fields = BTreeSet::new();
        let mut scopes = BTreeSet::new();
        let mut owner_count = 0;
        for parameter in &declaration.type_parameters {
            if !self.charge(parameter.span) {
                return;
            }
            if parameter.owner {
                owner_count += 1;
                if !declaration.opaque {
                    self.error("`owner` is only valid on an opaque struct", parameter.span);
                }
            }
        }
        if owner_count > 1 {
            self.error(
                "an opaque struct may delegate construction to at most one type parameter",
                declaration.name.span,
            );
        }
        for scope in &declaration.scopes {
            if !self.charge(scope.span) {
                return;
            }
            if !scopes.insert(scope.text.clone()) {
                self.error(
                    format!("duplicate struct scope `{}`", scope.text),
                    scope.span,
                );
            }
            if !self.policy.scopes.contains_key(&scope.text) {
                self.error(format!("unknown scope `{}`", scope.text), scope.span);
            }
        }
        let params: BTreeMap<_, _> = info
            .parameters
            .iter()
            .copied()
            .map(|parameter| (parameter, RawTy::Parameter(parameter)))
            .collect();
        let mut checked_fields = Vec::new();
        for field in &declaration.fields {
            if !self.charge(field.span) {
                return;
            }
            if !fields.insert(field.name.text.clone()) {
                self.error(
                    format!("duplicate field `{}`", field.name.text),
                    field.name.span,
                );
            }
            let ty = self.resolve_raw_type(&field.ty, &params, 0);
            if let Some(concrete) = self.concretize(ty.clone(), &BTreeMap::new(), field.span) {
                self.track_generic_instances(&concrete, field.ty.span);
            }
            checked_fields.push((field.name.text.clone(), ty, field.default.is_some()));
        }
        self.struct_fields.insert(id, checked_fields);
    }

    pub(super) fn validate_primitive(&mut self, id: ItemId, declaration: &PrimitiveDeclaration) {
        let is_value = self.item_kind(id) == Some(ResolvedItemKind::Value);
        if let Some(scope) = &declaration.scope {
            if is_value {
                self.error("a value cannot declare an ownership scope", scope.span);
            } else if !self.policy.scopes.contains_key(&scope.text) {
                self.error(format!("unknown scope `{}`", scope.text), scope.span);
            }
        }
        let primitive = match declaration.primitive {
            Primitive::Int => PrimitiveType::Int,
            Primitive::Str => PrimitiveType::Str,
        };
        for refinement in &declaration.refinements {
            if !self.charge(refinement.span) {
                return;
            }
            match &refinement.kind {
                RefinementKind::Range {
                    start,
                    end,
                    inclusive,
                } => {
                    if primitive != PrimitiveType::Int {
                        self.error("a range can only refine `int`", refinement.span);
                    }
                    if start.value > end.value || (!inclusive && start.value == end.value) {
                        self.error("this range can never be satisfied", refinement.span);
                    }
                }
                RefinementKind::Set(values) => {
                    if values.is_empty() {
                        self.error("a refinement set cannot be empty", refinement.span);
                        continue;
                    }
                    let first = literal_primitive(&values[0]);
                    if values.iter().any(|value| literal_primitive(value) != first) {
                        self.error("a refinement set must be homogeneous", refinement.span);
                    } else if first != primitive {
                        self.error(
                            "refinement set values have the wrong primitive type",
                            refinement.span,
                        );
                    }
                }
                RefinementKind::Predicate(path) => {
                    let name = display_path(path);
                    match self.policy.predicates.get(&name) {
                        Some(input) if *input == primitive => {}
                        Some(_) => self.error(
                            format!("predicate `{name}` has the wrong primitive input"),
                            path.span,
                        ),
                        None => self.error(format!("unknown predicate `{name}`"), path.span),
                    }
                }
            }
        }
    }
}

pub(super) fn display_path(path: &Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("::")
}

pub(super) fn literal_primitive(literal: &Literal) -> PrimitiveType {
    match literal {
        Literal::Integer(_) => PrimitiveType::Int,
        Literal::String(_) => PrimitiveType::Str,
    }
}
