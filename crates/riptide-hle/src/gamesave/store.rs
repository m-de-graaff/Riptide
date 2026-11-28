//! The save store on disk: containers of named blobs under a provider's root folder.
//!
//! The layout is
//!
//! ```text
//! <root>\c<hash of the container's name>-<readable part of the name>\
//!     live            the number of the generation that is the container, as text
//!     gen-<number>\   one whole version of the container
//!         index       the container's name, display name, time, and each blob's name and size
//!         b0, b1, ... the blobs' bytes, in the order the index lists them
//! ```
//!
//! No name of the title's ever becomes a file name. A container's folder is named by a
//! hash of its exact name (with a readable part added for whoever looks at the folder), and
//! the exact name is in the index, so two names that differ only in case or in characters a
//! file name cannot hold never share a folder, and none can point outside the root. A blob's
//! file is named by its position.
//!
//! An update writes a whole new generation beside the live one: new blobs are written and
//! flushed to the drive, blobs that stay are hard links to the old generation's files (which
//! are never changed once written), and the index is written last. Then `live` is replaced
//! in one step with [`crate::saves::write`]. That replacement is the commit: before it a
//! reader follows `live` to the whole previous generation, after it to the whole new one.
//! The old generation is removed afterwards. Deleting a container removes `live`, which is
//! also one step, and then the folder.
//!
//! Whatever a crash leaves behind is something `live` does not name: a generation that was
//! never switched in, an old one that was never removed, a `live.saving` file, or a folder
//! with no `live` at all. Readers never look at those, and [`sweep`] (run when a provider is
//! opened) and the next update to the container remove them.
//!
//! What is relied on and not proven here: that the file system keeps the order of its own
//! bookkeeping (a folder made and a link added before a later rename), as NTFS does with its
//! journal. Only the bytes of files are flushed explicitly. The store is guarded by one lock
//! in this process; two processes writing the same root at once are not guarded against.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// What the new blobs of one update may hold in all, as the console documents it.
pub(super) const UPDATE_LIMIT: u64 = 16 << 20;

/// The file that names the live generation.
const LIVE: &str = "live";
/// The file in a generation that lists what it holds.
const INDEX: &str = "index";
/// How an index begins, so that another file is never taken for one.
const MAGIC: &[u8] = b"RIPTIDE SAVE 1\n";
/// How much of a container's name is repeated readably in its folder's name.
const READABLE: usize = 40;

/// Held by every operation on the store, so that none sees another's work half done.
static STORE: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    STORE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Why the store could not do what was asked.
#[derive(Debug)]
pub(super) enum Error {
    /// The drive or the file system refused.
    Io(io::Error),
    /// The container's files are there but do not read as a container. Nothing is removed.
    Damaged,
    /// Another container's name has the same hash, so this one has no folder of its own.
    Clash,
    /// The container does not exist.
    NoContainer,
    /// The container has no blob of a name that was asked for.
    NoBlob,
    /// The update's new blobs are more than [`UPDATE_LIMIT`].
    TooBig,
    /// The update would take the saves past the quota.
    OverQuota,
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

/// One blob as the index lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlobEntry {
    pub(super) name: Vec<u8>,
    pub(super) size: u64,
}

/// What a container is at one moment: everything but the blobs' bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Snapshot {
    /// The container's exact name.
    pub(super) name: String,
    /// The name shown to the player, as the title gave it.
    pub(super) display: Vec<u8>,
    /// When the last update was applied, in seconds since 1970.
    pub(super) modified: i64,
    /// The blobs, in the order of their names.
    pub(super) blobs: Vec<BlobEntry>,
}

impl Snapshot {
    /// The bytes of all the container's blobs together.
    pub(super) fn total(&self) -> u64 {
        self.blobs.iter().map(|blob| blob.size).sum()
    }
}

/// A blob as a read gives it: its name and its bytes.
pub(super) type NamedBlob = (Vec<u8>, Vec<u8>);

/// One thing an update does to a container.
pub(super) enum Change {
    /// The blob of this name becomes these bytes.
    Write(Vec<u8>, Vec<u8>),
    /// The blob of this name goes, if it is there.
    Delete(Vec<u8>),
}

