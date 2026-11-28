//! Bounds-checked reads of little-endian fields.
//!
//! Every offset in a shader container comes from the file itself, so every
//! read goes through these functions, which return an error where indexing
//! would panic.

use crate::Error;

/// Returns `len` bytes starting at `at`, or an error naming `what` if they are
/// not all inside `data`. The position and length are 64-bit because some of
/// the console's structures store them so.
pub(crate) fn span<'a>(
    data: &'a [u8],
    at: u64,
    len: u64,
    what: &'static str,
) -> Result<&'a [u8], Error> {
    let start = usize::try_from(at).map_err(|_| Error::OutOfBounds(what))?;
    let len = usize::try_from(len).map_err(|_| Error::OutOfBounds(what))?;
    let end = start.checked_add(len).ok_or(Error::OutOfBounds(what))?;
    data.get(start..end).ok_or(Error::OutOfBounds(what))
}

/// Returns `N` bytes at `at` as an array.
fn array<const N: usize>(data: &[u8], at: usize, what: &'static str) -> Result<[u8; N], Error> {
    let end = at.checked_add(N).ok_or(Error::Truncated(what))?;
    let bytes = data.get(at..end).ok_or(Error::Truncated(what))?;
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    Ok(out)
}

/// Reads one byte at `at`.
pub(crate) fn u8_at(data: &[u8], at: usize, what: &'static str) -> Result<u8, Error> {
    data.get(at).copied().ok_or(Error::Truncated(what))
}

/// Reads a 16-bit field at `at`.
pub(crate) fn u16_at(data: &[u8], at: usize, what: &'static str) -> Result<u16, Error> {
    array(data, at, what).map(u16::from_le_bytes)
}

/// Reads a 32-bit field at `at`.
pub(crate) fn u32_at(data: &[u8], at: usize, what: &'static str) -> Result<u32, Error> {
    array(data, at, what).map(u32::from_le_bytes)
}

/// Reads a 64-bit field at `at`.
pub(crate) fn u64_at(data: &[u8], at: usize, what: &'static str) -> Result<u64, Error> {
    array(data, at, what).map(u64::from_le_bytes)
}

/// Reads the bytes of a NUL-terminated string that starts at `at`. Bytes that
/// are not valid UTF-8 are replaced, because a name is only ever shown or
/// compared.
pub(crate) fn c_string(data: &[u8], at: usize, what: &'static str) -> Result<String, Error> {
    let tail = data.get(at..).ok_or(Error::OutOfBounds(what))?;
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(Error::Truncated(what))?;
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}
