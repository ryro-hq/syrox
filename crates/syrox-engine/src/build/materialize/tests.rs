use super::*;

#[test]
fn cancellation_during_view_copy_stops_before_another_write() {
    struct CancelAfterFirstRead<'a>(&'a super::super::BuildCancellation);
    impl std::io::Read for CancelAfterFirstRead<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            buffer[0] = b'x';
            self.0.cancel();
            Ok(1)
        }
    }
    let cancellation = super::super::BuildCancellation::default();
    let mut output = Vec::new();
    assert!(matches!(
        copy_with_cancellation(
            CancelAfterFirstRead(&cancellation),
            &mut output,
            &cancellation
        ),
        Err(MaterializationError::Cancelled)
    ));
    assert!(output.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn named_receipt_retains_and_materializes_independent_outputs() {
    use super::super::records::NamedOutput;
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let mut action = super::super::cache::tests::action();
    action.protocol = super::super::BuildProtocol::Glibc;
    action.output_names = vec!["dev".into(), "out".into()];
    let make_artifact = |path: &str, content: &[u8]| {
        let tree = tempfile::tempdir().unwrap();
        let destination = tree.path().join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, content).unwrap();
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(if path == "bin/hello" { 0o755 } else { 0o644 }),
        )
        .unwrap();
        let mut bytes = Vec::new();
        artifact::pack(tree.path(), &mut bytes).unwrap();
        bytes
    };
    let primary = make_artifact("bin/hello", b"runtime");
    let development = make_artifact("usr/include/stdio.h", b"headers");
    let named = NamedOutput {
        artifact: ContentDigest::sha256(&development),
        files: 1,
        entry: String::new(),
    };
    let receipt = Receipt::new(&action, ContentDigest::sha256(&primary), 1)
        .with_outputs(&action, BTreeMap::from([("dev".into(), named)]))
        .unwrap();
    let encoded = receipt.encode();
    assert!(encoded.starts_with(b"syrox-build-result\n"));
    assert_eq!(Receipt::parse(&encoded).unwrap(), receipt);
    assert_eq!(receipt.references().len(), 6);
    let lease = store.operation().unwrap();
    let action_bytes = action.encode();
    for bytes in [
        &b"fixture source"[..],
        &b"syrox-host-toolchain\nfixture toolchain"[..],
        &action_bytes,
        &primary,
        &development,
        &encoded,
    ] {
        lease
            .ingest(bytes, ContentDigest::sha256(bytes), bytes.len() as u64)
            .unwrap();
    }
    lease
        .publish_root(&receipt.root(), &receipt.references())
        .unwrap();
    let result = super::super::BuildResult {
        execution: super::super::BuildExecution::Cached,
        action: action.digest(),
        artifact: receipt.output("out").unwrap().artifact,
        receipt: receipt.digest(),
        toolchain: action.toolchain,
        files: receipt.output("out").unwrap().files,
        root: receipt.root(),
    };
    super::super::cache::record(&lease, &action, &result).unwrap();
    assert!(
        super::super::cache::lookup(
            &lease,
            &action,
            None,
            &super::super::BuildCancellation::default()
        )
        .unwrap()
        .is_some()
    );
    drop(lease);
    let out = materialize_artifact(&store, &receipt.root(), receipt.digest()).unwrap();
    let dev = materialize_named_artifact(&store, &receipt.root(), receipt.digest(), "dev").unwrap();
    assert_eq!(
        fs::read(out.directory().unwrap().join("bin/hello")).unwrap(),
        b"runtime"
    );
    assert_eq!(
        fs::read(dev.directory().unwrap().join("usr/include/stdio.h")).unwrap(),
        b"headers"
    );
    assert_eq!(
        dev.logical_prefix(),
        format!("/syrox/store/{}/dev", action.digest())
    );
    assert!(
        materialize_named_artifact(&store, &receipt.root(), receipt.digest(), "missing").is_err()
    );
    fs::set_permissions(
        dev.directory().unwrap().join("usr/include/stdio.h"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(
        dev.directory().unwrap().join("usr/include/stdio.h"),
        b"changed",
    )
    .unwrap();
    assert!(materialize_named_artifact(&store, &receipt.root(), receipt.digest(), "dev").is_err());
    assert!(materialize_artifact(&store, &receipt.root(), receipt.digest()).is_ok());
    drop((out, dev));
    let digest = receipt.outputs["dev"].artifact.to_string();
    let object = temporary
        .path()
        .join("objects/sha256")
        .join(&digest[..2])
        .join(digest);
    fs::write(object, b"corrupt named output").unwrap();
    assert!(
        super::super::cache::lookup(
            &store.operation().unwrap(),
            &action,
            None,
            &super::super::BuildCancellation::default()
        )
        .is_err()
    );
    assert!(materialize_artifact(&store, &receipt.root(), receipt.digest()).is_err());
}

#[test]
fn verified_tree_reuses_and_detects_tampering_and_rebuilds_missing_view() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let result = {
        let lease = store.operation().unwrap();
        super::super::cache::tests::retained(
            &lease,
            &super::super::cache::tests::action(),
            b"hello",
        )
    };
    let caller = RootName::new("caller_view").unwrap();
    {
        let lease = store.operation().unwrap();
        let references = lease.build_references(&result.root).unwrap().unwrap();
        lease.publish_root(&caller, &references).unwrap();
    }
    let view = materialize_artifact(&store, &result.root, result.receipt).unwrap();
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
    assert!(!view.reused());
    assert_eq!(view.files(), 1);
    assert_eq!(
        view.logical_prefix(),
        format!("/syrox/store/{}/out", result.action)
    );
    let path = view.directory().unwrap();
    assert_eq!(fs::read(path.join("bin/hello")).unwrap(), b"hello");
    assert!(
        materialize_artifact(&store, &caller, result.receipt)
            .unwrap()
            .reused()
    );
    fs::set_permissions(path.join("bin/hello"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path.join("bin/hello"), b"other").unwrap();
    assert!(matches!(
        materialize_artifact(&store, &result.root, result.receipt),
        Err(MaterializationError::Invalid(_))
    ));
    drop(view);
    fs::remove_dir_all(&path).unwrap();
    let rebuilt = materialize_artifact(&store, &result.root, result.receipt).unwrap();
    assert!(!rebuilt.reused());
    assert_eq!(
        fs::read(rebuilt.directory().unwrap().join("bin/hello")).unwrap(),
        b"hello"
    );
    drop(rebuilt);
    assert!(store.maintenance().is_ok());
}

#[test]
fn only_retained_receipts_authorize_views_and_recovery_removes_staging() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let result = {
        let lease = store.operation().unwrap();
        super::super::cache::tests::retained(
            &lease,
            &super::super::cache::tests::action(),
            b"hello",
        )
    };
    assert!(
        materialize_artifact(&store, &RootName::new("unknown").unwrap(), result.receipt).is_err()
    );
    store.operation().unwrap().view_directory().unwrap();
    let parent = temporary.path().join(".syrox-store/views");
    fs::create_dir(parent.join("init-0123456789abcdef0123456789abcdef")).unwrap();
    fs::write(
        parent.join("init-0123456789abcdef0123456789abcdef/partial"),
        b"partial",
    )
    .unwrap();
    assert_eq!(recover_materializations(&store).unwrap(), 1);
    assert!(
        !parent
            .join("init-0123456789abcdef0123456789abcdef")
            .exists()
    );
    let _view = materialize_artifact(&store, &result.root, result.receipt).unwrap();
    assert_eq!(recover_materializations(&store).unwrap(), 0);
}

