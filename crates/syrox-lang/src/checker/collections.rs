use super::{Checker, Context, Elaboration, Expression, Primitive, ResolvedItemKind, Span, Ty};

impl Checker<'_> {
    pub(super) fn check_module_exports(
        &mut self,
        mapper: &Expression,
        context: Context,
        span: Span,
        elaboration: &mut Option<Elaboration>,
    ) -> Ty {
        let mapper_ty = self.check_expr(mapper, None, context, true);
        let Ty::Function {
            parameters,
            result,
            once: false,
        } = mapper_ty
        else {
            self.error("module mapper must be a reusable function", mapper.span);
            return Ty::Error;
        };
        let [
            Ty::Nominal(key),
            function @ Ty::Function { once: false, .. },
        ] = parameters.as_slice()
        else {
            self.error(
                "module mapper requires a string value key and a reusable function parameter",
                mapper.span,
            );
            return Ty::Error;
        };
        if self.item_kind(*key) != Some(ResolvedItemKind::Value)
            || !self
                .primitives
                .get(key)
                .is_some_and(|info| info.declaration.primitive == Primitive::Str)
        {
            self.error("module key must be a reusable value(str) type", mapper.span);
        }
        if matches!(result.as_ref(), Ty::Unit) {
            self.error(
                "module mapper cannot produce unit list elements",
                mapper.span,
            );
        }
        let Some(entries) = self.program.module_exports(span) else {
            return Ty::Error;
        };
        for entry in entries {
            if !self.charge(span) || !self.reserve_metadata(span) {
                return Ty::Error;
            }
            self.collection_metadata_units += 1;
            if !self.function_parameters(entry.item()).is_empty() {
                self.error(
                    format!(
                        "module export `{}` requires explicit generic specialization",
                        entry.key()
                    ),
                    mapper.span,
                );
                continue;
            }
            let Some((parameters, result)) = self.function_types.get(&entry.item()).cloned() else {
                continue;
            };
            let signature = Ty::Function {
                parameters,
                result: Box::new(result),
                once: false,
            };
            let Some(signature) = self.concretize(
                super::types::raw_from_ty(signature),
                &super::BTreeMap::new(),
                span,
            ) else {
                return Ty::Error;
            };
            self.expect_same(
                function,
                &signature,
                mapper.span,
                &format!("module export `{}`", entry.key()),
            );
        }
        *elaboration = Some(Elaboration::ModuleExports {
            key: *key,
            function: function.clone(),
        });
        Ty::List(result)
    }
}
