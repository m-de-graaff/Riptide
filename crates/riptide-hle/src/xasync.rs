//! Asynchronous calls (`XAsync`): how a title starts something that takes time and is told
//! when it is done.
//!
//! A title fills in a block with a task queue, a context and a completion routine, and
//! passes it to a call. The call's own code (its *provider*) is asked to begin, to do its
//! work on the queue's work port, to hand over its result, and to clean up. When the call
//! completes, the block's status and result size are written into the block and the
//! completion routine is run on the queue's completion port.
//!
//! These are methods of the task queue area. [`finish`] is for calls Riptide answers
//! itself: it completes a block at once with a result.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::taskqueue::{Queue, submit_callback};

const S_OK: i32 = 0;
const E_PENDING: i32 = 0x8000_000a_u32 as i32;
const E_ABORT: i32 = 0x8000_4004_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_NOT_SUFFICIENT_BUFFER: i32 = 0x8007_007a_u32 as i32;
const E_ILLEGAL_METHOD_CALL: i32 = 0x8000_000e_u32 as i32;

const WORK: u32 = 0;
const COMPLETION: u32 = 1;

/// What a title fills in, then four words that are the runtime's: the second is the
/// call's status and the third the size of its result.
#[repr(C)]
pub(crate) struct AsyncBlock {
    queue: *const Queue,
    context: *mut c_void,
    callback: Option<unsafe extern "system" fn(*mut AsyncBlock)>,
    internal: [AtomicUsize; 4],
}

/// What a provider is called with.
#[repr(C)]
pub(crate) struct ProviderData {
    block: *mut AsyncBlock,
    buffer_size: usize,
    buffer: *mut c_void,
    context: *mut c_void,
}

/// What a provider is asked to do.
const BEGIN: u32 = 0;
const DO_WORK: u32 = 1;
const GET_RESULT: u32 = 2;
const CANCEL: u32 = 3;
const CLEANUP: u32 = 4;

type Provider = unsafe extern "system" fn(u32, *const ProviderData) -> i32;

/// One call from when it begins until its result is taken or it fails.
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    /// The call's result once it has completed.
    done: Mutex<Option<i32>>,
    completed: Condvar,
}

impl Call {
    fn done(&self) -> MutexGuard<'_, Option<i32>> {
        self.done
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Asks the provider for one thing.
    ///
    /// # Safety
    /// `block` is the call's live block; `buffer` is null or `size` writable bytes.
    unsafe fn ask(
        &self,
        what: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> i32 {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        // SAFETY: the provider the call was begun with, called as the runtime calls it.
        unsafe { (self.provider)(what, &data) }
    }
}

/// Every call that has begun and whose result has not been taken, by its block's address.
static CALLS: Mutex<Option<HashMap<usize, Arc<Call>>>> = Mutex::new(None);

fn calls() -> MutexGuard<'static, Option<HashMap<usize, Arc<Call>>>> {
    CALLS.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn call_of(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    calls().as_ref()?.get(&(block as usize)).cloned()
}

fn forget(block: *mut AsyncBlock, call: &Arc<Call>) {
    if let Some(calls) = calls().as_mut()
        && calls
            .get(&(block as usize))
            .is_some_and(|current| Arc::ptr_eq(current, call))
    {
        calls.remove(&(block as usize));
    }
}

/// The block's status word and result-size word.
///
/// # Safety
/// `block` is a live block.
unsafe fn words<'a>(block: *mut AsyncBlock) -> (&'a AtomicUsize, &'a AtomicUsize) {
    // SAFETY: the words are atomics inside the live block.
    unsafe { (&(*block).internal[1], &(*block).internal[2]) }
}

/// # Safety
/// `block` is a live block.
unsafe fn status(block: *mut AsyncBlock) -> i32 {
    // SAFETY: as received.
    unsafe { words(block) }.0.load(Ordering::Acquire) as u32 as i32
}

