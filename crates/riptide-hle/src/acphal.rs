//! `AcpHal.dll`: the console's audio hardware, as a title's own audio engine drives it.
//!
//! The console has an audio processor that decodes and mixes sound. A title gives it
//! memory from a heap of its own, in which every block has a second, 32-bit address the
//! processor uses; lays out arrays of per-voice contexts there; and then sends the
//! processor commands and reads its messages through one object.
//!
//! Built: the heap with both kinds of address, the context arrays, and the object. A
//! command is acknowledged at once: message kinds are registered, a flowgraph is reported
//! complete, a command with an id gets its completion message, and its notification word is
//! set. **No sound is decoded or mixed**: the contexts are memory the title writes and
//! nothing reads, so the title runs silent.

use std::alloc::Layout;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Instant;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::trace::trace;

const MODULE: &str = "AcpHal.dll";

const S_OK: i32 = 0;
const E_FAIL: i32 = 0x8000_4005_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: i32 = 0x8007_000e_u32 as i32;
/// What `ApuCreateHeap` answers the second time.
const ALREADY_ALLOCATED: i32 = 0x8aca_0001_u32 as i32;

/// The first processor address handed out. Zero means "none" to a title.
const FIRST_ADDRESS: u32 = 0x0001_0000;

/// One block of the heap.
struct Block {
    address: u32,
    layout: Layout,
}

/// The audio heap: its blocks by host address, and the processor addresses not in use.
struct Heap {
    created: bool,
    blocks: BTreeMap<usize, Block>,
    /// Host addresses of blocks by processor address.
    by_address: BTreeMap<u32, usize>,
    /// Unused ranges of processor addresses: start to end.
    free: BTreeMap<u32, u32>,
}

static HEAP: Mutex<Heap> = Mutex::new(Heap {
    created: false,
    blocks: BTreeMap::new(),
    by_address: BTreeMap::new(),
    free: BTreeMap::new(),
});

fn heap() -> std::sync::MutexGuard<'static, Heap> {
    HEAP.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Heap {
    fn start(&mut self) {
        if !self.created {
            self.created = true;
            self.free.insert(FIRST_ADDRESS, u32::MAX);
        }
    }

    /// Takes `bytes` of processor addresses aligned to `alignment`: the first range that
    /// has room.
    fn take(&mut self, bytes: u32, alignment: u32) -> Option<u32> {
        self.start();
        let (start, end, address) = self.free.iter().find_map(|(&start, &end)| {
            let address = start.checked_next_multiple_of(alignment)?;
            (address.checked_add(bytes)? <= end).then_some((start, end, address))
        })?;
        self.free.remove(&start);
        if start < address {
            self.free.insert(start, address);
        }
        if address + bytes < end {
            self.free.insert(address + bytes, end);
        }
        Some(address)
    }

    /// Gives a range of processor addresses back, joined to its free neighbours.
    fn give_back(&mut self, mut start: u32, mut end: u32) {
        if let Some((&before, &before_end)) = self.free.range(..start).next_back()
            && before_end == start
        {
            self.free.remove(&before);
            start = before;
        }
        if let Some(after_end) = self.free.remove(&end) {
            end = after_end;
        }
        self.free.insert(start, end);
    }

    /// The processor address of any byte of a block, or zero.
    fn address_of(&self, host: usize) -> u32 {
        match self.blocks.range(..=host).next_back() {
            Some((&start, block)) if host < start + block.layout.size() => {
                block.address + (host - start) as u32
            }
            _ => 0,
        }
    }

    /// The host address of a processor address in a block, or null.
    fn host_of(&self, address: u32) -> *mut u8 {
        let Some((&start, &host)) = self.by_address.range(..=address).next_back() else {
            return std::ptr::null_mut();
        };
        let offset = (address - start) as usize;
        if offset < self.blocks[&host].layout.size() {
            (host + offset) as *mut u8
        } else {
            std::ptr::null_mut()
        }
    }
}

