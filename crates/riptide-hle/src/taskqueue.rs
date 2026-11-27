//! The runtime's task queues (`XTaskQueue`): the first area of the runtime a title asks for,
//! and the one every other area's calls run through.
//!
//! A queue has two ports, one for work and one for completions. Each port runs the callbacks
//! submitted to it in one of four ways, chosen when the queue is made: when the title asks
//! (it calls dispatch), on pool threads, on pool threads one at a time, or at once on the
//! submitting thread. A null queue handle means the process's queue.
//!
//! Not built yet: asynchronous call blocks (`XAsync`), composite queues, and wait
//! registrations. Their methods name themselves when called.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::trace::trace;

/// The area's provider id, which is also the interface id a title asks for:
/// `073b7dcb-1fcf-4030-94be-e3c9eb623428`.
pub(crate) const INTERFACE: [u8; 16] = [
    0xcb, 0x7d, 0x3b, 0x07, 0xcf, 0x1f, 0x30, 0x40, 0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28,
];

pub(crate) const NAME: &str = "task queues";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_ABORT: i32 = 0x8000_4004_u32 as i32;
const INFINITE: u32 = u32::MAX;

/// A callback the title submits: its context, and whether the queue was ended before it ran.
type Callback = unsafe extern "system" fn(*mut c_void, bool);
/// Told that a callback was submitted to a port, so the title can wake what dispatches it.
type Monitor = unsafe extern "system" fn(*mut c_void, *const Queue, u32);
type Terminated = unsafe extern "system" fn(*mut c_void);

/// How a port runs what is submitted to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// When the title calls dispatch.
    Manual,
    /// On pool threads, several at once.
    Pool,
    /// On pool threads, one at a time and in order.
    Serialized,
    /// At once, on the thread that submits.
    Immediate,
}

impl Mode {
    fn from_raw(raw: u32) -> Option<Mode> {
        [Mode::Manual, Mode::Pool, Mode::Serialized, Mode::Immediate]
            .get(raw as usize)
            .copied()
    }
}

/// A submitted callback. Addresses are kept as numbers so it can cross threads.
#[derive(Clone, Copy)]
struct Task {
    context: usize,
    callback: usize,
}

impl Task {
    fn run(self, canceled: bool) {
        // SAFETY: the title submitted this callback with this context, to be called once.
        unsafe {
            let callback = std::mem::transmute::<usize, Callback>(self.callback);
            callback(self.context as *mut c_void, canceled);
        }
    }
}

struct Port {
    mode: Mode,
    waiting: Mutex<VecDeque<Task>>,
    arrived: Condvar,
    /// For a serialized port: whether a pool thread is draining it right now.
    draining: AtomicBool,
}

impl Port {
    fn new(mode: Mode) -> Port {
        Port {
            mode,
            waiting: Mutex::new(VecDeque::new()),
            arrived: Condvar::new(),
            draining: AtomicBool::new(false),
        }
    }

    fn waiting(&self) -> MutexGuard<'_, VecDeque<Task>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// A task queue. A handle is a counted reference to one.
pub(crate) struct Queue {
    ports: [Port; 2],
    ended: AtomicBool,
    monitors: Mutex<Vec<(u64, usize, usize)>>,
}

impl Queue {
    fn new(work: Mode, completion: Mode) -> Arc<Queue> {
        Arc::new(Queue {
            ports: [Port::new(work), Port::new(completion)],
            ended: AtomicBool::new(false),
            monitors: Mutex::new(Vec::new()),
        })
    }

    /// Runs `task` as the port's mode says, or queues it for the title to dispatch.
    fn submit(self: &Arc<Queue>, port: usize, task: Task) -> i32 {
        if self.ended.load(Ordering::Acquire) {
            return E_ABORT;
        }
        let at = &self.ports[port];
        match at.mode {
            Mode::Immediate => task.run(false),
            Mode::Pool => crate::pool::run(move || task.run(false)),
            Mode::Manual => {
                at.waiting().push_back(task);
                at.arrived.notify_one();
                self.tell_monitors(port);
            }
            Mode::Serialized => {
                at.waiting().push_back(task);
                self.drain_serialized(port);
            }
        }
        S_OK
    }

    /// Has one pool thread run a serialized port's callbacks in order until none are left.
    fn drain_serialized(self: &Arc<Queue>, port: usize) {
        if self.ports[port].draining.swap(true, Ordering::AcqRel) {
            return;
        }
        let queue = Arc::clone(self);
        crate::pool::run(move || {
            let at = &queue.ports[port];
            loop {
                let next = at.waiting().pop_front();
                match next {
                    Some(task) => task.run(queue.ended.load(Ordering::Acquire)),
                    None => {
                        at.draining.store(false, Ordering::Release);
                        // One may have arrived between the last look and the flag.
                        if at.waiting().is_empty() || at.draining.swap(true, Ordering::AcqRel) {
                            return;
                        }
                    }
                }
            }
        });
    }

