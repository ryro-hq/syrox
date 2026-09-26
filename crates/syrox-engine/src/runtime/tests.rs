use super::*;
use crate::build::{self, BuildResult};
use std::fs;
use std::process::{Command, Stdio};

pub(crate) fn elf(
    kind: u16,
    interpreter: Option<&str>,
    needed: &[&str],
    soname: Option<&str>,
    runpath: Option<&str>,
) -> Vec<u8> {
    let mut bytes = vec![0_u8; 0x400];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4..7].copy_from_slice(&[2, 1, 1]);
    bytes[16..18].copy_from_slice(&kind.to_le_bytes());
    bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58]
        .copy_from_slice(&(if interpreter.is_some() { 3_u16 } else { 2_u16 }).to_le_bytes());
    let ph = 64;
    bytes[ph..ph + 4].copy_from_slice(&1_u32.to_le_bytes());
    bytes[ph + 16..ph + 24].copy_from_slice(&0x0040_0000_u64.to_le_bytes());
    bytes[ph + 32..ph + 40].copy_from_slice(&0x400_u64.to_le_bytes());
    bytes[ph + 40..ph + 48].copy_from_slice(&0x400_u64.to_le_bytes());
    let ph = ph + 56;
    bytes[ph..ph + 4].copy_from_slice(&2_u32.to_le_bytes());
    bytes[ph + 8..ph + 16].copy_from_slice(&0x200_u64.to_le_bytes());
    bytes[ph + 32..ph + 40].copy_from_slice(
        &((needed.len() + usize::from(soname.is_some()) + usize::from(runpath.is_some()) + 3)
            as u64
            * 16)
            .to_le_bytes(),
    );
    if let Some(path) = interpreter {
        let ph = ph + 56;
        bytes[ph..ph + 4].copy_from_slice(&3_u32.to_le_bytes());
        bytes[ph + 8..ph + 16].copy_from_slice(&0x180_u64.to_le_bytes());
        bytes[ph + 32..ph + 40].copy_from_slice(&((path.len() + 1) as u64).to_le_bytes());
        bytes[0x180..0x180 + path.len()].copy_from_slice(path.as_bytes());
    }
    let mut cursor = 0x300;
    let mut tags = Vec::new();
    for name in needed {
        tags.push((1_u64, (cursor - 0x300) as u64));
        bytes[cursor..cursor + name.len()].copy_from_slice(name.as_bytes());
        cursor += name.len() + 1;
    }
    if let Some(name) = soname {
        tags.push((14_u64, (cursor - 0x300) as u64));
        bytes[cursor..cursor + name.len()].copy_from_slice(name.as_bytes());
        cursor += name.len() + 1;
    }
    if let Some(path) = runpath {
        tags.push((29_u64, (cursor - 0x300) as u64));
        bytes[cursor..cursor + path.len()].copy_from_slice(path.as_bytes());
        cursor += path.len() + 1;
    }
    tags.push((5, 0x0040_0300));
    tags.push((10, (cursor - 0x300) as u64));
    for (n, (tag, value)) in tags.into_iter().enumerate() {
        let at = 0x200 + n * 16;
        bytes[at..at + 8].copy_from_slice(&tag.to_le_bytes());
        bytes[at + 8..at + 16].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn retained(
    store: &Store,
    name: &str,
    bytes: &[u8],
    library: Option<(&str, &[u8])>,
) -> BuildResult {
    let lease = store.operation().unwrap();
    let mut action = build::cache::tests::action();
    action.package = name.to_owned();
    if let Some((soname, content)) = library {
        build::cache::tests::retained_with_library(&lease, &action, bytes, soname, content)
    } else {
        build::cache::tests::retained(&lease, &action, bytes)
    }
}

fn output(result: &BuildResult) -> RuntimeOutput {
    RuntimeOutput {
        root: result.root.clone(),
        receipt: result.receipt,
    }
}

#[test]
fn cancellation_before_runtime_launch_has_a_distinct_error() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let app = retained(&store, "prelaunch", &elf(2, None, &[], None, None), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    let cancellation = crate::BuildCancellation::default();
    cancellation.cancel();
    assert!(matches!(
        verify_runtime_with_cancellation(
            &store,
            &RuntimeRequest {
                application: output(&app),
                loader: None,
                libraries: vec![]
            },
            &cancellation,
        ),
        Err(RuntimeError::Cancelled)
    ));
    assert!(matches!(
        run_runtime_with_cancellation(&closure, &[], temporary.path(), &cancellation),
        Err(RuntimeError::Cancelled)
    ));
}

#[test]
fn elf_inspection_reads_only_bounded_file_ranges() {
    let mut bytes = elf(3, None, &["libc.so.6"], Some("libsample.so"), None);
    bytes.resize(8 * 1024 * 1024, 0);
    let expected = inspect(&bytes).unwrap();
    let mut read_bytes = 0;
    let actual = inspect_ranges(bytes.len(), |start, length| {
        read_bytes += length;
        Ok(checked(&bytes, start, length)?.to_vec())
    })
    .unwrap();
    assert_eq!(actual.needed, expected.needed);
    assert_eq!(actual.soname, expected.soname);
    assert!(read_bytes < 16 * 1024);
}

#[test]
fn static_elf_closure_is_retained_and_non_elf_is_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let app = retained(&store, "static", &elf(2, None, &[], None, None), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    assert_eq!(
        closure.entry(),
        format!("/syrox/store/{}/out/bin/hello", app.action)
    );
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
    drop(closure);
    assert!(store.maintenance().is_ok());
    let text = retained(&store, "text", b"hello", None);
    assert!(
        verify_runtime(
            &store,
            &RuntimeRequest {
                application: output(&text),
                loader: None,
                libraries: vec![]
            }
        )
        .is_err()
    );
}

#[test]
fn dynamic_elf_requires_exact_loader_and_transitive_libraries() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let loader = retained(&store, "loader", &elf(3, None, &[], None, None), None);
    let path = format!("/syrox/store/{}/out/bin/hello", loader.action);
    let lib2 = retained(
        &store,
        "second",
        &elf(3, None, &[], None, None),
        Some((
            "libsecond.so.1",
            &elf(3, None, &[], Some("libsecond.so.1"), None),
        )),
    );
    let lib2_path = format!("/syrox/store/{}/out/usr/lib", lib2.action);
    let lib1 = retained(
        &store,
        "first",
        &elf(3, None, &[], None, None),
        Some((
            "libfirst.so.1",
            &elf(
                3,
                None,
                &["libsecond.so.1"],
                Some("libfirst.so.1"),
                Some(&lib2_path),
            ),
        )),
    );
    let lib1_path = format!("/syrox/store/{}/out/usr/lib", lib1.action);
    let app = retained(
        &store,
        "dynamic",
        &elf(3, Some(&path), &["libfirst.so.1"], None, Some(&lib1_path)),
        None,
    );
    let mut request = RuntimeRequest {
        application: output(&app),
        loader: Some(output(&loader)),
        libraries: vec![output(&lib1)],
    };
    assert!(verify_runtime(&store, &request).is_err());
    request.libraries.push(output(&lib2));
    assert_eq!(
        verify_runtime(&store, &request).unwrap().libraries().len(),
        2
    );
    request.loader = None;
    assert!(verify_runtime(&store, &request).is_err());
    request.loader = Some(output(&loader));
    let no_path = retained(
        &store,
        "no_path",
        &elf(3, Some(&path), &["libfirst.so.1"], None, None),
        None,
    );
    request.application = output(&no_path);
    assert!(verify_runtime(&store, &request).is_err());
    let host = retained(
        &store,
        "host",
        &elf(
            3,
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libc.so.6"],
            None,
            None,
        ),
        None,
    );
    request.application = output(&host);
    assert!(verify_runtime(&store, &request).is_err());
}

