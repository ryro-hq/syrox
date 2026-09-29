use super::*;
use crate::{
    AnalysisCancellation, AnalysisHost, CheckLimits, CheckPolicy, ResolutionOwnerPart,
    SemanticAnalysis, SourceSet,
};
use std::collections::BTreeSet;

fn analyze(files: &[(&str, &str)], policy: &CheckPolicy, limits: CheckLimits) -> SemanticAnalysis {
    let mut sources = SourceSet::new();
    for (name, text) in files {
        sources.add(*name, *text).unwrap();
    }
    AnalysisHost::default()
        .snapshot()
        .analyze_project_with_limits(
            &sources,
            &BTreeMap::new(),
            policy,
            &AnalysisCancellation::default(),
            limits,
        )
        .unwrap()
}

fn candidates(previous: &SemanticAnalysis, target: &SemanticAnalysis) -> BTreeMap<Span, Candidate> {
    let cancel = AnalysisCancellation::default();
    let mut candidates = BTreeMap::new();
    for (key, span) in target.resolution_owners() {
        let Some(facts) = target
            .remap_checked_body_from(previous, key, &cancel)
            .unwrap()
        else {
            continue;
        };
        let Some(effects) = target
            .remap_body_effects_from(previous, key, &cancel)
            .unwrap()
        else {
            continue;
        };
        let anchor = match key.part {
            ResolutionOwnerPart::FieldDefault(_) => span,
            ResolutionOwnerPart::Declaration => target
                .items()
                .find(|item| item.domain() == key.domain && item.path().segments() == key.path)
                .unwrap()
                .span(),
        };
        assert!(
            candidates
                .insert(anchor, Candidate { facts, effects })
                .is_none()
        );
    }
    candidates
}

fn incomplete(analysis: &SemanticAnalysis) -> BTreeSet<(u32, u32, u32)> {
    let mut incomplete = BTreeSet::new();
    for (id, source) in analysis.sources().iter() {
        let file = crate::parser::parse_file_cancellable(id, source, None).unwrap();
        incomplete.extend(
            file.incomplete_bodies
                .iter()
                .map(|span| crate::checker::span_key(*span)),
        );
    }
    incomplete
}

fn assert_same(actual: &Checker<'_>, expected: &Checker<'_>) {
    for (left, right) in actual
        .publications
        .entries
        .iter()
        .zip(&expected.publications.entries)
    {
        if let (Some(left_effects), Some(right_effects)) = (&left.effects, &right.effects) {
            assert_eq!(
                left_effects.exit().work - left_effects.entry().work,
                right_effects.exit().work - right_effects.entry().work,
                "body {:?}: {:?}",
                left.anchor,
                actual
                    .program
                    .items()
                    .find(|item| item.span() == left.anchor)
                    .map(crate::ResolvedItem::path)
            );
        }
    }
    assert_eq!(actual.diagnostics, expected.diagnostics);
    assert_eq!(actual.expressions, expected.expressions);
    assert_eq!(actual.patterns, expected.patterns);
    assert_eq!(actual.editor, expected.editor);
    assert_eq!(actual.function_types, expected.function_types);
    assert_eq!(actual.carriers, expected.carriers);
    assert_eq!(actual.generic_instances, expected.generic_instances);
    assert_eq!(
        actual.collection_metadata_units,
        expected.collection_metadata_units
    );
    assert_eq!(actual.budget_position(), expected.budget_position());
    assert_eq!(actual.publications, expected.publications);
    assert_eq!(actual.effect_retention, expected.effect_retention);
    assert_eq!(actual.reference_inventory, expected.reference_inventory);
    assert_eq!(actual.body.type_parameters, expected.body.type_parameters);
    assert_eq!(actual.body.fact_errors, expected.body.fact_errors);
    assert_eq!(actual.body.fact_unknowns, expected.body.fact_unknowns);
    assert!(actual.body.bindings.is_empty() && !actual.body.active);
}

