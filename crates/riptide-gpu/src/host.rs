//! The host's D3D12 device: the real graphics card every console graphics object is built
//! on.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_1;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_MESSAGE, D3D12CreateDevice, D3D12GetDebugInterface, ID3D12Debug, ID3D12Device,
    ID3D12InfoQueue,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    IDXGIAdapter1, IDXGIFactory6,
};
use windows::core::Interface;

use crate::object::Hresult;
use crate::trace;

/// The host device and what it runs on.
pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) adapter: String,
}

// SAFETY: D3D12 devices are free-threaded: any thread may call them at any time.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Whether the software adapter was asked for (`RIPTIDE_ADAPTER=software`): for tests on
/// machines without a graphics card, never for running a title.
fn software() -> bool {
    std::env::var_os("RIPTIDE_ADAPTER").is_some_and(|value| value == "software")
}

/// Whether the host's own checking of every graphics call was asked for
/// (`RIPTIDE_GPU_CHECKS=1`). It is slow, and needs Windows' "Graphics Tools" feature. What
/// it finds is written to the session log.
fn checks() -> bool {
    std::env::var_os("RIPTIDE_GPU_CHECKS").is_some_and(|value| value == "1")
}

/// Writes what the host's checking has found since the last time to the session log, and
/// the reason if the host device has been lost. Does nothing unless checking is on or the
/// device is lost.
pub(crate) fn report_problems() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOST: AtomicBool = AtomicBool::new(false);
    let Ok(host) = host() else {
        return;
    };
    // SAFETY: plain calls on the host device and its message queue; each message is read
    // into a buffer of the size the queue says it needs.
    unsafe {
        if let Ok(queue) = host.device.cast::<ID3D12InfoQueue>() {
            let count = queue.GetNumStoredMessages();
            for index in 0..count {
                let mut bytes = 0usize;
                let _ = queue.GetMessage(index, None, &mut bytes);
                let mut buffer = vec![0u64; bytes.div_ceil(8) + 1];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(index, Some(message), &mut bytes).is_ok() {
                    let text = std::slice::from_raw_parts(
                        (*message).pDescription,
                        (*message).DescriptionByteLength.saturating_sub(1),
                    );
                    show_check(&String::from_utf8_lossy(text));
                }
            }
            queue.ClearStoredMessages();
        }
        if let Err(error) = host.device.GetDeviceRemovedReason()
            && !LOST.swap(true, Ordering::Relaxed)
        {
            trace!("the host graphics device is lost: {error}");
        }
    }
}

/// Writes one thing the host's checking found to the session log, the first time it is
/// said and then each time the count of it doubles: a message repeated every frame does
/// not crowd out the ones that are said once. Messages are told apart with the numbers in
/// them left out, since the same complaint names another object or address each time.
fn show_check(message: &str) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
    /// How many different messages are kept apart; past that, all are one.
    const MOST: usize = 512;
    let mut seen = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let seen = seen.get_or_insert_with(HashMap::new);
    let mut kind = without_numbers(message);
    if seen.len() >= MOST && !seen.contains_key(&kind) {
        kind = String::new();
    }
    let count = seen.entry(kind).or_insert(0);
    *count += 1;
    if *count == 1 {
        trace!("host check: {message}");
    } else if count.is_power_of_two() {
        trace!("host check ({count} times so far): {message}");
    }
}

/// A message with every run of digits, and every hexadecimal number written with `0x`,
/// left out.
fn without_numbers(message: &str) -> String {
    let mut kind = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(first) = rest.chars().next() {
        if let Some(digits) = rest.strip_prefix("0x").or(rest.strip_prefix("0X")) {
            rest = digits.trim_start_matches(|c: char| c.is_ascii_hexdigit());
        } else if first.is_ascii_digit() {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        } else {
            kind.push(first);
            rest = &rest[first.len_utf8()..];
        }
    }
    kind
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain calls; every out-pointer is valid.
    unsafe {
        if checks() {
            let mut debug: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut debug) {
                Ok(()) => {
                    if let Some(debug) = debug {
                        debug.EnableDebugLayer();
                        trace!("the host checks every graphics call (RIPTIDE_GPU_CHECKS=1)");
                    }
                }
                Err(error) => trace!("the host's checking is not installed: {error}"),
            }
        }
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter1 = if software() {
            factory.EnumWarpAdapter()?
        } else {
            factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?
        };
        let description = adapter.GetDesc1()?;
        let length = description
            .Description
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(description.Description.len());
        let name = String::from_utf16_lossy(&description.Description[..length]);
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_1, &mut device)?;
        let device =
            device.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        Ok(Host {
            device,
            adapter: name,
        })
    }
}

/// The host device, made the first time it is asked for. An error is the host's own result
/// code, and is given again each time.
pub(crate) fn host() -> Result<&'static Host, Hresult> {
    static HOST: OnceLock<Result<Host, Hresult>> = OnceLock::new();
    HOST.get_or_init(|| {
        create()
            .inspect(|host| trace!("host graphics device made on {}", host.adapter))
            .map_err(|error| {
                trace!("no host graphics device: {error}");
                error.code().0
            })
    })
    .as_ref()
    .map_err(|&code| code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_complaint_about_another_object_is_the_same_message() {
        let one = "Resource 0x000001E95A42EA30 (subresource 3) is in state 0x4, not 0x80";
        let other = "Resource 0x00000274802B1F40 (subresource 12) is in state 0x4, not 0x80";
        assert_eq!(without_numbers(one), without_numbers(other));
        assert_eq!(
            without_numbers(one),
            "Resource  (subresource ) is in state , not "
        );
        assert_ne!(
            without_numbers(one),
            without_numbers("Resource 0x1 is not a buffer")
        );
        // Text without numbers is itself.
        assert_eq!(without_numbers("no clear value"), "no clear value");
    }
}
