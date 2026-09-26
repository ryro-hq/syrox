use super::*;
use crate::{SourceSet, parse_sources, resolve};

fn resolved(text: &str) -> ResolvedProgram {
    let mut sources = SourceSet::new();
    sources.add("main.srx", text).unwrap();
    resolve(parse_sources(&sources).unwrap()).unwrap()
}

fn checked(text: &str) -> CheckedProgram {
    check(resolved(text), &CheckPolicy::default()).unwrap()
}

fn messages(text: &str) -> Vec<String> {
    check(resolved(text), &CheckPolicy::default())
        .unwrap_err()
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn identity(path: &str) -> CanonicalItemIdentity {
    CanonicalItemIdentity::new(SourceDomainId::project(), path.split("::")).unwrap()
}

#[test]
fn aliases_expand_structurally_and_cycles_are_rejected() {
    checked("struct S {} type A = S; type B = A; fn f(x: B) -> S { x }");
    assert!(
        messages("type A = B; type B = A;")
            .iter()
            .any(|message| message.contains("cyclic type alias"))
    );
}

#[test]
fn alias_cycles_through_lists_terminate_and_share_the_depth_budget() {
    for text in [
        "type A = [A];",
        "type A = B; type B = [A];",
        "type A = [B]; type B = A;",
    ] {
        assert!(
            messages(text)
                .iter()
                .any(|message| message.contains("cyclic type alias")),
            "cycle was not diagnosed: {text}"
        );
    }

    checked(
        "value V(int); type A = V; type B = A; type C = A; struct Pair { left: [B]; right: [C]; }",
    );

    let errors = check_with_limits(
        resolved("value V(int); type A = V; type B = [A]; type C = B;"),
        &CheckPolicy::default(),
        CheckLimits {
            max_alias_depth: 1,
            ..CheckLimits::default()
        },
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("alias expansion depth"))
    );
}

#[test]
fn generic_arity_and_scalar_arguments_are_checked() {
    checked(
        "struct S {} struct Box<T> { item: T; } struct Outer<T> { inner: Box<T>; } outputs { out: Outer<S> = Outer<S> { inner = Box<S> { item = S {}; }; }; }",
    );
    assert!(
        messages("struct Box<T> {} type Bad = Box;")
            .iter()
            .any(|message| message.contains("expects 1 type argument"))
    );
    assert!(
        messages("struct Box<T> {} struct S {} type Bad = Box<[S]>;")
            .iter()
            .any(|message| message.contains("concrete scalar"))
    );
}

#[test]
fn contextual_variants_literals_and_empty_lists_are_elaborated() {
    let program = checked(
        "value Name(str); enum Mode { Fast, Slow } struct C { name: Name; modes: [Mode]; } outputs { c: C = C { name = \"x\"; modes = [Fast, Slow]; }; }",
    );
    assert!(
        program.expressions().any(|expression| matches!(
            expression.elaboration(),
            Some(Elaboration::ValueLiteral(_))
        ))
    );
    assert_eq!(
        program
            .expressions()
            .filter(|expression| matches!(
                expression.elaboration(),
                Some(Elaboration::ContextualVariant { .. })
            ))
            .count(),
        2
    );
    checked("enum E { A } outputs { values: [E] = []; }");
    assert!(
        messages("fn f() { []; }")
            .iter()
            .any(|message| message.contains("empty list requires"))
    );
}

#[test]
fn calls_structs_returns_and_outputs_must_match_exactly() {
    let errors = messages(
        "value V(str); struct S { v: V; } fn make(v: V) -> S { S { v = 1; extra = v; } } outputs { bad: V = make(\"x\"); }",
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("struct field type mismatch"))
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("unknown field"))
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("output value type mismatch"))
    );

    assert!(
        messages("value V(str); fn f(x: V) -> V { 1 }")
            .iter()
            .any(|message| message.contains("function return type mismatch"))
    );
}

