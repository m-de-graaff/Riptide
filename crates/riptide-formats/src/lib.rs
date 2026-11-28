//! Texture tiling and the console's compressed texture and audio formats.
//!
//! [`texture`] holds the console GPU's texture layouts: sizes, the address of
//! each element, and copies between console memory and plain rows. The
//! compressed texture format (BCPack) and the audio format (XMA2) are not
//! built yet. See README.md in this folder.

pub mod texture;
