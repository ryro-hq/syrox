use super::*;
use crate::{AnalysisHost, SourceDomainId, SourceSet};

fn analyze(files: &[(&str, &str)], policy: &CheckPolicy) -> SemanticAnalysis {
    let mut sources = SourceSet::new();
    for (path, text) in files {
        sources.add(*path, *text).unwrap();
    }
    AnalysisHost::default()
        .snapshot()
        .analyze_project(
            &sources,
            &BTreeMap::new(),
            policy,
            &AnalysisCancellation::default(),
        )
        .unwrap()
}

fn manifest(analysis: &SemanticAnalysis, name: &str) -> OwnerTypeDependencies {
    analysis
        .owner_type_dependencies(
            &ResolutionOwnerKey {
                domain: SourceDomainId::project(),
                path: vec![name.into()],
                part: ResolutionOwnerPart::Declaration,
            },
            &AnalysisCancellation::default(),
        )
        .unwrap()
        .unwrap()
}

#[test]
fn signatures_change_dependencies_but_other_bodies_and_offsets_do_not() {
    let text =
        "value I(int); value S(str); fn target() -> I { I(1) } fn inspect() -> I { target() }";
    let before = analyze(&[("main.srx", text)], &CheckPolicy::default());
    assert!(
        before.diagnostics().is_empty(),
        "{:?}",
        before.diagnostics()
    );
    let baseline = manifest(&before, "inspect");
    for changed in [
        text.replace("I(1)", "I(234)"),
        format!("// é\nfn earlier() {{}} {text}"),
    ] {
        let after = analyze(&[("main.srx", &changed)], &CheckPolicy::default());
        assert_eq!(baseline, manifest(&after, "inspect"));
    }
    let changed = text.replace(
        "fn target() -> I { I(1) }",
        "fn target() -> S { S(\"changed\") }",
    );
    let after = analyze(&[("main.srx", &changed)], &CheckPolicy::default());
    assert!(!after.diagnostics().is_empty());
    assert_ne!(baseline, manifest(&after, "inspect"));
}

#[test]
fn transitive_payload_changes_detect_affinity_through_aliases_and_generics() {
    let text = "value I(int); enum E<T> { Wrap(T) } type Alias = E<I>; struct Box { payload: Alias; } fn inspect(x: Box) { let a = x; let b = x; }";
    let before = analyze(&[("main.srx", text)], &CheckPolicy::default());
    assert!(
        before.diagnostics().is_empty(),
        "{:?}",
        before.diagnostics()
    );
    let baseline = manifest(&before, "inspect");
    assert!(baseline.interfaces.keys().any(|id| id.path() == ["I"]));
    let changed = text.replace("value I", "resource I");
    let after = analyze(&[("main.srx", &changed)], &CheckPolicy::default());
    assert!(
        after
            .diagnostics()
            .iter()
            .any(|d| d.code == crate::DiagnosticCode::MovedValue)
    );
    assert_ne!(baseline, manifest(&after, "inspect"));
}

#[test]
fn defaults_are_subowners_and_part_of_the_declared_interface() {
    let text = "value I(int); fn seed() -> I { I(1) } struct Box { payload: I = seed(); } fn inspect() -> Box { Box {} }";
    let before = analyze(&[("main.srx", text)], &CheckPolicy::default());
    assert!(
        before.diagnostics().is_empty(),
        "{:?}",
        before.diagnostics()
    );
    let baseline = manifest(&before, "inspect");
    assert!(baseline.interfaces.keys().any(|id| id.path() == ["seed"]));
    let changed = text.replace("= seed()", "= I(2)");
    assert_ne!(
        baseline,
        manifest(
            &analyze(&[("main.srx", &changed)], &CheckPolicy::default()),
            "inspect"
        )
    );
    let key = ResolutionOwnerKey {
        domain: SourceDomainId::project(),
        path: vec!["Box".into()],
        part: ResolutionOwnerPart::FieldDefault("payload".into()),
    };
    assert!(
        before
            .owner_type_dependencies(&key, &AnalysisCancellation::default())
            .unwrap()
            .is_some()
    );
}

