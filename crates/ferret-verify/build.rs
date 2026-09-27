// Records the compiling rustc's version so the toolchain ledger
// (`src/toolchain.rs`) can fail its test when the toolchain moves.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=FERRET_VERIFY_RUSTC={}", version.trim());
    println!("cargo:rerun-if-env-changed=RUSTC");
}
