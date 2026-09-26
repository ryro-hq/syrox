use std::sync::Arc;

use super::*;
use crate::{
    CanonicalItemIdentity, PrimitiveType, ScopeRule, SourceDomainId, SourceSet, check,
    parse_sources, resolve,
};

fn checked(source: &str, policy: &CheckPolicy) -> CheckedProgram {
    let mut sources = SourceSet::new();
    sources.add("main.srx", source).unwrap();
    check(resolve(parse_sources(&sources).unwrap()).unwrap(), policy).unwrap()
}

fn run(source: &str, policy: &CheckPolicy) -> RealizedProgram {
    let checked = checked(source, policy);
    evaluate(&checked, policy, &EvaluationEnvironment::new(policy)).unwrap()
}

fn identity(path: &str) -> CanonicalItemIdentity {
    CanonicalItemIdentity::new(SourceDomainId::project(), path.split("::")).unwrap()
}

#[test]
fn deterministic_roots_are_independent_and_failures_are_explicit() {
    let result = run(
        "resource Port(int); outputs { bad: [Port] = [Port(80), Port(80)]; good: Port = Port(80); }",
        &CheckPolicy::default(),
    );
    let roots: Vec<_> = result.roots().collect();
    assert_eq!(roots.len(), 2);
    assert!(matches!(roots[0].outcome(), RealizedRootOutcome::Failed(_)));
    assert!(roots[0].diagnostics()[0].message.contains("conflicts"));
    assert!(matches!(roots[1].outcome(), RealizedRootOutcome::Value(_)));
    assert_eq!(roots[1].claims().count(), 1);

    let independent = run(
        "resource Port(int); outputs { first: Port = Port(80); second: Port = Port(80); }",
        &CheckPolicy::default(),
    );
    assert!(independent.is_success());

    let again = run(
        "resource Port(int); outputs { bad: [Port] = [Port(80), Port(80)]; good: Port = Port(80); }",
        &CheckPolicy::default(),
    );
    assert_eq!(format!("{result:?}"), format!("{again:?}"));
}

#[test]
fn values_refine_without_claiming_and_predicates_run_on_evaluated_values() {
    fn even(value: i64) -> bool {
        value % 2 == 0
    }

    let policy = CheckPolicy::new("predicates")
        .unwrap()
        .with_predicate("number::even", PrimitiveType::Int)
        .unwrap();
    let program = checked(
        "value Even(int) where 2..=10, in [2, 4, 6, 8, 10], number::even(self); outputs { values: [Even] = [4, Even(6), 8]; }",
        &policy,
    );
    let environment = EvaluationEnvironment::new(&policy)
        .with_int_predicate("number::even", even)
        .unwrap();
    let result = evaluate(&program, &policy, &environment).unwrap();
    let root = result.roots().next().unwrap();
    assert!(root.value().is_some());
    assert_eq!(root.claims().count(), 0);

    let failed = checked(
        "value Even(int) where number::even(self); outputs { bad: Even = 3; }",
        &policy,
    );
    let failed = evaluate(&failed, &policy, &environment).unwrap();
    assert!(matches!(
        failed.roots().next().unwrap().outcome(),
        RealizedRootOutcome::Failed(_)
    ));
}

#[test]
fn refinement_comparisons_consume_the_evaluation_work_budget() {
    let policy = CheckPolicy::default();
    let environment = EvaluationEnvironment::new(&policy);
    let limits = EvaluationLimits {
        max_total_steps: 30,
        ..EvaluationLimits::default()
    };
    let short = checked("value V(int) where in [1]; outputs { v: V = 1; }", &policy);
    assert!(
        evaluate_with_limits(&short, &policy, &environment, limits)
            .unwrap()
            .is_success()
    );

    let entries = (0..40)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let many = checked(
        &format!("value V(int) where in [{entries}]; outputs {{ v: V = 39; }}"),
        &policy,
    );
    assert!(matches!(
        evaluate_with_limits(&many, &policy, &environment, limits),
        Err(EvaluationSetupError::EvaluationWorkLimit)
    ));

    let text = "a".repeat(4096);
    let long = checked(
        &format!("value V(str) where in [\"{text}\"]; outputs {{ v: V = \"{text}\"; }}"),
        &policy,
    );
    assert!(matches!(
        evaluate_with_limits(&long, &policy, &environment, limits),
        Err(EvaluationSetupError::EvaluationWorkLimit)
    ));
}

