//! Mapping a parsed image into this process, relocating it, binding its imports and running
//! it. The title's code runs natively, so a loaded image is real executable memory.

use std::ffi::c_void;
use std::fmt;
use std::io;
use std::ptr::NonNull;

use crate::carrier::Carrier;
use crate::fault;
use crate::parse::{Image, ParseError, Symbol};
use crate::sys;

/// The most TLS callbacks an image may list. More is taken as a missing terminator.
const MAX_TLS_CALLBACKS: usize = 1024;

/// The reason Windows gives a TLS callback when a process starts.
const DLL_PROCESS_ATTACH: u32 = 1;

/// Supplies the address of each function an image imports. `riptide-hle` implements it.
pub trait Resolver {
    /// The address of `symbol` in `module`, or `None` if there is no such function.
    ///
    /// The address is called by title code with the Windows x64 calling convention, with the
    /// arguments the title expects of that function.
    fn resolve(&self, module: &str, symbol: &Symbol) -> Option<usize>;
}

impl<F> Resolver for F
where
    F: Fn(&str, &Symbol) -> Option<usize>,
{
    fn resolve(&self, module: &str, symbol: &Symbol) -> Option<usize> {
        self(module, symbol)
    }
}

/// Why an image could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// The file is not an image this loader accepts.
    Parse(ParseError),
    /// Windows would not give memory for the image.
    Map(io::Error),
    /// Windows would not set a section's protection.
    Protect(io::Error),
    /// The image's preferred base is taken and it carries no relocations.
    NotRelocatable { preferred_base: u64 },
    /// Imports the resolver has no function for, each as `module!function`.
    Unresolved(Vec<String>),
    /// The image needs something that is not built yet.
    Unsupported(&'static str),
    /// The image has no entry point to call.
    NoEntryPoint,
    /// A table that is only readable once mapped points outside the image.
    Malformed(&'static str),
    /// Windows would not take the image's unwind table.
    FunctionTable,
    /// The carrier for the image's thread-local data could not be written or loaded.
    ThreadLocal(io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Parse(error) => write!(f, "not a loadable image: {error}"),
            LoadError::Map(error) => write!(f, "no memory for the image: {error}"),
            LoadError::Protect(error) => write!(f, "could not protect a section: {error}"),
            LoadError::NotRelocatable { preferred_base } => write!(
                f,
                "the image must be mapped at {preferred_base:#x}, which is taken, and it has \
                 no relocations"
            ),
            LoadError::Unresolved(names) => {
                write!(f, "unresolved imports: {}", names.join(", "))
            }
            LoadError::Unsupported(what) => write!(f, "the image uses {what}, not built yet"),
            LoadError::NoEntryPoint => write!(f, "the image has no entry point"),
            LoadError::Malformed(what) => write!(f, "the mapped image's {what} is malformed"),
            LoadError::FunctionTable => write!(f, "Windows refused the image's unwind table"),
            LoadError::ThreadLocal(error) => {
                write!(f, "no thread-local data for the image: {error}")
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Parse(error) => Some(error),
            LoadError::Map(error) | LoadError::Protect(error) | LoadError::ThreadLocal(error) => {
                Some(error)
            }
            _ => None,
        }
    }
}

impl From<ParseError> for LoadError {
    fn from(error: ParseError) -> Self {
        LoadError::Parse(error)
    }
}

/// Memory for one image, given back to Windows when dropped.
struct Mapping {
    base: NonNull<u8>,
    size: usize,
}

impl Mapping {
    /// Commits `size` bytes of zeroed, writable memory, at `preferred` if that is free.
    fn new(preferred: u64, size: usize) -> Result<Mapping, LoadError> {
        let flags = sys::MEM_RESERVE | sys::MEM_COMMIT;
        // SAFETY: VirtualAlloc with a hint address touches no existing memory: it gives new
        // pages or fails.
        let mut base = unsafe {
            sys::VirtualAlloc(preferred as *mut c_void, size, flags, sys::PAGE_READWRITE)
        };
        if base.is_null() {
            // SAFETY: as above, with no hint.
            base = unsafe {
                sys::VirtualAlloc(std::ptr::null_mut(), size, flags, sys::PAGE_READWRITE)
            };
        }
        match NonNull::new(base.cast::<u8>()) {
            Some(base) => Ok(Mapping { base, size }),
            None => Err(LoadError::Map(io::Error::last_os_error())),
        }
    }

    fn address(&self) -> usize {
        self.base.as_ptr() as usize
    }

    /// The mapped bytes. Only used while the whole mapping is still readable and writable.
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `base` points at `size` committed bytes this mapping owns, and the mapping
        // is borrowed mutably for as long as the slice lives.
        unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr(), self.size) }
    }

    fn protect(&self, offset: usize, length: usize, protection: u32) -> Result<(), LoadError> {
        debug_assert!(offset + length <= self.size);
        let mut old = 0u32;
        // SAFETY: the range lies inside this mapping.
        let done = unsafe {
            sys::VirtualProtect(
                self.base.as_ptr().add(offset).cast(),
                length,
                protection,
                &mut old,
            )
        };
        if done == 0 {
            return Err(LoadError::Protect(io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `base` came from VirtualAlloc and is released once, here.
        unsafe { sys::VirtualFree(self.base.as_ptr().cast(), 0, sys::MEM_RELEASE) };
    }
}

/// An image mapped into this process, relocated, bound and ready to run.
///
/// Dropping it unmaps the image. Nothing may still be running in it or pointing into it.
pub struct LoadedImage {
    name: String,
    /// What gives the image its thread-local data. Declared before `mapping` so that it is
    /// dropped first: once it is gone no thread event reaches the image's callbacks.
    _carrier: Option<Carrier>,
    mapping: Mapping,
    entry_rva: u32,
    exports: Vec<(String, u32)>,
    stack_reserve: u64,
    tls_callbacks: Vec<usize>,
    /// The unwind table registered with Windows, to take back on drop.
    function_table: Option<*const c_void>,
}

impl fmt::Debug for LoadedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedImage")
            .field("name", &self.name)
            .field("base", &format_args!("{:#x}", self.base()))
            .field("size", &self.size())
            .finish()
    }
}

