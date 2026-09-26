#[cfg(target_os = "linux")]
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use syrox_engine::{HostCapabilityStatus, inspect_host_with_probe};

#[derive(Debug, clap::Args)]
pub(super) struct Arguments {
    /// Inspect a selected cgroup parent beneath /sys/fs/cgroup; defaults to this process's membership.
    #[arg(long)]
    cgroup_parent: Option<PathBuf>,
    /// Probe resource limits and migration with a temporary cgroup and a gated srx child.
    #[arg(long)]
    probe_cgroup: bool,
}

#[cfg(target_os = "linux")]
pub(super) fn probe_child() -> ExitCode {
    // The parent waits for this byte before writing cgroup.procs; this process
    // cannot run a recipe and remains alive until its stdin is closed.
    let result = (|| -> std::io::Result<()> {
        std::io::stdout().write_all(b"R")?;
        std::io::stdout().flush()?;
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte)
    })();
    if result.is_err() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

pub(super) fn inspect(args: &Arguments) -> ExitCode {
    match inspect_host_with_probe(args.cgroup_parent.as_deref(), args.probe_cgroup) {
        Ok(report) => {
            println!("cgroup parent {}", report.cgroup_parent.display());
            for item in report.capabilities {
                match item.status {
                    HostCapabilityStatus::Available(detail) => {
                        println!("{}: available ({detail})", item.name);
                    }
                    HostCapabilityStatus::Unavailable(detail) => {
                        println!("{}: unavailable ({detail})", item.name);
                    }
                    HostCapabilityStatus::NeedsActiveProbe(detail) => {
                        println!("{}: requires active probe ({detail})", item.name);
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}
