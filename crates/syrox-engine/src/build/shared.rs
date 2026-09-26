//! Store-local single producer, with independently leased consumer interests.
//! The first caller coordinates the producer, even after detaching its own
//! interest. No PID, journal outcome, or lock is authority for a cache hit.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::journal::Operation;
use super::records::Action;
use super::{BuildCancellation, BuildError, BuildExecution, BuildResult, cache, journal};
use crate::linux_fd::{self as fd, FlockMode, OpenedPath};
use crate::{OperationLease, RootName};

pub(super) const CLOSING: &[u8] = b"syrox-build-closing\n";
const POLL: Duration = Duration::from_millis(25);
const WAIT_SETTLEMENT_MARGIN: Duration = Duration::from_mins(2);

struct Consumer {
    name: String,
    directory: OpenedPath,
    parent: OpenedPath,
    interest: Option<OpenedPath>,
    _observer: OpenedPath,
}

impl Consumer {
    fn join(parent: &OpenedPath, directory: OpenedPath, name: String) -> Result<Self, BuildError> {
        let interest = shared_lock(&directory, "consumers")?;
        let observer = shared_lock(&directory, "observers")?;
        Ok(Self {
            name,
            directory,
            parent: journal::directory(parent, ".")?,
            interest: Some(interest),
            _observer: observer,
        })
    }

    /// Admission and the last-interest transition use the same namespace lock.
    /// A late caller cannot join a producer that has already begun cancellation.
    fn detach(&mut self) -> Result<bool, BuildError> {
        let _mutation = journal::lock_namespace(&self.parent)?;
        self.interest.take();
        if journal::read(&self.directory, "closing", 64)?.is_some() {
            return Ok(true);
        }
        if journal::lock_existing(&self.directory, "consumers")?.is_some() {
            journal::write(&self.directory, "closing", CLOSING)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn failure(&self, reason: impl Into<String>) -> BuildError {
        BuildError::SharedProducer {
            operation: self.name.clone(),
            reason: reason.into(),
        }
    }

    fn wait(
        mut self,
        lease: &OperationLease,
        action: &Action,
        root: Option<&RootName>,
        cancellation: &BuildCancellation,
    ) -> Result<BuildResult, BuildError> {
        // A shared consumer must be allowed to wait for the selected action's
        // full build deadline, including service shutdown and publication.
        let deadline = Instant::now()
            + Duration::from_secs(u64::from(action.timeout))
            + WAIT_SETTLEMENT_MARGIN;
        let mut cancelled = false;
        loop {
            if !cancelled && cancellation.is_cancelled() {
                if !self.detach()? {
                    return Err(BuildError::Cancelled);
                }
                // The last consumer waits for confirmed settlement, retaining an
                // observer lease so recovery cannot erase its evidence meanwhile.
                cancelled = true;
            }
            if let Some(_coordinator) = journal::lock_existing(&self.directory, "lease")? {
                if cancelled
                    && journal::read(&self.directory, "cleaned", 64)?.as_deref()
                        == Some(b"syrox-build-cleaned\n")
                {
                    return Err(BuildError::Cancelled);
                }
                if !cancelled
                    && let Some(mut result) = cache::lookup(lease, action, root, cancellation)?
                {
                    result.execution = BuildExecution::Shared {
                        operation: self.name.clone(),
                    };
                    return Ok(result);
                }
                let outcome = journal::read(&self.directory, "outcome", 4096)?;
                let diagnostic = outcome.as_deref().map_or_else(
                    || "coordinator disappeared; run srx store recover".into(),
                    |bytes| String::from_utf8_lossy(bytes).into_owned(),
                );
                return Err(self.failure(format!(
                    "ended without a reusable result or confirmed cancellation: {diagnostic}"
                )));
            }
            if Instant::now() >= deadline {
                self.detach()?;
                return Err(
                    self.failure("consumer wait exceeded action deadline; settlement unconfirmed")
                );
            }
            std::thread::sleep(POLL);
        }
    }
}

fn shared_lock(directory: &OpenedPath, name: &str) -> Result<OpenedPath, BuildError> {
    let file = fd::open_existing_regular(directory.fd(), Path::new(name))?;
    if file.metadata().size() != 0 || !fd::flock(file.fd(), FlockMode::Shared)? {
        return Err(BuildError::Journal("consumer lease unavailable".into()));
    }
    Ok(file)
}

#[derive(Debug)]
pub(super) struct Interest {
    requested: BuildCancellation,
    consumer: Mutex<InterestState>,
}

#[derive(Debug)]
struct InterestState {
    consumer: Consumer,
    checked: Option<Instant>,
    stopped: bool,
}

impl std::fmt::Debug for Consumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consumer")
            .field("operation", &self.name)
            .field("interested", &self.interest.is_some())
            .finish_non_exhaustive()
    }
}

impl Interest {
    pub(super) fn check(&self) -> Result<(), BuildError> {
        self.check_interest(false)
    }

    pub(super) fn check_publication(&self) -> Result<(), BuildError> {
        self.check_interest(true)
    }

