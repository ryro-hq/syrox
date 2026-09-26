use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Cursor};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use syrox_lang::{
    CanonicalItemIdentity, CheckPolicy, EvaluationEnvironment, EvaluationSetupError,
    MAX_SOURCE_BYTES, MAX_SOURCES, SourceDomainId,
};

use super::loader::{
    InputRoot, LoadBudget, identity, open_beneath, open_error, open_top, read_source,
    reopen_directory, walk_input,
};
use super::*;
use crate::linux_fd::OpenError;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

const PACKAGE_STANDARD_LIBRARY: &str = include_str!("../../../../std/pkg.srx");

struct TempProject(PathBuf);

impl TempProject {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("syrox-project-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn package_configuration() -> CheckConfiguration {
    let source =
        AuthenticatedStandardSource::from_authenticated("std/pkg.srx", PACKAGE_STANDARD_LIBRARY)
            .unwrap();
    CheckConfiguration {
        standard_library: Some(
            AuthenticatedStandardLibrary::from_authenticated(vec![source]).unwrap(),
        ),
        ..CheckConfiguration::default()
    }
}

fn package_plan(source: &str) -> Result<crate::Plan, ProjectOperationError> {
    let project = TempProject::new();
    project.write("main.srx", source);
    let configuration = package_configuration();
    lock_project_with(&project.0, &configuration)?;
    plan_project_with(&project.0, &configuration)
}

#[test]
fn development_output_is_an_explicit_validated_build_edge() {
    let standard = AuthenticatedStandardSource::from_authenticated(
        "std/pkg.srx",
        include_str!("../../../../std/pkg.srx"),
    )
    .unwrap();
    let config = CheckConfiguration {
        standard_library: Some(
            AuthenticatedStandardLibrary::from_authenticated(vec![standard]).unwrap(),
        ),
        ..CheckConfiguration::default()
    };
    let recipe = r#"outputs {
        glibc: std::Package = std::Package { id = "glibc"; dependencies = []; };
        glibc_source: std::Acquisition = std::Acquisition { package = "glibc"; sources = [std::SourceRequest { url = "project:glibc.tar.xz"; sha256 = "0000000000000000000000000000000000000000000000000000000000000000"; maximum_bytes = 33554432; }]; };
        glibc_build: std::GlibcBuild = std::GlibcBuild { package = "glibc"; source_directory = "glibc-2.44"; entry = "usr/lib/ld-linux-x86-64.so.2"; timeout_seconds = 1800; };
        hello: std::Package = std::Package { id = "hello"; dependencies = []; };
        hello_source: std::Acquisition = std::Acquisition { package = "hello"; sources = [std::SourceRequest { url = "project:hello.tar.gz"; sha256 = "0000000000000000000000000000000000000000000000000000000000000000"; maximum_bytes = 2097152; }]; };
        hello_build: std::AutotoolsBuild = std::AutotoolsBuild { package = "hello"; source_directory = "hello-2.12.1"; entry = "usr/bin/hello"; timeout_seconds = 120; };
        hello_app: std::Application = std::Application { package = "hello"; loader = [std::RuntimeLoader { package = "glibc"; }]; libraries = [std::RuntimeLibrary { package = "glibc"; }]; };
        hello_inputs: std::BuildInputs = std::BuildInputs { package = "hello"; selected = [std::BuildOutput { package = "glibc"; output = "dev"; }]; };
    }"#;
    let plan = |source: &str| {
        let project = TempProject::new();
        project.write("main.srx", source);
        lock_project_with(&project.0, &config).unwrap();
        plan_project_with(&project.0, &config)
    };
    let valid = plan(recipe).unwrap();
    let store_directory = tempfile::tempdir().unwrap();
    fs::create_dir(store_directory.path().join("store")).unwrap();
    let store = crate::Store::open(&store_directory.path().join("store")).unwrap();
    let root = crate::RootName::new("test_build_request").unwrap();
    let glibc = valid
        .builds()
        .find(|build| build.package().as_str() == "glibc")
        .unwrap();
    let mut changed = glibc.request();
    changed.source_directory = "other-directory".into();
    assert!(matches!(
        crate::build_autotools(
            &valid,
            &changed,
            &store,
            &root,
            std::path::Path::new("/usr"),
            std::path::Path::new("/usr/bin/false")
        ),
        Err(crate::BuildError::UnsupportedInputs)
    ));
    let hello = valid
        .builds()
        .find(|build| build.package().as_str() == "hello")
        .unwrap();
    assert!(matches!(
        crate::build_autotools(
            &valid,
            &hello.request(),
            &store,
            &root,
            std::path::Path::new("/usr"),
            std::path::Path::new("/usr/bin/false")
        ),
        Err(crate::BuildError::UnsupportedInputs)
    ));
    assert_eq!(
        valid
            .builds()
            .find(|build| build.package().as_str() == "hello")
            .unwrap()
            .development()
            .unwrap()
            .as_str(),
        "glibc"
    );
    assert!(plan(&recipe.replace("output = \"dev\"", "output = \"out\"")).is_err());
    assert!(
        plan(&recipe.replace("package = \"glibc\"; output", "package = \"hello\"; output"))
            .is_err()
    );
}

#[test]
fn source_is_rejected_after_reading_one_byte_beyond_the_limit() {
    let input = vec![b'x'; MAX_SOURCE_BYTES + 1];
    let error = read_source(Cursor::new(input), Path::new("large.srx")).unwrap_err();
    assert!(matches!(error, CheckFailure::TooLarge { .. }));
}

