//! Conservative, inspection-only relocation. Successful coordinate conversion
//! does not replay work/metadata costs or shared generic-instance deduplication.
use super::SemanticAnalysis;
use crate::checker::FactMapper;
use crate::{
    AnalysisCancellation, AnalysisCancelled, CanonicalItemIdentity, ItemId, LocalId,
    OwnerCheckedFacts, OwnerSourceMap, ResolutionOwnerKey, Span,
};
use std::collections::{BTreeMap, BTreeSet};

struct Coordinates<'a> {
    old: &'a SemanticAnalysis,
    new: &'a SemanticAnalysis,
    cancel: &'a AnalysisCancellation,
    owners: Vec<(&'a ResolutionOwnerKey, Span, Option<usize>)>,
    items: BTreeMap<CanonicalItemIdentity, Option<ItemId>>,
    maps: BTreeMap<ResolutionOwnerKey, (OwnerSourceMap, OwnerSourceMap)>,
    locals: BTreeMap<LocalId, LocalId>,
    units: usize,
    bytes: usize,
}

impl SemanticAnalysis {
    /// Relocate the ordered effect journal independently of inspection facts.
    /// Replay requires the receiver's prefix state and remains simulation-only.
    pub fn remap_body_effects_from(
        &self,
        previous: &Self,
        key: &ResolutionOwnerKey,
        cancel: &AnalysisCancellation,
    ) -> Result<Option<crate::BodyEffects>, AnalysisCancelled> {
        if !self.body_dependencies_match(previous, key, cancel)? {
            return Ok(None);
        }
        let result = (|| {
            let mut map = Coordinates::new(previous, self, key, cancel, 262_144, 8 * 1024 * 1024)?;
            map.publication(key)?.effects.as_ref()?.remap(&mut map)
        })();
        cancel.check()?;
        Ok(result)
    }

    fn body_dependencies_match(
        &self,
        previous: &Self,
        key: &ResolutionOwnerKey,
        cancel: &AnalysisCancellation,
    ) -> Result<bool, AnalysisCancelled> {
        cancel.check()?;
        if previous.bodies.truncated || self.bodies.truncated || previous.policy != self.policy {
            return Ok(false);
        }
        let Some(before) = previous.owner_type_dependencies(key, cancel)? else {
            return Ok(false);
        };
        let Some(after) = self.owner_type_dependencies(key, cancel)? else {
            return Ok(false);
        };
        Ok(before == after)
    }

    /// Relocate a previous body's inspection facts into this snapshot. Guards are
    /// intentionally conservative (including exact text of external span owners).
    /// This never installs facts or bypasses checking. `None` means unavailable,
    /// changed dependencies, ambiguous identity, truncation or budget exhaustion.
    pub fn remap_checked_body_from(
        &self,
        previous: &Self,
        key: &ResolutionOwnerKey,
        cancel: &AnalysisCancellation,
    ) -> Result<Option<OwnerCheckedFacts>, AnalysisCancelled> {
        self.remap_checked_body_bounded(previous, key, cancel, 262_144, 8 * 1024 * 1024)
    }

