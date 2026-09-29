use super::*;
use crate::{CheckPolicy, SourceSet, parse_sources, resolve};
use std::collections::BTreeMap;

fn program() -> crate::ResolvedProgram {
    let mut sources = SourceSet::new();
    sources.add("main.srx","value I(int); struct Box<T> { payload: T; } fn inspect() { let x=Box<I> { payload = I(1); }; let y=Box<I> { payload = I(2); }; }").unwrap();
    resolve(parse_sources(&sources).unwrap()).unwrap()
}

fn prepare<'a>(
    program: &'a crate::ResolvedProgram,
    policy: &'a CheckPolicy,
    cancel: &'a AnalysisCancellation,
    limits: CheckLimits,
) -> Checker<'a> {
    let mut checker = Checker::new(program, policy, limits);
    checker.cancellation = Some(cancel);
    checker.index_resolution_metadata();
    checker.index();
    checker.validate_declarations();
    checker.propagate_carriers();
    checker.collect_function_types();
    checker.collect_editor_types();
    checker
}

fn run(checker: &mut Checker<'_>) -> super::super::BodyResult {
    let info = *checker.functions.values().next().unwrap();
    checker.evaluate_body(|checker| {
        checker.body.effect_anchor = Some(info.declaration.name.span);
        checker.check_block(&info.declaration.body, None, info.context);
    })
}

#[test]
fn replay_matches_cold_prefix_changes_and_both_deduplication_directions() {
    let program = program();
    let policy = CheckPolicy::default();
    let cancel = AnalysisCancellation::default();
    let limits = CheckLimits::default();
    let mut original = prepare(&program, &policy, &cancel, limits);
    let original_input = BodyReplayState {
        budget: original.budget_position(),
        instances: original.generic_instances.clone(),
    };
    let result = run(&mut original);
    let journal = result.effects.unwrap();
    assert!(journal.observation_count() > 0);
    assert_eq!(
        journal
            .replay(&original_input, limits, &cancel)
            .unwrap()
            .unwrap(),
        BodyReplayState {
            budget: result.after,
            instances: original.generic_instances.clone()
        }
    );
    let requested = original.generic_instances.clone();
    for seeded in [false, true] {
        let mut cold = prepare(&program, &policy, &cancel, limits);
        cold.work += 17;
        cold.prior_metadata += 5;
        cold.editor.units += 3;
        cold.prior_diagnostics += 2;
        if seeded {
            cold.generic_instances = requested.clone();
        }
        let input = BodyReplayState {
            budget: cold.budget_position(),
            instances: cold.generic_instances.clone(),
        };
        let expected = run(&mut cold);
        let replayed = journal.replay(&input, limits, &cancel).unwrap().unwrap();
        assert_eq!(
            replayed,
            BodyReplayState {
                budget: expected.after,
                instances: cold.generic_instances.clone()
            }
        );
        // Replay the journal recorded with the new prefix back at the old prefix.
        let other = expected.effects.unwrap();
        assert_eq!(
            other
                .replay(&original_input, limits, &cancel)
                .unwrap()
                .unwrap()
                .budget,
            result.after
        );
    }
}

