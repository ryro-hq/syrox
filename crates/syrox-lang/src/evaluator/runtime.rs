use std::{
    collections::BTreeMap,
    fmt::Write as _,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use super::{
    CanonicalType, EvaluationEnvironment, EvaluationLimits, GoverningScope, PrimitiveValue,
    ResourceClaim, ResourceClaimKey, Value, environment::PredicateCallback, index::ProgramIndex,
    span_key, types::nominal_head,
};
use crate::{
    Block, CanonicalItemIdentity, CheckPolicy, CheckedProgram, Diagnostic, Elaboration,
    EvaluationSetupError, Expression, ExpressionKind, ItemId, Literal, LocalId, MatchArm, Pattern,
    PrimitiveDeclaration, RefinementKind, ResolvedTarget, ScopeRule, Span, StatementKind,
    StringLiteral, StringPart, Ty,
};

#[derive(Clone)]
struct OpenScope {
    name: Option<Arc<str>>,
    boundary: u64,
}

pub(super) struct Halt;
pub(super) type Eval<T> = Result<T, Halt>;

pub(super) struct Evaluator<'a> {
    pub(super) checked: &'a CheckedProgram,
    policy: &'a CheckPolicy,
    environment: &'a EvaluationEnvironment,
    pub(super) index: &'a ProgramIndex<'a>,
    pub(super) limits: EvaluationLimits,
    root: Arc<CanonicalItemIdentity>,
    pub(super) diagnostics: Vec<Diagnostic>,
    locals: BTreeMap<LocalId, Value>,
    scopes: Vec<OpenScope>,
    pub(super) claims: BTreeMap<ResourceClaimKey, Span>,
    pub(super) canonical_types: BTreeMap<Ty, Arc<CanonicalType>>,
    next_boundary: u64,
    pub(super) steps: usize,
    depth: usize,
    pub(super) expansion: usize,
    pub(super) operation_error: Option<EvaluationSetupError>,
}

impl<'a> Evaluator<'a> {
    pub(super) fn new(
        checked: &'a CheckedProgram,
        policy: &'a CheckPolicy,
        environment: &'a EvaluationEnvironment,
        index: &'a ProgramIndex<'a>,
        limits: EvaluationLimits,
        span: Span,
        root: Arc<CanonicalItemIdentity>,
    ) -> Self {
        let scope_count = policy.scopes().len().saturating_add(1);
        let scope_bytes = scope_count.saturating_mul(size_of::<OpenScope>());
        let scopes_fit = scope_bytes <= limits.max_expansion_bytes;
        let mut evaluator = Self {
            checked,
            policy,
            environment,
            index,
            limits,
            root,
            diagnostics: Vec::new(),
            locals: BTreeMap::new(),
            scopes: if scopes_fit {
                Vec::with_capacity(scope_count)
            } else {
                Vec::new()
            },
            claims: BTreeMap::new(),
            canonical_types: BTreeMap::new(),
            next_boundary: 0,
            steps: 0,
            depth: 0,
            expansion: scope_bytes,
            operation_error: None,
        };
        if !scopes_fit {
            evaluator.error(span, "evaluation expansion byte limit reached");
            return evaluator;
        }
        if scope_bytes > limits.max_retained_expansion_bytes {
            evaluator.operation_error =
                Some(EvaluationSetupError::EvaluationRetainedExpansionLimit);
            return evaluator;
        }
        evaluator.push_scope(None, span);
        for (name, _) in policy.scopes() {
            let scope = index.scope_name(name).map_or_else(
                || {
                    evaluator.error(span, "policy scope identity is unavailable");
                    None
                },
                Some,
            );
            evaluator.push_scope(scope, span);
        }
        evaluator
    }

    fn push_scope(&mut self, name: Option<Arc<str>>, span: Span) {
        let boundary = self.next_boundary;
        if let Some(next) = self.next_boundary.checked_add(1) {
            self.next_boundary = next;
            self.scopes.push(OpenScope { name, boundary });
        } else {
            self.error(span, "scope boundary identity overflow");
        }
    }

    fn tick(&mut self, span: Span) -> Eval<()> {
        self.charge_steps(1, span)
    }

