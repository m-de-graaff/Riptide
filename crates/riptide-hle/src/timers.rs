//! Sleeps and timed waits that take as long as they say (smaller decision S1).
//!
//! Windows wakes sleeping threads on its timer tick, 15.6 ms by default, so a 1 ms sleep
//! takes up to 15.6 ms. On the console it takes 1 ms, and a title that sleeps 1 ms in its
//! frame loop expects that. These functions wait on a high-resolution waitable timer
//! instead, which Windows fires at the time asked for.

use std::cell::OnceCell;
use std::ffi::c_void;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::sys;

const INFINITE: u32 = u32::MAX;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_ABANDONED_0: u32 = 0x80;
const WAIT_TIMEOUT: u32 = 258;

/// One thread's high-resolution timer, closed when the thread ends.
struct Timer(*mut c_void);

impl Timer {
    fn new() -> Option<Timer> {
        // SAFETY: creates a new, unnamed timer; touches nothing that exists.
        let handle = unsafe {
            sys::CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                sys::CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                sys::TIMER_ALL_ACCESS,
            )
        };
        (!handle.is_null()).then_some(Timer(handle))
    }

    /// Arms the timer to fire once, `milliseconds` from now. Arming it again first takes
    /// back a firing that is still pending.
    fn arm(&self, milliseconds: u32) -> bool {
        // A negative due time is relative, in units of 100 ns.
        let due = -(i64::from(milliseconds) * 10_000);
        // SAFETY: the handle is this thread's live timer, and `due` outlives the call.
        let armed = unsafe {
            sys::SetWaitableTimer(self.0, &due, 0, std::ptr::null(), std::ptr::null(), 0)
        };
        armed != 0
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateWaitableTimerExW and is closed once, here.
        unsafe { sys::CloseHandle(self.0) };
    }
}

thread_local! {
    /// This thread's timer, made the first time the thread sleeps. `None` if Windows would
    /// not make one.
    static TIMER: OnceCell<Option<Timer>> = const { OnceCell::new() };
}

/// Runs `wait` with this thread's timer armed for `milliseconds`, or returns `None` if
/// there is no timer to arm.
fn with_armed_timer<R>(milliseconds: u32, wait: impl FnOnce(*mut c_void) -> R) -> Option<R> {
    TIMER.with(|timer| {
        let timer = timer.get_or_init(Timer::new).as_ref()?;
        timer.arm(milliseconds).then(|| wait(timer.0))
    })
}

/// The title's `Sleep`: returns after `milliseconds`, not at the next timer tick.
///
/// Zero gives up the rest of the time slice, as on the console and on Windows.
pub extern "system" fn sleep(milliseconds: u32) {
    if milliseconds == 0 || milliseconds == INFINITE {
        // SAFETY: Sleep takes any value.
        unsafe { sys::Sleep(milliseconds) };
        return;
    }
    let waited = with_armed_timer(milliseconds, |timer| {
        // SAFETY: `timer` is this thread's live timer.
        unsafe { sys::WaitForSingleObject(timer, INFINITE) }
    });
    if waited != Some(WAIT_OBJECT_0) {
        // No timer, or the wait failed: a late wake is better than none.
        // SAFETY: Sleep takes any value.
        unsafe { sys::Sleep(milliseconds) };
    }
}

/// The title's `WaitForSingleObject`: as Windows' own, but a timeout ends when it says.
///
/// # Safety
///
/// `handle` is whatever the title passes; Windows checks it. This is only `unsafe` because
/// it is a raw system call on the title's behalf.
pub unsafe extern "system" fn wait_for_single_object(
    handle: *mut c_void,
    milliseconds: u32,
) -> u32 {
    if milliseconds == 0 || milliseconds == INFINITE {
        // SAFETY: Windows validates the handle and returns WAIT_FAILED for a bad one.
        return unsafe { sys::WaitForSingleObject(handle, milliseconds) };
    }
    let waited = with_armed_timer(milliseconds, |timer| {
        let handles = [handle, timer];
        // SAFETY: `handles` holds two handles and outlives the call; Windows validates the
        // title's one.
        unsafe { sys::WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) }
    });
    match waited {
        // The title's handle is first, so its results come back unchanged.
        Some(WAIT_OBJECT_0) => WAIT_OBJECT_0,
        Some(WAIT_ABANDONED_0) => WAIT_ABANDONED_0,
        // The timer fired first.
        Some(result) if result == WAIT_OBJECT_0 + 1 => WAIT_TIMEOUT,
        // WAIT_FAILED, with the thread's last error set by Windows.
        Some(result) => result,
        // SAFETY: as above; without a timer, Windows' own timeout has to do.
        None => unsafe { sys::WaitForSingleObject(handle, milliseconds) },
    }
}