#[test]
fn interpolation_concat_defaults_calls_match_fields_and_erasure_execute_checked_paths() {
    let policy = CheckPolicy::new("erasure")
        .unwrap()
        .with_erasure(identity("Box"), identity("Public"))
        .unwrap();
    let source = r#"
            value Name(str);
            value Text(str);
            enum Mode { Fast, Slow }
            struct Config { name: Name; mode: Mode = Fast; }
            struct Public { name: Name; }
            struct Box<T> { name: Name; hidden: T; }
            fn choose(mode: Mode) -> Name { match mode { Fast => "quick", Slow => "slow" } }
            fn greet(name: Name) -> Text { "hello ${name}" }
            fn config(name: Name) -> Config { let made = Config { name = name; }; made }
            outputs {
                words: [Name] = ["a"] ++ ["b"];
                greeting: Text = greet("world");
                selected: Name = choose(Slow);
                projected: Name = config("field").name;
                public: Public = erase<Public>(Box<Name> { name = "visible"; hidden = "secret"; });
            }
        "#;
    let result = run(source, &policy);
    assert!(
        result.is_success(),
        "{:?}",
        result.diagnostics().collect::<Vec<_>>()
    );
    assert_eq!(result.roots().len(), 5);
    assert!(matches!(
        result.roots().nth(1).unwrap().value(),
        Some(Value::Nominal { value: PrimitiveValue::Str(value), .. }) if value == "hello world"
    ));
    assert!(matches!(
        result.roots().nth(4).unwrap().value(),
        Some(Value::Struct { fields, .. }) if fields.len() == 1 && fields[0].0 == "name"
    ));
}

#[test]
fn generic_defaults_are_substituted_at_the_specialization() {
    let result = run(
        "struct Phantom<T> {} struct Holder<T> { marker: Phantom<T> = Phantom<T> {}; } struct S {} outputs { out: Holder<S> = Holder<S> {}; }",
        &CheckPolicy::default(),
    );
    let root = result.roots().next().unwrap();
    assert!(root.value().is_some(), "{:?}", root.diagnostics());
    assert!(matches!(
        root.ty(),
        Some(CanonicalType::Specialization { arguments, .. }) if arguments.len() == 1
    ));
}

#[test]
fn recursion_and_expansion_limits_do_not_poison_later_roots() {
    let source = "value I(int); fn forever() -> I { forever() } outputs { stuck: I = forever(); later: I = 7; }";
    let policy = CheckPolicy::default();
    let program = checked(source, &policy);
    let result = evaluate_with_limits(
        &program,
        &policy,
        &EvaluationEnvironment::new(&policy),
        EvaluationLimits {
            max_depth: 8,
            ..EvaluationLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        result.roots().next().unwrap().outcome(),
        RealizedRootOutcome::Failed(_)
    ));
    assert!(result.roots().nth(1).unwrap().value().is_some());

    let stepped = evaluate_with_limits(
        &program,
        &policy,
        &EvaluationEnvironment::new(&policy),
        EvaluationLimits {
            max_steps_per_root: 5,
            ..EvaluationLimits::default()
        },
    )
    .unwrap();
    assert!(
        stepped
            .roots()
            .next()
            .unwrap()
            .diagnostics()
            .iter()
            .any(|error| error.message.contains("step limit"))
    );

    let checked = checked(
        "value Name(str); fn copy(x: Name) -> [Name] { [x, x] } outputs { large: [Name] = copy(\"0123456789\"); }",
        &policy,
    );
    let expanded = evaluate_with_limits(
        &checked,
        &policy,
        &EvaluationEnvironment::new(&policy),
        EvaluationLimits {
            max_expansion_bytes: 32,
            ..EvaluationLimits::default()
        },
    )
    .unwrap();
    assert!(
        expanded
            .diagnostics()
            .any(|error| error.message.contains("expansion"))
    );
}

#[test]
fn policy_and_environment_mismatches_fail_before_evaluation() {
    fn accepts(_: i64) -> bool {
        true
    }
    let first = CheckPolicy::new("first")
        .unwrap()
        .with_predicate("p", PrimitiveType::Int)
        .unwrap();
    let second = CheckPolicy::new("second").unwrap();
    let checked = checked("value I(int); outputs { out: I = 1; }", &first);
    assert!(matches!(
        evaluate(&checked, &second, &EvaluationEnvironment::new(&second)),
        Err(EvaluationSetupError::PolicyMismatch { .. })
    ));
    assert!(matches!(
        evaluate(&checked, &first, &EvaluationEnvironment::new(&first)),
        Err(EvaluationSetupError::MissingPredicateCallback)
    ));
    let wrong = EvaluationEnvironment::new(&second);
    assert!(matches!(
        evaluate(&checked, &first, &wrong),
        Err(EvaluationSetupError::EnvironmentPolicyMismatch)
    ));
    let complete = EvaluationEnvironment::new(&first)
        .with_int_predicate("p", accepts)
        .unwrap();
    assert!(evaluate(&checked, &first, &complete).is_ok());
}

