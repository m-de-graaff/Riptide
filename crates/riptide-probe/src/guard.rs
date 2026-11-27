//! What catching the first write to protected title memory costs (open question Q4).

use crate::buffers::{GIB, MIB, TILE, pattern, upload};
use crate::device::Gpu;
use crate::inplace::InPlace;
use crate::report::{Facts, GuardCost};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RemoveVectoredExceptionHandler,
};
use windows::Win32::System::Memory::{
    PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, VirtualProtect,
};

pub const PAGE: usize = 4096;
const EXCEPTION_ACCESS_VIOLATION: i32 = 0xC000_0005_u32 as i32;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

static GUARD_BASE: AtomicUsize = AtomicUsize::new(0);
static GUARD_LEN: AtomicUsize = AtomicUsize::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// Unprotects the page of a write fault inside the guarded range and retries the write.
unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the OS passes valid exception pointers to a vectored handler.
    let record = unsafe { &*(*info).ExceptionRecord };
    // ExceptionInformation[0] == 1 means a write; [1] is the address written.
    if record.ExceptionCode.0 != EXCEPTION_ACCESS_VIOLATION || record.ExceptionInformation[0] != 1 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let address = record.ExceptionInformation[1];
    let base = GUARD_BASE.load(Relaxed);
    if address < base || address >= base + GUARD_LEN.load(Relaxed) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the page lies inside the guarded range, which the caller of `measure` owns.
    if unsafe {
        VirtualProtect(
            (address & !(PAGE - 1)) as *const c_void,
            PAGE,
            PAGE_READWRITE,
            &mut old,
        )
    }
    .is_err()
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    FAULTS.fetch_add(1, Relaxed);
    EXCEPTION_CONTINUE_EXECUTION
}

pub fn measure(base: *mut u8, len: usize) -> Result<GuardCost, String> {
    assert!((base as usize).is_multiple_of(PAGE) && len.is_multiple_of(PAGE) && len > 0);
    let pages = len / PAGE;
    GUARD_BASE.store(base as usize, Relaxed);
    GUARD_LEN.store(len, Relaxed);
    FAULTS.store(0, Relaxed);
    // SAFETY: `on_fault` only acts on write faults inside the guarded range.
    let handler = unsafe { AddVectoredExceptionHandler(1, Some(on_fault)) };
    if handler.is_null() {
        return Err("AddVectoredExceptionHandler failed".into());
    }

    let mut old = PAGE_PROTECTION_FLAGS(0);
    let t = Instant::now();
    // SAFETY: the caller guarantees `len` committed bytes at `base`.
    let protected = unsafe { VirtualProtect(base.cast(), len, PAGE_READONLY, &mut old) };
    let protect = t.elapsed();

    let mut writes = std::time::Duration::ZERO;
    if protected.is_ok() {
        let t = Instant::now();
        for page in 0..pages {
            // SAFETY: inside the range; the fault handler makes the page writable and the
            // write is retried.
            unsafe { base.add(page * PAGE).write_volatile(1) };
        }
        writes = t.elapsed();
    }

    // SAFETY: `handler` is the value AddVectoredExceptionHandler returned.
    unsafe { RemoveVectoredExceptionHandler(handler) };
    GUARD_LEN.store(0, Relaxed);
    protected.map_err(|e| format!("VirtualProtect: {e}"))?;
    let faults = FAULTS.load(Relaxed);
    if faults != pages as u64 {
        return Err(format!("{faults} faults for {pages} protected pages"));
    }
    Ok(GuardCost {
        fault_us: writes.as_secs_f64() * 1e6 / pages as f64,
        protect_us_per_gib: protect.as_secs_f64() * 1e6 * (GIB as f64 / len as f64),
    })
}

pub fn check_guard(gpu: &Gpu, mem: &InPlace, facts: &mut Facts) {
    let len = 64 * MIB;
    assert!(mem.bytes >= len);
    mem.slice(0, len).fill(0);
    match measure(mem.view, len as usize) {
        Ok(cost) => facts.guard = Some(cost),
        Err(e) => {
            facts.notes.push(format!("guard on in-place memory: {e}"));
            return;
        }
    }

    // Does a GPU write to a protected page land, and without a fault?
    let data = pattern(0x55, TILE as usize);
    let mut old = PAGE_PROTECTION_FLAGS(0);
    FAULTS.store(0, Relaxed);
    // SAFETY: the first tile of the view is committed and owned by `mem`.
    let protected =
        unsafe { VirtualProtect(mem.view.cast(), TILE as usize, PAGE_READONLY, &mut old) };
    if let Err(e) = protected {
        facts.notes.push(format!("protect before GPU write: {e}"));
        return;
    }
    let wrote = upload(gpu, &mem.buffer, 0, &data);
    let landed = mem.slice(0, TILE) == data.as_slice();
    // SAFETY: restoring the protection changed above.
    let _ = unsafe { VirtualProtect(mem.view.cast(), TILE as usize, PAGE_READWRITE, &mut old) };
    match wrote {
        Ok(()) => facts.gpu_writes_bypass_guard = Some(landed && FAULTS.load(Relaxed) == 0),
        Err(e) => {
            facts.gpu_writes_bypass_guard = Some(false);
            facts
                .notes
                .push(format!("GPU write to a protected page: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn every_protected_page_faults_once_and_stays_writable() {
        let len = 256 * PAGE;
        // SAFETY: a fresh private allocation, released at the end of the test.
        let base = unsafe { VirtualAlloc(None, len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }
            .cast::<u8>();
        assert!(!base.is_null());
        let cost = measure(base, len).unwrap();
        assert!(cost.fault_us > 0.0 && cost.protect_us_per_gib > 0.0);
        // SAFETY: the range is writable again after `measure`; a second write must not fault.
        unsafe { base.write_volatile(7) };
        // SAFETY: releasing the allocation made above.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }.unwrap();
    }

    #[test]
    #[ignore = "needs the target machine"]
    fn guards_work_on_in_place_memory() {
        let gpu = crate::device::open(false).unwrap();
        let mem = crate::inplace::open(
            &gpu,
            GIB,
            windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_NONE,
        )
        .unwrap();
        let mut facts = Facts::default();
        check_guard(&gpu, &mem, &mut facts);
        assert!(facts.guard.is_some(), "notes: {:?}", facts.notes);
        eprintln!(
            "{:?} gpu bypass: {:?}",
            facts.guard, facts.gpu_writes_bypass_guard
        );
    }
}