/// A zeroed block of the heap: its host address and its processor address.
fn allocate(bytes: usize, alignment: usize) -> Option<(*mut u8, u32)> {
    let layout = Layout::from_size_align(bytes.max(1), alignment.max(16)).ok()?;
    let (size, align) = (
        u32::try_from(layout.size()).ok()?,
        u32::try_from(layout.align()).ok()?,
    );
    let mut heap = heap();
    let address = heap.take(size, align)?;
    // SAFETY: the layout has a size of at least one byte.
    let host = unsafe { std::alloc::alloc_zeroed(layout) };
    if host.is_null() {
        heap.give_back(address, address + size);
        return None;
    }
    heap.blocks.insert(host as usize, Block { address, layout });
    heap.by_address.insert(address, host as usize);
    Some((host, address))
}

fn free(host: *mut u8) {
    let mut heap = heap();
    let Some(block) = heap.blocks.remove(&(host as usize)) else {
        return;
    };
    heap.by_address.remove(&block.address);
    heap.give_back(block.address, block.address + block.layout.size() as u32);
    drop(heap);
    // SAFETY: the block was allocated by `allocate` with this layout.
    unsafe { std::alloc::dealloc(host, block.layout) };
}

/// `ApuCreateHeap`: `(cached bytes, uncached bytes)`. The console sets both parts aside at
/// once. Blocks are allocated one by one here, so the sizes only say the call is valid.
extern "system" fn create_heap(cached: u32, uncached: u32) -> i32 {
    if cached == 0 {
        return E_INVALIDARG;
    }
    let mut heap = heap();
    if heap.created {
        return ALREADY_ALLOCATED;
    }
    heap.start();
    trace!("audio heap made ({cached:#x} cached and {uncached:#x} uncached bytes asked for)");
    S_OK
}

/// `ApuAlloc`: `(host address out, processor address out, bytes, alignment, flags)`.
///
/// # Safety
/// `out` is null or where the host address goes; `address` likewise for the processor's.
unsafe extern "system" fn alloc(
    out: *mut *mut c_void,
    address: *mut u32,
    bytes: u32,
    alignment: u32,
    _flags: u32,
) -> i32 {
    if out.is_null() || bytes == 0 || alignment < 4 || !alignment.is_power_of_two() {
        return E_INVALIDARG;
    }
    let Some((host, processor)) = allocate(bytes as usize, alignment as usize) else {
        return E_OUTOFMEMORY;
    };
    // SAFETY: checked non-null; the second pointer is optional.
    unsafe {
        out.write(host.cast());
        if let Some(address) = address.as_mut() {
            *address = processor;
        }
    }
    S_OK
}

/// `ApuFree`.
extern "system" fn apu_free(host: *mut c_void) {
    free(host.cast());
}

/// `ApuMapVirtualAddress`: the processor address of a host address in the heap, or zero.
extern "system" fn map_virtual_address(host: *const c_void) -> u32 {
    heap().address_of(host as usize)
}

/// `ApuMapApuAddress`: the host address of a processor address in the heap, or null.
extern "system" fn map_apu_address(address: u32) -> *mut c_void {
    heap().host_of(address).cast()
}

/// `ApuIsVirtualAddressValid`: `(host address, alignment)`.
extern "system" fn is_virtual_address_valid(host: *const c_void, alignment: u32) -> bool {
    let address = heap().address_of(host as usize);
    address != 0 && (alignment == 0 || address.is_multiple_of(alignment))
}

/// What a title asks context arrays with: six counts (rate conversion, equaliser and
/// compressor, filter and volume, memory access, XMA decoding, PCM), then room for the six
/// arrays and for their processor addresses.
#[repr(C, packed)]
struct ShapeContexts {
    counts: [u32; 6],
    arrays: [*mut c_void; 6],
    addresses: [u32; 6],
}

/// The bytes one context of each kind takes. The first, fourth and fifth are known from
/// what titles clear; the others are upper bounds.
const CONTEXT_BYTES: [usize; 6] = [32, 256, 256, 16, 64, 256];

/// The context arrays, while they are allocated.
static CONTEXTS: Mutex<Option<[usize; 6]>> = Mutex::new(None);