    fn charge_steps(&mut self, amount: usize, span: Span) -> Eval<()> {
        self.steps = self.steps.saturating_add(amount);
        if self.steps > self.limits.max_total_steps {
            self.operation_error = Some(EvaluationSetupError::EvaluationWorkLimit);
            return Err(Halt);
        }
        if self.steps > self.limits.max_steps_per_root {
            return self.fail(span, "evaluation step limit reached");
        }
        Ok(())
    }

    pub(super) fn expand(&mut self, bytes: usize, span: Span) -> Eval<()> {
        self.expansion = self.expansion.saturating_add(bytes);
        if self.expansion > self.limits.max_retained_expansion_bytes {
            self.operation_error = Some(EvaluationSetupError::EvaluationRetainedExpansionLimit);
            return Err(Halt);
        }
        if self.expansion > self.limits.max_expansion_bytes {
            return self.fail(span, "evaluation expansion byte limit reached");
        }
        Ok(())
    }

    pub(super) fn fail<T>(&mut self, span: Span, message: impl Into<String>) -> Eval<T> {
        self.error(span, message);
        Err(Halt)
    }

    pub(super) fn error(&mut self, span: Span, message: impl Into<String>) {
        if self.diagnostics.len() < self.limits.max_diagnostics {
            self.diagnostics.push(Diagnostic::error(message, span));
        }
    }

    pub(super) fn expression(
        &mut self,
        expression: &Expression,
        substitutions: &BTreeMap<LocalId, Ty>,
    ) -> Eval<Value> {
        self.tick(expression.span)?;
        if self.depth >= self.limits.max_depth {
            return self.fail(expression.span, "evaluation/value depth limit reached");
        }
        self.expand(size_of::<Value>(), expression.span)?;
        self.depth += 1;
        let result = self.expression_inner(expression, substitutions);
        self.depth -= 1;
        result
    }