#[test]
fn private_scopes_isolate_nested_claims_while_inherited_scopes_share() {
    let source = r"
            resource Port(int) in net;
            struct Box in [net] { port: Port; }
            struct Pair { left: Box; right: Box; }
            outputs { pair: Pair = Pair { left = Box { port = Port(9); }; right = Box { port = Port(9); }; }; }
        ";
    let private = CheckPolicy::new("private")
        .unwrap()
        .with_scope_rule("net", ScopeRule::Private)
        .unwrap();
    assert!(run(source, &private).is_success());

    let inherited = CheckPolicy::new("inherited")
        .unwrap()
        .with_scope_rule("net", ScopeRule::Inherited)
        .unwrap();
    let result = run(source, &inherited);
    assert!(matches!(
        result.roots().next().unwrap().outcome(),
        RealizedRootOutcome::Failed(_)
    ));
}

#[test]
fn diagnostics_are_globally_capped_without_omitting_failed_roots() {
    use std::fmt::Write as _;

    let mut source = String::from("resource R(int); outputs {");
    for index in 0..crate::MAX_DIAGNOSTICS + 4 {
        write!(source, "r{index}: [R] = [R({index}), R({index})];").unwrap();
    }
    source.push('}');
    let result = run(&source, &CheckPolicy::default());
    assert_eq!(result.roots().len(), crate::MAX_DIAGNOSTICS + 4);
    assert_eq!(result.diagnostics().count(), crate::MAX_DIAGNOSTICS);
    assert!(
        result
            .roots()
            .all(|root| matches!(root.outcome(), RealizedRootOutcome::Failed(_)))
    );
}

#[test]
fn setup_work_and_identity_allocations_are_bounded() {
    let policy = CheckPolicy::default();
    let module = format!("m{}", "x".repeat(4_096));
    let source = format!("mod {module} {{ value V(int); }} outputs {{ out: {module}::V = 1; }}");
    let program = checked(&source, &policy);
    let environment = EvaluationEnvironment::new(&policy);
    let realized = evaluate_with_limits(
        &program,
        &policy,
        &environment,
        EvaluationLimits {
            max_expansion_bytes: 512,
            ..EvaluationLimits::default()
        },
    )
    .unwrap();
    assert!(realized.is_success());
    assert_eq!(
        evaluate_with_limits(
            &program,
            &policy,
            &environment,
            EvaluationLimits {
                max_setup_steps: 0,
                ..EvaluationLimits::default()
            },
        )
        .unwrap_err(),
        EvaluationSetupError::SetupWorkLimit
    );
    assert_eq!(
        evaluate_with_limits(
            &program,
            &policy,
            &environment,
            EvaluationLimits {
                max_setup_bytes: 1,
                ..EvaluationLimits::default()
            },
        )
        .unwrap_err(),
        EvaluationSetupError::SetupExpansionLimit
    );
}

#[test]
fn evaluation_limits_cannot_exceed_hard_maxima() {
    let policy = CheckPolicy::new("different-policy").unwrap();
    let program = checked(
        "value I(int); outputs { out: I = 1; }",
        &CheckPolicy::default(),
    );
    let environment = EvaluationEnvironment::new(&policy);
    let maxima = [
        (EvaluationLimit::SetupSteps, MAX_EVALUATION_SETUP_STEPS),
        (EvaluationLimit::SetupBytes, MAX_EVALUATION_SETUP_BYTES),
        (EvaluationLimit::TotalSteps, MAX_EVALUATION_TOTAL_STEPS),
        (EvaluationLimit::StepsPerRoot, MAX_EVALUATION_STEPS),
        (EvaluationLimit::Depth, MAX_EVALUATION_DEPTH),
        (EvaluationLimit::ExpansionBytes, MAX_EVALUATION_EXPANSION),
        (
            EvaluationLimit::RetainedExpansionBytes,
            MAX_EVALUATION_RETAINED_EXPANSION,
        ),
        (EvaluationLimit::Diagnostics, crate::MAX_DIAGNOSTICS),
    ];

    for (limit, maximum) in maxima {
        let configured = maximum + 1;
        let mut limits = EvaluationLimits::default();
        match limit {
            EvaluationLimit::SetupSteps => limits.max_setup_steps = configured,
            EvaluationLimit::SetupBytes => limits.max_setup_bytes = configured,
            EvaluationLimit::TotalSteps => limits.max_total_steps = configured,
            EvaluationLimit::StepsPerRoot => limits.max_steps_per_root = configured,
            EvaluationLimit::Depth => limits.max_depth = configured,
            EvaluationLimit::ExpansionBytes => limits.max_expansion_bytes = configured,
            EvaluationLimit::RetainedExpansionBytes => {
                limits.max_retained_expansion_bytes = configured;
            }
            EvaluationLimit::Diagnostics => limits.max_diagnostics = configured,
        }
        assert_eq!(
            evaluate_with_limits(&program, &policy, &environment, limits).unwrap_err(),
            EvaluationSetupError::LimitExceedsMaximum {
                limit,
                configured,
                maximum,
            }
        );
    }
}

