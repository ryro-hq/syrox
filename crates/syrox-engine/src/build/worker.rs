use std::io;
use std::os::fd::AsFd as _;
use std::path::Path;
use std::process::{Command, Stdio};

use super::{BuildError, artifact, group, relative_path};

/// Runs only in the private root mounted by the supervisor. stdout is a bounded
/// artifact channel; all payload diagnostics go to stderr. The worker never
/// sees the Store, caller's home, service-manager socket, or host staging directory.
#[allow(clippy::too_many_lines)]
pub(super) fn run(source_directory: &str, action: &str) -> Result<(), BuildError> {
    if !relative_path(source_directory) || source_directory.contains('/') {
        return Err(BuildError::InvalidRequest);
    }
    let status = std::fs::read_to_string("/proc/self/status")?;
    for required in [
        "NoNewPrivs:\t1",
        "Seccomp:\t2",
        "CapEff:\t0000000000000000",
        "CapBnd:\t0000000000000000",
    ] {
        if !status.lines().any(|line| line == required) {
            return Err(BuildError::UnsupportedPlatform);
        }
    }
    let protocol = std::env::var("SYROX_BUILD_WORKER").map_err(|_| BuildError::InvalidRequest)?;
    if !["autotools", "runtime", "development", "glibc"].contains(&protocol.as_str()) {
        return Err(BuildError::InvalidRequest);
    }
    let runtime = if protocol == "runtime" || protocol == "development" {
        let text = std::env::var("SYROX_RUNTIME_ACTION").map_err(|_| BuildError::InvalidRequest)?;
        let digest: crate::ContentDigest = text.parse().map_err(|_| BuildError::InvalidRequest)?;
        if digest.to_string() != text {
            return Err(BuildError::InvalidRequest);
        }
        Some(digest)
    } else {
        None
    };
    let action: crate::ContentDigest = action.parse().map_err(|_| BuildError::InvalidRequest)?;
    let prefix = format!("/syrox/store/{action}/out/usr");
    let output = format!("/out/syrox/store/{action}/out");
    crate::linux_fd::become_build_subreaper()?;
    std::fs::create_dir("/work/source")?;
    step(
        "/usr/bin/tar",
        &[
            if protocol == "glibc" { "-xJf" } else { "-xzf" },
            "/source.tar.gz",
            "-C",
            "/work/source",
            "--no-same-owner",
            "--no-same-permissions",
        ],
        "/work",
        &prefix,
    )?;
    if protocol == "glibc" {
        return run_glibc(source_directory, &action, &prefix, &output);
    }
    let cwd = format!("/work/source/{source_directory}");
    let c_environment = if protocol == "development" {
        Some(declared_c_environment(
            runtime.ok_or(BuildError::InvalidRequest)?,
        )?)
    } else {
        None
    };
    step_with_c(
        "/bin/sh",
        &[
            "./configure",
            &format!("--prefix={prefix}"),
            "--disable-nls",
        ],
        &cwd,
        &prefix,
        c_environment.as_ref(),
    )?;
    if let Some(runtime) = runtime {
        let library = format!("/syrox/store/{runtime}/out/usr/lib");
        let flags = format!(
            "LDFLAGS=-Wl,--dynamic-linker={library}/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags,-rpath,{library}"
        );
        let cc = c_environment.as_ref().map(|(cc, _)| format!("CC={cc}"));
        let args = if let Some(cc) = cc.as_deref() {
            vec!["-j2", flags.as_str(), cc]
        } else {
            vec!["-j2", flags.as_str()]
        };
        step_with_c(
            "/usr/bin/make",
            &args,
            &cwd,
            &prefix,
            c_environment.as_ref(),
        )?;
        let args = if let Some(cc) = cc.as_deref() {
            vec!["install", "DESTDIR=/out", flags.as_str(), cc]
        } else {
            vec!["install", "DESTDIR=/out", flags.as_str()]
        };
        step_with_c(
            "/usr/bin/make",
            &args,
            &cwd,
            &prefix,
            c_environment.as_ref(),
        )?;
    } else {
        step("/usr/bin/make", &["-j2"], &cwd, &prefix)?;
        step("/usr/bin/make", &["install", "DESTDIR=/out"], &cwd, &prefix)?;
    }
    settle_build_descendants()?;
    verify_install_layout(&output, &action.to_string())?;
    group::pack(&[("out", Path::new(&output))], &mut io::stdout().lock())
}

