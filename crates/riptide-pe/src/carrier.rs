//! Thread-local data for a loaded image.
//!
//! An image's code finds its thread-local block through a number, its TLS index, that only
//! Windows' own loader hands out: the index names a slot in a per-thread array the loader
//! owns, allocates for every new thread and frees when the thread ends. This loader maps
//! images itself, so Windows never gives them a slot.
//!
//! A carrier gets one. It is a small DLL written here, at run time, and loaded by Windows in
//! the ordinary way. It holds nothing but a TLS directory whose template is the image's own
//! (already relocated), so Windows gives every thread, present and future, a block that is
//! exactly the image's. The image's index is then set to the carrier's. The carrier's entry
//! point jumps to [`on_thread`], which passes each thread's start and end on to the image's
//! TLS callbacks, as Windows would for an image it had loaded.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::sys;

const FILE_ALIGNMENT: u32 = 0x200;
const SECTION_ALIGNMENT: u32 = 0x1000;
const HEADER_BYTES: u32 = 0x400;

const TEXT_RVA: u32 = 0x1000;
/// The read-only section: the TLS directory, then the import tables.
const RDATA_RVA: u32 = 0x2000;
const IMPORTS_RVA: u32 = RDATA_RVA + 0x40;
const LOOKUP_RVA: u32 = RDATA_RVA + 0x80;
const MODULE_NAME_RVA: u32 = RDATA_RVA + 0xC0;
const FUNCTION_NAME_RVA: u32 = RDATA_RVA + 0xD0;
/// The writable section: the TLS index, then the one import's address.
const DATA_RVA: u32 = 0x3000;
const IMPORT_SLOT_RVA: u32 = DATA_RVA + 0x10;
const TEMPLATE_RVA: u32 = 0x4000;

const PREFERRED_BASE: u64 = 0x1_8000_0000;

const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

/// The alignment bits of a TLS directory's characteristics.
const ALIGNMENT_MASK: u32 = 0x00F0_0000;

type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);

/// What a thread's start or end is passed on to.
struct Served {
    /// The carrier's module handle, which is how [`on_thread`] knows whose thread event it is.
    carrier: usize,
    image_base: usize,
    callbacks: Vec<usize>,
}

static SERVED: Mutex<Vec<Served>> = Mutex::new(Vec::new());

/// Every carrier's entry point jumps here, so Windows calls it for each thread that starts
/// or ends while the carrier is loaded, on that thread.
unsafe extern "system" fn on_thread(
    module: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    if reason == DLL_THREAD_ATTACH || reason == DLL_THREAD_DETACH {
        let found = {
            let served = SERVED.lock().unwrap_or_else(|poison| poison.into_inner());
            served
                .iter()
                .find(|entry| entry.carrier == module as usize)
                .map(|entry| (entry.image_base, entry.callbacks.clone()))
        };
        if let Some((image_base, callbacks)) = found {
            for address in callbacks {
                // SAFETY: the address was read from the image's callback list and lies in
                // the image, which stays loaded for as long as its carrier does.
                let callback = unsafe { std::mem::transmute::<usize, TlsCallback>(address) };
                // SAFETY: called as Windows calls a TLS callback for a thread event.
                unsafe { callback(image_base as *mut c_void, reason, std::ptr::null_mut()) };
            }
        }
    }
    1
}

/// A loaded carrier. Dropping it unloads the carrier and removes its file; no thread may use
/// the image's thread-local data after that.
pub(crate) struct Carrier {
    module: *mut c_void,
    path: PathBuf,
}

impl Carrier {
    /// Writes and loads a carrier whose every thread block starts as `template`, aligned as
    /// `characteristics` asks.
    pub(crate) fn load(template: &[u8], characteristics: u32) -> io::Result<Carrier> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "riptide-tls-{}-{}.dll",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let entry = on_thread as *const () as usize as u64;
        std::fs::write(&path, image_bytes(template, characteristics, entry))?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: the path is null-terminated. The file is the carrier just written; its
        // entry point is `on_thread`.
        let module = unsafe { sys::LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            let error = io::Error::last_os_error();
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Ok(Carrier { module, path })
    }

