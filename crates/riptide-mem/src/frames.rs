//! Which frames of which banks are free (decisions M2 and M6).
//!
//! A frame is 64 KiB of a bank. This module only counts: it knows nothing of Windows, so the
//! rules by which frames are handed out can be tested on their own.

use std::collections::BTreeMap;

/// The size of a frame: the unit the console's physical-page calls count in, and the unit
/// Windows maps views at.
pub const FRAME: u64 = 64 * 1024;
/// The size of a bank. A D3D12 heap opened on RAM must be smaller than 4 GiB on the target
/// machine (open question Q1).
pub const BANK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// How many frames a bank holds.
pub const BANK_FRAMES: u32 = (BANK_BYTES / FRAME) as u32;

/// Consecutive frames in one bank.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameRun {
    /// The first frame's number: its bank times [`BANK_FRAMES`] plus its place in the bank.
    pub first: u32,
    pub count: u32,
}

impl FrameRun {
    /// Which bank the run lies in.
    pub fn bank(&self) -> usize {
        (self.first / BANK_FRAMES) as usize
    }

    /// Where the run starts in its bank, in bytes.
    pub fn offset(&self) -> u64 {
        u64::from(self.first % BANK_FRAMES) * FRAME
    }

    /// The run's length in bytes.
    pub fn bytes(&self) -> u64 {
        u64::from(self.count) * FRAME
    }
}

/// The free frames of every bank.
#[derive(Default)]
pub(crate) struct Frames {
    /// For each bank, its free runs: where each starts in the bank, and how long it is.
    /// Neighbouring free runs are always joined.
    free: Vec<BTreeMap<u32, u32>>,
    in_use: u64,
}

impl Frames {
    pub(crate) fn new() -> Frames {
        Frames::default()
    }

    /// How many banks there are.
    pub(crate) fn banks(&self) -> usize {
        self.free.len()
    }

    /// How many frames are handed out right now.
    pub(crate) fn in_use(&self) -> u64 {
        self.in_use
    }

    /// Adds a bank, all of it free. Frame 0 of the first bank is never handed out: the
    /// console's calls use page number 0 to mean "no page".
    pub(crate) fn add_bank(&mut self) {
        let first_ever = self.free.is_empty();
        let mut runs = BTreeMap::new();
        if first_ever {
            runs.insert(1, BANK_FRAMES - 1);
        } else {
            runs.insert(0, BANK_FRAMES);
        }
        self.free.push(runs);
    }

    /// The smallest free run of at least `count` frames, as (bank, start).
    fn best_fit(&self, count: u32) -> Option<(usize, u32)> {
        self.free
            .iter()
            .enumerate()
            .flat_map(|(bank, runs)| {
                runs.iter()
                    .map(move |(&start, &length)| (bank, start, length))
            })
            .filter(|&(_, _, length)| length >= count)
            .min_by_key(|&(bank, start, length)| (length, bank, start))
            .map(|(bank, start, _)| (bank, start))
    }

    /// Takes `count` frames from the start of the free run at (`bank`, `start`).
    fn take(&mut self, bank: usize, start: u32, count: u32) -> FrameRun {
        let length = self.free[bank]
            .remove(&start)
            .expect("a free run starts there");
        if length > count {
            self.free[bank].insert(start + count, length - count);
        }
        self.in_use += u64::from(count);
        FrameRun {
            first: bank as u32 * BANK_FRAMES + start,
            count,
        }
    }

    /// Hands out `count` frames, in as few runs as can be: one run when any bank has the
    /// room, and for more than a bank's worth, whole banks first.
    ///
    /// When no free run is long enough, `more` is asked for another bank; it says whether it
    /// made one (then `add_bank` has been called). When it makes none, the frames come from
    /// the longest free runs there are. `None`: there are not `count` free frames.
    pub(crate) fn allocate(
        &mut self,
        count: u32,
        mut more: impl FnMut(&mut Frames) -> bool,
    ) -> Option<Vec<FrameRun>> {
        let mut runs = Vec::new();
        let mut left = count;
        while left > 0 {
            let want = left.min(BANK_FRAMES);
            if let Some((bank, start)) = self.best_fit(want) {
                runs.push(self.take(bank, start, want));
                left -= want;
                continue;
            }
            if more(self) {
                continue;
            }
            // No run is long enough and no bank can be added: take the longest there is.
            let longest = self
                .free
                .iter()
                .enumerate()
                .flat_map(|(bank, free)| {
                    free.iter()
                        .map(move |(&start, &length)| (length, bank, start))
                })
                .max();
            match longest {
                Some((length, bank, start)) => {
                    let taken = length.min(left);
                    runs.push(self.take(bank, start, taken));
                    left -= taken;
                }
                None => {
                    for run in runs {
                        self.free(run);
                    }
                    return None;
                }
            }
        }
        Some(runs)
    }