#[test]
fn project_merges_input_files_in_deterministic_relative_order() {
    let project = TempProject::new();
    project.write("main.srx", "inputs { dep = \"path:dep\"; }");
    project.write("dep/z.srx", "mod shared { struct Item { name: Name; } }");
    project.write("dep/a.srx", "mod shared { value Name(str); }");

    let report = check_project(&project.0).unwrap();
    assert_eq!(report.declarations, 3);

    project.write("dep/z.srx", "mod shared { value Name(str); }");
    let CheckFailure::Diagnostics { input, errors } = check_project(&project.0).unwrap_err() else {
        panic!("expected duplicate declaration diagnostic");
    };
    let duplicate = errors
        .iter()
        .find(|error| error.message == "duplicate item declaration in module")
        .unwrap();
    assert_eq!(
        input.get(duplicate.span.source_id()).unwrap().name(),
        "dep/z.srx"
    );
}

#[test]
fn syntax_errors_do_not_cross_file_boundaries() {
    let project = TempProject::new();
    project.write("main.srx", "inputs { dep = \"path:dep\"; }");
    project.write("dep/a.srx", "mod shared {");
    project.write("dep/b.srx", "}");

    let CheckFailure::Diagnostics { errors, .. } = check_project(&project.0).unwrap_err() else {
        panic!("expected syntax diagnostics");
    };
    assert!(errors.len() >= 2);
}

#[test]
fn project_rejects_unsafe_and_unsupported_locators() {
    for (locator, expected) in [
        ("path:../outside", "relative"),
        ("https:example.invalid", "unsupported locator"),
    ] {
        let project = TempProject::new();
        project.write("main.srx", &format!("inputs {{ dep = \"{locator}\"; }}"));
        assert!(
            check_project(&project.0)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
}

#[test]
fn locator_overlap_is_rejected_before_input_roots_are_opened() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "inputs { missing = \"path:missing\"; nested = \"path:missing/nested\"; }",
    );

    assert!(matches!(
        check_project(&project.0).unwrap_err(),
        CheckFailure::AliasedInputPaths { .. }
    ));
}

#[cfg(unix)]
#[test]
fn project_rejects_symlinked_inputs_and_files() {
    use std::os::unix::fs::symlink;

    let project = TempProject::new();
    project.write("main.srx", "inputs { dep = \"path:dep\"; }");
    project.write("real/source.srx", "value V(str);");
    symlink(project.0.join("real"), project.0.join("dep")).unwrap();
    let error = check_project(&project.0).unwrap_err();
    assert!(
        matches!(error, CheckFailure::SymbolicLink { .. }),
        "{error:?}"
    );

    fs::remove_file(project.0.join("dep")).unwrap();
    fs::create_dir(project.0.join("dep")).unwrap();
    symlink(
        project.0.join("real/source.srx"),
        project.0.join("dep/source.srx"),
    )
    .unwrap();
    assert!(matches!(
        check_project(&project.0).unwrap_err(),
        CheckFailure::SymbolicLink { .. }
    ));

    fs::remove_file(project.0.join("dep/source.srx")).unwrap();
    fs::remove_file(project.0.join("main.srx")).unwrap();
    symlink(
        project.0.join("real/source.srx"),
        project.0.join("main.srx"),
    )
    .unwrap();
    assert!(matches!(
        check_project(&project.0).unwrap_err(),
        CheckFailure::SymbolicLink { .. }
    ));
}

#[test]
fn traversal_keeps_using_the_opened_directory_after_a_path_swap() {
    use std::os::unix::fs::symlink;

    let project = TempProject::new();
    project.write("main.srx", "value Main(str);");
    project.write("dep/source.srx", "value Original(str);");
    project.write("replacement/source.srx", "value Replacement(str);");

    let mut budget = LoadBudget::new(ProjectLimits::default());
    let project_handle = open_top(&project.0, &mut budget).unwrap();
    let dep_path = project.0.join("dep");
    let dep = open_beneath(
        project_handle.fd(),
        Path::new("dep"),
        &dep_path,
        true,
        &mut budget,
    )
    .unwrap();
    let root = InputRoot {
        name: "dep".to_owned(),
        relative: PathBuf::from("dep"),
        opened: dep,
    };
    let mut directories = HashMap::new();
    directories.insert(identity(&project_handle), project.0.clone());
    directories.insert(identity(&root.opened), dep_path.clone());

    fs::rename(&dep_path, project.0.join("original-dep")).unwrap();
    symlink(project.0.join("replacement"), &dep_path).unwrap();
    let files = walk_input(
        &project.0,
        root,
        &mut directories,
        &mut HashMap::new(),
        &mut budget,
    )
    .unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].1.contains("Original"));

    fs::remove_file(&dep_path).unwrap();
    fs::rename(project.0.join("original-dep"), dep_path).unwrap();
}

#[test]
fn reopened_descendant_must_match_its_discovered_identity() {
    let project = TempProject::new();
    fs::create_dir_all(project.0.join("dep/child")).unwrap();
    fs::create_dir_all(project.0.join("replacement")).unwrap();

    let mut budget = LoadBudget::new(ProjectLimits::default());
    let project_handle = open_top(&project.0, &mut budget).unwrap();
    let dep_path = project.0.join("dep");
    let dep = open_beneath(
        project_handle.fd(),
        Path::new("dep"),
        &dep_path,
        true,
        &mut budget,
    )
    .unwrap();
    let child = open_beneath(
        dep.fd(),
        Path::new("child"),
        &dep_path.join("child"),
        true,
        &mut budget,
    )
    .unwrap();
    let expected = identity(&child);
    drop(child);

    fs::rename(dep_path.join("child"), project.0.join("original-child")).unwrap();
    fs::rename(project.0.join("replacement"), dep_path.join("child")).unwrap();

    assert!(matches!(
        reopen_directory(
            dep.fd(),
            Path::new("child"),
            &dep_path.join("child"),
            expected,
            &mut budget,
        ),
        Err(CheckFailure::Inspect { .. })
    ));
}