#[test]
fn operation_wide_budgets_fail_the_invocation_without_a_root_diagnostic() {
    let policy = CheckPolicy::default();
    let program = checked(
        "value I(int); outputs { first: I = 1; second: I = 2; }",
        &policy,
    );
    let environment = EvaluationEnvironment::new(&policy);
    assert_eq!(
        evaluate_with_limits(
            &program,
            &policy,
            &environment,
            EvaluationLimits {
                max_total_steps: 1,
                ..EvaluationLimits::default()
            },
        )
        .unwrap_err(),
        EvaluationSetupError::EvaluationWorkLimit
    );

    let program = checked("value I(int); outputs { only: I = I(1); }", &policy);
    assert_eq!(
        evaluate_with_limits(
            &program,
            &policy,
            &environment,
            EvaluationLimits {
                max_total_steps: 1,
                max_steps_per_root: 1,
                ..EvaluationLimits::default()
            },
        )
        .unwrap_err(),
        EvaluationSetupError::EvaluationWorkLimit
    );

    let text = "x".repeat(200);
    let program = checked(
        &format!("value S(str); outputs {{ first: S = \"{text}\"; second: S = \"{text}\"; }}"),
        &policy,
    );
    assert_eq!(
        evaluate_with_limits(
            &program,
            &policy,
            &environment,
            EvaluationLimits {
                max_retained_expansion_bytes: 1_024,
                ..EvaluationLimits::default()
            },
        )
        .unwrap_err(),
        EvaluationSetupError::EvaluationRetainedExpansionLimit
    );
}

#[test]
fn canonical_types_are_shared_and_root_claim_keys_are_distinct() {
    let result = run(
        "resource R(int); outputs { first: R = R(1); second: R = R(1); }",
        &CheckPolicy::default(),
    );
    let roots: Vec<_> = result.roots().collect();
    let first = roots[0].claims().next().unwrap();
    let second = roots[1].claims().next().unwrap();
    assert_ne!(first.key, second.key);
    assert_ne!(first.key.scope.root, second.key.scope.root);
    let value_ty = roots[0].value().unwrap().canonical_type();
    assert!(Arc::ptr_eq(&first.key.ty, &value_ty));
}

#[test]
fn predicate_panics_become_failed_root_diagnostics() {
    fn panics(_: i64) -> bool {
        panic!("host predicate panic");
    }

    let policy = CheckPolicy::new("predicate-panic")
        .unwrap()
        .with_predicate("host::accepts", PrimitiveType::Int)
        .unwrap();
    let program = checked(
        "value V(int) where host::accepts(self); outputs { out: V = V(1); }",
        &policy,
    );
    let environment = EvaluationEnvironment::new(&policy)
        .with_int_predicate("host::accepts", panics)
        .unwrap();
    let result = evaluate(&program, &policy, &environment).unwrap();
    let root = result.roots().next().unwrap();
    assert!(matches!(root.outcome(), RealizedRootOutcome::Failed(_)));
    assert!(
        root.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message == "predicate callback panicked")
    );
}

#[test]
fn maximum_supported_value_depth_drops_safely() {
    let ty = Arc::new(CanonicalType::Unit);
    let mut value = Value::Unit;
    for _ in 0..MAX_EVALUATION_DEPTH {
        value = Value::Struct {
            ty: ty.clone(),
            fields: vec![(String::from("next"), value)],
        };
    }
    drop(value);
}