    #[allow(clippy::too_many_lines)]
    fn expression_inner(
        &mut self,
        expression: &Expression,
        substitutions: &BTreeMap<LocalId, Ty>,
    ) -> Eval<Value> {
        let metadata = self.checked.expression(expression.span).ok_or_else(|| {
            self.error(expression.span, "missing checked expression metadata");
            Halt
        })?;
        match &expression.kind {
            ExpressionKind::Integer(integer) => {
                let value = PrimitiveValue::Int(integer.value);
                if let Some(Elaboration::ValueLiteral(item)) = metadata.elaboration() {
                    self.construct_primitive(*item, value, expression.span, false)
                } else {
                    Ok(Value::Int(integer.value))
                }
            }
            ExpressionKind::String(string) => {
                let text = self.string(string, substitutions)?;
                if let Some(Elaboration::ValueLiteral(item)) = metadata.elaboration() {
                    self.construct_primitive(
                        *item,
                        PrimitiveValue::Str(text),
                        expression.span,
                        false,
                    )
                } else {
                    Ok(Value::Str(text))
                }
            }
            ExpressionKind::Path(path) => match metadata.elaboration() {
                Some(Elaboration::ContextualVariant { enumeration, index }) => Ok(Value::Variant {
                    ty: self.canonical_ty(&Ty::Nominal(*enumeration), substitutions, path.span)?,
                    index: *index,
                }),
                _ => match self.index.target(path.span) {
                    Some(ResolvedTarget::Local(local)) => self.local_value(*local, path.span),
                    Some(ResolvedTarget::EnumVariant { enumeration, index }) => {
                        Ok(Value::Variant {
                            ty: self.canonical_ty(
                                &Ty::Nominal(*enumeration),
                                substitutions,
                                path.span,
                            )?,
                            index: *index,
                        })
                    }
                    _ => self.fail(path.span, "checked value path has no evaluable target"),
                },
            },
            ExpressionKind::Call { callee, arguments } => {
                self.call(callee.span, arguments, substitutions, expression.span)
            }
            ExpressionKind::Struct { fields, .. } => {
                let checked_ty =
                    self.substitute_ty(metadata.ty(), substitutions, expression.span)?;
                let item = nominal_head(&checked_ty).ok_or_else(|| {
                    self.error(
                        expression.span,
                        "checked struct expression has no nominal type",
                    );
                    Halt
                })?;
                self.structure(item, &checked_ty, fields, substitutions, expression.span)
            }
            ExpressionKind::List(items) => {
                self.expand(
                    items.len().saturating_mul(size_of::<Value>()),
                    expression.span,
                )?;
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.expression(item, substitutions)?);
                }
                Ok(Value::List {
                    ty: self.canonical_ty(metadata.ty(), substitutions, expression.span)?,
                    items: values,
                })
            }
            ExpressionKind::Erase { value, .. } => {
                let Some(Elaboration::Erasure { source }) = metadata.elaboration() else {
                    return self.fail(expression.span, "missing checked erasure metadata");
                };
                let source_ty = self.canonical_ty(source, substitutions, expression.span)?;
                let target_ty = self.canonical_ty(metadata.ty(), substitutions, expression.span)?;
                let target_item = nominal_head(metadata.ty()).ok_or_else(|| {
                    self.error(expression.span, "checked erasure target is not nominal");
                    Halt
                })?;
                let inner = self.expression(value, substitutions)?;
                self.erase(inner, &source_ty, target_ty, target_item, expression.span)
            }
            ExpressionKind::Match { value, arms } => {
                let scrutinee = self.expression(value, substitutions)?;
                let Value::Variant { ty, index } = scrutinee else {
                    return self.fail(value.span, "checked match produced a non-variant value");
                };
                for arm in arms {
                    if self.pattern_matches(arm, &ty, index)? {
                        return self.expression(&arm.value, substitutions);
                    }
                }
                self.fail(expression.span, "checked match selected no arm")
            }
            ExpressionKind::Group(inner) => self.expression(inner, substitutions),
            ExpressionKind::Field { value, fields } => {
                let mut current = self.expression(value, substitutions)?;
                for field in fields {
                    current = self.project(current, &field.text, field.span)?;
                }
                Ok(current)
            }
            ExpressionKind::Concat(parts) => {
                let mut result = Vec::new();
                for part in parts {
                    let Value::List { items: values, .. } = self.expression(part, substitutions)?
                    else {
                        return self.fail(part.span, "checked concatenation produced a non-list");
                    };
                    self.expand(values.len().saturating_mul(size_of::<Value>()), part.span)?;
                    result.extend(values);
                }
                Ok(Value::List {
                    ty: self.canonical_ty(metadata.ty(), substitutions, expression.span)?,
                    items: result,
                })
            }
        }
    }

    fn call(
        &mut self,
        callee_span: Span,
        arguments: &[Expression],
        substitutions: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        let Some(ResolvedTarget::Item(item)) = self.index.target(callee_span).cloned() else {
            return self.fail(callee_span, "checked call has no item target");
        };
        self.expand(arguments.len().saturating_mul(size_of::<Value>()), span)?;
        let mut values = Vec::with_capacity(arguments.len());
        for argument in arguments {
            values.push(self.expression(argument, substitutions)?);
        }
        if self.index.primitives.contains_key(&item) {
            if values.len() != 1 {
                return self.fail(span, "primitive constructor has invalid checked arity");
            }
            let primitive = match values.pop().expect("one value") {
                Value::Int(value) => PrimitiveValue::Int(value),
                Value::Str(value) => PrimitiveValue::Str(value),
                _ => {
                    return self.fail(span, "primitive constructor received a non-primitive value");
                }
            };
            return self.construct_primitive(item, primitive, callee_span, true);
        }
        let Some(function) = self.index.functions.get(&item).copied() else {
            return self.fail(callee_span, "checked function target is unavailable");
        };
        if values.len() != function.parameters.len() {
            return self.fail(span, "function call has invalid checked arity");
        }
        self.expand(
            values.len().saturating_mul(
                size_of::<(LocalId, Option<Value>)>().saturating_add(size_of::<(LocalId, Value)>()),
            ),
            span,
        )?;
        let mut bound = Vec::with_capacity(values.len());
        for (parameter, value) in function.parameters.iter().zip(values) {
            let Some(local) = self.index.local(parameter.name.span) else {
                return self.fail(
                    parameter.name.span,
                    "missing checked function parameter identity",
                );
            };
            bound.push((local, self.locals.insert(local, value)));
        }
        let result = self.block(&function.body, &BTreeMap::new());
        for (local, previous) in bound {
            self.locals.remove(&local);
            if let Some(previous) = previous {
                self.locals.insert(local, previous);
            }
        }
        result
    }

    fn block(&mut self, block: &Block, substitutions: &BTreeMap<LocalId, Ty>) -> Eval<Value> {
        let mut declared = Vec::new();
        let result = (|| {
            for statement in &block.statements {
                self.tick(statement.span)?;
                match &statement.kind {
                    StatementKind::Let { name, value } => {
                        let value = self.expression(value, substitutions)?;
                        let Some(local) = self.index.local(name.span) else {
                            return self.fail(name.span, "missing checked let identity");
                        };
                        self.expand(
                            size_of::<(LocalId, Option<Value>)>().saturating_add(size_of::<(
                                LocalId,
                                Value,
                            )>(
                            )),
                            name.span,
                        )?;
                        declared.push((local, self.locals.insert(local, value)));
                    }
                    StatementKind::Expression(expression) => {
                        let _ = self.expression(expression, substitutions)?;
                    }
                }
            }
            block
                .tail
                .as_ref()
                .map_or(Ok(Value::Unit), |tail| self.expression(tail, substitutions))
        })();
        for (local, previous) in declared {
            self.locals.remove(&local);
            if let Some(previous) = previous {
                self.locals.insert(local, previous);
            }
        }
        result
    }

    fn local_value(&mut self, local: LocalId, span: Span) -> Eval<Value> {
        let Some(value) = self.locals.get(&local) else {
            return self.fail(span, "checked local has no runtime value");
        };
        if value.affine() {
            return self.locals.remove(&local).ok_or(Halt);
        }
        self.copy_value(local, span)
    }

    fn copy_value(&mut self, local: LocalId, span: Span) -> Eval<Value> {
        fn copy(ev: &mut Evaluator<'_>, value: &Value, span: Span, depth: usize) -> Eval<Value> {
            if depth >= ev.limits.max_depth {
                return ev.fail(span, "value copy depth limit reached");
            }
            ev.expand(size_of::<Value>(), span)?;
            match value {
                Value::Unit => Ok(Value::Unit),
                Value::Int(value) => Ok(Value::Int(*value)),
                Value::Str(value) => {
                    ev.expand(value.len(), span)?;
                    Ok(Value::Str(value.clone()))
                }
                Value::Nominal {
                    ty,
                    value,
                    resource,
                } => {
                    if *resource {
                        return ev.fail(span, "cannot copy an affine resource value");
                    }
                    if let PrimitiveValue::Str(text) = value {
                        ev.expand(text.len(), span)?;
                    }
                    Ok(Value::Nominal {
                        ty: ty.clone(),
                        value: value.clone(),
                        resource: false,
                    })
                }
                Value::List { ty, items } => {
                    ev.expand(items.len().saturating_mul(size_of::<Value>()), span)?;
                    let mut copied = Vec::with_capacity(items.len());
                    for item in items {
                        copied.push(copy(ev, item, span, depth + 1)?);
                    }
                    Ok(Value::List {
                        ty: ty.clone(),
                        items: copied,
                    })
                }
                Value::Struct { ty, fields } => {
                    ev.expand(
                        fields.len().saturating_mul(size_of::<(String, Value)>()),
                        span,
                    )?;
                    let mut copied = Vec::with_capacity(fields.len());
                    for (name, value) in fields {
                        ev.expand(name.len(), span)?;
                        copied.push((name.clone(), copy(ev, value, span, depth + 1)?));
                    }
                    Ok(Value::Struct {
                        ty: ty.clone(),
                        fields: copied,
                    })
                }
                Value::Variant { ty, index } => Ok(Value::Variant {
                    ty: ty.clone(),
                    index: *index,
                }),
            }
        }
        let value = self.locals.remove(&local).ok_or(Halt)?;
        let result = copy(self, &value, span, 0);
        self.locals.insert(local, value);
        result
    }

    fn structure(
        &mut self,
        item: ItemId,
        checked_ty: &Ty,
        fields: &[crate::StructField],
        substitutions: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        let Some(declaration) = self.index.structures.get(&item).copied() else {
            return self.fail(span, "checked struct declaration is unavailable");
        };
        let concrete = self.canonical_ty(checked_ty, substitutions, span)?;
        let type_substitutions =
            self.specialization_substitutions(item, checked_ty, substitutions, span)?;
        let mark = self.scopes.len();
        for scope in &declaration.scopes {
            match self.policy.scope_rule(&scope.text) {
                Some(ScopeRule::Private) => {
                    let Some(name) = self.index.scope_name(&scope.text) else {
                        self.scopes.truncate(mark);
                        return self.fail(scope.span, "policy scope identity is unavailable");
                    };
                    self.push_scope(Some(name), scope.span);
                }
                Some(ScopeRule::Inherited) => {}
                None => {
                    self.scopes.truncate(mark);
                    return self.fail(scope.span, "checked structure uses an unsupported scope");
                }
            }
        }
        self.expand(
            declaration
                .fields
                .len()
                .saturating_mul(size_of::<(String, Value)>()),
            span,
        )?;
        let result = (|| {
            let mut supplied = BTreeMap::new();
            self.expand(
                fields.len().saturating_mul(size_of::<(&str, Value)>()),
                span,
            )?;
            for field in fields {
                let value = self.expression(&field.value, substitutions)?;
                supplied.insert(field.name.text.as_str(), value);
            }
            let mut realized = Vec::with_capacity(declaration.fields.len());
            for field in &declaration.fields {
                self.expand(field.name.text.len(), field.span)?;
                let value = if let Some(value) = supplied.remove(field.name.text.as_str()) {
                    value
                } else if let Some(default) = &field.default {
                    self.expression(default, &type_substitutions)?
                } else {
                    return self.fail(field.span, "checked struct is missing a field value");
                };
                realized.push((field.name.text.clone(), value));
            }
            Ok(Value::Struct {
                ty: concrete,
                fields: realized,
            })
        })();
        self.scopes.truncate(mark);
        result
    }

    fn construct_primitive(
        &mut self,
        item: ItemId,
        value: PrimitiveValue,
        span: Span,
        allow_resource: bool,
    ) -> Eval<Value> {
        let Some(info) = self.index.primitives.get(&item) else {
            return self.fail(span, "checked primitive declaration is unavailable");
        };
        if info.resource && !allow_resource {
            return self.fail(span, "resource literal lifting is not permitted");
        }
        self.refinements(info.declaration, &value, span)?;
        let ty = self.canonical_ty(&Ty::Nominal(item), &BTreeMap::new(), span)?;
        if let PrimitiveValue::Str(text) = &value {
            self.expand(text.len(), span)?;
        }
        if info.resource {
            let scope_index = if let Some(name) = &info.declaration.scope {
                self.scopes
                    .iter()
                    .rposition(|scope| scope.name.as_deref() == Some(&name.text))
            } else {
                self.scopes.iter().position(|scope| scope.name.is_none())
            };
            let Some(scope_index) = scope_index else {
                return self.fail(span, "resource governing scope is not open");
            };
            let scope = &self.scopes[scope_index];
            let scope_name_len = scope.name.as_ref().map_or(0, |name| name.len());
            let boundary = scope.boundary;
            self.expand(
                size_of::<ResourceClaimKey>()
                    .saturating_add(size_of::<ResourceClaim>())
                    .saturating_add(scope_name_len),
                span,
            )?;
            let scope_name = self.scopes[scope_index].name.clone();
            let key = ResourceClaimKey {
                ty: ty.clone(),
                value: value.clone(),
                scope: GoverningScope {
                    root: self.root.clone(),
                    name: scope_name.map(|name| name.to_string()),
                    boundary,
                },
            };
            if let Some(previous_span) = self.claims.get(&key).copied() {
                self.error(
                    span,
                    "resource claim conflicts with an earlier claim in this root",
                );
                if let Some(diagnostic) = self.diagnostics.last_mut() {
                    diagnostic.note =
                        Some(format!("first claimed at byte {}", previous_span.start()));
                }
                return Err(Halt);
            }
            self.claims.insert(key, span);
        }
        Ok(Value::Nominal {
            ty,
            value,
            resource: info.resource,
        })
    }

    fn refinements(
        &mut self,
        declaration: &PrimitiveDeclaration,
        value: &PrimitiveValue,
        span: Span,
    ) -> Eval<()> {
        for refinement in &declaration.refinements {
            self.tick(refinement.span)?;
            let accepted = match &refinement.kind {
                RefinementKind::Range {
                    start,
                    end,
                    inclusive,
                } => match value {
                    PrimitiveValue::Int(value) => {
                        *value >= start.value
                            && if *inclusive {
                                *value <= end.value
                            } else {
                                *value < end.value
                            }
                    }
                    PrimitiveValue::Str(_) => false,
                },
                RefinementKind::Set(values) => {
                    let mut found = false;
                    for candidate in values {
                        self.tick(span)?;
                        if let (Literal::String(literal), PrimitiveValue::Str(_)) =
                            (candidate, value)
                        {
                            // Charge the maximum byte comparisons before walking
                            // the literal; an early mismatch remains conservative.
                            self.charge_steps(literal.source.len().div_ceil(64), span)?;
                        }
                        if literal_equals(candidate, value) {
                            found = true;
                            break;
                        }
                    }
                    found
                }
                RefinementKind::Predicate(path) => {
                    let Some(name) = self.index.predicate_names.get(&span_key(path.span)) else {
                        return self.fail(path.span, "predicate name metadata is unavailable");
                    };
                    let accepted = match (self.environment.predicates.get(name.as_ref()), value) {
                        (Some(PredicateCallback::Int(callback)), PrimitiveValue::Int(value)) => {
                            catch_unwind(AssertUnwindSafe(|| callback(*value)))
                        }
                        (Some(PredicateCallback::Str(callback)), PrimitiveValue::Str(value)) => {
                            catch_unwind(AssertUnwindSafe(|| callback(value)))
                        }
                        _ => {
                            return self.fail(
                                path.span,
                                "predicate callback does not match checked metadata",
                            );
                        }
                    };
                    match accepted {
                        Ok(accepted) => accepted,
                        Err(_) => {
                            return self.fail(path.span, "predicate callback panicked");
                        }
                    }
                }
            };
            if !accepted {
                return self.fail(
                    span,
                    format!(
                        "value does not satisfy refinement on `{}`",
                        declaration.name.text
                    ),
                );
            }
        }
        Ok(())
    }

    fn string(
        &mut self,
        literal: &StringLiteral,
        substitutions: &BTreeMap<LocalId, Ty>,
    ) -> Eval<String> {
        self.expand(literal.source.len(), literal.span)?;
        let mut result = String::with_capacity(literal.source.len());
        for part in &literal.parts {
            match part {
                StringPart::Text { source, span } => {
                    decode_text_into(source, &mut result).ok_or_else(|| {
                        self.error(*span, "checked string contains an invalid escape");
                        Halt
                    })?;
                }
                StringPart::Interpolation { path, span } => {
                    let Some(ResolvedTarget::Local(local)) = self.index.target(path.span).cloned()
                    else {
                        return self.fail(*span, "checked interpolation has no local target");
                    };
                    let mut value = self.local_value(local, *span)?;
                    for field in path.segments.iter().skip(1) {
                        value = self.project(value, &field.text, field.span)?;
                    }
                    match value {
                        Value::Int(value)
                        | Value::Nominal {
                            value: PrimitiveValue::Int(value),
                            ..
                        } => {
                            let length = decimal_len(value);
                            self.expand(length, *span)?;
                            result.reserve(length);
                            if write!(result, "{value}").is_err() {
                                return self.fail(*span, "integer interpolation formatting failed");
                            }
                        }
                        Value::Str(value)
                        | Value::Nominal {
                            value: PrimitiveValue::Str(value),
                            ..
                        } => {
                            self.expand(value.len(), *span)?;
                            result.reserve(value.len());
                            result.push_str(&value);
                        }
                        _ => {
                            return self.fail(
                                *span,
                                "checked interpolation produced a non-primitive value",
                            );
                        }
                    }
                }
            }
        }
        let _ = substitutions;
        Ok(result)
    }

    fn project(&mut self, value: Value, field: &str, span: Span) -> Eval<Value> {
        let Value::Struct { fields, .. } = value else {
            return self.fail(span, "checked field projection produced a non-struct value");
        };
        fields
            .into_iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value)
            .ok_or_else(|| {
                self.error(span, "checked field projection could not find its field");
                Halt
            })
    }

    fn erase(
        &mut self,
        value: Value,
        source_ty: &Arc<CanonicalType>,
        target_ty: Arc<CanonicalType>,
        target_item: ItemId,
        span: Span,
    ) -> Eval<Value> {
        let Value::Struct { ty, fields } = value else {
            return self.fail(span, "checked erasure produced a non-struct source");
        };
        if &ty != source_ty {
            return self.fail(span, "checked erasure source identity mismatch");
        }
        let Some(target) = self.index.structures.get(&target_item).copied() else {
            return self.fail(span, "checked erasure target declaration is unavailable");
        };
        self.expand(
            target
                .fields
                .len()
                .saturating_mul(size_of::<(String, Value)>()),
            span,
        )?;
        let mut source: BTreeMap<_, _> = fields.into_iter().collect();
        let mut projected = Vec::with_capacity(target.fields.len());
        for field in &target.fields {
            let Some(value) = source.remove(&field.name.text) else {
                return self.fail(
                    field.span,
                    "checked erasure source is missing a projected field",
                );
            };
            projected.push((field.name.text.clone(), value));
        }
        Ok(Value::Struct {
            ty: target_ty,
            fields: projected,
        })
    }

    fn pattern_matches(
        &mut self,
        arm: &MatchArm,
        ty: &Arc<CanonicalType>,
        index: u32,
    ) -> Eval<bool> {
        match &arm.pattern {
            Pattern::Wildcard(_) => Ok(true),
            Pattern::Path(path) => {
                let checked = self.checked.pattern(path.span).ok_or_else(|| {
                    self.error(path.span, "missing checked pattern metadata");
                    Halt
                })?;
                let pattern_ty = self.canonical_ty(
                    &Ty::Nominal(checked.enumeration()),
                    &BTreeMap::new(),
                    path.span,
                )?;
                Ok(&pattern_ty == ty && checked.index() == index)
            }
        }
    }
}

