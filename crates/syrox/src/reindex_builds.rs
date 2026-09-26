use std::path::PathBuf;
use std::process::ExitCode;
use syrox_engine::{Store, UserConfiguration, rebuild_build_index};

#[derive(Debug, clap::Args)]
pub(super) struct Arguments {
    /// Explicit operational configuration instead of the global default.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the Store containing the retained build results.
    #[arg(long)]
    store: Option<PathBuf>,
}

pub(super) fn run(arguments: &Arguments) -> ExitCode {
    match execute(arguments) {
        Ok(complete) => ExitCode::from(u8::from(!complete)),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn execute(arguments: &Arguments) -> Result<bool, String> {
    let user =
        UserConfiguration::load_with_store(arguments.config.as_deref(), arguments.store.as_deref())
            .map_err(|e| e.to_string())?;
    let store = Store::open(user.store()).map_err(|e| e.to_string())?;
    let report = rebuild_build_index(&store).map_err(|e| e.to_string())?;
    println!(
        "indexed {} retained build actions; skipped {} unsupported receipts; removed {} stale entries",
        report.indexed, report.skipped, report.removed_stale
    );
    for action in &report.conflicts {
        eprintln!("divergent retained results for action {action}");
    }
    Ok(report.conflicts.is_empty())
}
