//! Pointing an image's thread-local accesses at the slot it was given.
//!
//! Code reaches its thread-local block in two steps: it loads the thread's TLS array from
//! `gs:[0x58]`, then reads the entry at the image's TLS index. A compiler building a
//! program's main executable may leave the index out and read entry 0, because Windows
//! always gives the executable index 0. A title is such an executable, but here it is not
//! the process's own: entry 0 belongs to Riptide, and the title's block is at the index its
//! carrier got (see `carrier.rs`).
//!
//! So every load of the TLS array in the image's code is replaced by a call to a small
//! routine that gives the address of the title's entry instead of the array's start. Code
//! that then reads "entry 0" reads the title's entry. Code that does add the image's index
//! adds zero, which is what the index is set to.

/// Bytes each routine has room for.
pub(crate) const THUNK_BYTES: usize = 32;

/// `mov reg, gs:[0x58]` is `65 REX 8B ModRM 25 58 00 00 00`.
const SITE_BYTES: usize = 9;
const TAIL: [u8; 5] = [0x25, 0x58, 0x00, 0x00, 0x00];

/// If the bytes at the start of `code` are `mov reg, gs:[0x58]` for a 64-bit register,
/// returns that register's number (0 for rax to 15 for r15).
fn site(code: &[u8]) -> Option<u8> {
    let code = code.get(..SITE_BYTES)?;
    if code[0] != 0x65 || code[2] != 0x8B || code[4..] != TAIL {
        return None;
    }
    let high = match code[1] {
        0x48 => 0,
        0x4C => 8,
        _ => return None,
    };
    // ModRM with no base register and an absolute address: only the register field varies.
    if code[3] & 0xC7 != 0x04 {
        return None;
    }
    Some(high | ((code[3] >> 3) & 7))
}

/// The routine for `register`: load the TLS array, step to entry `index`, return.
///
/// ```text
/// mov reg, gs:[0x58]
/// lea reg, [reg + index * 8]   ; lea leaves the flags alone
/// ret
/// ```
fn thunk(register: u8, index: u32) -> [u8; THUNK_BYTES] {
    let (high, low) = (register >> 3, register & 7);
    let mut code = Vec::with_capacity(THUNK_BYTES);
    code.extend_from_slice(&[0x65, 0x48 | (high << 2), 0x8B, 0x04 | (low << 3)]);
    code.extend_from_slice(&TAIL);
    code.extend_from_slice(&[0x48 | (high << 2) | high, 0x8D, 0x80 | (low << 3) | low]);
    if low == 4 {
        // rsp and r12 as a base need a SIB byte.
        code.push(0x24);
    }
    code.extend_from_slice(&(index * 8).to_le_bytes());
    code.push(0xC3);
    // Pad with breakpoints.
    code.resize(THUNK_BYTES, 0xCC);
    code.try_into().expect("a routine fits its room")
}

/// Writes the sixteen routines for `index` at `thunks_at` in `memory`, and replaces every
/// load of the TLS array inside `code_ranges` with a call to the routine for its register.
/// Returns how many loads were replaced.
///
/// `memory` is a mapped image and the ranges are its executable sections, as offsets. The
/// routines must lie within 2 GiB of every load, which they do inside one image.
pub(crate) fn redirect(
    memory: &mut [u8],
    code_ranges: &[std::ops::Range<usize>],
    thunks_at: usize,
    index: u32,
) -> usize {
    for register in 0..16u8 {
        let at = thunks_at + usize::from(register) * THUNK_BYTES;
        memory[at..at + THUNK_BYTES].copy_from_slice(&thunk(register, index));
    }
    let mut replaced = 0;
    for range in code_ranges {
        let mut at = range.start;
        while at + SITE_BYTES <= range.end {
            let Some(register) = site(&memory[at..range.end]) else {
                at += 1;
                continue;
            };
            let target = thunks_at + usize::from(register) * THUNK_BYTES;
            // A call's operand is counted from the end of its five bytes.
            let relative = target as i64 - (at as i64 + 5);
            let relative = i32::try_from(relative).expect("the routines are in the image");
            memory[at] = 0xE8;
            memory[at + 1..at + 5].copy_from_slice(&relative.to_le_bytes());
            // A four-byte no-op fills the rest of the nine.
            memory[at + 5..at + SITE_BYTES].copy_from_slice(&[0x0F, 0x1F, 0x40, 0x00]);
            replaced += 1;
            at += SITE_BYTES;
        }
    }
    replaced
}