    fn remap_checked_body_bounded(
        &self,
        previous: &Self,
        key: &ResolutionOwnerKey,
        cancel: &AnalysisCancellation,
        units: usize,
        bytes: usize,
    ) -> Result<Option<OwnerCheckedFacts>, AnalysisCancelled> {
        if !self.body_dependencies_match(previous, key, cancel)? {
            return Ok(None);
        }
        let result = (|| {
            let mut map = Coordinates::new(previous, self, key, cancel, units, bytes)?;
            let body = map.publication(key)?;
            Some(OwnerCheckedFacts {
                diagnostics: previous
                    .body_diagnostics
                    .get(body.diagnostics.clone())?
                    .iter()
                    .map(|value| map.diagnostic(value))
                    .collect::<Option<_>>()?,
                expressions: previous
                    .expressions
                    .get(body.expressions.clone())?
                    .iter()
                    .map(|value| map.expression(value))
                    .collect::<Option<_>>()?,
                patterns: previous
                    .editor
                    .patterns
                    .get(body.patterns.clone())?
                    .iter()
                    .map(|value| map.pattern(value))
                    .collect::<Option<_>>()?,
                locals: body
                    .locals
                    .iter()
                    .map(|id| {
                        let value = map.binding(previous.editor.locals.get(id)?)?;
                        Some((value.id, value))
                    })
                    .collect::<Option<_>>()?,
                ownership: previous
                    .editor
                    .uses
                    .get(body.uses.clone())?
                    .iter()
                    .map(|value| map.ownership(value))
                    .collect::<Option<_>>()?,
                fields: previous
                    .editor
                    .fields
                    .get(body.fields.clone())?
                    .iter()
                    .map(|(span, declaration, ty)| {
                        Some((map.span(*span)?, map.span(*declaration)?, map.ty(ty)?))
                    })
                    .collect::<Option<_>>()?,
                arguments: previous
                    .editor
                    .arguments
                    .get(body.arguments.clone())?
                    .iter()
                    .map(|value| map.argument(value))
                    .collect::<Option<_>>()?,
                outputs: body
                    .outputs
                    .iter()
                    .map(|id| Some((map.item(*id)?, map.ty(previous.editor.outputs.get(id)?)?)))
                    .collect::<Option<_>>()?,
            })
        })();
        cancel.check()?;
        Ok(result)
    }
}

impl<'a> Coordinates<'a> {
    fn new(
        old: &'a SemanticAnalysis,
        new: &'a SemanticAnalysis,
        key: &ResolutionOwnerKey,
        cancel: &'a AnalysisCancellation,
        units: usize,
        bytes: usize,
    ) -> Option<Self> {
        let mut map = Self {
            old,
            new,
            cancel,
            owners: Vec::new(),
            items: BTreeMap::new(),
            maps: BTreeMap::new(),
            locals: BTreeMap::new(),
            units,
            bytes,
        };
        map.initialize()?;
        map.ensure_owner(key)?;
        Some(map)
    }
    fn publication(
        &mut self,
        key: &ResolutionOwnerKey,
    ) -> Option<&'a crate::checker::BodyPublication> {
        let current_span = self.maps.get(key)?.1.span();
        let mut current = None;
        for body in &self.new.bodies.entries {
            self.reserve(0)?;
            if body.anchor.source_id() == current_span.source_id()
                && current_span.start() <= body.anchor.start()
                && body.anchor.end() <= current_span.end()
            {
                if current.is_some() || !body.complete {
                    return None;
                }
                current = Some(body);
            }
        }
        current?;
        let mut publication = None;
        for candidate in &self.old.bodies.entries {
            self.reserve(0)?;
            if self.span_owner(candidate.anchor) == Some(key) {
                if publication.is_some() || !candidate.complete {
                    return None;
                }
                publication = Some(candidate);
            }
        }
        publication
    }

    fn initialize(&mut self) -> Option<()> {
        if self.old.sources.input_domains() != self.new.sources.input_domains()
            || self.old.sources.project_roots() != self.new.sources.project_roots()
        {
            return None;
        }
        let mut topologies = Vec::new();
        for analysis in [self.old, self.new] {
            let mut topology = BTreeSet::new();
            for (id, source) in analysis.sources.iter() {
                self.reserve(0)?;
                let identity = (
                    analysis.sources.domain(id)?,
                    analysis.sources.module(id)?,
                    source.name(),
                );
                if !topology.insert(identity) {
                    return None;
                }
            }
            topologies.push(topology);
        }
        if topologies[0] != topologies[1] {
            return None;
        }
        for (key, span) in self.old.resolution_owners() {
            self.reserve(0)?;
            self.owners.push((key, span, None));
        }
        self.owners.sort_by_key(|(_, span, _)| {
            (
                span.source_id(),
                span.start(),
                std::cmp::Reverse(span.end()),
            )
        });
        let mut stack: Vec<usize> = Vec::new();
        for index in 0..self.owners.len() {
            self.reserve(0)?;
            let span = self.owners[index].1;
            while let Some(&parent) = stack.last() {
                let outer = self.owners[parent].1;
                if outer.source_id() == span.source_id()
                    && outer.start() <= span.start()
                    && span.end() <= outer.end()
                {
                    if outer == span {
                        return None;
                    }
                    break;
                }
                stack.pop();
            }
            self.owners[index].2 = stack.last().copied();
            stack.push(index);
        }
        for item in self.new.items() {
            for segment in item.path().segments() {
                self.reserve(segment.len())?;
            }
            self.reserve(0)?;
            let identity = CanonicalItemIdentity::from_resolved(item);
            self.items
                .entry(identity)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(item.id()));
        }
        Some(())
    }

    fn span_owner(&mut self, span: Span) -> Option<&'a ResolutionOwnerKey> {
        let mut index = self
            .owners
            .partition_point(|(_, owner, _)| {
                (owner.source_id(), owner.start()) <= (span.source_id(), span.start())
            })
            .checked_sub(1)?;
        loop {
            self.reserve(0)?;
            let (key, owner, parent) = self.owners[index];
            if owner.source_id() == span.source_id()
                && owner.start() <= span.start()
                && span.end() <= owner.end()
            {
                return Some(key);
            }
            index = parent?;
        }
    }

    fn ensure_owner(&mut self, key: &ResolutionOwnerKey) -> Option<()> {
        self.reserve(0)?;
        if self.maps.contains_key(key) {
            return Some(());
        }
        let old = self.old.owner_resolution(key, self.cancel).ok()??;
        let new = self.new.owner_resolution(key, self.cancel).ok()??;
        let old_span = old.source_map.span();
        let new_span = new.source_map.span();
        let old_text = self
            .old
            .sources
            .get(old_span.source_id())?
            .text()
            .get(old_span.range())?;
        let new_text = self
            .new
            .sources
            .get(new_span.source_id())?
            .text()
            .get(new_span.range())?;
        if old_text != new_text
            || !old.namespace_dependencies_complete
            || !new.namespace_dependencies_complete
            || old.locals != new.locals
            || old.references != new.references
            || old.diagnostics != new.diagnostics
            || old.namespace_dependencies != new.namespace_dependencies
        {
            return None;
        }
        for ordinal in 0..old.locals.len() {
            self.reserve(0)?;
            let ordinal = u32::try_from(ordinal).ok()?;
            self.locals.insert(
                old.source_map.local(ordinal)?,
                new.source_map.local(ordinal)?,
            );
        }
        for part in &key.path {
            self.reserve(part.len())?;
        }
        self.maps
            .insert(key.clone(), (old.source_map, new.source_map));
        Some(())
    }
}