/// `AcpHalAllocateShapeContexts`: `(counts in, arrays and addresses out)`.
///
/// # Safety
/// `contexts` is null or the title's request.
unsafe extern "system" fn allocate_shape_contexts(contexts: *mut ShapeContexts) -> i32 {
    if contexts.is_null() {
        return E_INVALIDARG;
    }
    let mut allocated = CONTEXTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if allocated.is_some() {
        return E_FAIL;
    }
    // SAFETY: the title's request; its fields are not aligned and are copied out.
    let counts = unsafe { (&raw const (*contexts).counts).read_unaligned() };
    let mut arrays = [std::ptr::null_mut::<c_void>(); 6];
    let mut addresses = [0u32; 6];
    for kind in 0..6 {
        if counts[kind] == 0 {
            continue;
        }
        let Some((host, address)) = allocate(counts[kind] as usize * CONTEXT_BYTES[kind], 256)
        else {
            for array in arrays {
                free(array.cast());
            }
            return E_OUTOFMEMORY;
        };
        (arrays[kind], addresses[kind]) = (host.cast(), address);
    }
    // SAFETY: as above, written back unaligned.
    unsafe {
        (&raw mut (*contexts).arrays).write_unaligned(arrays);
        (&raw mut (*contexts).addresses).write_unaligned(addresses);
    }
    *allocated = Some(arrays.map(|array| array as usize));
    trace!("audio contexts allocated: {counts:?}; nothing decodes or mixes them");
    S_OK
}

/// `AcpHalReleaseShapeContexts`.
extern "system" fn release_shape_contexts() -> i32 {
    let taken = CONTEXTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    match taken {
        Some(arrays) => {
            for array in arrays {
                free(array as *mut u8);
            }
            S_OK
        }
        None => E_FAIL,
    }
}

/// Commands that are understood: load a flowgraph, and register for or stop kinds of
/// message.
const LOAD_FLOWGRAPH: u32 = 1;
const REGISTER_MESSAGES: u32 = 2;
const UNREGISTER_MESSAGES: u32 = 3;

/// Kinds of message.
const FLOWGRAPH_COMPLETED: u32 = 0x02;
const COMMAND_COMPLETED: u32 = 0x10;
const DISCONNECTED: u32 = 0x80;

/// The processor works in frames of 128 samples at 48 kHz.
const FRAMES_PER_SECOND: u64 = 375;

/// A message from the processor, as a title reads it.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Message {
    kind: u32,
    /// Messages lost to a full queue before this one.
    dropped: u32,
    microseconds: u32,
    data: [u8; 16],
}

struct State {
    connected: bool,
    /// The kinds of message the title registered for.
    kinds: u32,
    messages: VecDeque<Message>,
    capacity: usize,
    dropped: u32,
    since: Instant,
}

impl State {
    fn microseconds(&self) -> u64 {
        self.since.elapsed().as_micros() as u64
    }

    /// Queues a message of a kind the title registered for. A full queue loses it and
    /// counts the loss in the next one.
    fn post(&mut self, kind: u32, data: &[u8]) {
        if self.kinds & kind == 0 {
            return;
        }
        if self.messages.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let mut message = Message {
            kind,
            dropped: std::mem::take(&mut self.dropped),
            microseconds: self.microseconds() as u32,
            data: [0; 16],
        };
        message.data[..data.len()].copy_from_slice(data);
        self.messages.push_back(message);
    }
}

/// The object a title drives the processor through: a table of six methods, then ours.
#[repr(C)]
struct Processor {
    table: &'static [usize; 6],
    state: Mutex<State>,
}

fn state(this: &Processor) -> std::sync::MutexGuard<'_, State> {
    this.state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connect: `(command queue size, message queue size)`.
extern "system" fn connect(this: &Processor, commands: u32, messages: u32) -> i32 {
    if commands == 0 {
        return E_INVALIDARG;
    }
    let mut state = state(this);
    state.connected = true;
    state.capacity = messages as usize;
    S_OK
}

extern "system" fn disconnect(this: &Processor) -> i32 {
    let mut state = state(this);
    if !state.connected {
        return E_FAIL;
    }
    state.post(DISCONNECTED, &[]);
    state.connected = false;
    S_OK
}

