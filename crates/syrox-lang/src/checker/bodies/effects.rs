//! Ordered observations of global decisions, separate from language budgets.
use super::{BodyBudget, Checker};
use crate::{AnalysisCancellation, AnalysisCancelled, CheckLimits, Span, Ty};
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyReplayState {
    pub budget: BodyBudget,
    /// Snapshot-local types, in the same coordinates as the journal.
    pub instances: BTreeSet<Ty>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::checker) enum EffectOperation {
    WorkProbe(usize),
    Metadata,
    EditorUnit,
    EditorType(usize),
    Diagnostic,
    Instance { ty: Ty, already_present: bool },
    ReferenceScan(Arc<[Span]>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::checker) struct EffectObservation {
    position: BodyBudget,
    span: Span,
    operation: EffectOperation,
}

/// Inspection-only journal. Replay does not install facts or construct a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyEffects {
    entry: BodyBudget,
    exit: BodyBudget,
    limits: CheckLimits,
    observations: Vec<EffectObservation>,
}

impl BodyEffects {
    pub(super) fn new(
        entry: BodyBudget,
        exit: BodyBudget,
        limits: CheckLimits,
        observations: Vec<EffectObservation>,
    ) -> Self {
        Self {
            entry,
            exit,
            limits,
            observations,
        }
    }
    pub const fn entry(&self) -> BodyBudget {
        self.entry
    }
    pub const fn exit(&self) -> BodyBudget {
        self.exit
    }
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }

    /// Simulate unchanged decisions against a different prefix. A failed replay
    /// never mutates `input`; callers must cold-check instead of publishing it.
    /// Across snapshots, first remap the journal and validate its dependencies;
    /// this scalar simulation cannot inspect the target reference inventory.
    pub fn replay(
        &self,
        input: &BodyReplayState,
        limits: CheckLimits,
        cancel: &AnalysisCancellation,
    ) -> Result<Option<BodyReplayState>, AnalysisCancelled> {
        cancel.check()?;
        if limits != self.limits
            || !usable(self.entry)
            || !usable(self.exit)
            || !usable(input.budget)
            || input.instances.len() != input.budget.generic_instances
            || input.instances.len() > limits.max_generic_instances
        {
            return Ok(None);
        }
        let mut state = BodyReplayState {
            budget: input.budget,
            instances: BTreeSet::new(),
        };
        let mut units = 262_144;
        for ty in &input.instances {
            if !type_fits(ty, &mut units, cancel) {
                cancel.check()?;
                return Ok(None);
            }
            state.instances.insert(ty.clone());
        }
        let result = (|| {
            let mut previous = self.entry;
            for observation in &self.observations {
                cancel.check().ok()?;
                advance(&mut state.budget, previous, observation.position, limits)?;
                let position = state.budget;
                match &observation.operation {
                    EffectOperation::WorkProbe(cost) => {
                        if position.work.checked_add(*cost)? > limits.max_work {
                            return None;
                        }
                    }
                    EffectOperation::Metadata => {
                        if position.metadata >= limits.max_metadata_units {
                            return None;
                        }
                    }
                    EffectOperation::EditorUnit => {
                        if position.editor_metadata >= limits.max_metadata_units {
                            return None;
                        }
                    }
                    EffectOperation::EditorType(cost) => {
                        if position.editor_metadata.checked_add(*cost)? > limits.max_metadata_units
                        {
                            return None;
                        }
                    }
                    EffectOperation::Diagnostic => {
                        if position.diagnostics >= crate::MAX_DIAGNOSTICS {
                            return None;
                        }
                    }
                    EffectOperation::Instance { ty, .. } => {
                        if !state.instances.contains(ty) {
                            if state.instances.len() >= limits.max_generic_instances
                                || !type_fits(ty, &mut units, cancel)
                            {
                                return None;
                            }
                            state.instances.insert(ty.clone());
                            state.budget.generic_instances = state.instances.len();
                        }
                    }
                    EffectOperation::ReferenceScan(_) => {}
                }
                previous = observation.position;
            }
            advance(&mut state.budget, previous, self.exit, limits)?;
            usable(state.budget).then_some(state)
        })();
        cancel.check()?;
        Ok(result)
    }

    pub(crate) fn remap(&self, mapper: &mut impl crate::checker::FactMapper) -> Option<Self> {
        let observations = self
            .observations
            .iter()
            .map(|observation| {
                mapper.reserve(0)?;
                Some(EffectObservation {
                    position: observation.position,
                    span: mapper.span(observation.span)?,
                    operation: match &observation.operation {
                        EffectOperation::ReferenceScan(references) => {
                            EffectOperation::ReferenceScan(mapper.reference_scan(references)?)
                        }
                        EffectOperation::Instance {
                            ty,
                            already_present,
                        } => EffectOperation::Instance {
                            ty: mapper.ty(ty)?,
                            already_present: *already_present,
                        },
                        other => other.clone(),
                    },
                })
            })
            .collect::<Option<_>>()?;
        Some(Self {
            entry: self.entry,
            exit: self.exit,
            limits: self.limits,
            observations,
        })
    }
}

