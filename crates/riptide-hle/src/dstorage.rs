//! The console's DirectStorage, `XCRDAPI.dll`: how a title streams its files (decision D12).
//!
//! The title asks for a factory, and through it opens files, makes queues and makes status
//! arrays. It enqueues read requests and status entries on a queue and then submits the
//! queue. A request reads from a file or from memory, optionally inflates what it read, and
//! writes the result to an address or to a list of physical pages. Requests of one submit
//! run side by side. A status entry is written once every request enqueued before it on its
//! queue has finished, with the first of their failures or with success, and the title
//! polls the array for it.
//!
//! An uncompressed read to an address goes from the file straight into the title's memory.
//!
//! BCPack data is unpacked by `riptide-formats`, after inflating if the request asks for
//! both. The format's constants are read from a file the user supplies ([`tables`]);
//! without it such a request is answered with black blocks, and the session log says so.
//!
//! Not built: the staging buffer's limit on how much is in flight.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{c_char, c_void};
use std::os::windows::fs::FileExt;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use riptide_mem::FRAME;
use riptide_pe::Symbol;

use crate::com::{self, Instance};
use crate::imports::Imports;
use crate::trace::trace;

const S_OK: i32 = 0;
const E_PENDING: i32 = 0x8000_000A_u32 as i32;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// The file was not found, as an HRESULT.
const FILE_NOT_FOUND: i32 = 0x8007_0002_u32 as i32;
/// The source ends before the request does.
const END_OF_FILE: i32 = 0x8007_0026_u32 as i32;
/// A compressed stream that does not decode.
const INVALID_DATA: i32 = 0x8007_000D_u32 as i32;

/// Request option: the source is a zlib stream (RFC 1950) to inflate.
const OPTION_ZLIB: u64 = 1;
/// Request option, four bits: the source is BCPack data of this block-compression format.
const OPTION_BCPACK: u64 = 0xF << 1;
/// Request option: the destination is a list of 64 KiB page numbers, not an address.
const OPTION_PAGE_DESTINATION: u64 = 1 << 21;
/// Request option: the source is an address, not a file.
const OPTION_MEMORY_SOURCE: u64 = 1 << 22;

/// The console's page numbers count 4 KiB; a frame is 64 KiB.
const PAGES_PER_FRAME: usize = (FRAME / 4096) as usize;

const FACTORY_SLOTS: usize = 16;
const FILE_SLOTS: usize = 8;
const STATUS_SLOTS: usize = 5;
const QUEUE_SLOTS: usize = 12;

/// A read request as the title builds it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Request {
    options: u64,
    /// An address, or with [`OPTION_PAGE_DESTINATION`] an array of page numbers.
    destination: *mut c_void,
    /// With a page destination: in its low 16 bits, where in the first page the result
    /// starts.
    page_offset: u64,
    destination_size: u32,
    /// A file object from `OpenFile`, or with [`OPTION_MEMORY_SOURCE`] an address.
    source: *const c_void,
    offset: u64,
    source_size: u32,
    /// The size of what the first of two stages gives the second: of the BCPack stream a
    /// zlib stream inflates to.
    intermediate_size: u32,
    /// Matched by cancellation.
    tag: u64,
    name: *const c_char,
}

const _: () = assert!(size_of::<Request>() == 0x48);

/// A queue's description as the title builds it.
#[repr(C)]
pub struct QueueDescription {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const c_char,
    unknown: u64,
}

/// An open file. Requests keep it open for as long as they run.
struct File {
    file: Arc<std::fs::File>,
}

/// One result per entry: pending until its status is written.
struct StatusArray {
    entries: Box<[AtomicI32]>,
}

type FileObject = Instance<File>;
type StatusObject = Instance<Arc<StatusArray>>;
type QueueObject = Instance<Queue>;

/// Where a request reads from.
enum Source {
    File(Arc<std::fs::File>),
    Memory(usize),
}

/// A request ready to run: what the title asked, with its file kept open.
struct Read {
    options: u64,
    source: Source,
    offset: u64,
    source_size: usize,
    destination: usize,
    page_offset: usize,
    destination_size: usize,
    /// How large what is inflated is, when it is unpacked after.
    intermediate_size: usize,
}

enum Command {
    Read(Read),
    Status(Arc<StatusArray>, u32),
}