#[test]
fn sequential_body_journals_compose_with_shared_specializations() {
    let mut sources = SourceSet::new();
    sources.add("main.srx","value I(int); struct Box<T> { payload: T; } fn first() { let x=Box<I> { payload=I(1); }; } fn second() { let x=Box<I> { payload=I(2); }; let y=Box<Box<I>> { payload=x; }; }").unwrap();
    let program = resolve(parse_sources(&sources).unwrap()).unwrap();
    let policy = CheckPolicy::default();
    let cancel = AnalysisCancellation::default();
    let limits = CheckLimits::default();
    let mut checker = prepare(&program, &policy, &cancel, limits);
    let mut simulated = BodyReplayState {
        budget: checker.budget_position(),
        instances: checker.generic_instances.clone(),
    };
    let functions: Vec<_> = checker.functions.values().copied().collect();
    for info in functions {
        let result = checker.evaluate_body(|checker| {
            checker.body.effect_anchor = Some(info.declaration.name.span);
            checker.check_block(&info.declaration.body, None, info.context);
        });
        let journal = result.effects.as_ref().unwrap();
        simulated = journal
            .replay(&simulated, limits, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(
            simulated,
            BodyReplayState {
                budget: result.after,
                instances: checker.generic_instances.clone()
            }
        );
        checker.publish_body(result);
        assert_eq!(simulated.budget, checker.budget_position());
    }
    assert_eq!(simulated.instances.len(), 2);
}

#[test]
fn exact_work_boundary_and_ordered_instance_limit_match_cold_runs() {
    let program = program();
    let policy = CheckPolicy::default();
    let cancel = AnalysisCancellation::default();
    let limits = CheckLimits {
        max_generic_instances: 1,
        ..CheckLimits::default()
    };
    let mut original = prepare(&program, &policy, &cancel, limits);
    let result = run(&mut original);
    let journal = result.effects.unwrap();
    let delta = result.after.work - result.before.work;
    for extra in [0, 1] {
        let mut cold = prepare(&program, &policy, &cancel, limits);
        cold.work = limits.max_work - delta + extra;
        let input = BodyReplayState {
            budget: cold.budget_position(),
            instances: cold.generic_instances.clone(),
        };
        let result = run(&mut cold);
        let replay = journal.replay(&input, limits, &cancel).unwrap();
        if extra == 0 {
            assert_eq!(replay.unwrap().budget, result.after);
        } else {
            assert!(replay.is_none() && result.after.exhausted);
        }
    }
    let requested = original.generic_instances.iter().next().unwrap().clone();
    let Ty::Specialization { template, .. } = requested else {
        panic!("specialization");
    };
    for instance in [
        requested,
        Ty::Specialization {
            template,
            arguments: vec![Ty::Str],
        },
    ] {
        let mut cold = prepare(&program, &policy, &cancel, limits);
        cold.generic_instances.insert(instance);
        let input = BodyReplayState {
            budget: cold.budget_position(),
            instances: cold.generic_instances.clone(),
        };
        let result = run(&mut cold);
        let replay = journal.replay(&input, limits, &cancel).unwrap();
        if result.after.exhausted {
            assert!(replay.is_none());
        } else {
            assert_eq!(
                replay.unwrap(),
                BodyReplayState {
                    budget: result.after,
                    instances: cold.generic_instances.clone()
                }
            );
        }
    }
}

#[test]
fn uncommitted_concretization_probes_are_not_lost_in_final_deltas() {
    use crate::checker::RawTy;
    let program = program();
    let policy = CheckPolicy::default();
    let cancel = AnalysisCancellation::default();
    let limits = CheckLimits::default();
    let mut checker = prepare(&program, &policy, &cancel, limits);
    let info = *checker.functions.values().next().unwrap();
    let span = info.declaration.name.span;
    let missing = program.locals().next().unwrap().id();
    let before = checker.budget_position();
    let result = checker.evaluate_body(|checker| {
        assert!(
            checker
                .concretize(
                    RawTy::List(Box::new(RawTy::Parameter(missing))),
                    &BTreeMap::default(),
                    span
                )
                .is_none()
        );
    });
    assert_eq!(before.work, result.after.work);
    let journal = result.effects.unwrap();
    assert!(journal.observation_count() >= 2);
    let input = BodyReplayState {
        budget: BodyBudget {
            work: limits.max_work - 1,
            ..before
        },
        instances: checker.generic_instances.clone(),
    };
    assert!(journal.replay(&input, limits, &cancel).unwrap().is_none());
    let mut cold = prepare(&program, &policy, &cancel, limits);
    cold.work = input.budget.work;
    let rejected = cold.evaluate_body(|checker| {
        checker.concretize(
            RawTy::List(Box::new(RawTy::Parameter(missing))),
            &BTreeMap::default(),
            span,
        );
    });
    assert!(rejected.after.exhausted);
    assert!(
        rejected
            .effects
            .unwrap()
            .replay(&input, limits, &cancel)
            .unwrap()
            .is_none()
    );
}

#[test]
fn diagnostics_near_the_cap_reject_changed_publication() {
    let mut sources = SourceSet::new();
    sources
        .add(
            "main.srx",
            "resource R(int); fn inspect() { let x=R(1); x; x; x; }",
        )
        .unwrap();
    let program = resolve(parse_sources(&sources).unwrap()).unwrap();
    let policy = CheckPolicy::default();
    let cancel = AnalysisCancellation::default();
    let limits = CheckLimits::default();
    let mut original = prepare(&program, &policy, &cancel, limits);
    let result = run(&mut original);
    assert_eq!(result.facts.diagnostics.len(), 2);
    let journal = result.effects.unwrap();
    for diagnostics in [
        crate::MAX_DIAGNOSTICS - 3,
        crate::MAX_DIAGNOSTICS - 2,
        crate::MAX_DIAGNOSTICS - 1,
    ] {
        let mut cold = prepare(&program, &policy, &cancel, limits);
        cold.prior_diagnostics = diagnostics;
        let input = BodyReplayState {
            budget: cold.budget_position(),
            instances: cold.generic_instances.clone(),
        };
        let result = run(&mut cold);
        let replay = journal.replay(&input, limits, &cancel).unwrap();
        if result.after.diagnostics < crate::MAX_DIAGNOSTICS {
            assert_eq!(replay.unwrap().budget, result.after);
        } else {
            assert!(replay.is_none());
        }
    }
}

#[test]
fn limits_cancel_and_journal_retention_are_transactional() {
    let program = program();
    let policy = CheckPolicy::default();
    let cancel = AnalysisCancellation::default();
    let limits = CheckLimits::default();
    let mut checker = prepare(&program, &policy, &cancel, limits);
    let input = BodyReplayState {
        budget: checker.budget_position(),
        instances: checker.generic_instances.clone(),
    };
    let expected = run(&mut checker);
    let journal = expected.effects.unwrap();
    for category in 0..4 {
        let mut saturated = input.clone();
        match category {
            0 => saturated.budget.work = limits.max_work,
            1 => saturated.budget.metadata = limits.max_metadata_units,
            2 => saturated.budget.editor_metadata = limits.max_metadata_units,
            _ => saturated.budget.diagnostics = crate::MAX_DIAGNOSTICS,
        }
        let untouched = saturated.clone();
        assert!(
            journal
                .replay(&saturated, limits, &cancel)
                .unwrap()
                .is_none()
        );
        assert_eq!(saturated, untouched);
    }
    let mut bounded = prepare(&program, &policy, &cancel, limits);
    bounded.effect_retention.remaining = 1;
    let result = run(&mut bounded);
    assert!(result.effects.is_none());
    assert_eq!(result.after, expected.after);
    assert_eq!(result.facts.expressions, expected.facts.expressions);
    assert_eq!(result.facts.locals, expected.facts.locals);
    assert_eq!(result.facts.uses, expected.facts.uses);
    assert_eq!(result.facts.diagnostics, expected.facts.diagnostics);
    cancel.cancel();
    assert!(journal.replay(&input, limits, &cancel).is_err());
}
