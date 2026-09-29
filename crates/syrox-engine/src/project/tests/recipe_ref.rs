use super::*;

#[test]
fn imported_function_output_is_a_lazy_value_with_a_direct_call_and_preserved_capture() {
    let workspace = TempProject::new();
    let config = package_configuration();
    workspace.write(
        "child/main.srx",
        r#"
        fn configure(id: std::PackageId) -> fn() -> std::Package {
            fn() -> std::Package { std::Package { id = id; dependencies = []; } }
        }
        fn recursive() -> fn() -> std::Package { recursive() }
        outputs {
            selected: fn() -> std::Package = configure("captured");
            unused: fn() -> std::Package = recursive();
        }
    "#,
    );
    workspace.write(
        "consumer/main.srx",
        r#"
        inputs { child = "path:../child"; }
        outputs { package: std::Package = child::selected(); }
    "#,
    );
    lock_project_with(&workspace.0.join("child"), &config).unwrap();
    let consumer = workspace.0.join("consumer");
    lock_project_with(&consumer, &config).unwrap();
    let plan = plan_project_with(&consumer, &config).unwrap();
    assert_eq!(plan.packages().len(), 1);
    assert_eq!(plan.packages().next().unwrap().id().as_str(), "captured");
}

#[test]
fn imported_homonymous_catalog_entries_are_distinct_and_reference_aliases_keep_identity() {
    use syrox_lang::Value;
    fn factory(reference: &Value) -> syrox_lang::MemoId {
        let Value::Struct { fields, .. } = reference else {
            panic!("expected RecipeRef");
        };
        let Value::MemoizedFunction { id, .. } =
            &fields.iter().find(|(name, _)| name == "factory").unwrap().1
        else {
            panic!("expected memoized factory");
        };
        id.clone()
    }
    let workspace = TempProject::new();
    let config = package_configuration();
    for name in ["left", "right"] {
        workspace.write(&format!("{name}/main.srx"), r#"
            fn hello() -> std::Package { std::Package { id = "hello"; dependencies = []; } }
            outputs {
                packages: std::Result<std::PackageSet<std::Package>, std::MapKey> = std::package_set([
                    std::MapEntry::Entry("hello", hello)
                ]);
            }
        "#);
        lock_project_with(&workspace.0.join(name), &config).unwrap();
    }
    workspace.write("consumer/main.srx", r#"
        inputs { left = "path:../left"; right = "path:../right"; }
        fn selected(set: std::Result<std::PackageSet<std::Package>, std::MapKey>) -> std::RecipeRef<std::Package> {
            match set {
                Ok(packages) => match std::package_ref(packages, "hello") {
                    Some(found) => found, None => selected(set),
                },
                Err(_) => selected(set),
            }
        }
        outputs {
            left_ref: std::RecipeRef<std::Package> = selected(left::packages);
            right_ref: std::RecipeRef<std::Package> = selected(right::packages);
            alias: std::RecipeRef<std::Package> = left_ref;
        }
    "#);
    let consumer = workspace.0.join("consumer");
    lock_project_with(&consumer, &config).unwrap();
    let project = open_locked_project_with(&consumer, &config).unwrap();
    let mut session = project.evaluation().unwrap();
    let left = factory(session.evaluate_root("left_ref").unwrap().value().unwrap());
    let right = factory(session.evaluate_root("right_ref").unwrap().value().unwrap());
    let alias = factory(session.evaluate_root("alias").unwrap().value().unwrap());
    assert_eq!(left, alias);
    assert_ne!(left, right);
}

#[test]
#[allow(clippy::too_many_lines)]
fn imported_named_reference_and_package_set_entry_share_the_same_instance() {
    use syrox_lang::Value;

    fn memo_id(value: &Value) -> syrox_lang::MemoId {
        let Value::Struct { fields, .. } = value else {
            panic!("expected RecipeRef");
        };
        let Value::MemoizedFunction { id, .. } =
            &fields.iter().find(|(name, _)| name == "factory").unwrap().1
        else {
            panic!("expected memoized factory");
        };
        id.clone()
    }

    let workspace = TempProject::new();
    let config = package_configuration();
    workspace.write(
        "catalog/main.srx",
        r#"
        fn make_hello() -> std::Package {
            std::Package { id = "hello"; dependencies = []; }
        }
        fn dormant() -> std::Package { dormant() }
        outputs {
            hello: std::RecipeRef<std::Package> = std::recipe_ref(make_hello);
            packages: std::Result<std::PackageSet<std::Package>, std::MapKey> =
                std::package_set_refs([
                    std::MapEntry::Entry("hello", hello),
                    std::MapEntry::Entry("unused", std::recipe_ref(dormant)),
                ]);
        }
        "#,
    );
    workspace.write(
        "consumer/main.srx",
        r#"
        inputs { pkgs = "path:../catalog"; }
        outputs {
            direct: std::RecipeRef<std::Package> = pkgs::hello;
            selected: std::RecipeRef<std::Package> = match pkgs::packages {
                Ok(set) => match std::package_ref(set, "hello") {
                    Some(reference) => reference, None => pkgs::hello,
                },
                Err(_) => pkgs::hello,
            };
            names: [std::MapKey] = match pkgs::packages {
                Ok(set) => std::package_names(set), Err(_) => [],
            };
        }
        "#,
    );
    let catalog = workspace.0.join("catalog");
    let consumer = workspace.0.join("consumer");
    lock_project_with(&catalog, &config).unwrap();
    lock_project_with(&consumer, &config).unwrap();
    let locked_catalog = open_locked_project_with(&catalog, &config).unwrap();
    let mut catalog_evaluation = locked_catalog.evaluation().unwrap();
    assert_eq!(
        catalog_evaluation
            .package_names("packages")
            .unwrap()
            .unwrap(),
        ["hello", "unused"]
    );
    assert!(catalog_evaluation.package_names("hello").unwrap().is_none());
    let catalog_named = memo_id(
        catalog_evaluation
            .evaluate_root("hello")
            .unwrap()
            .value()
            .unwrap(),
    );
    let catalog_selected = memo_id(
        catalog_evaluation
            .package_reference("packages", "hello")
            .unwrap()
            .unwrap(),
    );
    assert_eq!(catalog_named, catalog_selected);
    assert!(
        catalog_evaluation
            .package_reference("packages", "missing")
            .unwrap()
            .is_none()
    );
    assert!(
        catalog_evaluation
            .package_reference("hello", "hello")
            .unwrap()
            .is_none()
    );
    let editor = open_project_analysis_with(&consumer, &config).unwrap();
    let snapshot = editor.snapshot();
    let (source_id, source) = snapshot
        .sources()
        .iter()
        .find(|(_, source)| source.text().contains("inputs { pkgs"))
        .unwrap();
    let analysis = snapshot
        .analyze(&syrox_lang::AnalysisCancellation::default())
        .unwrap();
    assert!(
        analysis.diagnostics().is_empty(),
        "{:?}",
        analysis.diagnostics()
    );
    let member =
        u32::try_from(source.text().find("pkgs::hello").unwrap() + "pkgs::".len()).unwrap();
    assert_eq!(
        analysis
            .definition_at(source_id, member)
            .unwrap()
            .source_id(),
        snapshot
            .sources()
            .iter()
            .find(|(_, source)| source.text().contains("fn make_hello"))
            .unwrap()
            .0
    );
    let completions = analysis.complete_path(source_id, member, &["pkgs".into()], "");
    assert!(completions.iter().any(|(name, _)| name == "hello"));
    assert!(completions.iter().any(|(name, _)| name == "packages"));
    let project = open_locked_project_with(&consumer, &config).unwrap();
    let mut session = project.evaluation().unwrap();
    let direct = memo_id(session.evaluate_root("direct").unwrap().value().unwrap());
    let selected = memo_id(session.evaluate_root("selected").unwrap().value().unwrap());
    assert_eq!(direct, selected);
    let Value::List { items, .. } = session.evaluate_root("names").unwrap().value().unwrap() else {
        panic!("expected catalog names");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn catalog_names_report_duplicate_keys_without_running_recipe_factories() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        fn dormant() -> std::Package { dormant() }
        outputs {
            packages: std::Result<std::PackageSet<std::Package>, std::MapKey> = std::package_set([
                std::MapEntry::Entry("same", dormant),
                std::MapEntry::Entry("same", dormant),
            ]);
        }
        "#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    assert!(matches!(
        locked.evaluation().unwrap().package_names("packages"),
        Err(ProjectOperationError::InvalidPackageSet {
            reason: "duplicate catalog key"
        })
    ));
    assert!(matches!(
        locked
            .evaluation()
            .unwrap()
            .package_reference("packages", "same"),
        Err(ProjectOperationError::InvalidPackageSet {
            reason: "duplicate catalog key"
        })
    ));
}

#[test]
fn aliased_catalog_entries_share_one_selected_recipe_in_a_complete_plan() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        fn make() -> std::Package { std::Package { id = "hello"; dependencies = []; } }
        outputs {
            hello: std::RecipeRef<std::Package> = std::recipe_ref(make);
            packages: std::Result<std::PackageSet<std::Package>, std::MapKey> =
                std::package_set_refs([
                    std::MapEntry::Entry("alias", hello),
                    std::MapEntry::Entry("hello", hello),
                ]);
        }
    "#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut complete = locked.evaluation().unwrap();
    assert!(complete.select_all_packages().unwrap());
    let plan = complete.into_selected_plan().unwrap();
    assert_eq!(plan.packages().len(), 1);
    assert_eq!(plan.roots().len(), 1);
    let mut alias = locked.evaluation().unwrap();
    assert!(alias.select_package("hello").unwrap());
    assert_eq!(
        alias
            .into_selected_plan()
            .unwrap()
            .packages()
            .next()
            .unwrap()
            .export(),
        Some("hello")
    );
}