#[derive(Default)]
struct Lane {
    next: u64,
    /// Requests that have been submitted and have not finished, by their place in the
    /// queue.
    running: BTreeSet<u64>,
    /// Failures no status entry has reported yet, by the failed request's place.
    failures: BTreeMap<u64, i32>,
    /// Status entries waiting for the requests before them.
    statuses: VecDeque<(u64, Arc<StatusArray>, u32)>,
}

impl Lane {
    /// Writes every status entry that no running request stands before.
    fn settle(&mut self) {
        let first_running = self.running.first().copied().unwrap_or(u64::MAX);
        while let Some(&(place, _, _)) = self.statuses.front()
            && place < first_running
        {
            let (_, array, index) = self.statuses.pop_front().expect("looked at above");
            // The first failure before this entry is its result, and it takes every failure
            // before it with it.
            let earlier: Vec<u64> = self.failures.range(..place).map(|(&at, _)| at).collect();
            let result = earlier.first().map_or(S_OK, |at| self.failures[at]);
            for at in earlier {
                self.failures.remove(&at);
            }
            if let Some(entry) = array.entries.get(index as usize) {
                entry.store(result, Ordering::Release);
            }
        }
    }
}

struct Shared {
    lane: Mutex<Lane>,
    finished: Condvar,
}

impl Shared {
    fn lane(&self) -> MutexGuard<'_, Lane> {
        self.lane
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// A queue: what is enqueued and not yet submitted, and what is running.
struct Queue {
    waiting: Mutex<Vec<Command>>,
    shared: Arc<Shared>,
}

impl Queue {
    fn waiting(&self) -> MutexGuard<'_, Vec<Command>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Starts everything enqueued since the last submit.
    fn submit(&self) {
        let commands = std::mem::take(&mut *self.waiting());
        let mut lane = self.shared.lane();
        for command in commands {
            let place = lane.next;
            lane.next += 1;
            match command {
                Command::Status(array, index) => lane.statuses.push_back((place, array, index)),
                Command::Read(read) => {
                    lane.running.insert(place);
                    let shared = Arc::clone(&self.shared);
                    crate::pool::run(move || {
                        let result = read.run();
                        let mut lane = shared.lane();
                        lane.running.remove(&place);
                        if let Err(failure) = result {
                            lane.failures.insert(place, failure);
                        }
                        lane.settle();
                        drop(lane);
                        shared.finished.notify_all();
                    });
                }
            }
        }
        lane.settle();
    }

