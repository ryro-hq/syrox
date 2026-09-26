use super::super::cache::tests::{action, retained};
use super::*;
use crate::{BuildRecoveryState, Store};
use std::sync::mpsc;

#[test]
fn cancelling_either_consumer_keeps_the_single_producer_for_the_other() {
    for cancel_producer_consumer in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let lease = store.operation().unwrap();
        let action = action();
        let first = BuildCancellation::default();
        let second = BuildCancellation::default();
        let (started, ready) = mpsc::channel();
        let (resume, resume_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let (producer_lease, description, request) = (&lease, &action, &first);
            let build = scope.spawn(move || {
                realize(
                    producer_lease,
                    description,
                    None,
                    request,
                    |operation, cancellation| {
                        started.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        cancellation.check_publication()?;
                        operation.clean_stage()?;
                        Ok(retained(producer_lease, description, b"one producer"))
                    },
                )
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let Admission::Follow(consumer) = admit(&lease, &action, None, &second).unwrap() else {
                panic!("must join active producer");
            };
            let root = RootName::new("second_consumer").unwrap();
            if cancel_producer_consumer {
                first.cancel();
                resume.send(()).unwrap();
                let result = consumer
                    .wait(&lease, &action, Some(&root), &second)
                    .unwrap();
                assert!(matches!(result.execution, BuildExecution::Shared { .. }));
                assert_eq!(result.root, root);
                assert!(matches!(build.join().unwrap(), Err(BuildError::Cancelled)));
            } else {
                second.cancel();
                assert!(matches!(
                    consumer.wait(&lease, &action, Some(&root), &second),
                    Err(BuildError::Cancelled)
                ));
                resume.send(()).unwrap();
                assert!(build.join().unwrap().is_ok());
                assert!(
                    !directory
                        .path()
                        .join("roots/retained")
                        .join(root.as_str())
                        .exists()
                );
            }
        });
        assert_eq!(
            journal::entries(&lease.build_directory().unwrap(), 10)
                .unwrap()
                .len(),
            1
        );
        assert!(
            cache::lookup(&lease, &action, None, &BuildCancellation::default())
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn last_consumer_waits_for_settlement_and_closing_rejects_late_admission() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let lease = store.operation().unwrap();
    let action = action();
    let first = BuildCancellation::default();
    let second = BuildCancellation::default();
    let (started, ready) = mpsc::channel();
    let (resume, resume_rx) = mpsc::channel();
    let (stopping, stopped) = mpsc::channel();
    let (detached, detached_rx) = mpsc::channel();
    let (settle, settle_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let (producer_lease, description, request) = (&lease, &action, &first);
        let build = scope.spawn(move || {
            realize(
                producer_lease,
                description,
                None,
                request,
                |operation, cancellation| {
                    started.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    cancellation.check().unwrap();
                    detached.send(()).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while cancellation.check().is_ok() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(POLL);
                    }
                    stopping.send(()).unwrap();
                    settle_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    operation.clean_stage()?;
                    Err(BuildError::Cancelled)
                },
            )
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let Admission::Follow(consumer) = admit(&lease, &action, None, &second).unwrap() else {
            panic!("must follow");
        };
        first.cancel();
        resume.send(()).unwrap();
        detached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        second.cancel();
        let follower = scope.spawn(|| consumer.wait(&lease, &action, None, &second));
        stopped.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!follower.is_finished());
        assert!(matches!(
            admit(&lease, &action, None, &BuildCancellation::default()),
            Err(BuildError::SharedProducer { .. })
        ));
        assert_eq!(
            journal::recover(&store).unwrap().entries[0].state,
            BuildRecoveryState::Active
        );
        settle.send(()).unwrap();
        assert!(matches!(build.join().unwrap(), Err(BuildError::Cancelled)));
        assert!(matches!(
            follower.join().unwrap(),
            Err(BuildError::Cancelled)
        ));
    });
    assert!(
        cache::lookup(&lease, &action, None, &BuildCancellation::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        journal::recover(&store).unwrap().entries[0].state,
        BuildRecoveryState::Recovered
    );
}

#[test]
fn orphaned_producer_cannot_be_replaced_until_recovery_and_observers_block_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let lease = store.operation().unwrap();
    let action = action();
    let cancel = BuildCancellation::default();
    let Admission::Produce(operation, consumer) = admit(&lease, &action, None, &cancel).unwrap()
    else {
        panic!("must produce");
    };
    drop(operation);
    assert!(matches!(
        admit(&lease, &action, None, &cancel),
        Err(BuildError::SharedProducer { .. })
    ));
    assert_eq!(
        journal::recover(&store).unwrap().entries[0].state,
        BuildRecoveryState::Active
    );
    assert!(matches!(
        consumer.wait(&lease, &action, None, &cancel),
        Err(BuildError::SharedProducer { .. })
    ));
    assert_eq!(
        journal::recover(&store).unwrap().entries[0].state,
        BuildRecoveryState::Recovered
    );
    assert!(matches!(
        admit(&lease, &action, None, &cancel).unwrap(),
        Admission::Produce(_, _)
    ));
}

#[test]
fn consumer_root_conflict_does_not_invalidate_the_shared_committed_result() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let lease = store.operation().unwrap();
    let action = action();
    let root = RootName::new("occupied").unwrap();
    lease.publish_root(&root, &[]).unwrap();
    let result = realize(
        &lease,
        &action,
        Some(&root),
        &BuildCancellation::default(),
        |operation, cancellation| {
            operation.clean_stage()?;
            cancellation.check_publication()?;
            Ok(retained(&lease, &action, b"shared output"))
        },
    );
    assert!(matches!(
        result,
        Err(BuildError::Store(crate::StoreError::RootConflict { .. }))
    ));
    assert!(
        cache::lookup(&lease, &action, None, &BuildCancellation::default())
            .unwrap()
            .is_some()
    );
    assert_eq!(lease.build_references(&root).unwrap(), Some(Vec::new()));
}
