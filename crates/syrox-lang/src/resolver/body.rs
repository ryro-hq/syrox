use super::{
    BTreeMap, Block, Expected, Expression, ExpressionKind, Function, ItemKind, Literal, LocalId,
    MatchArm, ModuleId, Path, Pattern, ReferenceKind, RefinementKind, ResolvedReference,
    ResolvedTarget, Resolver, StatementKind, StringLiteral, StringPart, Type, TypeKind,
};

impl Resolver<'_> {
    #[allow(clippy::too_many_lines)]
    pub(super) fn resolve_contents(&mut self) {
        for located in self.walk_items() {
            if !self.charge(located.item.span) {
                return;
            }
            let module = self.module_ids[&(located.domain, located.module_path.clone())];
            match &located.item.kind {
                ItemKind::Module(_) | ItemKind::Use(_) | ItemKind::Enum(_) => {}
                ItemKind::Inputs(inputs) => {
                    for input in &inputs.entries {
                        self.resolve_string(&input.value, &BTreeMap::new());
                    }
                }
                ItemKind::TypeAlias(alias) => {
                    self.resolve_type(module, &alias.ty, &BTreeMap::new());
                }
                ItemKind::Struct(structure) => {
                    let mut type_locals = BTreeMap::new();
                    for parameter in &structure.type_parameters {
                        if !self.charge(parameter.span) {
                            return;
                        }
                        if type_locals.contains_key(&parameter.name.text) {
                            self.error("duplicate type parameter", parameter.span);
                            continue;
                        }
                        let Some(id) = self.local(parameter.name.span) else {
                            return;
                        };
                        type_locals.insert(parameter.name.text.clone(), id);
                    }
                    for field in &structure.fields {
                        self.resolve_type(module, &field.ty, &type_locals);
                        if let Some(default) = &field.default {
                            self.resolve_expression(
                                module,
                                default,
                                &BTreeMap::new(),
                                &type_locals,
                            );
                        }
                    }
                }
                ItemKind::Resource(declaration) | ItemKind::Value(declaration) => {
                    for refinement in &declaration.refinements {
                        if !self.charge(refinement.span) {
                            return;
                        }
                        match &refinement.kind {
                            // Predicate names are resolved by the caller's
                            // closed checking policy, not by source lookup.
                            RefinementKind::Predicate(path) => {
                                for segment in &path.segments {
                                    if !self.charge(segment.span) {
                                        return;
                                    }
                                }
                            }
                            RefinementKind::Set(values) => {
                                for value in values {
                                    let span = match value {
                                        Literal::Integer(integer) => integer.span,
                                        Literal::String(string) => string.span,
                                    };
                                    if !self.charge(span) {
                                        return;
                                    }
                                    if let Literal::String(string) = value {
                                        self.resolve_string(string, &BTreeMap::new());
                                    }
                                }
                            }
                            RefinementKind::Range { .. } => {}
                        }
                    }
                }
                ItemKind::Function(function) => self.resolve_function(module, function),
                ItemKind::Outputs(outputs) => {
                    for output in &outputs.entries {
                        if !self.charge(output.span) {
                            return;
                        }
                        match &output.kind {
                            crate::OutputKind::Value { ty, value, .. } => {
                                self.resolve_type(module, ty, &BTreeMap::new());
                                self.resolve_expression(
                                    module,
                                    value,
                                    &BTreeMap::new(),
                                    &BTreeMap::new(),
                                );
                            }
                            crate::OutputKind::Type { ty, .. } => {
                                self.resolve_type(module, ty, &BTreeMap::new());
                            }
                            crate::OutputKind::Function {
                                signature,
                                function,
                                ..
                            } => {
                                for parameter in &signature.parameters {
                                    self.resolve_type(module, parameter, &BTreeMap::new());
                                }
                                self.resolve_type(module, &signature.result, &BTreeMap::new());
                                self.resolve_path(
                                    module,
                                    function,
                                    Expected::Function,
                                    ReferenceKind::Function,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    pub(super) fn resolve_function(&mut self, module: ModuleId, function: &Function) {
        let mut locals = BTreeMap::new();
        for parameter in &function.parameters {
            self.resolve_type(module, &parameter.ty, &BTreeMap::new());
            if locals.contains_key(&parameter.name.text) {
                self.error("duplicate function parameter", parameter.span);
                continue;
            }
            let Some(id) = self.local(parameter.name.span) else {
                return;
            };
            locals.insert(parameter.name.text.clone(), id);
        }
        if let Some(result) = &function.result {
            self.resolve_type(module, result, &BTreeMap::new());
        }
        self.resolve_block(module, &function.body, &mut locals);
    }

    pub(super) fn resolve_block(
        &mut self,
        module: ModuleId,
        block: &Block,
        locals: &mut BTreeMap<String, LocalId>,
    ) {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::Let { name, value } => {
                    self.resolve_expression(module, value, locals, &BTreeMap::new());
                    let Some(id) = self.local(name.span) else {
                        return;
                    };
                    locals.insert(name.text.clone(), id);
                }
                StatementKind::Expression(expression) => {
                    self.resolve_expression(module, expression, locals, &BTreeMap::new());
                }
            }
        }
        if let Some(tail) = &block.tail {
            self.resolve_expression(module, tail, locals, &BTreeMap::new());
        }
    }

    pub(super) fn resolve_type(
        &mut self,
        module: ModuleId,
        root: &Type,
        locals: &BTreeMap<String, LocalId>,
    ) {
        let mut stack = vec![root];
        while let Some(ty) = stack.pop() {
            if !self.charge(ty.span) {
                return;
            }
            match &ty.kind {
                TypeKind::Named { path, arguments } => {
                    if let Some(local) = single_local(path, locals) {
                        self.references.push(ResolvedReference {
                            span: path.span,
                            kind: ReferenceKind::Type,
                            target: ResolvedTarget::Local(local),
                        });
                    } else {
                        self.resolve_path(module, path, Expected::Type, ReferenceKind::Type);
                    }
                    stack.extend(arguments.iter().rev());
                }
                TypeKind::List(path) => {
                    if let Some(local) = single_local(path, locals) {
                        self.references.push(ResolvedReference {
                            span: path.span,
                            kind: ReferenceKind::Type,
                            target: ResolvedTarget::Local(local),
                        });
                    } else {
                        self.resolve_path(module, path, Expected::Type, ReferenceKind::Type);
                    }
                }
            }
        }
    }

    pub(super) fn resolve_expression(
        &mut self,
        module: ModuleId,
        root: &Expression,
        value_locals: &BTreeMap<String, LocalId>,
        type_locals: &BTreeMap<String, LocalId>,
    ) {
        let mut stack = vec![root];
        while let Some(expression) = stack.pop() {
            if !self.charge(expression.span) {
                return;
            }
            match &expression.kind {
                ExpressionKind::Integer(_) => {}
                ExpressionKind::String(string) => {
                    self.resolve_string(string, value_locals);
                }
                ExpressionKind::Path(path) => {
                    if let Some(local) = single_local(path, value_locals) {
                        self.references.push(ResolvedReference {
                            span: path.span,
                            kind: ReferenceKind::Value,
                            target: ResolvedTarget::Local(local),
                        });
                    } else if path.segments.len() == 1 {
                        if self.lookup(module, path, Expected::Value).is_some() {
                            self.resolve_path(module, path, Expected::Value, ReferenceKind::Value);
                        } else {
                            self.references.push(ResolvedReference {
                                span: path.span,
                                kind: ReferenceKind::Value,
                                target: ResolvedTarget::ContextualEnumVariant,
                            });
                        }
                    } else {
                        if path
                            .segments
                            .first()
                            .is_some_and(|segment| value_locals.contains_key(&segment.text))
                        {
                            self.error("local value cannot qualify a `::` path", path.span);
                            continue;
                        }
                        self.resolve_qualified_value(module, path, ReferenceKind::Value);
                    }
                }
                ExpressionKind::Call { callee, arguments } => {
                    self.resolve_path(module, callee, Expected::Function, ReferenceKind::Function);
                    stack.extend(arguments.iter().rev());
                }
                ExpressionKind::Struct {
                    path,
                    type_arguments,
                    fields,
                } => {
                    self.resolve_path(
                        module,
                        path,
                        Expected::Struct,
                        ReferenceKind::StructConstructor,
                    );
                    for ty in type_arguments {
                        self.resolve_type(module, ty, type_locals);
                    }
                    stack.extend(fields.iter().rev().map(|field| &field.value));
                }
                ExpressionKind::List(items) | ExpressionKind::Concat(items) => {
                    stack.extend(items.iter().rev());
                }
                ExpressionKind::Erase { ty, value } => {
                    self.resolve_type(module, ty, type_locals);
                    stack.push(value);
                }
                ExpressionKind::Match { value, arms } => {
                    stack.push(value);
                    for MatchArm { pattern, value, .. } in arms.iter().rev() {
                        match pattern {
                            Pattern::Wildcard(_) => {}
                            Pattern::Path(path) if path.segments.len() == 1 => {
                                self.references.push(ResolvedReference {
                                    span: path.span,
                                    kind: ReferenceKind::Pattern,
                                    target: ResolvedTarget::ContextualEnumVariant,
                                });
                            }
                            Pattern::Path(path) => {
                                self.resolve_qualified_value(module, path, ReferenceKind::Pattern);
                            }
                        }
                        stack.push(value);
                    }
                }
                ExpressionKind::Group(inner) | ExpressionKind::Field { value: inner, .. } => {
                    stack.push(inner);
                }
            }
        }
    }

    pub(super) fn resolve_qualified_value(
        &mut self,
        module: ModuleId,
        path: &Path,
        kind: ReferenceKind,
    ) {
        if let Some((enumeration, index)) = self.lookup_variant(module, path) {
            self.references.push(ResolvedReference {
                span: path.span,
                kind,
                target: ResolvedTarget::EnumVariant { enumeration, index },
            });
        } else {
            self.resolve_path(module, path, Expected::Value, kind);
        }
    }

    pub(super) fn resolve_string(
        &mut self,
        string: &StringLiteral,
        locals: &BTreeMap<String, LocalId>,
    ) {
        for part in &string.parts {
            let StringPart::Interpolation { path, .. } = part else {
                continue;
            };
            if !self.charge(path.span) {
                return;
            }
            if let Some(local) = first_local(path, locals) {
                self.references.push(ResolvedReference {
                    span: path.span,
                    kind: ReferenceKind::Interpolation,
                    target: ResolvedTarget::Local(local),
                });
            } else {
                self.error("unknown local in string interpolation", path.span);
            }
        }
    }

    pub(super) fn resolve_path(
        &mut self,
        module: ModuleId,
        path: &Path,
        expected: Expected,
        kind: ReferenceKind,
    ) {
        if !self.charge(path.span) {
            return;
        }
        match self.lookup(module, path, expected) {
            Some(target) => self.references.push(ResolvedReference {
                span: path.span,
                kind,
                target,
            }),
            None => self.unknown(expected.description(), path),
        }
    }
}

fn single_local(path: &Path, locals: &BTreeMap<String, LocalId>) -> Option<LocalId> {
    (path.segments.len() == 1)
        .then(|| locals.get(&path.segments[0].text).copied())
        .flatten()
}

fn first_local(path: &Path, locals: &BTreeMap<String, LocalId>) -> Option<LocalId> {
    path.segments
        .first()
        .and_then(|segment| locals.get(&segment.text).copied())
}