fn declared_c_environment(runtime: crate::ContentDigest) -> Result<(String, String), BuildError> {
    let compiler_include = Command::new("/usr/bin/gcc")
        .arg("-print-file-name=include")
        .output()?;
    if !compiler_include.status.success() {
        return Err(BuildError::Toolchain);
    }
    let include = std::str::from_utf8(&compiler_include.stdout)
        .map_err(|_| BuildError::Toolchain)?
        .trim();
    if !include.starts_with("/usr/lib/gcc/")
        || !Path::new(include).is_dir()
        || include.contains("..")
    {
        return Err(BuildError::Toolchain);
    }
    let development = format!("/syrox/store/{runtime}/dev");
    let out = format!("/syrox/store/{runtime}/out");
    for file in [
        "usr/include/stdio.h",
        "usr/include/errno.h",
        "usr/lib/Scrt1.o",
        "usr/lib/crti.o",
        "usr/lib/crtn.o",
        "usr/lib/libc.so",
    ] {
        if !Path::new(&development).join(file).is_file() {
            return Err(BuildError::UnsupportedInputs);
        }
    }
    // configure executes probes using CC. Link them with the same interpreter
    // and RUNPATH used by make/install, rather than the host's default loader.
    let library = format!("{out}/usr/lib");
    let cc = format!(
        "/usr/bin/gcc -B{development}/usr/lib/ -nostdinc -isystem {include} -isystem {development}/usr/include -isystem /syrox-kernel-headers -Wl,-L,{library} -Wl,--dynamic-linker={library}/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags,-rpath,{library}"
    );
    for file in ["Scrt1.o", "crti.o", "crtn.o", "libc.so"] {
        let found = Command::new("/usr/bin/gcc")
            .args([
                format!("-B{development}/usr/lib/"),
                format!("-print-file-name={file}"),
            ])
            .output()?;
        if !found.status.success()
            || !std::str::from_utf8(&found.stdout)
                .is_ok_and(|path| path.trim().starts_with(&format!("{development}/usr/lib/")))
        {
            return Err(BuildError::UnsupportedInputs);
        }
    }
    Ok((cc, out))
}

fn run_glibc(
    source_directory: &str,
    action: &crate::ContentDigest,
    prefix: &str,
    output: &str,
) -> Result<(), BuildError> {
    let source = format!("/work/source/{source_directory}");
    std::fs::create_dir("/work/build")?;
    step(
        "/bin/sh",
        &[
            &format!("{source}/configure"),
            &format!("--prefix={prefix}"),
            "--disable-werror",
        ],
        "/work/build",
        prefix,
    )?;
    step("/usr/bin/make", &["-j2"], "/work/build", prefix)?;
    step(
        "/usr/bin/make",
        &["install", "DESTDIR=/out"],
        "/work/build",
        prefix,
    )?;
    settle_build_descendants()?;
    verify_install_layout(output, &action.to_string())?;
    generate_glibc_utf8_locale(output)?;
    settle_build_descendants()?;
    let development = format!("/out/syrox/store/{action}/dev");
    split_glibc(Path::new(output), Path::new(&development))?;
    normalize_hardlinks(Path::new(output))?;
    normalize_hardlinks(Path::new(&development))?;
    group::pack(
        &[("out", Path::new(output)), ("dev", Path::new(&development))],
        &mut io::stdout().lock(),
    )
}

fn settle_build_descendants() -> Result<(), BuildError> {
    while let Some(status) = crate::linux_fd::reap_build_descendant()? {
        if !status.success() {
            return Err(BuildError::Execution(format!(
                "background build descendant: {status}"
            )));
        }
    }
    Ok(())
}