    fn tell_monitors(&self, port: usize) {
        let monitors = self
            .monitors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        for (_, context, callback) in monitors {
            // SAFETY: the title registered this callback with this context.
            unsafe {
                let callback = std::mem::transmute::<usize, Monitor>(callback);
                callback(
                    context as *mut c_void,
                    std::ptr::from_ref(self),
                    port as u32,
                );
            }
        }
    }

    /// Runs one callback of a manual port, waiting up to `timeout` milliseconds for one.
    /// Returns whether one ran.
    fn dispatch(&self, port: usize, timeout: u32) -> bool {
        let at = &self.ports[port];
        if at.mode != Mode::Manual {
            return false;
        }
        let deadline = (timeout != INFINITE)
            .then(|| Instant::now() + Duration::from_millis(u64::from(timeout)));
        let mut waiting = at.waiting();
        let task = loop {
            if let Some(task) = waiting.pop_front() {
                break task;
            }
            if self.ended.load(Ordering::Acquire) {
                return false;
            }
            waiting = match deadline {
                None => at
                    .arrived
                    .wait(waiting)
                    .unwrap_or_else(|poison| poison.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    at.arrived
                        .wait_timeout(waiting, left)
                        .unwrap_or_else(|poison| poison.into_inner())
                        .0
                }
            };
        };
        drop(waiting);
        task.run(false);
        true
    }

    /// Ends the queue: nothing more is accepted, and what still waits is called as canceled.
    fn end(&self) {
        self.ended.store(true, Ordering::Release);
        for port in &self.ports {
            let left: Vec<Task> = port.waiting().drain(..).collect();
            for task in left {
                task.run(true);
            }
            port.arrived.notify_all();
        }
    }
}

/// The process's queue, which a null handle means: pool threads for both ports until the
/// title sets one of its own.
static PROCESS_QUEUE: Mutex<Option<Arc<Queue>>> = Mutex::new(None);

fn process_queue() -> Arc<Queue> {
    let mut queue = PROCESS_QUEUE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    Arc::clone(queue.get_or_insert_with(|| Queue::new(Mode::Pool, Mode::Pool)))
}

/// The queue behind a handle, without taking the handle's reference away.
///
/// # Safety
/// `handle` is null or a live handle this module gave out.
unsafe fn queue(handle: *const Queue) -> Arc<Queue> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a live handle is a counted reference; one more is taken for the caller.
    unsafe {
        Arc::increment_strong_count(handle);
        Arc::from_raw(handle)
    }
}

/// +0x60 `XTaskQueueCreate`: a queue whose ports run as the two modes say.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn create(
    _this: *mut c_void,
    work: u32,
    completion: u32,
    out: *mut *const Queue,
) -> i32 {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let handle = Arc::into_raw(Queue::new(work, completion));
    trace!("XTaskQueueCreate(work {work:?}, completion {completion:?}) = {handle:p}");
    // SAFETY: checked non-null; the title's own out-pointer.
    unsafe { out.write(handle) };
    S_OK
}

/// +0x78 `XTaskQueueDuplicateHandle`: another handle to the same queue.
///
/// # Safety
/// `handle` is a live handle; `out` is null or where the new one goes.
unsafe extern "system" fn duplicate(
    _this: *mut c_void,
    handle: *const Queue,
    out: *mut *const Queue,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: as this function's contract says.
    unsafe { out.write(Arc::into_raw(queue(handle))) };
    S_OK
}

