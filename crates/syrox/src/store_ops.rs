use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use syrox_engine::clear_materializations;
use syrox_engine::{GcRequest, Store, StoreError, UserConfiguration, check_build_host};

#[derive(Debug, clap::Args)]
pub(super) struct StoreOptions {
    /// Explicit operational configuration instead of the global default.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the selected Store.
    #[arg(long)]
    store: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub(super) struct GcArguments {
    #[command(flatten)]
    options: StoreOptions,
    /// Collect unretained objects after the dry-run report.
    #[arg(long)]
    apply: bool,
}

fn user(options: &StoreOptions) -> Result<UserConfiguration, String> {
    UserConfiguration::load_with_store(options.config.as_deref(), options.store.as_deref())
        .map_err(|e| e.to_string())
}

fn render(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

pub(super) fn gc(args: &GcArguments) -> ExitCode {
    render((|| {
        let user = user(&args.options)?;
        let store = Store::open(user.store()).map_err(|e| e.to_string())?;
        let mut maintenance = store.maintenance().map_err(|e| e.to_string())?;
        let report = maintenance
            .garbage_collect(&GcRequest {
                collect: args.apply,
                ..GcRequest::default()
            })
            .map_err(|e| e.to_string())?;
        println!(
            "{} unretained object(s), {} bytes {}",
            report.candidates.len(),
            report.candidate_bytes,
            if args.apply {
                "collected"
            } else {
                "collectable (use --apply to collect)"
            }
        );
        if report.uncertain || !report.failures.is_empty() {
            return Err(format!(
                "garbage collection left {} failure(s); inspect Store before retry",
                report.failures.len()
            ));
        }
        #[cfg(target_os = "linux")]
        if args.apply {
            let views = clear_materializations(&mut maintenance).map_err(|e| e.to_string())?;
            if views != 0 {
                println!("{views} reconstructible view(s) reclaimed");
            }
        }
        Ok(())
    })())
}

pub(super) fn doctor(options: &StoreOptions) -> ExitCode {
    render((|| {
        let user = user(options)?;
        println!("store {}", user.store().display());
        match Store::open(user.store()) {
            Ok(_) => println!("store: available"),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                println!("store: not initialized (first build will create it)");
            }
            Err(error) => return Err(format!("store: {error}")),
        }
        let toolchain = user
            .host_toolchain()
            .ok_or_else(|| "build.host-toolchain is not configured".to_owned())?;
        check_build_host(toolchain).map_err(|e| e.to_string())?;
        println!("build host: ready ({})", toolchain.display());
        Ok(())
    })())
}
