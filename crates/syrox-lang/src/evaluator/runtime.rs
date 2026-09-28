use std::{
    collections::{BTreeMap, BTreeSet},
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

// Calls use several evaluator frames per expression. Fold releases each call
// before the next item, independently of this native-stack protection.
const MAX_CALL_DEPTH: usize = 32;

pub(super) struct Evaluator<'a> {
    pub(super) checked: &'a CheckedProgram,
    policy: &'a CheckPolicy,
    environment: &'a EvaluationEnvironment,
    pub(super) index: &'a ProgramIndex<'a>,
    pub(super) limits: EvaluationLimits,
    root: Arc<CanonicalItemIdentity>,
    project_context: crate::SourceDomainId,
    pub(super) diagnostics: Vec<Diagnostic>,
    locals: BTreeMap<LocalId, Value>,
    active_outputs: BTreeSet<ItemId>,
    memo_outputs: BTreeMap<ItemId, Value>,
    scopes: Vec<OpenScope>,
    pub(super) claims: BTreeMap<ResourceClaimKey, Span>,
    pub(super) canonical_types: BTreeMap<Ty, Arc<CanonicalType>>,
    next_boundary: u64,
    pub(super) steps: usize,
    depth: usize,
    call_depth: usize,
    pub(super) expansion: usize,
    pub(super) operation_error: Option<EvaluationSetupError>,
}

impl<'a> Evaluator<'a> {
    fn enter_call(&mut self, span: Span) -> Eval<()> {
        if self.call_depth >= MAX_CALL_DEPTH.min(self.limits.max_depth) {
            return self.fail(span, "function call depth limit reached");
        }
        self.call_depth += 1;
        Ok(())
    }

    fn enter_project_context(&mut self, span: Span) -> crate::SourceDomainId {
        let previous = self.project_context;
        // A std helper acts on behalf of its caller; a project function or
        // imported output changes the current asset authority to its owner.
        if let Some(owner) = self.checked.resolved().parsed().owner(span.source_id()) {
            self.project_context = owner;
        }
        previous
    }

    pub(super) fn inherit_outputs(&mut self, outputs: BTreeMap<ItemId, Value>) {
        self.memo_outputs = outputs;
    }

