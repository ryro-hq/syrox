//! Explicit host integration: requires systemd user services, Bubblewrap,
//! setpriv and Landlock ABI >= 6. Execute with `cargo test --release --locked
//! -p syrox --test build -- --ignored` on the intended build host.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use flate2::{Compression, write::GzEncoder};
use syrox_engine::{
    ContentDigest, RootName, RuntimeOutput, RuntimeRequest, Store, materialize_artifact,
    materialize_named_artifact, run_runtime, verify_runtime,
};

#[test]
#[ignore = "requires Linux build preflight (user systemd, Bubblewrap, setpriv and Landlock); uses a local stalled TLS connection"]
#[allow(clippy::too_many_lines)]
fn build_cli_signals_cancel_pending_https_before_the_deadline() {
    for (signal, code) in [("-INT", 130), ("-TERM", 143)] {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "https://localhost:{}/stall",
            listener.local_addr().unwrap().port()
        );
        fs::write(
            project.join("main.srx"),
            format!(
                r#"outputs {{
            app: std::Package = std::Package {{ id = "app"; dependencies = []; }};
            source: std::Acquisition = std::Acquisition {{ package = "app"; sources = [
                std::SourceRequest {{ url = "{url}"; sha256 = "{}"; maximum_bytes = 100; }}
            ]; }};
            build: std::AutotoolsBuild = std::AutotoolsBuild {{ package = "app";
                source_directory = "app-1"; entry = "usr/bin/app"; timeout_seconds = 30;
            }};
            default_build: std::DefaultBuild = std::DefaultBuild {{ package = "app"; }};
        }}"#,
                "0".repeat(64)
            ),
        )
        .unwrap();
        let config = project.join("config.toml");
        fs::write(
            &config,
            format!("[build]\nhost-toolchain = '/usr'\n[network]\nallow-https = ['{url}']\n"),
        )
        .unwrap();
        let lock = Command::new(env!("CARGO_BIN_EXE_srx"))
            .args(["project", "lock"])
            .arg(project)
            .output()
            .unwrap();
        assert!(
            lock.status.success(),
            "{}",
            String::from_utf8_lossy(&lock.stderr)
        );
        let store = project.join("store");
        let mut child = Command::new(env!("CARGO_BIN_EXE_srx"))
            .arg("build")
            .arg(project)
            .arg("--config")
            .arg(&config)
            .arg("--store")
            .arg(&store)
            .arg("-v")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let observed = (|| {
            let (mut connection, _) = loop {
                match listener.accept() {
                    Ok(value) => break value,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && start.elapsed() < Duration::from_secs(5) =>
                    {
                        if child.try_wait().unwrap().is_some() {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return None,
                    Err(error) => panic!("TLS fixture failed to accept: {error}"),
                }
            };
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0_u8; 4096];
            if connection.read(&mut buffer).unwrap_or(0) == 0 {
                return Some(false);
            }
            let sent = Command::new("/usr/bin/kill")
                .arg(signal)
                .arg(child.id().to_string())
                .status()
                .unwrap();
            if !sent.success() {
                return Some(false);
            }
            let closed = match connection.read(&mut buffer) {
                Ok(0) => true,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => true,
                _ => false,
            };
            Some(closed)
        })();
        while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            observed == Some(true),
            "build never reached TLS: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{signal} took too long"
        );
        assert!(
            !store
                .join("roots/retained")
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("source_"))
        );
    }
}

#[test]
#[ignore = "inspects SYROX_REALIZED_GLIBC_STORE and SYROX_REALIZED_GLIBC_RECEIPT without rebuilding"]
fn retained_source_glibc_has_complete_runtime_and_development_outputs() {
    let path = std::env::var_os("SYROX_REALIZED_GLIBC_STORE").expect("retained Store path");
    let receipt: ContentDigest = std::env::var("SYROX_REALIZED_GLIBC_RECEIPT")
        .expect("retained glibc receipt")
        .parse()
        .unwrap();
    let store = Store::open(Path::new(&path)).unwrap();
    let root = RootName::new(format!("build_{receipt}")).unwrap();
    let runtime = materialize_artifact(&store, &root, receipt).unwrap();
    let development = materialize_named_artifact(&store, &root, receipt, "dev").unwrap();
    assert!(runtime.files() >= 315);
    assert_eq!(development.files(), 1186);
    assert_eq!(
        &fs::read(runtime.directory().unwrap().join(runtime.entry())).unwrap()[..4],
        b"\x7fELF"
    );
    assert!(
        runtime
            .directory()
            .unwrap()
            .join("usr/lib/libc.so.6")
            .is_file()
    );
    assert!(
        runtime
            .directory()
            .unwrap()
            .join("usr/lib/gconv/gconv-modules")
            .is_file()
    );
    assert!(
        runtime
            .directory()
            .unwrap()
            .join("usr/lib/locale/C.utf8/LC_CTYPE")
            .is_file()
    );
    assert!(
        !runtime
            .directory()
            .unwrap()
            .join("usr/include/stdio.h")
            .exists()
    );
    for file in ["usr/include/stdio.h", "usr/lib/libc.a", "usr/lib/crt1.o"] {
        assert!(
            development.directory().unwrap().join(file).is_file(),
            "missing {file}"
        );
    }
    let script =
        fs::read_to_string(development.directory().unwrap().join("usr/lib/libc.so")).unwrap();
    assert!(script.contains(&format!(
        "/syrox/store/{}/dev/usr/lib/libc_nonshared.a",
        runtime.action()
    )));
}

