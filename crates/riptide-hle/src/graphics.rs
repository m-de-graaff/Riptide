//! The console's graphics module, `d3d12_xs.dll`: where a title gets its graphics device.
//!
//! The device and everything made from it are `riptide-gpu`'s. This module binds the
//! module's functions to it and gives it what it needs from here: the title's memory and
//! allocator, the session log, and the stubs unbuilt methods are made of.

use std::sync::Arc;

use riptide_pe::Symbol;

use crate::imports::Imports;

pub(crate) fn register(imports: &mut Imports) {
    riptide_gpu::init(riptide_gpu::Services {
        memory: crate::xmem::memory(),
        log: Arc::new(|line: &str| crate::trace::trace!("{line}")),
        stub: crate::com::stub,
        allocator: crate::xmem::hooks,
    });
    // SAFETY: the function has the signature of the console's `D3D12XboxCreateDevice`.
    unsafe {
        imports.implement(
            "d3d12_xs.dll",
            Symbol::Name("D3D12XboxCreateDevice".to_owned()),
            riptide_gpu::create_device as *const () as usize,
        );
    }
}