fn run_with_low_descriptor_limit(test_name: &str, marker: &str) {
    let executable = std::env::current_exe().unwrap();
    let output = Command::new("sh")
        .args([
            "-c",
            "ulimit -n 32 && exec \"$1\" --exact \"$2\" --nocapture",
            "sh",
        ])
        .arg(executable)
        .arg(test_name)
        .env(marker, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "low-descriptor child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn traversal_does_not_retain_sibling_directory_descriptors() {
    const MARKER: &str = "SYROX_TEST_LOW_FD_SIBLINGS";
    if std::env::var_os(MARKER).is_none() {
        run_with_low_descriptor_limit(
            "project::tests::traversal_does_not_retain_sibling_directory_descriptors",
            MARKER,
        );
        return;
    }

    let project = TempProject::new();
    project.write("main.srx", "inputs { dep = \"path:dep\"; }");
    for index in 0..64 {
        fs::create_dir_all(project.0.join(format!("dep/child-{index:02}"))).unwrap();
    }
    check_project(&project.0).unwrap();
}

#[test]
fn project_does_not_retain_all_input_root_descriptors() {
    const MARKER: &str = "SYROX_TEST_LOW_FD_ROOTS";
    if std::env::var_os(MARKER).is_none() {
        run_with_low_descriptor_limit(
            "project::tests::project_does_not_retain_all_input_root_descriptors",
            MARKER,
        );
        return;
    }

    let project = TempProject::new();
    let mut main = String::from("inputs {");
    for index in 0..64 {
        write!(main, " dep{index} = \"path:dep{index}\";").unwrap();
        fs::create_dir(project.0.join(format!("dep{index}"))).unwrap();
    }
    main.push_str(" }");
    project.write("main.srx", &main);
    check_project(&project.0).unwrap();
}

#[test]
fn hard_linked_sources_cannot_receive_distinct_domain_authority() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "inputs { first = \"path:first\"; second = \"path:second\"; }",
    );
    project.write("first/source.srx", "value V(str);");
    fs::create_dir(project.0.join("second")).unwrap();
    fs::hard_link(
        project.0.join("first/source.srx"),
        project.0.join("second/source.srx"),
    )
    .unwrap();

    assert!(matches!(
        check_project(&project.0).unwrap_err(),
        CheckFailure::AliasedSourceFiles { .. }
    ));
}

#[test]
fn every_configurable_project_limit_has_a_hard_maximum() {
    let limits = [
        ProjectLimits {
            max_total_bytes: MAX_PROJECT_BYTES + 1,
            ..ProjectLimits::default()
        },
        ProjectLimits {
            max_sources: MAX_SOURCES + 1,
            ..ProjectLimits::default()
        },
        ProjectLimits {
            max_directory_entries: MAX_DIRECTORY_ENTRIES + 1,
            ..ProjectLimits::default()
        },
        ProjectLimits {
            max_work: MAX_PROJECT_WORK + 1,
            ..ProjectLimits::default()
        },
        ProjectLimits {
            max_directory_depth: MAX_DIRECTORY_DEPTH + 1,
            ..ProjectLimits::default()
        },
    ];
    for project_limits in limits {
        let configuration = CheckConfiguration {
            project_limits,
            ..CheckConfiguration::default()
        };
        assert!(matches!(
            check_project_with(Path::new("unused"), &configuration).unwrap_err(),
            CheckFailure::InvalidProjectLimit { .. }
        ));
    }
}

#[test]
fn zero_work_rejects_a_main_only_project_before_opening_it() {
    let project = TempProject::new();
    project.write("main.srx", "value V(str);");
    let mut configuration = CheckConfiguration::default();
    configuration.project_limits.max_work = 0;

    assert!(matches!(
        check_project_with(&project.0, &configuration).unwrap_err(),
        CheckFailure::WorkLimit { limit: 0 }
    ));
}

#[test]
fn unavailable_openat2_is_never_mapped_to_a_path_fallback() {
    assert!(matches!(
        open_error(
            Path::new("project"),
            OpenError::Unsupported(io::Error::other("openat2 unavailable")),
        ),
        CheckFailure::UnsupportedKernel { .. }
    ));
}

#[test]
fn configurable_total_byte_and_work_limits_fail_closed() {
    let project = TempProject::new();
    project.write("main.srx", "inputs { dep = \"path:dep\"; }");
    project.write("dep/a.srx", "value V(str);");

    let mut configuration = CheckConfiguration::default();
    configuration.project_limits.max_total_bytes = 1;
    assert!(matches!(
        check_project_with(&project.0, &configuration).unwrap_err(),
        CheckFailure::ProjectTooLarge { .. }
    ));

    configuration.project_limits.max_total_bytes = MAX_PROJECT_BYTES;
    configuration.project_limits.max_work = 0;
    assert!(matches!(
        check_project_with(&project.0, &configuration).unwrap_err(),
        CheckFailure::WorkLimit { .. }
    ));
}

#[test]
fn directory_entry_limit_rejects_during_enumeration_without_changing_later_output() {
    let project = TempProject::new();
    project.write("main.srx", "inputs { dep = \"path:dep\"; }");
    project.write("dep/z.srx", "value Z(str);");
    project.write("dep/a.srx", "value A(str);");
    let mut configuration = CheckConfiguration::default();
    configuration.project_limits.max_directory_entries = 1;

    assert!(matches!(
        check_project_with(&project.0, &configuration).unwrap_err(),
        CheckFailure::TooManyDirectoryEntries { limit: 1 }
    ));

    let loaded = loader::load_project_linux(&project.0, &CheckConfiguration::default()).unwrap();
    let paths: Vec<_> = loaded
        .inputs()
        .next()
        .unwrap()
        .files()
        .map(|source| source.relative_path().to_path_buf())
        .collect();
    assert_eq!(paths, [PathBuf::from("a.srx"), PathBuf::from("z.srx")]);
}

#[test]
fn failed_root_without_diagnostic_capacity_is_still_an_evaluation_failure() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "resource R(int); outputs { bad: [R] = [R(1), R(1)]; }",
    );
    let mut configuration = CheckConfiguration::default();
    configuration.evaluation_limits.max_diagnostics = 0;
    let error = check_file_with(&project.0.join("main.srx"), &configuration).unwrap_err();
    let CheckFailure::Evaluation {
        errors,
        failed_roots,
        ..
    } = error
    else {
        panic!("expected structured evaluation failure");
    };
    assert!(errors.is_empty());
    assert_eq!(failed_roots, ["bad"]);
}