#[test]
fn match_requires_unique_reachable_exhaustive_homogeneous_arms() {
    let errors =
        messages("value I(int); enum E { A, B } fn f(e: E) -> I { match e { A => 1, A => 2 } }");
    assert!(
        errors
            .iter()
            .any(|message| message.contains("more than once"))
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("not exhaustive"))
    );
    assert!(
        messages("value I(int); enum E { A } fn f(e: E) -> I { match e { _ => 1, A => \"x\" } }")
            .iter()
            .any(|message| message.contains("unreachable"))
    );
}

#[test]
fn affine_values_cannot_be_reused_and_branch_moves_join() {
    checked("resource R(int); struct Phantom<T> {} fn f(x: Phantom<R>) { x; x; }");
    assert!(
        messages("resource R(int); struct Box<T> { item: T; } fn f(x: Box<R>) { x; x; }")
            .iter()
            .any(|message| message.contains("moved affine"))
    );
    assert!(
        messages("resource R(int); fn f(x: R) { x; x; }")
            .iter()
            .any(|message| message.contains("moved affine"))
    );
    let errors = messages(
        "resource R(int); enum E { A, B } fn take(x: R) {} fn nothing() {} fn f(e: E, x: R) { match e { A => take(x), B => nothing() }; take(x); }",
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("moved affine"))
    );
}

#[test]
fn nested_generic_resource_remains_affine() {
    let errors =
        messages("resource R(int); struct Box<T> { item: T; } fn f(x: Box<Box<R>>) { x; x; }");
    assert!(
        errors
            .iter()
            .any(|message| message.contains("moved affine")),
        "{errors:?}"
    );
}

#[test]
fn shared_non_affine_type_graph_does_not_exhaust_checker_work() {
    use std::fmt::Write as _;
    let mut source = String::from("struct N0 {} ");
    for n in 1..23 {
        write!(source, "struct N{n} {{ a: N{}; b: N{}; }} ", n - 1, n - 1).unwrap();
    }
    source.push_str("fn f(x: N22) { x; x; }");
    let program = resolved(&source);
    check_with_limits(
        program,
        &CheckPolicy::default(),
        CheckLimits {
            max_work: 100_000,
            ..CheckLimits::default()
        },
    )
    .unwrap();
}

#[test]
fn opaque_construction_rejects_a_different_authenticated_domain() {
    let mut sources = SourceSet::new();
    sources
        .add_standard_library(
            "std.srx",
            "mod std { opaque struct Secret {} outputs { type Secret = Secret; } }",
        )
        .unwrap();
    sources
        .add("main.srx", "outputs { bad: std::Secret = std::Secret {}; }")
        .unwrap();
    let errors = check(
        resolve(parse_sources(&sources).unwrap()).unwrap(),
        &CheckPolicy::default(),
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|diagnostic| diagnostic.message.contains("opaque struct constructor"))
    );
}

#[test]
fn refinements_are_validated_against_closed_policy_data() {
    let policy = CheckPolicy::new("test")
        .unwrap()
        .with_scope("net")
        .unwrap()
        .with_predicate("path::clean", PrimitiveType::Str)
        .unwrap();
    let errors = check(
            resolved(
                "resource Mixed(int) where in [1, \"x\"]; resource Bad(str) where 2..2; value V(str) in net; value P(int) where path::clean(self);",
            ),
            &policy,
        )
        .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("homogeneous"))
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("only refine `int`"))
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("ownership scope"))
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("wrong primitive"))
    );
}

#[test]
fn erasure_is_explicitly_policy_controlled() {
    let source = "struct Public { n: N; } value N(int); struct Box<T> { n: N; hidden: T; } outputs { p: Public = erase<Public>(Box<N> { n = 1; hidden = 2; }); }";
    assert!(
        messages(source)
            .iter()
            .any(|message| message.contains("not allowed"))
    );
    check(
        resolved(source),
        &CheckPolicy::new("erase-v1")
            .unwrap()
            .with_erasure(identity("Box"), identity("Public"))
            .unwrap(),
    )
    .unwrap();
}

