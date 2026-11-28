//! Compiled shaders kept on disc, by what they were made from (design T4).
//!
//! Translating and compiling a program takes far longer than drawing with it, and a
//! title's programs are the same from run to run. So each compiled shader is kept in a
//! file named by a hash of everything that decided it: the program's bytes, what it was
//! translated for, the translator's version and the compiler. A run that meets the same
//! program again reads the file.
//!
//! The files are in `%LOCALAPPDATA%\Riptide\cache\shaders`, belong to no title, and can be
//! deleted at any time. `RIPTIDE_SHADER_CACHE=0` turns the cache off.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};

use crate::trace;

/// The folder the files are in, made the first time it is asked for. `None` if the cache
/// is off or the folder cannot be made.
fn folder() -> Option<&'static Path> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            if std::env::var_os("RIPTIDE_SHADER_CACHE").is_some_and(|value| value == "0") {
                trace!("compiled shaders are not kept (RIPTIDE_SHADER_CACHE=0)");
                return None;
            }
            let folder = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
                .join("Riptide")
                .join("cache")
                .join("shaders");
            match std::fs::create_dir_all(&folder) {
                Ok(()) => {
                    trace!("compiled shaders are kept in {}", folder.display());
                    Some(folder)
                }
                Err(error) => {
                    trace!(
                        "compiled shaders are not kept: {} cannot be made: {error}",
                        folder.display()
                    );
                    None
                }
            }
        })
        .as_deref()
}

/// The name a shader made from `parts` is kept under: the SHA-256 of the parts, each
/// preceded by its length so that no two lists of parts run together into the same bytes.
/// `None` if Windows gave no hash.
pub(crate) fn key(parts: &[&[u8]]) -> Option<String> {
    let mut all = Vec::with_capacity(parts.iter().map(|part| part.len() + 8).sum());
    for part in parts {
        all.extend_from_slice(&(part.len() as u64).to_le_bytes());
        all.extend_from_slice(part);
    }
    let mut hash = [0u8; 32];
    // SAFETY: a plain call: Windows' one-call hash, with its own SHA-256, over a buffer
    // that outlives it, into 32 bytes.
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, &all, &mut hash) }
        .ok()
        .ok()?;
    Some(hash.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The shader kept under `key`, if there is one.
pub(crate) fn get(key: &str) -> Option<Vec<u8>> {
    get_in(folder()?, key)
}

/// Keeps `shader` under `key`.
pub(crate) fn put(key: &str, shader: &[u8]) {
    if let Some(folder) = folder()
        && let Err(error) = put_in(folder, key, shader)
    {
        show_unkept(&error);
    }
}

fn get_in(folder: &Path, key: &str) -> Option<Vec<u8>> {
    std::fs::read(folder.join(key))
        .ok()
        .filter(|shader| !shader.is_empty())
}

/// Writes the file whole under another name and then gives it its own, so that a run
/// that stops in the middle leaves no half-written shader to be read later.
fn put_in(folder: &Path, key: &str, shader: &[u8]) -> std::io::Result<()> {
    let unfinished = folder.join(format!("{key}.{}.part", std::process::id()));
    std::fs::write(&unfinished, shader)?;
    std::fs::rename(&unfinished, folder.join(key)).inspect_err(|_| {
        let _ = std::fs::remove_file(&unfinished);
    })
}

/// Says once that a shader could not be kept.
fn show_unkept(error: &std::io::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if !SHOWN.swap(true, Ordering::Relaxed) {
        trace!("a compiled shader could not be kept: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_sha_256_of_the_parts_with_their_lengths() {
        // One empty part is eight zero bytes, whose SHA-256 is known.
        assert_eq!(
            key(&[&[]]).unwrap(),
            "af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc"
        );
        // Where one part ends and the next begins is part of the key.
        assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
        assert_eq!(key(&[b"ab", b"c"]), key(&[b"ab", b"c"]));
        assert_eq!(key(&[b"x"]).unwrap().len(), 64);
    }

    #[test]
    fn a_shader_put_is_the_shader_got_and_nothing_else_is() {
        let folder = std::env::temp_dir().join(format!("riptide-cache-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(get_in(&folder, "one"), None);
        put_in(&folder, "one", &[1, 2, 3]).unwrap();
        assert_eq!(get_in(&folder, "one"), Some(vec![1, 2, 3]));
        // Writing again replaces it, and no unfinished file is left behind.
        put_in(&folder, "one", &[4]).unwrap();
        assert_eq!(get_in(&folder, "one"), Some(vec![4]));
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
        // An empty file is no shader.
        std::fs::write(folder.join("empty"), []).unwrap();
        assert_eq!(get_in(&folder, "empty"), None);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
