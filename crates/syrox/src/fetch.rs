use std::path::Path;
use std::process::ExitCode;

use syrox_engine::{
    CheckConfiguration, HttpsSourceRequest, HttpsTransportPolicy, RootName, Store, acquire_https,
    plan_project_with,
};

/// The exact URL grant is independent of the project lock and digest. Disallow
/// redirects: each new destination would require a new grant from the caller.
pub(super) fn run(
    project: &Path,
    package: &str,
    source_index: usize,
    store_path: &Path,
    root: &str,
    allow_https: &str,
    configuration: &CheckConfiguration,
) -> ExitCode {
    match fetch(
        project,
        package,
        source_index,
        store_path,
        root,
        allow_https,
        configuration,
    ) {
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

fn fetch(
    project: &Path,
    package: &str,
    source_index: usize,
    store_path: &Path,
    root: &str,
    allow_https: &str,
    configuration: &CheckConfiguration,
) -> Result<String, String> {
    let plan = plan_project_with(project, configuration).map_err(|error| error.to_string())?;
    let request = plan
        .acquisitions()
        .find(|acquisition| acquisition.package().as_str() == package)
        .and_then(|acquisition| acquisition.sources().nth(source_index))
        .ok_or_else(|| "package has no acquisition source at that index".to_owned())?;
    if request.url() != allow_https {
        return Err("the exact planned source URL must match --allow-https".to_owned());
    }
    let request = HttpsSourceRequest::new(request.url(), request.digest(), request.maximum_bytes())
        .map_err(|error| error.to_string())?;
    let root = RootName::new(root).map_err(|error| error.to_string())?;
    let store = Store::open(store_path).map_err(|error| error.to_string())?;
    let acquired = acquire_https(
        &store,
        &request,
        &root,
        HttpsTransportPolicy {
            maximum_redirects: 0,
            ..HttpsTransportPolicy::default()
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "fetched {} source {} sha256 {} ({} bytes, cache_hit={}, root={:?})",
        package,
        source_index,
        acquired.object().digest(),
        acquired.object().size(),
        acquired.cache_hit(),
        acquired.root_state()
    ))
}
