//! Console API implementations, and mapping title threads onto host cores.
//!
//! So far this crate holds the import table (what each function a title imports is bound to)
//! the title's sleeps and timed waits, and title threads on their own cores.
//! See README.md in this folder.

mod accessibility;
mod acphal;
mod bvh;
mod com;
pub mod cores;
mod display;
pub mod dstorage;
mod files;
mod game;
mod graphics;
mod imports;
mod localstorage;
mod network;
mod offline;
mod package;
mod pool;
pub mod process;
pub mod runtime;
pub mod saves;
mod stub;
mod sys;
mod system;
mod taskqueue;
pub mod timers;
mod trace;
mod user;
mod xapu;
mod xasync;
mod xdsp;
mod xg;
pub mod xmem;

pub use imports::{Binding, Census, CensusLine, Imports};
pub use stub::{Log, UNIMPLEMENTED_IMPORT};
pub use trace::set_trace;