    /// Gives a run back, joining it with free neighbours.
    pub(crate) fn free(&mut self, run: FrameRun) {
        let bank = run.bank();
        let mut start = run.first % BANK_FRAMES;
        let mut length = run.count;
        let free = &mut self.free[bank];
        if let Some((&before, &before_length)) = free.range(..start).next_back()
            && before + before_length == start
        {
            free.remove(&before);
            start = before;
            length += before_length;
        }
        if let Some(after_length) = free.remove(&(start + length)) {
            length += after_length;
        }
        free.insert(start, length);
        self.in_use -= u64::from(run.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grow(frames: &mut Frames) -> bool {
        frames.add_bank();
        true
    }

    fn fixed(_: &mut Frames) -> bool {
        false
    }

    #[test]
    fn frames_come_in_one_run_and_never_include_frame_zero() {
        let mut frames = Frames::new();
        let runs = frames.allocate(100, grow).unwrap();
        assert_eq!(
            runs,
            [FrameRun {
                first: 1,
                count: 100
            }]
        );
        assert_eq!(frames.banks(), 1);
        assert_eq!(frames.in_use(), 100);
        assert_eq!(runs[0].offset(), FRAME);
        assert_eq!(runs[0].bytes(), 100 * FRAME);
    }

    #[test]
    fn freed_frames_are_joined_and_used_again() {
        let mut frames = Frames::new();
        let a = frames.allocate(10, grow).unwrap()[0];
        let b = frames.allocate(10, grow).unwrap()[0];
        let c = frames.allocate(10, grow).unwrap()[0];
        frames.free(a);
        frames.free(c);
        frames.free(b);
        assert_eq!(frames.in_use(), 0);
        // All thirty are one run again, so a request for them all fits where they were.
        let all = frames.allocate(BANK_FRAMES - 1, fixed).unwrap();
        assert_eq!(
            all,
            [FrameRun {
                first: 1,
                count: BANK_FRAMES - 1
            }]
        );
    }

    #[test]
    fn a_request_goes_to_the_smallest_run_that_holds_it() {
        let mut frames = Frames::new();
        let small = frames.allocate(4, grow).unwrap()[0];
        let _keep = frames.allocate(1, grow).unwrap();
        frames.free(small);
        // A hole of 4 at the start, and the long tail of the bank.
        let two = frames.allocate(2, fixed).unwrap()[0];
        assert_eq!(two.first, small.first);
    }

    #[test]
    fn a_new_bank_is_preferred_to_splitting_a_request() {
        let mut frames = Frames::new();
        let _most = frames.allocate(BANK_FRAMES - 10, grow).unwrap();
        let run = frames.allocate(100, grow).unwrap();
        assert_eq!(frames.banks(), 2);
        assert_eq!(
            run,
            [FrameRun {
                first: BANK_FRAMES,
                count: 100
            }]
        );
        assert_eq!(run[0].bank(), 1);
        assert_eq!(run[0].offset(), 0);
    }

    #[test]
    fn more_than_a_bank_is_whole_banks_then_the_rest() {
        let mut frames = Frames::new();
        let runs = frames.allocate(BANK_FRAMES + 5, grow).unwrap();
        assert_eq!(
            runs.iter().map(|run| run.count).sum::<u32>(),
            BANK_FRAMES + 5
        );
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().any(|run| run.count == BANK_FRAMES));
    }

    #[test]
    fn with_no_bank_to_add_the_longest_runs_are_used_and_then_it_fails_whole() {
        let mut frames = Frames::new();
        frames.add_bank();
        let a = frames.allocate(1000, fixed).unwrap()[0];
        let _b = frames
            .allocate(BANK_FRAMES - 1 - 1000 - 500, fixed)
            .unwrap();
        frames.free(a);
        // Free: 1000 at the start and 500 at the end. 1200 has to come from both.
        let split = frames.allocate(1200, fixed).unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].count, 1000);
        assert_eq!(split[1].count, 200);
        // 300 are left; 301 fails and takes nothing.
        let before = frames.in_use();
        assert_eq!(frames.allocate(301, fixed), None);
        assert_eq!(frames.in_use(), before);
        assert!(frames.allocate(300, fixed).is_some());
    }
}
