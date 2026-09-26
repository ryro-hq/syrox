use super::{
    BTreeMap, BTreeSet, CanonicalItemIdentity, Checker, Elaboration, ItemId, LocalId, ModuleId,
    Path, Primitive, RawTy, ResolvedItem, ResolvedItemKind, ResolvedTarget, Span, Ty, Type,
    TypeKind,
};

impl Checker<'_> {
    pub(super) fn collect_function_types(&mut self) {
        let functions: Vec<_> = self
            .functions
            .iter()
            .map(|(&id, info)| (id, *info))
            .collect();
        for (id, info) in functions {
            let parameters = info
                .declaration
                .parameters
                .iter()
                .map(|parameter| self.resolve_type(&parameter.ty, &BTreeMap::new(), 0))
                .collect();
            let result = info
                .declaration
                .result
                .as_ref()
                .map_or(Ty::Unit, |ty| self.resolve_type(ty, &BTreeMap::new(), 0));
            self.function_types.insert(id, (parameters, result));
        }
    }

    pub(super) fn resolve_type(
        &mut self,
        ty: &Type,
        params: &BTreeMap<LocalId, Ty>,
        depth: usize,
    ) -> Ty {
        let raw = self.resolve_raw_type(ty, &BTreeMap::new(), depth);
        let resolved = self.concretize(raw, params, ty.span).unwrap_or_else(|| {
            self.error("unbound generic type parameter", ty.span);
            Ty::Error
        });
        self.validate_concrete_type(&resolved, ty.span);
        self.track_generic_instances(&resolved, ty.span);
        resolved
    }

    pub(super) fn resolve_raw_type(
        &mut self,
        ty: &Type,
        params: &BTreeMap<LocalId, RawTy>,
        depth: usize,
    ) -> RawTy {
        self.resolve_raw_type_with_aliases(ty, params, depth, &mut BTreeSet::new())
    }

    fn resolve_raw_type_with_aliases(
        &mut self,
        ty: &Type,
        params: &BTreeMap<LocalId, RawTy>,
        depth: usize,
        aliases: &mut BTreeSet<ItemId>,
    ) -> RawTy {
        if !self.charge(ty.span) {
            return RawTy::Concrete(Ty::Error);
        }
        match &ty.kind {
            TypeKind::List(path) => {
                let element = self.resolve_named(path, &[], params, depth, aliases);
                if matches!(element, RawTy::Concrete(Ty::Error)) {
                    return element;
                }
                if matches!(element, RawTy::List(_))
                    || matches!(&element, RawTy::Concrete(element) if !element.scalar())
                {
                    self.error("lists cannot contain lists or `unit`", ty.span);
                    RawTy::Concrete(Ty::Error)
                } else {
                    RawTy::List(Box::new(element))
                }
            }
            TypeKind::Named { path, arguments } => {
                self.resolve_named(path, arguments, params, depth, aliases)
            }
        }
    }

    fn resolve_named(
        &mut self,
        path: &Path,
        arguments: &[Type],
        params: &BTreeMap<LocalId, RawTy>,
        depth: usize,
        aliases: &mut BTreeSet<ItemId>,
    ) -> RawTy {
        if depth > self.limits.max_specialization_depth {
            self.error("generic specialization depth limit reached", path.span);
            return RawTy::Concrete(Ty::Error);
        }
        let Some(target) = self.target(path.span).cloned() else {
            self.error("type has no resolved target", path.span);
            return RawTy::Concrete(Ty::Error);
        };
        match target {
            ResolvedTarget::Local(local) => {
                if !arguments.is_empty() {
                    self.error("a type parameter cannot take type arguments", path.span);
                }
                params
                    .get(&local)
                    .cloned()
                    .unwrap_or(RawTy::Parameter(local))
            }
            ResolvedTarget::Item(item) => self
                .expand_item_type_with_aliases(item, arguments, params, depth, path.span, aliases),
            ResolvedTarget::Module(_)
            | ResolvedTarget::EnumVariant { .. }
            | ResolvedTarget::ContextualEnumVariant => {
                self.error("resolved target is not a type", path.span);
                RawTy::Concrete(Ty::Error)
            }
        }
    }

    pub(super) fn expand_item_type(
        &mut self,
        item: ItemId,
        arguments: &[Type],
        params: &BTreeMap<LocalId, RawTy>,
        depth: usize,
        span: Span,
    ) -> RawTy {
        self.expand_item_type_with_aliases(
            item,
            arguments,
            params,
            depth,
            span,
            &mut BTreeSet::new(),
        )
    }

    fn expand_item_type_with_aliases(
        &mut self,
        mut item: ItemId,
        arguments: &[Type],
        params: &BTreeMap<LocalId, RawTy>,
        depth: usize,
        span: Span,
        aliases: &mut BTreeSet<ItemId>,
    ) -> RawTy {
        if !arguments.is_empty() && self.aliases.contains_key(&item) {
            self.error("type aliases cannot take type arguments", span);
            return RawTy::Concrete(Ty::Error);
        }
        let mut inserted = Vec::new();
        // A loop keeps long simple alias chains off the call stack. Keep their
        // identities live across recursive list/generic branches, then release
        // only the aliases owned by this expansion on every return path.
        let result = (|| {
            while let Some(alias) = self.aliases.get(&item).copied() {
                if !aliases.insert(item) {
                    self.error("cyclic type alias", alias.name.span);
                    return RawTy::Concrete(Ty::Error);
                }
                inserted.push(item);
                if aliases.len() > self.limits.max_alias_depth || !self.charge(alias.ty.span) {
                    self.error("type alias expansion depth limit reached", alias.ty.span);
                    return RawTy::Concrete(Ty::Error);
                }
                match &alias.ty.kind {
                    TypeKind::List(path) => {
                        let element = self.resolve_named(path, &[], params, depth, aliases);
                        return if matches!(element, RawTy::Concrete(Ty::Error)) {
                            element
                        } else {
                            RawTy::List(Box::new(element))
                        };
                    }
                    TypeKind::Named {
                        path,
                        arguments: alias_arguments,
                    } => {
                        if !alias_arguments.is_empty() {
                            return self.resolve_named(
                                path,
                                alias_arguments,
                                params,
                                depth + 1,
                                aliases,
                            );
                        }
                        match self.target(path.span).cloned() {
                            Some(ResolvedTarget::Item(next)) => item = next,
                            Some(ResolvedTarget::Local(local)) => {
                                return params
                                    .get(&local)
                                    .cloned()
                                    .unwrap_or(RawTy::Parameter(local));
                            }
                            _ => return RawTy::Concrete(Ty::Error),
                        }
                    }
                }
            }
            let Some(kind) = self.item_kind(item) else {
                return RawTy::Concrete(Ty::Error);
            };
            if kind != ResolvedItemKind::Struct {
                if !arguments.is_empty() {
                    self.error("only structs can take type arguments", span);
                    return RawTy::Concrete(Ty::Error);
                }
                return RawTy::Concrete(Ty::Nominal(item));
            }
            let arity = self
                .structs
                .get(&item)
                .map_or(0, |info| info.parameters.len());
            if arguments.len() != arity {
                self.error(
                    format!(
                        "generic struct expects {arity} type argument(s), found {}",
                        arguments.len()
                    ),
                    span,
                );
                return RawTy::Concrete(Ty::Error);
            }
            if arity == 0 {
                return RawTy::Concrete(Ty::Nominal(item));
            }
            let mut checked = Vec::with_capacity(arguments.len());
            for argument in arguments {
                let checked_argument =
                    self.resolve_raw_type_with_aliases(argument, params, depth + 1, aliases);
                if !raw_scalar(&checked_argument) {
                    self.error(
                        "generic arguments must be concrete scalar types",
                        argument.span,
                    );
                }
                checked.push(checked_argument);
            }
            RawTy::Specialization {
                template: item,
                arguments: checked,
            }
        })();
        for item in inserted {
            aliases.remove(&item);
        }
        result
    }

    pub(super) fn project(&mut self, ty: &Ty, field: &str, span: Span) -> Ty {
        self.fields_of(ty)
            .into_iter()
            .find(|(name, _)| name == field)
            .map_or_else(
                || {
                    self.error(format!("type has no field `{field}`"), span);
                    Ty::Error
                },
                |(_, ty)| ty,
            )
    }

    pub(super) fn fields_of(&mut self, ty: &Ty) -> Vec<(String, Ty)> {
        let Some(item) = nominal_head(ty) else {
            return Vec::new();
        };
        let Some(info) = self.structs.get(&item) else {
            return Vec::new();
        };
        let declaration = info.declaration;
        let substitutions = self.specialization_substitutions(item, ty);
        declaration
            .fields
            .iter()
            .map(|field| {
                (
                    field.name.text.clone(),
                    self.resolve_type(&field.ty, &substitutions, 1),
                )
            })
            .collect()
    }

    pub(super) fn specialization_substitutions(
        &self,
        item: ItemId,
        ty: &Ty,
    ) -> BTreeMap<LocalId, Ty> {
        let Some(info) = self.structs.get(&item) else {
            return BTreeMap::new();
        };
        let arguments = match ty {
            Ty::Specialization { arguments, .. } => arguments.as_slice(),
            _ => &[],
        };
        info.parameters
            .iter()
            .copied()
            .zip(arguments.iter().cloned())
            .collect()
    }

    pub(super) fn lift_literal(
        &self,
        primitive: Ty,
        expected: Option<&Ty>,
        elaboration: &mut Option<Elaboration>,
    ) -> Ty {
        let Some(Ty::Nominal(item)) = expected else {
            return primitive;
        };
        if self.item_kind(*item) != Some(ResolvedItemKind::Value) {
            return primitive;
        }
        let Some(info) = self.primitives.get(item) else {
            return primitive;
        };
        let matches = matches!(
            (&info.declaration.primitive, &primitive),
            (Primitive::Int, Ty::Int) | (Primitive::Str, Ty::Str)
        );
        if matches {
            *elaboration = Some(Elaboration::ValueLiteral(*item));
            Ty::Nominal(*item)
        } else {
            primitive
        }
    }

    pub(super) fn validate_concrete_type(&mut self, ty: &Ty, span: Span) {
        match ty {
            Ty::Specialization {
                template,
                arguments,
            } => {
                if let Some(info) = self.structs.get(template)
                    && let Some(index) = info
                        .declaration
                        .type_parameters
                        .iter()
                        .position(|parameter| parameter.owner)
                    && !matches!(arguments[index], Ty::Parameter(_))
                    && !self.is_nominal_struct(&arguments[index])
                {
                    self.error(
                        "delegated owner argument must be a nominal struct type",
                        span,
                    );
                }
                for argument in arguments {
                    self.validate_concrete_type(argument, span);
                }
            }
            Ty::List(element) => self.validate_concrete_type(element, span),
            Ty::Unit | Ty::Int | Ty::Str | Ty::Parameter(_) | Ty::Nominal(_) | Ty::Error => {}
        }
    }

    pub(super) fn expect_same(&mut self, want: &Ty, got: &Ty, span: Span, position: &str) {
        if !want.compatible(got) {
            self.error(
                format!("{position} type mismatch: expected {want:?}, found {got:?}"),
                span,
            );
        }
    }

    /// Charge every node that will be allocated before substituting a generic.
    /// One parameter can occur repeatedly inside a nested type; charging the
    /// input syntax alone does not bound the expanded result.
    pub(super) fn concretize(
        &mut self,
        raw: RawTy,
        params: &BTreeMap<LocalId, Ty>,
        span: Span,
    ) -> Option<Ty> {
        enum Node<'a> {
            Raw(&'a RawTy),
            Concrete(&'a Ty),
        }
        let mut pending = vec![Node::Raw(&raw)];
        let mut work = self.work;
        while let Some(node) = pending.pop() {
            work = work.saturating_add(1);
            if work > self.limits.max_work || self.exhausted {
                if !self.exhausted {
                    self.exhausted = true;
                    self.error("type checking work limit reached", span);
                }
                return None;
            }
            match node {
                Node::Raw(RawTy::Concrete(ty)) => match ty {
                    Ty::List(inner) => pending.push(Node::Concrete(inner)),
                    Ty::Specialization { arguments, .. } => {
                        pending.extend(arguments.iter().map(Node::Concrete));
                    }
                    _ => {}
                },
                Node::Raw(RawTy::Parameter(id)) => {
                    pending.push(Node::Concrete(params.get(id)?));
                }
                Node::Raw(RawTy::Specialization { arguments, .. }) => {
                    pending.extend(arguments.iter().map(Node::Raw));
                }
                Node::Raw(RawTy::List(inner)) => pending.push(Node::Raw(inner)),
                Node::Concrete(Ty::List(inner)) => pending.push(Node::Concrete(inner)),
                Node::Concrete(Ty::Specialization { arguments, .. }) => {
                    pending.extend(arguments.iter().map(Node::Concrete));
                }
                Node::Concrete(_) => {}
            }
        }
        self.work = work;
        concretize(raw, params)
    }

    pub(super) fn track_generic_instances(&mut self, ty: &Ty, span: Span) {
        if !self.charge(span) {
            return;
        }
        match ty {
            Ty::Specialization { arguments, .. } => {
                if !ty_contains_parameter(ty) && !self.generic_instances.contains(ty) {
                    if self.generic_instances.len() >= self.limits.max_generic_instances {
                        if !self.exhausted {
                            self.exhausted = true;
                            self.error("generic instance limit reached", span);
                        }
                        return;
                    }
                    self.generic_instances.insert(ty.clone());
                }
                for argument in arguments {
                    self.track_generic_instances(argument, span);
                }
            }
            Ty::List(element) => self.track_generic_instances(element, span),
            Ty::Unit | Ty::Int | Ty::Str | Ty::Parameter(_) | Ty::Nominal(_) | Ty::Error => {}
        }
    }

    pub(super) fn variant_index(&self, enumeration: ItemId, name: &str) -> Option<u32> {
        self.enums
            .get(&enumeration)?
            .variants
            .iter()
            .position(|variant| variant.text == name)
            .and_then(|index| u32::try_from(index).ok())
    }

    pub(super) fn variant_exists(&mut self, enumeration: ItemId, index: u32, span: Span) -> bool {
        let exists = self.enums.get(&enumeration).is_some_and(|item| {
            usize::try_from(index).is_ok_and(|index| index < item.variants.len())
        });
        if !exists {
            self.error("unknown enum variant", span);
        }
        exists
    }

    pub(super) fn module_is_descendant(&self, module: ModuleId, ancestor: ModuleId) -> bool {
        let module = self
            .program
            .modules()
            .find(|candidate| candidate.id() == module);
        let ancestor = self
            .program
            .modules()
            .find(|candidate| candidate.id() == ancestor);
        match (module, ancestor) {
            (Some(module), Some(ancestor)) => {
                module.domain() == ancestor.domain()
                    && module
                        .path()
                        .segments()
                        .starts_with(ancestor.path().segments())
            }
            _ => false,
        }
    }

    pub(super) fn item_kind(&self, item: ItemId) -> Option<ResolvedItemKind> {
        self.program
            .items()
            .find(|candidate| candidate.id() == item)
            .map(ResolvedItem::kind)
    }

    pub(super) fn item_span(&self, item: ItemId) -> Span {
        self.program
            .items()
            .find(|candidate| candidate.id() == item)
            .expect("known item")
            .span()
    }

    pub(super) fn item_identity(&self, item: ItemId) -> CanonicalItemIdentity {
        self.program
            .items()
            .find(|candidate| candidate.id() == item)
            .map(CanonicalItemIdentity::from_resolved)
            .expect("known item")
    }

    pub(super) fn expression_ty(&self, span: Span) -> Ty {
        self.expressions
            .iter()
            .rev()
            .find(|expression| expression.span == span)
            .map_or(Ty::Error, |expression| expression.ty.clone())
    }
}

