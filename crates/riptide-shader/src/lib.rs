//! Console shader programs (RDNA 2): reading their containers, decoding
//! their machine code, and lifting it into a form a host shader can be
//! written from.
//!
//! A shader of an Xbox Series title is a DXBC container. On a PC the `DXIL`
//! part of such a container holds LLVM bitcode. On the console that part holds
//! the console compiler's additions instead, among them the program compiled
//! to the machine code of the console's GPU. A PC GPU cannot run that code, so
//! it has to be read and translated. This crate holds the first three of the
//! four stages of that work (`docs/design/2026-11-28-shader-translation.md`):
//!
//! - [`container`] reads the container: its parts, the input and output
//!   signatures, the pipeline state validation part, and the console's `DXIL`
//!   part down to each program's code words and the data of its stage.
//! - [`inflate`] unpacks the zlib stream the machine code is stored in.
//! - [`gfx10`] decodes the code words into instructions of the GPU generation
//!   called GFX10 (the generation of RDNA 2) and prints them as text.
//! - [`ir`] is the intermediate form: a program as one lane of a wave sees
//!   it, as a control-flow graph of simple operations.
//! - [`lift`] turns a decoded program into that form, or says which
//!   instruction it cannot lift and why.
//! - [`interp`] runs the intermediate form on the CPU, as the reference
//!   every later translation is checked against, and runs the machine code
//!   itself a wave at a time, as the reference the lift is checked against.
//! - [`summarize`] reads and decodes one container and returns counts, so
//!   that the decoder can be checked against a real title without any of the
//!   title's data leaving the machine it runs on.
//!
//! Writing host shaders from the intermediate form is not built. See
//! README.md in this folder.
//!
//! Everything here is a pure function of its input bytes. Nothing touches the
//! operating system or a GPU, and malformed input gives an [`Error`], never a
//! panic.

mod bytes;
pub mod container;
pub mod gfx10;
pub mod inflate;
pub mod interp;
pub mod ir;
pub mod lift;
mod summary;

pub use summary::{
    ProgramSummary, Summary, UNKNOWN_SAMPLE_LIMIT, UnknownSample, summarize, summarize_program,
};

/// Why a container could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The bytes do not start with the `DXBC` magic.
    NotDxbc,
    /// The data ends before the named thing does.
    Truncated(&'static str),
    /// An offset or size stored in the data points outside the data. The
    /// string names what was being located.
    OutOfBounds(&'static str),
    /// A field has a value that makes no sense. The string says which.
    Malformed(&'static str),
    /// A structure carries a version number other than the one this crate was
    /// written against. Reading on would mean guessing, so the version is
    /// reported instead.
    UnknownVersion {
        /// The structure whose version is unknown.
        what: &'static str,
        /// The version number found.
        version: u32,
    },
    /// The compressed machine code could not be unpacked.
    Inflate(inflate::InflateError),
    /// The container has no part that holds console machine code.
    NoMachineCode,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotDxbc => write!(f, "not a DXBC container"),
            Error::Truncated(what) => write!(f, "the data ends inside {what}"),
            Error::OutOfBounds(what) => write!(f, "{what} lies outside the data"),
            Error::Malformed(what) => write!(f, "malformed shader: {what}"),
            Error::UnknownVersion { what, version } => {
                write!(f, "{what} has the unknown version {version:#x}")
            }
            Error::Inflate(error) => write!(f, "the machine code does not unpack: {error}"),
            Error::NoMachineCode => write!(f, "the container holds no console machine code"),
        }
    }
}

impl std::error::Error for Error {}

impl From<inflate::InflateError> for Error {
    fn from(error: inflate::InflateError) -> Self {
        Error::Inflate(error)
    }
}