/// Maps `bytes` as an image called `name`, relocates it, and binds every import through
/// `resolver`.
///
/// `name` is what a fault in the image is reported under, such as `title.exe`.
///
/// Loading runs none of the image's code. The addresses the resolver returns must stay valid
/// for as long as the image is loaded.
pub fn load(name: &str, bytes: &[u8], resolver: &dyn Resolver) -> Result<LoadedImage, LoadError> {
    let image = Image::parse(bytes)?;
    if image.entry_rva == 0 {
        return Err(LoadError::NoEntryPoint);
    }
    if image.has_delay_imports {
        return Err(LoadError::Unsupported("delay-loaded imports"));
    }

    // Ask the resolver for everything before mapping anything, so the error lists every
    // missing function at once.
    let mut bound = Vec::new();
    let mut unresolved = Vec::new();
    for module in &image.imports {
        for import in &module.imports {
            match resolver.resolve(&module.name, &import.symbol) {
                Some(address) => bound.push((import.slot_rva as usize, address)),
                None => unresolved.push(format!("{}!{}", module.name, import.symbol)),
            }
        }
    }
    if !unresolved.is_empty() {
        return Err(LoadError::Unresolved(unresolved));
    }

    // An image with thread-local data gets one more page after it, for the routines its
    // thread-local accesses are pointed at (see `tls_sites.rs`).
    let image_end = (image.size as usize).next_multiple_of(0x1000);
    let thunks = image.tls.as_ref().map(|_| image_end);
    let mapped_size = image_end + if thunks.is_some() { 0x1000 } else { 0 };
    let mut mapping = Mapping::new(image.preferred_base, mapped_size)?;
    let base = mapping.address();
    let memory = mapping.bytes_mut();

    // The parser has checked that the headers and every section's bytes lie inside both the
    // file and the image.
    let header_size = image.header_size as usize;
    memory[..header_size].copy_from_slice(&bytes[..header_size]);
    for section in &image.sections {
        let from = section.raw_offset as usize;
        let to = section.rva as usize;
        let length = section.raw_size as usize;
        memory[to..to + length].copy_from_slice(&bytes[from..from + length]);
    }

    let delta = (base as u64).wrapping_sub(image.preferred_base);
    if delta != 0 {
        // An image that may be moved and lists no fixups holds no absolute addresses.
        if !image.relocatable {
            return Err(LoadError::NotRelocatable {
                preferred_base: image.preferred_base,
            });
        }
        for &rva in &image.relocations {
            let slot = &mut memory[rva as usize..rva as usize + 8];
            let address = u64::from_le_bytes(slot.try_into().expect("an 8-byte slot"));
            slot.copy_from_slice(&address.wrapping_add(delta).to_le_bytes());
        }
    }

    for (slot_rva, address) in bound {
        memory[slot_rva..slot_rva + 8].copy_from_slice(&(address as u64).to_le_bytes());
    }

    let tls_callbacks = match image.tls.as_ref().and_then(|tls| tls.callbacks_rva) {
        Some(rva) => read_tls_callbacks(memory, base, rva as usize)?,
        None => Vec::new(),
    };

    // Thread-local data. Windows gives a slot in each thread's TLS array only to images it
    // loads itself, so a carrier it does load holds the image's template, and the image
    // takes the carrier's index. See `carrier.rs`.
    let carrier = match &image.tls {
        Some(tls) => {
            let start = tls.template_rva as usize;
            let mut template = memory[start..start + tls.template_size as usize].to_vec();
            template.resize(template.len() + tls.zero_fill as usize, 0);
            let carrier =
                Carrier::load(&template, tls.characteristics).map_err(LoadError::ThreadLocal)?;
            // The image's code is made to read the carrier's entry wherever it reads the
            // TLS array, so the index it adds on top of that is zero.
            let code: Vec<std::ops::Range<usize>> = image
                .sections
                .iter()
                .filter(|section| section.executable)
                .map(|section| {
                    let start = section.rva as usize;
                    start..start + section.virtual_size.max(section.raw_size) as usize
                })
                .collect();
            let thunks = thunks.expect("an image with TLS has the page");
            crate::tls_sites::redirect(memory, &code, thunks, carrier.index());
            let at = tls.index_rva as usize;
            memory[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
            Some(carrier)
        }
        None => None,
    };

    // From here the image is no longer written. Headers become read-only and each section
    // gets the protection it asks for.
    mapping.protect(0, header_size.max(1), sys::PAGE_READONLY)?;
    for section in &image.sections {
        let length = section.virtual_size.max(section.raw_size) as usize;
        if length == 0 {
            continue;
        }
        let protection = match (section.executable, section.writable) {
            (true, true) => sys::PAGE_EXECUTE_READWRITE,
            (true, false) => sys::PAGE_EXECUTE_READ,
            (false, true) => sys::PAGE_READWRITE,
            (false, false) => sys::PAGE_READONLY,
        };
        mapping.protect(section.rva as usize, length, protection)?;
    }
    if let Some(thunks) = thunks {
        mapping.protect(thunks, 0x1000, sys::PAGE_EXECUTE_READ)?;
    }
    // SAFETY: the range is this mapping. The call tells the CPU that new code lies there.
    unsafe {
        sys::FlushInstructionCache(
            sys::GetCurrentProcess(),
            mapping.base.as_ptr().cast(),
            mapping.size,
        )
    };

    // Give Windows the image's unwind table, so exceptions and stack walks pass through
    // title frames as they do for any loaded module.
    let function_table = match image.exception_table {
        Some((rva, length)) => {
            // SAFETY: the parser checked the table lies inside the image; it stays mapped
            // until this LoadedImage is dropped, which takes the table back first.
            let table = unsafe { mapping.base.as_ptr().add(rva as usize) }.cast::<c_void>();
            // SAFETY: `table` holds `length / 12` RUNTIME_FUNCTION entries relative to
            // `base`.
            let added = unsafe { sys::RtlAddFunctionTable(table, length / 12, base as u64) };
            if added == 0 {
                // Without the table an exception in title code cannot unwind; refuse now
                // instead of failing at the title's first exception.
                return Err(LoadError::FunctionTable);
            }
            Some(table.cast_const())
        }
        None => None,
    };

    fault::register(name, base, mapping.size);
    // Only now, with the image whole, are thread starts and ends passed on to its callbacks.
    if let Some(carrier) = &carrier {
        carrier.serve(base, tls_callbacks.clone());
    }
    Ok(LoadedImage {
        name: name.to_owned(),
        _carrier: carrier,
        mapping,
        entry_rva: image.entry_rva,
        exports: image.exports,
        stack_reserve: image.stack_reserve,
        tls_callbacks,
        function_table,
    })
}

/// Reads the image's null-terminated list of TLS callbacks from the mapped, relocated image.
fn read_tls_callbacks(memory: &[u8], base: usize, rva: usize) -> Result<Vec<usize>, LoadError> {
    const WHAT: &str = "TLS callback list";
    let mut callbacks = Vec::new();
    for index in 0..=MAX_TLS_CALLBACKS {
        let at = rva + index * 8;
        let slot = memory.get(at..at + 8).ok_or(LoadError::Malformed(WHAT))?;
        let address = u64::from_le_bytes(slot.try_into().expect("an 8-byte slot")) as usize;
        if address == 0 {
            return Ok(callbacks);
        }
        if address < base || address - base >= memory.len() {
            return Err(LoadError::Malformed(WHAT));
        }
        callbacks.push(address);
    }
    Err(LoadError::Malformed(WHAT))
}

impl LoadedImage {
    /// The name faults in this image are reported under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the image is mapped.
    pub fn base(&self) -> usize {
        self.mapping.address()
    }

    /// The size of the mapped image.
    pub fn size(&self) -> usize {
        self.mapping.size
    }

    /// The address of the image's entry point.
    pub fn entry_point(&self) -> usize {
        self.base() + self.entry_rva as usize
    }

    /// Everything the image exports by name, as addresses.
    pub fn exports(&self) -> Vec<(String, usize)> {
        self.exports
            .iter()
            .map(|(name, rva)| (name.clone(), self.base() + *rva as usize))
            .collect()
    }

    /// The address of what the image exports as `name`, if it exports that.
    pub fn export(&self, name: &str) -> Option<usize> {
        self.exports
            .iter()
            .find(|(exported, _)| exported == name)
            .map(|&(_, rva)| self.base() + rva as usize)
    }

    /// Whether `address` lies in this image.
    pub fn contains(&self, address: usize) -> bool {
        address >= self.base() && address - self.base() < self.size()
    }

    /// How many TLS callbacks the image has.
    pub fn tls_callback_count(&self) -> usize {
        self.tls_callbacks.len()
    }

    /// Runs the image on this thread: its TLS callbacks, then its entry point. Returns what
    /// the entry point returned, the title's exit code.
    ///
    /// # Safety
    ///
    /// This runs the image's code natively, with everything this process can do. Every
    /// address the resolver gave must be a function with the signature the image expects.
    pub unsafe fn run(&self) -> u32 {
        type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);
        type EntryPoint = unsafe extern "system" fn() -> u32;

        let base = self.mapping.base.as_ptr().cast::<c_void>();
        for &address in &self.tls_callbacks {
            // SAFETY: the address was read from the image's callback list and lies in the
            // image; the caller vouches for the image's code.
            let callback = unsafe { std::mem::transmute::<usize, TlsCallback>(address) };
            // SAFETY: called as Windows calls a TLS callback at process start.
            unsafe { callback(base, DLL_PROCESS_ATTACH, std::ptr::null_mut()) };
        }
        // SAFETY: the parser checked the entry point lies in an executable section; the
        // caller vouches for the image's code.
        let entry = unsafe { std::mem::transmute::<usize, EntryPoint>(self.entry_point()) };
        // SAFETY: as above.
        unsafe { entry() }
    }
}