pub(super) fn concretize(raw: RawTy, params: &BTreeMap<LocalId, Ty>) -> Option<Ty> {
    match raw {
        RawTy::Concrete(ty) => Some(ty),
        RawTy::Parameter(parameter) => params.get(&parameter).cloned(),
        RawTy::Specialization {
            template,
            arguments,
        } => arguments
            .into_iter()
            .map(|argument| concretize(argument, params))
            .collect::<Option<Vec<_>>>()
            .map(|arguments| Ty::Specialization {
                template,
                arguments,
            }),
        RawTy::List(element) => concretize(*element, params).map(|ty| Ty::List(Box::new(ty))),
    }
}

pub(super) fn raw_from_ty(ty: Ty) -> RawTy {
    match ty {
        Ty::Specialization {
            template,
            arguments,
        } => RawTy::Specialization {
            template,
            arguments: arguments.into_iter().map(raw_from_ty).collect(),
        },
        Ty::List(element) => RawTy::List(Box::new(raw_from_ty(*element))),
        ty => RawTy::Concrete(ty),
    }
}

pub(super) fn ty_contains_parameter(ty: &Ty) -> bool {
    match ty {
        Ty::Parameter(_) => true,
        Ty::Specialization { arguments, .. } => arguments.iter().any(ty_contains_parameter),
        Ty::List(element) => ty_contains_parameter(element),
        Ty::Unit | Ty::Int | Ty::Str | Ty::Nominal(_) | Ty::Error => false,
    }
}

pub(super) fn raw_scalar(raw: &RawTy) -> bool {
    match raw {
        RawTy::Concrete(ty) => ty.scalar(),
        RawTy::Parameter(_) | RawTy::Specialization { .. } => true,
        RawTy::List(_) => false,
    }
}

pub(super) fn nominal_head(ty: &Ty) -> Option<ItemId> {
    match ty {
        Ty::Nominal(item) => Some(*item),
        Ty::Specialization { template, .. } => Some(*template),
        Ty::Unit | Ty::Int | Ty::Str | Ty::Parameter(_) | Ty::List(_) | Ty::Error => None,
    }
}

pub(super) fn nominal_enum(ty: &Ty) -> Option<ItemId> {
    match ty {
        Ty::Nominal(item) => Some(*item),
        _ => None,
    }
}
