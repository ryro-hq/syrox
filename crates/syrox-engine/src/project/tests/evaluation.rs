use super::*;

#[test]
fn lock_and_enumeration_do_not_evaluate_roots_and_selected_plan_omits_disconnected_failures() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        fn broken() -> std::Package { broken() }
        outputs {
            hello: std::Package = std::Package { id = "hello"; dependencies = []; };
            disconnected: std::Package = broken();
        }
    "#,
    );
    let config = package_configuration();
    let report = lock_project_with(&project.0, &config).unwrap();
    assert_eq!(report.check.realized_roots, 0);
    assert_eq!(
        check_project_lock_with(&project.0, &config)
            .unwrap()
            .check
            .realized_roots,
        0
    );
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    let mut session = locked.evaluation().unwrap();
    assert_eq!(
        session.root_names().collect::<Vec<_>>(),
        ["disconnected", "hello"]
    );
    assert!(session.root_type("hello").is_some());
    assert!(
        matches!(session.evaluate_root("missing"), Err(ProjectOperationError::MissingOutput { name }) if name == "missing")
    );
    session.evaluate_root("hello").unwrap();
    session.evaluate_root("hello").unwrap();
    let plan = session.into_plan().unwrap();
    assert_eq!(plan.roots().len(), 1);
    assert_eq!(plan.packages().next().unwrap().export(), Some("hello"));
    assert!(matches!(
        plan_project_with(&project.0, &config),
        Err(ProjectOperationError::Check(
            CheckFailure::Evaluation { .. }
        ))
    ));
    assert!(
        matches!(plan_project_outputs_with(&project.0, &config, &["disconnected"]), Err(ProjectOperationError::Check(CheckFailure::Evaluation { failed_roots, .. })) if failed_roots == ["disconnected"])
    );
    assert_eq!(
        plan.to_string(),
        plan_project_outputs_with(&project.0, &config, &["hello", "hello"])
            .unwrap()
            .to_string()
    );
}

#[test]
fn locked_query_retains_its_source_snapshot_and_reopening_detects_drift_before_evaluation() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"outputs { app: std::Package = std::Package { id = "original"; dependencies = []; }; }"#,
    );
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let locked = open_locked_project_with(&project.0, &config).unwrap();
    project.write(
        "main.srx",
        "fn diverge() -> std::Package { diverge() } outputs { app: std::Package = diverge(); }",
    );
    let mut session = locked.evaluation().unwrap();
    session.evaluate_root("app").unwrap();
    let plan = session.into_plan().unwrap();
    assert_eq!(plan.packages().next().unwrap().id().as_str(), "original");
    assert_eq!(plan.lock_digest(), locked.lock_digest());
    assert!(matches!(
        open_locked_project_with(&project.0, &config),
        Err(ProjectOperationError::ProjectDrift)
    ));
    assert!(matches!(
        plan_project_with(&project.0, &config),
        Err(ProjectOperationError::ProjectDrift)
    ));
}

#[test]
fn selected_composite_recipe_keeps_child_asset_authority_and_verifies_unselected_sources() {
    let workspace = TempProject::new();
    workspace.write("child/main.srx", r#"
        pub fn recipe() -> std::Recipe<std::AutotoolsBuild> {
            std::Recipe<std::AutotoolsBuild> {
                package = std::Package { id = "hello"; dependencies = []; };
                acquisition = std::Acquisition { package = "hello"; sources = [std::source_request(
                    "project:assets/source", "ddc9e669194254cef019a29d3619a2c16592e5d52e1a81e98b01bd52319149a3", 100
                )]; };
                build = std::AutotoolsBuild { package = "hello"; source_directory = "hello-1";
                    entry = "usr/bin/hello"; timeout_seconds = 120; };
            }
        }
    "#);
    workspace.write("child/assets/source", "child");
    workspace.write("child/assets/unused", "unused");
    workspace.write("consumer/assets/source", "consumer");
    workspace.write("consumer/main.srx", r#"
        inputs { child = "path:../child"; }
        fn diverge() -> std::Package { diverge() }
        outputs { hello: std::Recipe<std::AutotoolsBuild> = child::recipe(); broken: std::Package = diverge(); }
    "#);
    let config = package_configuration();
    let child = workspace.0.join("child");
    let consumer = workspace.0.join("consumer");
    lock_project_with(&child, &config).unwrap();
    lock_project_with(&consumer, &config).unwrap();
    let plan = plan_project_outputs_with(&consumer, &config, &["hello"]).unwrap();
    assert_eq!(plan.builds().len(), 1);
    let source = plan
        .acquisitions()
        .next()
        .unwrap()
        .sources()
        .next()
        .unwrap();
    assert_eq!(
        fs::canonicalize(plan.project_root(source.owner().unwrap()).unwrap()).unwrap(),
        fs::canonicalize(&child).unwrap()
    );
    workspace.write("child/assets/unused", "changed");
    assert!(matches!(
        plan_project_outputs_with(&consumer, &config, &["hello"]),
        Err(ProjectOperationError::Check(
            CheckFailure::InvalidChildLock { .. }
        ))
    ));
    lock_project_with(&child, &config).unwrap();
    assert!(matches!(
        plan_project_outputs_with(&consumer, &config, &["hello"]),
        Err(ProjectOperationError::GraphDrift)
    ));
}

#[test]
fn locking_still_typechecks_every_factory_before_publishing() {
    let project = TempProject::new();
    project.write("main.srx", "value I(int); outputs { good: I = 1; }");
    let config = package_configuration();
    lock_project_with(&project.0, &config).unwrap();
    let previous = fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    project.write(
        "main.srx",
        r#"value I(int); value S(str); fn broken() -> I { S("wrong") } outputs { good: I = 1; }"#,
    );
    assert!(matches!(
        lock_project_with(&project.0, &config),
        Err(ProjectOperationError::Check(
            CheckFailure::Diagnostics { .. }
        ))
    ));
    assert_eq!(
        fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap(),
        previous
    );
}