/// +0x38 `XAsyncBegin`: `(block, context, identity, identity name, provider)`. The call
/// begins: its status is "pending" and its provider is asked to begin.
///
/// # Safety
/// `block` is null or a block that stays valid until the call's result is taken.
pub(crate) unsafe extern "system" fn begin(
    _this: *mut c_void,
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: Option<Provider>,
) -> i32 {
    let Some(provider) = provider else {
        return E_INVALIDARG;
    };
    if block.is_null() {
        return E_INVALIDARG;
    }
    // A block is used again once its call is over. One whose result nobody took is
    // cleaned up first.
    if let Some(left) = call_of(block) {
        if left.done().is_none() {
            return E_INVALIDARG;
        }
        // SAFETY: the call's own block.
        unsafe { left.ask(CLEANUP, block, 0, std::ptr::null_mut()) };
        forget(block, &left);
    }
    // SAFETY: a live block.
    let (status_word, size_word) = unsafe { words(block) };
    status_word.store(E_PENDING as u32 as usize, Ordering::Release);
    size_word.store(0, Ordering::Release);
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        done: Mutex::new(None),
        completed: Condvar::new(),
    });
    calls()
        .get_or_insert_with(HashMap::new)
        .insert(block as usize, call.clone());
    // SAFETY: the call's own block.
    let result = unsafe { call.ask(BEGIN, block, 0, std::ptr::null_mut()) };
    if result < 0 {
        // SAFETY: the call's own block.
        unsafe { complete_call(&call, block, result, 0) };
    }
    S_OK
}

/// What [`schedule`] puts on a work port.
struct Scheduled {
    call: Arc<Call>,
    block: usize,
}

/// # Safety
/// `context` is a boxed [`Scheduled`], taken once.
unsafe extern "system" fn do_work(context: *mut c_void, canceled: bool) {
    // SAFETY: made by `Box::into_raw` in `schedule`.
    let Scheduled { call, block } = *unsafe { Box::from_raw(context.cast::<Scheduled>()) };
    let block = block as *mut AsyncBlock;
    if call.done().is_some() {
        return;
    }
    let result = if canceled {
        E_ABORT
    } else {
        // SAFETY: the call's own block, live until the call is over.
        unsafe { call.ask(DO_WORK, block, 0, std::ptr::null_mut()) }
    };
    // SAFETY: as above.
    unsafe { complete_call(&call, block, result, 0) };
}

