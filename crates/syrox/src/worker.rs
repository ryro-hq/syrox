//! Internal execution entry point. Deliberately independent of the CLI parser
//! and bundled standard library: CLI changes must not change worker identity.
use std::path::PathBuf;
use std::process::ExitCode;

fn execute() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().ok_or("missing worker mode")?;
    #[cfg(target_os = "linux")]
    if mode == "__runtime" {
        let code = runtime(&args.collect::<Vec<_>>()).map_err(|error| error.to_string())?;
        std::process::exit(code);
    }
    let first = args.next().ok_or("missing worker argument")?;
    let second = args.next().ok_or("missing worker argument")?;
    if mode == "__build-worker" {
        if args.next().is_some() {
            return Err("unexpected worker argument".into());
        }
        syrox_engine::run_build_worker(
            first.to_str().ok_or("invalid source directory")?,
            second.to_str().ok_or("invalid action")?,
        )
        .map_err(|error| error.to_string())
    } else if mode == "__build-gate" {
        let mut blocked = Vec::new();
        while let Some(flag) = args.next() {
            if flag != "--blocked" || blocked.len() >= 1024 {
                return Err("invalid gate argument".into());
            }
            blocked.push(PathBuf::from(args.next().ok_or("missing blocked path")?));
        }
        syrox_engine::run_build_gate(
            &PathBuf::from(first),
            second.to_str().ok_or("invalid source directory")?,
            &blocked,
        )
        .map_err(|error| error.to_string())
    } else {
        Err("invalid worker mode".into())
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn runtime(args: &[std::ffi::OsString]) -> std::io::Result<i32> {
    // SAFETY: this dedicated executable has not started threads or retained
    // borrowed global locks. Channels inherited from the coordinator have no
    // Rust owners yet; the helper takes ownership and validates distinctness.
    unsafe { syrox_engine::runtime_helper(args) }
}

fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