#[test]
fn recovery_refuses_links_and_keeps_unrelated_staging_untouched() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    store.operation().unwrap().view_directory().unwrap();
    let staging = temporary
        .path()
        .join(".syrox-store/views/init-0123456789abcdef0123456789abcdef");
    fs::create_dir(&staging).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", staging.join("unsafe")).unwrap();
    assert!(recover_materializations(&store).is_err());
    assert!(fs::symlink_metadata(staging.join("unsafe")).is_ok());
}

#[test]
fn soname_alias_materializes_as_verified_regular_bytes() {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let tree = tempfile::tempdir().unwrap();
    fs::create_dir(tree.path().join("bin")).unwrap();
    fs::create_dir_all(tree.path().join("usr/lib")).unwrap();
    fs::write(tree.path().join("bin/hello"), b"hello").unwrap();
    fs::set_permissions(
        tree.path().join("bin/hello"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(
        tree.path().join("usr/lib/libanswer.so.1.2"),
        b"shared object",
    )
    .unwrap();
    symlink(
        "libanswer.so.1.2",
        tree.path().join("usr/lib/libanswer.so.1"),
    )
    .unwrap();
    let mut bytes = Vec::new();
    artifact::pack(tree.path(), &mut bytes).unwrap();
    let action = super::super::cache::tests::action();
    let receipt = Receipt::new(&action, ContentDigest::sha256(&bytes), 3);
    let lease = store.operation().unwrap();
    let action_bytes = action.encode();
    let receipt_bytes = receipt.encode();
    for bytes in [
        &b"fixture source"[..],
        &b"syrox-host-toolchain\nfixture toolchain"[..],
        &action_bytes,
        &bytes,
        &receipt_bytes,
    ] {
        lease
            .ingest(bytes, ContentDigest::sha256(bytes), bytes.len() as u64)
            .unwrap();
    }
    lease
        .publish_root(&receipt.root(), &receipt.references())
        .unwrap();
    drop(lease);
    let view = materialize_artifact(&store, &receipt.root(), receipt.digest()).unwrap();
    let alias = view.directory().unwrap().join("usr/lib/libanswer.so.1");
    assert!(!fs::symlink_metadata(&alias).unwrap().is_symlink());
    assert_eq!(fs::read(&alias).unwrap(), b"shared object");
    fs::set_permissions(&alias, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&alias, b"changed").unwrap();
    assert!(materialize_artifact(&store, &receipt.root(), receipt.digest()).is_err());
}

#[test]
fn cross_directory_loader_link_is_a_regular_materialized_file() {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let tree = tempfile::tempdir().unwrap();
    fs::create_dir_all(tree.path().join("usr/bin")).unwrap();
    fs::create_dir_all(tree.path().join("usr/lib")).unwrap();
    let loader = tree.path().join("usr/lib/ld-linux-x86-64.so.2");
    fs::write(&loader, b"loader bytes").unwrap();
    fs::set_permissions(&loader, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(
        "../lib/ld-linux-x86-64.so.2",
        tree.path().join("usr/bin/ld.so"),
    )
    .unwrap();
    let mut artifact_bytes = Vec::new();
    artifact::pack(tree.path(), &mut artifact_bytes).unwrap();
    let mut action = super::super::cache::tests::action();
    action.entry = "usr/bin/ld.so".into();
    let receipt = Receipt::new(&action, ContentDigest::sha256(&artifact_bytes), 2);
    let lease = store.operation().unwrap();
    let action_bytes = action.encode();
    let receipt_bytes = receipt.encode();
    for bytes in [
        &b"fixture source"[..],
        &b"syrox-host-toolchain\nfixture toolchain"[..],
        &action_bytes,
        &artifact_bytes,
        &receipt_bytes,
    ] {
        lease
            .ingest(bytes, ContentDigest::sha256(bytes), bytes.len() as u64)
            .unwrap();
    }
    lease
        .publish_root(&receipt.root(), &receipt.references())
        .unwrap();
    drop(lease);
    let view = materialize_artifact(&store, &receipt.root(), receipt.digest()).unwrap();
    let link = view.directory().unwrap().join("usr/bin/ld.so");
    assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(fs::read(link).unwrap(), b"loader bytes");
}

#[test]
fn exclusive_maintenance_reclaims_views_without_dropping_retained_results() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let result = {
        let lease = store.operation().unwrap();
        super::super::cache::tests::retained(
            &lease,
            &super::super::cache::tests::action(),
            b"hello",
        )
    };
    let view = materialize_artifact(&store, &result.root, result.receipt).unwrap();
    let physical = view.directory().unwrap();
    assert!(matches!(store.maintenance(), Err(StoreError::Busy)));
    drop(view);
    assert_eq!(
        clear_materializations(&mut store.maintenance().unwrap()).unwrap(),
        1
    );
    assert!(!physical.exists());
    let rebuilt = materialize_artifact(&store, &result.root, result.receipt).unwrap();
    assert!(!rebuilt.reused());
}

#[test]
fn materialization_rejects_a_missing_retained_source_even_with_a_valid_artifact() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path()).unwrap();
    let result = {
        let lease = store.operation().unwrap();
        super::super::cache::tests::retained(
            &lease,
            &super::super::cache::tests::action(),
            b"hello",
        )
    };
    let source = ContentDigest::sha256(b"fixture source").to_string();
    fs::remove_file(
        temporary
            .path()
            .join("objects/sha256")
            .join(&source[..2])
            .join(source),
    )
    .unwrap();
    assert!(materialize_artifact(&store, &result.root, result.receipt).is_err());
}
