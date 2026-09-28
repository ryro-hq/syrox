use super::types::nominal_head;
use super::{
    BTreeMap, BTreeSet, Checker, Diagnostic, ItemId, LocalId, RawTy, ResolvedItemKind, Span, Ty,
    VecDeque,
};

impl Checker<'_> {
    pub(super) fn propagate_carriers(&mut self) {
        let mut reverse: BTreeMap<ItemId, Vec<ItemId>> = BTreeMap::new();
        let mut queue = VecDeque::new();
        let fields: Vec<_> = self
            .struct_fields
            .iter()
            .map(|(&id, fields)| (id, fields.clone()))
            .collect();
        for (container, fields) in fields {
            let mut seeded = false;
            for (_, field, _) in &fields {
                let mut pending = vec![field];
                while let Some(field) = pending.pop() {
                    if !self.charge(self.item_span(container)) {
                        return;
                    }
                    match field {
                        RawTy::Concrete(ty) => {
                            if self.ty_directly_claims(ty) {
                                seeded = true;
                            }
                            if let Some(dependency) = nominal_head(ty) {
                                reverse.entry(dependency).or_default().push(container);
                            }
                        }
                        RawTy::List(element) => pending.push(element),
                        RawTy::Specialization { template, .. } => {
                            reverse.entry(*template).or_default().push(container);
                        }
                        RawTy::Function { once: true, .. } => seeded = true,
                        RawTy::Parameter(_) | RawTy::Function { once: false, .. } => {}
                    }
                }
            }
            if seeded && self.carriers.insert(container) {
                queue.push_back(container);
            }
        }
        while let Some(carrier) = queue.pop_front() {
            if !self.charge(self.item_span(carrier)) {
                return;
            }
            if let Some(containers) = reverse.get(&carrier) {
                for &container in containers {
                    if self.carriers.insert(container) {
                        queue.push_back(container);
                    }
                }
            }
        }
    }

    pub(super) fn use_local(&mut self, local: LocalId, span: Span, consuming: bool) -> Ty {
        let Some(binding) = self.bindings.get(&local) else {
            self.error("local has no checked binding", span);
            return Ty::Error;
        };
        if binding.affine
            && let Some(moved) = binding.moved
        {
            let declaration = binding.declaration;
            self.push_diagnostic(
                Diagnostic::error("use of moved affine value", span).with_note(format!(
                    "first moved at byte {}; declared at byte {}",
                    moved.start(),
                    declaration.start()
                )),
            );
        }
        let binding = self.bindings.get_mut(&local).expect("binding exists");
        if consuming && binding.affine && binding.moved.is_none() {
            binding.moved = Some(span);
        }
        binding.ty.clone()
    }

    pub(super) fn affine(&mut self, ty: &Ty, span: Span) -> bool {
        self.affine_inner(ty, span, &mut BTreeSet::new(), &mut BTreeMap::new(), 0)
    }

    pub(super) fn affine_inner(
        &mut self,
        ty: &Ty,
        span: Span,
        visiting: &mut BTreeSet<Ty>,
        memo: &mut BTreeMap<Ty, bool>,
        depth: usize,
    ) -> bool {
        if depth > self.limits.max_specialization_depth {
            self.error("affine classification depth limit reached", span);
            return true;
        }
        if !self.charge(span) {
            return false;
        }
        if let Some(&result) = memo.get(ty) {
            return result;
        }
        if !visiting.insert(ty.clone()) {
            return false;
        }
        let affine = match ty {
            Ty::Nominal(item) => {
                if self.item_kind(*item) == Some(ResolvedItemKind::Resource)
                    || self.carriers.contains(item)
                {
                    true
                } else {
                    let fields = self.struct_fields.get(item).cloned().unwrap_or_default();
                    fields.iter().any(|(_, field, _)| {
                        self.concretize(field.clone(), &BTreeMap::new(), span)
                            .is_some_and(|field| {
                                self.affine_inner(&field, span, visiting, memo, depth + 1)
                            })
                    })
                }
            }
            Ty::Specialization {
                template,
                arguments,
            } => {
                if self.carriers.contains(template) {
                    true
                } else {
                    let substitutions: BTreeMap<_, _> = self
                        .nominal_parameters(*template)
                        .into_iter()
                        .zip(arguments.iter().cloned())
                        .collect();
                    let fields = self
                        .struct_fields
                        .get(template)
                        .cloned()
                        .unwrap_or_default();
                    fields.iter().any(|(_, field, _)| {
                        self.concretize(field.clone(), &substitutions, span)
                            .is_some_and(|field| {
                                self.affine_inner(&field, span, visiting, memo, depth + 1)
                            })
                    })
                }
            }
            Ty::List(element) => self.affine_inner(element, span, visiting, memo, depth + 1),
            Ty::Parameter(_) => true,
            Ty::Function { once, .. } => *once,
            Ty::Unit | Ty::Int | Ty::Str | Ty::Error => false,
        };
        visiting.remove(ty);
        memo.insert(ty.clone(), affine);
        affine
    }

    pub(super) fn ty_directly_claims(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Nominal(item) => self.item_kind(*item) == Some(ResolvedItemKind::Resource),
            Ty::List(element) => self.ty_directly_claims(element),
            Ty::Function { once: true, .. } => true,
            Ty::Unit
            | Ty::Int
            | Ty::Str
            | Ty::Parameter(_)
            | Ty::Specialization { .. }
            | Ty::Function { .. }
            | Ty::Error => false,
        }
    }

    pub(super) fn is_nominal_struct(&self, ty: &Ty) -> bool {
        nominal_head(ty).is_some_and(|item| self.structs.contains_key(&item))
    }
}
