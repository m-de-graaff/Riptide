//! Loading, mapping and binding the title image.
//!
//! [`Image::parse`] reads a PE32+ file without running or mapping anything. [`load`] maps it
//! into this process, relocates it, and binds its imports through a [`Resolver`]. A
//! [`LoadedImage`] then runs: TLS callbacks first, then the entry point. An unhandled fault is
//! named as module plus offset once [`report_unhandled_faults`] has been called.
//!
//! See README.md in this folder for what is and is not built.

mod carrier;
mod fault;
mod load;
mod parse;
mod sys;
mod tls_sites;

pub use carrier::pass_thread_events;
pub use fault::{describe, report_unhandled_faults, report_unhandled_faults_to};
pub use load::{LoadError, LoadedImage, Resolver, load};
pub use parse::{Image, Import, ImportedModule, ParseError, Section, Symbol, Tls};
