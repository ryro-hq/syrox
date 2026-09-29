use super::*;

const PROVIDER: &str = r#"
        libc: std::Recipe<std::GlibcBuild> = std::Recipe<std::GlibcBuild> {
            package = std::Package { id = "libc"; dependencies = []; };
            acquisition = std::Acquisition { package = "libc"; sources = [std::source_request(
                "https://example.test/libc.tar.gz", "0000000000000000000000000000000000000000000000000000000000000000", 100
            )]; };
            build = std::GlibcBuild { package = "libc"; source_directory = "libc-1";
                entry = "usr/lib/ld-linux-x86-64.so.2"; timeout_seconds = 120; };
        };
"#;

#[test]
fn mixed_build_catalog_selects_each_backend_from_one_recipe_type() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        inputs { recipes = "modules:recipes"; }
        type Entry = std::Recipe<std::Build>;
        outputs {
            default_build: std::DefaultBuild = std::DefaultBuild { package = "hello"; };
            packages: std::Result<std::PackageSet<Entry>, std::MapKey> = std::package_set(
                module_exports(recipes, recipe, std::MapEntry::Entry<fn() -> Entry>)
            );
        }
    "#,
    );
    project.write("recipes/hello.srx", r#"
        pub fn recipe() -> std::Recipe<std::Build> {
            std::Recipe<std::Build> {
                package = std::Package { id = "hello"; dependencies = []; };
                acquisition = std::Acquisition { package = "hello"; sources = [std::source_request(
                    "https://example.test/hello", "0000000000000000000000000000000000000000000000000000000000000000", 100
                )]; };
                build = std::Build::Autotools(std::AutotoolsBuild {
                    package = "hello"; source_directory = "hello-1";
                    entry = "usr/bin/hello"; timeout_seconds = 30;
                });
            }
        }
    "#);
    project.write("recipes/libc.srx", r#"
        pub fn recipe() -> std::Recipe<std::Build> {
            std::Recipe<std::Build> {
                package = std::Package { id = "libc"; dependencies = []; };
                acquisition = std::Acquisition { package = "libc"; sources = [std::source_request(
                    "https://example.test/libc", "0000000000000000000000000000000000000000000000000000000000000000", 100
                )]; };
                build = std::Build::Glibc(std::GlibcBuild {
                    package = "libc"; source_directory = "libc-1";
                    entry = "usr/lib/ld-linux-x86-64.so.2"; timeout_seconds = 120;
                });
            }
        }
    "#);
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let catalog_plan = plan_project_with(&project.0, &config).unwrap();
    assert_eq!(catalog_plan.roots().len(), 3);
    assert_eq!(catalog_plan.packages().len(), 2);
    assert_eq!(catalog_plan.default_build().unwrap().as_str(), "hello");
    let user = crate::UserConfiguration::parse("", project.0.join("store")).unwrap();
    let default = crate::resolve_build(project.0.to_str().unwrap(), &user, &config).unwrap();
    assert_eq!(default.export(), "hello");
    let builds = crate::resolve_builds(project.0.to_str().unwrap(), &user, &config).unwrap();
    assert_eq!(
        builds
            .iter()
            .map(crate::ResolvedBuild::export)
            .collect::<Vec<_>>(),
        ["hello", "libc"]
    );
    assert!(!project.0.join("store").exists());
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    for (key, protocol) in [("hello", "autotools"), ("libc", "glibc")] {
        let mut evaluation = locked.evaluation().unwrap();
        assert!(evaluation.select_package(key).unwrap());
        assert!(evaluation.select_package(key).unwrap());
        let plan = evaluation.into_selected_plan().unwrap();
        assert_eq!(plan.packages().len(), 1);
        assert_eq!(plan.packages().next().unwrap().export(), Some(key));
        assert_eq!(plan.builds().next().unwrap().protocol(), protocol);
    }
    project.write(
        "recipes/unused.srx",
        "pub fn recipe() -> std::Recipe<std::Build> { recipe() }",
    );
    lock_project_with(&project.0, &config).unwrap();
    assert!(plan_project_with(&project.0, &config).is_err());
    assert_eq!(
        crate::resolve_build(project.0.to_str().unwrap(), &user, &config)
            .unwrap()
            .export(),
        "hello"
    );
    let source_plan = plan_project_package_with(&project.0, &config, "hello").unwrap();
    assert_eq!(source_plan.acquisitions().len(), 1);
    assert_eq!(source_plan.packages().len(), 1);
}

#[test]
fn complete_set_plan_retains_declared_default_application() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        fn recipe() -> std::CatalogRecipe {
            std::CatalogRecipe {
                package = std::Package { id = "app"; dependencies = []; };
                acquisition = std::Acquisition { package = "app"; sources = [
                    std::source_request("https://example.test/app.tar.gz",
                        "0000000000000000000000000000000000000000000000000000000000000000", 100)
                ]; };
                build = std::Build::Autotools(std::AutotoolsBuild {
                    package = "app"; source_directory = "app-1";
                    entry = "usr/bin/app"; timeout_seconds = 30;
                });
                build_inputs = std::Option::None;
                application = std::Option::Some(std::Application {
                    package = "app"; loader = []; libraries = [];
                });
            }
        }
        outputs {
            default_application: std::DefaultApplication =
                std::DefaultApplication { package = "app"; };
            packages: std::Result<std::PackageSet<std::CatalogRecipe>, std::MapKey> =
                std::package_set([std::MapEntry::Entry("app", recipe)]);
        }
    "#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let plan = plan_project_with(&project.0, &config).unwrap();
    assert_eq!(plan.default_application().unwrap().as_str(), "app");
    assert_eq!(plan.applications().len(), 1);
    let user = crate::UserConfiguration::parse("", project.0.join("store")).unwrap();
    project.write(
        "main.srx",
        &fs::read_to_string(project.0.join("main.srx"))
            .unwrap()
            .replace(
                "std::MapEntry::Entry(\"app\", recipe)",
                "std::MapEntry::Entry(\"app\", recipe), std::MapEntry::Entry(\"unused\", broken)",
            )
            .replace(
                "fn recipe()",
                "fn broken() -> std::CatalogRecipe { broken() } fn recipe()",
            ),
    );
    lock_project_with(&project.0, &config).unwrap();
    assert!(plan_project_with(&project.0, &config).is_err());
    assert_eq!(
        crate::resolve_application(project.0.to_str().unwrap(), &user, &config)
            .unwrap()
            .description()
            .package()
            .as_str(),
        "app"
    );
}

