//! Console API implementations, and mapping title threads onto host cores.
//!
//! So far this crate holds the import table (what each function a title imports is bound to)
//! the title's sleeps and timed waits, and title threads on their own cores.
//! See README.md in this folder.

mod com;
pub mod cores;
pub mod dstorage;
mod imports;
pub mod process;
pub mod runtime;
pub mod saves;
mod stub;
mod sys;
pub mod timers;
mod trace;
pub mod xmem;

pub use imports::{Binding, Census, CensusLine, Imports};
pub use stub::{Log, UNIMPLEMENTED_IMPORT};
pub use trace::set_trace;
