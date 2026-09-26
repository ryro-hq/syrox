//! Canonical build identities and named-output receipts.
use super::{BuildError, BuildProtocol, BuildSpecification, GLIBC_PROFILE, PROFILE, relative_path};
use crate::{ContentDigest, RootName};
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub(super) const MAX_RECORD_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Action {
    pub protocol: BuildProtocol,
    pub lock: ContentDigest,
    pub package: String,
    pub source: ContentDigest,
    pub toolchain: ContentDigest,
    pub worker: ContentDigest,
    pub directory: String,
    pub entry: String,
    pub timeout: u32,
    pub runtime: Option<ContentDigest>,
    pub development: Option<(ContentDigest, ContentDigest)>,
    pub output_names: Vec<String>,
}

/// The locked provider identity expected by both fresh builds and post-GC
/// consumer lookup. Its receipt digest is already bound in the consumer Action.
pub(super) struct ProviderIdentity<'a> {
    pub lock: &'a [u8; 32],
    pub package: &'a str,
    pub source: ContentDigest,
    pub worker: ContentDigest,
    pub toolchain: ContentDigest,
    pub protocol: BuildProtocol,
    pub directory: &'a str,
    pub entry: &'a str,
    pub deadline: u32,
}

impl ProviderIdentity<'_> {
    pub fn matches(&self, action: &Action) -> bool {
        action.lock.as_bytes() == self.lock
            && action.package == self.package
            && action.source == self.source
            && action.worker == self.worker
            && action.toolchain == self.toolchain
            && action.protocol == self.protocol
            && action.directory == self.directory
            && action.entry == self.entry
            && action.timeout == self.deadline
            && action.runtime.is_none()
            && action.development.is_none()
    }
}

impl Action {
    pub(super) fn new(
        lock: &[u8],
        request: &BuildSpecification,
        source: ContentDigest,
        toolchain: ContentDigest,
        worker: ContentDigest,
        development: Option<(ContentDigest, ContentDigest)>,
    ) -> Self {
        let mut hex = String::with_capacity(64);
        for byte in lock {
            write!(hex, "{byte:02x}").expect("String writes are infallible");
        }
        let lock = hex.parse().expect("locked SHA-256");
        Self {
            protocol: request.protocol(),
            lock,
            package: request.package.clone(),
            source,
            toolchain,
            worker,
            directory: request.source_directory.clone(),
            entry: request.entry.clone(),
            timeout: request.timeout_seconds,
            runtime: request.provider().map(|input| input.receipt),
            development,
            output_names: if request.protocol() == BuildProtocol::Glibc {
                vec!["dev".into(), "out".into()]
            } else {
                vec!["out".into()]
            },
        }
    }

    pub(super) fn encode(&self) -> Vec<u8> {
        let (profile, protocol, configure, install) = match self.protocol {
            BuildProtocol::Glibc => (
                GLIBC_PROFILE,
                "glibc",
                "glibc configure in /work/build --prefix=/syrox/store/<action>/out/usr --disable-werror",
                "glibc install DESTDIR=/out; split runtime and dev; collect named outputs",
            ),
            BuildProtocol::Autotools => (
                PROFILE,
                "autotools",
                "configure --prefix=/syrox/store/<action>/out/usr --disable-nls; declared C build inputs",
                "install DESTDIR=/out; collect /out/syrox/store/<action>/out",
            ),
        };
        let envelope = envelope(self.protocol);
        let mut text = format!(
            "syrox-build-action\nprofile {profile}\nlock {}\npackage {}\nsource {}\ntoolchain {}\nworker {}\ndirectory {}\nentry {}\ntimeout {}\nprotocol {protocol}\nenvelope {envelope}\n",
            self.lock,
            self.package,
            self.source,
            self.toolchain,
            self.worker,
            self.directory,
            self.entry,
            self.timeout
        );
        if let Some(runtime) = self.runtime {
            writeln!(text, "runtime {runtime}").expect("String writes are infallible");
        }
        if let Some((receipt, artifact)) = self.development {
            writeln!(text, "build-input dev {receipt} {artifact}")
                .expect("String writes are infallible");
        }
        writeln!(text, "outputs {}", self.output_names.len())
            .expect("String writes are infallible");
        for name in &self.output_names {
            writeln!(text, "output {name}").expect("String writes are infallible");
        }
        write!(
            text,
            "{configure}\nmake -j2\n{install}\npayload-settlement subreaper-echild\n"
        )
        .expect("String writes are infallible");
        text.into_bytes()
    }