    fn check_interest(&self, publication: bool) -> Result<(), BuildError> {
        if !self.requested.is_cancelled() {
            return Ok(());
        }
        let mut state = self
            .consumer
            .lock()
            .map_err(|_| BuildError::Journal("consumer state poisoned".into()))?;
        if state.stopped {
            return Err(BuildError::Cancelled);
        }
        // Hashing /usr checks cancellation at every chunk. Once detached, probe
        // other interests at most once per tick, but always at the commit boundary.
        if !publication && state.checked.is_some_and(|last| last.elapsed() < POLL) {
            return Ok(());
        }
        state.stopped = state.consumer.detach()?;
        state.checked = Some(Instant::now());
        if state.stopped {
            Err(BuildError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn detached(&self) -> bool {
        self.consumer
            .lock()
            .expect("producer interest not poisoned")
            .consumer
            .interest
            .is_none()
    }
}

enum Admission {
    Ready(BuildResult),
    Follow(Consumer),
    Produce(Operation, Consumer),
}

fn admit(
    lease: &OperationLease,
    action: &Action,
    root: Option<&RootName>,
    cancellation: &BuildCancellation,
) -> Result<Admission, BuildError> {
    let started = Instant::now();
    let parent = lease.build_directory()?;
    let _mutation = journal::lock_namespace(&parent)?;
    let acquired = Instant::now();
    let _profile = AdmissionLockProfile {
        acquired,
        wait: acquired.duration_since(started),
    };
    cancellation.check()?;
    // A producer may have committed between the first lookup and admission.
    if let Some(result) = cache::lookup(lease, action, root, cancellation)? {
        return Ok(Admission::Ready(result));
    }
    let reference = format!("syrox-build-action-reference\n{}\n", action.digest());
    for name in journal::entries(&parent, journal::MAX_OPERATIONS)? {
        if !journal::valid_name(&name, "op-") {
            continue;
        }
        let directory = journal::directory(&parent, &name)?;
        if journal::read(&directory, "action", 256)?.as_deref() != Some(reference.as_bytes()) {
            continue;
        }
        if journal::validate_identity(&parent, &directory, &name)? != journal::boot_id()? {
            return Err(BuildError::SharedProducer {
                operation: name,
                reason: "previous-boot operation; run srx store recover".into(),
            });
        }
        if journal::lock_existing(&directory, "lease")?.is_none() {
            if journal::read(&directory, "closing", 64)?.is_some() {
                return Err(BuildError::SharedProducer {
                    operation: name,
                    reason: "last consumer already cancelled; retry after finalization".into(),
                });
            }
            return Ok(Admission::Follow(Consumer::join(&parent, directory, name)?));
        }
        if journal::read(&directory, "cleaned", 64)?.as_deref() != Some(b"syrox-build-cleaned\n") {
            return Err(BuildError::SharedProducer {
                operation: name,
                reason: "orphaned or unsettled operation; run srx store recover".into(),
            });
        }
    }
    let operation = Operation::create_locked(&parent)?;
    // Bind before leaving admission, and before any launch authorization. An
    // abrupt death from this point cannot lead to an overlapping replacement.
    operation.bind(action.digest())?;
    let consumer = Consumer::join(
        &parent,
        journal::directory(&parent, &operation.name)?,
        operation.name.clone(),
    )?;
    Ok(Admission::Produce(operation, consumer))
}

struct AdmissionLockProfile {
    acquired: Instant,
    wait: Duration,
}

impl Drop for AdmissionLockProfile {
    fn drop(&mut self) {
        if std::env::var_os("SYROX_PROFILE_LOCKS").is_some() {
            eprintln!(
                "syrox-lock producer-admission wait_us={} held_us={}",
                self.wait.as_micros(),
                self.acquired.elapsed().as_micros()
            );
        }
    }
}

pub(super) fn realize(
    lease: &OperationLease,
    action: &Action,
    root: Option<&RootName>,
    cancellation: &BuildCancellation,
    produce: impl FnOnce(&Operation, &BuildCancellation) -> Result<BuildResult, BuildError>,
) -> Result<BuildResult, BuildError> {
    let (operation, consumer) = match admit(lease, action, root, cancellation)? {
        Admission::Ready(result) => return Ok(result),
        Admission::Follow(consumer) => return consumer.wait(lease, action, root, cancellation),
        Admission::Produce(operation, consumer) => (operation, consumer),
    };
    let interest = Arc::new(Interest {
        requested: cancellation.clone(),
        consumer: Mutex::new(InterestState {
            consumer,
            checked: None,
            stopped: false,
        }),
    });
    let producer = BuildCancellation::producer(Arc::clone(&interest));
    let result = operation.finish(produce(&operation, &producer))?;
    cache::record(lease, action, &result).map_err(|source| BuildError::PublishedAndCache {
        root: result.root.clone(),
        source: Box::new(source),
    })?;
    if interest.detached() {
        return Err(BuildError::Cancelled);
    }
    if let Some(root) = root {
        // Per-consumer retention is separate from the shared producer's managed
        // root. A caller's root conflict cannot prevent another caller's success.
        let mut retained = cache::lookup(lease, action, Some(root), cancellation)?
            .ok_or_else(|| BuildError::Cache("committed producer result disappeared".into()))?;
        retained.execution = result.execution;
        return Ok(retained);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