#[test]
fn output_initializers_are_excluded_and_recursive_interfaces_terminate() {
    let text = "value I(int); struct Node { next: Node; } fn inspect(x: Node) -> Node { x } outputs { number: I = I(1); }";
    let before = analyze(&[("main.srx", text)], &CheckPolicy::default());
    assert_eq!(manifest(&before, "inspect").interfaces.len(), 2);
    let changed = text.replace("I(1)", "I(987)");
    assert_eq!(
        manifest(&before, "number"),
        manifest(
            &analyze(&[("main.srx", &changed)], &CheckPolicy::default()),
            "number"
        )
    );
}

#[test]
fn policy_rules_participate_even_when_policy_names_are_identical() {
    let text = "value I(int); fn inspect(x: I) -> I { x }";
    let first = CheckPolicy::new("same").unwrap();
    let second = first.clone().with_scope("scope").unwrap();
    assert_ne!(
        manifest(&analyze(&[("main.srx", text)], &first), "inspect"),
        manifest(&analyze(&[("main.srx", text)], &second), "inspect")
    );
}

#[test]
fn reexports_change_interface_bindings_even_with_identical_spelling() {
    let text = "mod a { pub value I(int); } mod b { pub value I(int); } mod api { pub use a::I; pub fn target(x: I) -> I { x } } fn inspect() { let f = api::target; }";
    let before = analyze(&[("main.srx", text)], &CheckPolicy::default());
    assert!(
        before.diagnostics().is_empty(),
        "{:?}",
        before.diagnostics()
    );
    let changed = text.replace("use a::I", "use b::I");
    let after = analyze(&[("main.srx", &changed)], &CheckPolicy::default());
    assert!(after.diagnostics().is_empty(), "{:?}", after.diagnostics());
    assert_ne!(manifest(&before, "inspect"), manifest(&after, "inspect"));
}

#[test]
fn opaque_owner_refinement_and_scope_are_interface_inputs() {
    let cases = [
        (
            "value I(int); mod vault { pub struct Secret { payload: I; } } fn inspect(x: vault::Secret) -> I { x.payload }",
            "pub struct Secret",
            "pub opaque struct Secret",
        ),
        (
            "value I(int); struct Agent {} mod vault { pub opaque struct Secret<owner O> { payload: I; } } fn inspect(x: vault::Secret<Agent>) -> I { x.payload }",
            "<owner O>",
            "<O>",
        ),
        (
            "value I(int) where 1..3; fn inspect(x: I) -> I { x }",
            "1..3",
            "1..4",
        ),
        (
            "resource R(int) in net; fn inspect(x: R) -> R { x }",
            "in net",
            "in local",
        ),
    ];
    let policy = CheckPolicy::default()
        .with_scope("net")
        .unwrap()
        .with_scope("local")
        .unwrap();
    for (text, old, new) in cases {
        let before = analyze(&[("main.srx", text)], &policy);
        assert!(
            before.diagnostics().is_empty(),
            "{:?}",
            before.diagnostics()
        );
        let changed = text.replace(old, new);
        let after = analyze(&[("main.srx", &changed)], &policy);
        assert_ne!(
            manifest(&before, "inspect"),
            manifest(&after, "inspect"),
            "{old}"
        );
        if old.starts_with("pub") || old.starts_with('<') {
            assert!(!after.diagnostics().is_empty());
        }
    }
}