    pub(super) fn digest(&self) -> ContentDigest {
        ContentDigest::sha256(&self.encode())
    }

    pub(super) fn parse(bytes: &[u8]) -> Result<Self, BuildError> {
        let text = record_text(bytes)?;
        let mut lines = text.lines();
        if lines.next() != Some("syrox-build-action") {
            return Err(invalid("unsupported action format"));
        }
        let profile = field(&mut lines, "profile ")?;
        let protocol = match profile {
            GLIBC_PROFILE => BuildProtocol::Glibc,
            PROFILE => BuildProtocol::Autotools,
            _ => return Err(invalid("unsupported action profile")),
        };
        let mut action = Self {
            protocol,
            lock: digest(field(&mut lines, "lock ")?)?,
            package: field(&mut lines, "package ")?.to_owned(),
            source: digest(field(&mut lines, "source ")?)?,
            toolchain: digest(field(&mut lines, "toolchain ")?)?,
            worker: digest(field(&mut lines, "worker ")?)?,
            directory: field(&mut lines, "directory ")?.to_owned(),
            entry: field(&mut lines, "entry ")?.to_owned(),
            timeout: field(&mut lines, "timeout ")?
                .parse()
                .map_err(|_| invalid("invalid action deadline"))?,
            runtime: None,
            development: None,
            output_names: Vec::new(),
        };
        if field(&mut lines, "protocol ")?
            != if protocol == BuildProtocol::Glibc {
                "glibc"
            } else {
                "autotools"
            }
        {
            return Err(invalid("unsupported build protocol"));
        }
        if field(&mut lines, "envelope ")? != envelope(protocol) {
            return Err(invalid("unsupported build envelope"));
        }
        if lines
            .clone()
            .next()
            .is_some_and(|line| line.starts_with("runtime "))
        {
            action.runtime = Some(digest(field(&mut lines, "runtime ")?)?);
        }
        if lines
            .clone()
            .next()
            .is_some_and(|line| line.starts_with("build-input dev "))
        {
            let line = field(&mut lines, "build-input dev ")?;
            let (receipt, artifact) = line
                .split_once(' ')
                .ok_or_else(|| invalid("invalid build input"))?;
            action.development = Some((digest(receipt)?, digest(artifact)?));
        }
        let count: usize = field(&mut lines, "outputs ")?
            .parse()
            .map_err(|_| invalid("invalid action output count"))?;
        if !(1..=9).contains(&count) {
            return Err(invalid("invalid action output count"));
        }
        for _ in 0..count {
            action
                .output_names
                .push(field(&mut lines, "output ")?.to_owned());
        }
        if !crate::plan::valid_package_id(&action.package)
            || !relative_path(&action.directory)
            || action.directory.contains('/')
            || !relative_path(&action.entry)
            || !(1..=if protocol == BuildProtocol::Glibc {
                1800
            } else {
                300
            })
                .contains(&action.timeout)
            || !valid_output_names(&action.output_names, protocol == BuildProtocol::Glibc)
            || (protocol == BuildProtocol::Glibc
                && (action.runtime.is_some() || action.development.is_some()))
            || (action.development.is_some() && action.runtime.is_none())
            || action.encode() != bytes
        {
            return Err(invalid("noncanonical or invalid action"));
        }
        Ok(action)
    }
}

