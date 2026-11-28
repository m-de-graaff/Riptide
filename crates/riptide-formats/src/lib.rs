//! Texture tiling and the console's compressed texture and audio formats.
//!
//! [`texture`] holds the console GPU's texture layouts: sizes, the address of
//! each element, and copies between console memory and plain rows.
//! [`bcpack`] unpacks the console's lossless packing of block-compressed
//! textures (BCPack) into ordinary blocks. The audio format (XMA2) is not
//! built yet. See README.md in this folder.

pub mod bcpack;
pub mod texture;
