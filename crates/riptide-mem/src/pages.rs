//! Which pages of the block are reserved and committed. Pure bookkeeping in byte offsets
//! from the block's start: nothing here touches memory, so it is tested on its own.

use std::collections::BTreeMap;
use std::ops::Range;

/// The size of a page. Commit and decommit work in whole pages.
pub const PAGE: u64 = 4096;

/// Reservations start on a multiple of this, as on the console and on Windows.
pub const GRANULARITY: u64 = 65_536;

/// Why a memory call failed. `riptide-hle` turns these into the console's error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemError {
    /// A size of zero, or a block size that is not a multiple of [`GRANULARITY`].
    InvalidParameter,
    /// The range is outside title memory, overlaps a reservation, crosses the end of one,
    /// or is not the start of one where the call needs that.
    InvalidAddress,
    /// No free range of that size is left.
    OutOfMemory,
    /// Windows refused, with this error code.
    Os(u32),
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemError::InvalidParameter => f.write_str("invalid parameter"),
            MemError::InvalidAddress => f.write_str("invalid address"),
            MemError::OutOfMemory => f.write_str("out of title memory"),
            MemError::Os(code) => write!(f, "Windows error {code:#x}"),
        }
    }
}

impl std::error::Error for MemError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Free,
    Reserved,
    Committed,
}

/// What a query reports: the run of pages around an address that share one state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub base: u64,
    /// The start of the reservation the pages belong to. `None` for free pages.
    pub allocation_base: Option<u64>,
    pub size: u64,
    pub state: State,
}

/// A run of pages that has just been committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub range: Range<u64>,
    /// The pages hold bytes from before a decommit and must be zeroed before use.
    pub stale: bool,
}

struct Bits(Vec<u64>);

impl Bits {
    fn new(len: u64) -> Self {
        Bits(vec![0; len.div_ceil(64) as usize])
    }

    fn get(&self, i: u64) -> bool {
        self.0[(i / 64) as usize] >> (i % 64) & 1 == 1
    }

    fn set(&mut self, i: u64, on: bool) {
        let word = &mut self.0[(i / 64) as usize];
        if on {
            *word |= 1 << (i % 64);
        } else {
            *word &= !(1 << (i % 64));
        }
    }
}

fn down(x: u64, to: u64) -> u64 {
    x / to * to
}

fn up(x: u64, to: u64) -> Option<u64> {
    x.checked_next_multiple_of(to)
}

pub(crate) struct PageMap {
    size: u64,
    /// Start of each reservation and its size.
    reservations: BTreeMap<u64, u64>,
    committed: Bits,
    stale: Bits,
}

impl PageMap {
    /// `size` must be a multiple of [`GRANULARITY`].
    pub fn new(size: u64) -> Self {
        assert!(size.is_multiple_of(GRANULARITY));
        PageMap {
            size,
            reservations: BTreeMap::new(),
            committed: Bits::new(size / PAGE),
            stale: Bits::new(size / PAGE),
        }
    }

    /// Reserves `size` bytes: at `at` when given, otherwise at the lowest free place.
    ///
    /// A given start is rounded down to [`GRANULARITY`] and the end up to a page, so the
    /// reservation covers every byte asked for.
    pub fn reserve(&mut self, at: Option<u64>, size: u64) -> Result<Range<u64>, MemError> {
        if size == 0 {
            return Err(MemError::InvalidParameter);
        }
        let range = match at {
            Some(at) => {
                let start = down(at, GRANULARITY);
                let end = at
                    .checked_add(size)
                    .and_then(|e| up(e, PAGE))
                    .ok_or(MemError::InvalidAddress)?;
                if end > self.size || self.overlaps(start..end) {
                    return Err(MemError::InvalidAddress);
                }
                start..end
            }
            None => {
                let len = up(size, PAGE).ok_or(MemError::OutOfMemory)?;
                let start = self.lowest_gap(len).ok_or(MemError::OutOfMemory)?;
                start..start + len
            }
        };
        self.reservations
            .insert(range.start, range.end - range.start);
        Ok(range)
    }

    fn overlaps(&self, range: Range<u64>) -> bool {
        self.reservations
            .range(..range.end)
            .next_back()
            .is_some_and(|(base, size)| base + size > range.start)
    }

