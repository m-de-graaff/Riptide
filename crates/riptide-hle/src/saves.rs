//! Saves that cannot be half-written (smaller decision S6).
//!
//! A save is written to a second file beside the old one, flushed to the drive, and then
//! renamed over it. The rename is one step for Windows: a reader finds the whole old save
//! or the whole new one, whatever happens to the process or the power in between.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Where a save is written before it replaces `path`: the same name with `.saving` added,
/// in the same folder, so the rename never crosses drives.
pub fn pending_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".saving");
    PathBuf::from(name)
}

/// Writes `bytes` as the save at `path`, replacing what is there in one step. The folder
/// is made if it is missing.
///
/// If this fails, or the process dies before it returns, the save at `path` is what it was
/// before. A file left behind at [`pending_path`] is overwritten by the next write.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_in_steps(path, bytes, || {})
}

/// [`write`], calling `halfway` when half the bytes are in the pending file. Tests stop
/// the process there.
fn write_in_steps(path: &Path, bytes: &[u8], halfway: impl FnOnce()) -> io::Result<()> {
    if let Some(folder) = path.parent().filter(|f| !f.as_os_str().is_empty()) {
        std::fs::create_dir_all(folder)?;
    }
    let pending = pending_path(path);
    let (first, second) = bytes.split_at(bytes.len() / 2);
    let mut file = File::create(&pending)?;
    file.write_all(first)?;
    halfway();
    file.write_all(second)?;
    // The bytes have to be on the drive before the old save is given up for them.
    file.sync_all()?;
    drop(file);
    std::fs::rename(&pending, path)
}

/// Reads the save at `path`. A pending file beside it, left by a write that never
/// finished, is not looked at.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Set in the child process that the kill test starts: the save's path.
    const SAVE_CHILD: &str = "RIPTIDE_HLE_SAVE_CHILD";

    fn folder(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("riptide-save-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_save_is_written_and_read_back() {
        let dir = folder("round-trip");
        let save = dir.join("slot 1").join("progress.sav");

        write(&save, b"first").unwrap();
        assert_eq!(read(&save).unwrap(), b"first");
        write(&save, b"the second save, longer than the first").unwrap();
        assert_eq!(
            read(&save).unwrap(),
            b"the second save, longer than the first"
        );
        write(&save, b"").unwrap();
        assert_eq!(read(&save).unwrap(), b"");

        assert!(!pending_path(&save).exists(), "nothing is left pending");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_pending_file_is_beside_the_save() {
        let save = Path::new(r"C:\saves\slot.sav");
        assert_eq!(pending_path(save), Path::new(r"C:\saves\slot.sav.saving"));
    }

    #[test]
    fn a_missing_save_is_an_error_not_an_empty_save() {
        let dir = folder("missing");
        assert_eq!(
            read(&dir.join("none.sav")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_pending_file_left_behind_is_ignored_and_then_replaced() {
        let dir = folder("leftover");
        let save = dir.join("progress.sav");
        write(&save, b"good").unwrap();
        std::fs::write(pending_path(&save), b"half of a newer sa").unwrap();

        assert_eq!(read(&save).unwrap(), b"good");
        write(&save, b"newer").unwrap();
        assert_eq!(read(&save).unwrap(), b"newer");
        assert!(!pending_path(&save).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The body of the child process that the test below starts. On its own it does nothing.
    #[test]
    fn save_child() {
        let Some(save) = std::env::var_os(SAVE_CHILD) else {
            return;
        };
        let save = PathBuf::from(save);
        let new_save = vec![b'N'; 4 << 20];
        let _ = write_in_steps(&save, &new_save, || {
            // Half the new save is in the pending file. Say so, then wait to be killed.
            std::fs::write(save.with_extension("halfway"), b"").unwrap();
            std::thread::sleep(Duration::from_secs(60));
        });
        unreachable!("the parent kills this process while it waits");
    }

    #[test]
    fn killing_the_process_mid_save_leaves_the_old_save_intact() {
        let dir = folder("kill");
        let save = dir.join("progress.sav");
        let old_save = vec![b'O'; 1 << 20];
        write(&save, &old_save).unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "saves::tests::save_child", "--nocapture"])
            .env(SAVE_CHILD, &save)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let halfway = save.with_extension("halfway");
        let started = Instant::now();
        while !halfway.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the child never got halfway"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();

        // Half of the new save is on the drive, in the pending file, and the save itself is
        // the old one, whole.
        let pending = std::fs::read(pending_path(&save)).unwrap();
        assert_eq!(pending.len(), 2 << 20);
        assert!(pending.iter().all(|&byte| byte == b'N'));
        assert_eq!(read(&save).unwrap(), old_save);

        // And the next save goes through as if nothing had happened.
        write(&save, b"after the crash").unwrap();
        assert_eq!(read(&save).unwrap(), b"after the crash");
        assert!(!pending_path(&save).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
