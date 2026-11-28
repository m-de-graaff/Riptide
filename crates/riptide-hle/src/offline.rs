//! Keeping a title off the network.
//!
//! A title's sockets and HTTP calls are Windows' own, so without this a title could reach
//! any server from the user's machine. Riptide answers the console's networking area as a
//! console with no connection, and this makes that true: the calls that start talking to
//! another machine are refused, with the error Windows gives when there is no network.
//!
//! Refused: connecting a socket, sending a datagram, looking a host name up, and opening an
//! HTTP connection. Everything else about sockets and HTTP is Windows', so a title's
//! network code runs and finds nothing reachable. Each refusal is written to the session
//! log the first few times, with where the title wanted to go.

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::trace::trace;

/// Winsock's errors for "the network cannot be reached" and "no such host".
const WSAENETUNREACH: i32 = 10051;
const WSAHOST_NOT_FOUND: i32 = 11001;
/// WinHTTP's error for "the server cannot be reached".
const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;
const SOCKET_ERROR: i32 = -1;

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSASetLastError(error: i32);
}

/// Says what was refused, the first few times.
fn show(line: std::fmt::Arguments<'_>) {
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 12 {
        trace!("offline: {line}");
    }
}

/// An IPv4 or IPv6 socket address as text, as far as it can be read.
///
/// # Safety
/// `address` is null or `length` readable bytes.
unsafe fn address_text(address: *const u8, length: i32) -> String {
    if address.is_null() || length < 8 {
        return "an address".to_owned();
    }
    // SAFETY: at least 8 bytes, as checked.
    let bytes = unsafe { std::slice::from_raw_parts(address, length as usize) };
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    match family {
        2 => format!("{}.{}.{}.{}:{port}", bytes[4], bytes[5], bytes[6], bytes[7]),
        23 if bytes.len() >= 24 => {
            let mut groups = [0u16; 8];
            for (group, pair) in groups.iter_mut().zip(bytes[8..24].chunks_exact(2)) {
                *group = u16::from_be_bytes([pair[0], pair[1]]);
            }
            format!("[{}]:{port}", std::net::Ipv6Addr::from(groups))
        }
        _ => format!("an address of family {family}"),
    }
}

/// `ws2_32.dll!connect`: `(socket, address, address length)`.
///
/// # Safety
/// `address` is null or `length` readable bytes.
unsafe extern "system" fn connect(_socket: usize, address: *const u8, length: i32) -> i32 {
    // SAFETY: the title's address.
    let to = unsafe { address_text(address, length) };
    show(format_args!("a socket connection to {to} is refused"));
    // SAFETY: a plain call.
    unsafe { WSASetLastError(WSAENETUNREACH) };
    SOCKET_ERROR
}

/// `ws2_32.dll!sendto`: `(socket, data, data length, flags, address, address length)`.
///
/// # Safety
/// `address` is null or `length` readable bytes.
unsafe extern "system" fn send_to(
    _socket: usize,
    _data: *const u8,
    _bytes: i32,
    _flags: i32,
    address: *const u8,
    length: i32,
) -> i32 {
    // SAFETY: the title's address.
    let to = unsafe { address_text(address, length) };
    show(format_args!("a datagram to {to} is not sent"));
    // SAFETY: a plain call.
    unsafe { WSASetLastError(WSAENETUNREACH) };
    SOCKET_ERROR
}

/// `ws2_32.dll!getaddrinfo`: `(host name, service, hints, results out)`.
///
/// # Safety
/// `name` is null or a null-terminated string; `results` is null or where the list goes.
unsafe extern "system" fn get_address_info(
    name: *const c_char,
    _service: *const c_char,
    _hints: *const c_void,
    results: *mut *mut c_void,
) -> i32 {
    // SAFETY: the title's string and out-pointer.
    unsafe {
        let name = name
            .as_ref()
            .map(|_| CStr::from_ptr(name).to_string_lossy());
        show(format_args!(
            "the address of {} is not looked up",
            name.as_deref().unwrap_or("this machine")
        ));
        if let Some(results) = results.as_mut() {
            *results = std::ptr::null_mut();
        }
        WSASetLastError(WSAHOST_NOT_FOUND);
    }
    WSAHOST_NOT_FOUND
}