#[test]
fn identical_loader_and_library_roles_share_one_verified_view() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let provider = retained(
        &store,
        "shared_provider",
        &elf(3, None, &[], None, None),
        Some((
            "libshared.so.1",
            &elf(3, None, &[], Some("libshared.so.1"), None),
        )),
    );
    let prefix = format!("/syrox/store/{}/out", provider.action);
    let application = retained(
        &store,
        "shared_consumer",
        &elf(
            2,
            Some(&format!("{prefix}/bin/hello")),
            &["libshared.so.1"],
            None,
            Some(&format!("{prefix}/usr/lib")),
        ),
        None,
    );
    let request = RuntimeRequest {
        application: output(&application),
        loader: Some(output(&provider)),
        libraries: vec![output(&provider)],
    };
    let closure = verify_runtime(&store, &request).unwrap();
    assert!(
        closure
            .loader()
            .unwrap()
            .shares_directory(&closure.libraries()[0])
    );
    assert_eq!(closure.mounts().unwrap().len(), 2);
    drop(closure);

    let alias = RootName::new("shared_provider_alias").unwrap();
    let lease = store.operation().unwrap();
    let references = lease.build_references(&provider.root).unwrap().unwrap();
    lease.publish_root(&alias, &references).unwrap();
    drop(lease);
    let mut separately_authorized = request.clone();
    separately_authorized.libraries[0].root = alias;
    let closure = verify_runtime(&store, &separately_authorized).unwrap();
    assert!(
        !closure
            .loader()
            .unwrap()
            .shares_directory(&closure.libraries()[0])
    );
    drop(closure);

    crate::store::take_verified_read_bytes();
    verify_runtime(&store, &request).unwrap();
    let shared_bytes = crate::store::take_verified_read_bytes();
    verify_runtime(&store, &separately_authorized).unwrap();
    let separate_bytes = crate::store::take_verified_read_bytes();
    eprintln!("runtime_verified_bytes shared={shared_bytes} separate_roots={separate_bytes}");
    assert!(
        shared_bytes < separate_bytes,
        "shared={shared_bytes} separately_authorized={separate_bytes}"
    );
}