#[test]
fn evaluation_setup_failure_is_structured() {
    let project = TempProject::new();
    project.write("main.srx", "value V(int); outputs { out: V = 1; }");
    let mut configuration = CheckConfiguration::default();
    configuration.evaluation_limits.max_setup_steps = 0;
    let error = check_file_with(&project.0.join("main.srx"), &configuration).unwrap_err();
    assert!(matches!(
        error,
        CheckFailure::EvaluationSetup {
            source: EvaluationSetupError::SetupWorkLimit
        }
    ));
}

#[test]
fn lock_is_idempotent_and_detects_project_drift() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 42; }",
    );

    let created = lock_project(&project.0).unwrap();
    assert_eq!(created.status(), crate::LockStatus::Created);
    let first = fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    assert_eq!(
        lock_project(&project.0).unwrap().status(),
        crate::LockStatus::Unchanged
    );
    assert_eq!(
        fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap(),
        first
    );
    assert_eq!(
        check_project_lock(&project.0).unwrap().status(),
        crate::LockStatus::Unchanged
    );

    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 43; }",
    );
    assert!(matches!(
        check_project_lock(&project.0),
        Err(ProjectOperationError::ProjectDrift)
    ));
    assert_eq!(
        lock_project(&project.0).unwrap().status(),
        crate::LockStatus::Updated
    );
}

#[test]
fn lock_detects_input_and_standard_library_drift() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); inputs { dep = \"path:dep\"; } outputs { answer: Number = 42; }",
    );
    project.write("dep/a.srx", "value Name(str);");
    lock_project(&project.0).unwrap();
    project.write("dep/a.srx", "value Other(str);");
    assert!(matches!(
        check_project_lock(&project.0),
        Err(ProjectOperationError::ProjectDrift)
    ));

    project.write("dep/a.srx", "value Name(str);");
    let source =
        AuthenticatedStandardSource::from_authenticated("std.srx", "value Std(str);").unwrap();
    let configured = CheckConfiguration {
        standard_library: Some(
            AuthenticatedStandardLibrary::from_authenticated(vec![source]).unwrap(),
        ),
        ..CheckConfiguration::default()
    };
    assert!(matches!(
        check_project_lock_with(&project.0, &configured),
        Err(ProjectOperationError::StandardLibraryDrift)
    ));
}

#[test]
fn standard_library_rejects_an_empty_snapshot() {
    let source =
        AuthenticatedStandardSource::from_authenticated("std.srx", "value Std(str);").unwrap();
    assert!(AuthenticatedStandardLibrary::from_authenticated(vec![source]).is_ok());
    assert_eq!(
        AuthenticatedStandardLibrary::from_authenticated(vec![]).unwrap_err(),
        StandardLibraryError::InvalidSourceCount
    );
}

#[test]
fn locked_operations_reject_scope_and_erasure_policies() {
    let identity = |name| CanonicalItemIdentity::new(SourceDomainId::project(), [name]).unwrap();
    let policies = [
        CheckPolicy::new("scope")
            .unwrap()
            .with_scope("network")
            .unwrap(),
        CheckPolicy::new("erasure")
            .unwrap()
            .with_erasure(identity("Private"), identity("Public"))
            .unwrap(),
    ];
    for policy in policies {
        let configuration = CheckConfiguration {
            environment: EvaluationEnvironment::new(&policy),
            policy,
            ..CheckConfiguration::default()
        };
        assert!(matches!(
            lock_project_with(Path::new("unused"), &configuration),
            Err(ProjectOperationError::PolicyProvenanceRequired)
        ));
        assert!(matches!(
            check_project_lock_with(Path::new("unused"), &configuration),
            Err(ProjectOperationError::PolicyProvenanceRequired)
        ));
        assert!(matches!(
            plan_project_with(Path::new("unused"), &configuration),
            Err(ProjectOperationError::PolicyProvenanceRequired)
        ));
    }
}

#[test]
fn lock_rejects_an_environment_for_a_different_policy() {
    let environment_policy = CheckPolicy::new("other").unwrap();
    let configuration = CheckConfiguration {
        environment: EvaluationEnvironment::new(&environment_policy),
        ..CheckConfiguration::default()
    };
    assert!(matches!(
        lock_project_with(Path::new("unused"), &configuration),
        Err(ProjectOperationError::PolicyProvenanceRequired)
    ));
}

#[test]
fn generated_lock_size_failure_has_a_distinct_operation_error() {
    assert!(matches!(
        map_generated_lock_error(crate::LockFormatError::TooLarge),
        ProjectOperationError::GeneratedLockTooLarge
    ));
}

#[test]
fn lock_refuses_missing_malformed_symlink_nonregular_and_hardlinked_destinations() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 42; }",
    );
    assert!(matches!(
        check_project_lock(&project.0),
        Err(ProjectOperationError::MissingLock { .. })
    ));

    project.write(crate::LOCK_FILE_NAME, "not a lock\n");
    assert!(matches!(
        lock_project(&project.0),
        Err(ProjectOperationError::MalformedLock { .. })
    ));
    fs::remove_file(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    fs::create_dir(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    assert!(matches!(
        lock_project(&project.0),
        Err(ProjectOperationError::LockNonRegular)
    ));
    fs::remove_dir(project.0.join(crate::LOCK_FILE_NAME)).unwrap();

    project.write("target", "not a lock\n");
    symlink("target", project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    assert!(matches!(
        lock_project(&project.0),
        Err(ProjectOperationError::LockSymlink)
    ));
    fs::remove_file(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    fs::hard_link(
        project.0.join("target"),
        project.0.join(crate::LOCK_FILE_NAME),
    )
    .unwrap();
    assert!(matches!(
        lock_project(&project.0),
        Err(ProjectOperationError::LockHardLinked)
    ));
}

#[test]
fn lock_failure_before_rename_preserves_the_previous_bytes() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 1; }",
    );
    lock_project(&project.0).unwrap();
    let previous = fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 2; }",
    );
    crate::linux_fd::fail_next_lock_before_rename();

    let error = lock_project(&project.0).unwrap_err();

    assert!(matches!(
        error,
        ProjectOperationError::LockPublication(LockPublicationError::BeforeRename { .. })
    ));
    assert_eq!(
        fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap(),
        previous
    );
    assert_eq!(fs::read_dir(&project.0).unwrap().count(), 2);
}

