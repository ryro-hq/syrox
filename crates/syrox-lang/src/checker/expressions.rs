use super::types::{nominal_enum, nominal_head, raw_from_ty};
use super::{
    BTreeMap, BTreeSet, Binding, Block, CheckedExpression, CheckedPattern, Checker, Context,
    Elaboration, Expression, ExpressionKind, Item, ItemId, ItemKind, MatchArm, ModuleId,
    OutputKind, ParsedSource, Path, Pattern, Primitive, ResolvedTarget, Span, StatementKind,
    StringLiteral, StringPart, StructField, StructInfo, Ty, Type,
};

impl Checker<'_> {
    pub(super) fn check_defaults(&mut self) {
        let structures: Vec<_> = self
            .structs
            .iter()
            .map(|(&id, info)| (id, info.clone()))
            .collect();
        for (id, info) in structures {
            let args: BTreeMap<_, _> = info
                .parameters
                .iter()
                .copied()
                .map(|parameter| (parameter, Ty::Parameter(parameter)))
                .collect();
            self.type_parameters = args.clone();
            for field in &info.declaration.fields {
                if !self.charge(field.span) {
                    return;
                }
                if let Some(default) = &field.default {
                    self.bindings.clear();
                    let want = self.resolve_type(&field.ty, &args, 0);
                    let got = self.check_expr(default, Some(&want), info.context, true);
                    self.expect_same(&want, &got, default.span, "struct field default");
                }
            }
            self.type_parameters.clear();
            let _ = id;
        }
    }

    pub(super) fn check_functions(&mut self) {
        let functions: Vec<_> = self
            .functions
            .iter()
            .map(|(&id, info)| (id, *info))
            .collect();
        for (id, info) in functions {
            self.bindings.clear();
            let Some((parameter_types, result)) = self.function_types.get(&id).cloned() else {
                continue;
            };
            for (parameter, ty) in info.declaration.parameters.iter().zip(parameter_types) {
                if let Some(local) = self.local_id(parameter.name.span) {
                    let affine = self.affine(&ty, parameter.name.span);
                    self.bindings.insert(
                        local,
                        Binding {
                            affine,
                            ty,
                            moved: None,
                            declaration: parameter.name.span,
                        },
                    );
                }
            }
            let got = self.check_block(&info.declaration.body, Some(&result), info.context);
            self.expect_same(&result, &got, info.declaration.body.span, "function return");
        }
    }

    pub(super) fn check_block(
        &mut self,
        block: &Block,
        expected: Option<&Ty>,
        context: Context,
    ) -> Ty {
        for statement in &block.statements {
            if !self.charge(statement.span) {
                return Ty::Error;
            }
            match &statement.kind {
                StatementKind::Let { name, value } => {
                    let ty = self.check_expr(value, None, context, true);
                    if let Some(local) = self.local_id(name.span) {
                        let affine = self.affine(&ty, name.span);
                        self.bindings.insert(
                            local,
                            Binding {
                                affine,
                                ty,
                                moved: None,
                                declaration: name.span,
                            },
                        );
                    }
                }
                StatementKind::Expression(expression) => {
                    self.check_expr(expression, None, context, true);
                }
            }
        }
        block.tail.as_ref().map_or(Ty::Unit, |tail| {
            self.check_expr(tail, expected, context, true)
        })
    }

    pub(super) fn check_outputs(&mut self) {
        let program = self.program;
        for source in program.parsed().iter() {
            self.walk_outputs(source);
        }
    }

    pub(super) fn walk_outputs(&mut self, source: &ParsedSource) {
        let root = self
            .module_by_path
            .get(&source.domain())
            .and_then(|modules| modules.get(&[][..]))
            .expect("root module")
            .to_owned();
        let mut stack: Vec<(&Item, Vec<String>, ModuleId)> = source
            .program()
            .items
            .iter()
            .rev()
            .map(|item| (item, Vec::new(), root))
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
            } else if let ItemKind::Outputs(outputs) = &item.kind {
                let context = Context {
                    domain: source.domain(),
                    module,
                };
                for output in &outputs.entries {
                    if !self.charge(output.span) {
                        return;
                    }
                    match &output.kind {
                        OutputKind::Value { ty, value, .. } => {
                            self.bindings.clear();
                            let want = self.resolve_type(ty, &BTreeMap::new(), 0);
                            let got = self.check_expr(value, Some(&want), context, true);
                            self.expect_same(&want, &got, value.span, "output value");
                        }
                        OutputKind::Type { ty, .. } => {
                            self.resolve_type(ty, &BTreeMap::new(), 0);
                        }
                        OutputKind::Function {
                            signature,
                            function,
                            ..
                        } => {
                            let Some(ResolvedTarget::Item(target)) = self.target(function.span)
                            else {
                                continue;
                            };
                            let target = *target;
                            let Some((parameters, result)) =
                                self.function_types.get(&target).cloned()
                            else {
                                continue;
                            };
                            let wanted: Vec<_> = signature
                                .parameters
                                .iter()
                                .map(|ty| self.resolve_type(ty, &BTreeMap::new(), 0))
                                .collect();
                            let wanted_result =
                                self.resolve_type(&signature.result, &BTreeMap::new(), 0);
                            if parameters != wanted || result != wanted_result {
                                self.error(
                                    "exported function signature does not exactly match",
                                    output.span,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    pub(super) fn check_expr(
        &mut self,
        expression: &Expression,
        expected: Option<&Ty>,
        context: Context,
        consuming: bool,
    ) -> Ty {
        if !self.charge(expression.span) {
            return Ty::Error;
        }
        let mut elaboration = None;
        let ty = match &expression.kind {
            ExpressionKind::Integer(_) => self.lift_literal(Ty::Int, expected, &mut elaboration),
            ExpressionKind::String(string) => {
                self.check_string(string, context);
                self.lift_literal(Ty::Str, expected, &mut elaboration)
            }
            ExpressionKind::Path(path) => {
                self.check_path_value(path, expected, consuming, &mut elaboration)
            }
            ExpressionKind::Call { callee, arguments } => {
                self.check_call(callee, arguments, context)
            }
            ExpressionKind::Struct {
                path,
                type_arguments,
                fields,
            } => self.check_struct_literal(path, type_arguments, fields, context),
            ExpressionKind::List(items) => {
                self.check_list(items, expected, context, expression.span)
            }
            ExpressionKind::Erase { ty, value } => {
                let result = self.check_erase(ty, value, context);
                if !matches!(result, Ty::Error) {
                    elaboration = Some(Elaboration::Erasure {
                        source: self.expression_ty(value.span),
                    });
                }
                result
            }
            ExpressionKind::Match { value, arms } => {
                self.check_match(value, arms, expected, context)
            }
            ExpressionKind::Group(inner) => self.check_expr(inner, expected, context, consuming),
            ExpressionKind::Field { value, fields } => {
                let mut ty = self.check_expr(value, None, context, true);
                for field in fields {
                    ty = self.project(&ty, &field.text, field.span);
                }
                ty
            }
            ExpressionKind::Concat(parts) => {
                let mut result: Option<Ty> = None;
                for part in parts {
                    let got = self.check_expr(part, expected.or(result.as_ref()), context, true);
                    if !matches!(got, Ty::List(_)) {
                        self.error("`++` operands must be lists", part.span);
                    }
                    if let Some(want) = &result {
                        self.expect_same(want, &got, part.span, "list concatenation");
                    } else {
                        result = Some(got);
                    }
                }
                result.unwrap_or(Ty::Error)
            }
        };
        if self.reserve_metadata(expression.span) {
            self.expressions.push(CheckedExpression {
                span: expression.span,
                ty: ty.clone(),
                elaboration,
            });
        }
        ty
    }

    pub(super) fn check_path_value(
        &mut self,
        path: &Path,
        expected: Option<&Ty>,
        consuming: bool,
        elaboration: &mut Option<Elaboration>,
    ) -> Ty {
        match self.target(path.span).cloned() {
            Some(ResolvedTarget::Local(local)) => self.use_local(local, path.span, consuming),
            Some(ResolvedTarget::EnumVariant { enumeration, index }) => {
                Ty::Nominal(if self.variant_exists(enumeration, index, path.span) {
                    enumeration
                } else {
                    return Ty::Error;
                })
            }
            Some(ResolvedTarget::ContextualEnumVariant) => {
                let Some(enumeration) = expected.and_then(nominal_enum) else {
                    self.error(
                        "bare enum variant requires an expected enum type",
                        path.span,
                    );
                    return Ty::Error;
                };
                let name = path.segments.last().map_or("", |segment| &segment.text);
                let Some(index) = self.variant_index(enumeration, name) else {
                    self.error(format!("enum has no variant `{name}`"), path.span);
                    return Ty::Error;
                };
                *elaboration = Some(Elaboration::ContextualVariant { enumeration, index });
                Ty::Nominal(enumeration)
            }
            Some(ResolvedTarget::Item(item)) => {
                self.error(
                    "a declaration is not a value; call its constructor",
                    path.span,
                );
                Ty::Nominal(item)
            }
            _ => Ty::Error,
        }
    }

    pub(super) fn check_call(
        &mut self,
        callee: &Path,
        arguments: &[Expression],
        context: Context,
    ) -> Ty {
        let Some(ResolvedTarget::Item(item)) = self.target(callee.span).cloned() else {
            for argument in arguments {
                self.check_expr(argument, None, context, true);
            }
            return Ty::Error;
        };
        if let Some((parameters, result)) = self.function_types.get(&item).cloned() {
            self.check_arguments(arguments, &parameters, context, callee.span);
            return result;
        }
        let Some(primitive) = self.primitives.get(&item).copied() else {
            self.error("callee is not callable", callee.span);
            return Ty::Error;
        };
        let inner = match primitive.declaration.primitive {
            Primitive::Int => Ty::Int,
            Primitive::Str => Ty::Str,
        };
        self.check_arguments(arguments, &[inner], context, callee.span);
        Ty::Nominal(item)
    }

    pub(super) fn check_arguments(
        &mut self,
        arguments: &[Expression],
        parameters: &[Ty],
        context: Context,
        span: Span,
    ) {
        if arguments.len() != parameters.len() {
            self.error(
                format!(
                    "expected {} argument(s), found {}",
                    parameters.len(),
                    arguments.len()
                ),
                span,
            );
        }
        for (index, argument) in arguments.iter().enumerate() {
            let expected = parameters.get(index);
            let got = self.check_expr(argument, expected, context, true);
            if let Some(want) = expected {
                self.expect_same(want, &got, argument.span, "call argument");
            }
        }
    }

    pub(super) fn check_struct_literal(
        &mut self,
        path: &Path,
        type_arguments: &[Type],
        fields: &[StructField],
        context: Context,
    ) -> Ty {
        let Some(ResolvedTarget::Item(item)) = self.target(path.span).cloned() else {
            return Ty::Error;
        };
        let raw_parameters: BTreeMap<_, _> = self
            .type_parameters
            .iter()
            .map(|(&parameter, ty)| (parameter, raw_from_ty(ty.clone())))
            .collect();
        let raw = self.expand_item_type(item, type_arguments, &raw_parameters, 0, path.span);
        let ty = self
            .concretize(raw, &BTreeMap::new(), path.span)
            .unwrap_or(Ty::Error);
        self.validate_concrete_type(&ty, path.span);
        self.track_generic_instances(&ty, path.span);
        let Some(info) = self.structs.get(&item).cloned() else {
            return Ty::Error;
        };
        self.check_opaque_authority(item, &ty, &info, context, path.span);
        let canonical = self.item_identity(item);
        if self
            .policy
            .erasures
            .iter()
            .any(|rule| rule.target == canonical)
        {
            self.error(
                "an erasure target has no public literal constructor",
                path.span,
            );
        }
        let substitutions = self.specialization_substitutions(item, &ty);
        let declared: BTreeMap<_, _> = info
            .declaration
            .fields
            .iter()
            .map(|field| {
                (
                    field.name.text.clone(),
                    (
                        field.clone(),
                        self.resolve_type(&field.ty, &substitutions, 1),
                    ),
                )
            })
            .collect();
        let mut seen = BTreeSet::new();
        for field in fields {
            if !seen.insert(field.name.text.clone()) {
                self.error(
                    format!("field `{}` is given twice", field.name.text),
                    field.name.span,
                );
            }
            if let Some((declaration, want)) = declared.get(&field.name.text) {
                let got = self.check_expr(&field.value, Some(want), context, true);
                self.expect_same(want, &got, field.value.span, "struct field");
                let _ = declaration;
            } else {
                self.check_expr(&field.value, None, context, true);
                self.error(
                    format!("unknown field `{}`", field.name.text),
                    field.name.span,
                );
            }
        }
        for (name, (field, _)) in declared {
            if field.default.is_none() && !seen.contains(&name) {
                self.error(format!("missing field `{name}`"), path.span);
            }
        }
        ty
    }

    pub(super) fn check_opaque_authority(
        &mut self,
        item: ItemId,
        ty: &Ty,
        info: &StructInfo<'_>,
        context: Context,
        span: Span,
    ) {
        if !info.declaration.opaque {
            return;
        }
        let lexical = context.domain == info.context.domain
            && self.module_is_descendant(context.module, info.context.module);
        let delegated = info
            .declaration
            .type_parameters
            .iter()
            .position(|parameter| parameter.owner)
            .and_then(|index| match ty {
                Ty::Specialization { arguments, .. } => arguments.get(index),
                _ => None,
            })
            .and_then(nominal_head)
            .and_then(|owner| self.item_context.get(&owner).copied())
            .is_some_and(|owner| {
                owner.domain == info.context.domain
                    && context.domain == owner.domain
                    && self.module_is_descendant(context.module, owner.module)
            });
        if !lexical && !delegated {
            self.error(
                "opaque struct constructor is private in this source domain",
                span,
            );
        }
        let _ = item;
    }

    pub(super) fn check_list(
        &mut self,
        items: &[Expression],
        expected: Option<&Ty>,
        context: Context,
        span: Span,
    ) -> Ty {
        let expected_element = match expected {
            Some(Ty::List(element)) => Some(element.as_ref()),
            _ => None,
        };
        if items.is_empty() {
            return expected
                .cloned()
                .filter(|ty| matches!(ty, Ty::List(_)))
                .unwrap_or_else(|| {
                    self.error("an empty list requires a contextual list type", span);
                    Ty::Error
                });
        }
        let mut element: Option<Ty> = expected_element.cloned();
        for item in items {
            let got = self.check_expr(item, element.as_ref(), context, true);
            if matches!(got, Ty::Unit | Ty::List(_)) {
                self.error("lists cannot contain lists or `unit`", item.span);
            }
            if let Some(want) = &element {
                self.expect_same(want, &got, item.span, "list element");
            } else {
                element = Some(got);
            }
        }
        Ty::List(Box::new(element.unwrap_or(Ty::Error)))
    }

    pub(super) fn check_erase(
        &mut self,
        target: &Type,
        value: &Expression,
        context: Context,
    ) -> Ty {
        let target_ty = self.resolve_type(target, &BTreeMap::new(), 0);
        let source_ty = self.check_expr(value, None, context, true);
        let Ty::Specialization { template, .. } = &source_ty else {
            self.error(
                "only a generic struct specialization can be erased",
                value.span,
            );
            return Ty::Error;
        };
        let Some(target_item) = nominal_head(&target_ty) else {
            self.error("erasure target must be a non-generic struct", target.span);
            return Ty::Error;
        };
        let source_name = self.item_identity(*template);
        let target_name = self.item_identity(target_item);
        if !self
            .policy
            .erasures
            .iter()
            .any(|rule| rule.source == source_name && rule.target == target_name)
        {
            self.error("erasure is not allowed by the checking policy", target.span);
            return Ty::Error;
        }
        if !matches!(target_ty, Ty::Nominal(_)) {
            self.error("erasure target must be non-generic", target.span);
            return Ty::Error;
        }
        let Some(target_info) = self.structs.get(&target_item).cloned() else {
            self.error("erasure target must be a struct", target.span);
            return Ty::Error;
        };
        if !target_info.declaration.scopes.is_empty()
            || target_info
                .declaration
                .fields
                .iter()
                .any(|field| field.default.is_some())
        {
            self.error("erasure target cannot have scopes or defaults", target.span);
        }
        let source_fields = self.fields_of(&source_ty);
        for field in &target_info.declaration.fields {
            let wanted = self.resolve_type(&field.ty, &BTreeMap::new(), 0);
            if !source_fields
                .iter()
                .any(|(name, ty)| name == &field.name.text && ty == &wanted)
            {
                self.error(
                    format!(
                        "erasure target field `{}` is not an exact source field",
                        field.name.text
                    ),
                    field.name.span,
                );
            }
        }
        target_ty
    }

    pub(super) fn check_match(
        &mut self,
        value: &Expression,
        arms: &[MatchArm],
        expected: Option<&Ty>,
        context: Context,
    ) -> Ty {
        let scrutinee = self.check_expr(value, None, context, true);
        let Some(enumeration) = nominal_enum(&scrutinee) else {
            self.error("only an enum can be matched", value.span);
            return Ty::Error;
        };
        if !self.enums.contains_key(&enumeration) {
            self.error("only an enum can be matched", value.span);
            return Ty::Error;
        }
        let mut covered = BTreeSet::new();
        let mut wildcard = false;
        let baseline = self.bindings.clone();
        let mut merged = baseline.clone();
        let mut result = expected.cloned();
        for arm in arms {
            if !self.charge(arm.span) {
                return Ty::Error;
            }
            if wildcard {
                self.error("match arm is unreachable after wildcard", arm.span);
            }
            match &arm.pattern {
                Pattern::Wildcard(_) => wildcard = true,
                Pattern::Path(path) => {
                    let index = match self.target(path.span).cloned() {
                        Some(ResolvedTarget::EnumVariant {
                            enumeration: actual,
                            index,
                        }) if actual == enumeration => Some(index),
                        Some(ResolvedTarget::ContextualEnumVariant) => path
                            .segments
                            .last()
                            .and_then(|name| self.variant_index(enumeration, &name.text)),
                        Some(ResolvedTarget::EnumVariant { .. }) => {
                            self.error("match pattern belongs to a different enum", path.span);
                            None
                        }
                        _ => None,
                    };
                    if let Some(index) = index {
                        if self.reserve_metadata(path.span) {
                            self.patterns.push(CheckedPattern {
                                span: path.span,
                                enumeration,
                                index,
                            });
                        }
                        if !covered.insert(index) {
                            self.error("enum variant is covered more than once", path.span);
                        }
                    } else {
                        let name = path.segments.last().map_or("", |name| &name.text);
                        self.error(format!("enum has no variant `{name}`"), path.span);
                    }
                }
            }
            self.bindings = baseline.clone();
            let got = self.check_expr(&arm.value, result.as_ref(), context, true);
            if let Some(want) = &result {
                self.expect_same(want, &got, arm.value.span, "match arm");
            } else {
                result = Some(got);
            }
            for (local, binding) in &self.bindings {
                if binding.moved.is_some()
                    && let Some(joined) = merged.get_mut(local)
                    && joined.moved.is_none()
                {
                    joined.moved = binding.moved;
                }
            }
        }
        self.bindings = merged;
        if !wildcard {
            let count = self
                .enums
                .get(&enumeration)
                .map_or(0, |item| item.variants.len());
            if covered.len() != count {
                self.error("match is not exhaustive", value.span);
            }
        }
        result.unwrap_or(Ty::Error)
    }

    pub(super) fn check_string(&mut self, string: &StringLiteral, _context: Context) {
        for part in &string.parts {
            let StringPart::Interpolation { path, .. } = part else {
                continue;
            };
            let Some(ResolvedTarget::Local(local)) = self.target(path.span).cloned() else {
                continue;
            };
            let mut ty = self.use_local(local, path.span, true);
            for field in path.segments.iter().skip(1) {
                ty = self.project(&ty, &field.text, field.span);
            }
            let wrapper = match ty {
                Ty::Nominal(item) => self.primitives.contains_key(&item),
                _ => false,
            };
            if !matches!(ty, Ty::Int | Ty::Str | Ty::Error) && !wrapper {
                self.error(
                    "interpolation requires a primitive or primitive wrapper",
                    path.span,
                );
            }
        }
    }
}