#[test]
fn installed_but_unreachable_module_needs_no_runpath_but_still_needs_a_provider() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let loader = retained(
        &store,
        "module-loader",
        &elf(3, None, &[], None, None),
        None,
    );
    let loader_path = format!("/syrox/store/{}/out/bin/hello", loader.action);
    let libc = retained(
        &store,
        "module-libc",
        &elf(3, None, &[], None, None),
        Some(("libc.so.6", &elf(3, None, &[], Some("libc.so.6"), None))),
    );
    let module = retained(
        &store,
        "unused-module",
        &elf(3, None, &[], None, None),
        Some((
            "liboptional.so.1",
            &elf(3, None, &["libc.so.6"], Some("liboptional.so.1"), None),
        )),
    );
    let path = format!("/syrox/store/{}/out/usr/lib", libc.action);
    let app = retained(
        &store,
        "module-app",
        &elf(3, Some(&loader_path), &["libc.so.6"], None, Some(&path)),
        None,
    );
    let request = RuntimeRequest {
        application: output(&app),
        loader: Some(output(&loader)),
        libraries: vec![output(&libc), output(&module)],
    };
    assert!(verify_runtime(&store, &request).is_ok());
    let broken = retained(
        &store,
        "broken-module",
        &elf(3, None, &[], None, None),
        Some((
            "libbroken.so.1",
            &elf(3, None, &["libmissing.so.1"], Some("libbroken.so.1"), None),
        )),
    );
    let mut request = request;
    request.libraries.push(output(&broken));
    assert!(verify_runtime(&store, &request).is_err());
}

#[test]
fn soname_alias_is_resolved_without_duplicate_elf_provider() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let loader = retained(&store, "loader-alias", &elf(3, None, &[], None, None), None);
    let interpreter = format!("/syrox/store/{}/out/bin/hello", loader.action);
    let mut action = build::cache::tests::action();
    action.package = "aliased-library".into();
    let library = build::cache::tests::retained_with_library_alias(
        &store.operation().unwrap(),
        &action,
        &elf(3, None, &[], None, None),
        "libanswer.so.1",
        "libanswer.so.1.2",
        &elf(3, None, &[], Some("libanswer.so.1"), None),
    );
    let runpath = format!("/syrox/store/{}/out/usr/lib", library.action);
    let app = retained(
        &store,
        "app-alias",
        &elf(
            3,
            Some(&interpreter),
            &["libanswer.so.1"],
            None,
            Some(&runpath),
        ),
        None,
    );
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: Some(output(&loader)),
            libraries: vec![output(&library)],
        },
    )
    .unwrap();
    assert_eq!(closure.mounts().unwrap().len(), 3);
    let alias = closure.libraries()[0]
        .directory()
        .unwrap()
        .join("usr/lib/libanswer.so.1");
    assert!(fs::symlink_metadata(&alias).unwrap().is_file());
}

#[test]
fn library_direct_invocation_interpreter_and_extra_declared_provider_are_allowed() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let loader = retained(
        &store,
        "loader-independent",
        &elf(3, None, &[], None, None),
        None,
    );
    let loader_path = format!("/syrox/store/{}/out/bin/hello", loader.action);
    let library = retained(
        &store,
        "libc-like",
        &elf(3, None, &[], None, None),
        Some((
            "libordinary.so.1",
            &elf(
                3,
                Some("/lib64/ld-linux-x86-64.so.2"),
                &[],
                Some("libordinary.so.1"),
                None,
            ),
        )),
    );
    let extra = retained(
        &store,
        "extra-declared",
        &elf(3, None, &[], None, None),
        Some((
            "libextra.so.1",
            &elf(3, None, &[], Some("libextra.so.1"), None),
        )),
    );
    let app = retained(
        &store,
        "program",
        &elf(
            3,
            Some(&loader_path),
            &["libordinary.so.1"],
            None,
            Some(&format!(
                "/syrox/store/{}/out/usr/lib:/syrox/store/{}/out/usr/lib",
                library.action, extra.action
            )),
        ),
        None,
    );
    let request = RuntimeRequest {
        application: output(&app),
        loader: Some(output(&loader)),
        libraries: vec![output(&library), output(&extra)],
    };
    assert!(verify_runtime(&store, &request).is_ok());
    let missing = RuntimeRequest {
        libraries: vec![output(&extra)],
        ..request
    };
    assert!(verify_runtime(&store, &missing).is_err());
}

#[test]
fn loader_soname_may_be_supplied_by_its_separate_output() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut action = build::cache::tests::action();
    action.package = "standalone-loader".into();
    action.entry = "usr/lib/ld-linux-x86-64.so.2".into();
    let loader = build::cache::tests::retained(
        &store.operation().unwrap(),
        &action,
        &elf(3, None, &[], Some("ld-linux-x86-64.so.2"), None),
    );
    let interpreter = format!(
        "/syrox/store/{}/out/usr/lib/ld-linux-x86-64.so.2",
        loader.action
    );
    let libc = retained(
        &store,
        "separate-libc",
        &elf(3, None, &[], None, None),
        Some((
            "libc.so.6",
            &elf(
                3,
                Some(&interpreter),
                &["ld-linux-x86-64.so.2"],
                Some("libc.so.6"),
                None,
            ),
        )),
    );
    let runpath = format!("/syrox/store/{}/out/usr/lib", libc.action);
    let app = retained(
        &store,
        "separate-app",
        &elf(3, Some(&interpreter), &["libc.so.6"], None, Some(&runpath)),
        None,
    );
    let request = RuntimeRequest {
        application: output(&app),
        loader: Some(output(&loader)),
        libraries: vec![output(&libc)],
    };
    assert!(verify_runtime(&store, &request).is_ok());
}