/// `ws2_32.dll!gethostbyname`: `(host name)`, returning the host's entry or null.
///
/// # Safety
/// `name` is null or a null-terminated string.
unsafe extern "system" fn get_host_by_name(name: *const c_char) -> *mut c_void {
    // SAFETY: the title's string.
    unsafe {
        let name = name
            .as_ref()
            .map(|_| CStr::from_ptr(name).to_string_lossy());
        show(format_args!(
            "the address of {} is not looked up",
            name.as_deref().unwrap_or("this machine")
        ));
        WSASetLastError(WSAHOST_NOT_FOUND);
    }
    std::ptr::null_mut()
}

/// `winhttp.dll!WinHttpConnect`: `(session, server name, port, reserved)`, returning the
/// connection or null. Every HTTP request needs one.
///
/// # Safety
/// `server` is null or a null-terminated UTF-16 string.
unsafe extern "system" fn win_http_connect(
    _session: *mut c_void,
    server: *const u16,
    port: u16,
    _reserved: u32,
) -> *mut c_void {
    let mut name = String::new();
    if !server.is_null() {
        let mut length = 0;
        // SAFETY: the string is null-terminated; reading stops at the terminator or at a
        // length no host name has.
        unsafe {
            while length < 512 && *server.add(length) != 0 {
                length += 1;
            }
            name = String::from_utf16_lossy(std::slice::from_raw_parts(server, length));
        }
    }
    show(format_args!(
        "an HTTP connection to {name}:{port} is refused"
    ));
    // SAFETY: a plain call.
    unsafe { crate::sys::SetLastError(ERROR_WINHTTP_CANNOT_CONNECT) };
    std::ptr::null_mut()
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, Symbol, usize); 7] = [
        // Winsock's first functions are imported by number: 4 is `connect`, 20 `sendto`
        // and 52 `gethostbyname`.
        (
            "ws2_32.dll",
            Symbol::Ordinal(4),
            connect as *const () as usize,
        ),
        (
            "ws2_32.dll",
            Symbol::Ordinal(20),
            send_to as *const () as usize,
        ),
        (
            "ws2_32.dll",
            Symbol::Ordinal(52),
            get_host_by_name as *const () as usize,
        ),
        (
            "ws2_32.dll",
            Symbol::Name("connect".to_owned()),
            connect as *const () as usize,
        ),
        (
            "ws2_32.dll",
            Symbol::Name("sendto".to_owned()),
            send_to as *const () as usize,
        ),
        (
            "ws2_32.dll",
            Symbol::Name("getaddrinfo".to_owned()),
            get_address_info as *const () as usize,
        ),
        (
            "winhttp.dll",
            Symbol::Name("WinHttpConnect".to_owned()),
            win_http_connect as *const () as usize,
        ),
    ];
    for (module, symbol, address) in functions {
        // SAFETY: each function has the arguments of the Windows function it stands in for.
        unsafe { imports.implement(module, symbol, address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_connected_sent_or_looked_up() {
        // An IPv4 address: family 2, port 443, 203.0.113.9.
        let address = [2u8, 0, 0x01, 0xbb, 203, 0, 113, 9, 0, 0, 0, 0, 0, 0, 0, 0];
        let server: Vec<u16> = "example.invalid\0".encode_utf16().collect();
        let mut results: *mut c_void = std::ptr::dangling_mut();
        // SAFETY: valid addresses and strings.
        unsafe {
            assert_eq!(address_text(address.as_ptr(), 16), "203.0.113.9:443");
            assert_eq!(connect(0, address.as_ptr(), 16), SOCKET_ERROR);
            assert_eq!(
                send_to(0, std::ptr::null(), 0, 0, address.as_ptr(), 16),
                SOCKET_ERROR
            );
            assert_eq!(
                get_address_info(
                    c"example.invalid".as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut results
                ),
                WSAHOST_NOT_FOUND
            );
            assert!(results.is_null());
            assert!(get_host_by_name(c"example.invalid".as_ptr()).is_null());
            assert!(win_http_connect(std::ptr::null_mut(), server.as_ptr(), 443, 0).is_null());
            assert_eq!(address_text(std::ptr::null(), 0), "an address");
        }
    }

    #[test]
    fn an_ipv6_address_is_written_with_its_port() {
        let mut address = [0u8; 28];
        (address[0], address[2], address[3]) = (23, 0x1f, 0x90);
        address[23] = 1;
        // SAFETY: 28 readable bytes.
        assert_eq!(unsafe { address_text(address.as_ptr(), 28) }, "[::1]:8080");
    }
}