    fn lowest_gap(&self, len: u64) -> Option<u64> {
        let mut candidate = 0;
        for (base, size) in &self.reservations {
            if base - candidate >= len {
                return Some(candidate);
            }
            candidate = up(base + size, GRANULARITY)?;
        }
        (candidate <= self.size && self.size - candidate >= len).then_some(candidate)
    }

    /// The reservation that holds all of `range`.
    fn containing(&self, range: &Range<u64>) -> Result<Range<u64>, MemError> {
        match self.reservations.range(..=range.start).next_back() {
            Some((base, size)) if range.end <= base + size => Ok(*base..base + size),
            _ => Err(MemError::InvalidAddress),
        }
    }

    /// The pages that cover `size` bytes at `at`.
    fn pages(at: u64, size: u64) -> Result<Range<u64>, MemError> {
        if size == 0 {
            return Err(MemError::InvalidParameter);
        }
        let end = at
            .checked_add(size)
            .and_then(|e| up(e, PAGE))
            .ok_or(MemError::InvalidAddress)?;
        Ok(down(at, PAGE)..end)
    }

    /// Commits the pages covering `size` bytes at `at`, which must lie in one reservation.
    /// Returns the page range, and the runs in it that were not committed before.
    pub fn commit(&mut self, at: u64, size: u64) -> Result<(Range<u64>, Vec<Run>), MemError> {
        let range = Self::pages(at, size)?;
        self.containing(&range)?;
        let mut runs: Vec<Run> = Vec::new();
        for page in range.start / PAGE..range.end / PAGE {
            if self.committed.get(page) {
                continue;
            }
            let stale = self.stale.get(page);
            self.committed.set(page, true);
            self.stale.set(page, false);
            let start = page * PAGE;
            match runs.last_mut() {
                Some(run) if run.range.end == start && run.stale == stale => run.range.end += PAGE,
                _ => runs.push(Run {
                    range: start..start + PAGE,
                    stale,
                }),
            }
        }
        Ok((range, runs))
    }

    /// Decommits the pages covering `size` bytes at `at`, which must lie in one reservation.
    /// A `size` of zero means the whole reservation that starts at `at`. Returns the runs
    /// that were committed.
    pub fn decommit(&mut self, at: u64, size: u64) -> Result<Vec<Range<u64>>, MemError> {
        let range = if size == 0 {
            let size = self.reservations.get(&at).ok_or(MemError::InvalidAddress)?;
            at..at + size
        } else {
            let range = Self::pages(at, size)?;
            self.containing(&range)?;
            range
        };
        Ok(self.clear(range))
    }

    /// Frees the reservation that starts at `at`. Returns its range and the runs in it that
    /// were committed.
    pub fn release(&mut self, at: u64) -> Result<(Range<u64>, Vec<Range<u64>>), MemError> {
        let size = self
            .reservations
            .remove(&at)
            .ok_or(MemError::InvalidAddress)?;
        Ok((at..at + size, self.clear(at..at + size)))
    }

    fn clear(&mut self, range: Range<u64>) -> Vec<Range<u64>> {
        let mut runs: Vec<Range<u64>> = Vec::new();
        for page in range.start / PAGE..range.end / PAGE {
            if !self.committed.get(page) {
                continue;
            }
            self.committed.set(page, false);
            self.stale.set(page, true);
            let start = page * PAGE;
            match runs.last_mut() {
                Some(run) if run.end == start => run.end += PAGE,
                _ => runs.push(start..start + PAGE),
            }
        }
        runs
    }