#[test]
fn elf_inspection_refuses_truncated_and_unsafe_metadata_without_execution() {
    let good = elf(
        3,
        Some("/syrox/store/placeholder/out/bin/ld"),
        &["liba.so"],
        None,
        Some("/syrox/store/placeholder/out/usr/lib"),
    );
    assert_eq!(inspect(&good).unwrap().needed, vec!["liba.so"]);
    for end in [0, 48, 120, 0x310] {
        assert!(inspect(&good[..end]).is_err());
    }
    assert!(inspect(&elf(3, None, &["../host.so"], None, None)).is_err());
    assert_eq!(
        inspect(&elf(3, Some("/lib/ld-linux.so"), &[], None, None))
            .unwrap()
            .interpreter
            .as_deref(),
        Some("/lib/ld-linux.so")
    );
}

#[test]
fn elf_large_dynstr_is_bounded_by_file_and_each_name_not_64_kib() {
    let mut bytes = elf(3, None, &["libanswer.so.1"], None, None);
    bytes.resize(0x20300, 0);
    let length = bytes.len() as u64;
    bytes[64 + 32..64 + 40].copy_from_slice(&length.to_le_bytes());
    bytes[64 + 40..64 + 48].copy_from_slice(&length.to_le_bytes());
    // Dynamic entries: NEEDED, STRTAB, STRSZ, NULL.
    bytes[0x228..0x230].copy_from_slice(&(0x20000_u64).to_le_bytes());
    assert_eq!(inspect(&bytes).unwrap().needed, ["libanswer.so.1"]);
    bytes[0x228..0x230].copy_from_slice(&(length + 1).to_le_bytes());
    assert!(inspect(&bytes).is_err());
}