    /// Waits until everything submitted has finished and every status is written.
    fn drain(&self) {
        let mut lane = self.shared.lane();
        while !lane.running.is_empty() || !lane.statuses.is_empty() {
            lane = self
                .shared
                .finished
                .wait(lane)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

// SAFETY: a request holds addresses in the title's memory as numbers; running it on another
// thread is what the title asks for when it submits.
unsafe impl Send for Read {}

impl Read {
    /// Takes a request apart, keeping its file open. `None` if it names no usable source.
    ///
    /// # Safety
    /// `request.source` is a file object from `OpenFile` unless the request says it is
    /// memory.
    unsafe fn from(request: &Request) -> Option<Read> {
        let source = if request.options & OPTION_MEMORY_SOURCE != 0 {
            Source::Memory(request.source as usize)
        } else {
            // SAFETY: as this function's contract says; null is refused.
            let file = unsafe { request.source.cast::<FileObject>().as_ref() }?;
            Source::File(Arc::clone(&file.state.file))
        };
        Some(Read {
            options: request.options,
            source,
            offset: request.offset,
            source_size: request.source_size as usize,
            intermediate_size: request.intermediate_size as usize,
            destination: request.destination as usize,
            page_offset: (request.page_offset & 0xFFFF) as usize,
            destination_size: request.destination_size as usize,
        })
    }

    /// Reads exactly `into.len()` bytes of the source.
    fn read_source(&self, into: &mut [u8]) -> Result<(), i32> {
        match &self.source {
            Source::Memory(address) => {
                // SAFETY: the title says `source_size` bytes lie at the address it gave,
                // from `offset` on.
                let from = unsafe {
                    std::slice::from_raw_parts(
                        (address + self.offset as usize) as *const u8,
                        into.len(),
                    )
                };
                into.copy_from_slice(from);
                Ok(())
            }
            Source::File(file) => {
                let mut done = 0;
                while done < into.len() {
                    match file.seek_read(&mut into[done..], self.offset + done as u64) {
                        Ok(0) => return Err(END_OF_FILE),
                        Ok(count) => done += count,
                        Err(error) => {
                            return Err(error.raw_os_error().map_or(INVALID_DATA, |code| {
                                0x8007_0000_u32 as i32 | (code & 0xFFFF)
                            }));
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Does the request: reads, inflates if asked, and writes the result where it goes.
    fn run(&self) -> Result<(), i32> {
        let packed = ((self.options & OPTION_BCPACK) >> 1) as u8;
        let inflate = self.options & OPTION_ZLIB != 0;
        let to_pages = self.options & OPTION_PAGE_DESTINATION != 0;

        if packed == 0 && !inflate && !to_pages {
            // Straight from the file into the title's memory.
            if self.source_size > self.destination_size {
                return Err(E_INVALIDARG);
            }
            crate::xmem::memory().will_write(self.destination as u64, self.source_size as u64);
            // SAFETY: the title says `destination_size` writable bytes lie at the address.
            let into = unsafe {
                std::slice::from_raw_parts_mut(self.destination as *mut u8, self.source_size)
            };
            return self.read_source(into);
        }

        let mut source = vec![0u8; self.source_size];
        self.read_source(&mut source)?;
        // Inflating comes first, and unpacking after: an inflated BCPack stream is as
        // large as the request says the stage between is.
        let stream = if inflate {
            let room = if packed != 0 {
                self.intermediate_size
            } else {
                self.destination_size
            };
            let mut out = vec![0u8; room];
            let written = miniz_oxide::inflate::decompress_slice_iter_to_slice(
                &mut out,
                std::iter::once(source.as_slice()),
                true,
                false,
            )
            .map_err(|_| INVALID_DATA)?;
            out.truncate(written);
            out
        } else {
            source
        };
        let result = if packed != 0 {
            unpack(packed, &stream, self.destination_size)?
        } else {
            stream
        };
        if result.len() > self.destination_size {
            return Err(E_INVALIDARG);
        }

        if to_pages {
            let count = (self.page_offset + result.len()).div_ceil(FRAME as usize);
            // SAFETY: the title gives a list of page numbers long enough for the result; it
            // has filled the list in by the time it submits.
            let pages =
                unsafe { std::slice::from_raw_parts(self.destination as *const usize, count) };
            let frames: Option<Vec<u32>> = pages
                .iter()
                .map(|&page| {
                    (page != 0 && page.is_multiple_of(PAGES_PER_FRAME))
                        .then(|| u32::try_from(page / PAGES_PER_FRAME).ok())
                        .flatten()
                })
                .collect();
            let frames = frames.ok_or(E_INVALIDARG)?;
            crate::xmem::memory()
                .write_frames(&frames, self.page_offset, &result)
                .map_err(|_| E_INVALIDARG)
        } else {
            crate::xmem::memory().will_write(self.destination as u64, result.len() as u64);
            // SAFETY: the title says `destination_size` writable bytes lie at the address.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    result.as_ptr(),
                    self.destination as *mut u8,
                    result.len(),
                );
            }
            Ok(())
        }
    }
}

/// The BCPack format's constants, read once from a file the user supplies: the one
/// `RIPTIDE_BCPACK_TABLES` names, or `bcpack-tables.bin` in `%LOCALAPPDATA%\Riptide`. They
/// are not Riptide's to ship (see `riptide-formats`). `None` if there is no such file or
/// it is not the tables, which the session log says.
fn tables() -> Option<&'static riptide_formats::bcpack::Tables> {
    static TABLES: OnceLock<Option<riptide_formats::bcpack::Tables>> = OnceLock::new();
    TABLES
        .get_or_init(|| {
            let path = std::env::var_os("RIPTIDE_BCPACK_TABLES")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    Some(
                        std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
                            .join("Riptide")
                            .join("bcpack-tables.bin"),
                    )
                })?;
            let read = std::fs::read(&path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    riptide_formats::bcpack::Tables::parse(&bytes).map_err(|error| error.to_string())
                });
            match read {
                Ok(tables) => {
                    trace!("DirectStorage: BCPack tables read from {}", path.display());
                    Some(tables)
                }
                Err(why) => {
                    trace!(
                        "DirectStorage: no BCPack tables ({}: {why}); BCPack data will be                          answered with black blocks",
                        path.display()
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// The blocks a BCPack stream of block-compression format `format` holds, `bytes` of them.
///
/// Without the format's tables (see [`tables`]) the blocks come out all zero, which every
/// block-compressed format reads as black. A stream that does not decode fails the
/// request. `RIPTIDE_BCPACK_DUMP` names a folder each stream is written to as
/// `<number>.bc<format>.<bytes>.bin`, for work on the decoder; what is written is the
/// title's data and stays on this machine.
fn unpack(format: u8, packed: &[u8], bytes: usize) -> Result<Vec<u8>, i32> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ASKED: AtomicU64 = AtomicU64::new(0);
    static FAILED: AtomicU64 = AtomicU64::new(0);
    if format > 7 {
        // There are seven block-compression formats; what an eighth would hold is not
        // known, and black blocks of the wrong size would not stand in for it.
        crate::stub::stop(&format!(
            "a DirectStorage request for BCPack data of format {format} ({} bytes)",
            packed.len()
        ));
    }
    let asked = ASKED.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some(folder) = std::env::var_os("RIPTIDE_BCPACK_DUMP") {
        /// How many streams are written: enough to work with, and not a title's whole
        /// texture set.
        const MOST: u64 = 4096;
        if asked <= MOST {
            let name = format!("{asked:05}.bc{format}.{bytes}.bin");
            let _ = std::fs::write(std::path::Path::new(&folder).join(name), packed);
        }
    }
    let mut blocks = vec![0u8; bytes];
    let Some(tables) = tables() else {
        return Ok(blocks);
    };
    // A title may ask for the beginning of a stream only; whole blocks are decoded, and
    // what is left of the room after the last whole block stays zero.
    let block = if matches!(format, 1 | 4) { 8 } else { 16 };
    let whole = bytes - bytes % block;
    match riptide_formats::bcpack::decode(format, packed, &mut blocks[..whole], tables) {
        Ok(()) => Ok(blocks),
        Err(error) => {
            let failed = FAILED.fetch_add(1, Ordering::Relaxed) + 1;
            if failed <= 8 || failed.is_power_of_two() {
                trace!(
                    "DirectStorage: BCPack data of format {format} ({} bytes for {bytes}) does                      not decode ({failed} so far): {error}",
                    packed.len()
                );
            }
            Err(INVALID_DATA)
        }
    }
}

fn file_table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        com::table(
            "IDStorageFileX",
            FILE_SLOTS,
            &[
                (1, FileObject::add_ref as *const () as usize),
                (2, FileObject::release as *const () as usize),
            ],
        )
    })
}

fn status_table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        com::table(
            "IDStorageStatusArrayX",
            STATUS_SLOTS,
            &[
                (1, StatusObject::add_ref as *const () as usize),
                (2, StatusObject::release as *const () as usize),
                (3, status_is_complete as *const () as usize),
                (4, status_result as *const () as usize),
            ],
        )
    })
}

fn queue_table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        com::table(
            "IDStorageQueueX",
            QUEUE_SLOTS,
            &[
                (1, QueueObject::add_ref as *const () as usize),
                (2, QueueObject::release as *const () as usize),
                (3, queue_enqueue_request as *const () as usize),
                (4, queue_enqueue_status as *const () as usize),
                (6, queue_submit as *const () as usize),
                (8, queue_close as *const () as usize),
            ],
        )
    })
}

