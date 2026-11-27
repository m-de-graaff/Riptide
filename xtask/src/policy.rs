//! The repository's first rule: no game, console or SDK files, ever.
//!
//! A file is judged three ways: by its name, by its size, and by the bytes it starts with.
//! Each way catches what the others miss: a renamed package still carries its signature, and
//! a dump with no signature is still far larger than any source file.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// No tracked file may be larger than this. Source, docs and manifests stay far below it;
/// packages, dumps and captures do not.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// How much of a file is read to look for signatures. The furthest one sits at 64 KiB.
const HEAD_BYTES: usize = 0x1_0000 + 32;

/// Extensions that are refused whatever the file holds, with what they are.
const EXTENSIONS: &[(&str, &str)] = &[
    // Game packages and disc images.
    ("xvd", "an Xbox virtual disk"),
    ("xvc", "an Xbox game package"),
    ("msixvc", "an Xbox game package"),
    ("iso", "a disc image"),
    ("xiso", "an Xbox disc image"),
    ("xma", "console audio"),
    ("cso", "a compiled shader"),
    // Console files.
    ("xex", "an Xbox 360 executable"),
    ("xbe", "an Xbox executable"),
    ("cik", "a console content key"),
    ("nand", "a console flash dump"),
    ("dmp", "a memory dump"),
    // Built programs and SDK libraries. Ours are built from source and never tracked.
    ("exe", "a program image"),
    ("dll", "a program image"),
    ("sys", "a driver image"),
    ("lib", "a library"),
    ("pdb", "debug symbols"),
    ("winmd", "Windows metadata from an SDK"),
    ("obj", "a compiled object file"),
];

/// File names from the console SDK, lower case.
const SDK_NAMES: &[&str] = &[
    "d3d11_x.h",
    "d3d12_x.h",
    "d3d12_xs.h",
    "d3dx12_x.h",
    "d3dx12_xs.h",
    "xg_x.h",
    "xg_xs.h",
    "xgameruntime.h",
    "xgameruntime.lib",
    "xgameruntime.dll",
];

/// Byte signatures: where they sit, what they are, and what the file is.
const SIGNATURES: &[(usize, &[u8], &str)] = &[
    (0, b"MZ", "a program image (PE)"),
    (0x200, b"msft-xvd", "an Xbox virtual disk"),
    (0, b"XEX2", "an Xbox 360 executable"),
    (0, b"XEX1", "an Xbox 360 executable"),
    (0, b"XBEH", "an Xbox executable"),
    (0x1_0000, b"MICROSOFT*XBOX*MEDIA", "an Xbox disc image"),
    (0, b"DXBC", "a compiled shader"),
    (0, b"!<arch>\n", "a library"),
    (0, b"Microsoft C/C++ MSF 7.00", "debug symbols"),
];

/// Why a file is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The extension is on the list.
    Extension { what: &'static str },
    /// The name is a console SDK file's.
    SdkName,
    /// The file is larger than [`MAX_FILE_BYTES`].
    Size { bytes: u64 },
    /// The content carries a known signature.
    Signature { what: &'static str, offset: usize },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Extension { what } => write!(f, "its extension marks it as {what}"),
            Reason::SdkName => write!(f, "its name is a console SDK file's"),
            Reason::Size { bytes } => {
                write!(f, "it is {bytes} bytes, over the limit of {MAX_FILE_BYTES}")
            }
            Reason::Signature { what, offset } => {
                write!(f, "its content at byte {offset} is the signature of {what}")
            }
        }
    }
}

/// A refused file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The path as git gave it, relative to the repository root.
    pub path: String,
    pub reason: Reason,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// Judges a file by its name alone.
pub fn judge_name(path: &str) -> Option<Reason> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = name.to_ascii_lowercase();
    if SDK_NAMES.contains(&name.as_str()) {
        return Some(Reason::SdkName);
    }
    let (_, extension) = name.rsplit_once('.')?;
    EXTENSIONS
        .iter()
        .find(|(listed, _)| *listed == extension)
        .map(|&(_, what)| Reason::Extension { what })
}

/// Judges a file by its size.
pub fn judge_size(bytes: u64) -> Option<Reason> {
    (bytes > MAX_FILE_BYTES).then_some(Reason::Size { bytes })
}

/// Judges a file by the bytes it starts with.
pub fn judge_content(head: &[u8]) -> Option<Reason> {
    SIGNATURES
        .iter()
        .find(|(offset, signature, _)| {
            head.get(*offset..*offset + signature.len()) == Some(*signature)
        })
        .map(|&(offset, _, what)| Reason::Signature { what, offset })
}

/// Checks `paths`, relative to `root`, and returns every file that is refused, each with the
/// first reason found: name, then size, then content.
///
/// A path with no file behind it is skipped: a deleted file that is still listed holds
/// nothing to refuse.
pub fn check_files<I, S>(root: &Path, paths: I) -> io::Result<Vec<Finding>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut findings = Vec::new();
    for path in paths {
        let path = path.as_ref();
        if let Some(reason) = judge_file(root, path)? {
            findings.push(Finding {
                path: path.to_owned(),
                reason,
            });
        }
    }
    Ok(findings)
}

