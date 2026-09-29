use super::types::raw_from_ty;
use super::{BTreeMap, Checker, Context, Elaboration, Expression, Path, ResolvedTarget, Ty, Type};

impl Checker<'_> {
    pub(super) fn check_compare(
        &mut self,
        left: &Expression,
        right: &Expression,
        branches: &[Box<Expression>; 3],
        expected: Option<&Ty>,
        context: Context,
    ) -> Ty {
        let operand = self.check_expr(left, None, context, true);
        let right_ty = self.check_expr(right, Some(&operand), context, true);
        self.expect_same(&operand, &right_ty, right.span, "comparison operand");
        let ordered = match &operand {
            Ty::Int | Ty::Str | Ty::Error => true,
            Ty::Nominal(item) => {
                self.primitives.contains_key(item)
                    && self.item_kind(*item) == Some(super::ResolvedItemKind::Value)
            }
            _ => false,
        };
        if !ordered {
            self.error(
                "comparison requires integers, strings or reusable primitive values",
                left.span,
            );
        }
        let baseline = self.body.bindings.clone();
        let mut merged = baseline.clone();
        let mut result = expected.cloned();
        for (index, branch) in branches.iter().enumerate() {
            for _ in baseline.values() {
                if !self.charge(branch.span) {
                    return Ty::Error;
                }
            }
            self.body.bindings = baseline.clone();
            let actual = self.check_expr(branch, result.as_ref(), context, true);
            if let Some(expected) = &result {
                self.expect_same(expected, &actual, branch.span, "comparison branch");
            } else {
                result = Some(actual);
            }
            self.join_branch(&mut merged, index == 0);
        }
        self.body.bindings = merged;
        result.unwrap_or(Ty::Error)
    }

    pub(super) fn check_fold(
        &mut self,
        items: &Expression,
        initial: &Expression,
        step: &Expression,
        expected: Option<&Ty>,
        context: Context,
    ) -> Ty {
        let items_ty = self.check_expr(items, None, context, true);
        let accumulator = self.check_expr(initial, expected, context, true);
        let Ty::List(element) = items_ty else {
            self.error("fold requires a list", items.span);
            return Ty::Error;
        };
        let signature = Ty::Function {
            once: false,
            parameters: vec![accumulator.clone(), *element],
            result: Box::new(accumulator.clone()),
        };
        let got = self.check_expr(step, Some(&signature), context, true);
        self.expect_same(&signature, &got, step.span, "fold step");
        accumulator
    }

    pub(super) fn check_function_specialization(
        &mut self,
        function: &Path,
        arguments: &[Type],
        elaboration: &mut Option<Elaboration>,
    ) -> Ty {
        if let Some(ResolvedTarget::EnumVariant { enumeration, index }) =
            self.target(function.span).cloned()
        {
            return self.variant_constructor(
                enumeration,
                index,
                arguments,
                function.span,
                elaboration,
            );
        }
        let Some(ResolvedTarget::Item(item)) = self.target(function.span).cloned() else {
            return Ty::Error;
        };
        let Some(info) = self.functions.get(&item).copied() else {
            self.error("type arguments require a generic function", function.span);
            return Ty::Error;
        };
        let parameters = &info.declaration.type_parameters;
        if parameters.is_empty() || parameters.len() != arguments.len() {
            self.error(
                "generic function type argument count mismatch",
                function.span,
            );
            return Ty::Error;
        }
        let outer = self.body.type_parameters.clone();
        let mut substitutions = BTreeMap::new();
        let mut instance = Vec::with_capacity(arguments.len());
        for (parameter, argument) in parameters.iter().zip(arguments) {
            let ty = self.resolve_type(argument, &outer, 0);
            instance.push(ty.clone());
            let Some(local) = self.local_id(parameter.name.span) else {
                return Ty::Error;
            };
            substitutions.insert(local, ty);
        }
        self.track_generic_instances(
            &Ty::Specialization {
                template: item,
                arguments: instance,
            },
            function.span,
        );
        let Some((parameters, result)) = self.function_types.get(&item).cloned() else {
            return Ty::Error;
        };
        let signature = Ty::Function {
            once: false,
            parameters,
            result: Box::new(result),
        };
        let ty = self
            .concretize(raw_from_ty(signature), &substitutions, function.span)
            .unwrap_or(Ty::Error);
        self.validate_concrete_type(&ty, function.span);
        self.track_generic_instances(&ty, function.span);
        *elaboration = Some(Elaboration::FunctionSpecialization { substitutions });
        ty
    }
}