/// Adds the timer functions to the import table.
pub(crate) fn register(imports: &mut Imports) {
    // SAFETY: each function has the signature of the Windows function it stands in for.
    unsafe {
        imports.implement(
            "kernel32.dll",
            Symbol::Name("Sleep".to_owned()),
            sleep as *const () as usize,
        );
        imports.implement(
            "kernel32.dll",
            Symbol::Name("WaitForSingleObject".to_owned()),
            wait_for_single_object as *const () as usize,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(
            attributes: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn SetEvent(event: *mut c_void) -> i32;
    }

    /// A shared runner's timers jitter too much to hold a 2 ms bar; there the upper bounds
    /// are skipped and only "long enough" is checked.
    fn on_ci() -> bool {
        std::env::var_os("CI").is_some()
    }

    fn event(signalled: bool) -> *mut c_void {
        // SAFETY: creates a new, unnamed, auto-reset event.
        let event =
            unsafe { CreateEventW(std::ptr::null(), 0, i32::from(signalled), std::ptr::null()) };
        assert!(!event.is_null());
        event
    }

    #[test]
    fn a_1_ms_sleep_takes_at_least_1_ms_and_under_2_ms_over_1000_calls() {
        let mut times = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            sleep(1);
            times.push(start.elapsed());
        }
        let total: Duration = times.iter().sum();
        let mean = total / 1000;
        times.sort();
        let (median, worst) = (times[500], times[999]);
        println!("1 ms sleep over 1000 calls: mean {mean:?}, median {median:?}, worst {worst:?}");

        assert!(
            times[0] >= Duration::from_micros(900),
            "shortest {:?}",
            times[0]
        );
        if !on_ci() {
            assert!(mean < Duration::from_millis(2), "mean {mean:?}");
            assert!(median < Duration::from_millis(2), "median {median:?}");
        }
    }

    #[test]
    fn a_longer_sleep_takes_its_time() {
        let start = Instant::now();
        sleep(20);
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(19), "{took:?}");
        if !on_ci() {
            assert!(took < Duration::from_millis(25), "{took:?}");
        }
    }

    #[test]
    fn a_zero_sleep_returns_at_once() {
        let start = Instant::now();
        sleep(0);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_timed_wait_on_an_unsignalled_event_times_out_on_time() {
        let event = event(false);
        let start = Instant::now();
        // SAFETY: `event` is a live event handle.
        let result = unsafe { wait_for_single_object(event, 3) };
        let took = start.elapsed();
        assert_eq!(result, WAIT_TIMEOUT);
        assert!(took >= Duration::from_micros(2900), "{took:?}");
        if !on_ci() {
            assert!(took < Duration::from_millis(6), "{took:?}");
        }
        // SAFETY: the handle is closed once.
        unsafe { sys::CloseHandle(event) };
    }

    #[test]
    fn a_timed_wait_on_a_signalled_event_returns_at_once_with_the_event() {
        let event = event(true);
        let start = Instant::now();
        // SAFETY: `event` is a live event handle.
        let result = unsafe { wait_for_single_object(event, 500) };
        assert_eq!(result, WAIT_OBJECT_0);
        assert!(start.elapsed() < Duration::from_millis(100));
        // SAFETY: the handle is closed once.
        unsafe { sys::CloseHandle(event) };
    }

    #[test]
    fn a_timed_wait_ends_when_another_thread_signals() {
        let event = event(false) as usize;
        let signaller = std::thread::spawn(move || {
            sleep(5);
            // SAFETY: the event is still open; the waiting thread closes it after the join.
            unsafe { SetEvent(event as *mut c_void) };
        });
        let start = Instant::now();
        // SAFETY: `event` is a live event handle.
        let result = unsafe { wait_for_single_object(event as *mut c_void, 2000) };
        let took = start.elapsed();
        signaller.join().unwrap();
        assert_eq!(result, WAIT_OBJECT_0);
        assert!(took < Duration::from_millis(1000), "{took:?}");
        // SAFETY: the handle is closed once.
        unsafe { sys::CloseHandle(event as *mut c_void) };
    }

    #[test]
    fn a_wait_after_an_unused_timeout_is_not_cut_short_by_the_old_one() {
        // The first wait arms the timer for 50 ms and returns at once. The second must wait
        // its own 10 ms, not wake on the first one's leftover.
        let signalled = event(true);
        let silent = event(false);
        // SAFETY: both are live event handles.
        unsafe { wait_for_single_object(signalled, 50) };
        let start = Instant::now();
        // SAFETY: as above.
        let result = unsafe { wait_for_single_object(silent, 10) };
        let took = start.elapsed();
        assert_eq!(result, WAIT_TIMEOUT);
        assert!(took >= Duration::from_micros(9900), "{took:?}");
        // SAFETY: each handle is closed once.
        unsafe {
            sys::CloseHandle(signalled);
            sys::CloseHandle(silent);
        }
    }

    #[test]
    fn a_bad_handle_fails_as_windows_fails_it() {
        const WAIT_FAILED: u32 = u32::MAX;
        // SAFETY: an invalid handle is what this test passes on purpose; Windows rejects it.
        let result = unsafe { wait_for_single_object(0x1234_5678 as *mut c_void, 5) };
        assert_eq!(result, WAIT_FAILED);
    }
}