fn judge_file(root: &Path, path: &str) -> io::Result<Option<Reason>> {
    if let Some(reason) = judge_name(path) {
        return Ok(Some(reason));
    }
    let full = root.join(path);
    let metadata = match std::fs::metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    if let Some(reason) = judge_size(metadata.len()) {
        return Ok(Some(reason));
    }
    let mut head = Vec::with_capacity(HEAD_BYTES);
    File::open(&full)?
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut head)?;
    Ok(judge_content(&head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A directory of planted files that is removed again when the test ends.
    struct Planted(PathBuf);

    impl Planted {
        fn new(test: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("riptide-policy-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Planted(dir)
        }

        fn plant(&self, name: &str, content: &[u8]) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn check(&self, names: &[&str]) -> Vec<Finding> {
            check_files(&self.0, names).unwrap()
        }
    }

    impl Drop for Planted {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(offset: usize, signature: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; offset];
        bytes.extend_from_slice(signature);
        bytes.extend_from_slice(&[0u8; 64]);
        bytes
    }

    #[test]
    fn source_and_docs_pass() {
        let planted = Planted::new("pass");
        planted.plant("src/lib.rs", b"pub fn main() {}\n");
        planted.plant("README.md", b"# Riptide\n");
        planted.plant("Cargo.toml", b"[package]\n");
        assert_eq!(
            planted.check(&["src/lib.rs", "README.md", "Cargo.toml"]),
            []
        );
    }

    #[test]
    fn a_game_package_is_refused_by_extension() {
        let planted = Planted::new("game-ext");
        planted.plant("title/game.xvc", b"plain text");
        planted.plant("title/GAME.MSIXVC", b"plain text");
        let findings = planted.check(&["title/game.xvc", "title/GAME.MSIXVC"]);
        assert_eq!(findings.len(), 2);
        assert!(
            findings
                .iter()
                .all(|f| matches!(f.reason, Reason::Extension { .. }))
        );
    }

    #[test]
    fn a_renamed_game_package_is_refused_by_signature() {
        let planted = Planted::new("game-sig");
        planted.plant("notes.txt", &at(0x200, b"msft-xvd"));
        let findings = planted.check(&["notes.txt"]);
        assert_eq!(
            findings[0].reason,
            Reason::Signature {
                what: "an Xbox virtual disk",
                offset: 0x200
            }
        );
    }

    #[test]
    fn a_disc_image_is_refused_by_its_signature_at_64_kib() {
        let planted = Planted::new("disc");
        planted.plant("data.bin", &at(0x1_0000, b"MICROSOFT*XBOX*MEDIA"));
        let findings = planted.check(&["data.bin"]);
        assert!(matches!(
            findings[0].reason,
            Reason::Signature {
                offset: 0x1_0000,
                ..
            }
        ));
    }

    #[test]
    fn a_console_file_is_refused() {
        let planted = Planted::new("console");
        planted.plant("keys/title.cik", b"0123456789abcdef");
        planted.plant("default.txt", &at(0, b"XEX2"));
        planted.plant("flash.txt", &at(0, b"XBEH"));
        let findings = planted.check(&["keys/title.cik", "default.txt", "flash.txt"]);
        assert_eq!(findings.len(), 3);
        assert!(matches!(findings[0].reason, Reason::Extension { .. }));
        assert!(matches!(findings[1].reason, Reason::Signature { .. }));
        assert!(matches!(findings[2].reason, Reason::Signature { .. }));
    }

    #[test]
    fn an_sdk_file_is_refused() {
        let planted = Planted::new("sdk");
        planted.plant("include/D3D12_X.h", b"// header\n");
        planted.plant("import.txt", &at(0, b"!<arch>\n"));
        planted.plant("sdk/xg.winmd", b"x");
        let findings = planted.check(&["include/D3D12_X.h", "import.txt", "sdk/xg.winmd"]);
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0].reason, Reason::SdkName);
        assert!(matches!(findings[1].reason, Reason::Signature { .. }));
        assert!(matches!(findings[2].reason, Reason::Extension { .. }));
    }

    #[test]
    fn a_program_image_is_refused_whatever_it_is_called() {
        let planted = Planted::new("pe");
        planted.plant("fixture.dat", &at(0, b"MZ"));
        planted.plant("shader.dat", &at(0, b"DXBC"));
        let findings = planted.check(&["fixture.dat", "shader.dat"]);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn a_file_over_the_size_limit_is_refused() {
        let planted = Planted::new("size");
        planted.plant("capture.txt", &vec![b'a'; MAX_FILE_BYTES as usize + 1]);
        planted.plant("just-fits.txt", &vec![b'a'; MAX_FILE_BYTES as usize]);
        let findings = planted.check(&["capture.txt", "just-fits.txt"]);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].reason,
            Reason::Size {
                bytes: MAX_FILE_BYTES + 1
            }
        );
    }

    #[test]
    fn a_listed_file_that_is_gone_is_skipped() {
        let planted = Planted::new("gone");
        assert_eq!(planted.check(&["deleted.rs"]), []);
    }

    #[test]
    fn a_finding_names_the_file_and_the_reason() {
        let finding = Finding {
            path: "title/game.xvc".to_owned(),
            reason: judge_name("title/game.xvc").unwrap(),
        };
        assert_eq!(
            finding.to_string(),
            "title/game.xvc: its extension marks it as an Xbox game package"
        );
    }
}
