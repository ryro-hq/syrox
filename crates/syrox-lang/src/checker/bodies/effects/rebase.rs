//! Test-only rebasing/retention used by the experimental installation harness.
use super::*;

impl BodyEffects {
    pub(in crate::checker) fn rebase_for_test(
        &self,
        input: &BodyReplayState,
        limits: CheckLimits,
        cancel: &AnalysisCancellation,
    ) -> Result<Option<(BodyReplayState, Self)>, AnalysisCancelled> {
        let Some(result) = self.replay(input, limits, cancel)? else {
            return Ok(None);
        };
        let mut position = input.budget;
        let mut instances = input.instances.clone();
        let mut observations = Vec::new();
        let mut previous = self.entry;
        for event in &self.observations {
            cancel.check()?;
            if advance(&mut position, previous, event.position, limits).is_none() {
                return Ok(None);
            }
            let operation = match &event.operation {
                EffectOperation::Instance { ty, .. } => EffectOperation::Instance {
                    ty: ty.clone(),
                    already_present: instances.contains(ty),
                },
                other => other.clone(),
            };
            observations.push(EffectObservation {
                position,
                span: event.span,
                operation,
            });
            if let EffectOperation::Instance { ty, .. } = &event.operation {
                instances.insert(ty.clone());
                position.generic_instances = instances.len();
            }
            previous = event.position;
        }
        cancel.check()?;
        let journal = Self {
            entry: input.budget,
            exit: result.budget,
            limits,
            observations,
        };
        Ok(Some((result, journal)))
    }

    pub(in crate::checker) fn requested_instances_for_test(&self) -> BTreeSet<Ty> {
        self.observations
            .iter()
            .filter_map(|event| match &event.operation {
                EffectOperation::Instance { ty, .. } => Some(ty.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Checker<'_> {
    pub(in crate::checker) fn set_effect_retention_for_test(&mut self, units: usize) {
        self.effect_retention.remaining = units;
    }
    pub(in crate::checker) fn retain_replayed_effects(
        &mut self,
        mut journal: BodyEffects,
    ) -> Option<Arc<BodyEffects>> {
        let cancel = self.cancellation?;
        if self.effect_retention.truncated {
            return None;
        }
        for observation in &mut journal.observations {
            if let EffectOperation::ReferenceScan(references) = &mut observation.operation {
                if self.reference_inventory.is_none() {
                    if references.len() > self.effect_retention.remaining {
                        self.discard_effects();
                        return None;
                    }
                    self.effect_retention.remaining -= references.len();
                    self.reference_inventory = Some(references.clone());
                }
                *references = self.reference_inventory.as_ref()?.clone();
            }
            if let EffectOperation::Instance { ty, .. } = &observation.operation
                && !type_fits(ty, &mut self.effect_retention.remaining, cancel)
            {
                self.discard_effects();
                return None;
            }
            if self.effect_retention.remaining == 0 {
                self.discard_effects();
                return None;
            }
            self.effect_retention.remaining -= 1;
        }
        Some(Arc::new(journal))
    }
}