#[test]
fn policy_erasure_targets_and_discovered_functions_enter_the_closure() {
    let text =
        "value I(int); struct Raw<T> { payload: T; } struct Erased { payload: I; } fn inspect() {}";
    let identity = |name| CanonicalItemIdentity::new(SourceDomainId::project(), [name]).unwrap();
    let policy = CheckPolicy::default()
        .with_erasure(identity("Raw"), identity("Erased"))
        .unwrap();
    let before = analyze(&[("main.srx", text)], &policy);
    let changed = text.replace(
        "struct Erased { payload: I; }",
        "struct Erased { payload: I; extra: I; }",
    );
    assert_ne!(
        manifest(&before, "inspect"),
        manifest(&analyze(&[("main.srx", &changed)], &policy), "inspect")
    );

    let text = "value I(int); mod recipes { mod a { pub fn recipe() -> I { I(1) } } } fn mapper() {} fn inspect() { module_exports(recipes, recipe, mapper); }";
    let before = analyze(&[("main.srx", text)], &CheckPolicy::default());
    let closure = manifest(&before, "inspect");
    assert!(
        closure
            .interfaces
            .keys()
            .any(|id| id.path() == ["recipes", "a", "recipe"])
    );
    let changed = text.replace("-> I { I(1) }", "{ }");
    assert_ne!(
        closure,
        manifest(
            &analyze(&[("main.srx", &changed)], &CheckPolicy::default()),
            "inspect"
        )
    );
}

#[test]
fn source_reordering_and_same_text_revalidation_preserve_manifests() {
    let main = "fn inspect(x: model::I) -> model::I { x }";
    let model = "mod model { pub value I(int); }";
    let before = analyze(
        &[("main.srx", main), ("model.srx", model)],
        &CheckPolicy::default(),
    );
    let after = analyze(
        &[("model.srx", model), ("main.srx", main)],
        &CheckPolicy::default(),
    );
    assert_eq!(manifest(&before, "inspect"), manifest(&after, "inspect"));
    assert!(before.diagnostics().is_empty() && after.diagnostics().is_empty());
    assert!(
        manifest(&before, "inspect")
            .interfaces
            .keys()
            .any(|id| id.path() == ["model", "I"])
    );
    let cancel = AnalysisCancellation::default();
    let revalidated = AnalysisHost::default()
        .snapshot()
        .revalidate_project_analysis(&before, &cancel)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&before.interfaces, &revalidated.interfaces));
    assert!(Arc::ptr_eq(&before.policy, &revalidated.policy));
    assert_eq!(
        manifest(&before, "inspect"),
        manifest(&revalidated, "inspect")
    );
}

#[test]
fn duplicates_limits_and_cancellation_never_publish_partial_manifests() {
    let analysis = analyze(
        &[(
            "main.srx",
            "value I(int); value I(int); fn inspect(x: I) {}",
        )],
        &CheckPolicy::default(),
    );
    let key = ResolutionOwnerKey {
        domain: SourceDomainId::project(),
        path: vec!["inspect".into()],
        part: ResolutionOwnerPart::Declaration,
    };
    assert!(
        analysis
            .owner_type_dependencies(&key, &AnalysisCancellation::default())
            .unwrap()
            .is_none()
    );
    let analysis = analyze(
        &[("main.srx", "value I(int); fn inspect(x: I) {}")],
        &CheckPolicy::default(),
    );
    for limits in [
        Limits {
            units: 0,
            bytes: usize::MAX,
        },
        Limits {
            units: usize::MAX,
            bytes: 0,
        },
    ] {
        let index = analysis
            .build_interfaces(limits, &AnalysisCancellation::default())
            .unwrap();
        assert!(index.truncated && index.entries.is_empty());
    }
    let cancelled = AnalysisCancellation::default();
    cancelled.cancel();
    assert!(analysis.owner_type_dependencies(&key, &cancelled).is_err());
    assert!(analysis.interfaces.get().is_none());
    assert!(
        analysis
            .owner_type_dependencies(&key, &AnalysisCancellation::default())
            .unwrap()
            .is_some()
    );
    assert!(analysis.owner_type_dependencies(&key, &cancelled).is_err());
}