fn usable(position: BodyBudget) -> bool {
    !position.exhausted
        && !position.editor_truncated
        && position.diagnostics < crate::MAX_DIAGNOSTICS
}

fn advance(
    position: &mut BodyBudget,
    before: BodyBudget,
    after: BodyBudget,
    limits: CheckLimits,
) -> Option<()> {
    if !usable(before) || !usable(after) {
        return None;
    }
    position.work = position
        .work
        .checked_add(after.work.checked_sub(before.work)?)?;
    position.metadata = position
        .metadata
        .checked_add(after.metadata.checked_sub(before.metadata)?)?;
    position.editor_metadata = position
        .editor_metadata
        .checked_add(after.editor_metadata.checked_sub(before.editor_metadata)?)?;
    position.diagnostics = position
        .diagnostics
        .checked_add(after.diagnostics.checked_sub(before.diagnostics)?)?;
    (position.work <= limits.max_work
        && position.metadata <= limits.max_metadata_units
        && position.editor_metadata <= limits.max_metadata_units
        && position.diagnostics < crate::MAX_DIAGNOSTICS)
        .then_some(())
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::checker) struct EffectRetention {
    remaining: usize,
    pub truncated: bool,
}
impl Default for EffectRetention {
    fn default() -> Self {
        Self {
            remaining: 262_144,
            truncated: false,
        }
    }
}

impl Checker<'_> {
    pub(in crate::checker) fn observe_reference_scan(&mut self, span: Span) {
        if !self.body.active || self.cancellation.is_none() || self.effect_retention.truncated {
            return;
        }
        if self.reference_inventory.is_none() {
            let count = self.program.references().len();
            if count > self.effect_retention.remaining {
                self.discard_effects();
                return;
            }
            self.effect_retention.remaining -= count;
            let cancel = self
                .cancellation
                .expect("editor observation requires cancellation");
            let mut inventory = Vec::with_capacity(count);
            for reference in self.program.references() {
                if cancel.check().is_err() {
                    self.discard_effects();
                    return;
                }
                inventory.push(reference.span());
            }
            self.reference_inventory = Some(inventory.into());
        }
        self.observe_effect(
            span,
            EffectOperation::ReferenceScan(self.reference_inventory.as_ref().unwrap().clone()),
        );
    }
    pub(in crate::checker) fn observe_effect(&mut self, span: Span, operation: EffectOperation) {
        if !self.body.active || self.cancellation.is_none() || self.effect_retention.truncated {
            return;
        }
        let before = self.budget_position();
        self.observe_effect_at(before, span, operation);
    }

    pub(in crate::checker) fn observe_effect_at(
        &mut self,
        position: BodyBudget,
        span: Span,
        operation: EffectOperation,
    ) {
        if !self.body.active || self.cancellation.is_none() || self.effect_retention.truncated {
            return;
        }
        if self.effect_retention.remaining == 0 {
            self.discard_effects();
            return;
        }
        self.effect_retention.remaining -= 1;
        self.body.effects.push(EffectObservation {
            position,
            span,
            operation,
        });
    }

    pub(in crate::checker) fn observe_instance(&mut self, span: Span, ty: &Ty) {
        if !self.body.active || self.effect_retention.truncated {
            return;
        }
        let Some(cancel) = self.cancellation else {
            return;
        };
        if !type_fits(ty, &mut self.effect_retention.remaining, cancel) {
            self.discard_effects();
            return;
        }
        self.observe_effect(
            span,
            EffectOperation::Instance {
                ty: ty.clone(),
                already_present: self.generic_instances.contains(ty),
            },
        );
    }

    fn discard_effects(&mut self) {
        self.reference_inventory = None;
        self.effect_retention.truncated = true;
        self.body.effects.clear();
        for publication in &mut self.publications.entries {
            publication.effects = None;
        }
    }
}

fn type_fits(ty: &Ty, remaining: &mut usize, cancel: &AnalysisCancellation) -> bool {
    type_fits_at(ty, remaining, cancel, 0)
}

fn type_fits_at(
    ty: &Ty,
    remaining: &mut usize,
    cancel: &AnalysisCancellation,
    depth: usize,
) -> bool {
    if *remaining == 0 || depth > crate::MAX_DEPTH || cancel.check().is_err() {
        return false;
    }
    *remaining -= 1;
    match ty {
        Ty::Specialization { arguments, .. } => arguments
            .iter()
            .all(|ty| type_fits_at(ty, remaining, cancel, depth + 1)),
        Ty::Function {
            parameters, result, ..
        } => {
            parameters
                .iter()
                .all(|ty| type_fits_at(ty, remaining, cancel, depth + 1))
                && type_fits_at(result, remaining, cancel, depth + 1)
        }
        Ty::List(inner) => type_fits_at(inner, remaining, cancel, depth + 1),
        Ty::Unit | Ty::Int | Ty::Str | Ty::Parameter(_) | Ty::Nominal(_) | Ty::Error => true,
    }
}

#[cfg(test)]
mod rebase;
#[cfg(test)]
mod tests;
