use super::*;
use crate::{AnalysisHost, CheckPolicy, ResolutionOwnerPart, SourceDomainId, SourceSet};

fn analyze(files: &[(&str, &str)]) -> SemanticAnalysis {
    analyze_with(files, &CheckPolicy::default())
}

fn analyze_with(files: &[(&str, &str)], policy: &CheckPolicy) -> SemanticAnalysis {
    let mut sources = SourceSet::new();
    for (name, text) in files {
        sources.add(*name, *text).unwrap();
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

fn key(name: &str) -> ResolutionOwnerKey {
    ResolutionOwnerKey {
        domain: SourceDomainId::project(),
        path: vec![name.into()],
        part: ResolutionOwnerPart::Declaration,
    }
}

fn cold(analysis: &SemanticAnalysis, key: &ResolutionOwnerKey) -> OwnerCheckedFacts {
    let owner = analysis
        .owner_resolution(key, &AnalysisCancellation::default())
        .unwrap()
        .unwrap();
    let span = owner.source_map.span();
    let body = analysis
        .bodies
        .entries
        .iter()
        .find(|body| {
            body.anchor.source_id() == span.source_id()
                && body.anchor.start() >= span.start()
                && body.anchor.end() <= span.end()
        })
        .unwrap();
    OwnerCheckedFacts {
        diagnostics: analysis.body_diagnostics[body.diagnostics.clone()].to_vec(),
        expressions: analysis.expressions[body.expressions.clone()].to_vec(),
        patterns: analysis.editor.patterns[body.patterns.clone()].to_vec(),
        locals: body
            .locals
            .iter()
            .map(|id| (*id, analysis.editor.locals[id].clone()))
            .collect(),
        ownership: analysis.editor.uses[body.uses.clone()].to_vec(),
        fields: analysis.editor.fields[body.fields.clone()].to_vec(),
        arguments: analysis.editor.arguments[body.arguments.clone()].to_vec(),
        outputs: body
            .outputs
            .iter()
            .map(|id| (*id, analysis.editor.outputs[id].clone()))
            .collect(),
    }
}

#[test]
fn relocated_bodies_match_every_cold_fact_after_ids_and_offsets_move() {
    let text = r"
        value I(int); resource R(int);
        enum Choice<T> { Left(T), Right(T) }
        struct Box<T> { payload: T; }
        struct Defaults<T> { make: fn(T) -> T = fn(x: T) -> T { x }; }
        fn identity<T>(x: T) -> T { x }
        fn inspect(x: Choice<R>) -> R { match x { Left(y) => y, Right(y) => y } }
        fn capture(r: R) -> R { let f = once fn() -> R { r }; f() }
        fn bad(r: R) { r; r; }
        fn fields(x: Box<I>) -> I { identity(x.payload) }
        outputs { result: I = fields(Box<I> { payload = I(1); }); }
    ";
    let before = analyze(&[("main.srx", text)]);
    let changed = format!(
        "// déplacement é 🦀\nvalue Before(int); fn earlier<T>(x: T) -> T {{ let y=x; y }}\n{text}"
    );
    let after = analyze(&[("main.srx", &changed)]);
    let mut keys = vec![
        key("inspect"),
        key("capture"),
        key("bad"),
        key("fields"),
        key("result"),
    ];
    keys.push(ResolutionOwnerKey {
        part: ResolutionOwnerPart::FieldDefault("make".into()),
        ..key("Defaults")
    });
    for key in &keys {
        let mapped = after
            .remap_checked_body_from(&before, key, &AnalysisCancellation::default())
            .unwrap()
            .unwrap();
        assert_eq!(mapped, cold(&after, key), "{key:?}");
        assert_ne!(mapped, cold(&before, key), "{key:?}");
    }
    assert!(!cold(&after, &key("inspect")).patterns.is_empty());
    assert!(!cold(&after, &key("bad")).diagnostics.is_empty());
    assert!(
        cold(&after, &key("capture"))
            .ownership
            .iter()
            .any(|usage| usage.closure.is_some())
    );
    assert!(!cold(&after, &key("fields")).fields.is_empty());
    assert!(!cold(&after, &key("fields")).arguments.is_empty());
    assert!(!cold(&after, &key("result")).outputs.is_empty());
}

#[test]
fn reordered_sources_relocate_external_field_and_parameter_spans() {
    let main = "fn inspect(x: lib::Box) -> lib::I { lib::identity(x.payload) }";
    let lib = "mod lib { pub value I(int); pub struct Box { payload: I; } pub fn identity<T>(x: T) -> T { x } }";
    let before = analyze(&[("main.srx", main), ("lib.srx", lib)]);
    assert!(
        before.diagnostics().is_empty(),
        "{:?}",
        before.diagnostics()
    );
    let changed = format!("// é\n{lib}");
    let after = analyze(&[("lib.srx", &changed), ("main.srx", main)]);
    let key = key("inspect");
    let mapped = after
        .remap_checked_body_from(&before, &key, &AnalysisCancellation::default())
        .unwrap()
        .unwrap();
    assert_eq!(mapped, cold(&after, &key));
    assert!(!mapped.fields.is_empty() && !mapped.arguments.is_empty());
    assert_ne!(mapped, cold(&before, &key));
}

#[test]
fn recovered_bodies_keep_unknown_status_and_movement_diagnostics() {
    let text = "value I(int); resource R(int); fn inspect(r: R) { let missing: I = ; let later=I(2); r; r; }";
    let before = analyze(&[("main.srx", text)]);
    let changed = format!("fn earlier() {{}} {text}");
    let after = analyze(&[("main.srx", &changed)]);
    let key = key("inspect");
    let mapped = after
        .remap_checked_body_from(&before, &key, &AnalysisCancellation::default())
        .unwrap()
        .unwrap();
    assert_eq!(mapped, cold(&after, &key));
    assert!(!mapped.locals.is_empty());
    assert!(
        mapped
            .locals
            .values()
            .any(|local| local.status != crate::TypeStatus::Known)
    );
}

#[test]
fn elaborations_discovery_and_erasure_relocate_all_embedded_identities() {
    let text = r"
        value I(int); value Key(str); enum Mode { Fast, Slow }
        enum Choice<T> { Left(T), Right(T) }
        struct Box<T> { n: I; hidden: T; } struct Public { n: I; }
        mod collection { pub fn make() -> I { I(1) } }
        fn identity<T>(x: T) -> T { x }
        outputs {
            keys: [Key] = module_exports(collection, make, fn(key: Key, make: fn() -> I) -> Key { key });
            p: Public = erase<Public>(Box<I> { n = 1; hidden = 2; });
            mode: Mode = Fast;
            choice: Choice<I> = Choice::Left<I>(I(1));
            result: I = identity(I(2));
        }
    ";
    let identity = |name| CanonicalItemIdentity::new(SourceDomainId::project(), [name]).unwrap();
    let policy = CheckPolicy::default()
        .with_erasure(identity("Box"), identity("Public"))
        .unwrap();
    let before = analyze_with(&[("main.srx", text)], &policy);
    assert!(
        before.diagnostics().is_empty(),
        "{:?}",
        before.diagnostics()
    );
    let changed = format!("value Earlier(int); fn prior(x: Earlier) {{}} {text}");
    let after = analyze_with(&[("main.srx", &changed)], &policy);
    let mut kinds = BTreeSet::new();
    for name in ["keys", "p", "mode", "choice", "result"] {
        let mapped = after
            .remap_checked_body_from(&before, &key(name), &AnalysisCancellation::default())
            .unwrap()
            .unwrap();
        assert_eq!(mapped, cold(&after, &key(name)));
        for expression in mapped.expressions {
            if let Some(elaboration) = expression.elaboration() {
                kinds.insert(match elaboration {
                    crate::Elaboration::ModuleExports { .. } => 0,
                    crate::Elaboration::VariantConstructor { .. } => 1,
                    crate::Elaboration::FunctionSpecialization { .. } => 2,
                    crate::Elaboration::ContextualVariant { .. } => 3,
                    crate::Elaboration::ValueLiteral(_) => 4,
                    crate::Elaboration::Erasure { .. } => 5,
                });
            }
        }
    }
    assert_eq!(kinds.len(), 6, "{kinds:?}");
    let changed_policy = policy.with_scope("new_scope").unwrap();
    let different = analyze_with(&[("main.srx", text)], &changed_policy);
    assert!(
        different
            .remap_checked_body_from(&before, &key("result"), &AnalysisCancellation::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn remapped_effects_replay_in_the_receiving_type_coordinates() {
    let text = "value I(int); struct Box<T> { payload: T; } fn inspect() { let x=Box<I> { payload=I(1); }; }";
    let before = analyze(&[("main.srx", text)]);
    let changed = format!("value Earlier(int); fn earlier(x: Earlier) {{}} {text}");
    let after = analyze(&[("main.srx", &changed)]);
    let cancel = AnalysisCancellation::default();
    let old = after
        .remap_body_effects_from(&before, &key("inspect"), &cancel)
        .unwrap()
        .unwrap();
    let cold = after
        .remap_body_effects_from(&after, &key("inspect"), &cancel)
        .unwrap()
        .unwrap();
    assert_ne!(old.entry(), cold.entry());
    assert_eq!(cold.entry().generic_instances, 0);
    let input = crate::BodyReplayState {
        budget: cold.entry(),
        instances: BTreeSet::new(),
    };
    let mapped = old
        .replay(&input, crate::CheckLimits::default(), &cancel)
        .unwrap()
        .unwrap();
    let expected = cold
        .replay(&input, crate::CheckLimits::default(), &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(mapped, expected);
    assert_eq!(mapped.budget, cold.exit());
    assert_eq!(mapped.instances.len(), 1);
    cancel.cancel();
    assert!(
        after
            .remap_body_effects_from(&before, &key("inspect"), &cancel)
            .is_err()
    );
}

#[test]
fn remapping_limits_and_incomplete_publications_return_no_partial_facts() {
    let text = "value I(int); fn inspect(x: I) -> I { x }";
    let before = analyze(&[("main.srx", text)]);
    let key = key("inspect");
    let cancel = AnalysisCancellation::default();
    for (units, bytes) in [(0, usize::MAX), (usize::MAX, 0)] {
        assert!(
            before
                .remap_checked_body_bounded(&before, &key, &cancel, units, bytes)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        before
            .remap_checked_body_from(&before, &key, &cancel)
            .unwrap()
            .unwrap(),
        cold(&before, &key)
    );
    let mut after = analyze(&[("main.srx", text)]);
    std::sync::Arc::get_mut(&mut after.bodies).unwrap().entries[0].complete = false;
    assert!(
        after
            .remap_checked_body_from(&before, &key, &cancel)
            .unwrap()
            .is_none()
    );
    assert!(
        before
            .remap_checked_body_from(&after, &key, &cancel)
            .unwrap()
            .is_none()
    );
}

#[test]
fn changed_text_interfaces_topology_and_missing_owners_refuse_relocation() {
    let text = "value I(int); fn inspect(x: I) -> I { x }";
    let before = analyze(&[("main.srx", text)]);
    for changed in [
        text.replace("value I", "resource I"),
        text.replace("{ x }", "{ let y=x; y }"),
        text.replace("fn inspect", "fn other"),
        format!("{text} fn inspect() {{}} "),
    ] {
        let after = analyze(&[("main.srx", &changed)]);
        assert!(
            after
                .remap_checked_body_from(&before, &key("inspect"), &AnalysisCancellation::default())
                .unwrap()
                .is_none()
        );
    }
    let after = analyze(&[("main.srx", text), ("added.srx", "fn extra() {}")]);
    assert!(
        after
            .remap_checked_body_from(&before, &key("inspect"), &AnalysisCancellation::default())
            .unwrap()
            .is_none()
    );
    let cancel = AnalysisCancellation::default();
    cancel.cancel();
    assert!(
        before
            .remap_checked_body_from(&before, &key("inspect"), &cancel)
            .is_err()
    );
}