/// +0x88 `XTaskQueueCloseHandle`.
///
/// # Safety
/// `handle` is null or a live handle, not used again after this.
unsafe extern "system" fn close(_this: *mut c_void, handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: the handle's own reference is given up.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

/// +0x80 `XTaskQueueDispatch`: runs one waiting callback of a port the title dispatches
/// itself. Returns whether one ran.
///
/// # Safety
/// `handle` is null or a live handle.
unsafe extern "system" fn dispatch(
    _this: *mut c_void,
    handle: *const Queue,
    port: u32,
    timeout: u32,
) -> bool {
    if port > 1 {
        return false;
    }
    // SAFETY: as this function's contract says.
    unsafe { queue(handle) }.dispatch(port as usize, timeout)
}

/// +0x90 `XTaskQueueSubmitCallback`.
///
/// # Safety
/// `handle` is null or a live handle; `callback` may be called with `context` once.
unsafe extern "system" fn submit(
    _this: *mut c_void,
    handle: *const Queue,
    port: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> i32 {
    let Some(callback) = callback.filter(|_| port <= 1) else {
        return E_INVALIDARG;
    };
    let task = Task {
        context: context as usize,
        callback: callback as usize,
    };
    // SAFETY: as this function's contract says.
    unsafe { queue(handle) }.submit(port as usize, task)
}

/// +0x98 `XTaskQueueSubmitDelayedCallback`: as submit, after `delay` milliseconds.
///
/// # Safety
/// As [`submit`].
unsafe extern "system" fn submit_delayed(
    _this: *mut c_void,
    handle: *const Queue,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> i32 {
    let Some(callback) = callback.filter(|_| port <= 1) else {
        return E_INVALIDARG;
    };
    let task = Task {
        context: context as usize,
        callback: callback as usize,
    };
    // SAFETY: as this function's contract says.
    let queue = unsafe { queue(handle) };
    if delay == 0 {
        return queue.submit(port as usize, task);
    }
    crate::pool::run(move || {
        std::thread::sleep(Duration::from_millis(u64::from(delay)));
        if queue.submit(port as usize, task) != S_OK {
            task.run(true);
        }
    });
    S_OK
}

/// +0xb0 `XTaskQueueTerminate`: ends the queue, then calls `callback`.
///
/// # Safety
/// `handle` is null or a live handle; `callback` may be called with `context` once.
unsafe extern "system" fn terminate(
    _this: *mut c_void,
    handle: *const Queue,
    _wait: u8,
    context: *mut c_void,
    callback: Option<Terminated>,
) -> i32 {
    // SAFETY: as this function's contract says.
    unsafe { queue(handle) }.end();
    if let Some(callback) = callback {
        // SAFETY: the title's callback, called once.
        unsafe { callback(context) };
    }
    S_OK
}

/// +0xb8 `XTaskQueueRegisterMonitor`: `callback` is told of each callback submitted to a
/// port the title dispatches itself.
///
/// # Safety
/// `handle` is null or a live handle; `token` is null or where the registration's token
/// goes.
unsafe extern "system" fn register_monitor(
    _this: *mut c_void,
    handle: *const Queue,
    context: *mut c_void,
    callback: Option<Monitor>,
    token: *mut u64,
) -> i32 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    // SAFETY: as this function's contract says.
    let queue = unsafe { queue(handle) };
    queue
        .monitors
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push((id, context as usize, callback as usize));
    if !token.is_null() {
        // SAFETY: checked non-null.
        unsafe { token.write(id) };
    }
    S_OK
}

/// +0xc0 `XTaskQueueUnregisterMonitor`.
///
/// # Safety
/// `handle` is null or a live handle.
unsafe extern "system" fn unregister_monitor(_this: *mut c_void, handle: *const Queue, token: u64) {
    // SAFETY: as this function's contract says.
    unsafe { queue(handle) }
        .monitors
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|&(id, _, _)| id != token);
}

/// +0xc8 `XTaskQueueGetCurrentProcessTaskQueue`: a new handle to the process's queue.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn get_process_queue(_this: *mut c_void, out: *mut *const Queue) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(process_queue())) };
    true
}

/// +0xd0 `XTaskQueueSetCurrentProcessTaskQueue`: null handles mean this queue from now on.
///
/// # Safety
/// `handle` is null or a live handle.
unsafe extern "system" fn set_process_queue(_this: *mut c_void, handle: *const Queue) {
    let new = (!handle.is_null()).then(|| {
        // SAFETY: a live handle; the process queue takes a reference of its own.
        unsafe {
            Arc::increment_strong_count(handle);
            Arc::from_raw(handle)
        }
    });
    trace!("XTaskQueueSetCurrentProcessTaskQueue({handle:p})");
    *PROCESS_QUEUE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = new;
}