/// A real kernel/namespace proof, deliberately separate from synthetic ELF
/// metadata tests. Run on a configured Linux `x86_64` host with GCC and the worker.
#[test]
#[ignore = "requires syrox-worker beside the test profile directory, user namespaces and static GCC"]
fn host_static_runtime_mounts_only_the_verified_output_and_preserves_cwd() {
    let temporary = tempfile::tempdir().unwrap();
    let source = r#"
        #include <unistd.h>
        #include <string.h>
        #include <limits.h>
        #include <errno.h>
        #include <linux/capability.h>
        #include <sys/prctl.h>
        #include <sys/syscall.h>
        int main(int argc, char **argv) {
            char cwd[PATH_MAX];
            if (argc != 4 || access(argv[1], R_OK) != 0) return 10;
            if (getcwd(cwd, sizeof(cwd)) == 0 || strcmp(cwd, argv[2]) != 0) return 13;
            if (access("input", R_OK) != 0) return 11;
            if (access("/usr", F_OK) == 0) return 12;
            if (access(argv[3], F_OK) == 0) return 14;
            struct __user_cap_header_struct header = { _LINUX_CAPABILITY_VERSION_3, 0 };
            struct __user_cap_data_struct caps[2] = {0};
            if (syscall(SYS_capget, &header, caps)) return 15;
            for (int n = 0; n < 2; ++n)
                if (caps[n].effective || caps[n].permitted || caps[n].inheritable) return 16;
            if (prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1) return 17;
            for (int cap = 0; cap < 1024; ++cap) {
                int bit = prctl(PR_CAPBSET_READ, cap, 0, 0, 0);
                if (bit < 0) return errno == EINVAL && cap > 0 ? 37 : 18;
                if (bit != 0) return 19;
            }
            return 20;
        }
    "#;
    let binary = temporary.path().join("static-program");
    let mut compiler = Command::new("gcc")
        .args(["-static", "-x", "c", "-o"])
        .arg(&binary)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    compiler
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    assert!(compiler.wait().unwrap().success());
    let store_path = temporary.path().join("store");
    fs::create_dir(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();
    let app = retained(&store, "static-host", &fs::read(binary).unwrap(), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    let cwd = temporary.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    fs::write(cwd.join("input"), b"cwd remains visible").unwrap();
    let entry = closure.entry();
    assert_eq!(closure.mounts().unwrap().len(), 1);
    let result = run_runtime(
        &closure,
        &[
            entry.into(),
            cwd.as_os_str().to_owned(),
            store_path.join("roots").into_os_string(),
        ],
        &cwd,
    )
    .unwrap();
    assert_eq!(result.code(), Some(37));
    fs::write(temporary.path().join("input"), b"parent is writable").unwrap();
    let parent_status = run_runtime(
        &closure,
        &[
            temporary.path().join("input").into_os_string(),
            temporary.path().as_os_str().to_owned(),
            store_path.join("roots").into_os_string(),
        ],
        temporary.path(),
    )
    .unwrap();
    assert_eq!(
        parent_status.code(),
        Some(37),
        "Store must be masked from a parent cwd"
    );
    assert!(run_runtime(&closure, &[], &store_path).is_err());
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
    drop(closure);
    assert!(store.maintenance().is_ok());
    assert!(!temporary.path().join("syrox").exists());
}

fn compile(source: &str, output: &std::path::Path, options: &[&str]) {
    let mut compiler = Command::new("gcc")
        .args(["-x", "c", "-"])
        .args(options)
        .arg("-o")
        .arg(output)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    compiler
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    assert!(compiler.wait().unwrap().success());
}

#[test]
#[ignore = "requires syrox-worker, user namespaces, GCC and a Linux x86_64 bootstrap loader"]
fn host_dynamic_runtime_uses_only_declared_loader_and_library() {
    let temporary = tempfile::tempdir().unwrap();
    let store_path = temporary.path().join("store");
    fs::create_dir(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();
    // Explicitly copy a host loader into a retained fixture output: this is
    // bootstrap provenance for a host proof, not a portable toolchain claim.
    let loader = fs::read("/lib64/ld-linux-x86-64.so.2").unwrap();
    let loader = retained(&store, "bootstrap-loader", &loader, None);
    let shared = temporary.path().join("libanswer.so.1");
    compile(
        "int answer(void) { return 37; }",
        &shared,
        &[
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,-soname,libanswer.so.1",
        ],
    );
    let library = retained(
        &store,
        "bootstrap-library",
        &elf(3, None, &[], None, None),
        Some(("libanswer.so.1", &fs::read(&shared).unwrap())),
    );
    let interpreter = format!("/syrox/store/{}/out/bin/hello", loader.action);
    let runpath = format!("/syrox/store/{}/out/usr/lib", library.action);
    let binary = temporary.path().join("dynamic-program");
    let source = r#"
        extern int answer(void);
        __attribute__((noreturn)) void _start(void) {
            long code = answer();
            __asm__ volatile("mov $60, %%rax; mov %0, %%rdi; syscall"
                : : "r"(code) : "rax", "rdi", "rcx", "r11", "memory");
            __builtin_unreachable();
        }
    "#;
    compile(
        source,
        &binary,
        &[
            "-nostdlib",
            "-no-pie",
            "-Wl,-e,_start",
            &format!("-Wl,--dynamic-linker={interpreter}"),
            &format!("-Wl,-rpath={runpath}"),
            &format!("-L{}", temporary.path().display()),
            "-l:libanswer.so.1",
        ],
    );
    let app = retained(&store, "dynamic-host", &fs::read(binary).unwrap(), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: Some(output(&loader)),
            libraries: vec![output(&library)],
        },
    )
    .unwrap();
    assert_eq!(closure.mounts().unwrap().len(), 3);
    let cwd = temporary.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    assert_eq!(
        run_runtime(&closure, &[], temporary.path()).unwrap().code(),
        Some(37)
    );
    let status = run_runtime(&closure, &[], &cwd).unwrap();
    assert_eq!(status.code(), Some(37));
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
}

#[test]
#[ignore = "requires SYROX_SOURCE_BUILT_GLIBC_OUTPUT pointing to a source-built x86_64 glibc install, GCC and rootless Bubblewrap"]
fn source_built_glibc_loader_and_libc_run_from_separate_retained_outputs() {
    let installed = std::env::var_os("SYROX_SOURCE_BUILT_GLIBC_OUTPUT")
        .expect("set SYROX_SOURCE_BUILT_GLIBC_OUTPUT to the glibc install's out directory");
    let installed = PathBuf::from(installed);
    let library = fs::read(installed.join("usr/lib/libc.so.6")).unwrap();
    let loader_bytes = fs::read(installed.join("usr/lib/ld-linux-x86-64.so.2")).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let store_path = temporary.path().join("store");
    fs::create_dir(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();
    let mut action = build::cache::tests::action();
    action.package = "loader-from-source".into();
    action.entry = "usr/lib/ld-linux-x86-64.so.2".into();
    let loader = build::cache::tests::retained(&store.operation().unwrap(), &action, &loader_bytes);
    let libc = retained(
        &store,
        "libc-from-source",
        &elf(3, None, &[], None, None),
        Some(("libc.so.6", &library)),
    );
    let interpreter = format!(
        "/syrox/store/{}/out/usr/lib/ld-linux-x86-64.so.2",
        loader.action
    );
    let runpath = format!("/syrox/store/{}/out/usr/lib", libc.action);
    let binary = temporary.path().join("application");
    compile(
        "int main(void) { return 37; }",
        &binary,
        &[
            &format!("-Wl,--dynamic-linker={interpreter}"),
            &format!("-Wl,-rpath={runpath}"),
        ],
    );
    let app = retained(
        &store,
        "source-runtime-app",
        &fs::read(binary).unwrap(),
        None,
    );
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: Some(output(&loader)),
            libraries: vec![output(&libc)],
        },
    )
    .unwrap();
    assert_eq!(closure.mounts().unwrap().len(), 3);
    assert_eq!(
        run_runtime(&closure, &[], temporary.path()).unwrap().code(),
        Some(37)
    );
}

#[test]
#[ignore = "requires syrox-worker, user namespaces and a host static compiler"]
fn host_session_ends_descendants_before_the_store_lease_can_be_released() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("fork-program");
    compile(
        r#"
        #include <unistd.h>
        #include <fcntl.h>
        int main(void) {
            int ready[2];
            if (pipe(ready)) return 10;
            int child = fork();
            if (child < 0) return 11;
            if (child == 0) {
                close(ready[0]);
                if (write(ready[1], "x", 1) != 1) return 12;
                close(ready[1]);
                sleep(1);
                int fd = open("late", O_WRONLY | O_CREAT | O_EXCL, 0600);
                if (fd >= 0) close(fd);
                return 0;
            }
            close(ready[1]);
            char signal;
            if (read(ready[0], &signal, 1) != 1) return 13;
            return 37;
        }
    "#,
        &binary,
        &["-static"],
    );
    let store_path = temporary.path().join("store");
    fs::create_dir(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();
    let app = retained(&store, "descendants-host", &fs::read(binary).unwrap(), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&app),
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    let cwd = temporary.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    assert_eq!(run_runtime(&closure, &[], &cwd).unwrap().code(), Some(37));
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
    drop(closure);
    assert!(store.maintenance().is_ok());
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        !cwd.join("late").exists(),
        "a descendant survived the PID-namespace session"
    );
}