#[test]
fn dependency_can_use_a_second_key_for_an_already_selected_reference() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        fn hello() -> std::Package { std::Package { id = "hello"; dependencies = []; } }
        fn app() -> std::Package {
            std::Package {
                id = "app";
                dependencies = [std::Dependency { package = "hello"; }];
            }
        }
        outputs {
            shared: std::RecipeRef<std::Package> = std::recipe_ref(hello);
            packages: std::Result<std::PackageSet<std::Package>, std::MapKey> =
                std::package_set_refs([
                    std::MapEntry::Entry("alias", shared),
                    std::MapEntry::Entry("app", std::recipe_ref(app)),
                    std::MapEntry::Entry("hello", shared),
                ]);
        }
    "#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut selected = locked.evaluation().unwrap();
    assert!(selected.select_package("app").unwrap());
    let selected_plan = selected.into_selected_plan().unwrap();
    assert_eq!(selected_plan.packages().len(), 2);
    assert!(
        selected_plan
            .packages()
            .any(|package| package.id().as_str() == "hello")
    );
    let plan = plan_project_with(&project.0, &config).unwrap();
    assert_eq!(plan.packages().len(), 2);
    assert_eq!(plan.roots().len(), 2);
}

#[test]
fn complete_package_set_plan_keeps_other_outputs_and_reports_their_failures() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        value I(int); value Text(str);
        fn make() -> std::Package { std::Package { id = "hello"; dependencies = []; } }
        fn broken() -> I { broken() }
        outputs {
            packages: std::Result<std::PackageSet<std::Package>, std::MapKey> =
                std::package_set([std::MapEntry::Entry("hello", make)]);
            label: Text = "published";
            direct: std::Package = std::Package { id = "extra"; dependencies = []; };
        }
    "#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let plan = plan_project_with(&project.0, &config).unwrap();
    assert_eq!(plan.packages().len(), 2);
    assert!(plan.roots().any(|root| root.name() == "label"));
    project.write(
        "main.srx",
        &fs::read_to_string(project.0.join("main.srx"))
            .unwrap()
            .replace("label: Text = \"published\"", "label: I = broken()"),
    );
    lock_project_with(&project.0, &config).unwrap();
    assert!(matches!(plan_project_with(&project.0, &config),
        Err(ProjectOperationError::Check(CheckFailure::Evaluation { failed_roots, .. }))
            if failed_roots == ["label"]));
}