/// +0xe8: the runtime's check that the caller is not a time-critical thread. Every runtime
/// call in a title makes it first. Nothing is checked here.
extern "system" fn check_thread(_this: *mut c_void) {}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (12, create as *const () as usize),
        (15, duplicate as *const () as usize),
        (16, dispatch as *const () as usize),
        (17, close as *const () as usize),
        (18, submit as *const () as usize),
        (19, submit_delayed as *const () as usize),
        (22, terminate as *const () as usize),
        (23, register_monitor as *const () as usize),
        (24, unregister_monitor as *const () as usize),
        (25, get_process_queue as *const () as usize),
        (26, set_process_queue as *const () as usize),
        (29, check_thread as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;

    unsafe extern "system" fn add(context: *mut c_void, canceled: bool) {
        // SAFETY: every test passes the address of an `AtomicU32` that outlives the call.
        let counter = unsafe { &*context.cast::<AtomicU32>() };
        counter.fetch_add(if canceled { 100 } else { 1 }, Ordering::SeqCst);
    }

    fn make(work: u32, completion: u32) -> *const Queue {
        let mut handle = std::ptr::null();
        // SAFETY: a valid out-pointer.
        let made = unsafe { create(std::ptr::null_mut(), work, completion, &mut handle) };
        assert_eq!(made, S_OK);
        handle
    }

    #[test]
    fn a_manual_port_runs_a_callback_only_when_dispatched() {
        let counter = AtomicU32::new(0);
        let context = (&raw const counter).cast_mut().cast::<c_void>();
        let handle = make(0, 0);
        // SAFETY: a live handle and a callback whose context outlives the queue.
        unsafe {
            assert_eq!(
                submit(std::ptr::null_mut(), handle, 0, context, Some(add)),
                S_OK
            );
            assert_eq!(counter.load(Ordering::SeqCst), 0);
            assert!(dispatch(std::ptr::null_mut(), handle, 0, 0));
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            // Nothing waits: dispatch gives up at once, and the other port is separate.
            assert!(!dispatch(std::ptr::null_mut(), handle, 0, 0));
            assert!(!dispatch(std::ptr::null_mut(), handle, 1, 10));
            close(std::ptr::null_mut(), handle);
        }
    }

    #[test]
    fn an_immediate_port_runs_the_callback_before_submit_returns() {
        let counter = AtomicU32::new(0);
        let context = (&raw const counter).cast_mut().cast::<c_void>();
        let handle = make(3, 3);
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                submit(std::ptr::null_mut(), handle, 1, context, Some(add)),
                S_OK
            );
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            close(std::ptr::null_mut(), handle);
        }
    }

    #[test]
    fn pool_and_serialized_ports_run_every_callback() {
        unsafe extern "system" fn send(context: *mut c_void, _canceled: bool) {
            // SAFETY: the test passes the address of a sender that outlives the calls.
            let sender = unsafe { &*context.cast::<mpsc::Sender<()>>() };
            let _ = sender.send(());
        }
        for mode in [1, 2] {
            let (sender, receiver) = mpsc::channel::<()>();
            let context = (&raw const sender).cast_mut().cast::<c_void>();
            let handle = make(mode, mode);
            // SAFETY: a live handle; the sender outlives the wait below.
            unsafe {
                for _ in 0..20 {
                    assert_eq!(
                        submit(std::ptr::null_mut(), handle, 0, context, Some(send)),
                        S_OK
                    );
                }
            }
            for _ in 0..20 {
                receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("every callback runs");
            }
            // SAFETY: the handle is not used again.
            unsafe { close(std::ptr::null_mut(), handle) };
        }
    }

    #[test]
    fn ending_a_queue_cancels_what_waits_and_refuses_more() {
        let counter = AtomicU32::new(0);
        let context = (&raw const counter).cast_mut().cast::<c_void>();
        let handle = make(0, 0);
        // SAFETY: as above.
        unsafe {
            submit(std::ptr::null_mut(), handle, 0, context, Some(add));
            assert_eq!(
                terminate(std::ptr::null_mut(), handle, 0, std::ptr::null_mut(), None),
                S_OK
            );
            assert_eq!(counter.load(Ordering::SeqCst), 100);
            assert_eq!(
                submit(std::ptr::null_mut(), handle, 0, context, Some(add)),
                E_ABORT
            );
            close(std::ptr::null_mut(), handle);
        }
    }

    #[test]
    fn a_monitor_hears_of_a_submission_until_it_is_taken_off() {
        static HEARD: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn heard(_context: *mut c_void, _queue: *const Queue, port: u32) {
            HEARD.fetch_add(10 + port, Ordering::SeqCst);
        }
        let counter = AtomicU32::new(0);
        let context = (&raw const counter).cast_mut().cast::<c_void>();
        let handle = make(0, 0);
        let mut token = 0u64;
        // SAFETY: as above.
        unsafe {
            register_monitor(
                std::ptr::null_mut(),
                handle,
                std::ptr::null_mut(),
                Some(heard),
                &mut token,
            );
            submit(std::ptr::null_mut(), handle, 1, context, Some(add));
            assert_eq!(HEARD.load(Ordering::SeqCst), 11);
            unregister_monitor(std::ptr::null_mut(), handle, token);
            submit(std::ptr::null_mut(), handle, 1, context, Some(add));
            assert_eq!(HEARD.load(Ordering::SeqCst), 11);
            close(std::ptr::null_mut(), handle);
        }
    }
}