#[test]
#[ignore = "requires SYROX_REALIZED_GLIBC_STORE and SYROX_REALIZED_GLIBC_RECEIPT; copies verified objects into an isolated Store"]
fn retained_source_glibc_refuses_primary_when_development_artifact_is_tampered() {
    let path =
        PathBuf::from(std::env::var_os("SYROX_REALIZED_GLIBC_STORE").expect("retained Store path"));
    let receipt: ContentDigest = std::env::var("SYROX_REALIZED_GLIBC_RECEIPT")
        .expect("retained glibc receipt")
        .parse()
        .unwrap();
    let source = Store::open(&path).unwrap();
    let lease = source.operation().unwrap();
    let record = lease.read_verified(receipt, 4096).unwrap().unwrap();
    let record = std::str::from_utf8(record.as_bytes()).unwrap();
    let mut references = vec![receipt];
    let mut development = None;
    for line in record.lines() {
        if let Some(digest) = line.split_whitespace().nth(1).filter(|_| {
            ["action", "source", "toolchain"]
                .iter()
                .any(|key| line.starts_with(&format!("{key} ")))
        }) {
            references.push(digest.parse().unwrap());
        }
        if let Some(output) = line.strip_prefix("output ") {
            let mut fields = output.split_whitespace();
            let name = fields.next().unwrap();
            let digest = fields.next().unwrap();
            let digest: ContentDigest = digest.parse().unwrap();
            if name == "dev" {
                development = Some(digest);
            }
            references.push(digest);
        }
    }
    references.sort_unstable();
    references.dedup();
    let development = development.expect("named dev output");
    let temporary = tempfile::tempdir().unwrap();
    let isolated = Store::initialize(temporary.path()).unwrap();
    let target = isolated.operation().unwrap();
    for digest in &references {
        let input = lease
            .open_verified(*digest, 128 * 1024 * 1024)
            .unwrap()
            .unwrap();
        target.ingest(input, *digest, 128 * 1024 * 1024).unwrap();
    }
    let root = RootName::new(format!("build_{receipt}")).unwrap();
    target.publish_root(&root, &references).unwrap();
    drop(target);
    assert!(materialize_artifact(&isolated, &root, receipt).is_ok());
    assert!(materialize_named_artifact(&isolated, &root, receipt, "dev").is_ok());
    let blob = temporary.path().join(format!(
        "objects/sha256/{}/{development}",
        &development.to_string()[..2]
    ));
    fs::set_permissions(&blob, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&blob, b"tampered development output").unwrap();
    assert!(materialize_artifact(&isolated, &root, receipt).is_err());
}

#[test]
#[ignore = "requires SYROX_GLIBC_ARCHIVE and a Linux rootless build host; builds glibc 2.44 from source"]
fn host_source_glibc_build_retains_both_outputs_from_one_action() {
    let archive =
        PathBuf::from(std::env::var_os("SYROX_GLIBC_ARCHIVE").expect("pinned glibc tarball path"));
    let temporary = tempfile::tempdir().unwrap();
    fs::copy(&archive, temporary.path().join("source.tar.xz")).unwrap();
    fs::write(temporary.path().join("main.srx"), r#"outputs {
        glibc: std::Package = std::Package { id = "glibc"; dependencies = []; };
        glibc_sources: std::Acquisition = std::Acquisition { package = "glibc"; sources = [
            std::SourceRequest { url = "project:source.tar.xz"; sha256 = "37f600f2bef3c5e8300147059568b2a2e40a7ad6ccc65ce942556d49429cc667"; maximum_bytes = 33554432; }
        ]; };
        glibc_build: std::GlibcBuild = std::GlibcBuild { package = "glibc"; source_directory = "glibc-2.44"; entry = "usr/lib/ld-linux-x86-64.so.2"; timeout_seconds = 1800; };
        default_build: std::DefaultBuild = std::DefaultBuild { package = "glibc"; };
    }"#).unwrap();
    let config = temporary.path().join("config.toml");
    fs::write(&config, "[build]\nhost-toolchain = '/usr'\n").unwrap();
    let store_path = temporary.path().join("store");
    let lock = run(&["project", "lock", temporary.path().to_str().unwrap()]);
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let build = || {
        run(&[
            "build",
            temporary.path().to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--store",
            store_path.to_str().unwrap(),
            "-o",
        ])
    };
    let first = build();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let summary = String::from_utf8(first.stdout).unwrap();
    assert!(summary.contains("built glibc") && summary.contains("protocol glibc"));
    let line = |prefix: &str| {
        summary
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap()
            .to_owned()
    };
    let receipt: ContentDigest = line("receipt ").parse().unwrap();
    let root = RootName::new(line("root ")).unwrap();
    let store = Store::open(&store_path).unwrap();
    let runtime = materialize_artifact(&store, &root, receipt).unwrap();
    let dev = materialize_named_artifact(&store, &root, receipt, "dev").unwrap();
    assert!(
        runtime
            .directory()
            .unwrap()
            .join("usr/lib/libc.so.6")
            .is_file()
    );
    assert!(
        runtime
            .directory()
            .unwrap()
            .join("usr/lib/gconv/gconv-modules")
            .is_file()
    );
    assert!(
        dev.directory()
            .unwrap()
            .join("usr/include/stdio.h")
            .is_file()
    );
    assert!(dev.directory().unwrap().join("usr/lib/libc.a").is_file());
    assert!(dev.directory().unwrap().join("usr/lib/crt1.o").is_file());
    assert!(
        !runtime
            .directory()
            .unwrap()
            .join("usr/include/stdio.h")
            .exists()
    );
    let second = build();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("reused glibc"));
    assert_eq!(managed_roots(&store_path), 1);
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(args)
        .output()
        .unwrap()
}

