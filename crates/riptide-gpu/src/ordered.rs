//! A queue's submissions, kept in order when one of them has to wait.
//!
//! Almost always a title's submission goes to the host queue at once, on the title's
//! thread. The exception is a list the GPU is to run only when a value in memory has been
//! set (a *gate*): the host cannot wait inside a list, and the title's thread must not be
//! held, because it may be the one that sets the value. Such a list, and everything the
//! title submits to the same queue after it until it has run, is handed to a thread of the
//! queue's own, which makes the wait and then submits in the order the title did.
//!
//! A title may submit a list it has recorded once again and again. The host runs a list
//! again only when its run before is over, so each host list remembers when it last ran
//! and the queue waits for that before it submits the list again.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FENCE_FLAG_NONE, ID3D12CommandList, ID3D12CommandQueue, ID3D12Fence, ID3D12Resource,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

use crate::list::{Piece, Wait};
use crate::trace;

/// One thing a title asked of a queue.
pub(crate) enum Job {
    /// Wait until a value in title memory lets the GPU go on.
    Gate(Wait),
    /// Submit the host lists of these pieces, in order.
    Lists(Vec<Piece>),
    /// Have the host fence reach a value when everything before this has run.
    Signal(ID3D12Fence, u64),
    /// Show a frame.
    Present(ID3D12Resource),
}

struct State {
    jobs: VecDeque<Job>,
    /// Whether the queue's thread is in the middle of a job.
    busy: bool,
    started: bool,
}

/// A queue's order of submission.
pub(crate) struct Ordered {
    host: ID3D12CommandQueue,
    state: Mutex<State>,
    arrived: Condvar,
    /// A host fence of the queue's own, signalled after each submission of lists, and the
    /// event it wakes a wait with. `None` if the host gave none, in which case a list is
    /// never waited for.
    ran: Option<(ID3D12Fence, HANDLE)>,
    /// The number of the last submission of lists.
    submissions: AtomicU64,
}

// SAFETY: the host queue, the fence and everything a job holds are free-threaded host
// objects, the event is a process-wide handle, and the state is behind a lock.
unsafe impl Send for Ordered {}
// SAFETY: as above.
unsafe impl Sync for Ordered {}

