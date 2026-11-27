//! Title threads on their own cores (decision D8).
//!
//! The console gives a title seven cores. Riptide gives each of them one physical core of
//! the host, and keeps what is left for itself: the remaining cores, and the second hardware
//! thread of the title's cores. On the Ryzen 7 7800X3D that is seven cores for the title,
//! and for Riptide the eighth core plus seven sibling threads.
//!
//! A title names cores by the console's numbers, 0 to 6, in affinity masks and processor
//! numbers. [`CoreMap`] turns those into the host's and back.

use std::ffi::c_void;
use std::sync::OnceLock;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::sys;

/// How many cores the console gives a title.
pub const TITLE_CORES: usize = 7;

const CREATE_SUSPENDED: u32 = 0x4;

/// One physical core of the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalCore {
    /// Its hardware threads, as a mask of logical processors.
    pub mask: u64,
    /// Windows' efficiency class: higher is faster. All cores of a machine with one kind of
    /// core have the same class.
    pub efficiency_class: u8,
}

/// The host's physical cores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    pub cores: Vec<PhysicalCore>,
}

impl Topology {
    /// Reads this machine's cores from Windows. Only the first processor group is read: 64
    /// logical processors, which covers every desktop processor.
    ///
    /// If Windows will not say, the answer is one core with one thread.
    pub fn read() -> Topology {
        let cores = read_cores().unwrap_or_default();
        if cores.is_empty() {
            return Topology {
                cores: vec![PhysicalCore {
                    mask: 1,
                    efficiency_class: 0,
                }],
            };
        }
        Topology { cores }
    }
}

