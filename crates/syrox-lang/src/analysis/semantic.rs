use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use thiserror::Error;

use super::{AnalysisCancellation, AnalysisCancelled, AnalysisRevision, AnalysisSnapshot};
use crate::{
    CheckPolicy, CheckedExpression, Diagnostic, ItemId, MAX_DIAGNOSTICS, ParsedSource,
    ParsedSources, ResolvedItem, ResolvedProgram, ResolvedReference, ResolvedTarget, SourceError,
    SourceId, SourceSet, Span, Ty,
};

#[cfg(test)]
mod tests;

#[derive(Debug, Error)]
pub enum SemanticAnalysisError {
    #[error(transparent)]
    Cancelled(#[from] AnalysisCancelled),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("project binding refers to an unknown source or missing document")]
    InvalidBinding,
}

/// Inspection-only semantic data. Recovery never produces a public resolved or
/// checked program, and this value cannot be passed to the evaluator.
#[derive(Debug)]
pub struct SemanticAnalysis {
    revision: AnalysisRevision,
    bindings: Arc<BTreeMap<SourceId, String>>,
    pub(super) sources: Arc<SourceSet>,
    pub(super) resolved: Arc<ResolvedProgram>,
    diagnostics: Arc<[Diagnostic]>,
    pub(super) expressions: Arc<[CheckedExpression]>,
    functions: Arc<BTreeMap<ItemId, Ty>>,
    pub(super) editor: Arc<crate::checker::EditorFacts>,
    pub(super) symbols: Arc<std::sync::OnceLock<super::symbols::SymbolIndex>>,
    pub(super) interfaces: Arc<std::sync::OnceLock<super::interfaces::InterfaceIndex>>,
    pub(super) policy: Arc<CheckPolicy>,
    pub(super) bodies: Arc<crate::checker::BodyPublications>,
    pub(super) body_diagnostics: Arc<[Diagnostic]>,
}

impl SemanticAnalysis {
    #[cfg(test)]
    pub(crate) fn test_resolved(&self) -> &ResolvedProgram {
        &self.resolved
    }
    /// Structural resolution owners in this topology. Ambiguous declarations
    /// and truncated owner metadata are not returned.
    pub fn resolution_owners(&self) -> impl Iterator<Item = (&crate::ResolutionOwnerKey, Span)> {
        self.resolved.resolution_owners()
    }

    pub fn resolution_owners_truncated(&self) -> bool {
        self.resolved.resolution_owners_truncated()
    }

    /// Snapshot-local source maps plus relocatable resolution facts. This does
    /// not validate namespace/interface dependencies for cross-revision reuse.
    pub fn owner_resolution(
        &self,
        key: &crate::ResolutionOwnerKey,
        cancel: &AnalysisCancellation,
    ) -> Result<Option<crate::OwnerResolution>, AnalysisCancelled> {
        self.resolved.owner_resolution(key, cancel)
    }
    pub fn complete_type_path(
        &self,
        source: SourceId,
        offset: u32,
        qualifier: &[String],
        prefix: &str,
    ) -> Vec<(String, ResolvedTarget)> {
        self.resolved
            .editor_completions(source, offset, qualifier, prefix, true)
    }
    /// Editor-only retention limits do not turn valid code into a language error.
    pub fn editor_facts_truncated(&self) -> bool {
        self.editor.truncated
    }
    pub fn display_type(&self, ty: &Ty) -> String {
        self.resolved.display_type(ty)
    }

    pub fn locals(&self) -> impl Iterator<Item = &crate::ResolvedLocal> {
        self.resolved.locals()
    }

    pub fn local_type(&self, id: crate::LocalId) -> Option<&Ty> {
        self.editor.locals.get(&id).map(|local| &local.ty)
    }

    pub fn checked_locals(&self) -> impl Iterator<Item = &crate::CheckedLocal> {
        self.editor.locals.values()
    }

    pub fn ownership_uses(&self) -> &[crate::OwnershipUse] {
        &self.editor.uses
    }
    pub fn parameter_hints(&self) -> &[crate::ParameterHint] {
        &self.editor.arguments
    }