fn envelope(protocol: BuildProtocol) -> String {
    let limits = protocol.envelope();
    format!(
        "memory {} tasks {} cpu-percent {} nofile {} output-bytes {} work-tmpfs {} output-tmpfs {}",
        limits.memory,
        limits.tasks,
        limits.cpu_percent,
        limits.nofile,
        limits.output_bytes,
        limits.work_tmpfs,
        limits.output_tmpfs
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    pub action: ContentDigest,
    pub source: ContentDigest,
    pub toolchain: ContentDigest,
    /// Objects retained from the declared provider's complete result root.
    pub inputs: Vec<ContentDigest>,
    pub outputs: BTreeMap<String, NamedOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NamedOutput {
    pub artifact: ContentDigest,
    pub files: usize,
    /// Empty for data/development outputs without an executable entry.
    pub entry: String,
}

impl Receipt {
    /// The root lists opaque digests; its presence alone does not prove that
    /// every referenced input still exists or matches its named object.
    pub(super) fn verify_retained_inputs(
        &self,
        lease: &crate::OperationLease,
        action: &Action,
        mut check: impl FnMut() -> Result<(), BuildError>,
    ) -> Result<(), BuildError> {
        for (digest, maximum) in [
            (
                self.source,
                if action.protocol == BuildProtocol::Glibc {
                    super::MAX_LARGE_SOURCE_BYTES
                } else {
                    super::MAX_SOURCE_BYTES
                },
            ),
            (self.toolchain, super::cache::MAX_INVENTORY_BYTES),
        ] {
            check()?;
            if lease.open_verified_checked(digest, maximum, || {
                check().map_err(|_| std::io::Error::new(std::io::ErrorKind::Interrupted, "build cancelled"))
            }).map_err(|error| if matches!(error, crate::StoreError::Io(ref io) if io.kind() == std::io::ErrorKind::Interrupted) { BuildError::Cancelled } else { error.into() })?.is_none() {
                return Err(invalid("retained build input is missing"));
            }
        }
        for digest in &self.inputs {
            check()?;
            if lease
                .open_verified_checked(*digest, crate::MAX_STORE_BLOB_BYTES, || {
                    check().map_err(|_| std::io::Error::new(std::io::ErrorKind::Interrupted, "build cancelled"))
                }).map_err(|error| if matches!(error, crate::StoreError::Io(ref io) if io.kind() == std::io::ErrorKind::Interrupted) { BuildError::Cancelled } else { error.into() })?
                .is_none()
            {
                return Err(invalid("retained provider input is missing"));
            }
        }
        Ok(())
    }

    pub(super) fn new(action: &Action, artifact: ContentDigest, files: usize) -> Self {
        Self {
            action: action.digest(),
            source: action.source,
            toolchain: action.toolchain,
            inputs: Vec::new(),
            outputs: BTreeMap::from([(
                "out".into(),
                NamedOutput {
                    artifact,
                    files,
                    entry: action.entry.clone(),
                },
            )]),
        }
    }
    pub(super) fn with_outputs(
        mut self,
        action: &Action,
        outputs: BTreeMap<String, NamedOutput>,
    ) -> Result<Self, BuildError> {
        let names = outputs
            .keys()
            .chain(self.outputs.keys())
            .cloned()
            .collect::<Vec<_>>();
        let mut sorted = names.clone();
        sorted.sort();
        if outputs.is_empty()
            || outputs.len() > 8
            || action.protocol != BuildProtocol::Glibc
            || action.digest() != self.action
            || action.output_names != sorted
            || outputs.iter().any(|(name, output)| {
                !valid_output_name(name) || name == "out" || !valid_output(output)
            })
        {
            return Err(invalid("invalid named output manifest"));
        }
        self.outputs.extend(outputs);
        Ok(self)
    }
    pub(super) fn with_inputs(
        mut self,
        action: &Action,
        inputs: Vec<ContentDigest>,
    ) -> Result<Self, BuildError> {
        self.inputs = inputs;
        if action.digest() != self.action || !self.matches_inputs(action) {
            return Err(invalid("invalid retained build input closure"));
        }
        Ok(self)
    }
    pub(super) fn matches_inputs(&self, action: &Action) -> bool {
        self.inputs.len() <= 32
            && self.inputs.windows(2).all(|pair| pair[0] < pair[1])
            && match action.runtime {
                Some(receipt) => self.inputs.contains(&receipt),
                None => self.inputs.is_empty(),
            }
            && action.development.is_none_or(|(receipt, artifact)| {
                self.inputs.contains(&receipt) && self.inputs.contains(&artifact)
            })
    }
    pub(super) fn matches_action(&self, action: &Action) -> bool {
        self.action == action.digest()
            && self.source == action.source
            && self.toolchain == action.toolchain
            && self.names() == action.output_names
            && self
                .output("out")
                .is_some_and(|out| out.entry == action.entry)
            && self.matches_inputs(action)
    }
    pub(super) fn verify_inputs(
        &self,
        lease: &crate::OperationLease,
        action: &Action,
    ) -> Result<(), BuildError> {
        if !self.matches_inputs(action) {
            return Err(invalid("invalid retained build input closure"));
        }
        let Some(provider_digest) = action.runtime else {
            return Ok(());
        };
        let bytes = lease
            .read_verified(provider_digest, MAX_RECORD_BYTES)?
            .ok_or_else(|| invalid("provider receipt is missing"))?;
        let provider = Self::parse(bytes.as_bytes())?;
        if provider.digest() != provider_digest
            || !provider.inputs.is_empty()
            || provider.references() != self.inputs
            || action.development.is_some_and(|(receipt, artifact)| {
                receipt != provider_digest
                    || provider
                        .output("dev")
                        .is_none_or(|output| output.artifact != artifact)
            })
        {
            return Err(invalid("provider closure disagrees with retained inputs"));
        }
        let bytes = lease
            .read_verified(provider.action, MAX_RECORD_BYTES)?
            .ok_or_else(|| invalid("provider action is missing"))?;
        let provider_action = Action::parse(bytes.as_bytes())?;
        if !provider.matches_action(&provider_action) {
            return Err(invalid("provider action disagrees with receipt"));
        }
        Ok(())
    }
    pub(super) fn output(&self, name: &str) -> Option<&NamedOutput> {
        self.outputs.get(name)
    }
    pub(super) fn names(&self) -> Vec<String> {
        self.outputs.keys().cloned().collect()
    }
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut text = format!(
            "syrox-build-result\naction {}\nsource {}\ntoolchain {}\ninputs {}\n",
            self.action,
            self.source,
            self.toolchain,
            self.inputs.len()
        );
        for digest in &self.inputs {
            writeln!(text, "input {digest}").expect("String writes are infallible");
        }
        writeln!(text, "outputs {}", self.outputs.len()).expect("String writes are infallible");
        for (name, output) in &self.outputs {
            writeln!(
                text,
                "output {name} {} {} {}",
                output.artifact,
                output.files,
                if output.entry.is_empty() {
                    "-"
                } else {
                    &output.entry
                }
            )
            .expect("String writes are infallible");
        }
        text.push_str(
            "payload-settlement subreaper-echild\nsettlement systemd-unit-inactive-or-collected\n",
        );
        text.into_bytes()
    }
    pub(super) fn digest(&self) -> ContentDigest {
        ContentDigest::sha256(&self.encode())
    }
    pub(super) fn root(&self) -> RootName {
        RootName::new(format!("build_{}", self.digest())).expect("receipt-derived root")
    }
    pub(super) fn references(&self) -> Vec<ContentDigest> {
        let mut references = vec![self.action, self.source, self.toolchain, self.digest()];
        references.extend_from_slice(&self.inputs);
        references.extend(self.outputs.values().map(|output| output.artifact));
        references.sort_unstable();
        references.dedup();
        references
    }
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, BuildError> {
        let text = record_text(bytes)?;
        let mut lines = text.lines();
        if lines.next() != Some("syrox-build-result") {
            return Err(invalid("unsupported receipt format"));
        }
        let action = digest(field(&mut lines, "action ")?)?;
        let source = digest(field(&mut lines, "source ")?)?;
        let toolchain = digest(field(&mut lines, "toolchain ")?)?;
        let input_count: usize = field(&mut lines, "inputs ")?
            .parse()
            .map_err(|_| invalid("invalid input count"))?;
        if input_count > 32 {
            return Err(invalid("too many retained inputs"));
        }
        let mut inputs = Vec::with_capacity(input_count);
        for _ in 0..input_count {
            let input = digest(field(&mut lines, "input ")?)?;
            if inputs.last().is_some_and(|previous| *previous >= input) {
                return Err(invalid("unsorted retained inputs"));
            }
            inputs.push(input);
        }
        let count: usize = field(&mut lines, "outputs ")?
            .parse()
            .map_err(|_| invalid("invalid output count"))?;
        if !(1..=9).contains(&count) {
            return Err(invalid("invalid output count"));
        }
        let mut previous = String::new();
        let mut outputs = BTreeMap::new();
        for _ in 0..count {
            let line = field(&mut lines, "output ")?;
            let mut parts = line.split(' ');
            let name = parts.next().ok_or_else(|| invalid("missing output name"))?;
            let output = NamedOutput {
                artifact: digest(
                    parts
                        .next()
                        .ok_or_else(|| invalid("missing output artifact"))?,
                )?,
                files: parts
                    .next()
                    .ok_or_else(|| invalid("missing output file count"))?
                    .parse()
                    .map_err(|_| invalid("invalid output file count"))?,
                entry: parts
                    .next()
                    .ok_or_else(|| invalid("missing output entry"))?
                    .to_owned(),
            };
            let output = NamedOutput {
                entry: if output.entry == "-" {
                    String::new()
                } else {
                    output.entry
                },
                ..output
            };
            if parts.next().is_some()
                || !valid_output_name(name)
                || name <= previous.as_str()
                || !valid_output(&output)
            {
                return Err(invalid("invalid output record"));
            }
            name.clone_into(&mut previous);
            if outputs.insert(name.to_owned(), output).is_some() {
                return Err(invalid("duplicate output record"));
            }
        }
        let receipt = Self {
            action,
            source,
            toolchain,
            inputs,
            outputs,
        };
        if receipt
            .output("out")
            .is_none_or(|out| !relative_path(&out.entry))
            || receipt.outputs.len() != count
            || receipt.encode() != bytes
        {
            return Err(invalid("noncanonical or invalid receipt"));
        }
        Ok(receipt)
    }
}

fn valid_output_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_output_names(names: &[String], multi_output: bool) -> bool {
    names.len() >= if multi_output { 2 } else { 1 }
        && names.len() <= 9
        && names.iter().any(|name| name == "out")
        && (!multi_output || names.iter().any(|name| name == "dev"))
        && names.iter().all(|name| valid_output_name(name))
        && names.windows(2).all(|pair| pair[0] < pair[1])
}

fn valid_output(output: &NamedOutput) -> bool {
    (1..=16384).contains(&output.files)
        && output.entry != "-"
        && (output.entry.is_empty() || relative_path(&output.entry))
}

fn record_text(bytes: &[u8]) -> Result<&str, BuildError> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("oversized build record"));
    }
    std::str::from_utf8(bytes).map_err(|_| invalid("build record is not UTF-8"))
}
fn field<'a>(lines: &mut std::str::Lines<'a>, prefix: &str) -> Result<&'a str, BuildError> {
    lines
        .next()
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or_else(|| invalid("invalid build record field"))
}
fn digest(text: &str) -> Result<ContentDigest, BuildError> {
    text.parse()
        .map_err(|_| invalid("invalid build record digest"))
}
pub(super) fn invalid(reason: &str) -> BuildError {
    BuildError::Cache(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_manifest_is_canonical_and_bound_to_a_distinct_action() {
        let old = super::super::cache::tests::action();
        let mut named = old.clone();
        named.protocol = BuildProtocol::Glibc;
        named.output_names = vec!["dev".into(), "doc".into(), "out".into()];
        assert_ne!(old.digest(), named.digest());
        assert_eq!(Action::parse(&named.encode()).unwrap(), named);
        let extra = NamedOutput {
            artifact: ContentDigest::sha256(b"dev"),
            files: 1,
            entry: "usr/include/header.h".into(),
        };
        let outputs = BTreeMap::from([
            ("dev".into(), extra.clone()),
            (
                "doc".into(),
                NamedOutput {
                    artifact: ContentDigest::sha256(b"doc"),
                    ..extra.clone()
                },
            ),
        ]);
        assert!(
            Receipt::new(&old, ContentDigest::sha256(b"out"), 1)
                .with_outputs(&old, outputs.clone())
                .is_err()
        );
        let receipt = Receipt::new(&named, ContentDigest::sha256(b"out"), 1)
            .with_outputs(&named, outputs)
            .unwrap();
        let bytes = receipt.encode();
        assert_eq!(Receipt::parse(&bytes).unwrap(), receipt);
        let swapped = String::from_utf8(bytes.clone())
            .unwrap()
            .replace("output dev ", "output TEMP ")
            .replace("output doc ", "output dev ")
            .replace("output TEMP ", "output doc ");
        assert!(Receipt::parse(swapped.as_bytes()).is_err());
        assert!(Receipt::parse(&bytes[..bytes.len() - 1]).is_err());
        let mut invalid_action = named.clone();
        invalid_action.output_names.swap(0, 1);
        assert!(Action::parse(&invalid_action.encode()).is_err());
        invalid_action.output_names = vec!["../escape".into()];
        assert!(Action::parse(&invalid_action.encode()).is_err());
    }

    #[test]
    fn development_input_changes_action_without_changing_provider_result() {
        let old = super::super::cache::tests::action();
        let mut runtime = old.clone();
        runtime.runtime = Some(ContentDigest::sha256(b"glibc result"));
        let mut supplied = runtime.clone();
        supplied.development = Some((
            runtime.runtime.unwrap(),
            ContentDigest::sha256(b"glibc development artifact"),
        ));
        assert_ne!(runtime.digest(), supplied.digest());
        assert_eq!(Action::parse(&supplied.encode()).unwrap(), supplied);
        let mut other = supplied.clone();
        other.development.as_mut().unwrap().1 = ContentDigest::sha256(b"changed dev");
        assert_ne!(supplied.digest(), other.digest());
        assert!(Action::parse(&other.encode()).is_ok());
    }

    #[test]
    fn provider_identity_checks_the_complete_locked_builder_description() {
        let provider = super::super::cache::tests::action();
        let identity = ProviderIdentity {
            lock: provider.lock.as_bytes(),
            package: &provider.package,
            source: provider.source,
            worker: provider.worker,
            toolchain: provider.toolchain,
            protocol: provider.protocol,
            directory: &provider.directory,
            entry: &provider.entry,
            deadline: provider.timeout,
        };
        assert!(identity.matches(&provider));
        for changed in [
            Action {
                source: ContentDigest::sha256(b"other source"),
                ..provider.clone()
            },
            Action {
                timeout: provider.timeout + 1,
                ..provider.clone()
            },
            Action {
                runtime: Some(ContentDigest::sha256(b"nested provider")),
                ..provider.clone()
            },
        ] {
            assert!(!identity.matches(&changed));
        }
    }
}
