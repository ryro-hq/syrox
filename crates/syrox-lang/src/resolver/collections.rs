use super::{
    Expected, ModuleId, Path, ReferenceKind, ResolvedItemKind, ResolvedModuleExport,
    ResolvedReference, ResolvedTarget, Resolver, Span,
};

impl Resolver<'_> {
    pub(super) fn resolve_module_exports(
        &mut self,
        requester: ModuleId,
        namespace: &Path,
        export: &crate::Ident,
        span: Span,
    ) {
        let target = self.collection_namespace(requester, namespace);
        let Some(ResolvedTarget::Module(root)) = target else {
            self.error("module_exports requires a module namespace", namespace.span);
            return;
        };
        self.references.push(ResolvedReference {
            span: namespace.span,
            kind: ReferenceKind::Value,
            target: ResolvedTarget::Module(root),
        });
        let domain = self.modules[root.index()].domain;
        let prefix = self.modules[root.index()].path.segments.clone();
        let mut entries = Vec::new();
        let mut key_bytes = 0_usize;
        for index in 0..self.modules.len() {
            if !self.charge(span) {
                return;
            }
            let module = &self.modules[index];
            if module.domain != domain || !module.path.segments.starts_with(&prefix) {
                continue;
            }
            let Some(&item) = self.module_info[index].exports.get(&export.text) else {
                continue;
            };
            let owner = module.id;
            let bytes = module
                .path
                .segments
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(module.path.segments.len().saturating_mul(2))
                .saturating_add(export.text.len());
            for _ in 0..bytes.saturating_add(1) {
                if !self.charge(span) {
                    return;
                }
            }
            let mut candidate = self.modules[index].path.segments.clone();
            candidate.push(export.text.clone());
            let Some(item) =
                self.visible_from_module(requester, owner, item, &candidate, export.span)
            else {
                continue;
            };
            if self.items[item.index()].kind != ResolvedItemKind::Function {
                self.error(
                    format!(
                        "module export `{}` must be a function",
                        candidate.join("::")
                    ),
                    export.span,
                );
                continue;
            }
            let relative = &candidate[prefix.len()..candidate.len() - 1];
            let key = relative.join("::");
            key_bytes = key_bytes.saturating_add(key.len().saturating_add(1));
            entries.push(ResolvedModuleExport { key, item });
        }
        // Structural path order differs from byte order when one path is a
        // prefix of another (e.g. a::x versus a0), so sort the actual keys.
        let levels = usize::try_from(entries.len().checked_ilog2().unwrap_or(0) + 1)
            .expect("logarithm fits usize");
        for _ in 0..key_bytes.saturating_mul(levels).saturating_mul(2) {
            if !self.charge(span) {
                return;
            }
        }
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        self.module_exports.insert(
            (span.source_id().index(), span.start(), span.end()),
            entries,
        );
    }

    fn collection_namespace(
        &mut self,
        requester: ModuleId,
        namespace: &Path,
    ) -> Option<ResolvedTarget> {
        if namespace.segments.len() == 1 {
            let alias = &namespace.segments[0].text;
            let domain = self
                .input_domains
                .get(&self.modules[requester.index()].domain)
                .and_then(|inputs| inputs.get(alias))
                .copied();
            if let Some(root) =
                domain.and_then(|domain| self.module_ids.get(&(domain, Vec::new())).copied())
            {
                return Some(ResolvedTarget::Module(root));
            }
        }
        self.lookup(requester, namespace, Expected::Any)
    }
}
