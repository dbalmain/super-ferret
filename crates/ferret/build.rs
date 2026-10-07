//! Semantic build identity shared by the CLI and ferretd, including workspace
//! Rust sources. A version number alone cannot distinguish unreleased builds.
use std::hash::{Hash, Hasher};
use std::path::Path;

fn sources(path: &Path, hash: &mut impl Hasher) -> std::io::Result<()> {
    let mut paths = std::fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    for path in paths {
        if path.is_dir() {
            sources(&path, hash)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            std::fs::read(path)?.hash(hash);
        }
    }
    Ok(())
}
fn main() -> std::io::Result<()> {
    let root = Path::new("../..");
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut crates = std::fs::read_dir(root.join("crates"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    crates.sort();
    for path in crates {
        let src = path.join("src");
        if src.is_dir() {
            println!("cargo:rerun-if-changed={}", src.display());
            sources(&src, &mut hash)?;
        }
    }
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    std::fs::read(root.join("Cargo.lock"))?.hash(&mut hash);
    println!("cargo:rustc-env=FERRET_BUILD={:016x}", hash.finish());
    Ok(())
}