impl LoadedImage {
    /// Starts the image as a library: runs its start-up as Windows does when a library is
    /// loaded into a process, and returns whether the library accepted. Its exports can be
    /// called afterwards. Threads made later are not announced to it.
    ///
    /// # Safety
    ///
    /// As for [`LoadedImage::run`]. The image is a library, whose entry point takes the
    /// three arguments of a library's.
    pub unsafe fn attach_as_library(&self) -> bool {
        type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);
        type LibraryEntry = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;

        let base = self.mapping.base.as_ptr().cast::<c_void>();
        for &address in &self.tls_callbacks {
            // SAFETY: the address was read from the image's callback list and lies in the
            // image; the caller vouches for the image's code.
            let callback = unsafe { std::mem::transmute::<usize, TlsCallback>(address) };
            // SAFETY: called as Windows calls a TLS callback when a library is loaded.
            unsafe { callback(base, DLL_PROCESS_ATTACH, std::ptr::null_mut()) };
        }
        // SAFETY: the parser checked the entry point lies in an executable section; the
        // caller vouches for the image's code and says it is a library's.
        let entry = unsafe { std::mem::transmute::<usize, LibraryEntry>(self.entry_point()) };
        // SAFETY: as above.
        unsafe { entry(base, DLL_PROCESS_ATTACH, std::ptr::null_mut()) != 0 }
    }
}