fn archive(configure: &[u8]) -> Vec<u8> {
    let mut header = [0_u8; 512];
    header[..17].copy_from_slice(b"fixture/configure");
    header[100..108].copy_from_slice(b"0000644\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", configure.len()).as_bytes());
    header[156] = b'0';
    header[148..156].fill(b' ');
    let checksum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    let mut tar = header.to_vec();
    tar.extend_from_slice(configure);
    tar.resize(tar.len().div_ceil(512) * 512 + 1024, 0);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}

fn project(path: &Path, store: &Store, configure: &[u8], timeout: u32) {
    let bytes = archive(configure);
    let digest = ContentDigest::sha256(&bytes);
    store
        .operation()
        .unwrap()
        .ingest(bytes.as_slice(), digest, bytes.len() as u64)
        .unwrap();
    fs::write(path.join("main.srx"), format!(r#"outputs {{
        package: std::Package = std::Package {{ id = "fixture"; dependencies = []; }};
        source: std::Acquisition = std::Acquisition {{ package = "fixture"; sources = [
            std::SourceRequest {{ url = "https://nonexistent.invalid/source"; sha256 = "{digest}"; maximum_bytes = 1048576; }}
        ]; }};
        build: std::AutotoolsBuild = std::AutotoolsBuild {{
            package = "fixture"; source_directory = "fixture";
            entry = "usr/bin/hello"; timeout_seconds = {timeout};
        }};
        default_build: std::DefaultBuild = std::DefaultBuild {{ package = "fixture"; }};
    }}"#)).unwrap();
    fs::write(
        path.join("config.toml"),
        "[build]\nhost-toolchain = '/usr'\n",
    )
    .unwrap();
    let result = run(&["project", "lock", path.to_str().unwrap()]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn build(project: &Path, store: &Path) -> Output {
    run(&[
        "build",
        project.to_str().unwrap(),
        "--config",
        project.join("config.toml").to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--offline",
    ])
}

fn managed_roots(store: &Path) -> usize {
    fs::read_dir(store.join("roots/retained"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("build_"))
        .count()
}

fn corrupt_provider_view(store: &Path) {
    let libc = fs::read_dir(store.join(".syrox-store/views"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("usr/lib/libc.so.6"))
        .find(|path| path.is_file())
        .unwrap();
    let mut bytes = fs::read(&libc).unwrap();
    bytes[0] ^= 1;
    fs::set_permissions(&libc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&libc, bytes).unwrap();
}

fn preseed_sources_if_available(store: &Path) {
    for (variable, identity, maximum) in [
        (
            "SYROX_GLIBC_ARCHIVE",
            "37f600f2bef3c5e8300147059568b2a2e40a7ad6ccc65ce942556d49429cc667",
            32 * 1024 * 1024,
        ),
        (
            "SYROX_HELLO_ARCHIVE",
            "8d99142afd92576f30b0cd7cb42a8dc6809998bc5d607d88761f512e26c7db20",
            2 * 1024 * 1024,
        ),
    ] {
        let Some(archive) = std::env::var_os(variable) else {
            continue;
        };
        let bytes = fs::read(archive).unwrap();
        let digest: ContentDigest = identity.parse().unwrap();
        let store_ref = Store::initialize(store).unwrap();
        let lease = store_ref.operation().unwrap();
        lease.ingest(bytes.as_slice(), digest, maximum).unwrap();
        lease
            .publish_root(
                &RootName::new(format!("source_{digest}")).unwrap(),
                &[digest],
            )
            .unwrap();
    }
}

#[test]
#[ignore = "requires a rootless build host and HTTPS for pinned GNU Hello and glibc 2.44 sources"]
fn host_catalog_dynamic_hello_runs_offline_after_first_realization() {
    let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../syrox-pkgs");
    let catalog = fs::canonicalize(catalog).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    fs::write(&config, "[build]\nhost-toolchain = '/usr'\n[network]\nallow-https = ['https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz', 'https://ftp.gnu.org/gnu/glibc/glibc-2.44.tar.xz']\n").unwrap();
    let store = temp.path().join("store");
    preseed_sources_if_available(&store);
    let main = catalog.join("main.srx");
    let invoke_build = || {
        Command::new(env!("CARGO_BIN_EXE_srx"))
            .args(["build", "-f"])
            .arg(&main)
            .arg("--config")
            .arg(&config)
            .arg("--store")
            .arg(&store)
            .output()
            .unwrap()
    };
    let invocation = |offline: bool, arguments: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_srx"));
        command
            .args(["run", "-f"])
            .arg(&main)
            .arg("--config")
            .arg(&config)
            .arg("--store")
            .arg(&store);
        if offline {
            command.arg("-o");
        }
        command.arg("--").args(arguments).output().unwrap()
    };
    let built = invoke_build();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let built = String::from_utf8(built.stdout).unwrap();
    assert!(built.contains("built hello") && built.contains("runtime-provider glibc"));
    assert_declared_development_action(&store, &built);
    assert_eq!(
        managed_roots(&store),
        2,
        "provider and application have separate roots"
    );
    let provider_root = independent_provider_root(&store, &built);
    let first = invocation(true, &["--version"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("GNU Hello"));
    let cached = invocation(true, &["--greeting", "Olá"]);
    assert!(
        cached.status.success(),
        "{}",
        String::from_utf8_lossy(&cached.stderr)
    );
    assert_eq!(String::from_utf8(cached.stdout).unwrap().trim(), "Olá");
    assert_eq!(managed_roots(&store), 2, "run must reuse the build action");
    let reused = invoke_build();
    assert!(
        reused.status.success(),
        "{}",
        String::from_utf8_lossy(&reused.stderr)
    );
    assert!(String::from_utf8_lossy(&reused.stdout).contains("reused hello"));
    let batch = Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(["build", "-A", "-j", "2", "-f"])
        .arg(&main)
        .arg("--config")
        .arg(&config)
        .arg("--store")
        .arg(&store)
        .arg("-o")
        .output()
        .unwrap();
    assert!(
        batch.status.success(),
        "{}",
        String::from_utf8_lossy(&batch.stderr)
    );
    assert_eq!(
        managed_roots(&store),
        3,
        "batch build also includes the retained bootstrap compatibility export"
    );
    assert_offline_after_provider_gc(&store, &provider_root, || {
        invocation(true, &["--greeting", "Olá"])
    });
    corrupt_provider_view(&store);
    let corrupt = invocation(true, &["--version"]);
    assert!(!corrupt.status.success());
    assert!(
        corrupt.stdout.is_empty(),
        "tampered provider must not launch"
    );
}

fn independent_provider_root(store: &Path, built: &str) -> RootName {
    let application_root = built
        .lines()
        .find_map(|line| line.strip_prefix("root "))
        .unwrap();
    let provider_root = fs::read_dir(store.join("roots/retained"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .find(|name| name.starts_with("build_") && name != application_root)
        .unwrap();
    RootName::new(provider_root).unwrap()
}

fn assert_offline_after_provider_gc(
    store: &Path,
    provider_root: &RootName,
    invoke: impl FnOnce() -> Output,
) {
    let report = Store::open(store)
        .unwrap()
        .maintenance()
        .unwrap()
        .garbage_collect(&syrox_engine::GcRequest {
            collect: true,
            remove_roots: vec![provider_root.clone()],
            ..syrox_engine::GcRequest::default()
        })
        .unwrap();
    assert!(report.removed_roots.contains(provider_root));
    let after_gc = invoke();
    assert!(
        after_gc.status.success(),
        "{}",
        String::from_utf8_lossy(&after_gc.stderr)
    );
    assert_eq!(after_gc.stdout, "Olá\n".as_bytes());
    assert_eq!(
        managed_roots(store),
        2,
        "run after GC must use the retained closure"
    );
}

fn assert_declared_development_action(store: &Path, built: &str) {
    let action: ContentDigest = built
        .lines()
        .find_map(|line| line.strip_prefix("action "))
        .expect("Hello action")
        .parse()
        .unwrap();
    let retained = Store::open(store).unwrap();
    let action_bytes = retained
        .operation()
        .unwrap()
        .read_verified(action, 4096)
        .unwrap()
        .expect("retained action");
    let record = std::str::from_utf8(action_bytes.as_bytes()).unwrap();
    assert!(record.starts_with("syrox-build-action\n"));
    assert!(
        record
            .lines()
            .any(|line| line.starts_with("build-input dev "))
    );
}

#[test]
#[ignore = "builds a static application on a configured Linux host and runs its retained output rootless"]
fn host_build_prefix_materializes_and_runs_at_its_action_path() {
    let project_path = tempfile::tempdir().unwrap();
    let store_path = tempfile::tempdir().unwrap();
    let store = Store::open(store_path.path()).unwrap();
    let configure = br#"set -eu
cat > hello.c <<'END'
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "hold") == 0) {
        int child = fork();
        if (child < 0) return 11;
        if (child == 0) {
            sleep(1);
            int late = open("late", O_WRONLY | O_CREAT | O_EXCL, 0600);
            if (late >= 0) close(late);
            return 0;
        }
        int ready = open("ready", O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (ready < 0) return 12;
        close(ready);
        sleep(5);
        return 0;
    }
    return 37;
}
END
cat > Makefile <<'END'
all:
	/usr/bin/gcc -static -o hello hello.c
install:
	mkdir -p $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin
	install -m 755 hello $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
END
"#;
    project(project_path.path(), &store, configure, 60);
    let main = project_path.path().join("main.srx");
    let source = fs::read_to_string(&main).unwrap().replace(
        "default_build: std::DefaultBuild",
        "application: std::Application = std::Application { package = \"fixture\"; loader = []; libraries = []; }; default_application: std::DefaultApplication = std::DefaultApplication { package = \"fixture\"; }; default_build: std::DefaultBuild",
    );
    fs::write(&main, source).unwrap();
    assert!(
        run(&["project", "lock", project_path.path().to_str().unwrap()])
            .status
            .success()
    );
    let output = build(project_path.path(), store_path.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let field = |label: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(label))
            .unwrap()
    };
    let root = RootName::new(field("root ")).unwrap();
    let receipt = field("receipt ").parse().unwrap();
    let action = field("action ");
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: RuntimeOutput { root, receipt },
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    assert_eq!(
        closure.entry(),
        format!("/syrox/store/{action}/out/usr/bin/hello")
    );
    let status = run_runtime(&closure, &[], project_path.path()).unwrap();
    assert_eq!(status.code(), Some(37));
    let public = run(&[
        "run",
        "-f",
        main.to_str().unwrap(),
        "--config",
        project_path.path().join("config.toml").to_str().unwrap(),
        "--store",
        store_path.path().to_str().unwrap(),
        "-o",
        "--",
        "hello",
    ]);
    assert_eq!(
        public.status.code(),
        Some(37),
        "{}",
        String::from_utf8_lossy(&public.stderr)
    );
    assert_runtime_signals(&main, project_path.path(), store_path.path());
}

fn assert_runtime_signals(main: &Path, project: &Path, store: &Path) {
    for (signal, code) in [("INT", 130), ("TERM", 143)] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_srx"))
            .args([
                "run",
                "-f",
                main.to_str().unwrap(),
                "--config",
                project.join("config.toml").to_str().unwrap(),
                "--store",
                store.to_str().unwrap(),
                "-o",
                "--",
                "hold",
            ])
            .current_dir(project)
            .spawn()
            .unwrap();
        let ready = project.join("ready");
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(240) {
            assert!(
                child.try_wait().unwrap().is_none(),
                "runtime exited before signal"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "runtime did not start");
        assert!(
            Command::new("/usr/bin/kill")
                .args([format!("-{signal}"), child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(child.wait().unwrap().code(), Some(code));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!project.join("late").exists());
        fs::remove_file(ready).unwrap();
    }
}

#[test]
#[ignore = "executes the bounded sandbox on a configured Linux host; inventories /usr"]
fn host_build_publishes_only_after_success_and_rejects_timeout_and_symlink_output() {
    let project_path = tempfile::tempdir().unwrap();
    let store_path = tempfile::tempdir().unwrap();
    let store = Store::open(store_path.path()).unwrap();
    let script = br"set -eu
test ! -e /home
test ! -e /run/user
if touch /usr/syrox-should-be-denied 2>/dev/null; then exit 71; fi
python3 -c 'import socket
try: socket.socket()
except PermissionError: pass
else: raise SystemExit(72)'
cat > Makefile <<'END'
all:
	/usr/bin/true
install:
	mkdir -p $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin
	printf 'fixture executable' > $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
	chmod 755 $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
END
";
    project(project_path.path(), &store, script, 30);
    let output = build(project_path.path(), store_path.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let artifact: ContentDigest = stdout
        .lines()
        .find_map(|l| l.strip_prefix("artifact "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        store
            .operation()
            .unwrap()
            .read_verified(artifact, 1024 * 1024)
            .unwrap()
            .is_some()
    );
    let root = stdout
        .lines()
        .find_map(|line| line.strip_prefix("root "))
        .unwrap();
    assert!(root.starts_with("build_"));
    assert!(store_path.path().join("roots/retained").join(root).exists());
    let retained = fs::read_dir(store_path.path().join("roots/retained"))
        .unwrap()
        .count();

    assert_reuse_and_reindex(project_path.path(), store_path.path(), &stdout);

    // A lingering descendant must be terminated by the enclosing cgroup. No
    // root is created even though the initial script can spawn successfully.
    project(project_path.path(), &store, b"sleep 30 &\nwait\n", 1);
    let timeout = build(project_path.path(), store_path.path());
    assert!(!timeout.status.success());
    assert!(
        String::from_utf8_lossy(&timeout.stderr).contains("deadline"),
        "{}",
        String::from_utf8_lossy(&timeout.stderr)
    );
    assert_eq!(
        fs::read_dir(store_path.path().join("roots/retained"))
            .unwrap()
            .count(),
        retained
    );

    let unsafe_script = br"cat > Makefile <<'END'
all:
	true
install:
	mkdir -p $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin
	ln -s /etc/passwd $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
END
";
    project(project_path.path(), &store, unsafe_script, 30);
    let rejected = build(project_path.path(), store_path.path());
    assert!(!rejected.status.success());
    assert_eq!(
        fs::read_dir(store_path.path().join("roots/retained"))
            .unwrap()
            .count(),
        retained
    );

    assert_outside_output_rejected(project_path.path(), store_path.path(), &store, retained);
}

fn assert_outside_output_rejected(
    project_path: &Path,
    store_path: &Path,
    store: &Store,
    retained: usize,
) {
    let outside_output = br"cat > Makefile <<'END'
all:
	true
install:
	mkdir -p $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin
	printf 'valid' > $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
	chmod 755 $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
	printf 'undeclared' > $(DESTDIR)/unexpected
END
";
    project(project_path, store, outside_output, 30);
    let rejected = build(project_path, store_path);
    assert!(!rejected.status.success());
    assert_eq!(
        fs::read_dir(store_path.join("roots/retained"))
            .unwrap()
            .count(),
        retained
    );
}

fn assert_reuse_and_reindex(project: &Path, store: &Path, stdout: &str) {
    let operations = fs::read_dir(store.join(".syrox-store/builds"))
        .unwrap()
        .count();
    // A verified local hit neither starts a sandbox nor needs a service manager.
    let reused = Command::new(env!("CARGO_BIN_EXE_srx"))
        .args([
            "build",
            project.to_str().unwrap(),
            "--config",
            project.join("config.toml").to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
            "--offline",
        ])
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/syrox-cache-test",
        )
        .output()
        .unwrap();
    assert!(
        reused.status.success(),
        "{}",
        String::from_utf8_lossy(&reused.stderr)
    );
    let reused = String::from_utf8(reused.stdout).unwrap();
    assert!(reused.starts_with("reused fixture\n"));
    let selected = run(&[
        "build",
        "-A",
        "-j",
        "2",
        "-f",
        project.join("main.srx").to_str().unwrap(),
        "--config",
        project.join("config.toml").to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "-o",
    ]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(String::from_utf8_lossy(&selected.stdout).starts_with("reused fixture\n"));
    for prefix in ["action ", "artifact ", "receipt ", "root "] {
        assert_eq!(
            stdout.lines().find(|line| line.starts_with(prefix)),
            reused.lines().find(|line| line.starts_with(prefix))
        );
    }
    assert_eq!(
        fs::read_dir(store.join(".syrox-store/builds"))
            .unwrap()
            .count(),
        operations
    );
    fs::remove_dir_all(store.join(".syrox-store/actions")).unwrap();
    let reindexed = run(&[
        "store",
        "reindex",
        "--config",
        project.join("config.toml").to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
    ]);
    assert!(
        reindexed.status.success(),
        "{}",
        String::from_utf8_lossy(&reindexed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&reindexed.stdout).contains("indexed 1 retained build actions")
    );
}

#[test]
fn malformed_build_request_cannot_launch_or_publish() {
    let path = tempfile::tempdir().unwrap();
    let store_path = tempfile::tempdir().unwrap();
    let store = Store::open(store_path.path()).unwrap();
    project(path.path(), &store, b"exit 0\n", 30);
    let main = path.path().join("main.srx");
    let text = fs::read_to_string(&main).unwrap().replace(
        "source_directory = \"fixture\"",
        "source_directory = \"../escape\"",
    );
    fs::write(&main, text).unwrap();
    assert!(
        run(&["project", "lock", path.path().to_str().unwrap()])
            .status
            .success()
    );
    let invalid = build(path.path(), store_path.path());
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid build request"));
    assert_eq!(
        fs::read_dir(store_path.path().join("roots/retained"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
#[ignore = "executes adopted descendants and output collection on a configured Linux host"]
fn host_collection_waits_for_double_fork_and_rejects_late_failure_or_unsafe_output() {
    let project_path = tempfile::tempdir().unwrap();
    let store_path = tempfile::tempdir().unwrap();
    let store = Store::open(store_path.path()).unwrap();
    let configure = r"cat > late.py <<'PY'
import os, time
if os.fork(): os._exit(0)
os.setsid()
if os.fork(): os._exit(0)
time.sleep(0.3)
with open('/out' + os.environ['SYROX_OUTPUT_PREFIX'] + '/bin/hello', 'w') as output: output.write('stable after orphan')
PY
cat > Makefile <<'END'
all:
	true
install:
	mkdir -p $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin
	printf 'unstable before orphan' > $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
	chmod 755 $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
	python3 late.py
END
";
    project(project_path.path(), &store, configure.as_bytes(), 30);
    let result = build(project_path.path(), store_path.path());
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let artifact: ContentDigest = stdout
        .lines()
        .find_map(|line| line.strip_prefix("artifact "))
        .unwrap()
        .parse()
        .unwrap();
    let bytes = store
        .operation()
        .unwrap()
        .read_verified(artifact, 1024 * 1024)
        .unwrap()
        .unwrap();
    assert!(
        bytes
            .as_bytes()
            .windows(b"stable after orphan".len())
            .any(|part| part == b"stable after orphan")
    );
    assert!(
        !bytes
            .as_bytes()
            .windows(b"unstable before orphan".len())
            .any(|part| part == b"unstable before orphan")
    );
    let roots = fs::read_dir(store_path.path().join("roots/retained"))
        .unwrap()
        .count();
    for (replacement, diagnostic) in [
        ("os._exit(42)", "background build descendant"),
        (
            "path = '/out' + os.environ['SYROX_OUTPUT_PREFIX'] + '/bin/hello'; os.unlink(path); os.symlink('/etc/passwd', path)",
            "build output",
        ),
    ] {
        let script = configure.replace(
            "with open('/out' + os.environ['SYROX_OUTPUT_PREFIX'] + '/bin/hello', 'w') as output: output.write('stable after orphan')",
            replacement,
        );
        project(project_path.path(), &store, script.as_bytes(), 30);
        let result = build(project_path.path(), store_path.path());
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::read_dir(store_path.path().join("roots/retained"))
                .unwrap()
                .count(),
            roots
        );
    }
    project(project_path.path(), &store, b"python3 -c 'import sys; sys.stderr.write(\"x\" * 1048576); sys.stderr.write(\"FINAL_FAILURE_MARKER\\n\"); sys.exit(19)'\n", 30);
    let failed = build(project_path.path(), store_path.path());
    assert!(!failed.status.success());
    let diagnostic = String::from_utf8_lossy(&failed.stderr);
    assert!(
        diagnostic.contains("earlier diagnostics truncated")
            && diagnostic.contains("FINAL_FAILURE_MARKER"),
        "{diagnostic}"
    );
    assert!(failed.stderr.len() < 20 * 1024);
    assert_eq!(
        fs::read_dir(store_path.path().join("roots/retained"))
            .unwrap()
            .count(),
        roots
    );
}

/// Owns exactly one test CLI. Failure paths stop only its uniquely named unit.
struct RunningBuild {
    child: Option<Child>,
    namespace: PathBuf,
    operation: Option<PathBuf>,
    unit: Option<String>,
}

impl RunningBuild {
    fn start(project: &Path, store: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_srx"))
            .arg("build")
            .arg(project)
            .arg("--config")
            .arg(project.join("config.toml"))
            .arg("--store")
            .arg(store)
            .arg("--offline")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            namespace: store.join(".syrox-store/builds"),
            operation: None,
            unit: None,
        }
    }

    fn wait_for_payload(&mut self) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if self.unit.is_none() {
                self.operation = fs::read_dir(&self.namespace)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with("op-")
                            && path.join("stage").exists()
                    });
                self.unit = self.operation.as_ref().map(|path| {
                    format!(
                        "syrox-build-{}.service",
                        &path.file_name().unwrap().to_str().unwrap()[3..]
                    )
                });
            }
            if let Some(unit) = &self.unit {
                let result = systemctl(&["show", unit, "--property=ControlGroup", "--value"]);
                let group = String::from_utf8_lossy(&result.stdout);
                let group = group.trim();
                if result.status.success() && group.starts_with('/') && !group.contains("..") {
                    let path = Path::new("/sys/fs/cgroup").join(&group[1..]);
                    if fs::read_to_string(path.join("cgroup.procs")).is_ok_and(|pids| {
                        pids.lines().any(|pid| {
                            pid.parse::<u32>().is_ok()
                                && fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|argv| {
                                    argv.windows(6).any(|part| part == b"sleep\0")
                                })
                        })
                    }) {
                        return path;
                    }
                }
            }
            if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                let output = self.child.take().unwrap().wait_with_output().unwrap();
                panic!(
                    "build ended before payload readiness: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("build payload did not become ready");
    }

    fn signal_and_wait(&mut self, signal: &str) -> Output {
        let child = self.child.as_mut().unwrap();
        assert!(
            Command::new("/usr/bin/kill")
                .args(["--signal", signal, &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "coordinator did not terminate after {signal}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        self.child.take().unwrap().wait_with_output().unwrap()
    }

    fn wait_for_interest(&mut self, operation: &Path) {
        use std::os::unix::fs::MetadataExt as _;
        let inode = fs::metadata(operation.join("consumers"))
            .unwrap()
            .ino()
            .to_string();
        let pid = self.child.as_ref().unwrap().id().to_string();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let locks = fs::read_to_string("/proc/locks").unwrap();
            if locks.lines().any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.get(1) == Some(&"FLOCK")
                    && fields.get(3) == Some(&"READ")
                    && fields.get(4) == Some(&pid.as_str())
                    && fields
                        .get(5)
                        .is_some_and(|value| value.rsplit(':').next() == Some(inode.as_str()))
            }) {
                return;
            }
            if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                let output = self.child.take().unwrap().wait_with_output().unwrap();
                panic!(
                    "consumer ended before joining: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "consumer did not join the producer"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "test build did not finish");
            std::thread::sleep(Duration::from_millis(25));
        }
        self.child.take().unwrap().wait_with_output().unwrap()
    }
}

#[test]
#[ignore = "two real CLI consumers share a producer and cancel independently on a configured Linux host"]
fn host_concurrent_consumers_share_one_payload_and_cancel_independently() {
    for cancel_coordinator_consumer in [true, false] {
        let project_path = tempfile::tempdir().unwrap();
        let store_path = tempfile::tempdir().unwrap();
        let store = Store::open(store_path.path()).unwrap();
        project(
            project_path.path(),
            &store,
            br"sleep 20
cat > Makefile <<'END'
all:
	true
install:
	mkdir -p $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin
	printf 'shared executable' > $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
	chmod 755 $(DESTDIR)$(SYROX_OUTPUT_PREFIX)/bin/hello
END
",
            60,
        );
        let mut first = RunningBuild::start(project_path.path(), store_path.path());
        let group = first.wait_for_payload();
        let journal = first.operation.clone().unwrap();
        let mut second = RunningBuild::start(project_path.path(), store_path.path());
        second.wait_for_interest(&journal);
        let (cancelled, success) = if cancel_coordinator_consumer {
            (first.signal_and_wait("INT"), second.finish())
        } else {
            (second.signal_and_wait("TERM"), first.finish())
        };
        assert_eq!(
            cancelled.status.code(),
            Some(if cancel_coordinator_consumer {
                130
            } else {
                143
            }),
            "{}",
            String::from_utf8_lossy(&cancelled.stderr)
        );
        assert!(
            success.status.success(),
            "{}",
            String::from_utf8_lossy(&success.stderr)
        );
        assert!(String::from_utf8_lossy(&success.stdout).starts_with(
            if cancel_coordinator_consumer {
                "shared fixture\n"
            } else {
                "built fixture\n"
            }
        ));
        assert_eq!(
            fs::read_dir(store_path.path().join(".syrox-store/builds"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            fs::read_dir(store_path.path().join("roots/retained"))
                .unwrap()
                .count(),
            1
        );
        wait_for_empty_cgroup(&group);
        first.unit = None;
        assert!(
            recover(project_path.path(), store_path.path())
                .status
                .success()
        );
        let hit = build(project_path.path(), store_path.path());
        assert!(
            hit.status.success(),
            "{}",
            String::from_utf8_lossy(&hit.stderr)
        );
        assert!(String::from_utf8_lossy(&hit.stdout).starts_with("reused fixture\n"));
        assert_eq!(
            fs::read_dir(store_path.path().join(".syrox-store/builds"))
                .unwrap()
                .count(),
            0
        );
    }
}

impl Drop for RunningBuild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(unit) = &self.unit {
            let stopped = systemctl(&["stop", unit]);
            if !stopped.status.success() {
                eprintln!(
                    "test unit stop unconfirmed: {unit}, journal {}",
                    self.namespace.display()
                );
            }
        }
    }
}

fn systemctl(arguments: &[&str]) -> Output {
    Command::new("/usr/bin/timeout")
        .args(["--kill-after=1", "5", "/usr/bin/systemctl", "--user"])
        .args(arguments)
        .output()
        .unwrap()
}

fn wait_for_empty_cgroup(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match fs::read_to_string(path.join("cgroup.events")) {
            Ok(events) if events.lines().any(|line| line == "populated 0") => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "test payload cgroup did not settle: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "sends signals to its own coordinator and inspects its owned cgroup on a configured Linux host"]
fn host_cancellation_cleans_up_and_coordinator_death_cannot_publish() {
    let project_path = tempfile::tempdir().unwrap();
    let store_path = tempfile::tempdir().unwrap();
    let store = Store::open(store_path.path()).unwrap();
    for (signal, expected) in [("INT", Some(130)), ("TERM", Some(143)), ("KILL", None)] {
        project(
            project_path.path(),
            &store,
            b"sleep 30 &\nwait\n",
            if signal == "KILL" { 5 } else { 30 },
        );
        let mut operation = RunningBuild::start(project_path.path(), store_path.path());
        let group = operation.wait_for_payload();
        let active = recover(project_path.path(), store_path.path());
        assert!(
            active.status.success(),
            "{}",
            String::from_utf8_lossy(&active.stderr)
        );
        assert!(String::from_utf8_lossy(&active.stdout).contains("active op-"));
        let journal = operation.operation.clone().unwrap();
        let result = operation.signal_and_wait(signal);
        assert_eq!(
            result.status.code(),
            expected,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        wait_for_empty_cgroup(&group);
        if signal == "KILL" {
            assert!(
                journal.join("stage/source").exists(),
                "abrupt death should retain the private staging evidence"
            );
        } else {
            assert!(String::from_utf8_lossy(&result.stderr).contains("build cancelled"));
            assert!(!journal.join("stage").exists());
            assert!(
                fs::read_to_string(journal.join("outcome"))
                    .unwrap()
                    .contains("build cancelled")
            );
        }
        assert_eq!(
            fs::read_dir(store_path.path().join("roots/retained"))
                .unwrap()
                .count(),
            0
        );
        // The cgroup has now independently proved empty. Cleanup is safe even
        // if systemd already garbage-collected the transient unit's metadata.
        operation.unit = None;
        let recovered = recover(project_path.path(), store_path.path());
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(String::from_utf8_lossy(&recovered.stdout).contains("recovered op-"));
        assert!(!journal.exists());
        assert!(
            recover(project_path.path(), store_path.path())
                .status
                .success()
        );
    }
}

fn recover(project: &Path, store: &Path) -> Output {
    run(&[
        "store",
        "recover",
        "--config",
        project.join("config.toml").to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
    ])
}

#[test]
#[ignore = "recovers its own live orphaned cgroup and simulates an unavailable manager connection"]
fn host_orphan_recovery_retains_on_manager_loss_then_stops_and_revokes_launch() {
    let project_path = tempfile::tempdir().unwrap();
    let store_path = tempfile::tempdir().unwrap();
    let store = Store::open(store_path.path()).unwrap();
    project(project_path.path(), &store, b"sleep 30 &\nwait\n", 30);
    let mut operation = RunningBuild::start(project_path.path(), store_path.path());
    let group = operation.wait_for_payload();
    let journal = operation.operation.clone().unwrap();
    let mut child = operation.child.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    // Keep the inherited channels alive but do not wait for their EOF: the
    // orphan is deliberately still running when recovery starts.
    let unavailable = Command::new(env!("CARGO_BIN_EXE_srx"))
        .args(["store", "recover"])
        .arg("--config")
        .arg(project_path.path().join("config.toml"))
        .arg("--store")
        .arg(store_path.path())
        .env(
            "XDG_RUNTIME_DIR",
            project_path.path().join("absent-runtime"),
        )
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-syrox-test-bus",
        )
        .output()
        .unwrap();
    assert!(!unavailable.status.success());
    assert!(String::from_utf8_lossy(&unavailable.stderr).contains("retained op-"));
    assert!(journal.join("stage/source").exists());
    let recovered = recover(project_path.path(), store_path.path());
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    wait_for_empty_cgroup(&group);
    assert!(!journal.exists());
    let delayed = Command::new(env!("CARGO_BIN_EXE_syrox-worker"))
        .args(["__build-gate", journal.to_str().unwrap(), "fixture"])
        .output()
        .unwrap();
    assert!(!delayed.status.success());
    assert_eq!(
        fs::read_dir(store_path.path().join("roots/retained"))
            .unwrap()
            .count(),
        0
    );
    operation.unit = None;
}
