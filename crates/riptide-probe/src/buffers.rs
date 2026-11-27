//! Buffers of each kind, and moving bytes between the CPU and the GPU for checks.

use crate::device::{Gpu, submit};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};

pub const TILE: u64 = 65_536;
pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

/// Bytes that differ by position and by `salt`, to tell regions and runs apart.
pub fn pattern(salt: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8 ^ salt)
        .collect()
}

pub fn buffer_desc(bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
    }
}

pub fn committed(
    gpu: &Gpu,
    heap: D3D12_HEAP_TYPE,
    bytes: u64,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES {
        Type: heap,
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    // SAFETY: valid descriptions and out-pointer.
    unsafe {
        gpu.device.CreateCommittedResource(
            &props,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(bytes, flags),
            state,
            None,
            &mut r,
        )?
    };
    Ok(r.expect("CreateCommittedResource succeeded without a resource"))
}

/// Maps subresource 0 and returns its CPU address. Resources stay mapped until released.
pub fn map(resource: &ID3D12Resource) -> windows::core::Result<*mut u8> {
    let mut p = std::ptr::null_mut();
    // SAFETY: buffers have one subresource; `p` is a valid out-pointer.
    unsafe { resource.Map(0, None, Some(&mut p))? };
    Ok(p.cast())
}

/// Has the GPU copy `data` into `dst` at `offset`.
pub fn upload(
    gpu: &Gpu,
    dst: &ID3D12Resource,
    offset: u64,
    data: &[u8],
) -> windows::core::Result<()> {
    let len = data.len() as u64;
    let staging = committed(
        gpu,
        D3D12_HEAP_TYPE_UPLOAD,
        len,
        D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_GENERIC_READ,
    )?;
    let p = map(&staging)?;
    // SAFETY: `staging` maps `len` bytes and `data` holds exactly `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len()) };
    submit(gpu, |list| {
        // SAFETY: the caller guarantees `offset + len` lies inside `dst`; `staging` is `len` bytes.
        unsafe { list.CopyBufferRegion(dst, offset, &staging, 0, len) }
    })
}

/// Has the GPU copy `bytes` bytes of `src` at `offset` out to the CPU.
pub fn read_back(
    gpu: &Gpu,
    src: &ID3D12Resource,
    offset: u64,
    bytes: u64,
) -> windows::core::Result<Vec<u8>> {
    let out = committed(
        gpu,
        D3D12_HEAP_TYPE_READBACK,
        bytes,
        D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    )?;
    submit(gpu, |list| {
        // SAFETY: the caller guarantees `offset + bytes` lies inside `src`; `out` is `bytes` bytes.
        unsafe { list.CopyBufferRegion(&out, 0, src, offset, bytes) }
    })?;
    let p = map(&out)?;
    // SAFETY: `out` maps `bytes` bytes, all written by the finished copy above.
    Ok(unsafe { std::slice::from_raw_parts(p, bytes as usize) }.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_differ_by_salt_and_position() {
        let a = pattern(1, 4096);
        let b = pattern(2, 4096);
        assert_eq!(a.len(), 4096);
        assert_ne!(a, b);
        assert!(a.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn buffer_desc_is_a_row_major_buffer() {
        let d = buffer_desc(TILE, D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER);
        assert_eq!(d.Dimension, D3D12_RESOURCE_DIMENSION_BUFFER);
        assert_eq!(d.Layout, D3D12_TEXTURE_LAYOUT_ROW_MAJOR);
        assert_eq!(d.Width, TILE);
        assert_eq!(d.Flags, D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER);
    }
}
