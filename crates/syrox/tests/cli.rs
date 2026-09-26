#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use syrox_engine::{ContentDigest, Store};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempProject(PathBuf);

impl TempProject {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("syrox-cli-test-{}-{id}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(
            path.join("main.srx"),
            "value Number(int); outputs { answer: Number = 42; }",
        )
        .unwrap();
        Self(path)
    }

    fn write(&self, text: &str) {
        std::fs::write(self.0.join("main.srx"), text).unwrap();
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(arguments)
        .output()
        .expect("srx should start")
}

fn project_command(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_srx"))
        .arg("project")
        .args(arguments)
        .output()
        .expect("srx should start")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

#[cfg(target_os = "linux")]
#[test]
fn host_inspect_needs_no_external_command_and_rejects_outside_cgroupfs() {
    let result = Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(["host", "inspect"])
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("openat2:"));
    assert!(output.contains("cgroup-delegation: requires active probe"));
    assert!(output.contains("cgroup-child-control: requires active probe"));
    assert!(output.contains("cgroup-probe-migration: requires active probe"));
    let probe = Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(["host", "inspect", "--probe-cgroup"])
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let probed = String::from_utf8_lossy(&probe.stdout);
    assert!(
        probed.contains("cgroup-child-control: available (")
            || probed.contains("cgroup-child-control: unavailable (")
    );
    assert!(
        probed.contains("cgroup-probe-migration: available (")
            || probed.contains("cgroup-probe-migration: unavailable (")
    );
    assert!(probed.contains("cgroup-delegation: requires active probe"));
    let invalid = run(&["host", "inspect", "--cgroup-parent", "/tmp"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("beneath /sys/fs/cgroup"));
}

#[cfg(target_os = "linux")]
#[test]
fn gated_probe_child_waits_for_parent_before_exit() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(["host", "probe-child"])
        .env("PATH", "/nonexistent")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"R");
    assert!(child.try_wait().unwrap().is_none());
    drop(child.stdin.take());
    assert!(!child.wait().unwrap().success());
}

#[test]
fn check_reports_success_and_language_errors_with_distinct_exit_codes() {
    let valid = project_command(&["check", fixture("valid.srx").to_str().unwrap()]);
    assert!(valid.status.success());
    assert!(String::from_utf8_lossy(&valid.stdout).contains("4 top-level declaration(s)"));

    let without_std = run(&[
        "--std",
        "none",
        "project",
        "check",
        fixture("valid.srx").to_str().unwrap(),
    ]);
    assert!(without_std.status.success());
    assert!(String::from_utf8_lossy(&without_std.stdout).contains("3 top-level declaration(s)"));

    let invalid = project_command(&["check", fixture("invalid.srx").to_str().unwrap()]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("expected `]` after list type"));

    let evaluation_invalid =
        project_command(&["check", fixture("evaluation-invalid.srx").to_str().unwrap()]);
    assert_eq!(evaluation_invalid.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&evaluation_invalid.stderr).contains("resource claim conflicts")
    );
    assert!(
        String::from_utf8_lossy(&evaluation_invalid.stderr)
            .contains("failed evaluation root(s): duplicate")
    );

    let usage = run(&[]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("Usage:"));
    for removed in [
        "recover-builds",
        "reindex-builds",
        "lock",
        "plan",
        "check",
        "fetch",
    ] {
        let output = run(&[removed]);
        assert_eq!(output.status.code(), Some(2), "{removed}");
    }
}

#[test]
fn check_accepts_project_directories_and_reports_loader_errors() {
    let valid = project_command(&["check", fixture("project").to_str().unwrap()]);
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert!(String::from_utf8_lossy(&valid.stdout).contains("4 top-level declaration(s)"));

    let invalid = project_command(&["check", fixture("project-invalid").to_str().unwrap()]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unsupported locator"));
}

#[test]
fn built_in_std_mode_is_locked_and_plans_package_dependencies() {
    let project = TempProject::new();
    project.write(
        r#"outputs {
            application: std::Package = std::Package {
                id = "app";
                dependencies = [std::Dependency { package = "lib"; }];
            };
            library: std::Package = std::Package { id = "lib"; dependencies = []; };
        }"#,
    );
    let path = project.0.to_str().unwrap();

    assert!(project_command(&["lock", path]).status.success());

    let plan = project_command(&["plan", path]);
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let output = String::from_utf8_lossy(&plan.stdout);
    assert!(output.contains("std sha256 "));
    assert!(output.contains("packages 2 edges 1"));
    assert!(output.find("package \"lib\"").unwrap() < output.find("package \"app\"").unwrap());

    let no_std_project = TempProject::new();
    let no_std_path = no_std_project.0.to_str().unwrap();
    assert!(
        run(&["--std", "none", "project", "lock", no_std_path])
            .status
            .success()
    );
    let drift = project_command(&["lock", "--check", no_std_path]);
    assert_eq!(drift.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&drift.stderr).contains("standard library has drifted"));
}