#[test]
fn lock_failure_after_rename_reports_uncertainty_and_keeps_new_bytes() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 1; }",
    );
    lock_project(&project.0).unwrap();
    let previous = fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 2; }",
    );
    crate::linux_fd::fail_next_lock_after_rename();

    let error = lock_project(&project.0).unwrap_err();

    assert!(matches!(
        error,
        ProjectOperationError::LockPublicationUncertain { .. }
    ));
    assert_ne!(
        fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap(),
        previous
    );
    check_project_lock(&project.0).unwrap();
    assert_eq!(fs::read_dir(&project.0).unwrap().count(), 2);
}

#[test]
fn lock_failure_preserves_the_primary_and_cleanup_sync_errors() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 1; }",
    );
    lock_project(&project.0).unwrap();
    let previous = fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 2; }",
    );
    crate::linux_fd::fail_next_lock_before_rename();
    crate::linux_fd::fail_next_lock_cleanup_sync();

    let error = lock_project(&project.0).unwrap_err();
    let ProjectOperationError::LockPublication(LockPublicationError::CleanupFailed {
        source,
        cleanup,
    }) = error
    else {
        panic!("expected primary and cleanup failures");
    };

    assert_eq!(source.to_string(), "injected lock failure before rename");
    let LockCleanupError::Synchronization { source } = cleanup else {
        panic!("expected cleanup synchronization failure");
    };
    assert_eq!(source.to_string(), "injected lock cleanup sync failure");
    assert_eq!(
        fs::read(project.0.join(crate::LOCK_FILE_NAME)).unwrap(),
        previous
    );
    assert_eq!(fs::read_dir(&project.0).unwrap().count(), 2);
}

#[test]
fn plan_is_deterministic_and_projects_values() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); value Text(str); struct Pair { left: Number; right: [Text]; } outputs { pair: Pair = Pair { left = 7; right = [\"a\", \"b\"]; }; }",
    );
    lock_project(&project.0).unwrap();
    let first = plan_project(&project.0).unwrap().to_string();
    let plan = plan_project(&project.0).unwrap();
    let second = plan.to_string();
    assert_eq!(first, second);
    assert_eq!(plan.policy_identity(), "syrox.empty");
    assert_eq!(plan.lock_digest().len(), 32);
    assert!(plan.standard_library().is_none());
    assert_eq!(plan.packages().len(), 0);
    assert!(first.contains("lock sha256 "));
    assert!(first.contains("policy syrox.empty"));
    assert!(first.contains("std absent"));
    assert!(first.contains("left = 0:Number(7)"));
    assert!(first.contains("right = [0:Text(\"a\"), 0:Text(\"b\")]"));
}

#[test]
fn plan_extracts_only_exact_authenticated_package_roots_with_explicit_ids() {
    let plan = package_plan(
        r#"
        value FakeId(str);
        struct FakeDependency { package: FakeId; }
        struct Package { id: FakeId; dependencies: [FakeDependency]; }
        outputs {
            root_name_is_not_the_id: std::Package = std::Package {
                id = "app";
                dependencies = [std::Dependency { package = "lib"; }];
            };
            z: std::Package = std::Package { id = "z"; dependencies = []; };
            library_root: std::Package = std::Package { id = "lib"; dependencies = []; };
            project_lookalike: Package = Package { id = "ignored"; dependencies = []; };
        }
        "#,
    )
    .unwrap();

    let packages: Vec<_> = plan.packages().collect();
    assert_eq!(
        packages
            .iter()
            .map(|package| package.id().as_str())
            .collect::<Vec<_>>(),
        ["lib", "app", "z"]
    );
    assert_eq!(
        packages[1]
            .dependencies()
            .map(crate::PlanPackageId::as_str)
            .collect::<Vec<_>>(),
        ["lib"]
    );
    let display = plan.to_string();
    assert!(display.contains("syrox plan\n"));
    assert!(display.contains("package dependency graph\npackages 3 edges 1"));
    assert!(display.find("package \"lib\"").unwrap() < display.find("root ").unwrap());
}

#[test]
fn acquisition_roots_are_pure_explicit_and_multi_source() {
    let digest = "0".repeat(64);
    let text = format!(
        r#"
        outputs {{
            aggregate: std::Package = std::Package {{ id = "aggregate"; dependencies = []; }};
            app: std::Package = std::Package {{ id = "app"; dependencies = []; }};
            request: std::Acquisition = std::Acquisition {{
                package = "app";
                sources = [
                    std::SourceRequest {{ url = "file:///tmp/pinned.tar"; sha256 = "{digest}"; maximum_bytes = 0; }},
                    std::SourceRequest {{ url = "https://example.org/pinned.tar"; sha256 = "{digest}"; maximum_bytes = 1024; }},
                ];
            }};
        }}
    "#
    );
    let plan = package_plan(&text).unwrap();
    assert_eq!(plan.packages().len(), 2);
    let acquisition = plan.acquisitions().next().unwrap();
    assert_eq!(acquisition.package().as_str(), "app");
    assert_eq!(acquisition.sources().len(), 2);
    assert_eq!(acquisition.sources().next().unwrap().maximum_bytes(), 0);
    assert!(plan.to_string().contains("acquisition \"app\" sources 2"));
}

#[test]
fn plan_refuses_unowned_duplicate_and_invalid_source_requests() {
    let missing = package_plan(
        r#"outputs {
        request: std::Acquisition = std::Acquisition { package = "missing"; sources = []; };
    }"#,
    )
    .unwrap_err();
    assert!(matches!(
        missing,
        ProjectOperationError::Plan(crate::PlanError::MissingAcquisitionPackage { .. })
    ));

    let duplicate = package_plan(
        r#"outputs {
        package: std::Package = std::Package { id = "app"; dependencies = []; };
        first: std::Acquisition = std::Acquisition { package = "app"; sources = []; };
        second: std::Acquisition = std::Acquisition { package = "app"; sources = []; };
    }"#,
    )
    .unwrap_err();
    assert!(matches!(
        duplicate,
        ProjectOperationError::Plan(crate::PlanError::DuplicateAcquisition { .. })
    ));

    let invalid = package_plan(&format!(r#"outputs {{
        package: std::Package = std::Package {{ id = "app"; dependencies = []; }};
        request: std::Acquisition = std::Acquisition {{ package = "app"; sources = [
            std::SourceRequest {{ url = "http://example.org/plain"; sha256 = "{}"; maximum_bytes = 1; }}
        ]; }};
    }}"#, "0".repeat(64))).unwrap_err();
    assert!(matches!(
        invalid,
        ProjectOperationError::Plan(crate::PlanError::InvalidSourceRequest { .. })
    ));
    let local = package_plan(&format!(r#"outputs {{
        package: std::Package = std::Package {{ id = "app"; dependencies = []; }};
        request: std::Acquisition = std::Acquisition {{ package = "app"; sources = [
            std::SourceRequest {{ url = "project:assets/runtime.tar.gz"; sha256 = "{}"; maximum_bytes = 2048; }}
        ]; }};
    }}"#, "0".repeat(64))).unwrap();
    assert_eq!(
        local
            .acquisitions()
            .next()
            .unwrap()
            .sources()
            .next()
            .unwrap()
            .url(),
        "project:assets/runtime.tar.gz"
    );
    let traversal = package_plan(&format!(
        r#"outputs {{
        package: std::Package = std::Package {{ id = "app"; dependencies = []; }};
        request: std::Acquisition = std::Acquisition {{ package = "app"; sources = [
            std::SourceRequest {{ url = "project:../outside.tar.gz"; sha256 = "{}"; maximum_bytes = 2048; }}
        ]; }};
    }}"#,
        "0".repeat(64)
    ));
    assert!(matches!(
        traversal,
        Err(ProjectOperationError::Plan(
            crate::PlanError::InvalidSourceRequest { .. }
        ))
    ));
}