    pub(super) fn take_shareable_outputs(&mut self, succeeded: bool) -> BTreeMap<ItemId, Value> {
        if succeeded && self.diagnostics.is_empty() && self.claims.is_empty() {
            std::mem::take(&mut self.memo_outputs)
        } else {
            BTreeMap::new()
        }
    }

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
            project_context: crate::SourceDomainId::project(),
            diagnostics: Vec::new(),
            locals: BTreeMap::new(),
            active_outputs: BTreeSet::new(),
            memo_outputs: BTreeMap::new(),
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
            call_depth: 0,
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
                Some(Elaboration::ContextualVariant { index, .. }) => Ok(Value::Variant {
                    ty: self.canonical_ty(metadata.ty(), substitutions, path.span)?,
                    index: *index,
                    payload: Vec::new(),
                }),
                _ => match self.index.target(path.span) {
                    Some(ResolvedTarget::Local(local)) => self.local_value(*local, path.span),
                    Some(ResolvedTarget::Item(item)) if self.index.functions.contains_key(item) => {
                        let supplied = match metadata.elaboration() {
                            Some(Elaboration::FunctionSpecialization {
                                substitutions: supplied,
                            }) => self.substitute_bindings(supplied, substitutions, path.span)?,
                            _ => BTreeMap::new(),
                        };
                        Ok(Value::Function {
                            item: *item,
                            substitutions: supplied,
                            ty: self.canonical_ty(metadata.ty(), substitutions, path.span)?,
                        })
                    }
                    Some(ResolvedTarget::Item(item))
                        if self.index.output_values.contains_key(item) =>
                    {
                        self.output_value(*item, path.span)
                    }
                    Some(ResolvedTarget::EnumVariant { index, .. }) => Ok(Value::Variant {
                        ty: self.canonical_ty(metadata.ty(), substitutions, path.span)?,
                        index: *index,
                        payload: Vec::new(),
                    }),
                    _ => self.fail(path.span, "checked value path has no evaluable target"),
                },
            },
            ExpressionKind::Call { callee, arguments } => {
                self.call(callee.span, arguments, substitutions, expression.span)
            }
            ExpressionKind::Specialize { function, .. } => {
                if let Some(Elaboration::VariantConstructor { index }) = metadata.elaboration() {
                    return Ok(Value::VariantConstructor {
                        ty: self.canonical_ty(metadata.ty(), substitutions, expression.span)?,
                        index: *index,
                    });
                }
                let Some(ResolvedTarget::Item(item)) = self.index.target(function.span).cloned()
                else {
                    return self.fail(expression.span, "missing generic function target");
                };
                let Some(Elaboration::FunctionSpecialization {
                    substitutions: supplied,
                }) = metadata.elaboration()
                else {
                    return self.fail(expression.span, "missing function specialization metadata");
                };
                let specialized =
                    self.substitute_bindings(supplied, substitutions, expression.span)?;
                Ok(Value::Function {
                    item,
                    ty: self.canonical_ty(metadata.ty(), substitutions, expression.span)?,
                    substitutions: specialized,
                })
            }
            ExpressionKind::Apply { callee, arguments } => {
                let callable = self.expression(callee, substitutions)?;
                self.call_value(
                    callable,
                    callee.span,
                    arguments,
                    substitutions,
                    expression.span,
                )
            }
            ExpressionKind::ModuleExports { mapper, .. } => {
                self.module_exports(mapper, substitutions, expression.span)
            }
            ExpressionKind::Compare {
                left,
                right,
                branches,
            } => {
                let left = self.expression(left, substitutions)?;
                let right = self.expression(right, substitutions)?;
                let index = self.compare_values(left, right, expression.span)?;
                self.expression(&branches[index], substitutions)
            }
            ExpressionKind::Fold {
                items,
                initial,
                step,
            } => {
                let Value::List { items, .. } = self.expression(items, substitutions)? else {
                    return self.fail(expression.span, "checked fold input is not a list");
                };
                let mut accumulator = self.expression(initial, substitutions)?;
                let callback = self.expression(step, substitutions)?;
                for item in items {
                    self.tick(expression.span)?;
                    self.expand(2 * size_of::<Value>(), expression.span)?;
                    let callable = Self::copy_value_ref(self, &callback, step.span, 0)?;
                    accumulator =
                        self.invoke_value(callable, vec![accumulator, item], expression.span)?;
                }
                Ok(accumulator)
            }
            ExpressionKind::Closure {
                parameters,
                body,
                once,
                ..
            } => {
                let referenced = self.index.locals_referenced_in(expression.span).count();
                self.charge_steps(referenced, expression.span)?;
                self.expand(
                    referenced.saturating_mul(size_of::<LocalId>()),
                    expression.span,
                )?;
                let referenced: BTreeSet<_> =
                    self.index.locals_referenced_in(expression.span).collect();
                self.expand(
                    body.span.end().saturating_sub(body.span.start()) as usize * 32,
                    expression.span,
                )?;
                let mut captures = Vec::new();
                for local in referenced {
                    if self.locals.contains_key(&local) {
                        self.expand(size_of::<(LocalId, Value)>(), expression.span)?;
                        let value = if *once {
                            self.local_value(local, expression.span)?
                        } else {
                            self.copy_value(local, expression.span)?
                        };
                        captures.push((local, value));
                    }
                }
                self.expand(
                    parameters.len().saturating_mul(size_of::<LocalId>()),
                    expression.span,
                )?;
                let parameters = parameters
                    .iter()
                    .map(|parameter| self.index.local(parameter.name.span).ok_or(Halt))
                    .collect::<Eval<Vec<_>>>()?;
                Ok(Value::Closure {
                    once: *once,
                    owner: self
                        .checked
                        .resolved()
                        .parsed()
                        .owner(expression.span.source_id())
                        .unwrap_or(self.project_context),
                    substitutions: self.substitute_bindings(
                        substitutions,
                        &BTreeMap::new(),
                        expression.span,
                    )?,
                    ty: self.canonical_ty(metadata.ty(), substitutions, expression.span)?,
                    body: Arc::new(body.clone()),
                    parameters,
                    captures,
                })
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
                let Value::Variant { ty, index, payload } = scrutinee else {
                    return self.fail(value.span, "checked match produced a non-variant value");
                };
                for arm in arms {
                    if self.pattern_matches(arm, &ty, index)? {
                        return self.match_payload(arm, payload, substitutions);
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

    fn output_value(&mut self, item: ItemId, span: Span) -> Eval<Value> {
        if let Some(value) = self.memo_outputs.remove(&item) {
            let result = Self::copy_value_ref(self, &value, span, 0);
            self.memo_outputs.insert(item, value);
            return result;
        }
        if self.active_outputs.contains(&item) {
            return self.fail(span, "cycle in imported value outputs");
        }
        self.expand(size_of::<ItemId>(), span)?;
        self.active_outputs.insert(item);
        let value = self.index.output_values.get(&item).copied().ok_or(Halt)?;
        let previous = self.enter_project_context(value.span);
        let result = self.expression(value, &BTreeMap::new());
        self.project_context = previous;
        self.active_outputs.remove(&item);
        let value = result?;
        if value.affine() {
            return self.fail(
                span,
                "imported value output cannot carry an affine resource",
            );
        }
        self.expand(size_of::<(ItemId, Value)>(), span)?;
        let copy = Self::copy_value_ref(self, &value, span, 0);
        self.memo_outputs.insert(item, value);
        copy
    }

    fn call(
        &mut self,
        callee_span: Span,
        arguments: &[Expression],
        substitutions: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        let item = match self.index.target(callee_span).cloned() {
            Some(ResolvedTarget::EnumVariant { index, .. }) => {
                let payload = self.evaluate_arguments(arguments, substitutions, span)?;
                let ty = self.checked.expression(span).ok_or(Halt)?.ty();
                let ty = self.canonical_ty(ty, substitutions, span)?;
                return self.construct_variant(ty, index, payload, span);
            }
            Some(ResolvedTarget::Item(item)) => item,
            Some(ResolvedTarget::Local(local)) => {
                let callable = self.local_value(local, callee_span)?;
                return self.call_value(callable, callee_span, arguments, substitutions, span);
            }
            _ => return self.fail(callee_span, "checked call has no item target"),
        };
        let supplied = match self
            .checked
            .expression(span)
            .and_then(|metadata| metadata.elaboration())
        {
            Some(Elaboration::FunctionSpecialization {
                substitutions: supplied,
            }) => self.substitute_bindings(supplied, substitutions, span)?,
            _ => BTreeMap::new(),
        };
        self.call_item(item, callee_span, arguments, substitutions, &supplied, span)
    }

    fn call_value(
        &mut self,
        callable: Value,
        callee_span: Span,
        arguments: &[Expression],
        substitutions: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        let values = self.evaluate_arguments(arguments, substitutions, span)?;
        self.invoke_value(callable, values, callee_span)
    }

    fn evaluate_arguments(
        &mut self,
        arguments: &[Expression],
        substitutions: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Vec<Value>> {
        self.expand(arguments.len().saturating_mul(size_of::<Value>()), span)?;
        arguments
            .iter()
            .map(|argument| self.expression(argument, substitutions))
            .collect()
    }

    fn invoke_value(&mut self, callable: Value, values: Vec<Value>, span: Span) -> Eval<Value> {
        match callable {
            Value::VariantConstructor { ty, index } => {
                let CanonicalType::Function {
                    parameters, result, ..
                } = ty.as_ref()
                else {
                    return self.fail(span, "variant constructor has no function type");
                };
                if values.len() != parameters.len() {
                    return self.fail(span, "variant constructor arity mismatch");
                }
                self.construct_variant(result.clone(), index, values, span)
            }
            Value::Function {
                item,
                substitutions: supplied,
                ..
            } => self.invoke_item(item, values, &supplied, span),
            Value::Closure {
                body,
                owner,
                parameters,
                captures,
                substitutions: supplied,
                ..
            } => {
                if values.len() != parameters.len() {
                    return self.fail(span, "closure call has invalid checked arity");
                }
                self.expand(
                    captures
                        .len()
                        .saturating_add(values.len())
                        .saturating_mul(size_of::<(LocalId, Option<Value>)>()),
                    span,
                )?;
                let mut bound = Vec::new();
                for (local, value) in captures
                    .into_iter()
                    .chain(parameters.into_iter().zip(values))
                {
                    bound.push((local, self.locals.insert(local, value)));
                }
                self.enter_call(span)?;
                let previous_context = std::mem::replace(&mut self.project_context, owner);
                let result = self.block(&body, &supplied);
                self.call_depth -= 1;
                self.project_context = previous_context;
                for (local, previous) in bound.into_iter().rev() {
                    self.locals.remove(&local);
                    if let Some(previous) = previous {
                        self.locals.insert(local, previous);
                    }
                }
                result
            }
            _ => self.fail(span, "checked callee is not a function value"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call_item(
        &mut self,
        item: ItemId,
        callee_span: Span,
        arguments: &[Expression],
        substitutions: &BTreeMap<LocalId, Ty>,
        supplied: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        let values = self.evaluate_arguments(arguments, substitutions, span)?;
        self.invoke_item(item, values, supplied, callee_span)
    }

    fn invoke_item(
        &mut self,
        item: ItemId,
        values: Vec<Value>,
        supplied: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        self.enter_call(span)?;
        let result = self.invoke_item_inner(item, values, supplied, span);
        self.call_depth -= 1;
        result
    }

    fn invoke_item_inner(
        &mut self,
        item: ItemId,
        mut values: Vec<Value>,
        supplied: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
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
            return self.construct_primitive(item, primitive, span, true);
        }
        let Some(function) = self.index.functions.get(&item).copied() else {
            return self.fail(span, "checked function target is unavailable");
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
        let previous_context = self.enter_project_context(function.body.span);
        let result = self.block(&function.body, supplied);
        self.project_context = previous_context;
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

    #[allow(clippy::too_many_lines)]
    fn copy_value_ref(
        ev: &mut Evaluator<'_>,
        value: &Value,
        span: Span,
        depth: usize,
    ) -> Eval<Value> {
        if depth >= ev.limits.max_depth {
            return ev.fail(span, "value copy depth limit reached");
        }
        ev.expand(size_of::<Value>(), span)?;
        match value {
            Value::VariantConstructor { ty, index } => Ok(Value::VariantConstructor {
                ty: ty.clone(),
                index: *index,
            }),
            Value::Function {
                item,
                ty,
                substitutions,
            } => Ok(Value::Function {
                item: *item,
                ty: ty.clone(),
                substitutions: ev.substitute_bindings(substitutions, &BTreeMap::new(), span)?,
            }),
            Value::Closure {
                ty,
                once,
                owner,
                body,
                parameters,
                captures,
                substitutions,
            } => {
                if *once {
                    return ev.fail(span, "cannot copy a consumable closure");
                }
                let parameter_bytes = parameters.len().saturating_mul(size_of::<LocalId>());
                let capture_bytes = captures.len().saturating_mul(size_of::<(LocalId, Value)>());
                ev.expand(parameter_bytes.saturating_add(capture_bytes), span)?;
                let mut copied = Vec::with_capacity(captures.len());
                for (local, value) in captures {
                    copied.push((*local, Self::copy_value_ref(ev, value, span, depth + 1)?));
                }
                Ok(Value::Closure {
                    once: false,
                    owner: *owner,
                    substitutions: ev.substitute_bindings(substitutions, &BTreeMap::new(), span)?,
                    ty: ty.clone(),
                    body: body.clone(),
                    parameters: parameters.clone(),
                    captures: copied,
                })
            }
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
                    copied.push(Self::copy_value_ref(ev, item, span, depth + 1)?);
                }
                Ok(Value::List {
                    ty: ty.clone(),
                    items: copied,
                })
            }
            Value::Struct { ty, fields, owner } => {
                ev.expand(
                    fields.len().saturating_mul(size_of::<(String, Value)>()),
                    span,
                )?;
                let mut copied = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    ev.expand(name.len(), span)?;
                    copied.push((
                        name.clone(),
                        Self::copy_value_ref(ev, value, span, depth + 1)?,
                    ));
                }
                Ok(Value::Struct {
                    ty: ty.clone(),
                    fields: copied,
                    owner: *owner,
                })
            }
            Value::Variant { ty, index, payload } => {
                ev.expand(payload.len().saturating_mul(size_of::<Value>()), span)?;
                let payload = payload
                    .iter()
                    .map(|value| Self::copy_value_ref(ev, value, span, depth + 1))
                    .collect::<Eval<Vec<_>>>()?;
                Ok(Value::Variant {
                    ty: ty.clone(),
                    index: *index,
                    payload,
                })
            }
        }
    }

    fn copy_value(&mut self, local: LocalId, span: Span) -> Eval<Value> {
        let value = self.locals.remove(&local).ok_or(Halt)?;
        let result = Self::copy_value_ref(self, &value, span, 0);
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
                owner: Some(
                    self.checked
                        .resolved()
                        .parsed()
                        .owner(span.source_id())
                        .unwrap_or(self.project_context),
                ),
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
        let Value::Struct { ty, fields, owner } = value else {
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
            owner,
        })
    }

    fn module_exports(
        &mut self,
        mapper: &Expression,
        substitutions: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Eval<Value> {
        let metadata = self.checked.expression(span).ok_or(Halt)?;
        let Some(Elaboration::ModuleExports { key, function }) = metadata.elaboration() else {
            return self.fail(span, "missing checked module collection metadata");
        };
        let entries = self.checked.resolved().module_exports(span).ok_or(Halt)?;
        self.expand(entries.len().saturating_mul(size_of::<Value>()), span)?;
        let callback = self.expression(mapper, substitutions)?;
        let function = self.canonical_ty(function, substitutions, span)?;
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries {
            self.tick(span)?;
            self.expand(
                entry.key().len().saturating_add(2 * size_of::<Value>()),
                span,
            )?;
            let key = self.construct_primitive(
                *key,
                PrimitiveValue::Str(entry.key().to_owned()),
                span,
                true,
            )?;
            let factory = Value::Function {
                item: entry.item(),
                ty: function.clone(),
                substitutions: BTreeMap::new(),
            };
            let callable = Self::copy_value_ref(self, &callback, mapper.span, 0)?;
            items.push(self.invoke_value(callable, vec![key, factory], span)?);
        }
        Ok(Value::List {
            ty: self.canonical_ty(metadata.ty(), substitutions, span)?,
            items,
        })
    }

    fn compare_values(&mut self, left: Value, right: Value, span: Span) -> Eval<usize> {
        fn primitive(value: Value) -> Option<PrimitiveValue> {
            match value {
                Value::Int(value) => Some(PrimitiveValue::Int(value)),
                Value::Str(value) => Some(PrimitiveValue::Str(value)),
                Value::Nominal {
                    value,
                    resource: false,
                    ..
                } => Some(value),
                _ => None,
            }
        }
        let ordering = match (primitive(left), primitive(right)) {
            (Some(PrimitiveValue::Int(left)), Some(PrimitiveValue::Int(right))) => left.cmp(&right),
            (Some(PrimitiveValue::Str(left)), Some(PrimitiveValue::Str(right))) => {
                self.charge_steps(left.len().min(right.len()).saturating_add(1), span)?;
                left.as_bytes().cmp(right.as_bytes())
            }
            _ => return self.fail(span, "checked comparison has invalid operands"),
        };
        Ok(match ordering {
            std::cmp::Ordering::Less => 0,
            std::cmp::Ordering::Equal => 1,
            std::cmp::Ordering::Greater => 2,
        })
    }

    fn construct_variant(
        &mut self,
        ty: Arc<CanonicalType>,
        index: u32,
        payload: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        let value = Value::Variant { ty, index, payload };
        self.check_payload_depth(&value, span, 0)?;
        Ok(value)
    }

    // An iterative fold can grow a recursive payload without increasing call
    // depth. Bound the retained tree before later copying, affine inspection or
    // destruction can recurse through it. Traversal itself consumes work budget.
    fn check_payload_depth(&mut self, value: &Value, span: Span, depth: usize) -> Eval<()> {
        self.tick(span)?;
        if depth >= self.limits.max_depth {
            return self.fail(span, "enum payload value depth limit reached");
        }
        match value {
            Value::Variant { payload: items, .. } | Value::List { items, .. } => {
                for item in items {
                    self.check_payload_depth(item, span, depth + 1)?;
                }
            }
            Value::Struct { fields, .. } => {
                for (_, item) in fields {
                    self.check_payload_depth(item, span, depth + 1)?;
                }
            }
            Value::Closure { captures, .. } => {
                for (_, item) in captures {
                    self.check_payload_depth(item, span, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn match_payload(
        &mut self,
        arm: &MatchArm,
        payload: Vec<Value>,
        substitutions: &BTreeMap<LocalId, Ty>,
    ) -> Eval<Value> {
        let mut bound = Vec::new();
        if let Pattern::Variant { bindings, .. } = &arm.pattern {
            if bindings.len() != payload.len() {
                return self.fail(arm.span, "checked pattern payload arity mismatch");
            }
            self.expand(
                bindings
                    .len()
                    .saturating_mul(size_of::<(LocalId, Option<Value>)>()),
                arm.span,
            )?;
            for (binding, value) in bindings.iter().zip(payload) {
                if binding.text == "_" {
                    continue;
                }
                let local = self.index.local(binding.span).ok_or(Halt)?;
                bound.push((local, self.locals.insert(local, value)));
            }
        }
        let result = self.expression(&arm.value, substitutions);
        for (local, previous) in bound.into_iter().rev() {
            self.locals.remove(&local);
            if let Some(previous) = previous {
                self.locals.insert(local, previous);
            }
        }
        result
    }

    fn pattern_matches(
        &mut self,
        arm: &MatchArm,
        ty: &Arc<CanonicalType>,
        index: u32,
    ) -> Eval<bool> {
        match &arm.pattern {
            Pattern::Wildcard(_) => Ok(true),
            Pattern::Path(path) | Pattern::Variant { path, .. } => {
                let checked = self.checked.pattern(path.span).ok_or_else(|| {
                    self.error(path.span, "missing checked pattern metadata");
                    Halt
                })?;
                let pattern_ty = self.canonical_ty(
                    &Ty::Nominal(checked.enumeration()),
                    &BTreeMap::new(),
                    path.span,
                )?;
                let same_enum = match (pattern_ty.as_ref(), ty.as_ref()) {
                    (
                        CanonicalType::Nominal(expected),
                        CanonicalType::Specialization { template, .. },
                    ) => expected == template,
                    _ => &pattern_ty == ty,
                };
                Ok(same_enum && checked.index() == index)
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
