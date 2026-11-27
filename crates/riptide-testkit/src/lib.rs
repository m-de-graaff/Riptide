//! What the workspace's tests share. A dev-dependency only. See README.md in this folder.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The file name of the stand-in title's image.
pub const STAND_IN_TITLE: &str = "stand-in-title.exe";

/// Builds the stand-in title from source, once per test program, and returns the path of
/// its image.
///
/// # Panics
///
/// If the image does not build. The build's output is in the message.
pub fn stand_in_title() -> &'static Path {
    static IMAGE: OnceLock<PathBuf> = OnceLock::new();
    IMAGE.get_or_init(|| {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stand-in-title");
        let target = fixture.join("target");
        let output = Command::new(env!("CARGO"))
            .arg("build")
            .arg("--manifest-path")
            .arg(fixture.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            // The outer cargo's settings are for the workspace, not for this image.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .expect("cargo runs");
        assert!(
            output.status.success(),
            "the stand-in title did not build:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target.join("debug").join(STAND_IN_TITLE)
    })
}

/// The stand-in title's image, built if need be.
pub fn stand_in_title_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| std::fs::read(stand_in_title()).expect("the built image is readable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stand_in_title_builds_into_a_program_image() {
        let bytes = stand_in_title_bytes();
        assert!(bytes.starts_with(b"MZ"));
        assert!(stand_in_title().ends_with(STAND_IN_TITLE));
    }
}