/// The upstream install supplies locale sources, but does not compile a
/// C.utf8 locale. Invoke the just-built localedef with the just-built loader
/// and libc in the install staging tree before dividing runtime and dev.
fn generate_glibc_utf8_locale(output: &str) -> Result<(), BuildError> {
    let loader = format!("{output}/usr/lib/ld-linux-x86-64.so.2");
    let library = format!("{output}/usr/lib");
    let localedef = format!("{output}/usr/bin/localedef");
    let i18n = format!("{output}/usr/share/i18n");
    let destination = format!("{output}/usr/lib/locale/C.utf8");
    std::fs::create_dir_all(Path::new(&destination).parent().ok_or(BuildError::Output)?)?;
    let stderr = io::stderr().as_fd().try_clone_to_owned()?;
    let status = Command::new(loader)
        .args([
            "--library-path",
            &library,
            &localedef,
            "--no-archive",
            "-i",
            "C",
            "-f",
            "UTF-8",
            &destination,
        ])
        .env("I18NPATH", i18n)
        .env("LC_ALL", "C")
        .current_dir("/work/build")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stderr))
        .status()?;
    if !status.success() {
        return Err(BuildError::Execution(format!("glibc localedef: {status}")));
    }
    if !Path::new(&destination).join("LC_CTYPE").is_file() {
        return Err(BuildError::Output);
    }
    Ok(())
}

fn normalize_hardlinks(root: &Path) -> Result<(), BuildError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            visited += 1;
            if visited > artifact::MAX_FILES {
                return Err(BuildError::Output);
            }
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() && metadata.nlink() > 1 {
                let temporary = path.with_extension("syrox-copy");
                let mut output = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)?;
                let mut input = std::fs::File::open(&path)?;
                io::copy(&mut input, &mut output)?;
                output.set_permissions(std::fs::Permissions::from_mode(metadata.mode() & 0o777))?;
                output.sync_all()?;
                std::fs::rename(temporary, path)?;
            }
        }
    }
    Ok(())
}

/// Move development-only inputs out of the runtime output after *one* install.
/// This policy follows file roles, never an output-size target. The original
/// installer owns all files; no second configure/make is run for `dev`.
fn split_glibc(runtime: &Path, development: &Path) -> Result<(), BuildError> {
    for directory in ["usr/include", "usr/share"] {
        let source = runtime.join(directory);
        if source.exists() {
            let destination = development.join(directory);
            std::fs::create_dir_all(destination.parent().ok_or(BuildError::Output)?)?;
            std::fs::rename(source, destination)?;
        }
    }
    let libs = runtime.join("usr/lib");
    let dev_libs = development.join("usr/lib");
    std::fs::create_dir_all(&dev_libs)?;
    for entry in std::fs::read_dir(&libs)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(BuildError::Output)?;
        let extension = Path::new(name).extension().and_then(|part| part.to_str());
        if matches!(extension, Some("a" | "o"))
            || name == "libc.so"
            || extension == Some("so") && name != "ld.so"
        {
            let path = entry.path();
            let destination = dev_libs.join(name);
            if std::fs::symlink_metadata(&path)?.is_symlink() {
                // Development linker aliases may refer to runtime SONAMEs in
                // another output. Copy the verified target as a regular file;
                // Store views must never contain cross-output symlinks.
                let target = std::fs::read_link(&path)?;
                if target.components().count() != 1
                    || target
                        .to_str()
                        .is_none_or(|text| !relative_path(text) || text.contains('/'))
                {
                    return Err(BuildError::Output);
                }
                std::fs::copy(libs.join(target), &destination)?;
                std::fs::remove_file(path)?;
            } else {
                std::fs::rename(path, destination)?;
            }
        }
    }
    // glibc's libc.so is a linker script, not an ELF runtime provider. Its
    // absolute path to libc_nonshared.a must refer to the dev output after
    // partitioning; its other references stay at the runtime prefix.
    let script = dev_libs.join("libc.so");
    if script.exists() {
        let text = std::fs::read_to_string(&script)?;
        if text.len() > 4096 {
            return Err(BuildError::Output);
        }
        let runtime_path = runtime
            .to_str()
            .ok_or(BuildError::Output)?
            .trim_start_matches("/out");
        let dev_path = development
            .to_str()
            .ok_or(BuildError::Output)?
            .trim_start_matches("/out");
        let from = format!("{runtime_path}/usr/lib/libc_nonshared.a");
        if !text.contains(&from) {
            return Err(BuildError::Output);
        }
        std::fs::write(
            script,
            text.replace(&from, &format!("{dev_path}/usr/lib/libc_nonshared.a")),
        )?;
    }
    Ok(())
}