/// Submit: `(command, id, frame, data, notification word's processor address)`. Done at
/// once, as far as anything is done.
///
/// # Safety
/// `data` is null or the command's words.
unsafe extern "system" fn submit(
    this: &Processor,
    command: u32,
    id: u64,
    _frame: u32,
    data: *const u32,
    notification: u32,
) -> i32 {
    static SEEN: Mutex<u64> = Mutex::new(0);
    if command < 64 {
        let mut seen = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *seen & (1 << command) == 0 {
            *seen |= 1 << command;
            trace!("audio processor: the first command of kind {command}");
        }
    }
    // SAFETY: a command's data starts with 32-bit words; null has none.
    let word = |at: usize| unsafe { data.as_ref().map(|_| data.add(at).read_unaligned()) };
    let mut state = state(this);
    match command {
        REGISTER_MESSAGES => state.kinds |= word(0).unwrap_or(0),
        UNREGISTER_MESSAGES => state.kinds &= !word(0).unwrap_or(0),
        LOAD_FLOWGRAPH => {
            let flowgraph = word(1).unwrap_or(0);
            state.post(FLOWGRAPH_COMPLETED, &flowgraph.to_le_bytes());
        }
        // Every other command changes what is decoded or mixed, and nothing is.
        _ => {}
    }
    if id != 0 {
        let frame = (state.microseconds() * FRAMES_PER_SECOND / 1_000_000) as u32;
        let mut done = [0u8; 16];
        done[..4].copy_from_slice(&command.to_le_bytes());
        done[4..12].copy_from_slice(&id.to_le_bytes());
        done[12..].copy_from_slice(&frame.to_le_bytes());
        state.post(COMMAND_COMPLETED, &done);
    }
    drop(state);
    if notification != 0 {
        let at = heap().host_of(notification);
        if !at.is_null() {
            // SAFETY: a word in a heap block, which the title reads to learn the command
            // is done.
            unsafe { at.cast::<u32>().write_volatile(1) };
        }
    }
    S_OK
}

/// Pop a message: `(message out)`, returning whether there was one.
///
/// # Safety
/// `out` is null or room for a message.
unsafe extern "system" fn pop_message(this: &Processor, out: *mut Message) -> bool {
    let Some(message) = state(this).messages.pop_front() else {
        return false;
    };
    if !out.is_null() {
        // SAFETY: the title passes room for one message.
        unsafe { out.write_unaligned(message) };
    }
    true
}

extern "system" fn message_count(this: &Processor) -> u32 {
    state(this).messages.len() as u32
}

/// # Safety
/// `this` came from [`create`] and is released once.
unsafe extern "system" fn release(this: *mut Processor) {
    // SAFETY: made by `Box::into_raw` in `create`.
    drop(unsafe { Box::from_raw(this) });
}

static METHODS: std::sync::OnceLock<[usize; 6]> = std::sync::OnceLock::new();