#[test]
fn generated_catalog_selects_a_memoized_reference_without_forcing_other_factories() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        inputs { recipes = "modules:recipes"; }
        outputs {
            packages: std::Result<std::PackageSet<std::Package>, std::MapKey> =
                std::package_set(module_exports(
                    recipes, recipe, std::MapEntry::Entry<fn() -> std::Package>
                ));
        }
        "#,
    );
    project.write(
        "recipes/group/hello.srx",
        "pub fn recipe() -> std::Package { std::Package { id = \"hello\"; dependencies = []; } }",
    );
    project.write(
        "recipes/unused.srx",
        "pub fn recipe() -> std::Package { recipe() }",
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut evaluation = locked.evaluation().unwrap();
    assert!(matches!(
        evaluation.package_reference("packages", "group::hello").unwrap(),
        Some(syrox_lang::Value::Struct { fields, .. })
            if matches!(fields.as_slice(), [(name, syrox_lang::Value::MemoizedFunction { .. })] if name == "factory")
    ));
    assert!(
        evaluation
            .package_reference("packages", "absent")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        evaluation.package_names("packages").unwrap().unwrap(),
        ["group::hello", "unused"]
    );
    assert!(evaluation.select_package("group::hello").unwrap());
    let plan = evaluation.into_selected_plan().unwrap();
    assert_eq!(plan.roots().len(), 1);
    let root = plan.roots().next().unwrap();
    assert_eq!(root.name(), "group::hello");
    assert_eq!(root.path(), ["packages", "group", "hello"]);
    assert_eq!(
        plan.packages().next().unwrap().export(),
        Some("group::hello")
    );
    let mut missing = locked.evaluation().unwrap();
    assert!(matches!(
        missing.select_package("absent"),
        Err(ProjectOperationError::MissingPackageReference { key }) if key == "absent"
    ));
}