impl Ordered {
    pub(crate) fn new(host: ID3D12CommandQueue) -> Arc<Ordered> {
        let ran = crate::host::host().ok().and_then(|device| {
            // SAFETY: plain calls: a new host fence and a new unnamed event.
            unsafe {
                let fence: ID3D12Fence =
                    device.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some((fence, event))
            }
        });
        Arc::new(Ordered {
            host,
            state: Mutex::new(State {
                jobs: VecDeque::new(),
                busy: false,
                started: false,
            }),
            arrived: Condvar::new(),
            ran,
            submissions: AtomicU64::new(0),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `jobs` now, on this thread, if nothing submitted earlier is still waiting and
    /// `now` allows; otherwise hands them to the queue's thread, behind what waits.
    /// Returns whether they ran now.
    pub(crate) fn submit(self: &Arc<Ordered>, jobs: Vec<Job>, now: bool) -> bool {
        let mut state = self.state();
        if now && !state.busy && state.jobs.is_empty() {
            // The lock is kept, so that nothing else can go in front.
            for job in jobs {
                self.run(job);
            }
            return true;
        }
        state.jobs.extend(jobs);
        if !state.started {
            state.started = true;
            let ordered = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("riptide queue".to_owned())
                .spawn(move || ordered.work());
            if let Err(error) = spawned {
                trace!("a queue's thread could not be started: {error}");
                state.started = false;
            }
        }
        drop(state);
        self.arrived.notify_one();
        false
    }

    /// The queue's thread: one job after another, for as long as the process lives.
    fn work(&self) {
        loop {
            let job = {
                let mut state = self.state();
                state.busy = false;
                loop {
                    if let Some(job) = state.jobs.pop_front() {
                        state.busy = true;
                        break job;
                    }
                    state = self
                        .arrived
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };
            self.run(job);
        }
    }

    fn run(&self, job: Job) {
        self.run_one(job);
        crate::host::report_problems();
    }

    /// Waits until the GPU has finished the submission numbered `number`.
    fn wait_for(&self, number: u64) {
        let Some((fence, event)) = &self.ran else {
            return;
        };
        // SAFETY: plain calls on the queue's own fence and event.
        unsafe {
            if fence.GetCompletedValue() < number
                && fence.SetEventOnCompletion(number, *event).is_ok()
            {
                WaitForSingleObject(*event, INFINITE);
            }
        }
    }

    fn run_one(&self, job: Job) {
        match job {
            Job::Gate(wait) => hold(&wait),
            Job::Lists(pieces) => {
                // A host list that has run before runs again only when that run is over.
                let before = pieces
                    .iter()
                    .map(|piece| piece.run.ran_at.load(Ordering::Acquire))
                    .max()
                    .unwrap_or(0);
                if before != 0 {
                    self.wait_for(before);
                }
                // What the lists read of title memory is what is mapped now.
                crate::pagetable::sync();
                crate::bindless::sync();
                // And what they read of its textures is what their memory holds now.
                let done = self
                    .ran
                    .as_ref()
                    // SAFETY: a plain call on the queue's own fence.
                    .map_or(u64::MAX, |(fence, _)| unsafe { fence.GetCompletedValue() });
                crate::upload::flush(
                    &self.host,
                    self.submissions.load(Ordering::Acquire) + 1,
                    done,
                );
                let host: Vec<Option<ID3D12CommandList>> = pieces
                    .iter()
                    .filter_map(|piece| piece.host.cast::<ID3D12CommandList>().ok())
                    .map(Some)
                    .collect();
                // SAFETY: a plain call on the host queue with closed host lists.
                unsafe { self.host.ExecuteCommandLists(&host) };
                let number = self.submissions.fetch_add(1, Ordering::AcqRel) + 1;
                if let Some((fence, _)) = &self.ran {
                    // SAFETY: a plain call on the host queue with its own fence.
                    let _ = unsafe { self.host.Signal(fence, number) };
                }
                for piece in pieces {
                    piece.run.ran_at.store(number, Ordering::Release);
                    piece.run.queued.fetch_sub(1, Ordering::AcqRel);
                    // A host list its title list has let go of can record again.
                    if Arc::strong_count(&piece.run) == 1 {
                        crate::list::recycle(piece.kind, piece.host);
                    }
                }
            }
            Job::Signal(fence, value) => {
                // SAFETY: a plain call on the host queue with a host fence.
                if let Err(error) = unsafe { self.host.Signal(&fence, value) } {
                    trace!("the host queue did not take a signal of {value}: {error}");
                }
            }
            Job::Present(frame) => {
                crate::present::show(&self.host, &frame);
            }
        }
    }
}

/// The value a wait looks at, read from title memory. `None` if nothing is mapped there.
fn value(wait: &Wait) -> Option<u64> {
    let bytes = if wait.wide { 8 } else { 4 };
    crate::services()?.memory.locate(wait.address, bytes)?;
    // SAFETY: that many bytes are mapped at the address, as just checked. The title or
    // the GPU may be writing them, so they are read as volatile.
    Some(unsafe {
        if wait.wide {
            (wait.address as *const u64).read_volatile()
        } else {
            u64::from((wait.address as *const u32).read_volatile())
        }
    })
}

/// Returns when the value a wait looks at lets the GPU go on. A wait on memory that is not
/// mapped cannot be made and is passed over, with a line in the log.
fn hold(wait: &Wait) {
    let begun = Instant::now();
    let mut told = false;
    loop {
        match value(wait) {
            None => {
                trace!(
                    "a GPU wait on {:#x} is passed over: nothing is mapped there",
                    wait.address
                );
                return;
            }
            Some(value) if wait.passes(value) => return,
            Some(_) => {}
        }
        if !told && begun.elapsed() > Duration::from_secs(5) {
            told = true;
            trace!(
                "a queue has waited 5 seconds for the value at {:#x} (comparison {}, \
                 reference {:#x}) and waits on",
                wait.address, wait.comparison, wait.reference
            );
        }
        std::thread::sleep(Duration::from_micros(50));
    }
}