fn recipe_source(body: &str) -> String {
    format!(
        r#"
        pub fn recipe() -> std::ApplicationRecipe<std::AutotoolsBuild> {{
            {body}
            std::ApplicationRecipe<std::AutotoolsBuild> {{
                package = std::Package {{ id = "hello"; dependencies = []; }};
                acquisition = std::Acquisition {{ package = "hello"; sources = [
                    std::source_request("project:assets/source",
                        "ddc9e669194254cef019a29d3619a2c16592e5d52e1a81e98b01bd52319149a3", 100)
                ]; }};
                build = std::AutotoolsBuild {{ package = "hello"; source_directory = "hello-1";
                    entry = "usr/bin/hello"; timeout_seconds = 120; }};
                build_inputs = std::BuildInputs {{ package = "hello"; selected = [
                    std::BuildOutput {{ package = "libc"; output = "dev"; }}
                ]; }};
                application = std::Application {{ package = "hello"; loader = [
                    std::RuntimeLoader {{ package = "libc"; }}
                ]; libraries = [std::RuntimeLibrary {{ package = "libc"; }}]; }};
            }}
        }}
    "#
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn direct_application_selection_expands_only_its_declared_glibc_provider() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        inputs { recipes = "modules:recipes"; }
        type Entry = std::CatalogRecipe;
        outputs {
            packages: std::Result<std::PackageSet<Entry>, std::MapKey> = std::package_set(
                module_exports(recipes, recipe, std::MapEntry::Entry<fn() -> Entry>)
            );
        }
    "#,
    );
    project.write(
        "recipes/hello.srx",
        r#"
        pub fn recipe() -> std::CatalogRecipe {
            std::CatalogRecipe {
                package = std::Package { id = "hello"; dependencies = []; };
                acquisition = std::Acquisition { package = "hello"; sources = [std::source_request(
                    "https://example.test/hello.tar.gz",
                    "0000000000000000000000000000000000000000000000000000000000000000", 100
                )]; };
                build = std::Build::Autotools(std::AutotoolsBuild { package = "hello"; source_directory = "hello-1";
                    entry = "usr/bin/hello"; timeout_seconds = 120; });
                build_inputs = std::Option::Some(std::BuildInputs { package = "hello"; selected = [
                    std::BuildOutput { package = "libc"; output = "dev"; }
                ]; });
                application = std::Option::Some(std::Application { package = "hello";
                    loader = [std::RuntimeLoader { package = "libc"; }];
                    libraries = [std::RuntimeLibrary { package = "libc"; }]; });
            }
        }
    "#,
    );
    project.write(
        "recipes/unused.srx",
        "pub fn recipe() -> std::CatalogRecipe { recipe() }",
    );
    project.write(
        "recipes/libc.srx",
        r#"
        pub fn recipe() -> std::CatalogRecipe {
            std::CatalogRecipe {
                package = std::Package { id = "libc"; dependencies = []; };
                acquisition = std::Acquisition { package = "libc"; sources = [std::source_request(
                    "https://example.test/libc.tar.gz",
                    "0000000000000000000000000000000000000000000000000000000000000000", 100
                )]; };
                build = std::Build::Glibc(std::GlibcBuild { package = "libc";
                    source_directory = "libc-1"; entry = "usr/lib/ld-linux-x86-64.so.2";
                    timeout_seconds = 120; });
                build_inputs = std::Option::None;
                application = std::Option::None;
            }
        }
    "#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut evaluation = locked.evaluation().unwrap();
    assert!(evaluation.select_package("hello").unwrap());
    let plan = evaluation.into_selected_plan().unwrap();
    assert_eq!(plan.roots().len(), 2);
    assert_eq!(plan.packages().len(), 2);
    assert_eq!(plan.builds().len(), 2);
    assert_eq!(plan.applications().len(), 1);
    assert_eq!(
        plan.builds()
            .find(|build| build.package().as_str() == "hello")
            .unwrap()
            .development()
            .unwrap()
            .as_str(),
        "libc"
    );
    assert_eq!(
        plan.applications()
            .next()
            .unwrap()
            .loader()
            .unwrap()
            .as_str(),
        "libc"
    );
    assert!(plan.roots().all(|root| root.name() != "unused"));
    let user = crate::UserConfiguration::parse("", project.0.join("store")).unwrap();
    let reference = format!("{}#hello", project.0.display());
    let build = crate::resolve_build(&reference, &user, &config).unwrap();
    assert_eq!(build.description().development().unwrap().as_str(), "libc");
    let application = crate::resolve_application(&reference, &user, &config).unwrap();
    assert_eq!(application.description().loader().unwrap().as_str(), "libc");
    assert!(!project.0.join("store").exists());
    let provider = fs::read_to_string(project.0.join("recipes/libc.srx")).unwrap();
    project.write(
        "recipes/libc.srx",
        &provider.replace("id = \"libc\"", "id = \"different\""),
    );
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut evaluation = locked.evaluation().unwrap();
    assert!(evaluation.select_package("hello").unwrap());
    assert!(matches!(
        evaluation.into_selected_plan(),
        Err(ProjectOperationError::InvalidPackageSet {
            reason: "dependency key does not match package id"
        })
    ));
    project.write(
        "recipes/libc.srx",
        "pub fn recipe() -> std::CatalogRecipe { recipe() }",
    );
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut evaluation = locked.evaluation().unwrap();
    assert!(evaluation.select_package("hello").unwrap());
    assert!(matches!(
        evaluation.into_selected_plan(),
        Err(ProjectOperationError::Check(CheckFailure::Evaluation { failed_roots, .. }))
            if failed_roots == ["libc"]
    ));
}

#[test]
fn imported_package_set_selects_one_composite_recipe_and_preserves_asset_origin() {
    let workspace = TempProject::new();
    let child = workspace.0.join("child");
    let consumer = workspace.0.join("consumer");
    workspace.write(
        "child/main.srx",
        r#"
        inputs { recipes = "modules:recipes"; }
        pub type Entry = std::ApplicationRecipe<std::AutotoolsBuild>;
        outputs {
            packages: std::Result<std::PackageSet<Entry>, std::MapKey> = std::package_set(
                module_exports(recipes, recipe, std::MapEntry::Entry<fn() -> Entry>)
            );
        }
    "#,
    );
    workspace.write("child/recipes/hello.srx", &recipe_source(""));
    workspace.write(
        "child/recipes/unused.srx",
        &recipe_source("let cycle = recipe();"),
    );
    workspace.write("child/assets/source", "child");
    workspace.write("consumer/assets/source", "consumer");
    let main = r#"
        inputs { pkgs = "path:../child"; }
        fn selected() -> pkgs::Entry {
            match pkgs::packages {
                Ok(packages) => match std::package_get(packages, "hello") {
                    Some(recipe) => recipe, None => selected(),
                },
                Err(_) => selected(),
            }
        }
        outputs {
            names: [std::MapKey] = match pkgs::packages {
                Ok(packages) => std::package_names(packages), Err(_) => [],
            };
            hello: pkgs::Entry = selected();
        }
    "#;
    workspace.write(
        "consumer/main.srx",
        &main.replacen("outputs {", &format!("outputs {{ {PROVIDER}"), 1),
    );
    let config = package_configuration();
    lock_project_with(&child, &config).unwrap();
    lock_project_with(&consumer, &config).unwrap();
    let plan = plan_project_with(&consumer, &config).unwrap();
    assert_eq!(plan.packages().len(), 2);
    assert!(
        plan.packages()
            .any(|package| package.export() == Some("hello"))
    );
    assert_eq!(plan.builds().len(), 2);
    assert_eq!(plan.applications().len(), 1);
    assert_eq!(
        plan.builds()
            .find(|build| build.package().as_str() == "hello")
            .unwrap()
            .development()
            .unwrap()
            .as_str(),
        "libc"
    );
    assert_eq!(
        plan.applications()
            .next()
            .unwrap()
            .loader()
            .unwrap()
            .as_str(),
        "libc"
    );
    assert!(plan.default_build().is_none());
    assert!(plan.default_application().is_none());
    let names = plan.roots().find(|root| root.name() == "names").unwrap();
    let crate::PlanValue::List { items, .. } = names.value() else {
        panic!("expected names");
    };
    assert_eq!(items.len(), 2);
    assert!(
        matches!(&items[0], crate::PlanValue::Nominal { value, .. } if **value == crate::PlanValue::Str("hello".into()))
    );
    let source = plan
        .acquisitions()
        .find(|acquisition| acquisition.package().as_str() == "hello")
        .unwrap()
        .sources()
        .next()
        .unwrap();
    assert_eq!(
        fs::canonicalize(plan.project_root(source.owner().unwrap()).unwrap()).unwrap(),
        fs::canonicalize(&child).unwrap()
    );
}

#[test]
fn package_set_reports_duplicate_and_missing_keys_without_running_factories() {
    let plan = package_plan(
        r#"
        value Flag(int);
        fn dormant() -> std::Package { dormant() }
        fn empty() -> std::PackageSet<std::Package> {
            match std::package_set<std::Package>([]) { Ok(set) => set, Err(_) => empty(), }
        }
        fn names_for_reuse() -> [std::MapKey] {
            let set = empty();
            let absent = std::package_factory(set, "absent");
            std::package_names(set)
        }
        outputs {
            duplicate: std::MapKey = match std::package_set([
                std::MapEntry::Entry("same", dormant), std::MapEntry::Entry("same", dormant)
            ]) { Ok(_) => "wrong", Err(key) => key, };
            missing: Flag = match std::package_get(empty(), "absent") { Some(_) => 0, None => 1, };
            reusable: [std::MapKey] = names_for_reuse();
        }
    "#,
    )
    .unwrap();
    let duplicate = plan
        .roots()
        .find(|root| root.name() == "duplicate")
        .unwrap();
    assert!(
        matches!(duplicate.value(), crate::PlanValue::Nominal { value, .. } if **value == crate::PlanValue::Str("same".into()))
    );
    let missing = plan.roots().find(|root| root.name() == "missing").unwrap();
    assert!(
        matches!(missing.value(), crate::PlanValue::Nominal { value, .. } if **value == crate::PlanValue::Int(1))
    );
}

#[test]
fn composite_recipe_is_evaluated_once_before_projecting_all_components() {
    use std::sync::atomic::AtomicUsize;
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn observed(_: i64) -> bool {
        CALLS.fetch_add(1, Ordering::Relaxed);
        true
    }
    let policy = CheckPolicy::new("recipe-observation")
        .unwrap()
        .with_predicate("host::observed", syrox_lang::PrimitiveType::Int)
        .unwrap();
    let config = CheckConfiguration {
        environment: EvaluationEnvironment::new(&policy)
            .with_int_predicate("host::observed", observed)
            .unwrap(),
        policy,
        ..package_configuration()
    };
    let project = TempProject::new();
    let factory = format!(
        "value Observed(int) where host::observed(self); {}",
        recipe_source("let observation = Observed(1);")
    );
    project.write("main.srx", &format!("{factory} outputs {{ hello: std::ApplicationRecipe<std::AutotoolsBuild> = recipe(); {PROVIDER} }}"));
    project.write("assets/source", "child");
    CALLS.store(0, Ordering::Relaxed);
    let validated = validate_project_with(&project.0, &config).unwrap();
    let plan = crate::Plan::from_realized(
        validated.realized(),
        [0; 32],
        config.policy.identity(),
        config.standard_library.as_ref(),
    )
    .unwrap();
    assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    assert_eq!(plan.packages().len(), 2);
    assert_eq!(plan.acquisitions().len(), 2);
    assert_eq!(plan.builds().len(), 2);
    assert_eq!(plan.applications().len(), 1);
}

#[test]
fn composite_recipe_rejects_components_with_different_package_owners() {
    for component in ["acquisition", "build", "build_inputs", "application"] {
        let project = TempProject::new();
        let source = recipe_source("");
        let marker = match component {
            "acquisition" => "acquisition = std::Acquisition",
            "build" => "build = std::AutotoolsBuild",
            "build_inputs" => "build_inputs = std::BuildInputs",
            _ => "application = std::Application",
        };
        let (before, after) = source.split_once(marker).unwrap();
        project.write("main.srx", &format!("{before}{marker}{} outputs {{ hello: std::ApplicationRecipe<std::AutotoolsBuild> = recipe(); }}", after.replacen("package = \"hello\"", "package = \"other\"", 1)));
        project.write("assets/source", "child");
        let config = package_configuration();
        lock_project_with(&project.0, &config).unwrap();
        assert!(
            matches!(plan_project_with(&project.0, &config), Err(ProjectOperationError::Plan(crate::PlanError::InvalidRecipe { reason, .. })) if reason.contains("same package")),
            "{component}"
        );
    }
}
