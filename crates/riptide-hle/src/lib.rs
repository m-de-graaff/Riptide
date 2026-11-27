//! Console API implementations, and mapping title threads onto host cores.
//!
//! So far this crate holds the import table: what each function a title imports is bound to.
//! See README.md in this folder.

mod imports;
mod stub;
mod sys;

pub use imports::{Binding, Census, CensusLine, Imports};
pub use stub::{Log, UNIMPLEMENTED_IMPORT};
