use std::path::PathBuf;
use std::process::ExitCode;
use syrox_engine::{BuildRecoveryState, Store, UserConfiguration, recover_builds};

#[derive(Debug, clap::Args)]
pub(super) struct Arguments {
    /// Explicit operational configuration instead of the global default.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the configured Store containing the build operations.
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
    let report = recover_builds(&store).map_err(|e| e.to_string())?;
    let mut complete = true;
    for entry in &report.entries {
        match &entry.state {
            BuildRecoveryState::Active => println!("active {}", entry.operation),
            BuildRecoveryState::Recovered => println!("recovered {}", entry.operation),
            BuildRecoveryState::Retained(reason) => {
                eprintln!("retained {}: {reason}", entry.operation);
                complete = false;
            }
        }
    }
    if report.entries.is_empty() {
        println!("no build operations to recover");
    }
    Ok(complete)
}
