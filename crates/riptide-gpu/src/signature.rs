//! Command signatures: the layout of the records an indirect command reads its arguments
//! from.
//!
//! The records lie in title memory, which the host GPU reads where it is, so a signature
//! whose arguments are only draws and dispatches is the same signature on the host and is
//! made there at once. One that also sets views or constants belongs to a root signature,
//! and its host signature is made with that.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_SIGNATURE_DESC, D3D12_INDIRECT_ARGUMENT_DESC, D3D12_INDIRECT_ARGUMENT_TYPE,
    ID3D12CommandSignature, ID3D12RootSignature,
};

use crate::device::Device;
use crate::object::{self, Child, E_INVALIDARG, Hresult, Iid, Object, guid};
use crate::trace;

/// `ID3D12CommandSignature`.
const IID: Iid = guid(
    0xc36a797c,
    0xec80,
    0x4f0a,
    [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
);

/// `D3D12_INDIRECT_ARGUMENT_DESC`: a type, then up to three words that say where a view
/// or constants go. A draw, an indexed draw and the dispatches have none, and what the
/// title leaves in those words means nothing.
pub(crate) type Argument = [u32; 4];

/// The argument types that need no root signature: a draw, an indexed draw, a dispatch,
/// a dispatch of rays and a dispatch of mesh shaders.
const PLAIN_TYPES: [u32; 5] = [0, 1, 2, 9, 10];

/// `D3D12_COMMAND_SIGNATURE_DESC`, as on PC.
#[repr(C)]
pub(crate) struct Description {
    stride: u32,
    count: u32,
    arguments: *const Argument,
    nodes: u32,
}

/// A command signature as a title's code sees it.
#[repr(C)]
pub(crate) struct Signature {
    object: Object,
    stride: u32,
    arguments: Vec<Argument>,
    /// The host signature, for one of draws and dispatches only.
    host: Option<ID3D12CommandSignature>,
}

// SAFETY: `Signature` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Signature {
    const NAME: &'static str = "command signature";
    const IIDS: &'static [Iid] = &[IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

impl Signature {
    /// The bytes from one record to the next, and what each record holds.
    #[allow(dead_code)] // the first user is ExecuteIndirect on a list
    pub(crate) fn layout(&self) -> (u32, &[Argument]) {
        (self.stride, &self.arguments)
    }

    /// The host signature, if this one needs no root signature.
    #[allow(dead_code)] // the first user is ExecuteIndirect on a list
    pub(crate) fn host(&self) -> Option<&ID3D12CommandSignature> {
        self.host.as_ref()
    }
}

/// Whether a signature of these arguments is the same on the host with no root signature:
/// every argument is a draw or a dispatch.
fn plain(arguments: &[Argument]) -> bool {
    !arguments.is_empty()
        && arguments
            .iter()
            .all(|argument| PLAIN_TYPES.contains(&argument[0]))
}

/// The signature has no methods of its own after the eight every device child has.
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
        let methods = object::child_methods::<Signature>();
        object::table("ID3D12CommandSignature", SLOTS, &NAMES, &methods)
    })
}

/// The host signature for plain `arguments`.
fn host_signature(stride: u32, arguments: &[Argument]) -> Result<ID3D12CommandSignature, Hresult> {
    let host = crate::host::host()?;
    let host_arguments: Vec<D3D12_INDIRECT_ARGUMENT_DESC> = arguments
        .iter()
        .map(|argument| D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE(argument[0] as i32),
            ..Default::default()
        })
        .collect();
    let description = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: host_arguments.len() as u32,
        pArgumentDescs: host_arguments.as_ptr(),
        NodeMask: 0,
    };
    let mut made: Option<ID3D12CommandSignature> = None;
    // SAFETY: a plain call on the host device; the description points at
    // `host_arguments`, which outlives the call.
    let result = unsafe {
        host.device
            .CreateCommandSignature(&description, None::<&ID3D12RootSignature>, &mut made)
    };
    match (result, made) {
        (Ok(()), Some(signature)) => Ok(signature),
        (result, _) => Err(result.err().map_or(E_INVALIDARG, |error| error.code().0)),
    }
}

/// `ID3D12Device::CreateCommandSignature`: `(description, root signature, interface id,
/// signature out)`.
///
/// # Safety
/// The title's pointers: each null or valid, and the description's argument list as long
/// as its count says.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    description: *const Description,
    root: usize,
    iid: *const Iid,
    out: *mut *const Signature,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    if description.count == 0 || description.arguments.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    // SAFETY: the title passes `count` arguments.
    let arguments =
        unsafe { std::slice::from_raw_parts(description.arguments, description.count as usize) }
            .to_vec();
    let host = if plain(&arguments) {
        match host_signature(description.stride, &arguments) {
            Ok(host) => Some(host),
            Err(code) => {
                trace!(
                    "the host refused a command signature (stride {}, {arguments:?}): {code:#x}",
                    description.stride
                );
                return code;
            }
        }
    } else {
        None
    };
    let signature = Box::new(Signature {
        object: Object::new(table(), 1),
        stride: description.stride,
        arguments,
        host,
    });
    trace!(
        "command signature {:p} made: stride {}, arguments {:?}, root signature {root:#x}{}",
        &raw const *signature,
        signature.stride,
        signature.arguments,
        if signature.host.is_some() {
            ""
        } else {
            "; its host signature waits for the root signature"
        }
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(signature, out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER};

    #[test]
    fn only_draws_and_dispatches_need_no_root_signature() {
        assert!(plain(&[[0, 0, 0, 0]]));
        assert!(plain(&[[1, 0, 0, 0], [2, 0, 0, 0]]));
        assert!(plain(&[[10, 0, 0x44ec_e5af, 1]]));
        // 105 is one of the console's own, not known here.
        assert!(!plain(&[[105, 0, 60000, 0]]));
        // A constant argument (type 5) names a root parameter.
        assert!(!plain(&[[5, 1, 0, 4], [1, 0, 0, 0]]));
        assert!(!plain(&[]));
    }

    #[test]
    fn a_signature_needs_a_description_with_arguments() {
        let arguments: [Argument; 1] = [[1, 0, 0, 0]];
        let mut description = Description {
            stride: 20,
            count: 1,
            arguments: arguments.as_ptr(),
            nodes: 0,
        };
        let mut out: *const Signature = std::ptr::dangling();
        let device = std::ptr::null();
        // SAFETY: valid pointers, and the null ones under test.
        unsafe {
            assert_eq!(
                create(device, std::ptr::null(), 0, &IID, &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                create(device, &description, 0, &[9u8; 16], &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                create(device, &description, 0, &IID, std::ptr::null_mut()),
                E_POINTER
            );
            description.count = 0;
            assert_eq!(
                create(device, &description, 0, &IID, &mut out),
                E_INVALIDARG
            );
            description.count = 1;
            description.arguments = std::ptr::null();
            assert_eq!(
                create(device, &description, 0, &IID, &mut out),
                E_INVALIDARG
            );
        }
    }
}