/// `IDStorageFactoryX::OpenFile`, at +0x20. The path is led as every file path is, so `G:`
/// is the game folder; the file is opened for reading only.
///
/// # Safety
/// `path` is a null-terminated UTF-16 string; `out` is null or where the object goes.
unsafe extern "system" fn open_file(
    _this: *mut c_void,
    path: *const u16,
    _interface: *const [u8; 16],
    out: *mut *mut FileObject,
) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's string.
    let path = unsafe { crate::files::wide(path) };
    let (led, _) = crate::files::lead_wide(&path);
    let led = String::from_utf16_lossy(&led[..led.len() - 1]);
    match std::fs::File::open(&led) {
        Ok(file) => {
            trace!("DirectStorage: opened {path}");
            let object = FileObject::create(
                file_table(),
                File {
                    file: Arc::new(file),
                },
            );
            // SAFETY: checked non-null; the title's own out-pointer.
            unsafe { out.write(object) };
            S_OK
        }
        Err(error) => {
            trace!("DirectStorage: {path} could not be opened: {error}");
            error.raw_os_error().map_or(FILE_NOT_FOUND, |code| {
                0x8007_0000_u32 as i32 | (code & 0xFFFF)
            })
        }
    }
}

/// `IDStorageFactoryX::CreateStatusArray`, at +0x28: `capacity` entries, all pending.
///
/// # Safety
/// `out` is null or where the object goes.
unsafe extern "system" fn create_status_array(
    _this: *mut c_void,
    capacity: u32,
    _name: *const c_char,
    _interface: *const [u8; 16],
    out: *mut *mut StatusObject,
) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    let entries = (0..capacity).map(|_| AtomicI32::new(E_PENDING)).collect();
    let object = StatusObject::create(status_table(), Arc::new(StatusArray { entries }));
    // SAFETY: checked non-null; the title's own out-pointer.
    unsafe { out.write(object) };
    S_OK
}