/// Where a thread made by [`LoadedImage::run_on_own_thread`] starts.
///
/// # Safety
/// `image` points at a [`LoadedImage`] that outlives the thread, whose code the caller of
/// `run_on_own_thread` vouched for.
unsafe extern "system" fn first_thread_start(image: *mut c_void) -> u32 {
    // SAFETY: as this function's contract says.
    unsafe { (*image.cast::<LoadedImage>()).run() }
}

impl LoadedImage {
    /// Runs the image as [`LoadedImage::run`] does, on a new thread with the stack the image
    /// asks for (at least 1 MiB), and waits for it. A title's first thread expects the stack
    /// size written in its header, which is often larger than this program's own.
    ///
    /// # Safety
    ///
    /// As for [`LoadedImage::run`].
    pub unsafe fn run_on_own_thread(&self) -> std::io::Result<u32> {
        const MIB: usize = 1 << 20;
        const CREATE_SUSPENDED: u32 = 0x4;
        const STACK_SIZE_IS_A_RESERVATION: u32 = 0x1_0000;
        const INFINITE: u32 = u32::MAX;
        let stack = usize::try_from(self.stack_reserve).unwrap_or(MIB).max(MIB);
        let mut id = 0u32;
        // SAFETY: the thread starts suspended, so it is marked as the image's first before
        // it runs anything, and this call waits for it to end before `self` can go away.
        unsafe {
            let thread = sys::CreateThread(
                std::ptr::null(),
                stack,
                first_thread_start,
                std::ptr::from_ref(self).cast_mut().cast(),
                CREATE_SUSPENDED | STACK_SIZE_IS_A_RESERVATION,
                &mut id,
            );
            if thread.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            crate::carrier::first_thread(id);
            sys::ResumeThread(thread);
            sys::WaitForSingleObject(thread, INFINITE);
            let mut exit_code = 0u32;
            let read = sys::GetExitCodeThread(thread, &mut exit_code);
            sys::CloseHandle(thread);
            if read == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(exit_code)
        }
    }
}

impl Drop for LoadedImage {
    fn drop(&mut self) {
        fault::unregister(self.base());
        if let Some(table) = self.function_table {
            // SAFETY: `table` was registered by `load` and is taken back once, before the
            // memory it lies in is freed by `mapping`'s drop.
            unsafe { sys::RtlDeleteFunctionTable(table) };
        }
    }
}
