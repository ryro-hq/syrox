use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use syrox_engine::CheckConfiguration;
#[cfg(target_os = "linux")]
use syrox_engine::UserConfiguration;

#[derive(Debug, clap::Args)]
pub(super) struct Arguments {
    /// Catalog application, .#export, or locked project (default: current project).
    reference: Option<String>,
    /// Select a project's main.srx.
    #[arg(short = 'f', long)]
    file: Option<PathBuf>,
    /// Explicit user configuration.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the configured Store.
    #[arg(long)]
    store: Option<PathBuf>,
    /// Require all build sources in the verified local cache.
    #[arg(short = 'o', long)]
    offline: bool,
    /// Arguments for the application (after --).
    #[arg(last = true, allow_hyphen_values = true)]
    argv: Vec<OsString>,
}

pub(super) fn run(args: &Arguments, checks: &CheckConfiguration) -> ExitCode {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (args, checks);
        eprintln!("error: runtime is unsupported on this platform");
        ExitCode::from(1)
    }
    #[cfg(target_os = "linux")]
    {
        match execute(args, checks) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("error: {message}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn execute(args: &Arguments, checks: &CheckConfiguration) -> Result<ExitCode, String> {
    use std::os::unix::process::ExitStatusExt as _;
    use syrox_engine::{
        ApplicationError, BuildError, RealizeError, RuntimeError, realize_application,
        resolve_application, run_runtime_with_cancellation,
    };

    let user = UserConfiguration::load_with_store(args.config.as_deref(), args.store.as_deref())
        .map_err(|e| e.to_string())?;
    let reference = super::reference::select(args.reference.as_deref(), args.file.as_deref())?;
    let app = resolve_application(&reference, &user, checks).map_err(|e| e.to_string())?;
    let signals = super::build_signals::BuildSignals::install().map_err(|e| e.to_string())?;
    let worker = crate::worker_path::installed()?;
    let realized = realize_application(
        &app,
        &user,
        &worker,
        args.offline,
        &signals.cancellation,
        |_, _| {},
    );
    let closure = match realized {
        Ok(closure) => closure,
        Err(
            ApplicationError::Resolve(RealizeError::Build(BuildError::Cancelled))
            | ApplicationError::Runtime(RuntimeError::Cancelled),
        ) => {
            return Ok(ExitCode::from(signals.cancelled_exit_code()));
        }
        Err(error) => return Err(error.to_string()),
    };
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let status =
        match run_runtime_with_cancellation(&closure, &args.argv, &cwd, &signals.cancellation) {
            Ok(status) => status,
            Err(RuntimeError::Cancelled) => {
                return Ok(ExitCode::from(signals.cancelled_exit_code()));
            }
            Err(error) => return Err(error.to_string()),
        };
    if signals.cancellation.is_cancelled() {
        return Ok(ExitCode::from(signals.cancelled_exit_code()));
    }
    let code = status
        .code()
        .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal));
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}