#[cfg(test)]
// A list of code ranges is meant as a list, also when it holds one.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn only_a_load_of_the_tls_array_is_a_site() {
        assert_eq!(
            site(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]),
            Some(0)
        );
        assert_eq!(
            site(&[0x65, 0x48, 0x8B, 0x0C, 0x25, 0x58, 0, 0, 0]),
            Some(1)
        );
        assert_eq!(
            site(&[0x65, 0x4C, 0x8B, 0x3C, 0x25, 0x58, 0, 0, 0]),
            Some(15)
        );
        // Another offset in the thread's block, a 32-bit load, another segment, too short.
        assert_eq!(site(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0x30, 0, 0, 0]), None);
        assert_eq!(site(&[0x65, 0x40, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]), None);
        assert_eq!(site(&[0x64, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]), None);
        assert_eq!(site(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0]), None);
    }

    #[test]
    fn the_routines_are_the_instructions_they_claim() {
        // mov rax, gs:[0x58]; lea rax, [rax + 0x18]; ret
        assert_eq!(
            thunk(0, 3)[..17],
            [
                0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0, 0x48, 0x8D, 0x80, 0x18, 0, 0, 0, 0xC3
            ]
        );
        // mov r12, gs:[0x58]; lea r12, [r12 + 8]; ret, with the SIB byte r12 needs.
        assert_eq!(
            thunk(12, 1)[..18],
            [
                0x65, 0x4C, 0x8B, 0x24, 0x25, 0x58, 0, 0, 0, 0x4D, 0x8D, 0xA4, 0x24, 0x08, 0, 0, 0,
                0xC3
            ]
        );
        // mov r9, gs:[0x58]; lea r9, [r9 + 0x10]; ret
        assert_eq!(
            thunk(9, 2)[..17],
            [
                0x65, 0x4C, 0x8B, 0x0C, 0x25, 0x58, 0, 0, 0, 0x4D, 0x8D, 0x89, 0x10, 0, 0, 0, 0xC3
            ]
        );
    }

    #[test]
    fn loads_in_code_are_replaced_by_calls_and_data_is_left_alone() {
        let load_rcx = [0x65, 0x48, 0x8B, 0x0C, 0x25, 0x58, 0, 0, 0];
        let mut memory = vec![0x90u8; 0x3000];
        memory[0x1010..0x1019].copy_from_slice(&load_rcx);
        memory[0x1100..0x1109].copy_from_slice(&load_rcx);
        // The same bytes outside the code range are data.
        memory[0x2800..0x2809].copy_from_slice(&load_rcx);

        let replaced = redirect(&mut memory, &[0x1000..0x2000], 0x2000, 5);
        assert_eq!(replaced, 2);
        let target = 0x2000 + THUNK_BYTES as i32;
        let mut expected = vec![0xE8];
        expected.extend_from_slice(&(target - 0x1015).to_le_bytes());
        expected.extend_from_slice(&[0x0F, 0x1F, 0x40, 0x00]);
        assert_eq!(memory[0x1010..0x1019], expected[..]);
        assert_eq!(memory[0x2800..0x2809], load_rcx);
        assert_eq!(memory[0x2000..0x2000 + THUNK_BYTES], thunk(0, 5));
    }

    /// Runs a routine and a patched load for real: the value that comes back is the address
    /// of this thread's entry `index` in its TLS array.
    #[test]
    fn a_patched_load_gives_the_address_of_the_entry() {
        use crate::sys;
        let index = 3u32;
        // A page of code: a patched load into rax, then ret; the routines follow.
        let mut page = vec![0xCCu8; 0x1000];
        page[..9].copy_from_slice(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]);
        page[9] = 0xC3;
        assert_eq!(redirect(&mut page, &[0..16], 0x100, index), 1);

        // SAFETY: a fresh page is filled with the code above, made executable and called as
        // a function with no arguments that returns in rax; then it is freed.
        unsafe {
            let memory = sys::VirtualAlloc(
                std::ptr::null_mut(),
                0x1000,
                sys::MEM_RESERVE | sys::MEM_COMMIT,
                sys::PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!memory.is_null());
            std::ptr::copy_nonoverlapping(page.as_ptr(), memory, 0x1000);
            let mut old = 0u32;
            assert_ne!(
                sys::VirtualProtect(memory.cast(), 0x1000, sys::PAGE_EXECUTE_READ, &mut old),
                0
            );
            let call = std::mem::transmute::<*mut u8, extern "system" fn() -> usize>(memory);
            let array: usize;
            std::arch::asm!("mov {}, gs:[0x58]", out(reg) array);
            assert_eq!(call(), array + index as usize * 8);
            sys::VirtualFree(memory.cast(), 0, sys::MEM_RELEASE);
        }
    }
}