impl FactMapper for Coordinates<'_> {
    fn reference_scan(&mut self, references: &[Span]) -> Option<std::sync::Arc<[Span]>> {
        if references.len() != self.new.references().len() {
            return None;
        }
        let mut mapped = Vec::new();
        for (span, current) in references.iter().zip(self.new.references()) {
            let span = self.span(*span)?;
            if span != current.span() {
                return None;
            }
            mapped.push(span);
        }
        Some(mapped.into())
    }
    fn reserve(&mut self, bytes: usize) -> Option<()> {
        self.cancel.check().ok()?;
        self.units = self.units.checked_sub(1)?;
        self.bytes = self.bytes.checked_sub(bytes)?;
        Some(())
    }

    fn span(&mut self, span: Span) -> Option<Span> {
        self.reserve(0)?;
        let key = self.span_owner(span)?;
        self.ensure_owner(key)?;
        let (old, new) = self.maps.get(key)?;
        new.absolute_span(old.relative_span(span)?)
    }

    fn item(&mut self, id: ItemId) -> Option<ItemId> {
        self.reserve(0)?;
        let old = self.old.items().nth(id.index())?;
        for segment in old.path().segments() {
            self.reserve(segment.len())?;
        }
        let identity = CanonicalItemIdentity::from_resolved(old);
        let new = (*self.items.get(&identity)?)?;
        (self.new.items().nth(new.index())?.kind() == old.kind()).then_some(new)
    }

    fn local(&mut self, id: LocalId) -> Option<LocalId> {
        self.reserve(0)?;
        if let Some(mapped) = self.locals.get(&id) {
            return Some(*mapped);
        }
        let span = self.old.locals().nth(id.index())?.span();
        let key = self.span_owner(span)?;
        self.ensure_owner(key)?;
        self.locals.get(&id).copied()
    }
}

#[cfg(test)]
mod tests;
