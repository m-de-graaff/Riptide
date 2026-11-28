//! A queue's submissions, kept in order when one of them has to wait.
//!
//! Almost always a title's submission goes to the host queue at once, on the title's
//! thread. The exception is a list the GPU is to run only when a value in memory has been
//! set (a *gate*): the host cannot wait inside a list, and the title's thread must not be
//! held, because it may be the one that sets the value. Such a list, and everything the
//! title submits to the same queue after it until it has run, is handed to a thread of the
//! queue's own, which makes the wait and then submits in the order the title did.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandList, ID3D12CommandQueue, ID3D12Fence, ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::core::Interface;

use crate::list::Wait;
use crate::trace;

/// One thing a title asked of a queue.
pub(crate) enum Job {
    /// Wait until a value in title memory lets the GPU go on.
    Gate(Wait),
    /// Submit host lists. Each is given back afterwards to record again, by its list type.
    Lists(Vec<(u32, ID3D12GraphicsCommandList)>),
    /// Submit host lists that stay their title list's own.
    Borrowed(Vec<Option<ID3D12CommandList>>),
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
}

// SAFETY: the host queue and everything a job holds are free-threaded host objects, and
// the state is behind a lock.
unsafe impl Send for Ordered {}
// SAFETY: as above.
unsafe impl Sync for Ordered {}

impl Ordered {
    pub(crate) fn new(host: ID3D12CommandQueue) -> Arc<Ordered> {
        Arc::new(Ordered {
            host,
            state: Mutex::new(State {
                jobs: VecDeque::new(),
                busy: false,
                started: false,
            }),
            arrived: Condvar::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether something submitted earlier has not run yet, so that what comes now has to
    /// wait its turn.
    pub(crate) fn behind(&self) -> bool {
        let state = self.state();
        state.busy || !state.jobs.is_empty()
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
        match job {
            Job::Gate(wait) => hold(&wait),
            Job::Lists(lists) => {
                let host: Vec<Option<ID3D12CommandList>> = lists
                    .iter()
                    .filter_map(|(_, list)| list.cast::<ID3D12CommandList>().ok())
                    .map(Some)
                    .collect();
                // SAFETY: a plain call on the host queue with closed host lists.
                unsafe { self.host.ExecuteCommandLists(&host) };
                for (kind, list) in lists {
                    crate::list::recycle(kind, list);
                }
            }
            Job::Borrowed(host) => {
                // SAFETY: a plain call on the host queue with closed host lists.
                unsafe { self.host.ExecuteCommandLists(&host) };
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