#[test]
fn input_domain_package_lookalike_is_not_extracted() {
    let project = TempProject::new();
    project.write("main.srx", "inputs { fake = \"path:fake\"; }");
    project.write(
        "fake/package.srx",
        "value Id(str); struct Dependency { package: Id; } struct Package { id: Id; dependencies: [Dependency]; } outputs { lookalike: Package = Package { id = \"ignored\"; dependencies = []; }; }",
    );
    let configuration = package_configuration();
    lock_project_with(&project.0, &configuration).unwrap();
    let plan = plan_project_with(&project.0, &configuration).unwrap();
    assert_eq!(plan.packages().len(), 0);
}

fn build_fixture() -> String {
    format!(
        r#"outputs {{
        app: std::Package = std::Package {{ id = "internal-app"; dependencies = []; }};
        aggregate: std::Package = std::Package {{ id = "aggregate"; dependencies = []; }};
        source: std::Acquisition = std::Acquisition {{ package = "internal-app"; sources = [
            std::SourceRequest {{ url = "https://nonexistent.invalid/source"; sha256 = "{}"; maximum_bytes = 100; }}
        ]; }};
        build: std::AutotoolsBuild = std::AutotoolsBuild {{ package = "internal-app";
            source_directory = "app-1.0"; entry = "usr/bin/app"; timeout_seconds = 30;
        }};
        default_build: std::DefaultBuild = std::DefaultBuild {{ package = "internal-app"; }};
    }}"#,
        "0".repeat(64)
    )
}

#[test]
fn recipe_build_projection_preserves_package_generality_and_public_selection() {
    let plan = package_plan(&build_fixture()).unwrap();
    assert_eq!(plan.builds().len(), 1);
    let build = plan.builds().next().unwrap();
    assert_eq!(build.package().as_str(), "internal-app");
    assert_eq!(build.source_directory(), "app-1.0");
    assert_eq!(plan.default_build(), Some(build.package()));
    assert_eq!(
        plan.packages()
            .find(|package| package.id() == build.package())
            .unwrap()
            .export(),
        Some("app")
    );
    assert!(
        plan.to_string()
            .contains("build \"internal-app\" protocol autotools")
    );
    // An ordinary project struct with the same spelling does not request a build.
    let lookalike = package_plan("value Id(str); struct AutotoolsBuild { package: Id; } outputs { fake: AutotoolsBuild = AutotoolsBuild { package = \"absent\"; }; }").unwrap();
    assert_eq!(lookalike.builds().len(), 0);
}