fn read_cores() -> Option<Vec<PhysicalCore>> {
    let mut length = 0u32;
    // SAFETY: a null buffer with a length of zero asks only for the length needed.
    unsafe {
        sys::GetLogicalProcessorInformationEx(
            sys::RELATION_PROCESSOR_CORE,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0u8; length as usize];
    // SAFETY: the buffer is `length` bytes, as the call was just told.
    let read = unsafe {
        sys::GetLogicalProcessorInformationEx(
            sys::RELATION_PROCESSOR_CORE,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if read == 0 {
        return None;
    }
    Some(parse_cores(&buffer[..length as usize]))
}

/// Reads `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` records for processor cores. Each is:
/// relationship (4 bytes), size (4), flags (1), efficiency class (1), reserved (20), group
/// count (2), then per group a mask (8), a group number (2) and padding (6).
fn parse_cores(mut records: &[u8]) -> Vec<PhysicalCore> {
    let mut cores = Vec::new();
    while records.len() >= 48 {
        let size = u32::from_le_bytes(records[4..8].try_into().expect("four bytes")) as usize;
        if size < 48 || size > records.len() {
            break;
        }
        let relationship = u32::from_le_bytes(records[0..4].try_into().expect("four bytes"));
        let group = u16::from_le_bytes(records[40..42].try_into().expect("two bytes"));
        if relationship == sys::RELATION_PROCESSOR_CORE as u32 && group == 0 {
            cores.push(PhysicalCore {
                mask: u64::from_le_bytes(records[32..40].try_into().expect("eight bytes")),
                efficiency_class: records[9],
            });
        }
        records = &records[size..];
    }
    cores
}

/// Which host processors the title's cores are, and which are Riptide's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreMap {
    /// For each console core, the one host logical processor its threads run on.
    title: [u64; TITLE_CORES],
    /// For each console core, every hardware thread of the physical core it is on.
    physical: [u64; TITLE_CORES],
    /// The logical processors Riptide's own threads may use.
    riptide: u64,
}

impl CoreMap {
    /// Lays the title's cores over the host's.
    ///
    /// The fastest cores go first (by efficiency class, then in Windows' order). Console
    /// core `n` gets the first hardware thread of the `n`th of them. A host with fewer than
    /// eight physical cores cannot give seven away and keep one: its cores are then shared,
    /// console cores wrapping around, which runs but is not what Riptide is built for.
    ///
    /// Riptide gets every logical processor no console core is on. If that is none, it
    /// gets all of them.
    pub fn new(topology: &Topology) -> CoreMap {
        let mut cores: Vec<PhysicalCore> = topology
            .cores
            .iter()
            .copied()
            .filter(|core| core.mask != 0)
            .collect();
        if cores.is_empty() {
            cores.push(PhysicalCore {
                mask: 1,
                efficiency_class: 0,
            });
        }
        cores.sort_by_key(|core| {
            (
                std::cmp::Reverse(core.efficiency_class),
                core.mask.trailing_zeros(),
            )
        });

        let mut title = [0u64; TITLE_CORES];
        let mut physical = [0u64; TITLE_CORES];
        for console_core in 0..TITLE_CORES {
            let core = cores[console_core % cores.len()];
            title[console_core] = 1 << core.mask.trailing_zeros();
            physical[console_core] = core.mask;
        }
        let every = cores.iter().fold(0, |all, core| all | core.mask);
        let all_title = title.iter().fold(0, |all, bit| all | bit);
        let riptide = match every & !all_title {
            0 => every,
            rest => rest,
        };
        CoreMap {
            title,
            physical,
            riptide,
        }
    }

    /// The host logical processor console core `console_core` runs on, as a mask of one bit.
    pub fn title_core(&self, console_core: usize) -> u64 {
        self.title[console_core]
    }

    /// Every host logical processor a title thread may run on.
    pub fn all_title(&self) -> u64 {
        self.title.iter().fold(0, |all, bit| all | bit)
    }

    /// The host logical processors Riptide's own threads may use.
    pub fn riptide(&self) -> u64 {
        self.riptide
    }

    /// Turns an affinity mask in the console's core numbers into the host's. Bits past the
    /// title's cores are dropped. A mask with none of the title's cores means all of them:
    /// Windows refuses an empty mask, and a thread has to run somewhere.
    pub fn host_mask(&self, console_mask: u64) -> u64 {
        let host = (0..TITLE_CORES)
            .filter(|core| console_mask & (1 << core) != 0)
            .fold(0, |mask, core| mask | self.title[core]);
        if host == 0 { self.all_title() } else { host }
    }

    /// Turns a host affinity mask back into the console's core numbers.
    pub fn console_mask(&self, host_mask: u64) -> u64 {
        (0..TITLE_CORES)
            .filter(|&core| host_mask & self.title[core] != 0)
            .fold(0, |mask, core| mask | (1 << core))
    }

    /// The console core a host logical processor counts as: the one that runs on it, or
    /// failing that the one on the same physical core, or failing that core 0.
    pub fn console_core(&self, host_processor: u32) -> u32 {
        let bit = 1u64.checked_shl(host_processor).unwrap_or(0);
        let on = |masks: &[u64; TITLE_CORES]| masks.iter().position(|mask| mask & bit != 0);
        on(&self.title).or_else(|| on(&self.physical)).unwrap_or(0) as u32
    }

    /// The host logical processor console core `console_core` runs on, as a number.
    fn host_processor(&self, console_core: usize) -> u32 {
        self.title[console_core.min(TITLE_CORES - 1)].trailing_zeros()
    }
}

/// This machine's core map, read once.
pub fn core_map() -> &'static CoreMap {
    static MAP: OnceLock<CoreMap> = OnceLock::new();
    MAP.get_or_init(|| CoreMap::new(&Topology::read()))
}

/// Keeps the calling thread, one of Riptide's own, off the title's processors. Returns
/// false if Windows refuses.
pub fn pin_riptide_thread() -> bool {
    // SAFETY: the pseudo-handle for the current thread is always valid.
    unsafe {
        sys::SetThreadAffinityMask(sys::GetCurrentThread(), core_map().riptide() as usize) != 0
    }
}

/// The title's `CreateThread`: as Windows' own, and the new thread may run on the title's
/// processors only, as a new thread on the console may run on any of the title's cores.
///
/// # Safety
///
/// As `CreateThread`: `start` is a thread start routine and `parameter` is what it expects.
pub unsafe extern "system" fn create_thread(
    attributes: *const c_void,
    stack_size: usize,
    start: *const c_void,
    parameter: *const c_void,
    flags: u32,
    thread_id: *mut u32,
) -> *mut c_void {
    // SAFETY: the title's arguments go to Windows unchanged; the thread is only made to
    // start suspended, so its affinity is set before it runs a single instruction.
    unsafe {
        let thread = sys::CreateThread(
            attributes,
            stack_size,
            start,
            parameter,
            flags | CREATE_SUSPENDED,
            thread_id,
        );
        if thread.is_null() {
            return thread;
        }
        sys::SetThreadAffinityMask(thread, core_map().all_title() as usize);
        if flags & CREATE_SUSPENDED == 0 {
            sys::ResumeThread(thread);
        }
        thread
    }
}

/// The title's `SetThreadAffinityMask`: the mask is in the console's core numbers, and so
/// is the previous mask it returns. Zero means Windows refused, as it does for the title.
///
/// # Safety
///
/// `thread` is whatever the title passes; Windows checks it.
pub unsafe extern "system" fn set_thread_affinity_mask(thread: *mut c_void, mask: usize) -> usize {
    let map = core_map();
    // SAFETY: Windows validates the handle and returns zero for a bad one.
    let previous =
        unsafe { sys::SetThreadAffinityMask(thread, map.host_mask(mask as u64) as usize) };
    map.console_mask(previous as u64) as usize
}

/// The title's `SetThreadIdealProcessor`: the processor is a console core number, and so
/// is the previous one it returns. `u32::MAX` means Windows refused.
///
/// # Safety
///
/// `thread` is whatever the title passes; Windows checks it.
pub unsafe extern "system" fn set_thread_ideal_processor(thread: *mut c_void, core: u32) -> u32 {
    let map = core_map();
    // SAFETY: Windows validates the handle and returns `u32::MAX` for a bad one.
    let previous =
        unsafe { sys::SetThreadIdealProcessor(thread, map.host_processor(core as usize)) };
    if previous == u32::MAX {
        return previous;
    }
    map.console_core(previous)
}

/// The title's `GetCurrentProcessorNumber`: the console core this thread is running on.
pub extern "system" fn get_current_processor_number() -> u32 {
    // SAFETY: the call takes nothing and only reads.
    core_map().console_core(unsafe { sys::GetCurrentProcessorNumber() })
}

/// Adds the thread and core functions to the import table.
pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 4] = [
        ("CreateThread", create_thread as *const () as usize),
        (
            "SetThreadAffinityMask",
            set_thread_affinity_mask as *const () as usize,
        ),
        (
            "SetThreadIdealProcessor",
            set_thread_ideal_processor as *const () as usize,
        ),
        (
            "GetCurrentProcessorNumber",
            get_current_processor_number as *const () as usize,
        ),
    ];
    for (name, address) in functions {
        // SAFETY: each function has the signature of the Windows function it stands in for.
        unsafe { imports.implement("kernel32.dll", Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn core(mask: u64, efficiency_class: u8) -> PhysicalCore {
        PhysicalCore {
            mask,
            efficiency_class,
        }
    }

    /// The target machine: eight cores of one kind, two hardware threads each.
    fn ryzen_7800x3d() -> Topology {
        Topology {
            cores: (0..8).map(|n| core(0b11 << (2 * n), 0)).collect(),
        }
    }

    #[test]
    fn on_the_7800x3d_each_console_core_is_the_first_thread_of_its_own_core() {
        let map = CoreMap::new(&ryzen_7800x3d());
        for console_core in 0..TITLE_CORES {
            assert_eq!(map.title_core(console_core), 1 << (2 * console_core));
        }
        assert_eq!(map.all_title(), 0b0001_0101_0101_0101);
        // Riptide has the eighth core and the second thread of the other seven.
        assert_eq!(map.riptide(), 0b1110_1010_1010_1010);
        assert_eq!(map.all_title() & map.riptide(), 0);
        assert_eq!(map.all_title() | map.riptide(), 0xFFFF);
    }

    #[test]
    fn affinity_masks_convert_both_ways() {
        let map = CoreMap::new(&ryzen_7800x3d());
        assert_eq!(map.host_mask(0b000_0001), 0b1);
        assert_eq!(map.host_mask(0b000_0101), 0b1_0001);
        assert_eq!(map.host_mask(0b111_1111), map.all_title());
        assert_eq!(map.console_mask(0b1_0001), 0b101);
        assert_eq!(map.console_mask(map.all_title()), 0b111_1111);
        // Riptide's processors are no console core's.
        assert_eq!(map.console_mask(map.riptide()), 0);
        for console_mask in 1..(1u64 << TITLE_CORES) {
            assert_eq!(map.console_mask(map.host_mask(console_mask)), console_mask);
        }
    }

    #[test]
    fn a_mask_with_none_of_the_titles_cores_means_all_of_them() {
        let map = CoreMap::new(&ryzen_7800x3d());
        assert_eq!(map.host_mask(0), map.all_title());
        // The console's eighth core is not the title's.
        assert_eq!(map.host_mask(1 << 7), map.all_title());
        assert_eq!(map.host_mask(1 << 7 | 1 << 2), 1 << 4);
    }

    #[test]
    fn a_host_processor_counts_as_the_console_core_on_it_or_beside_it() {
        let map = CoreMap::new(&ryzen_7800x3d());
        assert_eq!(map.console_core(0), 0);
        assert_eq!(map.console_core(6), 3);
        // The second thread of a title core counts as that core.
        assert_eq!(map.console_core(7), 3);
        // Riptide's own core is no console core; the answer is core 0.
        assert_eq!(map.console_core(14), 0);
        assert_eq!(map.console_core(200), 0);
    }

    #[test]
    fn on_a_hybrid_processor_the_fast_cores_go_to_the_title_first() {
        // Six fast cores with one thread each on processors 8 to 13, eight slow ones on 0 to 7.
        let mut cores: Vec<PhysicalCore> = (0..8).map(|n| core(1 << n, 0)).collect();
        cores.extend((8..14).map(|n| core(1 << n, 1)));
        let map = CoreMap::new(&Topology { cores });

        for console_core in 0..6 {
            assert_eq!(map.title_core(console_core), 1 << (8 + console_core));
        }
        // The seventh console core is the first slow one.
        assert_eq!(map.title_core(6), 1);
        assert_eq!(map.riptide(), 0b1111_1110);
    }

    #[test]
    fn with_too_few_cores_console_cores_share_and_riptide_keeps_the_sibling_threads() {
        let four = Topology {
            cores: (0..4).map(|n| core(0b11 << (2 * n), 0)).collect(),
        };
        let map = CoreMap::new(&four);
        assert_eq!(map.title_core(0), 0b1);
        assert_eq!(map.title_core(4), 0b1, "console core 4 shares core 0");
        assert_eq!(map.title_core(6), 0b1_0000);
        assert_eq!(map.riptide(), 0b1010_1010);

        // One core, one thread: everything is shared.
        let one = CoreMap::new(&Topology {
            cores: vec![core(1, 0)],
        });
        assert_eq!(one.all_title(), 1);
        assert_eq!(one.riptide(), 1);
    }

    #[test]
    fn core_records_from_windows_are_read() {
        // Two records as Windows lays them out: a core with processors 0 and 1, class 1,
        // and a core in another group, which is skipped.
        let mut records = vec![0u8; 96];
        for (at, mask, group, class) in [(0, 0b11u64, 0u16, 1u8), (48, 0b1100, 1, 0)] {
            records[at + 4..at + 8].copy_from_slice(&48u32.to_le_bytes());
            records[at + 9] = class;
            records[at + 30..at + 32].copy_from_slice(&1u16.to_le_bytes());
            records[at + 32..at + 40].copy_from_slice(&mask.to_le_bytes());
            records[at + 40..at + 42].copy_from_slice(&group.to_le_bytes());
        }
        assert_eq!(parse_cores(&records), [core(0b11, 1)]);
        assert_eq!(parse_cores(&records[..40]), []);
    }

    #[test]
    fn this_machines_cores_are_read_and_mapped() {
        let topology = Topology::read();
        assert!(!topology.cores.is_empty());
        let map = core_map();
        assert_ne!(map.all_title(), 0);
        assert_ne!(map.riptide(), 0);
        println!(
            "{} physical cores; title {:#x}, riptide {:#x}",
            topology.cores.len(),
            map.all_title(),
            map.riptide()
        );
    }

    /// What the thread started below saw: its affinity mask, in the host's numbers.
    static SEEN_AFFINITY: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn report_affinity(_: *mut c_void) -> u32 {
        // Setting a mask returns the previous one; set it back at once.
        // SAFETY: the pseudo-handle for the current thread is always valid.
        unsafe {
            let thread = sys::GetCurrentThread();
            let previous = sys::SetThreadAffinityMask(thread, 1);
            sys::SetThreadAffinityMask(thread, previous);
            SEEN_AFFINITY.store(previous, Ordering::SeqCst);
        }
        0
    }

    #[test]
    fn a_thread_the_title_creates_runs_on_the_titles_processors_only() {
        // SAFETY: `report_affinity` is a thread start routine that ignores its parameter.
        let thread = unsafe {
            create_thread(
                std::ptr::null(),
                0,
                report_affinity as *const c_void,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(!thread.is_null());
        // SAFETY: `thread` is the live handle just returned; it is closed once.
        unsafe {
            assert_eq!(sys::WaitForSingleObject(thread, 10_000), 0);
            sys::CloseHandle(thread);
        }
        assert_eq!(
            SEEN_AFFINITY.load(Ordering::SeqCst) as u64,
            core_map().all_title()
        );
    }

    #[test]
    fn the_title_sets_and_reads_affinity_in_console_core_numbers() {
        // On a thread of its own, so the test harness's thread keeps its affinity.
        std::thread::spawn(|| {
            let map = core_map();
            // SAFETY: the pseudo-handle for the current thread is always valid.
            unsafe {
                let thread = sys::GetCurrentThread();
                assert_ne!(set_thread_affinity_mask(thread, 0b111_1111), 0);
                // The previous mask comes back in console numbers.
                assert_eq!(set_thread_affinity_mask(thread, 0b000_0100), 0b111_1111);
                // Pinned to console core 2, the thread runs there.
                sys::Sleep(0);
                assert_eq!(
                    get_current_processor_number(),
                    map.console_core(map.host_processor(2))
                );
                assert_eq!(
                    set_thread_affinity_mask(thread, 0b111_1111),
                    map.console_mask(map.title_core(2)) as usize
                );
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_thread_of_riptides_own_keeps_off_the_titles_processors() {
        std::thread::spawn(|| {
            assert!(pin_riptide_thread());
            // SAFETY: the pseudo-handle for the current thread is always valid.
            let previous = unsafe {
                let thread = sys::GetCurrentThread();
                let previous = sys::SetThreadAffinityMask(thread, core_map().riptide() as usize);
                sys::SetThreadAffinityMask(thread, previous);
                previous
            };
            assert_eq!(previous as u64, core_map().riptide());
        })
        .join()
        .unwrap();
    }
}