/// A moment in an update or a deletion at which the process might die. Tests stop the work
/// at each of them in turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    /// One blob of the new generation is in place.
    BlobStored,
    /// The new generation is whole, and `live` still names the old one.
    IndexStored,
    /// `live` names the new generation.
    Switched,
    /// The old generation is gone.
    OldRemoved,
    /// A deleted container's `live` is gone, and its folder is still there.
    Unlisted,
}

/// Passes `point`. Outside tests this does nothing.
#[cfg(not(test))]
fn reached(_point: Point) -> io::Result<()> {
    Ok(())
}

/// Passes `point`, or stops the work there as a crash would if a test asked for that.
#[cfg(test)]
fn reached(point: Point) -> io::Result<()> {
    tests::reached(point)
}

/// 64-bit FNV-1a, which gives the same hash in every build, unlike the standard hasher.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The name of the folder of the container `name`: the hash of the exact name, then the
/// start of the name with everything but lower-case letters, digits and `_` made `_`.
fn folder_name(name: &str) -> String {
    let readable: String = name
        .chars()
        .take(READABLE)
        .map(|character| match character {
            'a'..='z' | '0'..='9' => character,
            'A'..='Z' => character.to_ascii_lowercase(),
            _ => '_',
        })
        .collect();
    format!("c{:016x}-{readable}", fnv(name.as_bytes()))
}

/// Whether `file` is named as [`folder_name`] names a folder. Nothing else in the root is
/// ever touched.
fn is_container_folder(file: &str) -> bool {
    let bytes = file.as_bytes();
    bytes.len() >= 18
        && bytes[0] == b'c'
        && bytes[17] == b'-'
        && bytes[1..17]
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn generation_name(number: u64) -> String {
    format!("gen-{number}")
}

/// The number of a generation folder, if `file` is named exactly as one is.
fn generation_number(file: &str) -> Option<u64> {
    let number = file.strip_prefix("gen-")?.parse().ok()?;
    (generation_name(number) == file).then_some(number)
}

fn put_text(out: &mut Vec<u8>, text: &[u8]) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text);
}

/// An index as bytes: the magic, the container's name, the display name, the time, the
/// count of blobs, each blob's name and size, and last a hash of all that came before.
fn encode(snapshot: &Snapshot) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    put_text(&mut out, snapshot.name.as_bytes());
    put_text(&mut out, &snapshot.display);
    out.extend_from_slice(&snapshot.modified.to_le_bytes());
    out.extend_from_slice(&(snapshot.blobs.len() as u32).to_le_bytes());
    for blob in &snapshot.blobs {
        put_text(&mut out, &blob.name);
        out.extend_from_slice(&blob.size.to_le_bytes());
    }
    let check = fnv(&out);
    out.extend_from_slice(&check.to_le_bytes());
    out
}

/// What is left to read of an index.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(count)?;
        self.0 = rest;
        Some(taken)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn text(&mut self) -> Option<Vec<u8>> {
        let length = self.u32()? as usize;
        Some(self.take(length)?.to_vec())
    }
}

/// The snapshot an index holds, or nothing if the bytes are not a whole index.
fn decode(bytes: &[u8]) -> Option<Snapshot> {
    let (body, check) = bytes.split_at_checked(bytes.len().checked_sub(8)?)?;
    if u64::from_le_bytes(check.try_into().ok()?) != fnv(body) {
        return None;
    }
    let mut reader = Reader(body);
    if reader.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let name = String::from_utf8(reader.text()?).ok()?;
    let display = reader.text()?;
    let modified = reader.u64()? as i64;
    let count = reader.u32()?;
    let mut blobs = Vec::new();
    for _ in 0..count {
        let name = reader.text()?;
        let size = reader.u64()?;
        blobs.push(BlobEntry { name, size });
    }
    reader.0.is_empty().then_some(Snapshot {
        name,
        display,
        modified,
        blobs,
    })
}

/// A container as it is on disk now.
struct Live {
    folder: PathBuf,
    generation: u64,
    snapshot: Snapshot,
}