/// `AcpHalCreate`: `(processor out)`.
///
/// # Safety
/// `out` is null or where the object goes.
unsafe extern "system" fn create(out: *mut *mut Processor) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let table = METHODS.get_or_init(|| {
        [
            connect as *const () as usize,
            disconnect as *const () as usize,
            submit as *const () as usize,
            pop_message as *const () as usize,
            message_count as *const () as usize,
            release as *const () as usize,
        ]
    });
    let processor = Box::new(Processor {
        table,
        state: Mutex::new(State {
            connected: false,
            kinds: 0,
            messages: VecDeque::new(),
            capacity: 0,
            dropped: 0,
            since: Instant::now(),
        }),
    });
    trace!("audio processor made; the title runs silent, since nothing decodes or mixes");
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(processor)) };
    S_OK
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 9] = [
        ("ApuCreateHeap", create_heap as *const () as usize),
        ("ApuAlloc", alloc as *const () as usize),
        ("ApuFree", apu_free as *const () as usize),
        (
            "ApuMapVirtualAddress",
            map_virtual_address as *const () as usize,
        ),
        ("ApuMapApuAddress", map_apu_address as *const () as usize),
        (
            "ApuIsVirtualAddressValid",
            is_virtual_address_valid as *const () as usize,
        ),
        (
            "AcpHalAllocateShapeContexts",
            allocate_shape_contexts as *const () as usize,
        ),
        (
            "AcpHalReleaseShapeContexts",
            release_shape_contexts as *const () as usize,
        ),
        ("AcpHalCreate", create as *const () as usize),
    ];
    for (name, address) in functions {
        // SAFETY: each function has the arguments of the console function it is bound as.
        unsafe { imports.implement(MODULE, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_has_two_addresses_that_map_to_each_other() {
        let (mut host, mut address) = (std::ptr::null_mut(), 0);
        // SAFETY: valid out-pointers, and the null one under test.
        unsafe {
            assert_eq!(alloc(&mut host, &mut address, 1000, 256, 0), S_OK);
            assert_eq!(
                alloc(std::ptr::null_mut(), &mut address, 8, 16, 0),
                E_INVALIDARG
            );
            assert_eq!(
                alloc(&mut host, std::ptr::null_mut(), 8, 3, 0),
                E_INVALIDARG
            );
        }
        assert!(address >= FIRST_ADDRESS && address.is_multiple_of(256));
        let inside = host.cast::<u8>().wrapping_add(100).cast::<c_void>();
        assert_eq!(map_virtual_address(inside), address + 100);
        assert_eq!(map_apu_address(address + 100), inside);
        assert!(is_virtual_address_valid(host, 256));
        assert!(!is_virtual_address_valid(inside, 256));
        // A fresh block reads as zero.
        // SAFETY: 1000 bytes were handed out.
        let block = unsafe { std::slice::from_raw_parts(host.cast::<u8>(), 1000) };
        assert!(block.iter().all(|&byte| byte == 0));
        apu_free(host);
        assert_eq!(map_virtual_address(inside), 0);
        assert!(map_apu_address(address).is_null());
    }

    #[test]
    fn freed_addresses_join_their_neighbours() {
        let mut heap = Heap {
            created: false,
            blocks: BTreeMap::new(),
            by_address: BTreeMap::new(),
            free: BTreeMap::new(),
        };
        let first = heap.take(0x100, 0x100).unwrap();
        let second = heap.take(0x100, 0x100).unwrap();
        let third = heap.take(0x100, 0x100).unwrap();
        assert_eq!((second, third), (first + 0x100, first + 0x200));
        heap.give_back(first, first + 0x100);
        heap.give_back(third, third + 0x100);
        heap.give_back(second, second + 0x100);
        assert_eq!(heap.free, BTreeMap::from([(FIRST_ADDRESS, u32::MAX)]));
    }

    #[test]
    fn the_processor_acknowledges_commands_with_the_messages_asked_for() {
        let mut made = std::ptr::null_mut();
        // SAFETY: a valid out-pointer; the object is used as made and released once.
        unsafe {
            assert_eq!(create(&mut made), S_OK);
            let this = &*made;
            assert_eq!(connect(this, 0, 4), E_INVALIDARG);
            assert_eq!(connect(this, 64, 4), S_OK);
            // Nothing registered: a completed command leaves no message.
            assert_eq!(submit(this, 9, 77, 0, std::ptr::null(), 0), S_OK);
            assert_eq!(message_count(this), 0);

            let kinds = [COMMAND_COMPLETED | FLOWGRAPH_COMPLETED];
            assert_eq!(
                submit(this, REGISTER_MESSAGES, 0, 0, kinds.as_ptr(), 0),
                S_OK
            );
            let flowgraph = [0u32, 0x1234];
            assert_eq!(
                submit(this, LOAD_FLOWGRAPH, 5, 0, flowgraph.as_ptr(), 0),
                S_OK
            );
            assert_eq!(message_count(this), 2);

            let mut message = std::mem::zeroed::<Message>();
            assert!(pop_message(this, &mut message));
            assert_eq!({ message.kind }, FLOWGRAPH_COMPLETED);
            assert_eq!(message.data[..4], 0x1234u32.to_le_bytes());
            assert!(pop_message(this, &mut message));
            assert_eq!({ message.kind }, COMMAND_COMPLETED);
            assert_eq!(message.data[4..12], 5u64.to_le_bytes());
            assert!(!pop_message(this, &mut message));

            assert_eq!(disconnect(this), S_OK);
            assert_eq!(disconnect(this), E_FAIL);
            release(made);
        }
    }
}