    /// The run of pages around `at` that share its state.
    pub fn query(&self, at: u64) -> Result<Region, MemError> {
        if at >= self.size {
            return Err(MemError::InvalidAddress);
        }
        let base = down(at, PAGE);
        let reservation = self
            .reservations
            .range(..=base)
            .next_back()
            .filter(|(start, size)| base < *start + *size)
            .map(|(start, size)| *start..start + size);
        let Some(reservation) = reservation else {
            let end = self
                .reservations
                .range(base..)
                .next()
                .map_or(self.size, |(start, _)| *start);
            return Ok(Region {
                base,
                allocation_base: None,
                size: end - base,
                state: State::Free,
            });
        };
        let committed = self.committed.get(base / PAGE);
        let mut end = base + PAGE;
        while end < reservation.end && self.committed.get(end / PAGE) == committed {
            end += PAGE;
        }
        Ok(Region {
            base,
            allocation_base: Some(reservation.start),
            size: end - base,
            state: if committed {
                State::Committed
            } else {
                State::Reserved
            },
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::single_range_in_vec_init,
    reason = "the tests compare lists of ranges"
)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn map() -> PageMap {
        PageMap::new(16 * MIB)
    }

    #[test]
    fn reservations_take_the_lowest_free_place_on_a_64k_boundary() {
        let mut m = map();
        assert_eq!(m.reserve(None, 1), Ok(0..PAGE));
        // The next one cannot share the first one's 64 KiB.
        assert_eq!(
            m.reserve(None, 3 * GRANULARITY),
            Ok(GRANULARITY..4 * GRANULARITY)
        );
        assert_eq!(
            m.reserve(None, PAGE + 1),
            Ok(4 * GRANULARITY..4 * GRANULARITY + 2 * PAGE)
        );
    }

    #[test]
    fn a_freed_range_is_reserved_again() {
        let mut m = map();
        let a = m.reserve(None, MIB).unwrap();
        let b = m.reserve(None, MIB).unwrap();
        m.release(a.start).unwrap();
        assert_eq!(m.reserve(None, 2 * MIB).unwrap().start, b.end);
        assert_eq!(m.reserve(None, MIB), Ok(a));
    }

    #[test]
    fn a_reservation_at_an_address_covers_every_byte_asked_for() {
        let mut m = map();
        // Starts mid-way through a 64 KiB unit and ends mid-page.
        assert_eq!(m.reserve(Some(MIB + 5000), 10), Ok(MIB..MIB + 2 * PAGE));
    }

    #[test]
    fn a_reservation_cannot_overlap_another_or_leave_the_block() {
        let mut m = map();
        m.reserve(Some(MIB), MIB).unwrap();
        assert_eq!(
            m.reserve(Some(MIB + GRANULARITY), PAGE),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(m.reserve(Some(0), MIB + 1), Err(MemError::InvalidAddress));
        assert_eq!(
            m.reserve(Some(15 * MIB), 2 * MIB),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(
            m.reserve(Some(u64::MAX - 10), 100),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(m.reserve(Some(0), MIB), Ok(0..MIB));
    }

    #[test]
    fn a_full_block_is_out_of_memory() {
        let mut m = map();
        m.reserve(None, 16 * MIB).unwrap();
        assert_eq!(m.reserve(None, 1), Err(MemError::OutOfMemory));
        assert_eq!(
            map().reserve(None, 16 * MIB + 1),
            Err(MemError::OutOfMemory)
        );
    }

    #[test]
    fn zero_sizes_are_refused() {
        let mut m = map();
        assert_eq!(m.reserve(None, 0), Err(MemError::InvalidParameter));
        m.reserve(Some(0), MIB).unwrap();
        assert_eq!(m.commit(0, 0), Err(MemError::InvalidParameter));
    }

    #[test]
    fn commit_rounds_to_pages_and_reports_only_new_pages() {
        let mut m = map();
        m.reserve(Some(0), MIB).unwrap();
        let (range, runs) = m.commit(PAGE + 1, PAGE).unwrap();
        assert_eq!(range, PAGE..3 * PAGE);
        assert_eq!(
            runs,
            vec![Run {
                range: PAGE..3 * PAGE,
                stale: false
            }]
        );
        // Committing over committed pages succeeds and reports only the rest.
        let (_, runs) = m.commit(0, 4 * PAGE).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    range: 0..PAGE,
                    stale: false
                },
                Run {
                    range: 3 * PAGE..4 * PAGE,
                    stale: false
                }
            ]
        );
        assert_eq!(m.commit(0, 4 * PAGE).unwrap().1, vec![]);
    }

    #[test]
    fn commit_needs_one_reservation_around_the_whole_range() {
        let mut m = map();
        assert_eq!(m.commit(0, PAGE), Err(MemError::InvalidAddress));
        m.reserve(Some(0), GRANULARITY).unwrap();
        m.reserve(Some(GRANULARITY), GRANULARITY).unwrap();
        assert_eq!(
            m.commit(GRANULARITY - PAGE, 2 * PAGE),
            Err(MemError::InvalidAddress)
        );
        assert!(m.commit(GRANULARITY - PAGE, PAGE).is_ok());
    }

    #[test]
    fn recommitted_pages_are_marked_stale_once() {
        let mut m = map();
        m.reserve(Some(0), MIB).unwrap();
        m.commit(0, 4 * PAGE).unwrap();
        assert_eq!(m.decommit(PAGE, 2 * PAGE), Ok(vec![PAGE..3 * PAGE]));
        // Pages 1 and 2 were used before; page 4 never was.
        let (_, runs) = m.commit(0, 5 * PAGE).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    range: PAGE..3 * PAGE,
                    stale: true
                },
                Run {
                    range: 4 * PAGE..5 * PAGE,
                    stale: false
                }
            ]
        );
        m.decommit(0, 5 * PAGE).unwrap();
        assert!(m.commit(0, 5 * PAGE).unwrap().1.iter().all(|run| run.stale));
    }

    #[test]
    fn decommit_skips_pages_that_were_not_committed() {
        let mut m = map();
        m.reserve(Some(0), MIB).unwrap();
        m.commit(0, PAGE).unwrap();
        m.commit(2 * PAGE, PAGE).unwrap();
        assert_eq!(
            m.decommit(0, 4 * PAGE),
            Ok(vec![0..PAGE, 2 * PAGE..3 * PAGE])
        );
        assert_eq!(m.decommit(0, 4 * PAGE), Ok(vec![]));
    }

    #[test]
    fn decommit_with_size_zero_takes_the_whole_reservation_from_its_start() {
        let mut m = map();
        m.reserve(Some(0), MIB).unwrap();
        m.commit(0, MIB).unwrap();
        assert_eq!(m.decommit(PAGE, 0), Err(MemError::InvalidAddress));
        assert_eq!(m.decommit(0, 0), Ok(vec![0..MIB]));
        // Still reserved.
        assert_eq!(m.query(0).unwrap().state, State::Reserved);
    }

    #[test]
    fn release_needs_the_start_of_a_reservation_and_frees_all_of_it() {
        let mut m = map();
        m.reserve(Some(0), MIB).unwrap();
        m.commit(PAGE, PAGE).unwrap();
        assert_eq!(m.release(PAGE), Err(MemError::InvalidAddress));
        assert_eq!(m.release(0), Ok((0..MIB, vec![PAGE..2 * PAGE])));
        assert_eq!(m.release(0), Err(MemError::InvalidAddress));
        assert_eq!(m.query(PAGE).unwrap().state, State::Free);
    }

    #[test]
    fn query_reports_the_run_of_pages_in_one_state() {
        let mut m = map();
        m.reserve(Some(MIB), MIB).unwrap();
        m.commit(MIB + PAGE, 2 * PAGE).unwrap();

        assert_eq!(
            m.query(5),
            Ok(Region {
                base: 0,
                allocation_base: None,
                size: MIB,
                state: State::Free
            })
        );
        assert_eq!(
            m.query(MIB),
            Ok(Region {
                base: MIB,
                allocation_base: Some(MIB),
                size: PAGE,
                state: State::Reserved
            })
        );
        assert_eq!(
            m.query(MIB + 2 * PAGE + 7),
            Ok(Region {
                base: MIB + 2 * PAGE,
                allocation_base: Some(MIB),
                size: PAGE,
                state: State::Committed
            })
        );
        assert_eq!(
            m.query(MIB + 3 * PAGE),
            Ok(Region {
                base: MIB + 3 * PAGE,
                allocation_base: Some(MIB),
                size: MIB - 3 * PAGE,
                state: State::Reserved
            })
        );
        assert_eq!(
            m.query(2 * MIB),
            Ok(Region {
                base: 2 * MIB,
                allocation_base: None,
                size: 14 * MIB,
                state: State::Free
            })
        );
        assert_eq!(m.query(16 * MIB), Err(MemError::InvalidAddress));
    }
}