impl Live {
    fn generation_folder(&self) -> PathBuf {
        self.folder.join(generation_name(self.generation))
    }

    /// The file of the blob the index lists at `position`.
    fn blob_file(&self, position: usize) -> PathBuf {
        self.generation_folder().join(format!("b{position}"))
    }
}

/// The container in `folder`: nothing if the folder or its `live` file is missing, an
/// error if they are there and do not read.
fn live(folder: &Path) -> Result<Option<Live>, Error> {
    let text = match std::fs::read(folder.join(LIVE)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Io(error)),
    };
    let generation = std::str::from_utf8(&text)
        .ok()
        .and_then(|text| text.trim_end().parse::<u64>().ok())
        .ok_or(Error::Damaged)?;
    let index = folder.join(generation_name(generation)).join(INDEX);
    let bytes = match std::fs::read(index) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Error::Damaged),
        Err(error) => return Err(Error::Io(error)),
    };
    let snapshot = decode(&bytes).ok_or(Error::Damaged)?;
    Ok(Some(Live {
        folder: folder.to_owned(),
        generation,
        snapshot,
    }))
}

/// The container `name` under `root`, if it exists. A folder that holds another name with
/// the same hash is not this container.
fn find(root: &Path, name: &str) -> Result<Option<Live>, Error> {
    Ok(live(&root.join(folder_name(name)))?.filter(|found| found.snapshot.name == name))
}

/// Every container under `root`, in the order of their names. The lock is the caller's.
fn all(root: &Path) -> Vec<Snapshot> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str().filter(|file| is_container_folder(file)) else {
            continue;
        };
        if let Ok(Some(container)) = live(&entry.path())
            && folder_name(&container.snapshot.name) == file
        {
            found.push(container.snapshot);
        }
    }
    found.sort_by(|one, other| one.name.cmp(&other.name));
    found
}

/// Removes from a container's folder what its `live` file does not name: generations other
/// than `keep`, and a `live.saving` file. What cannot be removed now is left for next time.
fn sweep_folder(folder: &Path, keep: u64) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else {
            continue;
        };
        if generation_number(file).is_some_and(|number| number != keep) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let _ = std::fs::remove_file(crate::saves::pending_path(&folder.join(LIVE)));
}

/// Removes what interrupted updates and deletions left under `root`. A container folder
/// with no `live` file was never committed or has been deleted, and goes whole. A folder
/// whose `live` does not read is left as it is, for a person to look at.
pub(super) fn sweep(root: &Path) {
    let _store = lock();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let file = entry.file_name();
        if !file.to_str().is_some_and(is_container_folder) {
            continue;
        }
        let folder = entry.path();
        if !folder.is_dir() {
            continue;
        }
        match live(&folder) {
            Ok(Some(container)) => sweep_folder(&folder, container.generation),
            Ok(None) => {
                let _ = std::fs::remove_dir_all(&folder);
            }
            Err(_) => {}
        }
    }
}

/// The container `name`, if it exists.
pub(super) fn container(root: &Path, name: &str) -> Result<Option<Snapshot>, Error> {
    let _store = lock();
    Ok(find(root, name)?.map(|found| found.snapshot))
}

/// Every container under `root`, in the order of their names. A container that does not
/// read is left out.
pub(super) fn containers(root: &Path) -> Vec<Snapshot> {
    let _store = lock();
    all(root)
}

/// The bytes of all blobs of all containers under `root`: what counts against the quota.
pub(super) fn used(root: &Path) -> u64 {
    let _store = lock();
    all(root).iter().map(Snapshot::total).sum()
}

/// The names and bytes of blobs of the container `name`: those of `wanted`, in that order,
/// or all of them in the order of their names.
pub(super) fn read(
    root: &Path,
    name: &str,
    wanted: Option<&[Vec<u8>]>,
) -> Result<Vec<NamedBlob>, Error> {
    let _store = lock();
    let found = find(root, name)?.ok_or(Error::NoContainer)?;
    let blobs = &found.snapshot.blobs;
    let positions: Vec<usize> = match wanted {
        None => (0..blobs.len()).collect(),
        Some(wanted) => wanted
            .iter()
            .map(|name| blobs.iter().position(|blob| blob.name == *name))
            .collect::<Option<_>>()
            .ok_or(Error::NoBlob)?,
    };
    positions
        .into_iter()
        .map(|position| {
            let bytes = std::fs::read(found.blob_file(position))?;
            if bytes.len() as u64 != blobs[position].size {
                return Err(Error::Damaged);
            }
            Ok((blobs[position].name.clone(), bytes))
        })
        .collect()
}