fn static_string_equals(literal: &StringLiteral, expected: &str) -> bool {
    if literal
        .parts
        .iter()
        .any(|part| matches!(part, StringPart::Interpolation { .. }))
    {
        return false;
    }
    let mut expected = expected.chars();
    for part in &literal.parts {
        if let StringPart::Text { source, .. } = part {
            let mut source = source.chars();
            while let Some(character) = source.next() {
                let decoded = if character == '\\' {
                    match source.next() {
                        Some('"') => '"',
                        Some('\\') => '\\',
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('$') => '$',
                        _ => return false,
                    }
                } else {
                    character
                };
                if expected.next() != Some(decoded) {
                    return false;
                }
            }
        }
    }
    expected.next().is_none()
}

fn literal_equals(literal: &Literal, value: &PrimitiveValue) -> bool {
    match (literal, value) {
        (Literal::Integer(left), PrimitiveValue::Int(right)) => left.value == *right,
        (Literal::String(left), PrimitiveValue::Str(right)) => static_string_equals(left, right),
        _ => false,
    }
}

fn decode_text_into(source: &str, result: &mut String) -> Option<()> {
    let mut chars = source.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        result.push(match chars.next()? {
            '"' => '"',
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '$' => '$',
            _ => return None,
        });
    }
    Some(())
}

fn decimal_len(value: i64) -> usize {
    if value == 0 {
        return 1;
    }
    let mut magnitude = value.unsigned_abs();
    let mut length = usize::from(value.is_negative());
    while magnitude != 0 {
        magnitude /= 10;
        length += 1;
    }
    length
}