#[test]
fn package_set_lookups_and_diamond_references_evaluate_a_factory_once() {
    use std::sync::atomic::AtomicUsize;
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn observe(_: i64) -> bool {
        CALLS.fetch_add(1, Ordering::Relaxed);
        true
    }
    let project = TempProject::new();
    project.write("main.srx", r#"
        value I(int); value Probe(int) where observe(self);
        fn base() -> I { let probe = Probe(1); I(7) }
        fn unused() -> I { unused() }
        fn selected() -> I {
            match packages {
                Ok(set) => match std::package_get(set, "base") { Some(found) => found, None => I(0), },
                Err(_) => I(0),
            }
        }
        fn reference() -> std::RecipeRef<I> {
            match packages {
                Ok(set) => match std::package_ref(set, "base") { Some(found) => found, None => reference(), },
                Err(_) => reference(),
            }
        }
        outputs {
            packages: std::Result<std::PackageSet<I>, std::MapKey> = std::package_set([
                std::MapEntry::Entry("base", base), std::MapEntry::Entry("unused", unused)
            ]);
            left: std::RecipeRef<I> = std::recipe_ref(fn() -> I { std::recipe_value(reference()) });
            right: std::RecipeRef<I> = std::recipe_ref(fn() -> I { std::recipe_value(reference()) });
            development: std::OutputRef<I> = std::recipe_output(reference(), "dev");
            name: std::OutputName = std::output_name(development);
            values: [I] = [selected(), std::recipe_value(left), std::recipe_value(right), std::recipe_value(std::output_recipe(development))];
        }
    "#);
    let policy = CheckPolicy::new("std-recipe-ref-observation")
        .unwrap()
        .with_predicate("observe", syrox_lang::PrimitiveType::Int)
        .unwrap();
    let config = CheckConfiguration {
        environment: EvaluationEnvironment::new(&policy)
            .with_int_predicate("observe", observe)
            .unwrap(),
        policy,
        ..package_configuration()
    };
    let loaded = loader::load_project_linux(&project.0, &config).unwrap();
    let checked = analyze::check_loaded(&loaded, &config).unwrap();
    let mut session = syrox_lang::EvaluationSession::new(
        &checked,
        &config.policy,
        &config.environment,
        config.evaluation_limits,
    )
    .unwrap();
    session.evaluate_root("name").unwrap();
    assert_eq!(CALLS.load(Ordering::Relaxed), 0);
    let syrox_lang::Value::List { items, .. } =
        session.evaluate_root("values").unwrap().value().unwrap()
    else {
        panic!("expected values");
    };
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|value| matches!(
        value,
        syrox_lang::Value::Nominal {
            value: syrox_lang::PrimitiveValue::Int(7),
            ..
        }
    )));
    assert_eq!(CALLS.load(Ordering::Relaxed), 1);
}