    /// The TLS index Windows gave the carrier, which the image uses as its own.
    pub(crate) fn index(&self) -> u32 {
        // SAFETY: the carrier is loaded, and its index lies at this offset, written by
        // Windows when it was loaded.
        unsafe {
            self.module
                .cast::<u8>()
                .add(DATA_RVA as usize)
                .cast::<u32>()
                .read()
        }
    }

    /// Passes the start and end of every thread on to `callbacks`, with `image_base` as the
    /// module they are told of.
    pub(crate) fn serve(&self, image_base: usize, callbacks: Vec<usize>) {
        SERVED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Served {
                carrier: self.module as usize,
                image_base,
                callbacks,
            });
    }
}

impl Drop for Carrier {
    fn drop(&mut self) {
        SERVED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|entry| entry.carrier != self.module as usize);
        // SAFETY: `module` came from LoadLibraryW and is freed once, here.
        unsafe { sys::FreeLibrary(self.module) };
        let _ = std::fs::remove_file(&self.path);
    }
}

fn align(value: u32, to: u32) -> u32 {
    value.div_ceil(to) * to
}

fn put16(file: &mut [u8], at: usize, value: u16) {
    file[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(file: &mut [u8], at: usize, value: u32) {
    file[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(file: &mut [u8], at: usize, value: u64) {
    file[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The bytes of a carrier DLL: an entry point that jumps to `entry`, and a TLS directory
/// with `template` as its template.
fn image_bytes(template: &[u8], characteristics: u32, entry: u64) -> Vec<u8> {
    // Windows wants a block of at least one byte.
    let template = if template.is_empty() {
        &[0u8][..]
    } else {
        template
    };
    let template_len = template.len() as u32;
    let reloc_rva = TEMPLATE_RVA + align(template_len, SECTION_ALIGNMENT);
    let image_size = reloc_rva + SECTION_ALIGNMENT;

    // mov rax, entry; jmp rax
    let mut text = vec![0x48, 0xB8];
    text.extend_from_slice(&entry.to_le_bytes());
    text.extend_from_slice(&[0xFF, 0xE0]);

    // IMAGE_TLS_DIRECTORY64: start, end, address of index, address of callbacks, zero fill,
    // characteristics. The three addresses are relocated with the carrier.
    let start = PREFERRED_BASE + u64::from(TEMPLATE_RVA);
    let mut rdata = Vec::new();
    rdata.extend_from_slice(&start.to_le_bytes());
    rdata.extend_from_slice(&(start + u64::from(template_len)).to_le_bytes());
    rdata.extend_from_slice(&(PREFERRED_BASE + u64::from(DATA_RVA)).to_le_bytes());
    rdata.extend_from_slice(&0u64.to_le_bytes());
    rdata.extend_from_slice(&0u32.to_le_bytes());
    rdata.extend_from_slice(&(characteristics & ALIGNMENT_MASK).to_le_bytes());
    let tls_directory_len = rdata.len() as u32;

    // One import, never called. Windows sets up thread-local data only for a DLL that
    // imports something: measured on Windows 11 build 26200, where a carrier without
    // imports loaded and ran its entry point but was given no TLS index.
    rdata.resize((IMPORTS_RVA - RDATA_RVA) as usize, 0);
    for field in [LOOKUP_RVA, 0, 0, MODULE_NAME_RVA, IMPORT_SLOT_RVA] {
        rdata.extend_from_slice(&field.to_le_bytes());
    }
    rdata.resize((LOOKUP_RVA - RDATA_RVA) as usize, 0);
    rdata.extend_from_slice(&u64::from(FUNCTION_NAME_RVA).to_le_bytes());
    rdata.resize((MODULE_NAME_RVA - RDATA_RVA) as usize, 0);
    rdata.extend_from_slice(b"KERNEL32.dll\0");
    rdata.resize((FUNCTION_NAME_RVA - RDATA_RVA) as usize, 0);
    rdata.extend_from_slice(b"\0\0Sleep\0");

    // The TLS index, zero until Windows writes it, and the import's slot.
    let mut data = vec![0u8; (IMPORT_SLOT_RVA - DATA_RVA) as usize];
    data.extend_from_slice(&u64::from(FUNCTION_NAME_RVA).to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());

    // One relocation block for the TLS directory's page: three 8-byte addresses and a pad.
    const DIR64: u16 = 10 << 12;
    let mut reloc = Vec::new();
    reloc.extend_from_slice(&RDATA_RVA.to_le_bytes());
    reloc.extend_from_slice(&16u32.to_le_bytes());
    for offset in [0u16, 8, 16] {
        reloc.extend_from_slice(&(DIR64 | offset).to_le_bytes());
    }
    reloc.extend_from_slice(&0u16.to_le_bytes());

    // (name, where it is mapped, its bytes, characteristics)
    let sections: [(&[u8; 8], u32, &[u8], u32); 5] = [
        (b".text\0\0\0", TEXT_RVA, &text, 0x6000_0020),
        (b".rdata\0\0", RDATA_RVA, &rdata, 0x4000_0040),
        (b".data\0\0\0", DATA_RVA, &data, 0xC000_0040),
        (b".tls\0\0\0\0", TEMPLATE_RVA, template, 0xC000_0040),
        (b".reloc\0\0", reloc_rva, &reloc, 0x4200_0040),
    ];

    let mut file = vec![0u8; HEADER_BYTES as usize];

    // DOS header: the signature and where the PE header starts.
    file[0..2].copy_from_slice(b"MZ");
    put32(&mut file, 0x3C, 0x40);
    file[0x40..0x44].copy_from_slice(b"PE\0\0");

    // File header: x86-64, an executable image, a DLL, at home above 4 GiB.
    let coff = 0x44;
    put16(&mut file, coff, 0x8664);
    put16(&mut file, coff + 2, sections.len() as u16);
    put16(&mut file, coff + 16, 240);
    put16(&mut file, coff + 18, 0x2022);

    // Optional header (PE32+).
    let optional = coff + 20;
    put16(&mut file, optional, 0x20B);
    put32(&mut file, optional + 4, FILE_ALIGNMENT);
    put32(&mut file, optional + 16, TEXT_RVA);
    put32(&mut file, optional + 20, TEXT_RVA);
    put64(&mut file, optional + 24, PREFERRED_BASE);
    put32(&mut file, optional + 32, SECTION_ALIGNMENT);
    put32(&mut file, optional + 36, FILE_ALIGNMENT);
    put16(&mut file, optional + 40, 6);
    put16(&mut file, optional + 48, 6);
    put32(&mut file, optional + 56, image_size);
    put32(&mut file, optional + 60, HEADER_BYTES);
    // Console subsystem; may be moved anywhere; data is not executable.
    put16(&mut file, optional + 68, 3);
    put16(&mut file, optional + 70, 0x0160);
    put64(&mut file, optional + 72, 0x10_0000);
    put64(&mut file, optional + 80, 0x1000);
    put64(&mut file, optional + 88, 0x10_0000);
    put64(&mut file, optional + 96, 0x1000);
    put32(&mut file, optional + 108, 16);
    // Data directories: imports (1), base relocations (5), TLS (9).
    let directories = optional + 112;
    put32(&mut file, directories + 8, IMPORTS_RVA);
    put32(&mut file, directories + 8 + 4, 40);
    put32(&mut file, directories + 5 * 8, reloc_rva);
    put32(&mut file, directories + 5 * 8 + 4, reloc.len() as u32);
    put32(&mut file, directories + 9 * 8, RDATA_RVA);
    put32(&mut file, directories + 9 * 8 + 4, tls_directory_len);

    // Section headers, then each section's bytes padded to the file alignment.
    let mut raw_offset = HEADER_BYTES;
    for (index, (name, rva, bytes, flags)) in sections.iter().enumerate() {
        let at = optional + 240 + index * 40;
        let raw_size = align(bytes.len() as u32, FILE_ALIGNMENT);
        file[at..at + 8].copy_from_slice(*name);
        put32(&mut file, at + 8, bytes.len() as u32);
        put32(&mut file, at + 12, *rva);
        put32(&mut file, at + 16, raw_size);
        put32(&mut file, at + 20, raw_offset);
        put32(&mut file, at + 36, *flags);
        raw_offset += raw_size;
    }
    for (_, _, bytes, _) in &sections {
        let start = file.len();
        file.extend_from_slice(bytes);
        file.resize(
            start + align(bytes.len() as u32, FILE_ALIGNMENT) as usize,
            0,
        );
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Image, Symbol};

    #[test]
    fn a_carrier_is_an_image_this_crate_can_read_back() {
        let template: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let bytes = image_bytes(&template, 0x0050_0000, 0x1122_3344_5566_7788);
        let image = Image::parse(&bytes).unwrap();
        let tls = image.tls.unwrap();
        assert_eq!(tls.template_rva, TEMPLATE_RVA);
        assert_eq!(tls.template_size, 5000);
        assert_eq!(tls.index_rva, DATA_RVA);
        assert_eq!(tls.callbacks_rva, None);
        assert_eq!(tls.characteristics, 0x0050_0000);
        assert_eq!(image.entry_rva, TEXT_RVA);
        assert_eq!(
            image.relocations,
            [RDATA_RVA, RDATA_RVA + 8, RDATA_RVA + 16]
        );
        assert_eq!(image.imports.len(), 1);
        assert_eq!(image.imports[0].name, "KERNEL32.dll");
        assert_eq!(
            image.imports[0].imports[0].symbol,
            Symbol::Name("Sleep".to_owned())
        );
        assert_eq!(image.imports[0].imports[0].slot_rva, IMPORT_SLOT_RVA);
    }

    /// This thread's block for `index`, read the way an image's code reads it.
    fn block(index: u32, length: usize) -> Vec<u8> {
        let array: *const *const u8;
        // SAFETY: gs:[0x58] holds the thread's TLS array on x64 Windows, and the entry at a
        // loaded module's index points at that module's block for this thread.
        unsafe {
            std::arch::asm!("mov {}, gs:[0x58]", out(reg) array);
            std::slice::from_raw_parts(*array.add(index as usize), length).to_vec()
        }
    }

    #[test]
    fn windows_gives_every_thread_the_template() {
        let template: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        let carrier = Carrier::load(&template, 0).unwrap();
        let index = carrier.index();
        // Index 0 is the test program's own.
        assert_ne!(index, 0);

        // A thread that already ran when the carrier was loaded, and one started after.
        assert!(block(index, template.len()) == template);
        let expected = template.clone();
        std::thread::spawn(move || assert!(block(index, expected.len()) == expected))
            .join()
            .unwrap();
    }

    #[test]
    fn thread_starts_and_ends_reach_the_callbacks_with_the_images_base() {
        // (module, reason, thread). The callback runs while a thread is being set up or
        // torn down, so it uses nothing of the standard library's that is per thread.
        static SEEN: Mutex<Vec<(usize, u32, u32)>> = Mutex::new(Vec::new());
        unsafe extern "system" fn callback(module: *mut c_void, reason: u32, _: *mut c_void) {
            // SAFETY: the call has no preconditions.
            let thread = unsafe { sys::GetCurrentThreadId() };
            if let Ok(mut seen) = SEEN.lock() {
                seen.push((module as usize, reason, thread));
            }
        }

        let carrier = Carrier::load(&[1, 2, 3], 0).unwrap();
        carrier.serve(0x1234_0000, vec![callback as *const () as usize]);
        // SAFETY: the call has no preconditions.
        let thread = std::thread::spawn(|| unsafe { sys::GetCurrentThreadId() })
            .join()
            .unwrap();
        drop(carrier);

        // Other tests start threads too; only this test's own thread is looked at.
        let seen: Vec<(usize, u32)> = SEEN
            .lock()
            .unwrap()
            .iter()
            .filter(|&&(_, _, whose)| whose == thread)
            .map(|&(module, reason, _)| (module, reason))
            .collect();
        assert_eq!(
            seen,
            [
                (0x1234_0000, DLL_THREAD_ATTACH),
                (0x1234_0000, DLL_THREAD_DETACH)
            ]
        );
    }
}
