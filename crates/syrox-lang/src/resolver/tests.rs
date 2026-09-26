use super::*;
use crate::{SourceId, SourceSet, parse_sources};

fn resolved(sources: &[(&str, &str)]) -> ResolvedProgram {
    let mut set = SourceSet::new();
    for (name, text) in sources {
        set.add(*name, *text).unwrap();
    }
    resolve(parse_sources(&set).unwrap()).unwrap()
}

fn errors(sources: &[(&str, &str)]) -> Vec<Diagnostic> {
    let mut set = SourceSet::new();
    for (name, text) in sources {
        set.add(*name, *text).unwrap();
    }
    resolve(parse_sources(&set).unwrap()).unwrap_err()
}

#[test]
fn forward_references_and_repeated_modules_across_sources_resolve() {
    let program = resolved(&[
        (
            "first.srx",
            "mod shared::pkg { fn identity(item: Later) -> Later { item } }",
        ),
        ("second.srx", "mod shared::pkg { struct Later {} }"),
    ]);

    let modules: Vec<_> = program
        .modules()
        .map(|module| module.path().segments().to_vec())
        .collect();
    assert_eq!(
        modules,
        [
            vec![],
            vec!["shared".to_owned()],
            vec!["shared".to_owned(), "pkg".to_owned()]
        ]
    );
    assert_eq!(
        program
            .items()
            .find(|item| item.path().segments().last().unwrap() == "Later")
            .unwrap()
            .id(),
        ItemId(0)
    );
}

#[test]
fn duplicate_declarations_and_import_conflicts_are_diagnosed() {
    let duplicate = errors(&[("main.srx", "struct Same {} fn Same() {}")]);
    assert!(
        duplicate
            .iter()
            .any(|error| error.message.contains("duplicate item"))
    );

    let conflict = errors(&[(
        "main.srx",
        "mod a { value Name(str); } mod b { value Name(str); } mod c { use a::Name; use b::Name; }",
    )]);
    assert!(
        conflict
            .iter()
            .any(|error| error.message.contains("conflicting imports"))
    );
}

#[test]
fn unknown_types_and_closed_interface_private_access_are_diagnosed() {
    let unknown = errors(&[("main.srx", "fn bad(item: Missing) {}")]);
    assert!(
        unknown
            .iter()
            .any(|error| error.message == "unknown type `Missing`")
    );

    let private = errors(&[(
        "main.srx",
        "mod api { struct Hidden {} outputs { type Public = Hidden; } } fn bad(item: api::Hidden) {}",
    )]);
    assert!(
        private
            .iter()
            .any(|error| error.message.contains("private to a module interface"))
    );
}

#[test]
fn locals_are_distinct_from_items_and_bare_variants_remain_contextual() {
    let program = resolved(&[(
        "main.srx",
        "value Name(str); enum Mode { Fast } fn choose(Name: Name) -> Name { let copy = Name; match Fast { Fast => copy } }",
    )]);

    assert!(program.references().any(|reference| {
        reference.kind() == ReferenceKind::Value
            && matches!(reference.target(), ResolvedTarget::Local(_))
    }));
    assert!(program.references().any(|reference| {
        reference.kind() == ReferenceKind::Pattern
            && reference.target() == &ResolvedTarget::ContextualEnumVariant
    }));
    assert_eq!(program.contextual_obligations().count(), 2);
    assert!(program.references().any(|reference| {
        reference.kind() == ReferenceKind::Type
            && matches!(reference.target(), ResolvedTarget::Item(_))
    }));
}