    pub fn local_at(&self, source: SourceId, offset: u32) -> Option<&crate::CheckedLocal> {
        if let Some(local) = self
            .editor
            .locals
            .values()
            .find(|local| contains(local.declaration, source, offset))
        {
            return Some(local);
        }
        self.references()
            .filter(|reference| contains(reference.span(), source, offset))
            .find_map(|reference| match reference.target() {
                ResolvedTarget::Local(id) => self.editor.locals.get(id),
                _ => None,
            })
    }

    pub fn type_at(&self, source: SourceId, offset: u32) -> Option<&Ty> {
        if let Some((_, _, ty)) = self
            .editor
            .fields
            .iter()
            .find(|(span, _, _)| contains(*span, source, offset))
        {
            return Some(ty);
        }
        self.local_at(source, offset)
            .map(|local| &local.ty)
            .or_else(|| {
                self.expression_at(source, offset)
                    .map(CheckedExpression::ty)
            })
    }

    pub fn expected_type_at(&self, source: SourceId, offset: u32) -> Option<&Ty> {
        self.expression_at(source, offset)?.expected_type()
    }
    /// Bounded namespace completion. Every spelling is checked by the resolver,
    /// including imports, reexports and closed module interfaces.
    pub fn complete_path(
        &self,
        source: SourceId,
        offset: u32,
        qualifier: &[String],
        prefix: &str,
    ) -> Vec<(String, ResolvedTarget)> {
        if self
            .sources
            .get(source)
            .is_none_or(|source| !source.text().is_char_boundary(offset as usize))
        {
            return Vec::new();
        }
        if !qualifier.is_empty()
            && let Some(ResolvedTarget::Item(enumeration)) =
                self.lookup_path(source, offset, qualifier)
        {
            let variants: Vec<_> = self
                .variants(enumeration)
                .enumerate()
                .filter(|(_, (name, _))| name.starts_with(prefix))
                .take(256)
                .map(|(index, (name, _))| {
                    (
                        name.to_owned(),
                        ResolvedTarget::EnumVariant {
                            enumeration,
                            index: u32::try_from(index).expect("bounded variants"),
                        },
                    )
                })
                .collect();
            if !variants.is_empty() {
                return variants;
            }
        }
        self.resolved
            .editor_completions(source, offset, qualifier, prefix, false)
    }

