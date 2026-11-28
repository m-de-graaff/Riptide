//! Root signatures: what a pipeline's shaders are given, parameter by parameter.
//!
//! A title describes a root signature, has the description packed into a blob, and makes a
//! root signature object from the blob. The blob's format is the same on the console and on
//! PC, so the host's own functions pack and unpack it. The object made here keeps the
//! unpacked description: it is what says how the title's later calls on a command list
//! ("parameter 3 is this table") are to be read.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC, D3D12CreateVersionedRootSignatureDeserializer,
    D3D12SerializeVersionedRootSignature, ID3D12VersionedRootSignatureDeserializer,
};

use crate::device::Device;
use crate::object::{self, Child, E_INVALIDARG, Hresult, Iid, Object, S_OK, guid};
use crate::trace;

/// `ID3D12RootSignature`.
const IID: Iid = guid(
    0xc54a6b66,
    0x72df,
    0x4ee8,
    [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
);

/// `d3d12_xs.dll!D3D12SerializeVersionedRootSignature`: `(description, blob out, error
/// blob out)`. The host packs the description. The blobs are the host's own objects, whose
/// methods are where a title expects them.
///
/// # Safety
/// `description` is null or a valid versioned root signature description; `blob` and
/// `error` are null or where a blob pointer goes.
pub unsafe extern "system" fn serialize_versioned_root_signature(
    description: *const c_void,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> Hresult {
    if description.is_null() || blob.is_null() {
        return E_INVALIDARG;
    }
    let mut problems: Option<ID3DBlob> = None;
    // SAFETY: the title's description, and room for the two blobs. An `Option` of an
    // interface is one pointer, which is what the title's out-pointer is.
    let result = unsafe {
        D3D12SerializeVersionedRootSignature(
            description.cast::<D3D12_VERSIONED_ROOT_SIGNATURE_DESC>(),
            blob.cast::<Option<ID3DBlob>>(),
            Some(&raw mut problems),
        )
    };
    let code = match result {
        Ok(()) => S_OK,
        Err(failure) => {
            let said = problems.as_ref().map(text).unwrap_or_default();
            trace!("the host did not pack a root signature description: {failure} {said}");
            failure.code().0
        }
    };
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(error) = unsafe { error.as_mut() } {
        // The reference the blob holds becomes the title's.
        *error = problems.map_or(std::ptr::null_mut(), windows::core::Interface::into_raw);
    }
    code
}

/// What an error blob says.
fn text(blob: &ID3DBlob) -> String {
    // SAFETY: a blob's pointer and size describe its own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .trim()
        .to_owned()
}

/// A root signature as a title's code sees it.
#[repr(C)]
pub(crate) struct RootSignature {
    object: Object,
    /// Holds the unpacked description, which [`RootSignature::description`] points into.
    unpacked: ID3D12VersionedRootSignatureDeserializer,
}

// SAFETY: `RootSignature` is `#[repr(C)]` and starts with its header.
unsafe impl Child for RootSignature {
    const NAME: &'static str = "root signature";
    const IIDS: &'static [Iid] = &[IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

impl RootSignature {
    /// The description as the title wrote it, in the version it wrote it in.
    pub(crate) fn description(&self) -> Option<&D3D12_VERSIONED_ROOT_SIGNATURE_DESC> {
        // SAFETY: the pointer is into memory the deserializer holds as long as it lives,
        // which is as long as this object does.
        unsafe { self.unpacked.GetUnconvertedRootSignatureDesc().as_ref() }
    }

    /// How many parameters and static samplers it has, and its flags.
    pub(crate) fn shape(&self) -> (u32, u32, u32) {
        let Some(description) = self.description() else {
            return (0, 0, 0);
        };
        // SAFETY: the version says which member of the union is the description. Version
        // 1.0 and 1.1 have the same first fields; 1.2 has them too.
        let d = unsafe { &description.Anonymous.Desc_1_1 };
        (d.NumParameters, d.NumStaticSamplers, d.Flags.0 as u32)
    }
}

/// The root signature has no methods of its own after the eight every device child has.
const SLOTS: usize = 12;

const NAMES: [(usize, &str); 8] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
];

fn table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let methods = object::child_methods::<RootSignature>();
        object::table("ID3D12RootSignature", SLOTS, &NAMES, &methods)
    })
}