#[test]
fn namespace_info_wait_observes_cancellation_without_eof() {
    let (mut info, _writer) = UnixStream::pair().unwrap();
    let cancellation = crate::BuildCancellation::default();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            cancellation.cancel();
        });
        assert!(matches!(
            read_namespace_info(&mut info, &cancellation),
            Err(RuntimeError::Cancelled)
        ));
    });
}

#[test]
fn post_spawn_abort_waits_for_pinned_process() {
    let mut child = Command::new("/usr/bin/sleep").arg("10").spawn().unwrap();
    let pinned = crate::linux_fd::pidfd_open(child.id()).unwrap();
    let result = abort_launch(&mut child, Some(&pinned), RuntimeError::Cancelled);
    assert!(matches!(result, RuntimeError::Cancelled));
    assert!(child.try_wait().unwrap().is_some());
    crate::linux_fd::wait_pidfd(pinned.as_fd()).unwrap();
}

#[test]
fn failed_gate_release_still_settles_pinned_namespace() {
    let mut child = Command::new("/usr/bin/sleep").arg("10").spawn().unwrap();
    let (mut info, mut writer) = UnixStream::pair().unwrap();
    write!(writer, "{{\"child-pid\":{}}}", child.id()).unwrap();
    drop(writer);
    let (mut gate, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let mut namespace = None;
    let cancellation = crate::BuildCancellation::default();
    let error = prepare_namespace(&mut info, &mut gate, &cancellation, &mut namespace).unwrap_err();
    assert!(namespace.is_some(), "gate failed after the PID was pinned");
    drop(gate);
    let result = abort_launch(&mut child, namespace.as_ref(), error);
    assert!(matches!(result, RuntimeError::Io(_)));
    assert!(child.try_wait().unwrap().is_some());
    crate::linux_fd::wait_pidfd(namespace.as_ref().unwrap().as_fd()).unwrap();
}

#[test]
#[ignore = "requires rootless Bubblewrap user and PID namespaces and host static GCC"]
fn real_bubblewrap_cancellation_before_pin_prevents_payload() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("gated-payload");
    compile(
        r#"#include <fcntl.h>
        #include <unistd.h>
        int main(void) {
            int fd = open("ran", O_WRONLY | O_CREAT | O_EXCL, 0600);
            if (fd >= 0) close(fd);
            return 0;
        }"#,
        &binary,
        &["-static"],
    );
    let (mut settled, keepalive) = UnixStream::pair().unwrap();
    let (mut info, info_writer) = UnixStream::pair().unwrap();
    let (mut gate, gate_reader) = UnixStream::pair().unwrap();
    let mut command = Command::new("/usr/bin/bwrap");
    command.process_group(0);
    command.args([
        "--unshare-user",
        "--unshare-pid",
        "--cap-drop",
        "ALL",
        "--die-with-parent",
    ]);
    for (option, fd) in [
        ("--sync-fd", keepalive.as_raw_fd()),
        ("--info-fd", info_writer.as_raw_fd()),
        ("--block-fd", gate_reader.as_raw_fd()),
    ] {
        command.arg(option).arg(fd.to_string());
        crate::linux_fd::pass_fd_to_child(&mut command, fd);
    }
    command
        .arg("--bind")
        .arg(temporary.path())
        .arg(temporary.path());
    command.arg("--chdir").arg(temporary.path());
    command.arg("--").arg(&binary);
    let mut child = command.spawn().unwrap();
    drop((keepalive, info_writer, gate_reader));

    // Bubblewrap has already created PID 1, but the launch code has not yet
    // consumed the info record or opened a pidfd. The unread record remains
    // available to confirm this is the real pre-pin window, not an early exit.
    let mut info_record =
        read_namespace_info(&mut info, &crate::BuildCancellation::default()).unwrap();
    assert!(namespace_pid(&info_record).is_ok());
    info_record.clear();
    let cancellation = crate::BuildCancellation::default();
    cancellation.cancel();
    assert!(matches!(
        prepare_namespace(&mut info, &mut gate, &cancellation, &mut None),
        Err(RuntimeError::Cancelled)
    ));
    assert!(matches!(
        abort_unpinned_launch(&mut child, &mut settled, RuntimeError::Cancelled),
        RuntimeError::Cancelled
    ));
    drop(gate);
    assert!(
        !temporary.path().join("ran").exists(),
        "payload crossed the closed gate"
    );
    assert!(child.try_wait().unwrap().is_some());
}

