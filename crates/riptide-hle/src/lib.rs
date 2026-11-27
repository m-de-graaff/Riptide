//! Console API implementations, and mapping title threads onto host cores.
//!
//! So far this crate holds the import table (what each function a title imports is bound to)
//! and the title's sleeps and timed waits.
//! See README.md in this folder.

mod imports;
mod stub;
mod sys;
pub mod timers;

pub use imports::{Binding, Census, CensusLine, Imports};
pub use stub::{Log, UNIMPLEMENTED_IMPORT};