#[test]
fn checked_program_carries_policy_identity_and_detects_mismatch() {
    let first = CheckPolicy::new("first").unwrap();
    let program = check(resolved("struct S {}"), &first).unwrap();
    assert_eq!(program.policy_identity(), "first");
    assert!(program.require_policy(&first).is_ok());
    let mismatch = program
        .require_policy(&CheckPolicy::new("second").unwrap())
        .unwrap_err();
    assert_ne!(mismatch.checked, mismatch.supplied);
}

#[test]
fn checker_work_limit_fails_closed() {
    let errors = check_with_limits(
        resolved("struct S {}"),
        &CheckPolicy::default(),
        CheckLimits {
            max_work: 0,
            ..CheckLimits::default()
        },
    )
    .unwrap_err();
    assert_eq!(errors[0].message, "type checking work limit reached");
}

#[test]
fn generic_substitution_charges_the_expanded_tree_before_cloning() {
    let program = resolved("struct S {} fn f(x: S) -> S { x }");
    let span = program.locals().next().unwrap().span();
    let parameter = program.locals().next().unwrap().id();
    let template = program.items().next().unwrap().id();
    let mut expansion = Ty::Nominal(template);
    for _ in 0..16 {
        expansion = Ty::Specialization {
            template,
            arguments: vec![expansion.clone(), expansion],
        };
    }
    let policy = CheckPolicy::default();
    let mut checker = Checker::new(
        &program,
        &policy,
        CheckLimits {
            max_work: 512,
            ..CheckLimits::default()
        },
    );
    assert!(
        checker
            .concretize(
                RawTy::Parameter(parameter),
                &BTreeMap::from([(parameter, expansion)]),
                span
            )
            .is_none()
    );
    assert_eq!(
        checker.diagnostics[0].message,
        "type checking work limit reached"
    );
}

#[test]
fn policy_is_canonical_bounded_and_rejects_conflicts() {
    let once = CheckPolicy::new("policy")
        .unwrap()
        .with_scope("net")
        .unwrap()
        .with_predicate("p", PrimitiveType::Int)
        .unwrap();
    let duplicate = once
        .clone()
        .with_scope("net")
        .unwrap()
        .with_predicate("p", PrimitiveType::Int)
        .unwrap();
    assert_eq!(once, duplicate);
    assert_eq!(once.fingerprint(), duplicate.fingerprint());
    assert_eq!(once.predicates().count(), 1);
    assert!(matches!(
        once.with_predicate("p", PrimitiveType::Str),
        Err(PolicyError::ConflictingPredicate { .. })
    ));
    assert!(matches!(
        CheckPolicy::new("x".repeat(MAX_POLICY_BYTES + 1)),
        Err(PolicyError::TooManyBytes)
    ));

    let mut policy = CheckPolicy::new("entries").unwrap();
    for index in 0..MAX_POLICY_ENTRIES {
        policy = policy.with_scope(format!("s{index}")).unwrap();
    }
    assert_eq!(
        policy.with_scope("overflow").unwrap_err(),
        PolicyError::TooManyEntries
    );
}

#[test]
fn policy_matching_is_exact_and_erasure_authority_includes_domain() {
    let first = CheckPolicy::new("same").unwrap().with_scope("a").unwrap();
    let second = CheckPolicy::new("same").unwrap().with_scope("b").unwrap();
    let checked = check(resolved("struct S {}"), &first).unwrap();
    assert!(checked.require_policy(&second).is_err());

    let mut sources = SourceSet::new();
    sources
            .add_standard_library(
                "std.srx",
                "mod std { value N(int); struct Public { n: N; } struct Box<T> { n: N; hidden: T; } outputs { type N = N; type Public = Public; type BoxN = Box<N>; } }",
            )
            .unwrap();
    sources
            .add(
                "main.srx",
                "mod std { value N(int); struct Public { n: N; } struct Box<T> { n: N; hidden: T; } fn exploit() -> Public { erase<Public>(Box<N> { n = 1; hidden = 2; }) } }",
            )
            .unwrap();
    let policy = CheckPolicy::new("domain erasure")
        .unwrap()
        .with_erasure(
            CanonicalItemIdentity::new(SourceDomainId::standard_library(), ["std", "Box"]).unwrap(),
            CanonicalItemIdentity::new(SourceDomainId::standard_library(), ["std", "Public"])
                .unwrap(),
        )
        .unwrap();
    let errors = check(resolve(parse_sources(&sources).unwrap()).unwrap(), &policy).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("erasure is not allowed"))
    );
}