#[test]
#[ignore = "requires the release syrox-worker binary, Linux user namespaces and static GCC"]
fn native_runtime_refuses_eof_invalid_authorization_and_coordinator_loss() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::initialize(&temporary.path().join("retention-store")).unwrap();
    let binary = temporary.path().join("gated-native");
    compile(
        r#"#include <fcntl.h>
        #include <unistd.h>
        int main(void) {
            int fd = open("ran", O_WRONLY | O_CREAT, 0600);
            if (fd >= 0) close(fd);
            return 0;
        }"#,
        &binary,
        &["-static"],
    );
    let helper = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("syrox-worker");
    for case in 0..6 {
        let (control, monitor_control) = UnixStream::pair().unwrap();
        let (info, info_writer) = crate::linux_fd::seqpacket_pair().unwrap();
        let (mut gate, gate_reader) = UnixStream::pair().unwrap();
        let mut command = Command::new(&helper);
        let retention = store.operation().unwrap().process_retention().unwrap();
        crate::linux_fd::pass_fd_to_child(&mut command, retention.as_raw_fd());
        command.args(["__runtime", "monitor"]);
        for fd in [
            monitor_control.as_raw_fd(),
            info_writer.as_raw_fd(),
            gate_reader.as_raw_fd(),
        ] {
            command.arg(fd.to_string());
            crate::linux_fd::pass_fd_to_child(&mut command, fd);
        }
        command
            .arg("--rw")
            .arg(temporary.path())
            .arg(temporary.path());
        if case == 5 {
            command.args(["--mask", "relative-invalid-target"]);
        }
        command
            .arg("--cwd")
            .arg(temporary.path())
            .arg("--")
            .arg(&binary);
        let mut monitor = command.spawn().unwrap();
        drop(retention);
        drop((monitor_control, info_writer, gate_reader));
        if case == 4 {
            // Lose the coordinator before consuming the kernel-pinned message.
            control.shutdown(std::net::Shutdown::Both).unwrap();
        }
        let pinned = receive_namespace(info.as_fd(), &crate::BuildCancellation::default()).unwrap();
        if case < 4 {
            assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
        }
        match case {
            0 => drop(gate), // EOF must never authorize a payload.
            1 => {
                gate.write_all(b"!").unwrap();
                drop(gate);
            }
            2 | 4 => {
                control.shutdown(std::net::Shutdown::Both).unwrap();
                drop(gate);
            }
            3 => {
                monitor.kill().unwrap();
                drop(gate);
            }
            5 => {
                drop(gate);
            }
            _ => unreachable!(),
        }
        assert!(!monitor.wait().unwrap().success());
        crate::linux_fd::wait_pidfd(pinned.as_fd()).unwrap();
        assert!(store.maintenance().is_ok());
        assert!(
            !temporary.path().join("ran").exists(),
            "unauthorized payload in case {case}"
        );
    }
}

#[test]
#[ignore = "requires rootless Bubblewrap user and PID namespaces and host static GCC"]
fn real_bubblewrap_cancellation_after_pin_prevents_payload_and_settles() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("gated-payload");
    compile(
        r#"#include <fcntl.h>
        #include <unistd.h>
        int main(void) {
            int fd = open("ran", O_WRONLY | O_CREAT | O_EXCL, 0600);
            if (fd >= 0) close(fd);
            return 0;
        }"#,
        &binary,
        &["-static"],
    );
    let (mut info, info_writer) = UnixStream::pair().unwrap();
    let (mut gate, gate_reader) = UnixStream::pair().unwrap();
    let info_fd = info_writer.as_raw_fd();
    let gate_fd = gate_reader.as_raw_fd();
    let mut command = Command::new("/usr/bin/bwrap");
    command.args([
        "--unshare-user",
        "--unshare-pid",
        "--cap-drop",
        "ALL",
        "--die-with-parent",
    ]);
    command.arg("--info-fd").arg(info_fd.to_string());
    command.arg("--block-fd").arg(gate_fd.to_string());
    command
        .arg("--bind")
        .arg(temporary.path())
        .arg(temporary.path());
    command.arg("--chdir").arg(temporary.path());
    command.arg("--").arg(&binary);
    crate::linux_fd::pass_fd_to_child(&mut command, info_fd);
    crate::linux_fd::pass_fd_to_child(&mut command, gate_fd);
    let mut child = command.spawn().unwrap();
    drop((info_writer, gate_reader));
    let cancellation = crate::BuildCancellation::default();
    let mut namespace = None;
    let error =
        prepare_namespace_with_hook(&mut info, &mut gate, &cancellation, &mut namespace, || {
            cancellation.cancel();
        })
        .unwrap_err();
    assert!(
        namespace.is_some(),
        "the real namespace PID 1 must be pinned"
    );
    assert!(matches!(
        abort_launch(&mut child, namespace.as_ref(), error),
        RuntimeError::Cancelled
    ));
    drop(gate);
    assert!(
        !temporary.path().join("ran").exists(),
        "payload crossed the closed gate"
    );
    assert!(child.try_wait().unwrap().is_some());
    crate::linux_fd::wait_pidfd(namespace.as_ref().unwrap().as_fd()).unwrap();
}

