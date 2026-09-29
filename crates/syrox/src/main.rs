use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use syrox_engine::{
    CheckConfiguration, CheckFailure, LockStatus, ProjectOperationError, check_path_with,
    check_project_lock_with, lock_project_with, plan_project_with,
};

mod build;
mod build_signals;
#[cfg(target_os = "linux")]
#[path = "../elf_release.rs"]
mod elf_release;
mod fetch;
mod host;
mod inspect_source;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod install_self;
#[cfg(target_os = "linux")]
mod lsp;
mod recover_builds;
mod reference;
mod reindex_builds;
mod run;
mod standard_library;
mod store_ops;
mod worker_path;

#[derive(Debug, Parser)]
#[command(
    name = "srx",
    version,
    about = "Syrox package manager",
    arg_required_else_help = true
)]
struct Cli {
    /// Select the authenticated standard library.
    #[arg(long = "std", value_enum, default_value_t = StandardLibrarySelection::Bundled, global = true)]
    standard_library: StandardLibrarySelection,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum StandardLibrarySelection {
    Bundled,
    None,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve editor diagnostics and navigation over LSP stdio.
    #[cfg(target_os = "linux")]
    Lsp { path: Option<PathBuf> },
    /// Install this release and its embedded worker without host utilities.
    #[cfg(target_os = "linux")]
    InstallSelf(install_self::Arguments),
    /// Inspect native Linux capabilities and candidate cgroup delegation.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Build a declared package with automatic source acquisition and retention.
    Build(build::Arguments),
    /// Run a declared application with its verified runtime closure.
    Run(run::Arguments),
    /// Inspect a locked build reference and its origin without realizing it.
    Info(build::ReferenceArguments),
    /// Project checking, locking, planning and explicit source inspection.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Store build-operation maintenance.
    Store {
        #[command(subcommand)]
        command: StoreCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Check the selected project without effects.
    Check {
        path: Option<PathBuf>,
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
    },
    /// Create or verify its Syrox.lock.
    Lock {
        path: Option<PathBuf>,
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    /// Print the pure, locked project plan.
    Plan {
        path: Option<PathBuf>,
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
    },
    /// Explicitly acquire a pinned source (normally handled by build).
    Fetch {
        path: PathBuf,
        package: String,
        source_index: usize,
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        root: String,
        #[arg(long)]
        allow_https: String,
    },
    /// Inspect a verified source archive without extracting it.
    InspectSource {
        path: PathBuf,
        package: String,
        source_index: usize,
        #[arg(long)]
        store: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum StoreCommand {
    /// Inspect configured Store and build-host capabilities without building.
    Doctor(store_ops::StoreOptions),
    /// Preview or collect unretained Store objects.
    Gc(store_ops::GcArguments),
    /// Recover orphaned build operations after confirmed settlement.
    Recover(recover_builds::Arguments),
    /// Rebuild the action index from retained results.
    Reindex(reindex_builds::Arguments),
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// Inspect kernel and cgroup v2 capabilities; active probes are opt-in.
    Inspect(host::Arguments),
    /// Internal, gated process used exclusively for the cgroup migration probe.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    ProbeChild,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let configuration = standard_library::configuration(matches!(
        cli.standard_library,
        StandardLibrarySelection::Bundled
    ));
    match cli.command {
        #[cfg(target_os = "linux")]
        Command::Lsp { path } => lsp::run(path, configuration),
        #[cfg(target_os = "linux")]
        Command::InstallSelf(arguments) => install_self::run(&arguments),
        Command::Host { command } => match command {
            HostCommand::Inspect(arguments) => host::inspect(&arguments),
            #[cfg(target_os = "linux")]
            HostCommand::ProbeChild => host::probe_child(),
        },
        Command::Build(arguments) => build::run(&arguments, &configuration),
        Command::Run(arguments) => run::run(&arguments, &configuration),
        Command::Info(arguments) => build::info(&arguments, &configuration),
        Command::Project { command } => project_command(command, &configuration),
        Command::Store { command } => match command {
            StoreCommand::Doctor(args) => store_ops::doctor(&args),
            StoreCommand::Gc(args) => store_ops::gc(&args),
            StoreCommand::Recover(args) => recover_builds::run(&args),
            StoreCommand::Reindex(args) => reindex_builds::run(&args),
        },
    }
}

fn project_command(command: ProjectCommand, checks: &CheckConfiguration) -> ExitCode {
    match command {
        ProjectCommand::Check { path, file } => {
            match reference::project(file.as_deref(), path.as_deref()) {
                Ok(path) => check(&path, checks),
                Err(error) => usage_error(&error),
            }
        }
        ProjectCommand::Lock {
            path,
            file,
            check: only,
        } => match reference::project(file.as_deref(), path.as_deref()) {
            Ok(path) => lock(&path, only, checks),
            Err(error) => usage_error(&error),
        },
        ProjectCommand::Plan { path, file } => {
            match reference::project(file.as_deref(), path.as_deref()) {
                Ok(path) => plan(&path, checks),
                Err(error) => usage_error(&error),
            }
        }
        ProjectCommand::Fetch {
            path,
            package,
            source_index,
            store,
            root,
            allow_https,
        } => fetch::run(
            &path,
            &package,
            source_index,
            &store,
            &root,
            &allow_https,
            checks,
        ),
        ProjectCommand::InspectSource {
            path,
            package,
            source_index,
            store,
        } => inspect_source::run(&path, &package, source_index, &store, checks),
    }
}

fn usage_error(error: &str) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(2)
}

fn lock(path: &Path, check_only: bool, configuration: &CheckConfiguration) -> ExitCode {
    let result = if check_only {
        check_project_lock_with(path, configuration)
    } else {
        lock_project_with(path, configuration)
    };
    match result {
        Ok(report) => {
            let state = if check_only {
                "current"
            } else {
                match report.status() {
                    LockStatus::Created => "created",
                    LockStatus::Updated => "updated",
                    LockStatus::Unchanged => "unchanged",
                }
            };
            println!("lock {state}: {}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => render_operation_error(error),
    }
}

fn plan(path: &Path, configuration: &CheckConfiguration) -> ExitCode {
    match plan_project_with(path, configuration) {
        Ok(plan) => {
            print!("{plan}");
            ExitCode::SUCCESS
        }
        Err(error) => render_operation_error(error),
    }
}

fn render_operation_error(error: ProjectOperationError) -> ExitCode {
    match error {
        ProjectOperationError::Check(error) => render_check_error(error),
        error => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn check(path: &Path, configuration: &CheckConfiguration) -> ExitCode {
    match check_path_with(path, configuration) {
        Ok(report) => {
            println!(
                "checked {}: {} top-level declaration(s)",
                report.path.display(),
                report.declarations
            );
            ExitCode::SUCCESS
        }
        Err(error) => render_check_error(error),
    }
}

fn render_check_error(error: CheckFailure) -> ExitCode {
    match error {
        CheckFailure::Diagnostics { input, errors } => {
            for error in errors {
                if let Some(rendered) = error.render_from(&input) {
                    eprint!("{rendered}");
                } else {
                    eprintln!("error: {error}");
                }
            }
            ExitCode::from(1)
        }
        CheckFailure::Evaluation {
            input,
            errors,
            failed_roots,
        } => {
            for error in errors {
                if let Some(rendered) = error.render_from(&input) {
                    eprint!("{rendered}");
                } else {
                    eprintln!("error: {error}");
                }
            }
            if failed_roots.is_empty() {
                eprintln!("error: evaluation failed");
            } else {
                eprintln!(
                    "error: failed evaluation root(s): {}",
                    failed_roots.join(", ")
                );
            }
            ExitCode::from(1)
        }
        error => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}