/// `IDStorageFactoryX::CreateQueue`, at +0x50.
///
/// # Safety
/// `description` is null or the title's description; `out` is null or where the object
/// goes.
unsafe extern "system" fn create_queue(
    _this: *mut c_void,
    description: *const QueueDescription,
    _interface: *const [u8; 16],
    out: *mut *mut QueueObject,
) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's structure; null is tolerated.
    if let Some(description) = unsafe { description.as_ref() } {
        trace!(
            "DirectStorage: queue made (source type {}, capacity {}, priority {})",
            description.source_type, description.capacity, description.priority
        );
    }
    let object = QueueObject::create(
        queue_table(),
        Queue {
            waiting: Mutex::new(Vec::new()),
            shared: Arc::new(Shared {
                lane: Mutex::new(Lane::default()),
                finished: Condvar::new(),
            }),
        },
    );
    // SAFETY: checked non-null; the title's own out-pointer.
    unsafe { out.write(object) };
    S_OK
}

/// `IDStorageFactoryX::SetStagingBufferSize`, at +0x40: how much compressed data may be in
/// flight at once. Noted; the limit is not kept yet.
extern "system" fn set_staging_buffer_size(_this: *mut c_void, size: u32) -> i32 {
    trace!("DirectStorage: staging buffer {size:#x} bytes");
    S_OK
}

/// `IDStorageStatusArrayX::IsComplete`, at +0x18.
///
/// # Safety
/// `this` is a status array from `CreateStatusArray`.
unsafe extern "system" fn status_is_complete(this: *mut StatusObject, index: u32) -> bool {
    // SAFETY: as this function's contract says.
    let array = unsafe { &(*this).state };
    array
        .entries
        .get(index as usize)
        .is_some_and(|entry| entry.load(Ordering::Acquire) != E_PENDING)
}

/// `IDStorageStatusArrayX::GetHResult`, at +0x20: the entry's result, pending until written.
///
/// # Safety
/// `this` is a status array from `CreateStatusArray`.
unsafe extern "system" fn status_result(this: *mut StatusObject, index: u32) -> i32 {
    // SAFETY: as this function's contract says.
    let array = unsafe { &(*this).state };
    array
        .entries
        .get(index as usize)
        .map_or(E_INVALIDARG, |entry| entry.load(Ordering::Acquire))
}

/// `IDStorageQueueX::EnqueueRequest`, at +0x18. Nothing starts until the queue is
/// submitted: the title may still be filling in the pages a request writes to.
///
/// # Safety
/// `this` is a queue from `CreateQueue`; `request` is null or the title's request, whose
/// source is a file from `OpenFile` or memory as its options say.
unsafe extern "system" fn queue_enqueue_request(this: *mut QueueObject, request: *const Request) {
    // SAFETY: as this function's contract says.
    let (queue, request) = unsafe { (&(*this).state, request.as_ref()) };
    // SAFETY: the request's source is what its options say it is.
    match request.and_then(|request| unsafe { Read::from(request) }) {
        Some(read) => queue.waiting().push(Command::Read(read)),
        None => trace!("DirectStorage: a request with no source was dropped"),
    }
}

/// `IDStorageQueueX::EnqueueStatus`, at +0x20: entry `index` of `array` is written when
/// everything enqueued before it has finished. Until then it reads as pending.
///
/// # Safety
/// `this` is a queue from `CreateQueue`; `array` is a status array from
/// `CreateStatusArray`.
unsafe extern "system" fn queue_enqueue_status(
    this: *mut QueueObject,
    array: *mut StatusObject,
    index: u32,
) {
    // SAFETY: as this function's contract says; a null array is dropped.
    let (queue, array) = unsafe { (&(*this).state, array.as_ref()) };
    let Some(array) = array else {
        return;
    };
    if let Some(entry) = array.state.entries.get(index as usize) {
        entry.store(E_PENDING, Ordering::Release);
    }
    queue
        .waiting()
        .push(Command::Status(Arc::clone(&array.state), index));
}

