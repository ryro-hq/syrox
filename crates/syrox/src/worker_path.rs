use std::path::PathBuf;

/// Installed alongside srx; never resolve an execution helper through PATH.
pub(super) fn installed() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let worker = executable.with_file_name("syrox-worker");
    if !worker.is_file() {
        return Err(format!(
            "missing build worker {}; install srx and syrox-worker together",
            worker.display()
        ));
    }
    Ok(worker)
}