fn compare(
    previous: &SemanticAnalysis,
    target: &SemanticAnalysis,
    policy: &CheckPolicy,
    limits: CheckLimits,
    setup: impl Fn(&mut Checker<'_>),
) -> (usize, usize) {
    let cancel = AnalysisCancellation::default();
    let mut cold = Checker::new(target.test_resolved(), policy, limits);
    cold.cancellation = Some(&cancel);
    cold.incomplete_bodies = incomplete(target);
    let mut reused = Checker::new(target.test_resolved(), policy, limits);
    reused.cancellation = Some(&cancel);
    reused.incomplete_bodies = incomplete(target);
    reused.reuse.candidates = candidates(previous, target);
    setup(&mut cold);
    setup(&mut reused);
    cold.run();
    reused.run();
    assert_same(&reused, &cold);
    (reused.reuse.installed.len(), reused.reuse.rejected.len())
}

#[test]
fn full_checker_matches_cold_after_edits_with_defaults_outputs_and_recovery() {
    let text = r"
        value I(int); resource R(int); value Key(str);
        struct Box<T> { payload: T; }
        struct Defaults<T> { f: fn(T) -> T = fn(x: T) -> T { x }; }
        enum Choice<T> { Left(T), Right(T) }
        fn identity<T>(x: T) -> T { x }
        fn inspect(x: Choice<R>) -> R { match x { Left(y) => y, Right(y) => y } }
        fn capture(x: R) -> R { let f=once fn() -> R { x }; f() }
        fn bad(x: R) { x; x; }
        fn field(x: Box<I>) -> I { identity(x.payload) }
        fn broken() { let missing: I = ; let later=I(2); }
        mod recipes { pub fn recipe() -> I { I(1) } }
        outputs {
            result: I = field(Box<I> { payload=I(1); });
            keys: [Key] = module_exports(recipes, recipe, fn(key: Key, make: fn() -> I) -> Key { key });
        }
    ";
    let policy = CheckPolicy::default();
    let limits = CheckLimits::default();
    let old = analyze(&[("main.srx", text)], &policy, limits);
    let changed =
        format!("// édition é 🦀\nvalue Earlier(int); fn earlier<T>(x: T) -> T {{ x }} {text}");
    let new = analyze(&[("main.srx", &changed)], &policy, limits);
    let (hits, misses) = compare(&old, &new, &policy, limits, |_| {});
    assert_eq!(hits, 7);
    assert_eq!(misses, 0);
    // A primitive declaration shifts IDs/offsets without changing the reference inventory.
    let changed = format!("// édition é 🦀\nvalue Earlier(int); {text}");
    let new = analyze(&[("main.srx", &changed)], &policy, limits);
    assert_eq!(compare(&old, &new, &policy, limits, |_| {}), (10, 0));
}

#[test]
fn source_reordering_and_changed_dependencies_mix_reuse_with_cold_bodies() {
    let main = "fn inspect(x: lib::Box) -> lib::I { lib::identity(x.payload) } fn unrelated() {}";
    let lib = "mod lib { pub value I(int); pub struct Box { payload: I; } pub fn identity<T>(x: T) -> T { x } }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits::default();
    let old = analyze(&[("main.srx", main), ("lib.srx", lib)], &policy, limits);
    assert!(old.diagnostics().is_empty(), "{:?}", old.diagnostics());
    let shifted = format!("// changed offsets\n{lib}");
    let new = analyze(
        &[("lib.srx", &shifted), ("main.srx", main)],
        &policy,
        limits,
    );
    assert_eq!(compare(&old, &new, &policy, limits, |_| {}), (3, 0));
    let changed = lib.replace("value I", "resource I");
    let new = analyze(
        &[("lib.srx", &changed), ("main.srx", main)],
        &policy,
        limits,
    );
    let (hits, misses) = compare(&old, &new, &policy, limits, |_| {});
    assert!(hits > 0 && hits < 3, "hits={hits}, misses={misses}");
    assert_eq!(misses, 0);
}

#[test]
fn replay_failure_falls_back_without_double_charging_any_global_budget() {
    let text = "value I(int); value S(str); struct Box<T> { payload: T; } fn inspect() { let x=Box<I> { payload=I(1); }; }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits {
        max_generic_instances: 1,
        ..CheckLimits::default()
    };
    let old = analyze(&[("main.srx", text)], &policy, limits);
    let changed = text.replace(
        "fn inspect",
        "fn earlier() { let x=Box<S> { payload=S(\"x\"); }; } fn inspect",
    );
    // Preview supplies target coordinates/guards, not the prefix under test.
    let new = analyze(&[("main.srx", &changed)], &policy, CheckLimits::default());
    assert_eq!(compare(&old, &new, &policy, limits, |_| {}), (0, 1));

    let text = "value I(int); fn inspect() -> I { I(1) }";
    for limits in [
        CheckLimits {
            max_work: 100,
            ..CheckLimits::default()
        },
        CheckLimits {
            max_metadata_units: 8,
            ..CheckLimits::default()
        },
    ] {
        let old = analyze(&[("main.srx", text)], &policy, limits);
        let changed = format!(
            "value I(int); fn earlier() {{ {} }} fn inspect() -> I {{ I(1) }}",
            "I(1); ".repeat(30)
        );
        let new = analyze(&[("main.srx", &changed)], &policy, CheckLimits::default());
        assert_eq!(
            compare(&old, &new, &policy, limits, |_| {}),
            (0, 1),
            "{limits:?}"
        );
    }
}