/// Writes `bytes` as a new file at `path` and flushes them to the drive.
fn write_flushed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Where a blob of a new generation comes from.
enum Source<'a> {
    /// The previous generation's file, unchanged.
    Kept(PathBuf, u64),
    /// Bytes of this update.
    New(&'a [u8]),
}

impl Source<'_> {
    fn size(&self) -> u64 {
        match self {
            Source::Kept(_, size) => *size,
            Source::New(bytes) => bytes.len() as u64,
        }
    }
}

/// Seconds since 1970, now.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// Applies an update to the container `name` in one step, making the container if it does
/// not exist: `changes` in order, and `display` as the new display name if one is given.
/// `quota` is what all containers under `root` may hold together.
///
/// If this fails, or the process dies before it returns, the container is either wholly
/// what it was or wholly what the update makes it.
pub(super) fn apply(
    root: &Path,
    name: &str,
    display: Option<&[u8]>,
    changes: &[Change],
    quota: u64,
) -> Result<(), Error> {
    let _store = lock();
    let folder = root.join(folder_name(name));
    let current = live(&folder)?;
    if current
        .as_ref()
        .is_some_and(|current| current.snapshot.name != name)
    {
        return Err(Error::Clash);
    }

    // The blobs after the update, by name: so they come out in the order of their names.
    let mut blobs: BTreeMap<&[u8], Source<'_>> = BTreeMap::new();
    if let Some(current) = &current {
        for (position, blob) in current.snapshot.blobs.iter().enumerate() {
            let kept = Source::Kept(current.blob_file(position), blob.size);
            blobs.insert(&blob.name, kept);
        }
    }
    for change in changes {
        match change {
            Change::Write(name, bytes) => {
                blobs.insert(name, Source::New(bytes));
            }
            Change::Delete(name) => {
                blobs.remove(name.as_slice());
            }
        }
    }
    let written: u64 = blobs
        .values()
        .map(|source| match source {
            Source::New(bytes) => bytes.len() as u64,
            Source::Kept(..) => 0,
        })
        .sum();
    if written > UPDATE_LIMIT {
        return Err(Error::TooBig);
    }
    let before = current
        .as_ref()
        .map_or(0, |current| current.snapshot.total());
    let after: u64 = blobs.values().map(Source::size).sum();
    let elsewhere = all(root)
        .iter()
        .map(Snapshot::total)
        .sum::<u64>()
        .saturating_sub(before);
    // An update that makes the container no bigger is never refused for the quota.
    if after > before && elsewhere.saturating_add(after) > quota {
        return Err(Error::OverQuota);
    }

    // Leftovers of an interrupted update would be in the new generation's way.
    let next = match &current {
        Some(current) => {
            sweep_folder(&folder, current.generation);
            current.generation + 1
        }
        None => {
            match std::fs::remove_dir_all(&folder) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
            1
        }
    };
    let staged = folder.join(generation_name(next));
    std::fs::create_dir_all(&staged)?;
    for (position, source) in blobs.values().enumerate() {
        let file = staged.join(format!("b{position}"));
        match source {
            Source::New(bytes) => write_flushed(&file, bytes)?,
            // A file is never changed once written, so two generations can share it. Where
            // the file system has no hard links the bytes are copied.
            Source::Kept(from, _) => {
                if std::fs::hard_link(from, &file).is_err() {
                    write_flushed(&file, &std::fs::read(from)?)?;
                }
            }
        }
        reached(Point::BlobStored)?;
    }
    let snapshot = Snapshot {
        name: name.to_owned(),
        display: match (display, &current) {
            (Some(display), _) => display.to_vec(),
            (None, Some(current)) => current.snapshot.display.clone(),
            (None, None) => Vec::new(),
        },
        modified: now(),
        blobs: blobs
            .iter()
            .map(|(name, source)| BlobEntry {
                name: name.to_vec(),
                size: source.size(),
            })
            .collect(),
    };
    write_flushed(&staged.join(INDEX), &encode(&snapshot))?;
    reached(Point::IndexStored)?;

    // The commit: from here on the new generation is the container.
    crate::saves::write(&folder.join(LIVE), format!("{next}\n").as_bytes())?;
    reached(Point::Switched)?;

    if let Some(current) = &current {
        // If this fails the old generation is swept away later.
        let _ = std::fs::remove_dir_all(current.generation_folder());
        reached(Point::OldRemoved)?;
    }
    Ok(())
}