fn verify_install_layout(output: &str, action: &str) -> Result<(), BuildError> {
    let mut path = std::path::PathBuf::from("/out");
    for name in ["syrox", "store", action, "out"] {
        let mut entries = std::fs::read_dir(&path)?;
        let entry = entries.next().ok_or(BuildError::Output)??;
        if entry.file_name() != name || entries.next().is_some() {
            return Err(BuildError::Output);
        }
        path.push(name);
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            return Err(BuildError::Output);
        }
    }
    if path != Path::new(output) {
        return Err(BuildError::Output);
    }
    Ok(())
}

fn step(program: &str, arguments: &[&str], cwd: &str, prefix: &str) -> Result<(), BuildError> {
    step_with_c(program, arguments, cwd, prefix, None)
}

fn step_with_c(
    program: &str,
    arguments: &[&str],
    cwd: &str,
    prefix: &str,
    c_environment: Option<&(String, String)>,
) -> Result<(), BuildError> {
    let stderr = io::stderr().as_fd().try_clone_to_owned()?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .env("SYROX_OUTPUT_PREFIX", prefix);
    if let Some((cc, out)) = c_environment {
        command
            .env("CC", cc)
            .env("CPP", format!("{cc} -E"))
            .env("LIBRARY_PATH", format!("{out}/usr/lib"));
    }
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stderr))
        .status()?;
    if !status.success() {
        return Err(BuildError::Execution(format!("{program}: {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[test]
    fn source_install_partitions_development_without_losing_runtime_or_hardlinked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("out");
        let dev = tmp.path().join("dev");
        std::fs::create_dir_all(runtime.join("usr/include")).unwrap();
        std::fs::create_dir_all(runtime.join("usr/include/bits/types")).unwrap();
        std::fs::create_dir_all(runtime.join("usr/share/man")).unwrap();
        std::fs::create_dir_all(runtime.join("usr/share/i18n/locales")).unwrap();
        std::fs::create_dir_all(runtime.join("usr/share/i18n/charmaps")).unwrap();
        std::fs::create_dir_all(runtime.join("usr/lib")).unwrap();
        std::fs::create_dir_all(runtime.join("usr/bin")).unwrap();
        std::fs::write(runtime.join("usr/include/stdio.h"), b"header").unwrap();
        std::fs::write(runtime.join("usr/include/bits/types.h"), b"types").unwrap();
        std::fs::write(runtime.join("usr/include/bits/types/wint_t.h"), b"wint").unwrap();
        std::fs::write(runtime.join("usr/share/man/glibc.1"), b"manual").unwrap();
        std::fs::write(runtime.join("usr/share/i18n/locales/de_DE@euro"), b"locale").unwrap();
        std::fs::write(
            runtime.join("usr/share/i18n/charmaps/ISO_8859-1,GL.gz"),
            b"charmap",
        )
        .unwrap();
        std::fs::write(runtime.join("usr/lib/libc_nonshared.a"), b"archive").unwrap();
        let loader = runtime.join("usr/lib/ld-linux-x86-64.so.2");
        std::fs::write(&loader, b"loader").unwrap();
        std::fs::set_permissions(&loader, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(runtime.join("usr/lib/libc.so.6"), b"library").unwrap();
        std::fs::write(runtime.join("usr/lib/libm.so.6"), b"libm").unwrap();
        symlink("libm.so.6", runtime.join("usr/lib/libm.so")).unwrap();
        symlink("../lib/ld-linux-x86-64.so.2", runtime.join("usr/bin/ld.so")).unwrap();
        let script = format!("GROUP ( {}/usr/lib/libc_nonshared.a )\n", runtime.display());
        std::fs::write(runtime.join("usr/lib/libc.so"), script).unwrap();
        let getconf = runtime.join("usr/bin/getconf");
        std::fs::write(&getconf, b"getconf").unwrap();
        std::fs::hard_link(&getconf, runtime.join("usr/bin/getconf2")).unwrap();
        split_glibc(&runtime, &dev).unwrap();
        normalize_hardlinks(&runtime).unwrap();
        normalize_hardlinks(&dev).unwrap();
        assert!(runtime.join("usr/lib/libc.so.6").is_file());
        assert!(dev.join("usr/include/stdio.h").is_file());
        assert!(!runtime.join("usr/include/stdio.h").exists());
        assert_eq!(std::fs::read(dev.join("usr/lib/libm.so")).unwrap(), b"libm");
        assert!(
            std::fs::read_to_string(dev.join("usr/lib/libc.so"))
                .unwrap()
                .contains(&format!("{}/usr/lib/libc_nonshared.a", dev.display()))
        );
        let mut channel = Vec::new();
        group::pack(&[("out", &runtime), ("dev", &dev)], &mut channel).unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = crate::Store::initialize(&store_dir.path().join("store")).unwrap();
        let outputs = group::unpack(
            channel.as_slice(),
            &BTreeMap::from([
                ("dev".into(), String::new()),
                ("out".into(), "usr/lib/ld-linux-x86-64.so.2".into()),
            ]),
            &store.operation().unwrap(),
            &crate::BuildCancellation::default(),
        )
        .unwrap();
        assert!(outputs["out"].files > 2 && outputs["dev"].files > 2);
    }

    #[test]
    #[ignore = "requires SYROX_GLIBC_INSTALL_TREE pointing to a locally installed glibc 2.44 out tree"]
    fn installed_glibc_fixture_partitions_and_packs() {
        use std::io::Seek as _;
        let original = std::path::PathBuf::from(
            std::env::var_os("SYROX_GLIBC_INSTALL_TREE").expect("glibc install tree"),
        );
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("out");
        let status = Command::new("/usr/bin/cp")
            .args(["-a", "--"])
            .arg(&original)
            .arg(&runtime)
            .status()
            .unwrap();
        assert!(status.success());
        let action = original
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let original_prefix = format!("/syrox/store/{action}/out");
        let script = runtime.join("usr/lib/libc.so");
        let text = std::fs::read_to_string(&script).unwrap();
        assert!(text.contains(&original_prefix));
        std::fs::write(
            &script,
            text.replace(&original_prefix, runtime.to_str().unwrap()),
        )
        .unwrap();
        let dev = temp.path().join("dev");
        split_glibc(&runtime, &dev).expect("partition glibc");
        normalize_hardlinks(&runtime).expect("normalize runtime hardlinks");
        normalize_hardlinks(&dev).expect("normalize development hardlinks");
        let mut bytes = io::sink();
        artifact::pack(&runtime, &mut bytes).expect("pack runtime");
        artifact::pack(&dev, &mut bytes).expect("pack development");
        for (root, entry) in [(&runtime, "usr/lib/ld-linux-x86-64.so.2"), (&dev, "")] {
            let mut artifact_file = tempfile::tempfile().unwrap();
            artifact::pack(root, &mut artifact_file).unwrap();
            artifact_file.rewind().unwrap();
            artifact::validate_stream(artifact_file, entry).expect("validate installed output");
        }
        let mut framed = tempfile::tempfile().unwrap();
        group::pack(&[("out", &runtime), ("dev", &dev)], &mut framed).expect("pack both outputs");
        framed.rewind().unwrap();
        let store = crate::Store::initialize(&temp.path().join("store")).unwrap();
        let outputs = group::unpack(
            framed,
            &BTreeMap::from([
                ("dev".into(), String::new()),
                ("out".into(), "usr/lib/ld-linux-x86-64.so.2".into()),
            ]),
            &store.operation().unwrap(),
            &crate::BuildCancellation::default(),
        )
        .expect("validate both installed outputs");
        assert!(outputs["out"].files > 100 && outputs["dev"].files > 100);
    }
}