#[test]
fn application_projection_is_explicit_and_checks_default_and_runtime_roles() {
    let source = build_fixture().replace(
        "default_build: std::DefaultBuild",
        "application: std::Application = std::Application { package = \"internal-app\"; loader = []; libraries = []; }; default_application: std::DefaultApplication = std::DefaultApplication { package = \"internal-app\"; }; default_build: std::DefaultBuild",
    );
    let plan = package_plan(&source).unwrap();
    let app = plan.applications().next().unwrap();
    assert_eq!(app.package().as_str(), "internal-app");
    assert!(app.loader().is_none());
    assert_eq!(app.libraries().len(), 0);
    assert_eq!(plan.default_application(), Some(app.package()));
    assert!(
        plan.to_string()
            .contains("default-application \"internal-app\"")
    );

    let missing_build = source.replace(
        "libraries = [];",
        "libraries = [std::RuntimeLibrary { package = \"aggregate\"; }];",
    );
    assert!(matches!(
        package_plan(&missing_build),
        Err(ProjectOperationError::Plan(
            crate::PlanError::InvalidApplication { .. }
        ))
    ));
    let duplicate = source.replace("default_build: std::DefaultBuild", "another: std::Application = std::Application { package = \"internal-app\"; loader = []; libraries = []; }; default_build: std::DefaultBuild");
    assert!(matches!(
        package_plan(&duplicate),
        Err(ProjectOperationError::Plan(
            crate::PlanError::DuplicateApplication { .. }
        ))
    ));
    let missing_application = source.replace("application: std::Application = std::Application { package = \"internal-app\"; loader = []; libraries = []; }; ", "");
    assert!(matches!(
        package_plan(&missing_application),
        Err(ProjectOperationError::Plan(
            crate::PlanError::InvalidApplication { .. }
        ))
    ));

    let provider = source.replace(
        "default_build: std::DefaultBuild",
        &format!(
            "provider_source: std::Acquisition = std::Acquisition {{ package = \"aggregate\"; sources = [std::SourceRequest {{ url = \"https://nonexistent.invalid/provider\"; sha256 = \"{}\"; maximum_bytes = 100; }}]; }}; provider_build: std::AutotoolsBuild = std::AutotoolsBuild {{ package = \"aggregate\"; source_directory = \"provider-1.0\"; entry = \"usr/bin/loader\"; timeout_seconds = 30; }}; default_build: std::DefaultBuild",
            "1".repeat(64)
        ),
    );
    let provider = provider.replace(
        "loader = [];",
        "loader = [std::RuntimeLoader { package = \"aggregate\"; }];",
    );
    assert_eq!(
        package_plan(&provider)
            .unwrap()
            .applications()
            .next()
            .unwrap()
            .loader()
            .unwrap()
            .as_str(),
        "aggregate"
    );
    let shared_provider = provider.replace(
        "libraries = [];",
        "libraries = [std::RuntimeLibrary { package = \"aggregate\"; }];",
    );
    assert_eq!(
        package_plan(&shared_provider)
            .unwrap()
            .applications()
            .next()
            .unwrap()
            .libraries()
            .len(),
        1
    );
    let duplicate_provider = provider.replace(
        "libraries = [];",
        "libraries = [std::RuntimeLibrary { package = \"aggregate\"; }, std::RuntimeLibrary { package = \"aggregate\"; }];",
    );
    assert!(matches!(
        package_plan(&duplicate_provider),
        Err(ProjectOperationError::Plan(
            crate::PlanError::InvalidApplication { .. }
        ))
    ));
}

#[test]
fn cancelled_realization_has_no_store_or_host_effects() {
    let project = TempProject::new();
    project.write("main.srx", &build_fixture());
    let checks = package_configuration();
    lock_project_with(&project.0, &checks).unwrap();
    let store = project.0.join("not-created");
    let user =
        crate::UserConfiguration::parse("[build]\nhost-toolchain='/usr'", store.clone()).unwrap();
    let selected = crate::resolve_build(project.0.to_str().unwrap(), &user, &checks).unwrap();
    let cancellation = crate::BuildCancellation::default();
    cancellation.cancel();
    let result = crate::realize_build_with_cancellation(
        &selected,
        &user,
        Path::new("/absent-worker"),
        true,
        &cancellation,
        |_| panic!("no progress after pre-cancellation"),
    );
    assert!(matches!(
        result,
        Err(crate::RealizeError::Build(crate::BuildError::Cancelled))
    ));
    assert!(!store.exists());
}

#[test]
fn invalid_build_relationships_paths_and_defaults_fail_during_pure_planning() {
    for (from, to) in [
        (
            "source_directory = \"app-1.0\"",
            "source_directory = \"../escape\"",
        ),
        ("entry = \"usr/bin/app\"", "entry = \"/usr/bin/app\""),
        ("timeout_seconds = 30", "timeout_seconds = 301"),
        (
            "dependencies = [];",
            "dependencies = [std::Dependency { package = \"aggregate\"; }];",
        ),
        (
            "std::DefaultBuild { package = \"internal-app\"; }",
            "std::DefaultBuild { package = \"aggregate\"; }",
        ),
        (
            "std::Acquisition { package = \"internal-app\"",
            "std::Acquisition { package = \"aggregate\"",
        ),
    ] {
        // Only replace the first package's dependencies; avoid an unrelated cycle.
        let text = build_fixture().replacen(from, to, 1);
        assert!(
            matches!(
                package_plan(&text),
                Err(ProjectOperationError::Plan(
                    crate::PlanError::InvalidBuild { .. }
                ))
            ),
            "{text}"
        );
    }
    let duplicate = build_fixture().replace("default_build: std::DefaultBuild", "again: std::AutotoolsBuild = std::AutotoolsBuild { package = \"internal-app\"; source_directory = \"app-1.0\"; entry = \"usr/bin/app\"; timeout_seconds = 30; }; default_build: std::DefaultBuild");
    assert!(matches!(
        package_plan(&duplicate),
        Err(ProjectOperationError::Plan(
            crate::PlanError::DuplicateBuild { .. }
        ))
    ));
}

#[test]
fn authenticated_input_factory_flows_through_lock_check_and_plan() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        r#"
        inputs { catalog = "path:catalog"; }
        outputs { package: std::Package = catalog::zlib(); }
        "#,
    );
    project.write(
        "catalog/package.srx",
        r#"
        fn zlib() -> std::Package {
            std::Package { id = "zlib"; dependencies = []; }
        }
        outputs { zlib: fn() -> std::Package = zlib; }
        "#,
    );
    let configuration = package_configuration();

    lock_project_with(&project.0, &configuration).unwrap();
    assert_eq!(
        check_project_lock_with(&project.0, &configuration)
            .unwrap()
            .status(),
        crate::LockStatus::Unchanged
    );
    let plan = plan_project_with(&project.0, &configuration).unwrap();
    assert_eq!(
        plan.packages()
            .map(|package| package.id().as_str())
            .collect::<Vec<_>>(),
        ["zlib"]
    );
}