/// `ID3D12Device::CreateRootSignature`: `(node mask, blob bytes, blob size, interface id,
/// root signature out)`.
///
/// # Safety
/// `blob` is null or `bytes` readable bytes; the other pointers are null or valid.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    _nodes: u32,
    blob: *const c_void,
    bytes: usize,
    iid: *const Iid,
    out: *mut *const RootSignature,
) -> Hresult {
    if blob.is_null() || bytes == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let mut unpacked: Option<ID3D12VersionedRootSignatureDeserializer> = None;
    // SAFETY: the title's blob, of that many bytes; an `Option` of an interface is one
    // pointer, which is what the function writes.
    let result = unsafe {
        D3D12CreateVersionedRootSignatureDeserializer(
            blob,
            bytes,
            &<ID3D12VersionedRootSignatureDeserializer as windows::core::Interface>::IID,
            (&raw mut unpacked).cast(),
        )
    };
    let unpacked = match (result, unpacked) {
        (Ok(()), Some(unpacked)) => unpacked,
        (result, _) => {
            let code = result.err().map_or(E_INVALIDARG, |error| error.code().0);
            trace!("a root signature blob of {bytes} bytes does not unpack: {code:#x}");
            return code;
        }
    };
    let signature = Box::new(RootSignature {
        object: Object::new(table(), 1),
        unpacked,
    });
    let (parameters, samplers, flags) = signature.shape();
    show(format_args!(
        "root signature {:p} made: {parameters} parameters, {samplers} static samplers, \
         flags {flags:#x}",
        &raw const *signature
    ));
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(signature, out) }
}

/// Says what was made, for the first few: a title makes many.
fn show(line: std::fmt::Arguments<'_>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 8 {
        trace!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, method};
    use windows::Win32::Graphics::Direct3D12::{
        D3D_ROOT_SIGNATURE_VERSION_1_1, D3D12_ROOT_CONSTANTS,
        D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS, D3D12_ROOT_PARAMETER_TYPE_CBV,
        D3D12_ROOT_PARAMETER1, D3D12_ROOT_PARAMETER1_0, D3D12_ROOT_SIGNATURE_DESC1,
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT, D3D12_SHADER_VISIBILITY_ALL,
        D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0,
    };

    /// A description with a constant-buffer parameter and four root constants.
    fn described() -> ([D3D12_ROOT_PARAMETER1; 2], u32) {
        let parameters = [
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                ..Default::default()
            },
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 1,
                        RegisterSpace: 0,
                        Num32BitValues: 4,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        (
            parameters,
            D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT.0 as u32,
        )
    }

    #[test]
    fn a_description_is_packed_and_a_root_signature_made_of_the_blob_reads_the_same() {
        let (parameters, flags) = described();
        let description = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: 2,
                    pParameters: parameters.as_ptr(),
                    NumStaticSamplers: 0,
                    pStaticSamplers: std::ptr::null(),
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
                },
            },
        };
        let mut blob = std::ptr::null_mut();
        let mut error = std::ptr::dangling_mut();
        // SAFETY: a valid description and out-pointers; the blob is used as the interface
        // it is and released once.
        unsafe {
            assert_eq!(
                serialize_versioned_root_signature(
                    (&raw const description).cast(),
                    &mut blob,
                    &mut error
                ),
                S_OK
            );
            assert!(error.is_null());
            let blob = <ID3DBlob as windows::core::Interface>::from_raw(blob);

            let mut made: *const RootSignature = std::ptr::null();
            let device = std::ptr::null();
            assert_eq!(
                create(
                    device,
                    0,
                    blob.GetBufferPointer(),
                    blob.GetBufferSize(),
                    &[9u8; 16],
                    &mut made
                ),
                E_NOINTERFACE
            );
            assert_eq!(
                create(
                    device,
                    0,
                    blob.GetBufferPointer(),
                    blob.GetBufferSize(),
                    &IID,
                    &mut made
                ),
                S_OK
            );
            let signature = &*made;
            assert_eq!(signature.shape(), (2, 0, flags));
            let unpacked = signature.description().unwrap();
            assert_eq!(unpacked.Version, D3D_ROOT_SIGNATURE_VERSION_1_1);
            let second = &*unpacked.Anonymous.Desc_1_1.pParameters.add(1);
            assert_eq!(second.Anonymous.Constants.Num32BitValues, 4);

            let release: unsafe extern "system" fn(*const RootSignature) -> u32 =
                method(made.cast(), 2);
            assert_eq!(release(made), 0);
        }
    }

    #[test]
    fn bytes_that_are_no_blob_make_no_root_signature() {
        let junk = [0x5au8; 64];
        let mut made: *const RootSignature = std::ptr::dangling();
        // SAFETY: readable bytes and valid pointers, and the null ones under test.
        unsafe {
            assert!(
                create(
                    std::ptr::null(),
                    0,
                    junk.as_ptr().cast(),
                    64,
                    &IID,
                    &mut made
                ) < 0
            );
            assert!(made.is_null());
            assert_eq!(
                create(std::ptr::null(), 0, std::ptr::null(), 64, &IID, &mut made),
                E_INVALIDARG
            );
            let mut blob = std::ptr::null_mut();
            assert_eq!(
                serialize_versioned_root_signature(
                    std::ptr::null(),
                    &mut blob,
                    std::ptr::null_mut()
                ),
                E_INVALIDARG
            );
        }
    }
}
