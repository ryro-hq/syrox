use std::path::Path;
use std::process::ExitCode;

use syrox_engine::{
    ArchiveFormat, ArchiveLimits, CheckConfiguration, Store, inspect_archive,
    inspect_source_archive, plan_project_with,
};

pub(super) fn run(
    project: &Path,
    package: &str,
    source_index: usize,
    store_path: &Path,
    configuration: &CheckConfiguration,
) -> ExitCode {
    match inspect(project, package, source_index, store_path, configuration) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn inspect(
    project: &Path,
    package: &str,
    source_index: usize,
    store_path: &Path,
    configuration: &CheckConfiguration,
) -> Result<String, String> {
    let plan = plan_project_with(project, configuration).map_err(|error| error.to_string())?;
    let source = plan
        .acquisitions()
        .find(|acquisition| acquisition.package().as_str() == package)
        .and_then(|acquisition| acquisition.sources().nth(source_index))
        .ok_or_else(|| "package has no acquisition source at that index".to_owned())?;
    let store = Store::open(store_path).map_err(|error| error.to_string())?;
    let inventory = if plan
        .builds()
        .any(|build| build.package().as_str() == package && build.protocol() == "glibc")
    {
        let mut limits = ArchiveLimits::large_xz();
        limits.compressed_bytes = limits.compressed_bytes.min(source.maximum_bytes());
        inspect_archive(&store, source.digest(), ArchiveFormat::XzTar, limits)
    } else {
        inspect_source_archive(&store, source.digest(), source.maximum_bytes())
    }
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "source {}:{} sha256 {}: {} entries, {} regular files, {} expanded file bytes",
        package,
        source_index,
        source.digest(),
        inventory.entries(),
        inventory.files(),
        inventory.expanded_bytes()
    ))
}
