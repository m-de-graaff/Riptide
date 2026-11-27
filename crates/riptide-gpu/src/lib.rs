//! The title's D3D12.X objects on host D3D12.
//!
//! The console hands a title its graphics device, queues and command lists as objects with
//! tables of methods. This crate makes those objects. Behind them is one host D3D12 device,
//! and each call the title makes becomes host calls at once, on the title's thread
//! (decision D6). See README.md in this folder.

mod device;
mod fence;
mod host;
mod list;
mod object;
mod queue;
mod resource;

use std::sync::{Arc, OnceLock};

use riptide_mem::TitleMemory;

pub use device::create as create_device;
pub use object::Hresult;

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
}

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