    /// Resolve an editor path without requiring a complete expression body.
    pub fn lookup_path(
        &self,
        source: SourceId,
        offset: u32,
        parts: &[String],
    ) -> Option<ResolvedTarget> {
        if !self
            .sources
            .get(source)?
            .text()
            .is_char_boundary(offset as usize)
        {
            return None;
        }
        self.resolved.editor_lookup(source, offset, parts)
    }
    pub fn revision(&self) -> AnalysisRevision {
        self.revision.clone()
    }
    pub fn sources(&self) -> &SourceSet {
        &self.sources
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    pub fn items(&self) -> impl ExactSizeIterator<Item = &ResolvedItem> {
        self.resolved.items()
    }
    pub fn references(&self) -> impl ExactSizeIterator<Item = &ResolvedReference> {
        self.resolved.references()
    }
    pub fn function_type(&self, item: ItemId) -> Option<&Ty> {
        self.functions.get(&item)
    }
    /// Syntactic parameter labels for a resolved function identity, including a
    /// function reached through a reexport. Does not imply a valid body.
    pub fn function_parameters(&self, item: ItemId) -> Option<&[crate::Parameter]> {
        let span = self
            .resolved
            .items()
            .find(|candidate| candidate.id() == item)?
            .span();
        let file = self
            .resolved
            .parsed()
            .iter()
            .find(|file| file.source_id() == span.source_id())?;
        let mut pending: Vec<_> = file.program().items.iter().collect();
        while let Some(item) = pending.pop() {
            match &item.kind {
                crate::ItemKind::Module(module) => pending.extend(&module.items),
                crate::ItemKind::Function(function) if function.name.span == span => {
                    return Some(&function.parameters);
                }
                _ => {}
            }
        }
        None
    }
    pub fn expression_at(&self, source: SourceId, offset: u32) -> Option<&CheckedExpression> {
        self.expressions
            .iter()
            .filter(|expression| contains(expression.span(), source, offset))
            .min_by_key(|expression| expression.span().end() - expression.span().start())
    }
    pub fn definition_at(&self, source: SourceId, offset: u32) -> Option<Span> {
        if let Ok(Some(symbol)) = self.symbol_at(source, offset, &AnalysisCancellation::default()) {
            return Some(symbol.declaration);
        }
        let reference = self
            .references()
            .filter(|reference| contains(reference.span(), source, offset))
            .min_by_key(|reference| reference.span().end() - reference.span().start())?;
        match reference.target() {
            ResolvedTarget::Item(id)
            | ResolvedTarget::EnumVariant {
                enumeration: id, ..
            } => self
                .items()
                .find(|item| item.id() == *id)
                .map(ResolvedItem::span),
            ResolvedTarget::Local(id) => self
                .resolved
                .locals()
                .find(|local| local.id() == *id)
                .map(crate::ResolvedLocal::span),
            _ => None,
        }
    }
}

fn contains(span: Span, source: SourceId, offset: u32) -> bool {
    span.source_id() == source && span.start() <= offset && offset < span.end()
}

impl AnalysisSnapshot {
    /// Revalidate immutable semantic data against the effective documents of a
    /// newer snapshot using the original bindings. Topology and policy remain
    /// those of the previous analysis; no binding can be omitted by the caller.
    /// A fresh publication stamp never makes old text current by itself.
    pub fn revalidate_project_analysis(
        &self,
        previous: &SemanticAnalysis,
        cancellation: &AnalysisCancellation,
    ) -> Result<Option<SemanticAnalysis>, SemanticAnalysisError> {
        cancellation.check()?;
        for (id, key) in previous.bindings.iter() {
            cancellation.check()?;
            let source = previous
                .sources
                .get(*id)
                .ok_or(SemanticAnalysisError::InvalidBinding)?;
            let document = self
                .document(key)
                .ok_or(SemanticAnalysisError::InvalidBinding)?;
            if document.source().text() != source.text() {
                return Ok(None);
            }
        }
        Ok(Some(SemanticAnalysis {
            revision: self.revision(),
            bindings: previous.bindings.clone(),
            sources: previous.sources.clone(),
            resolved: previous.resolved.clone(),
            diagnostics: previous.diagnostics.clone(),
            expressions: previous.expressions.clone(),
            functions: previous.functions.clone(),
            editor: previous.editor.clone(),
            symbols: previous.symbols.clone(),
            interfaces: previous.interfaces.clone(),
            policy: previous.policy.clone(),
            bodies: previous.bodies.clone(),
            body_diagnostics: previous.body_diagnostics.clone(),
        }))
    }

