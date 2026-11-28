//! `XboxBvhBuilder.dll`: the module a title builds its ray-tracing structures with.
//!
//! On the console this module ships in the title's own package. It is a class whose
//! methods record compute work on the title's command lists, with the console's own shader
//! programs, to build structures in the console's own format.
//!
//! This is a stand-in, not that module. A title can make and drop a builder and set its
//! options, so that start-up goes on. Every method that would build or measure a structure
//! is left unbuilt and names itself when called. The real thing is to load the package's
//! own module, which needs its shader programs translated first.

use std::ffi::c_void;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::trace::trace;

const MODULE: &str = "XboxBvhBuilder.dll";

/// `XboxBvhBuilder::XboxBvhBuilder(ID3D12Device11*, bool)`. A constructor returns the
/// object it was given. Nothing is kept in the object, whose size is the title's to know.
extern "system" fn construct(
    this: *mut c_void,
    device: *const c_void,
    option: bool,
) -> *mut c_void {
    trace!(
        "XboxBvhBuilder made at {this:p} for device {device:p} (option {option}): a stand-in \
         that builds no ray-tracing structures"
    );
    this
}

/// `XboxBvhBuilder::~XboxBvhBuilder()`.
extern "system" fn destruct(_this: *mut c_void) {}

/// `XboxBvhBuilder::Set64BitMortonCodes(bool)`: how finely the builder sorts. Nothing is
/// built, so nothing is sorted.
extern "system" fn set_64_bit_morton_codes(_this: *mut c_void, _on: bool) {}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 3] = [
        (
            "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z",
            construct as *const () as usize,
        ),
        ("??1XboxBvhBuilder@@QEAA@XZ", destruct as *const () as usize),
        (
            "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z",
            set_64_bit_morton_codes as *const () as usize,
        ),
    ];
    for (name, address) in functions {
        // SAFETY: each function has the arguments its decorated name declares.
        unsafe { imports.implement(MODULE, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builder_is_made_where_the_title_put_it_and_nothing_is_written_there() {
        let mut object = [0x55u8; 64];
        let this = object.as_mut_ptr().cast::<c_void>();
        assert_eq!(construct(this, std::ptr::null(), true), this);
        set_64_bit_morton_codes(this, true);
        destruct(this);
        assert_eq!(object, [0x55; 64]);
    }
}