#[test]
fn package_graph_errors_are_structured_and_deterministic() {
    let duplicate_node = package_plan(
        r#"outputs {
            z: std::Package = std::Package { id = "same"; dependencies = []; };
            a: std::Package = std::Package { id = "same"; dependencies = []; };
        }"#,
    )
    .unwrap_err();
    assert!(matches!(
        duplicate_node,
        ProjectOperationError::Plan(crate::PlanError::DuplicatePackageId { ref id })
            if id.as_str() == "same"
    ));

    let duplicate_edge = package_plan(
        r#"outputs {
            app: std::Package = std::Package {
                id = "app";
                dependencies = [
                    std::Dependency { package = "lib"; },
                    std::Dependency { package = "lib"; },
                ];
            };
            lib: std::Package = std::Package { id = "lib"; dependencies = []; };
        }"#,
    )
    .unwrap_err();
    assert!(matches!(
        duplicate_edge,
        ProjectOperationError::Plan(crate::PlanError::DuplicateDependency {
            ref package,
            ref dependency,
        }) if package.as_str() == "app" && dependency.as_str() == "lib"
    ));

    let missing = package_plan(
        r#"outputs {
            app: std::Package = std::Package {
                id = "app";
                dependencies = [std::Dependency { package = "missing"; }];
            };
        }"#,
    )
    .unwrap_err();
    assert!(matches!(
        missing,
        ProjectOperationError::Plan(crate::PlanError::MissingDependency {
            ref package,
            ref dependency,
        }) if package.as_str() == "app" && dependency.as_str() == "missing"
    ));
}

#[test]
fn package_ids_and_cycle_witnesses_are_canonical() {
    let invalid_ids = [
        String::new(),
        "Upper".to_owned(),
        "a/b".to_owned(),
        "é".to_owned(),
        "a".repeat(256),
    ];
    for invalid in invalid_ids {
        let source = format!(
            "outputs {{ p: std::Package = std::Package {{ id = \"{invalid}\"; dependencies = []; }}; }}"
        );
        assert!(matches!(
            package_plan(&source),
            Err(ProjectOperationError::Plan(
                crate::PlanError::InvalidPackageId { .. }
            ))
        ));
    }

    let self_cycle = package_plan(
        r#"outputs { p: std::Package = std::Package {
            id = "self";
            dependencies = [std::Dependency { package = "self"; }];
        }; }"#,
    )
    .unwrap_err();
    assert!(matches!(
        self_cycle,
        ProjectOperationError::Plan(crate::PlanError::Cycle { ref packages })
            if packages.iter().map(crate::PlanPackageId::as_str).collect::<Vec<_>>() == ["self"]
    ));

    let cycle = package_plan(
        r#"outputs {
            c: std::Package = std::Package { id = "c"; dependencies = [std::Dependency { package = "b"; }]; };
            a: std::Package = std::Package { id = "a"; dependencies = [std::Dependency { package = "c"; }]; };
            b: std::Package = std::Package { id = "b"; dependencies = [std::Dependency { package = "a"; }]; };
        }"#,
    )
    .unwrap_err();
    assert!(matches!(
        cycle,
        ProjectOperationError::Plan(crate::PlanError::Cycle { ref packages })
            if packages.iter().map(crate::PlanPackageId::as_str).collect::<Vec<_>>() == ["a", "c", "b"]
    ));
}

#[test]
fn deep_package_chain_is_planned_iteratively() {
    let mut source = String::from("outputs {\n");
    for index in 0..512 {
        let dependency = if index == 0 {
            "[]".to_owned()
        } else {
            format!("[std::Dependency {{ package = \"p{}\"; }}]", index - 1)
        };
        writeln!(
            source,
            "p{index}: std::Package = std::Package {{ id = \"p{index}\"; dependencies = {dependency}; }};"
        )
        .unwrap();
    }
    source.push('}');
    let plan = package_plan(&source).unwrap();
    assert_eq!(plan.packages().len(), 512);
    assert_eq!(plan.packages().next().unwrap().id().as_str(), "p0");
    assert_eq!(plan.packages().last().unwrap().id().as_str(), "p511");
}

#[test]
fn plan_carries_authenticated_standard_library_provenance() {
    let project = TempProject::new();
    project.write(
        "main.srx",
        "value Number(int); outputs { answer: Number = 42; }",
    );
    let source =
        AuthenticatedStandardSource::from_authenticated("std.srx", "value Std(str);").unwrap();
    let library = AuthenticatedStandardLibrary::from_authenticated(vec![source]).unwrap();
    let configuration = CheckConfiguration {
        standard_library: Some(library.clone()),
        ..CheckConfiguration::default()
    };

    lock_project_with(&project.0, &configuration).unwrap();
    let plan = plan_project_with(&project.0, &configuration).unwrap();
    let standard = plan.standard_library().unwrap();
    assert_eq!(standard.digest(), library.digest());
    assert!(plan.to_string().contains("std sha256 "));
}

#[test]
fn lock_io_stays_relative_to_retained_root_after_path_swap() {
    let project = TempProject::new();
    project.write("main.srx", "value Number(int);");
    let loaded = loader::load_project_linux(&project.0, &CheckConfiguration::default()).unwrap();
    let moved = project.0.with_extension("opened");
    fs::rename(&project.0, &moved).unwrap();
    fs::create_dir(&project.0).unwrap();

    crate::linux_fd::write_atomic_beneath(
        loaded.root.fd(),
        Path::new(crate::LOCK_FILE_NAME),
        b"retained\n",
    )
    .unwrap();
    assert_eq!(
        fs::read(moved.join(crate::LOCK_FILE_NAME)).unwrap(),
        b"retained\n"
    );
    assert!(!project.0.join(crate::LOCK_FILE_NAME).exists());

    fs::remove_dir(&project.0).unwrap();
    fs::rename(moved, &project.0).unwrap();
}