/// +0x48 `XAsyncSchedule`: `(block, delay in milliseconds)`. The provider is asked to do its
/// work on the block's queue. Work that answers "pending" goes on by itself and completes
/// later.
///
/// # Safety
/// `block` is null or the live block of a call that has begun.
pub(crate) unsafe extern "system" fn schedule(
    _this: *mut c_void,
    block: *mut AsyncBlock,
    delay: u32,
) -> i32 {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let Some(call) = call_of(block).filter(|call| call.done().is_none()) else {
        return E_INVALIDARG;
    };
    let context = Box::into_raw(Box::new(Scheduled {
        call,
        block: block as usize,
    }));
    // SAFETY: the block's queue is null or a live handle of the title's.
    let result = unsafe { submit_callback((*block).queue, WORK, delay, context.cast(), do_work) };
    if result < 0 {
        // SAFETY: the task was not taken, so the box is still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    result
}

/// # Safety
/// `context` is a live block.
unsafe extern "system" fn run_completion(context: *mut c_void, _canceled: bool) {
    let block = context.cast::<AsyncBlock>();
    // SAFETY: the block is live until its completion routine has run.
    if let Some(routine) = unsafe { (*block).callback } {
        // SAFETY: the title's routine, called once with its block.
        unsafe { routine(block) };
    }
}

/// Completes a call: writes its status and result size into the block, wakes whoever waits
/// for it, and runs the completion routine on the block's completion port.
///
/// # Safety
/// `block` is the call's live block.
unsafe fn complete_call(call: &Arc<Call>, block: *mut AsyncBlock, result: i32, required: usize) {
    if result == E_PENDING {
        return;
    }
    {
        let mut done = call.done();
        if done.is_some() {
            return;
        }
        *done = Some(result);
    }
    // A call that failed, or has no result to hand over, is over now.
    let over = result < 0 || required == 0;
    if over {
        // SAFETY: the call's own block.
        unsafe { call.ask(CLEANUP, block, 0, std::ptr::null_mut()) };
        forget(block, call);
    }
    // SAFETY: a live block.
    let (status_word, size_word) = unsafe { words(block) };
    size_word.store(required, Ordering::Release);
    status_word.store(result as u32 as usize, Ordering::Release);
    call.completed.notify_all();
    // SAFETY: a live block.
    let (queue, routine) = unsafe { ((*block).queue, (*block).callback) };
    if routine.is_some() {
        // SAFETY: the block's queue is null or a live handle of the title's.
        let submitted =
            unsafe { submit_callback(queue, COMPLETION, 0, block.cast(), run_completion) };
        if submitted < 0 {
            // A queue that takes nothing more must not swallow the completion.
            // SAFETY: the block is live.
            unsafe { run_completion(block.cast(), true) };
        }
    }
}

/// +0x50 `XAsyncComplete`: `(block, result, size of the result to hand over)`.
///
/// # Safety
/// `block` is null or a live block.
pub(crate) unsafe extern "system" fn complete(
    _this: *mut c_void,
    block: *mut AsyncBlock,
    result: i32,
    required: usize,
) {
    if let Some(call) = call_of(block) {
        // SAFETY: the call's own block.
        unsafe { complete_call(&call, block, result, required) };
    }
}

/// +0x18 `XAsyncGetStatus`: `(block, whether to wait)`.
///
/// # Safety
/// `block` is null or a live block.
pub(crate) unsafe extern "system" fn get_status(
    _this: *mut c_void,
    block: *mut AsyncBlock,
    wait: u8,
) -> i32 {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait != 0
        && let Some(call) = call_of(block)
    {
        let mut done = call.done();
        while done.is_none() {
            done = call
                .completed
                .wait(done)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
    // SAFETY: a live block.
    unsafe { status(block) }
}

/// +0x20 `XAsyncGetResultSize`: `(block, size out)`.
///
/// # Safety
/// `block` is null or a live block; `out` is null or where the size goes.
pub(crate) unsafe extern "system" fn get_result_size(
    _this: *mut c_void,
    block: *mut AsyncBlock,
    out: *mut usize,
) -> i32 {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a live block and a checked out-pointer.
    unsafe {
        let result = status(block);
        if result >= 0 {
            out.write(words(block).1.load(Ordering::Acquire));
        }
        result
    }
}

/// +0x28 `XAsyncCancel`.
///
/// # Safety
/// `block` is null or a live block.
pub(crate) unsafe extern "system" fn cancel(_this: *mut c_void, block: *mut AsyncBlock) {
    if let Some(call) = call_of(block).filter(|call| call.done().is_none()) {
        // SAFETY: the call's own block.
        unsafe {
            call.ask(CANCEL, block, 0, std::ptr::null_mut());
            complete_call(&call, block, E_ABORT, 0);
        }
    }
}

/// +0x58 `XAsyncGetResult`: `(block, identity, buffer size, buffer, bytes used out)`. The
/// provider hands its result over, and the call is over.
///
/// # Safety
/// `block` is null or a live block; `buffer` is null or `size` writable bytes; `used` is
/// null or where the count goes.
pub(crate) unsafe extern "system" fn get_result(
    _this: *mut c_void,
    block: *mut AsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> i32 {
    if block.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a live block.
    let (result, required) = unsafe { (status(block), words(block).1.load(Ordering::Acquire)) };
    if result < 0 || required == 0 {
        // SAFETY: the title's out-pointer; null is tolerated.
        let used = unsafe { used.as_mut() };
        if let (true, Some(used)) = (result >= 0, used) {
            *used = 0;
        }
        return result;
    }
    let Some(call) = call_of(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if size < required || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the call's own block and the title's buffer.
    let handed = unsafe { call.ask(GET_RESULT, block, size, buffer) };
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(used) = unsafe { used.as_mut() } {
        *used = required;
    }
    // SAFETY: the call's own block.
    unsafe { call.ask(CLEANUP, block, 0, std::ptr::null_mut()) };
    forget(block, &call);
    handed
}

type Work = unsafe extern "system" fn(*mut AsyncBlock) -> i32;

/// The provider of a call made by [`run`]: its work is the title's routine.
///
/// # Safety
/// Called by this module with valid data whose context is a [`Work`].
unsafe extern "system" fn run_provider(what: u32, data: *const ProviderData) -> i32 {
    // SAFETY: the data is valid for the call.
    let data = unsafe { &*data };
    match what {
        // SAFETY: the call's own block.
        BEGIN => unsafe { schedule(std::ptr::null_mut(), data.block, 0) },
        DO_WORK => {
            // SAFETY: the context is the routine `run` was given.
            let work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
            // SAFETY: the title's routine, with its block.
            unsafe { work(data.block) }
        }
        _ => S_OK,
    }
}

/// +0x30 `XAsyncRun`: `(block, work)`. Runs the title's routine on the block's work port as
/// a call of its own, whose result is what the routine returns.
///
/// # Safety
/// As [`begin`].
pub(crate) unsafe extern "system" fn run(
    this: *mut c_void,
    block: *mut AsyncBlock,
    work: Option<Work>,
) -> i32 {
    let Some(work) = work else {
        return E_INVALIDARG;
    };
    // SAFETY: as received; the identity is this function's own address.
    unsafe {
        begin(
            this,
            block,
            work as *mut c_void,
            run as *const c_void,
            std::ptr::null(),
            Some(run_provider),
        )
    }
}

/// The provider of a call [`finish`] completes: it hands over the bytes it was made with.
///
/// # Safety
/// Called by this module with valid data whose context is a boxed `Vec<u8>`.
unsafe extern "system" fn finished_provider(what: u32, data: *const ProviderData) -> i32 {
    // SAFETY: the data is valid for the call.
    let data = unsafe { &*data };
    match what {
        GET_RESULT => {
            // SAFETY: the context is the boxed result, alive until cleanup.
            let bytes = unsafe { &*data.context.cast::<Vec<u8>>() };
            if data.buffer.is_null() || data.buffer_size < bytes.len() {
                return E_NOT_SUFFICIENT_BUFFER;
            }
            // SAFETY: the buffer has room, as checked.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.buffer.cast(), bytes.len());
            }
            S_OK
        }
        CLEANUP => {
            // SAFETY: made by `Box::into_raw` in `finish`, cleaned up once.
            drop(unsafe { Box::from_raw(data.context.cast::<Vec<u8>>()) });
            S_OK
        }
        _ => S_OK,
    }
}

/// Completes `block` at once, for a call Riptide answers itself: with a failure, or with
/// success and `bytes` as the result the title then asks for. `identity` is what the
/// title's matching result call will name.
///
/// # Safety
/// `block` is null or a block that stays valid until the call's result is taken.
pub(crate) unsafe fn finish(
    block: *mut AsyncBlock,
    identity: usize,
    outcome: Result<Vec<u8>, i32>,
) -> i32 {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let (result, bytes) = match outcome {
        Ok(bytes) => (S_OK, bytes),
        Err(code) => (code, Vec::new()),
    };
    let required = bytes.len();
    let context = Box::into_raw(Box::new(bytes));
    // SAFETY: the block is live, and the provider takes the boxed bytes as its context.
    unsafe {
        let begun = begin(
            std::ptr::null_mut(),
            block,
            context.cast(),
            identity as *const c_void,
            std::ptr::null(),
            Some(finished_provider),
        );
        if begun < 0 {
            drop(Box::from_raw(context));
            return begun;
        }
        complete(std::ptr::null_mut(), block, result, required);
    }
    S_OK
}

/// The async methods of the task queue area, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (3, get_status as *const () as usize),
        (4, get_result_size as *const () as usize),
        (5, cancel as *const () as usize),
        (6, run as *const () as usize),
        (7, begin as *const () as usize),
        (9, schedule as *const () as usize),
        (10, complete as *const () as usize),
        (11, get_result as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn block(
        callback: Option<unsafe extern "system" fn(*mut AsyncBlock)>,
        context: *mut c_void,
    ) -> AsyncBlock {
        AsyncBlock {
            // The process queue, whose ports run on pool threads.
            queue: std::ptr::null(),
            context,
            callback,
            internal: [const { AtomicUsize::new(0) }; 4],
        }
    }

    unsafe extern "system" fn count(block: *mut AsyncBlock) {
        // SAFETY: every test passes the address of an `AtomicU32` as the context.
        unsafe { &*(*block).context.cast::<AtomicU32>() }.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "system" fn seven(_block: *mut AsyncBlock) -> i32 {
        7
    }

    #[test]
    fn work_runs_on_the_queue_and_its_result_is_the_calls() {
        let ran = AtomicU32::new(0);
        let mut block = block(Some(count), (&raw const ran).cast_mut().cast());
        let none = std::ptr::null_mut();
        // SAFETY: a live block, used for one call.
        unsafe {
            assert_eq!(run(none, &mut block, Some(seven)), S_OK);
            assert_eq!(get_status(none, &mut block, 1), 7);
            let mut size = 9;
            assert_eq!(get_result_size(none, &mut block, &mut size), 7);
            assert_eq!(size, 0);
            assert_eq!(run(none, &mut block, None), E_INVALIDARG);
        }
        // The completion routine runs on a pool thread, a moment after the status is set.
        for _ in 0..2000 {
            if ran.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_call_finished_at_once_hands_its_result_over_once() {
        let mut block = block(None, std::ptr::null_mut());
        let none = std::ptr::null_mut();
        let identity = 0x1234usize;
        let mut out = [0u8; 8];
        let mut used = 0;
        // SAFETY: a live block and a buffer of the size given.
        unsafe {
            assert_eq!(finish(&mut block, identity, Ok(vec![1, 2, 3, 4])), S_OK);
            assert_eq!(get_status(none, &mut block, 0), S_OK);
            let mut size = 0;
            assert_eq!(get_result_size(none, &mut block, &mut size), S_OK);
            assert_eq!(size, 4);
            // The wrong identity, then too little room, then the result.
            assert_eq!(
                get_result(
                    none,
                    &mut block,
                    9 as *const c_void,
                    8,
                    out.as_mut_ptr().cast(),
                    &mut used
                ),
                E_INVALIDARG
            );
            assert_eq!(
                get_result(
                    none,
                    &mut block,
                    identity as *const c_void,
                    2,
                    out.as_mut_ptr().cast(),
                    &mut used
                ),
                E_NOT_SUFFICIENT_BUFFER
            );
            assert_eq!(
                get_result(
                    none,
                    &mut block,
                    identity as *const c_void,
                    8,
                    out.as_mut_ptr().cast(),
                    &mut used
                ),
                S_OK
            );
            assert_eq!((used, &out[..4]), (4, &[1u8, 2, 3, 4][..]));
            // The call is over: its result cannot be taken again.
            assert_eq!(
                get_result(
                    none,
                    &mut block,
                    identity as *const c_void,
                    8,
                    out.as_mut_ptr().cast(),
                    &mut used
                ),
                E_ILLEGAL_METHOD_CALL
            );
        }
    }

    #[test]
    fn a_failed_call_has_its_failure_as_its_status_and_no_result() {
        let mut block = block(None, std::ptr::null_mut());
        let none = std::ptr::null_mut();
        let mut used = 5;
        // SAFETY: a live block.
        unsafe {
            assert_eq!(finish(&mut block, 16, Err(E_ABORT)), S_OK);
            assert_eq!(get_status(none, &mut block, 1), E_ABORT);
            assert_eq!(
                get_result(
                    none,
                    &mut block,
                    16 as *const c_void,
                    0,
                    std::ptr::null_mut(),
                    &mut used
                ),
                E_ABORT
            );
            // The block can be used for another call.
            assert_eq!(finish(&mut block, 16, Ok(Vec::new())), S_OK);
            assert_eq!(
                get_result(
                    none,
                    &mut block,
                    16 as *const c_void,
                    0,
                    std::ptr::null_mut(),
                    &mut used
                ),
                S_OK
            );
            assert_eq!(used, 0);
            assert_eq!(
                finish(std::ptr::null_mut(), 16, Ok(Vec::new())),
                E_INVALIDARG
            );
        }
    }
}
