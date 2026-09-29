//! Experimental installation used only by differential tests. Candidates require
//! a cold target snapshot today; this is not an incremental production path.
use super::{BodyFacts, BodyResult, Checker};
use crate::{BodyEffects, BodyReplayState, OwnerCheckedFacts, Span};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(in crate::checker) struct Candidate {
    pub facts: OwnerCheckedFacts,
    pub effects: BodyEffects,
}

#[derive(Default)]
pub(in crate::checker) struct ReuseHarness {
    pub candidates: BTreeMap<Span, Candidate>,
    pub installed: Vec<Span>,
    pub rejected: Vec<Span>,
}

impl Checker<'_> {
    pub(super) fn try_reuse_body(&mut self, anchor: Span) -> bool {
        let Some(candidate) = self.reuse.candidates.remove(&anchor) else {
            return false;
        };
        if self.install_candidate(anchor, candidate).is_some() {
            self.reuse.installed.push(anchor);
            true
        } else {
            self.reuse.rejected.push(anchor);
            false
        }
    }

    fn install_candidate(&mut self, anchor: Span, candidate: Candidate) -> Option<()> {
        assert!(!self.body.active);
        let cancel = self.cancellation?;
        cancel.check().ok()?;
        let before = self.budget_position();
        let input = BodyReplayState {
            budget: before,
            instances: self.generic_instances.clone(),
        };
        let (after, effects) = candidate
            .effects
            .rebase_for_test(&input, self.limits, cancel)
            .ok()??;
        let OwnerCheckedFacts {
            diagnostics,
            expressions,
            patterns,
            locals,
            ownership,
            fields,
            arguments,
            outputs,
        } = candidate.facts;
        if after.budget.diagnostics.checked_sub(before.diagnostics)? != diagnostics.len()
            || locals.keys().any(|id| self.editor.locals.contains_key(id))
            || outputs
                .keys()
                .any(|id| self.editor.outputs.contains_key(id))
        {
            return None;
        }
        let collection_units = after
            .budget
            .metadata
            .checked_sub(before.metadata)?
            .checked_sub(expressions.len())?
            .checked_sub(patterns.len())?;
        let collection_units = self
            .collection_metadata_units
            .checked_add(collection_units)?;
        let instances = effects.requested_instances_for_test();
        let facts = BodyFacts {
            diagnostics,
            expressions,
            patterns,
            locals,
            uses: ownership,
            fields,
            arguments,
            outputs,
        };
        cancel.check().ok()?;
        // Everything above is provisional. No strict counter or fact changes on
        // rejection; a miss can immediately execute the body normally.
        let effects = self.retain_replayed_effects(effects);
        self.work = after.budget.work;
        self.editor.units = after.budget.editor_metadata;
        self.editor.truncated = after.budget.editor_truncated;
        self.exhausted = after.budget.exhausted;
        self.collection_metadata_units = collection_units;
        self.generic_instances = after.instances;
        let result = BodyResult {
            facts,
            before,
            after: after.budget,
            instances,
            effects,
            cancelled: false,
        };
        self.record_body(anchor, &result);
        self.publish_body(result);
        Some(())
    }
}

#[cfg(test)]
mod tests;