#[test]
fn canonical_ids_and_diagnostic_messages_do_not_depend_on_source_order() {
    let left = resolved(&[
        ("a.srx", "mod merged { struct A {} }"),
        ("b.srx", "mod merged { struct B {} }"),
    ]);
    let right = resolved(&[
        ("b.srx", "mod merged { struct B {} }"),
        ("a.srx", "mod merged { struct A {} }"),
    ]);
    let identities = |program: &ResolvedProgram| {
        program
            .items()
            .map(|item| (item.path().segments().to_vec(), item.id()))
            .collect::<Vec<_>>()
    };
    assert_eq!(identities(&left), identities(&right));

    let first = errors(&[("a.srx", "struct Same {}"), ("b.srx", "struct Same {}")]);
    let second = errors(&[("b.srx", "struct Same {}"), ("a.srx", "struct Same {}")]);
    assert_eq!(
        first.iter().map(|error| &error.message).collect::<Vec<_>>(),
        second
            .iter()
            .map(|error| &error.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn domains_isolate_merges_and_only_authenticated_std_uses_reserved_lookup() {
    let mut sources = SourceSet::new();
    sources
        .add("project.srx", "mod shared { struct Project {} }")
        .unwrap();
    let input_domain = sources.create_input_domain("input").unwrap();
    sources
        .add_to_input_domain(
            input_domain,
            "input.srx",
            "mod shared { struct Input {} fn own(item: Input) {} }",
        )
        .unwrap();
    sources
        .add_standard_library(
            "std.srx",
            "mod std { struct Trusted {} outputs { type Trusted = Trusted; } }",
        )
        .unwrap();
    sources
        .add("consumer.srx", "fn trusted(item: std::Trusted) {}")
        .unwrap();

    let program = resolve(parse_sources(&sources).unwrap()).unwrap();
    let shared: Vec<_> = program
        .modules()
        .filter(|module| module.path().segments() == ["shared"])
        .map(ResolvedModule::domain)
        .collect();
    assert_eq!(shared.len(), 2);
    assert_ne!(shared[0], shared[1]);
    assert!(program.items().any(|item| {
        item.domain() == SourceDomainId::STANDARD_LIBRARY
            && item.path().segments() == ["std", "Trusted"]
    }));

    let forged = errors(&[(
        "project.srx",
        "mod std { struct Forged {} } fn bad(item: std::Forged) {}",
    )]);
    assert!(
        forged
            .iter()
            .any(|error| error.message == "unknown type `std::Forged`")
    );

    let mut private_std = SourceSet::new();
    private_std
        .add_standard_library("std.srx", "mod std { struct Private {} }")
        .unwrap();
    private_std
        .add("project.srx", "fn bad(item: std::Private) {}")
        .unwrap();
    let diagnostics = resolve(parse_sources(&private_std).unwrap()).unwrap_err();
    assert!(
        diagnostics
            .iter()
            .any(|error| error.message.contains("private to a module interface"))
    );
}

#[test]
fn authenticated_project_input_alias_resolves_only_root_outputs() {
    let mut sources = SourceSet::new();
    sources
        .add(
            "project.srx",
            "inputs { catalog = \"path:catalog\"; } fn public() { catalog::make(); } fn private() { catalog::hidden(); }",
        )
        .unwrap();
    let catalog = sources.create_input_domain("catalog").unwrap();
    sources
        .add_to_input_domain(
            catalog,
            "catalog.srx",
            "value Result(str); fn make() -> Result { \"ok\" } fn hidden() {} outputs { make: fn() -> Result = make; }",
        )
        .unwrap();

    let diagnostics = resolve(parse_sources(&sources).unwrap()).unwrap_err();
    assert!(
        diagnostics
            .iter()
            .any(|error| { error.message == "item is private to a module interface" })
    );

    let mut sources = SourceSet::new();
    sources
        .add(
            "project.srx",
            "inputs { catalog = \"path:catalog\"; } fn call() { catalog::make(); }",
        )
        .unwrap();
    let catalog = sources.create_input_domain("catalog").unwrap();
    sources
        .add_to_input_domain(
            catalog,
            "catalog.srx",
            "value Result(str); fn make() -> Result { \"ok\" } outputs { make: fn() -> Result = make; }",
        )
        .unwrap();
    let program = resolve(parse_sources(&sources).unwrap()).unwrap();
    let target = program
        .references()
        .find(|reference| reference.kind() == ReferenceKind::Function)
        .and_then(|reference| match reference.target() {
            ResolvedTarget::Item(item) => Some(*item),
            _ => None,
        })
        .unwrap();
    let item = program.items().find(|item| item.id() == target).unwrap();
    assert_eq!(item.domain(), catalog);
    assert_eq!(item.path().segments(), ["make"]);
}

#[test]
fn loader_binding_without_a_matching_inputs_declaration_is_not_visible() {
    let mut sources = SourceSet::new();
    sources
        .add("project.srx", "fn call() { catalog::make(); }")
        .unwrap();
    let catalog = sources.create_input_domain("catalog").unwrap();
    sources
        .add_to_input_domain(
            catalog,
            "catalog.srx",
            "value Result(str); fn make() -> Result { \"ok\" } outputs { make: fn() -> Result = make; }",
        )
        .unwrap();

    let diagnostics = resolve(parse_sources(&sources).unwrap()).unwrap_err();
    assert!(
        diagnostics
            .iter()
            .any(|error| error.message == "unknown function `catalog::make`")
    );
}

#[test]
fn input_domains_cannot_use_project_input_aliases() {
    let mut sources = SourceSet::new();
    sources
        .add(
            "project.srx",
            "inputs { catalog = \"path:catalog\"; sibling = \"path:sibling\"; }",
        )
        .unwrap();
    let catalog = sources.create_input_domain("catalog").unwrap();
    sources
        .add_to_input_domain(
            catalog,
            "catalog.srx",
            "value Result(str); fn make() -> Result { \"ok\" } outputs { make: fn() -> Result = make; }",
        )
        .unwrap();
    let sibling = sources.create_input_domain("sibling").unwrap();
    sources
        .add_to_input_domain(sibling, "sibling.srx", "fn denied() { catalog::make(); }")
        .unwrap();

    let diagnostics = resolve(parse_sources(&sources).unwrap()).unwrap_err();
    assert!(
        diagnostics
            .iter()
            .any(|error| error.message == "unknown function `catalog::make`")
    );
}

#[test]
fn inputs_blocks_are_rejected_outside_the_project_domain_root() {
    let mut sources = SourceSet::new();
    sources
        .add(
            "project.srx",
            "inputs { allowed = \"path:allowed\"; } mod nested { inputs { bad = \"path:bad\"; } }",
        )
        .unwrap();
    let input = sources.create_input_domain("allowed").unwrap();
    sources
        .add_to_input_domain(
            input,
            "input.srx",
            "inputs { transitive = \"path:transitive\"; } mod nested { inputs { also_bad = \"path:bad\"; } }",
        )
        .unwrap();

    let diagnostics = resolve(parse_sources(&sources).unwrap()).unwrap_err();
    let placement = diagnostics
        .iter()
        .filter(|error| error.message == "`inputs` blocks are only allowed at the project root")
        .count();
    assert_eq!(placement, 3);
}

#[test]
fn interface_binding_reaches_a_bounded_fixpoint() {
    resolved(&[
        (
            "a.srx",
            "mod a { struct Base {} outputs { type A = Base; } }",
        ),
        ("b.srx", "mod b { use a::A; outputs { type B = A; } }"),
        ("c.srx", "mod c { use b::B; outputs { type C = B; } }"),
        ("main.srx", "fn consume(item: c::C) {}"),
    ]);
}

#[test]
fn local_namespaces_qualification_and_simultaneous_duplicates_are_checked() {
    let program = resolved(&[("valid.srx", "struct T {} fn f(T: T) -> T { erase<T>(T) }")]);
    assert!(program.references().any(|reference| {
        reference.kind() == ReferenceKind::Type
            && matches!(reference.target(), ResolvedTarget::Item(_))
    }));

    let invalid = errors(&[(
        "invalid.srx",
        "struct T {} value V(str); struct Box<T> { field: V = T; } fn qualified(x: V) { x::bad; } fn duplicate(x: V, x: V) {} struct Pair<A, A> {}",
    )]);
    let messages: Vec<_> = invalid.iter().map(|error| error.message.as_str()).collect();
    assert!(messages.contains(&"local value cannot qualify a `::` path"));
    assert!(messages.contains(&"duplicate function parameter"));
    assert!(messages.contains(&"duplicate type parameter"));
    assert!(messages.contains(&"struct used where value is required"));
}

#[test]
fn every_string_interpolation_is_resolved_in_its_context() {
    let diagnostics = errors(&[(
        "main.srx",
        "inputs { source = \"${missing}\"; } value V(str) where in [\"${also_missing}\"];",
    )]);
    assert_eq!(
        diagnostics
            .iter()
            .filter(|error| error.message == "unknown local in string interpolation")
            .count(),
        2
    );
}

#[test]
fn imported_items_shadow_outer_modules_in_qualified_paths() {
    let diagnostics = errors(&[(
        "main.srx",
        "mod a { struct T {} } mod source { struct a {} } mod client { use source::a; fn f(item: a::T) {} }",
    )]);
    assert!(
        diagnostics
            .iter()
            .any(|error| { error.message == "struct used as a module in a qualified path" })
    );
}

#[test]
fn local_id_overflow_is_a_diagnostic_not_aliasing() {
    let mut sources = SourceSet::new();
    sources.add("empty.srx", "").unwrap();
    let parsed = parse_sources(&sources).unwrap();
    let mut resolver = Resolver::new(&parsed);
    resolver.next_local = u32::MAX;

    assert_eq!(resolver.local(Span::new(SourceId::SINGLE, 0, 0)), None);
    assert_eq!(
        resolver.diagnostics[0].message,
        "too many local declarations"
    );
}

#[test]
fn every_resolution_pass_fails_closed_at_the_work_limit() {
    let mut sources = SourceSet::new();
    sources
        .add("main.srx", "struct T {} fn f(item: T) { item; }")
        .unwrap();
    let parsed = parse_sources(&sources).unwrap();
    let mut resolver = Resolver::new(&parsed);
    resolver.collect();
    resolver.work = MAX_RESOLUTION_WORK;
    resolver.resolve_contents();

    assert!(resolver.exhausted);
    assert_eq!(
        resolver.diagnostics[0].message,
        "name resolution work limit reached"
    );
    assert!(resolver.references.is_empty());
}