#[test]
fn late_replay_rejection_discards_provisional_specialization_insertions() {
    let text = "value I(int); value S(str); value B(int); struct Box<T> { payload: T; } fn inspect() { let a=Box<I> { payload=I(1); }; let b=Box<S> { payload=S(\"s\"); }; }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits {
        max_generic_instances: 2,
        ..CheckLimits::default()
    };
    let old = analyze(&[("main.srx", text)], &policy, limits);
    let changed = text.replace(
        "fn inspect",
        "fn earlier() { let b=Box<B> { payload=B(1); }; } fn inspect",
    );
    let new = analyze(&[("main.srx", &changed)], &policy, CheckLimits::default());
    assert_eq!(compare(&old, &new, &policy, limits, |_| {}), (0, 1));
}

#[test]
fn diagnostic_cap_and_publication_retention_have_identical_fallback() {
    let text = "resource R(int); fn inspect() { let x=R(1); x; x; x; }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits::default();
    let old = analyze(&[("main.srx", text)], &policy, limits);
    assert_eq!(
        compare(&old, &old, &policy, limits, |checker| checker
            .prior_diagnostics =
            crate::MAX_DIAGNOSTICS - 1),
        (0, 1)
    );
    assert_eq!(
        compare(&old, &old, &policy, limits, |checker| checker
            .publications
            .units = 262_144),
        (1, 0)
    );
    for units in [0, 1, 5, 30] {
        assert_eq!(
            compare(&old, &old, &policy, limits, |checker| checker
                .set_effect_retention_for_test(units)),
            (1, 0)
        );
    }
}

#[test]
fn closure_inventory_guards_and_retention_match_cold_publication() {
    let text = "value I(int); fn inspect(x: I) -> I { let f=fn(y: I) -> I { y }; f(x) }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits::default();
    let old = analyze(&[("main.srx", text)], &policy, limits);
    for units in [0, 1, 5, 30, 100] {
        assert_eq!(
            compare(&old, &old, &policy, limits, |checker| checker
                .set_effect_retention_for_test(units)),
            (1, 0)
        );
    }
    let mut preceding = old;
    for prefix in [
        "// é\n",
        "value Earlier(int); ",
        "fn added(x: I) -> I { x } ",
    ] {
        let changed = format!("{prefix}{text}");
        let current = analyze(&[("main.srx", &changed)], &policy, limits);
        let (hits, misses) = compare(&preceding, &current, &policy, limits, |_| {});
        assert_eq!(hits, usize::from(!prefix.starts_with("fn")));
        assert_eq!(misses, 0);
        preceding = current;
    }
}

#[test]
fn closure_global_inventory_rejects_reference_reordering_at_equal_count() {
    let main = "fn inspect(x: lib::I) -> lib::I { let f=fn(y: lib::I) -> lib::I { y }; f(x) }";
    let lib = "mod lib { pub value I(int); pub fn identity(x: I) -> I { x } }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits::default();
    let old = analyze(&[("main.srx", main), ("lib.srx", lib)], &policy, limits);
    let new = analyze(&[("lib.srx", lib), ("main.srx", main)], &policy, limits);
    assert!(old.diagnostics().is_empty() && new.diagnostics().is_empty());
    assert_eq!(old.references().len(), new.references().len());
    assert_eq!(compare(&old, &new, &policy, limits, |_| {}), (1, 0));
}

#[test]
fn cancellation_and_inconsistent_candidates_do_not_mutate_semantic_state() {
    let text = "value I(int); fn inspect() -> I { I(1) }";
    let policy = CheckPolicy::default();
    let limits = CheckLimits::default();
    let analysis = analyze(&[("main.srx", text)], &policy, limits);
    let (anchor, mut candidate) = candidates(&analysis, &analysis).pop_first().unwrap();
    let cancel = AnalysisCancellation::default();
    let mut checker = Checker::new(analysis.test_resolved(), &policy, limits);
    checker.cancellation = Some(&cancel);
    let original = checker.budget_position();
    candidate
        .facts
        .diagnostics
        .push(crate::Diagnostic::error("extra", anchor));
    assert!(checker.install_candidate(anchor, candidate).is_none());
    assert_eq!(checker.budget_position(), original);
    assert!(checker.expressions.is_empty() && checker.diagnostics.is_empty());
    let (_, candidate) = candidates(&analysis, &analysis).pop_first().unwrap();
    cancel.cancel();
    assert!(checker.install_candidate(anchor, candidate).is_none());
    assert_eq!(checker.budget_position(), original);
    assert!(checker.expressions.is_empty() && checker.diagnostics.is_empty());
}
