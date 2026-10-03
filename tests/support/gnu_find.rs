//! Shared clean-room oracle. Only execute the pinned binary, never consult its
//! source.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

pub fn binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let path = PathBuf::from(std::env::var_os("FERRET_GNU_FIND").unwrap_or_else(|| {
            panic!("GNU oracle missing: run nix develop --command cargo test --workspace")
        }));
        let version = Command::new(&path)
            .arg("--version")
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "cannot execute FERRET_GNU_FIND; enter the repository's Nix dev shell: {error}"
                )
            });
        assert!(version.status.success());
        assert_eq!(
            version.stdout.split(|byte| *byte == b'\n').next(),
            Some(b"find (GNU findutils) 4.11.0".as_slice()),
            "FERRET_GNU_FIND must be GNU findutils 4.11.0"
        );
        path
    })
}

pub fn command() -> Command {
    let mut command = Command::new(binary());
    command.env("LC_ALL", "C").env("TZ", "UTC");
    command
}

pub fn output(command: &mut Command) -> Output {
    command
        .output()
        .unwrap_or_else(|error| panic!("GNU find oracle failed to execute: {error}"))
}