    /// Compose file queries with caller-supplied domains, module paths and input
    /// bindings. All source IDs in the result refer to this topology; names need
    /// not be unique across domains. Unbound sources keep their supplied text.
    pub fn analyze_project(
        &self,
        topology: &SourceSet,
        bindings: &BTreeMap<SourceId, String>,
        policy: &CheckPolicy,
        cancellation: &AnalysisCancellation,
    ) -> Result<SemanticAnalysis, SemanticAnalysisError> {
        self.analyze_project_with_limits(
            topology,
            bindings,
            policy,
            cancellation,
            crate::CheckLimits::default(),
        )
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn analyze_project_with_limits(
        &self,
        topology: &SourceSet,
        bindings: &BTreeMap<SourceId, String>,
        policy: &CheckPolicy,
        cancellation: &AnalysisCancellation,
        limits: crate::CheckLimits,
    ) -> Result<SemanticAnalysis, SemanticAnalysisError> {
        cancellation.check()?;
        if bindings.keys().any(|id| topology.get(*id).is_none()) {
            return Err(SemanticAnalysisError::InvalidBinding);
        }
        let mut sources = topology.clone();
        let mut parsed = Vec::with_capacity(sources.len());
        let mut diagnostics = Vec::new();
        let mut incomplete = BTreeSet::new();
        for (id, original) in topology.iter() {
            cancellation.check()?;
            let file = if let Some(key) = bindings.get(&id) {
                let document = self
                    .document(key)
                    .ok_or(SemanticAnalysisError::InvalidBinding)?;
                sources.replace_text(id, document.source().text())?;
                let cache = document
                    .document
                    .analysis
                    .bound
                    .lock()
                    .expect("file cache lock is not poisoned");
                let cached = cache
                    .as_ref()
                    .filter(|(bound, _)| *bound == id)
                    .map(|(_, file)| file.clone());
                drop(cache);
                if let Some(file) = cached {
                    file
                } else {
                    let file = Arc::new(crate::parser::parse_file_cancellable(
                        id,
                        document.source(),
                        Some(cancellation),
                    )?);
                    cancellation.check()?;
                    *document
                        .document
                        .analysis
                        .bound
                        .lock()
                        .expect("file cache lock is not poisoned") = Some((id, file.clone()));
                    file
                }
            } else {
                Arc::new(crate::parser::parse_file_cancellable(
                    id,
                    original,
                    Some(cancellation),
                )?)
            };
            cancellation.check()?;
            diagnostics.extend(
                file.diagnostics()
                    .iter()
                    .take(MAX_DIAGNOSTICS.saturating_sub(diagnostics.len()))
                    .cloned(),
            );
            incomplete.extend(file.incomplete_bodies.iter().map(|span| {
                (
                    u32::try_from(span.source_id().index()).expect("bounded source id"),
                    span.start(),
                    span.end(),
                )
            }));
            parsed.push(ParsedSource {
                source_id: id,
                domain: sources.domain(id).expect("registered domain"),
                module: sources.module(id).expect("registered module").to_vec(),
                program: file.recovered_program().clone(),
            });
        }
        let parsed = ParsedSources {
            sources: parsed,
            input_domains: topology.input_domains().cloned().unwrap_or_default(),
            project_roots: topology.project_roots().clone(),
        };
        let (resolved, errors, exhausted) =
            crate::resolver::resolve_partial(parsed, Some(cancellation));
        cancellation.check()?;
        diagnostics.extend(
            errors
                .into_iter()
                .take(MAX_DIAGNOSTICS.saturating_sub(diagnostics.len())),
        );
        let (expressions, functions, editor, bodies, body_diagnostics) = if exhausted {
            (
                Vec::new(),
                BTreeMap::new(),
                crate::checker::EditorFacts::default(),
                crate::checker::BodyPublications {
                    truncated: true,
                    ..Default::default()
                },
                Vec::new(),
            )
        } else {
            let checked =
                crate::checker::check_partial(&resolved, policy, incomplete, cancellation, limits);
            cancellation.check()?;
            diagnostics.extend(
                checked
                    .diagnostics
                    .iter()
                    .take(MAX_DIAGNOSTICS.saturating_sub(diagnostics.len()))
                    .cloned(),
            );
            (
                checked.expressions,
                checked.functions,
                checked.editor,
                checked.bodies,
                checked.diagnostics,
            )
        };
        diagnostics.sort_by_key(|diagnostic| {
            (
                diagnostic.span.source_id(),
                diagnostic.span.start(),
                diagnostic.message.clone(),
            )
        });
        Ok(SemanticAnalysis {
            revision: self.revision(),
            bindings: Arc::new(bindings.clone()),
            sources: Arc::new(sources),
            resolved: Arc::new(resolved),
            diagnostics: diagnostics.into(),
            expressions: expressions.into(),
            functions: Arc::new(functions),
            editor: Arc::new(editor),
            symbols: Arc::new(std::sync::OnceLock::new()),
            interfaces: Arc::new(std::sync::OnceLock::new()),
            policy: Arc::new(policy.clone()),
            bodies: Arc::new(bodies),
            body_diagnostics: body_diagnostics.into(),
        })
    }
}