/// `IDStorageQueueX::Submit`, at +0x30.
///
/// # Safety
/// `this` is a queue from `CreateQueue`.
unsafe extern "system" fn queue_submit(this: *mut QueueObject) {
    // SAFETY: as this function's contract says.
    unsafe { &(*this).state }.submit();
}

/// `IDStorageQueueX::Close`, at +0x40: waits for everything submitted.
///
/// # Safety
/// `this` is a queue from `CreateQueue`.
unsafe extern "system" fn queue_close(this: *mut QueueObject) {
    // SAFETY: as this function's contract says.
    unsafe { &(*this).state }.drain();
}

fn factory() -> usize {
    static FACTORY: OnceLock<usize> = OnceLock::new();
    *FACTORY.get_or_init(|| {
        com::singleton(
            "IDStorageFactoryX",
            FACTORY_SLOTS,
            &[
                (1, com::keep as *const () as usize),
                (2, com::keep as *const () as usize),
                (4, open_file as *const () as usize),
                (5, create_status_array as *const () as usize),
                (8, set_staging_buffer_size as *const () as usize),
                (10, create_queue as *const () as usize),
            ],
        )
    })
}

/// `XCRDAPI.dll!DStorageGetFactory`: the one factory, whatever interface id is asked for.
///
/// # Safety
/// `out` is null or where the title wants the factory's address.
pub unsafe extern "system" fn get_factory(interface_id: *const [u8; 16], out: *mut usize) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title passes an interface id; null is tolerated.
    let id = unsafe { interface_id.as_ref() };
    trace!("DStorageGetFactory(interface {})", guid(id));
    // SAFETY: checked non-null; the title's own out-pointer.
    unsafe { out.write(factory()) };
    S_OK
}

