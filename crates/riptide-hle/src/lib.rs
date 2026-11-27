//! Console API implementations, and mapping title threads onto host cores.
//!
//! So far this crate holds the import table (what each function a title imports is bound to)
//! the title's sleeps and timed waits, and title threads on their own cores.
//! See README.md in this folder.

pub mod cores;
mod imports;
pub mod process;
pub mod saves;
mod stub;
mod sys;
pub mod timers;

pub use imports::{Binding, Census, CensusLine, Imports};
pub use stub::{Log, UNIMPLEMENTED_IMPORT};
