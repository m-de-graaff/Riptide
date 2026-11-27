//! The threads that run work the console's system would do for a title: runtime callbacks
//! and file requests.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, OnceLock};

use crate::trace::trace;

/// How many threads there are. The title's own threads do its heavy work.
const THREADS: usize = 4;

type Work = Box<dyn FnOnce() + Send>;

struct Pool {
    work: Mutex<VecDeque<Work>>,
    arrived: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool: &'static Pool = Box::leak(Box::new(Pool {
            work: Mutex::new(VecDeque::new()),
            arrived: Condvar::new(),
        }));
        for index in 0..THREADS {
            let started = std::thread::Builder::new()
                .name(format!("riptide pool {index}"))
                .spawn(move || {
                    loop {
                        let mut work = pool
                            .work
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        let next = loop {
                            match work.pop_front() {
                                Some(next) => break next,
                                None => {
                                    work = pool
                                        .arrived
                                        .wait(work)
                                        .unwrap_or_else(|poison| poison.into_inner());
                                }
                            }
                        };
                        drop(work);
                        next();
                    }
                });
            if started.is_err() {
                trace!("a pool thread could not be started");
            }
        }
        pool
    })
}

/// Runs `work` on a pool thread, after whatever was given earlier.
pub(crate) fn run(work: impl FnOnce() + Send + 'static) {
    let pool = pool();
    pool.work
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push_back(Box::new(work));
    pool.arrived.notify_one();
}