/// An interface id as it is usually written, for the log.
pub(crate) fn guid(id: Option<&[u8; 16]>) -> String {
    let Some(b) = id else {
        return "null".to_owned();
    };
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

pub(crate) fn register(imports: &mut Imports) {
    // SAFETY: `get_factory` has the signature of the console function it is registered as.
    unsafe {
        imports.implement(
            "xcrdapi.dll",
            Symbol::Name("DStorageGetFactory".to_owned()),
            get_factory as *const () as usize,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use riptide_mem::{Access, Limits};
    use std::io::Write;

    const ANYWHERE: Limits = Limits {
        lowest: 0x1_0000_0000,
        highest: (1 << 37) - 1,
        alignment: 0,
    };

    /// A file of `bytes` in the temporary folder, removed when dropped.
    struct Planted(std::path::PathBuf);

    impl Planted {
        fn new(name: &str, bytes: &[u8]) -> Planted {
            let path = std::env::temp_dir().join(format!(
                "riptide-dstorage-{name}-{}.bin",
                std::process::id()
            ));
            std::fs::File::create(&path)
                .unwrap()
                .write_all(bytes)
                .unwrap();
            Planted(path)
        }

        /// Opens the file through the factory's `OpenFile`.
        fn open(&self) -> *mut FileObject {
            let path: Vec<u16> = self.0.to_string_lossy().encode_utf16().chain([0]).collect();
            let mut file = std::ptr::null_mut();
            // SAFETY: a null-terminated path and a valid out-pointer.
            let opened = unsafe {
                open_file(
                    std::ptr::null_mut(),
                    path.as_ptr(),
                    std::ptr::null(),
                    &mut file,
                )
            };
            assert_eq!(opened, S_OK);
            file
        }
    }

    impl Drop for Planted {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn queue() -> *mut QueueObject {
        let mut queue = std::ptr::null_mut();
        // SAFETY: a null description is tolerated; a valid out-pointer.
        let made = unsafe {
            create_queue(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                &mut queue,
            )
        };
        assert_eq!(made, S_OK);
        queue
    }

    fn statuses(capacity: u32) -> *mut StatusObject {
        let mut array = std::ptr::null_mut();
        // SAFETY: a valid out-pointer.
        let made = unsafe {
            create_status_array(
                std::ptr::null_mut(),
                capacity,
                std::ptr::null(),
                std::ptr::null(),
                &mut array,
            )
        };
        assert_eq!(made, S_OK);
        array
    }

    fn request(options: u64, source: *const c_void, offset: u64, source_size: u32) -> Request {
        Request {
            options,
            destination: std::ptr::null_mut(),
            page_offset: 0,
            destination_size: 0,
            source,
            offset,
            source_size,
            intermediate_size: 0,
            tag: 0,
            name: std::ptr::null(),
        }
    }

    /// Enqueues `requests` and one status entry, submits, waits, and returns the entry.
    fn run(requests: &[Request]) -> i32 {
        let (queue, array) = (queue(), statuses(4));
        // SAFETY: objects made above, used as the title uses them, then released.
        unsafe {
            assert!(!status_is_complete(array, 2));
            for request in requests {
                queue_enqueue_request(queue, request);
            }
            queue_enqueue_status(queue, array, 2);
            assert_eq!(status_result(array, 2), E_PENDING);
            queue_submit(queue);
            queue_close(queue);
            assert!(status_is_complete(array, 2));
            let result = status_result(array, 2);
            QueueObject::release(queue);
            StatusObject::release(array);
            result
        }
    }

    #[test]
    fn a_plain_read_goes_from_the_file_into_title_memory() {
        let data = pattern(300_000);
        let planted = Planted::new("plain", &data);
        let file = planted.open();
        let memory = crate::xmem::memory();
        let at = memory
            .commit(None, 4 * FRAME, ANYWHERE, Access::ReadWrite)
            .unwrap();
        // The destination is guarded: the read ends the guard instead of failing on it.
        memory.guard(at, 4096, 0xD5).unwrap();

        let mut read = request(0, file.cast(), 1000, 200_000);
        read.destination = at as *mut c_void;
        read.destination_size = 200_000;
        assert_eq!(run(&[read]), S_OK);

        // SAFETY: the memory was committed above for more than the bytes read.
        let got = unsafe { std::slice::from_raw_parts(at as *const u8, 200_000) };
        assert!(got == &data[1000..201_000]);
        assert!(memory.take_written().contains(&0xD5));
        // SAFETY: the file object is not used again.
        unsafe { FileObject::release(file) };
        memory.release(at).unwrap();
    }

    #[test]
    fn a_zlib_request_is_inflated_from_a_file_and_from_memory() {
        let data = pattern(150_000);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&data, 6);
        let planted = Planted::new("zlib", &packed);
        let file = planted.open();

        let mut from_file = vec![0u8; data.len()];
        let mut first = request(OPTION_ZLIB, file.cast(), 0, packed.len() as u32);
        first.destination = from_file.as_mut_ptr().cast();
        first.destination_size = data.len() as u32;

        let mut from_memory = vec![0u8; data.len()];
        let mut second = request(
            OPTION_ZLIB | OPTION_MEMORY_SOURCE,
            packed.as_ptr().cast(),
            0,
            packed.len() as u32,
        );
        second.destination = from_memory.as_mut_ptr().cast();
        second.destination_size = data.len() as u32;

        assert_eq!(run(&[first, second]), S_OK);
        assert!(from_file == data && from_memory == data);
        // SAFETY: the file object is not used again.
        unsafe { FileObject::release(file) };
    }

    #[test]
    fn a_request_can_write_to_physical_pages_from_part_way_into_the_first() {
        let data = pattern(100_000);
        let planted = Planted::new("pages", &data);
        let file = planted.open();
        let memory = crate::xmem::memory();
        let frames = memory.allocate_frames(2).unwrap();
        let pages: Vec<usize> = frames
            .iter()
            .map(|&frame| frame as usize * PAGES_PER_FRAME)
            .collect();

        let mut read = request(OPTION_PAGE_DESTINATION, file.cast(), 0, 100_000);
        read.destination = pages.as_ptr().cast_mut().cast();
        read.page_offset = 0x1234_0000 | 500;
        read.destination_size = 100_000;
        assert_eq!(run(&[read]), S_OK);

        // Mapped afterwards, the pages hold the file from byte 500 on.
        let range = memory.reserve(2 * FRAME, ANYWHERE).unwrap();
        memory
            .map_frames(range, &frames, Access::ReadWrite)
            .unwrap();
        // SAFETY: two frames are mapped at `range`.
        let got = unsafe { std::slice::from_raw_parts((range + 500) as *const u8, 100_000) };
        assert!(got == data.as_slice());
        memory.release(range).unwrap();
        memory.free_frames(&frames).unwrap();
        // SAFETY: the file object is not used again.
        unsafe { FileObject::release(file) };
    }

    #[test]
    fn a_status_carries_the_first_failure_before_it_and_the_next_starts_clean() {
        let data = pattern(1000);
        let planted = Planted::new("failure", &data);
        let file = planted.open();
        let mut good_buffer = vec![0u8; 100];
        let mut good = request(0, file.cast(), 0, 100);
        good.destination = good_buffer.as_mut_ptr().cast();
        good.destination_size = 100;
        // Past the end of the file.
        let mut bad_buffer = vec![0u8; 100];
        let mut bad = request(0, file.cast(), 5000, 100);
        bad.destination = bad_buffer.as_mut_ptr().cast();
        bad.destination_size = 100;
        // Not a zlib stream.
        let mut worse_buffer = vec![0u8; 100];
        let mut worse = request(OPTION_ZLIB, file.cast(), 0, 100);
        worse.destination = worse_buffer.as_mut_ptr().cast();
        worse.destination_size = 100;

        let (queue, array) = (queue(), statuses(2));
        // SAFETY: objects made above, used as the title uses them, then released.
        unsafe {
            queue_enqueue_request(queue, &good);
            queue_enqueue_request(queue, &bad);
            queue_enqueue_request(queue, &worse);
            queue_enqueue_status(queue, array, 0);
            queue_enqueue_request(queue, &good);
            queue_enqueue_status(queue, array, 1);
            queue_submit(queue);
            queue_close(queue);
            assert_eq!(status_result(array, 0), END_OF_FILE);
            assert_eq!(status_result(array, 1), S_OK);
            assert_eq!(status_result(array, 9), E_INVALIDARG);
            QueueObject::release(queue);
            StatusObject::release(array);
            FileObject::release(file);
        }
        assert!(good_buffer == data[..100]);
    }

    #[test]
    fn nothing_runs_before_submit_and_a_missing_file_is_refused() {
        let mut buffer = vec![0u8; 16];
        let source = pattern(16);
        let mut read = request(OPTION_MEMORY_SOURCE, source.as_ptr().cast(), 0, 16);
        read.destination = buffer.as_mut_ptr().cast();
        read.destination_size = 16;
        let (queue, array) = (queue(), statuses(1));
        // SAFETY: objects made above, used as the title uses them, then released.
        unsafe {
            queue_enqueue_request(queue, &read);
            queue_enqueue_status(queue, array, 0);
            std::thread::sleep(std::time::Duration::from_millis(30));
            assert!(!status_is_complete(array, 0));
            assert!(buffer.iter().all(|&byte| byte == 0));
            queue_submit(queue);
            queue_close(queue);
            assert_eq!(status_result(array, 0), S_OK);
            QueueObject::release(queue);
            StatusObject::release(array);
        }
        assert!(buffer == source);

        let missing: Vec<u16> = r"C:\riptide-no-such-folder\nothing.bin"
            .encode_utf16()
            .chain([0])
            .collect();
        let mut file = std::ptr::null_mut();
        // SAFETY: a null-terminated path and a valid out-pointer.
        let opened = unsafe {
            open_file(
                std::ptr::null_mut(),
                missing.as_ptr(),
                std::ptr::null(),
                &mut file,
            )
        };
        assert!(opened < 0 && file.is_null());
    }

    #[test]
    fn an_interface_id_reads_as_it_is_written() {
        let id = [
            0x11, 0x9d, 0x95, 0x62, 0x5f, 0xd4, 0x84, 0x48, 0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2,
            0x43, 0xae,
        ];
        assert_eq!(guid(Some(&id)), "62959d11-d45f-4884-a0a3-f7862ea243ae");
        assert_eq!(guid(None), "null");
    }

    #[test]
    fn the_factory_is_handed_out_and_takes_its_staging_size() {
        let mut factory = 0usize;
        // SAFETY: a valid out-pointer; the method called is the one built at +0x40.
        unsafe {
            assert_eq!(get_factory(std::ptr::null(), &mut factory), S_OK);
            let refused = get_factory(std::ptr::null(), std::ptr::null_mut());
            assert_eq!(refused, E_POINTER);
            let table = *(factory as *const *const usize);
            let set = std::mem::transmute::<usize, extern "system" fn(*mut c_void, u32) -> i32>(
                *table.add(8),
            );
            assert_eq!(set(factory as *mut c_void, 0x200_0000), S_OK);
        }
    }
}