/// Deletes the container `name` in one step. A container that does not exist is no error.
pub(super) fn delete(root: &Path, name: &str) -> Result<(), Error> {
    let _store = lock();
    let Some(found) = find(root, name)? else {
        return Ok(());
    };
    // The one step: without `live` the folder is no container.
    std::fs::remove_file(found.folder.join(LIVE))?;
    reached(Point::Unlisted)?;
    // If this fails the folder is swept away later.
    let _ = std::fs::remove_dir_all(&found.folder);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// How many points this thread's work may still pass before it is stopped.
        static LEFT: Cell<Option<u32>> = const { Cell::new(None) };
        /// The point at which this thread's work was last stopped.
        static STOPPED: Cell<Option<Point>> = const { Cell::new(None) };
    }

    /// The test build's hook: stops the work at the point a test asked for by failing
    /// there, which leaves the disk as a crash at that moment would.
    pub(super) fn reached(point: Point) -> io::Result<()> {
        match LEFT.get() {
            Some(0) => {
                LEFT.set(None);
                STOPPED.set(Some(point));
                Err(io::Error::other("stopped by the test"))
            }
            Some(left) => {
                LEFT.set(Some(left - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Runs `work`, stopping it at the point it reaches after passing `passed` others.
    /// Gives the point it was stopped at, or nothing if it finished first.
    fn stopping_after(passed: u32, work: impl FnOnce() -> Result<(), Error>) -> Option<Point> {
        LEFT.set(Some(passed));
        STOPPED.set(None);
        let outcome = work();
        LEFT.set(None);
        let stopped = STOPPED.take();
        assert_eq!(outcome.is_err(), stopped.is_some());
        stopped
    }

    /// A fresh folder for one test, removed when the test is over.
    struct Folder(PathBuf);

    impl Folder {
        fn new(test: &str) -> Folder {
            let path =
                std::env::temp_dir().join(format!("riptide-store-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Folder(path)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const QUOTA: u64 = 1 << 20;

    fn write(name: &str, bytes: &[u8]) -> Change {
        Change::Write(name.as_bytes().to_vec(), bytes.to_vec())
    }

    fn blobs(root: &Path, name: &str) -> Vec<(String, Vec<u8>)> {
        read(root, name, None)
            .unwrap()
            .into_iter()
            .map(|(name, bytes)| (String::from_utf8(name).unwrap(), bytes))
            .collect()
    }

    /// The names of everything in the folder of the container `name`, sorted.
    fn files(root: &Path, name: &str) -> Vec<String> {
        let mut files: Vec<String> = std::fs::read_dir(root.join(folder_name(name)))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        files
    }

    #[test]
    fn an_index_reads_back_and_a_damaged_one_does_not() {
        let snapshot = Snapshot {
            name: "slot/one".to_owned(),
            display: b"Slot one".to_vec(),
            modified: 1_800_000_000,
            blobs: vec![
                BlobEntry {
                    name: b"a".to_vec(),
                    size: 3,
                },
                BlobEntry {
                    name: vec![0xff, b'/'],
                    size: 0,
                },
            ],
        };
        let bytes = encode(&snapshot);
        assert_eq!(decode(&bytes), Some(snapshot));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        let mut flipped = bytes.clone();
        flipped[MAGIC.len() + 5] ^= 1;
        assert_eq!(decode(&flipped), None);
        assert_eq!(decode(b""), None);
    }

    #[test]
    fn names_that_differ_only_in_what_a_file_name_cannot_hold_get_their_own_folders() {
        let names = ["Save", "save", "SAVE", "a/b", "a_b", "a.b", "con", "..", ""];
        let mut folders: Vec<String> = names.iter().map(|name| folder_name(name)).collect();
        for folder in &folders {
            assert!(is_container_folder(folder));
            assert!(
                folder
                    .bytes()
                    .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_')),
                "{folder} holds only what a file name may"
            );
            assert!(folder.len() <= 18 + READABLE);
        }
        folders.sort();
        folders.dedup();
        assert_eq!(folders.len(), names.len());
        assert!(!is_container_folder("notes"));
        assert!(!is_container_folder("c0123-short"));
        assert_eq!(generation_number("gen-12"), Some(12));
        assert_eq!(generation_number("gen-012"), None);
        assert_eq!(generation_number("gen-12.old"), None);
    }

    #[test]
    fn a_name_that_tries_to_leave_the_root_stays_inside_it() {
        let folder = Folder::new("leave");
        let root = folder.0.join("root");
        std::fs::create_dir_all(&root).unwrap();
        for name in [r"..\..\escape", "../escape", r"C:\escape", "nul", "a:b"] {
            apply(&root, name, None, &[write("../../blob", b"x")], QUOTA).unwrap();
            assert_eq!(
                blobs(&root, name),
                [("../../blob".to_owned(), b"x".to_vec())]
            );
        }
        // Everything made is a container folder directly under the root.
        let made: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(made.len(), 5);
        assert!(made.iter().all(|file| is_container_folder(file)));
        assert_eq!(std::fs::read_dir(&folder.0).unwrap().count(), 1);
        assert_eq!(containers(&root).len(), 5);
    }

    #[test]
    fn names_differing_only_in_case_are_different_containers_and_blobs() {
        let folder = Folder::new("case");
        let root = &folder.0;
        apply(
            root,
            "Slot",
            None,
            &[write("Data", b"upper"), write("data", b"lower")],
            QUOTA,
        )
        .unwrap();
        apply(root, "slot", None, &[write("data", b"other")], QUOTA).unwrap();
        assert_eq!(
            blobs(root, "Slot"),
            [
                ("Data".to_owned(), b"upper".to_vec()),
                ("data".to_owned(), b"lower".to_vec())
            ]
        );
        assert_eq!(
            blobs(root, "slot"),
            [("data".to_owned(), b"other".to_vec())]
        );
        assert!(container(root, "SLOT").unwrap().is_none());
        let names: Vec<String> = containers(root).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Slot", "slot"]);
    }

    #[test]
    fn a_folder_that_holds_another_name_is_not_the_container_and_is_not_written_over() {
        let folder = Folder::new("clash");
        let root = &folder.0;
        apply(root, "first", None, &[write("blob", b"1")], QUOTA).unwrap();
        // What two names with the same hash would look like on disk.
        std::fs::rename(
            root.join(folder_name("first")),
            root.join(folder_name("second")),
        )
        .unwrap();
        assert!(container(root, "second").unwrap().is_none());
        assert!(matches!(
            read(root, "second", None),
            Err(Error::NoContainer)
        ));
        assert!(matches!(
            apply(root, "second", None, &[write("blob", b"2")], QUOTA),
            Err(Error::Clash)
        ));
        delete(root, "second").unwrap();
        assert!(root.join(folder_name("second")).join(LIVE).exists());
    }

    #[test]
    fn an_update_stopped_at_any_moment_leaves_the_old_container_or_the_new_one_whole() {
        let folder = Folder::new("crash");
        let root = &folder.0;
        let old = [
            ("keep".to_owned(), b"kept bytes".to_vec()),
            ("replace".to_owned(), b"old".to_vec()),
            ("remove".to_owned(), b"going".to_vec()),
        ];
        let mut old_sorted = old.to_vec();
        old_sorted.sort();
        let mut new = vec![
            ("keep".to_owned(), b"kept bytes".to_vec()),
            ("replace".to_owned(), b"new and longer".to_vec()),
            ("added".to_owned(), b"fresh".to_vec()),
        ];
        new.sort();
        let update = [
            write("replace", b"new and longer"),
            write("added", b"fresh"),
            Change::Delete(b"remove".to_vec()),
        ];

        let mut stops = Vec::new();
        for passed in 0.. {
            let _ = std::fs::remove_dir_all(root);
            std::fs::create_dir_all(root).unwrap();
            let first: Vec<Change> = old.iter().map(|(n, b)| write(n, b)).collect();
            apply(root, "slot", Some(b"Old"), &first, QUOTA).unwrap();

            let stopped =
                stopping_after(passed, || apply(root, "slot", Some(b"New"), &update, QUOTA));
            let Some(point) = stopped else {
                break;
            };
            stops.push(point);

            // A reader finds one whole version, and which one depends only on whether the
            // switch was made.
            let switched = matches!(point, Point::Switched | Point::OldRemoved);
            let seen = container(root, "slot").unwrap().unwrap();
            if switched {
                assert_eq!(blobs(root, "slot"), new, "stopped at {point:?}");
                assert_eq!(seen.display, b"New");
            } else {
                assert_eq!(blobs(root, "slot"), old_sorted, "stopped at {point:?}");
                assert_eq!(seen.display, b"Old");
            }
            assert_eq!(used(root), seen.total());

            // The next open removes the leftovers and changes nothing a reader sees.
            sweep(root);
            let kept = if switched { "gen-2" } else { "gen-1" };
            assert_eq!(files(root, "slot"), [kept, LIVE], "stopped at {point:?}");
            assert_eq!(container(root, "slot").unwrap().unwrap(), seen);

            // And the update goes through afterwards as if nothing had happened.
            apply(root, "slot", Some(b"New"), &update, QUOTA).unwrap();
            assert_eq!(blobs(root, "slot"), new);
        }
        assert_eq!(
            stops,
            [
                Point::BlobStored,
                Point::BlobStored,
                Point::BlobStored,
                Point::IndexStored,
                Point::Switched,
                Point::OldRemoved
            ]
        );
    }

    #[test]
    fn a_first_update_stopped_before_the_switch_leaves_no_container() {
        let folder = Folder::new("first");
        let root = &folder.0;
        let update = [write("blob", b"bytes")];
        let stopped = stopping_after(1, || apply(root, "slot", None, &update, QUOTA));
        assert_eq!(stopped, Some(Point::IndexStored));
        assert!(container(root, "slot").unwrap().is_none());
        assert!(containers(root).is_empty());
        assert_eq!(used(root), 0);
        assert_eq!(files(root, "slot"), ["gen-1"]);
        sweep(root);
        assert!(!root.join(folder_name("slot")).exists());

        // The same leftover is also no obstacle to the next update.
        assert!(stopping_after(1, || apply(root, "slot", None, &update, QUOTA)).is_some());
        apply(root, "slot", None, &update, QUOTA).unwrap();
        assert_eq!(
            blobs(root, "slot"),
            [("blob".to_owned(), b"bytes".to_vec())]
        );
        assert_eq!(files(root, "slot"), ["gen-1", LIVE]);
    }

    #[test]
    fn leftovers_built_by_hand_are_ignored_by_readers_and_removed_by_the_next_open() {
        let folder = Folder::new("leftovers");
        let root = &folder.0;
        apply(
            root,
            "slot",
            Some(b"Good"),
            &[write("blob", b"good")],
            QUOTA,
        )
        .unwrap();
        let slot = root.join(folder_name("slot"));
        // A newer generation with half a blob and no index, one with an index that was cut
        // short, a `live` that was being written, and a folder that never got a `live`.
        std::fs::create_dir_all(slot.join("gen-2")).unwrap();
        std::fs::write(slot.join("gen-2").join("b0"), b"half of a newer bl").unwrap();
        std::fs::create_dir_all(slot.join("gen-3")).unwrap();
        std::fs::write(slot.join("gen-3").join(INDEX), &MAGIC[..7]).unwrap();
        std::fs::write(slot.join("live.saving"), b"2").unwrap();
        let never = root.join(folder_name("never"));
        std::fs::create_dir_all(never.join("gen-1")).unwrap();
        // Something that is not the store's, which is not to be touched.
        std::fs::write(root.join("notes.txt"), b"mine").unwrap();

        assert_eq!(blobs(root, "slot"), [("blob".to_owned(), b"good".to_vec())]);
        assert_eq!(containers(root).len(), 1);
        sweep(root);
        assert_eq!(files(root, "slot"), ["gen-1", LIVE]);
        assert!(!never.exists());
        assert!(root.join("notes.txt").exists());
        assert_eq!(blobs(root, "slot"), [("blob".to_owned(), b"good".to_vec())]);
    }

    #[test]
    fn a_container_whose_live_file_does_not_read_is_refused_and_kept() {
        let folder = Folder::new("damaged");
        let root = &folder.0;
        apply(root, "slot", None, &[write("blob", b"good")], QUOTA).unwrap();
        let slot = root.join(folder_name("slot"));
        std::fs::write(slot.join(LIVE), b"seven").unwrap();
        assert!(matches!(container(root, "slot"), Err(Error::Damaged)));
        assert!(matches!(
            apply(root, "slot", None, &[write("blob", b"new")], QUOTA),
            Err(Error::Damaged)
        ));
        assert!(containers(root).is_empty());
        sweep(root);
        assert!(slot.join("gen-1").join("b0").exists());
    }

    #[test]
    fn a_deletion_stopped_after_its_one_step_has_deleted_the_container() {
        let folder = Folder::new("delete");
        let root = &folder.0;
        apply(root, "slot", None, &[write("blob", b"bytes")], QUOTA).unwrap();
        apply(root, "other", None, &[write("blob", b"stays")], QUOTA).unwrap();
        let stopped = stopping_after(0, || delete(root, "slot"));
        assert_eq!(stopped, Some(Point::Unlisted));
        assert!(container(root, "slot").unwrap().is_none());
        assert_eq!(containers(root).len(), 1);
        assert_eq!(files(root, "slot"), ["gen-1"]);
        sweep(root);
        assert!(!root.join(folder_name("slot")).exists());
        assert_eq!(
            blobs(root, "other"),
            [("blob".to_owned(), b"stays".to_vec())]
        );

        delete(root, "other").unwrap();
        assert!(!root.join(folder_name("other")).exists());
        delete(root, "other").unwrap();
    }

    #[test]
    fn the_quota_counts_every_container_and_the_limit_counts_one_update() {
        let folder = Folder::new("quota");
        let root = &folder.0;
        apply(root, "one", None, &[write("blob", &[1; 600])], 1000).unwrap();
        assert!(matches!(
            apply(root, "two", None, &[write("blob", &[2; 401])], 1000),
            Err(Error::OverQuota)
        ));
        assert!(container(root, "two").unwrap().is_none());
        apply(root, "two", None, &[write("blob", &[2; 400])], 1000).unwrap();
        assert_eq!(used(root), 1000);
        // Replacing a blob with one of the same size fits a full quota, growing does not,
        // and shrinking always does.
        apply(root, "one", None, &[write("blob", &[3; 600])], 1000).unwrap();
        assert!(matches!(
            apply(root, "one", None, &[write("more", &[3; 1])], 1000),
            Err(Error::OverQuota)
        ));
        assert_eq!(blobs(root, "one"), [("blob".to_owned(), vec![3; 600])]);
        apply(root, "one", None, &[write("blob", &[4; 10])], 5).unwrap();
        assert_eq!(used(root), 410);

        // Two blobs that are each under the limit and together over it.
        let half = vec![0u8; (UPDATE_LIMIT / 2) as usize + 1];
        let big = [
            Change::Write(b"a".to_vec(), half.clone()),
            Change::Write(b"b".to_vec(), half),
        ];
        assert!(matches!(
            apply(root, "big", None, &big, u64::MAX),
            Err(Error::TooBig)
        ));
        // A blob written twice in one update counts once: the last write is the blob.
        apply(
            root,
            "big",
            None,
            &[write("a", b"12"), write("a", b"3")],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(blobs(root, "big"), [("a".to_owned(), b"3".to_vec())]);
    }
}