#[test]
#[ignore = "requires syrox-worker, user namespaces and host static GCC"]
fn host_runtime_cancellation_settles_before_releasing_retention() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("sleeper");
    compile(
        r#"#include <unistd.h>
        #include <fcntl.h>
        int main(void) {
            int ready = open("ready", O_WRONLY | O_CREAT | O_EXCL, 0600);
            if (ready < 0) return 1;
            close(ready);
            sleep(1);
            int late = open("late", O_WRONLY | O_CREAT | O_EXCL, 0600);
            if (late >= 0) close(late);
            return 0;
        }"#,
        &binary,
        &["-static"],
    );
    let store = Store::initialize(&temporary.path().join("store")).unwrap();
    let result = retained(&store, "cancelled-host", &fs::read(&binary).unwrap(), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&result),
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    let cwd = temporary.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    let cancellation = crate::BuildCancellation::default();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let start = Instant::now();
            while !cwd.join("ready").exists() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                cwd.join("ready").exists(),
                "runtime never reached the payload"
            );
            cancellation.cancel();
        });
        assert!(
            !run_runtime_with_cancellation(&closure, &[], &cwd, &cancellation)
                .unwrap()
                .success()
        );
    });
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
    drop(closure);
    assert!(store.maintenance().is_ok());
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!cwd.join("late").exists());
}

#[test]
#[ignore = "requires syrox-worker, user namespaces and a host static compiler"]
fn namespace_descendants_are_exited_at_return_not_merely_eventually() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("fork-many");
    compile(
        r#"#include <stdio.h>
        #include <unistd.h>
        #include <sys/types.h>
        #include <sys/file.h>
        #include <fcntl.h>
        int main(int argc, char **argv) {
            (void)argv;
            int witness = open("witness", O_RDONLY);
            if (witness < 0 || flock(witness, LOCK_EX)) return 11;
            int ready[2];
            if (pipe(ready)) return 12;
            for (int n = 0; n < 16; ++n) {
                pid_t child = fork();
                if (child < 0) return 13;
                if (child == 0) {
                    close(ready[0]);
                    if (write(ready[1], "x", 1) != 1) _exit(14);
                    sleep(3);
                    _exit(0);
                }
            }
            close(ready[1]);
            char signal;
            for (int n = 0; n < 16; ++n) {
                if (read(ready[0], &signal, 1) != 1) return 15;
            }
            if (argc > 1) {
                FILE *marker = fopen("ready", "w");
                if (!marker) return 16;
                fclose(marker);
                sleep(3);
            }
            return 37;
        }"#,
        &binary,
        &["-static"],
    );
    let store = Store::initialize(&temporary.path().join("store")).unwrap();
    let result = retained(&store, "fork-many", &fs::read(&binary).unwrap(), None);
    let closure = verify_runtime(
        &store,
        &RuntimeRequest {
            application: output(&result),
            loader: None,
            libraries: vec![],
        },
    )
    .unwrap();
    let cwd = temporary.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    fs::write(cwd.join("witness"), b"retention witness").unwrap();
    for _ in 0..10 {
        assert_eq!(run_runtime(&closure, &[], &cwd).unwrap().code(), Some(37));
        let witness = fs::File::open(cwd.join("witness")).unwrap();
        assert!(
            crate::linux_fd::flock(
                witness.as_fd(),
                crate::linux_fd::FlockMode::ExclusiveNonblocking
            )
            .unwrap(),
            "payload still holds a descriptor when run_runtime returned"
        );
    }
    for _ in 0..10 {
        let cancellation = crate::BuildCancellation::default();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !cwd.join("ready").exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert!(cwd.join("ready").exists());
                cancellation.cancel();
            });
            let status =
                run_runtime_with_cancellation(&closure, &["hold".into()], &cwd, &cancellation)
                    .unwrap();
            assert!(!status.success());
        });
        fs::remove_file(cwd.join("ready")).unwrap();
        let witness = fs::File::open(cwd.join("witness")).unwrap();
        assert!(
            crate::linux_fd::flock(
                witness.as_fd(),
                crate::linux_fd::FlockMode::ExclusiveNonblocking
            )
            .unwrap(),
            "cancelled payload still holds a descriptor when run_runtime returned"
        );
    }
}
