//! The console's DirectStorage, `XCRDAPI.dll`: how a title streams its files.
//!
//! The title asks for a factory, and through it opens files, makes queues of read requests
//! and polls status arrays. Only what a title needs to get past its start-up is built so
//! far; every other method names itself when called.

use std::ffi::c_void;
use std::sync::OnceLock;

use riptide_pe::Symbol;

use crate::com;
use crate::imports::Imports;
use crate::trace::trace;

const S_OK: i32 = 0;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;

/// How many methods the factory interface has.
const FACTORY_SLOTS: usize = 16;

/// `IDStorageFactoryX::SetStagingBufferSize`, at +0x40: how much compressed data may be in
/// flight at once. Noted; nothing reads files yet.
extern "system" fn set_staging_buffer_size(_this: *mut c_void, size: u32) -> i32 {
    trace!("DirectStorage: staging buffer {size:#x} bytes");
    S_OK
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
                (8, set_staging_buffer_size as *const () as usize),
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
            assert_eq!(
                get_factory(std::ptr::null(), std::ptr::null_mut()),
                E_POINTER
            );
            let table = *(factory as *const *const usize);
            let set = std::mem::transmute::<usize, extern "system" fn(*mut c_void, u32) -> i32>(
                *table.add(8),
            );
            assert_eq!(set(factory as *mut c_void, 0x200_0000), S_OK);
        }
    }
}