#[test]
fn indirect_specialized_carriers_remain_affine_but_phantoms_do_not() {
    let errors = messages(
        "resource R(int); struct Inner { r: R; } struct Box<T> { x: T; } struct Outer { b: Box<Inner>; } fn f(x: Outer) { x; x; }",
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("moved affine"))
    );
    checked("resource R(int); struct Phantom<T> {} fn f(x: Phantom<R>) { x; x; }");
    checked(
        "resource R(int); struct Phantom<T> {} struct Outer { item: Phantom<R>; } fn f(x: Outer) { x; x; }",
    );
}

#[test]
fn contextual_match_patterns_are_validated_and_recorded() {
    let errors = messages("enum E { A } fn f(e: E) -> E { match e { Bogus => E::A, _ => E::A } }");
    assert!(
        errors
            .iter()
            .any(|message| message.contains("enum has no variant `Bogus`"))
    );

    let program = checked("enum E { A } fn f(e: E) -> E { match e { A => E::A } }");
    let pattern = program.patterns().next().unwrap();
    assert_eq!(pattern.index(), 0);
}

#[test]
fn declared_return_type_elaborates_function_tails() {
    checked(
        "enum E { A } value V(int); fn enum_value() -> E { A } fn wrapped() -> V { 1 } fn empty() -> [E] { [] }",
    );
}

#[test]
fn generic_defaults_are_checked_symbolically() {
    let program = checked(
        "struct Phantom<T> {} struct G<T> { p: Phantom<T> = Phantom<T> {}; } struct S {} outputs { g: G<S> = G<S> {}; }",
    );
    assert!(program.expressions().any(|expression| matches!(
        expression.ty(),
        Ty::Specialization { arguments, .. }
            if matches!(arguments.as_slice(), [Ty::Parameter(_)])
    )));
    assert!(
        messages("struct Bad<T> { item: T = 1; }")
            .iter()
            .any(|message| message.contains("struct field default type mismatch"))
    );
}

#[test]
fn generic_instance_and_metadata_limits_fail_closed() {
    let generic_errors = check_with_limits(
        resolved(
            "struct A {} struct B {} struct Box<T> { x: T; } type One = Box<A>; type Two = Box<B>;",
        ),
        &CheckPolicy::default(),
        CheckLimits {
            max_generic_instances: 1,
            ..CheckLimits::default()
        },
    )
    .unwrap_err();
    assert!(
        generic_errors
            .iter()
            .any(|error| error.message == "generic instance limit reached")
    );

    let metadata_errors = check_with_limits(
        resolved("value V(int); fn f(x: V) { x; x; }"),
        &CheckPolicy::default(),
        CheckLimits {
            max_metadata_units: 1,
            ..CheckLimits::default()
        },
    )
    .unwrap_err();
    assert!(
        metadata_errors
            .iter()
            .any(|error| error.message == "checked metadata limit reached")
    );
}

#[test]
fn moved_value_diagnostics_obey_the_phase_cap() {
    let mut source = String::from("resource R(int); fn f(x: R) {");
    for _ in 0..MAX_DIAGNOSTICS + 44 {
        source.push_str("x;");
    }
    source.push('}');
    assert_eq!(messages(&source).len(), MAX_DIAGNOSTICS);
}

#[test]
fn checked_metadata_builds_complete_span_indexes_once() {
    let mut source = String::from("value V(int); fn f(x: V) {");
    for _ in 0..2_048 {
        source.push_str("x;");
    }
    source.push('}');
    let program = checked(&source);
    assert_eq!(program.expression_index.len(), program.expressions.len());
    assert_eq!(program.pattern_index.len(), program.patterns.len());
    assert!(
        program
            .expressions()
            .all(|expression| program.expression(expression.span()).is_some())
    );
}
