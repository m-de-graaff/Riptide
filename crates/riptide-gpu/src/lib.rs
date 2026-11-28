//! The title's D3D12.X objects on host D3D12.
//!
//! The console hands a title its graphics device, queues and command lists as objects with
//! tables of methods. This crate makes those objects. Behind them is one host D3D12 device,
//! and each call the title makes becomes host calls at once, on the title's thread
//! (decision D6). See README.md in this folder.

mod bindless;
mod census;
mod compile;
mod descriptor;
mod device;
mod draw;
mod dxgi;
mod fence;
mod frame;
mod host;
mod hostpipe;
#[cfg(test)]
mod hosttest;
mod inplace;
mod list;
mod object;
mod ordered;
mod pagepool;
mod pagetable;
mod pipeline;
mod present;
mod query;
mod queue;
mod record;
mod resource;
mod root;
mod signature;
mod stage;
mod target;
mod translate;
mod view;

use std::sync::{Arc, OnceLock};

use riptide_mem::TitleMemory;

pub use device::create as create_device;
pub use object::Hresult;
pub use root::serialize_versioned_root_signature;

/// What this crate needs from the rest of the runtime. Given once, with [`init`], before a
/// title can call anything here.
pub struct Services {
    /// The title's memory: graphics addresses are the title's own addresses.
    pub memory: &'static TitleMemory,
    /// Receives one line for each graphics call worth knowing about.
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    /// Makes a piece of code that, when called, names itself as unbuilt and stops the
    /// title. Every method that is not built yet gets one of its own.
    pub stub: fn(name: String) -> usize,
    /// The title's own allocation and free routines, if it has them. The console's graphics
    /// runtime gets the memory of committed resources from them, and the title counts on
    /// that: it keeps its own books of what its graphics memory holds.
    pub allocator: fn() -> Option<(Allocate, Free)>,
}

/// A title's allocation routine: `(bytes, attributes)`, returning the address or null.
pub type Allocate = unsafe extern "system" fn(usize, u64) -> *mut std::ffi::c_void;
/// A title's free routine: `(address, attributes)`, with the attributes it was allocated
/// with.
pub type Free = unsafe extern "system" fn(*mut std::ffi::c_void, u64);

static SERVICES: OnceLock<Services> = OnceLock::new();

/// Gives this crate its services. The first call wins.
pub fn init(services: Services) {
    let _ = SERVICES.set(services);
}

pub(crate) fn services() -> Option<&'static Services> {
    SERVICES.get()
}

pub(crate) fn write(line: std::fmt::Arguments<'_>) {
    if let Some(services) = services() {
        (services.log)(&line.to_string());
    }
}

/// Writes one line to the session log, formatted as `format!` does.
macro_rules! trace {
    ($($argument:tt)*) => {
        $crate::write(format_args!($($argument)*))
    };
}

pub(crate) use trace;
