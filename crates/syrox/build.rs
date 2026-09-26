use std::path::PathBuf;

#[path = "elf_release.rs"]
mod elf_release;

fn main() {
    println!("cargo:rerun-if-env-changed=SYROX_EMBED_WORKER");
    let destination =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("syrox-worker.bin");
    let bytes = if let Some(path) = std::env::var_os("SYROX_EMBED_WORKER") {
        assert_eq!(
            std::env::var("TARGET").unwrap(),
            "x86_64-unknown-linux-musl",
            "embedded releases require the static Linux x86_64 target"
        );
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes =
            std::fs::read(&path).expect("SYROX_EMBED_WORKER must name the built syrox-worker");
        assert!(
            elf_release::static_x86_64(&bytes),
            "embedded worker must have no PT_INTERP or DT_NEEDED"
        );
        bytes
    } else {
        Vec::new()
    };
    std::fs::write(destination, bytes).expect("write embedded worker to OUT_DIR");
}