#[test]
fn plan_displays_pinned_sources_without_acquiring_them() {
    let project = TempProject::new();
    let digest = "0".repeat(64);
    project.write(&format!(
        r#"outputs {{
        app: std::Package = std::Package {{ id = "app"; dependencies = []; }};
        inputs_for_app: std::Acquisition = std::Acquisition {{
            package = "app";
            sources = [std::SourceRequest {{
                url = "https://nonexistent.invalid/pinned.tar";
                sha256 = "{digest}";
                maximum_bytes = 1024;
            }}];
        }};
    }}"#
    ));
    let path = project.0.to_str().unwrap();
    assert!(project_command(&["lock", path]).status.success());
    assert!(project_command(&["lock", "--check", path]).status.success());
    let output = project_command(&["plan", path]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let display = String::from_utf8_lossy(&output.stdout);
    assert!(display.contains("syrox plan\n"));
    assert!(display.contains("acquisition \"app\" sources 1"));
    assert!(display.contains("https://nonexistent.invalid/pinned.tar"));
}

#[test]
fn fetch_requires_exact_grant_and_reuses_verified_bytes_offline() {
    let project = TempProject::new();
    let store_path = TempProject::new();
    let bytes = b"pinned source bytes";
    let digest = ContentDigest::sha256(bytes);
    let url = "https://nonexistent.invalid/hello.tar";
    project.write(&format!(
        r#"outputs {{
        app: std::Package = std::Package {{ id = "app"; dependencies = []; }};
        sources: std::Acquisition = std::Acquisition {{ package = "app"; sources = [
            std::SourceRequest {{ url = "{url}"; sha256 = "{digest}"; maximum_bytes = 100; }}
        ]; }};
    }}"#
    ));
    let path = project.0.to_str().unwrap();
    let store = store_path.0.to_str().unwrap();
    assert!(project_command(&["lock", path]).status.success());
    let missing = project_command(&["inspect-source", path, "app", "0", "--store", store]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("absent"));
    let denied = project_command(&[
        "fetch",
        path,
        "app",
        "0",
        "--store",
        store,
        "--root",
        "catalog_hello",
        "--allow-https",
        "https://other.invalid/hello.tar",
    ]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("exact planned source URL"));

    let store_handle = Store::open(&store_path.0).unwrap();
    store_handle
        .operation()
        .unwrap()
        .ingest(&bytes[..], digest, 100)
        .unwrap();
    let fetched = project_command(&[
        "fetch",
        path,
        "app",
        "0",
        "--store",
        store,
        "--root",
        "catalog_hello",
        "--allow-https",
        url,
    ]);
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert!(String::from_utf8_lossy(&fetched.stdout).contains("cache_hit=true"));
    assert!(store_path.0.join("roots/retained/catalog_hello").exists());

    project.write("value Changed(int); outputs { changed: Changed = 2; }");
    let drift = project_command(&[
        "fetch",
        path,
        "app",
        "0",
        "--store",
        store,
        "--root",
        "other_root",
        "--allow-https",
        url,
    ]);
    assert_eq!(drift.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&drift.stderr).contains("drift"));
    assert!(!store_path.0.join("roots/retained/other_root").exists());
}

#[test]
fn package_graph_failures_are_stable_cli_errors() {
    let project = TempProject::new();
    let path = project.0.to_str().unwrap();
    project.write(
        r#"outputs { app: std::Package = std::Package {
            id = "app";
            dependencies = [std::Dependency { package = "missing"; }];
        }; }"#,
    );
    assert!(project_command(&["lock", path]).status.success());
    let missing = project_command(&["plan", path]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("package `app` depends on missing package `missing`")
    );

    project.write(
        r#"outputs {
            a: std::Package = std::Package { id = "a"; dependencies = [std::Dependency { package = "b"; }]; };
            b: std::Package = std::Package { id = "b"; dependencies = [std::Dependency { package = "a"; }]; };
        }"#,
    );
    assert!(project_command(&["lock", path]).status.success());
    let cycle = project_command(&["plan", path]);
    assert_eq!(cycle.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cycle.stderr).contains("package dependency cycle: a -> b"));
}

#[test]
fn lock_check_drift_and_plan_are_end_to_end() {
    let project = TempProject::new();
    let path = project.0.to_str().unwrap();

    let missing = project_command(&["plan", path]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no Syrox.lock"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let created = project_command(&["lock", path]);
    assert!(created.status.success());
    assert!(String::from_utf8_lossy(&created.stdout).contains("lock created"));
    let locked = std::fs::read(project.0.join("Syrox.lock")).unwrap();

    let checked = project_command(&["lock", "--check", path]);
    assert!(checked.status.success());
    assert!(String::from_utf8_lossy(&checked.stdout).contains("lock current"));
    assert_eq!(std::fs::read(project.0.join("Syrox.lock")).unwrap(), locked);

    let planned = project_command(&["plan", path]);
    assert!(planned.status.success());
    assert!(String::from_utf8_lossy(&planned.stdout).contains("value: 0:Number(42)"));
    assert_eq!(std::fs::read(project.0.join("Syrox.lock")).unwrap(), locked);

    std::fs::write(
        project.0.join("main.srx"),
        "value Number(int); outputs { answer: Number = 43; }",
    )
    .unwrap();
    let drift = project_command(&["lock", "--check", path]);
    assert_eq!(drift.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&drift.stderr).contains("drifted"));
    assert_eq!(std::fs::read(project.0.join("Syrox.lock")).unwrap(), locked);
}
