//! Frame events: how a title learns when to start its next frame.
//!
//! A title sets a frame interval, and then waits for the frame-origin event before it
//! starts each frame. Each origin wait that ends gives the title a token, which it hands
//! back with what it submits and presents for that frame.
//!
//! Until frames are presented the events come from a clock, not from the screen. Its ticks
//! are at whole intervals from when the interval was set, so a late wait does not make the
//! next one late too.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::device::Device;
use crate::object::{Hresult, S_OK};
use crate::trace;

/// `S_FALSE`: what a wait answers when its time is up before the event.
const TIMED_OUT: Hresult = 1;
/// The frame-origin event, the one whose waits start frames.
const ORIGIN: u32 = 0;
/// Wait flag: give the frame its token even if the wait times out.
const TOKEN_ON_TIMEOUT: u32 = 2;
/// What an interval of zero means: the screen's own, taken as 60 Hz.
const SIXTIETH: Duration = Duration::from_micros(16_667);

/// The kinds of event a title can wait for, as far as each keeps its own place.
const KINDS: usize = 3;

struct Clock {
    start: Instant,
    interval: Duration,
    /// The ticks counted before `start`, so ticks never repeat when the interval changes.
    before: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
/// Frames begun: the last token handed out.
static FRAMES: AtomicU64 = AtomicU64::new(0);
/// Per kind of event, the tick the last wait for it ended on.
static LAST: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(KINDS - 1)]
}

/// The tick a wait ends on and how long until it, `elapsed` after the clock started.
///
/// An event happens once per tick. If one has happened since the last wait for it ended,
/// the wait ends at once on the newest; otherwise it ends on the next.
fn next(elapsed: Duration, interval: Duration, before: u64, last: u64) -> (u64, Duration) {
    let nanos = interval.as_nanos().max(1);
    let passed = (elapsed.as_nanos() / nanos) as u64;
    let newest = before + passed;
    if newest > last {
        return (newest, Duration::ZERO);
    }
    let tick = last + 1;
    let at = u128::from(tick - before) * nanos;
    let wait = at.saturating_sub(elapsed.as_nanos());
    (
        tick,
        Duration::from_nanos(u64::try_from(wait).unwrap_or(u64::MAX)),
    )
}

/// `SetFrameIntervalX`: `(output, interval in microseconds, period, flags)`. A frame starts
/// every interval. The period is how many intervals a frame may take to reach the screen,
/// which changes nothing here.
pub(crate) extern "system" fn set_interval(
    _this: &Device,
    output: *const c_void,
    length: u32,
    period: u32,
    flags: u32,
) -> Hresult {
    let interval = match length {
        0 => SIXTIETH,
        micros => Duration::from_micros(u64::from(micros)),
    };
    trace!(
        "SetFrameIntervalX(output {output:p}, {length} us, period {period}, flags {flags:#x}): a frame every {interval:?}"
    );
    let before = LAST
        .iter()
        .map(|tick| tick.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *CLOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Clock {
        start: Instant::now(),
        interval,
        before,
    });
    S_OK
}

/// `ScheduleFrameEventX`: `(kind, offset in microseconds, list, flags)`. Every kind of
/// event already happens at every tick. The offset moves an event against the screen's
/// refresh, which the clock does not follow yet.
pub(crate) extern "system" fn schedule(
    _this: &Device,
    kind: u32,
    offset: u32,
    list: *const c_void,
    flags: u32,
) -> Hresult {
    trace!("ScheduleFrameEventX(kind {kind}, offset {offset} us, list {list:p}, flags {flags:#x})");
    S_OK
}

/// `WaitFrameEventX`: `(kind, time-out in milliseconds, list, flags, token out)`. Blocks
/// until the event or the time-out. An origin wait that ends starts a frame and writes its
/// token.
///
/// # Safety
/// `token` is null or where the token goes.
pub(crate) unsafe extern "system" fn wait(
    _this: &Device,
    kind: u32,
    timeout: u32,
    _list: *const c_void,
    flags: u32,
    token: *mut u64,
) -> Hresult {
    let (tick, wait) = {
        let mut clock = CLOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let clock = clock.get_or_insert_with(|| Clock {
            start: Instant::now(),
            interval: SIXTIETH,
            before: 0,
        });
        next(
            clock.start.elapsed(),
            clock.interval,
            clock.before,
            last(kind).load(Ordering::Relaxed),
        )
    };
    let limit = if timeout == u32::MAX {
        Duration::MAX
    } else {
        Duration::from_millis(u64::from(timeout))
    };
    let result = if wait > limit {
        std::thread::sleep(limit);
        TIMED_OUT
    } else {
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    if kind == ORIGIN && (result == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
        if frame <= 3 {
            trace!("frame {frame} begins (origin wait: time-out {timeout} ms, flags {flags:#x})");
        }
        // SAFETY: the title's token pointer; null is tolerated.
        if let Some(token) = unsafe { token.as_mut() } {
            *token = frame;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(10);

    #[test]
    fn a_prompt_title_waits_for_the_next_tick() {
        // The last wait ended on tick 3; 32 ms in, tick 4 is 8 ms away.
        assert_eq!(
            next(Duration::from_millis(32), INTERVAL, 0, 3),
            (4, Duration::from_millis(8))
        );
        // The very first wait ends on tick 1.
        assert_eq!(next(Duration::ZERO, INTERVAL, 0, 0), (1, INTERVAL));
    }

    #[test]
    fn a_late_title_does_not_wait_and_takes_the_newest_tick() {
        assert_eq!(
            next(Duration::from_millis(57), INTERVAL, 0, 3),
            (5, Duration::ZERO)
        );
    }

    #[test]
    fn ticks_go_on_from_where_an_older_interval_left_them() {
        // 100 ticks before this interval was set, the last wait on tick 100.
        assert_eq!(
            next(Duration::from_millis(3), INTERVAL, 100, 100),
            (101, Duration::from_millis(7))
        );
        // A wait woken a little early still ends on a tick of its own.
        assert_eq!(
            next(Duration::from_micros(9_990), INTERVAL, 100, 101),
            (102, Duration::from_micros(10_010))
        );
    }

    #[test]
    fn an_origin_wait_that_ends_gives_the_next_token() {
        let device = crate::device::device();
        let mut first = 0;
        let mut second = 0;
        // SAFETY: valid token pointers, and null.
        unsafe {
            assert_eq!(set_interval(device, std::ptr::null(), 1000, 1, 0), S_OK);
            assert_eq!(
                wait(device, ORIGIN, 1000, std::ptr::null(), 0, &mut first),
                S_OK
            );
            assert_eq!(
                wait(device, ORIGIN, 1000, std::ptr::null(), 0, &mut second),
                S_OK
            );
            assert_eq!(
                wait(device, 1, 1000, std::ptr::null(), 0, std::ptr::null_mut()),
                S_OK
            );
        }
        assert!(first >= 1);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn a_wait_shorter_than_the_interval_times_out_without_a_token() {
        let far = next(Duration::ZERO, Duration::from_secs(60), 0, 0);
        assert_eq!(far, (1, Duration::from_secs(60)));
        assert!(far.1 > Duration::from_millis(1));
    }
}