#[test]
fn deferred_standard_factory_uses_the_reference_creation_project_for_assets() {
    let workspace = TempProject::new();
    let child = workspace.0.join("child");
    let consumer = workspace.0.join("consumer");
    workspace.write("child/assets/source", "child");
    workspace.write("consumer/assets/source", "consumer");
    workspace.write(
        "child/main.srx",
        r#"
        outputs {
            source: std::RecipeRef<std::Acquisition> = std::recipe_ref(std::preset_sources);
            selected: std::OutputRef<std::Acquisition> = std::recipe_output(source, "out");
        }
    "#,
    );
    workspace.write(
        "consumer/main.srx",
        r#"
        inputs { child = "path:../child"; }
        outputs {
            package: std::Package = std::Package { id = "app"; dependencies = []; };
            request: std::Acquisition = std::recipe_value(std::output_recipe(child::selected));
        }
    "#,
    );
    let mut sources = AuthenticatedStandardLibrary::bundled()
        .sources()
        .cloned()
        .collect::<Vec<_>>();
    sources.push(AuthenticatedStandardSource::from_authenticated("std/preset.srx", r#"
        mod std {
            pub fn preset_sources() -> Acquisition {
                Acquisition { package = "app"; sources = [source_request(
                    "project:assets/source", "ddc9e669194254cef019a29d3619a2c16592e5d52e1a81e98b01bd52319149a3", 100
                )]; }
            }
        }
    "#).unwrap());
    let config = CheckConfiguration {
        standard_library: Some(AuthenticatedStandardLibrary::from_authenticated(sources).unwrap()),
        ..CheckConfiguration::default()
    };
    lock_project_with(&child, &config).unwrap();
    lock_project_with(&consumer, &config).unwrap();
    let plan = plan_project_with(&consumer, &config).unwrap();
    let request = plan
        .acquisitions()
        .next()
        .unwrap()
        .sources()
        .next()
        .unwrap();
    assert_eq!(
        fs::canonicalize(plan.project_root(request.owner().unwrap()).unwrap()).unwrap(),
        fs::canonicalize(child).unwrap()
    );
}

#[test]
fn recipe_references_are_opaque_and_cannot_be_serialized_as_persistent_plan_values() {
    let project = TempProject::new();
    let config = package_configuration();
    project.write("main.srx", "value I(int); fn make() -> I { I(1) } outputs { instance: std::RecipeRef<I> = std::RecipeRef<I> { factory = make; }; }");
    assert!(
        matches!(lock_project_with(&project.0, &config), Err(ProjectOperationError::Check(CheckFailure::Diagnostics { errors, .. })) if errors.iter().any(|error| error.message.contains("opaque")))
    );
    project.write("main.srx", "value I(int); fn make() -> I { I(1) } outputs { instance: std::RecipeRef<I> = std::recipe_ref(make); }");
    lock_project_with(&project.0, &config).unwrap();
    assert!(matches!(
        plan_project_with(&project.0, &config),
        Err(ProjectOperationError::Plan(crate::PlanError::FunctionValue))
    ));
}
