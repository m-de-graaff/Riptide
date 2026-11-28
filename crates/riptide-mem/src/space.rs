//! The title's addresses: what is reserved, and which frames are mapped where (decisions M3
//! and M4).
//!
//! A reservation is cut into pieces. A piece is either empty address space or a view of
//! consecutive frames of one bank. The pieces are the page table: finding the frames behind
//! an address is finding its piece.
//!
//! A title can also cut a reservation into parts of its own, each of which it then fills,
//! empties and gives back by itself. A cut is a place where empty pieces are never joined,
//! so what Windows holds there stays what the title made.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::MemError;
use crate::frames::{FRAME, FrameRun};
use crate::os::{self, Limits, Section};

/// One piece of a reservation: its length, and the first frame mapped there if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    bytes: u64,
    frame: Option<u32>,
}

impl Piece {
    fn run(&self) -> Option<FrameRun> {
        self.frame.map(|first| FrameRun {
            first,
            count: (self.bytes / FRAME) as u32,
        })
    }
}

struct Reservation {
    bytes: u64,
    /// Every piece by where it starts. Together they cover the reservation exactly, and no
    /// two empty pieces are neighbours unless a cut lies between them.
    pieces: BTreeMap<u64, Piece>,
    /// Where the title cut the reservation into parts: addresses strictly inside it. A
    /// piece never reaches across one.
    cuts: BTreeSet<u64>,
}

/// Part of a bank that a range of title addresses lies in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
    /// The title address this part starts at.
    pub address: u64,
    pub bank: usize,
    /// Where it starts in the bank's buffer.
    pub offset: u64,
    pub bytes: u64,
}

#[derive(Default)]
pub(crate) struct Space {
    reservations: BTreeMap<u64, Reservation>,
}

impl Space {
    pub(crate) fn new() -> Space {
        Space::default()
    }

    /// Reserves `bytes` of addresses inside `limits` and returns where they start.
    pub(crate) fn reserve(&mut self, bytes: u64, limits: Limits) -> Result<u64, MemError> {
        if bytes == 0 || !bytes.is_multiple_of(FRAME) {
            return Err(MemError::InvalidParameter);
        }
        let start = os::reserve(bytes, limits)?;
        let pieces = BTreeMap::from([(start, Piece { bytes, frame: None })]);
        self.reservations.insert(
            start,
            Reservation {
                bytes,
                pieces,
                cuts: BTreeSet::new(),
            },
        );
        Ok(start)
    }

    /// The start of the reservation `range` lies wholly inside.
    fn holding(&self, range: &Range<u64>) -> Result<u64, MemError> {
        let (&start, reservation) = self
            .reservations
            .range(..=range.start)
            .next_back()
            .ok_or(MemError::InvalidAddress)?;
        if range.start >= range.end || range.end > start + reservation.bytes {
            return Err(MemError::InvalidAddress);
        }
        Ok(start)
    }

    /// Where the lowest reservation starts, if there is one.
    pub(crate) fn first_reservation(&self) -> Option<u64> {
        self.reservations.keys().next().copied()
    }

    /// The part that starts exactly at `address`: a whole reservation, or what lies between
    /// two of the title's cuts in one.
    pub(crate) fn part(&self, address: u64) -> Option<Range<u64>> {
        let (&start, reservation) = self.reservations.range(..=address).next_back()?;
        let end = start + reservation.bytes;
        if address >= end || (address != start && !reservation.cuts.contains(&address)) {
            return None;
        }
        let next = reservation.cuts.range(address + 1..).next().copied();
        Some(address..next.unwrap_or(end))
    }

    /// Whether `address` lies in a reservation.
    pub(crate) fn holds(&self, address: u64) -> bool {
        self.reservations
            .range(..=address)
            .next_back()
            .is_some_and(|(&start, reservation)| address < start + reservation.bytes)
    }

    /// The pieces that overlap `range`, as (start, piece).
    fn overlapping(reservation: &Reservation, range: &Range<u64>) -> Vec<(u64, Piece)> {
        let first = reservation
            .pieces
            .range(..=range.start)
            .next_back()
            .map_or(range.start, |(&start, _)| start);
        reservation
            .pieces
            .range(first..range.end)
            .filter(|&(&start, piece)| start + piece.bytes > range.start)
            .map(|(&start, &piece)| (start, piece))
            .collect()
    }

    /// Joins the empty piece at `at` with empty neighbours, in the bookkeeping and in
    /// Windows. Nothing is joined across a cut.
    fn join_around(reservation: &mut Reservation, at: u64) -> Result<(), MemError> {
        let mut start = at;
        let mut end = at + reservation.pieces[&at].bytes;
        let mut joined = 1;
        while let Some((&before, piece)) = reservation.pieces.range(..start).next_back()
            && piece.frame.is_none()
            && !reservation.cuts.contains(&start)
        {
            start = before;
            joined += 1;
        }
        while let Some(piece) = reservation.pieces.get(&end)
            && piece.frame.is_none()
            && !reservation.cuts.contains(&end)
        {
            end += piece.bytes;
            joined += 1;
        }
        if joined == 1 {
            return Ok(());
        }
        // SAFETY: `start..end` is covered exactly by neighbouring empty pieces, each a
        // placeholder of this reservation.
        unsafe { os::join(start..end)? };
        let gone: Vec<u64> = reservation
            .pieces
            .range(start..end)
            .map(|(&s, _)| s)
            .collect();
        for piece in gone {
            reservation.pieces.remove(&piece);
        }
        reservation.pieces.insert(
            start,
            Piece {
                bytes: end - start,
                frame: None,
            },
        );
        Ok(())
    }

    /// Takes away whatever is mapped in `range`, which must be whole frames inside one
    /// reservation. Returns the frames that were mapped there. A view that reaches outside
    /// `range` keeps its part outside.
    pub(crate) fn unmap(
        &mut self,
        range: Range<u64>,
        sections: &[Section],
    ) -> Result<Vec<FrameRun>, MemError> {
        if !range.start.is_multiple_of(FRAME) || !range.end.is_multiple_of(FRAME) {
            return Err(MemError::InvalidParameter);
        }
        let holder = self.holding(&range)?;
        let reservation = self.reservations.get_mut(&holder).expect("found above");
        let mut removed = Vec::new();
        for (start, piece) in Self::overlapping(reservation, &range) {
            let Some(run) = piece.run() else {
                continue;
            };
            let end = start + piece.bytes;
            // SAFETY: `start` is a view this module mapped; the caller takes it away.
            unsafe { os::unmap(start)? };
            reservation.pieces.insert(
                start,
                Piece {
                    bytes: piece.bytes,
                    frame: None,
                },
            );
            let cut = range.start.max(start)..range.end.min(end);
            let skipped = ((cut.start - start) / FRAME) as u32;
            removed.push(FrameRun {
                first: run.first + skipped,
                count: ((cut.end - cut.start) / FRAME) as u32,
            });
            // The parts of the view outside the range go back where they were.
            let kept = [
                (start, cut.start, run.first),
                (
                    cut.end,
                    end,
                    run.first + skipped + removed.last().expect("pushed").count,
                ),
            ];
            for (from, to, first) in kept {
                if from < to {
                    let part = FrameRun {
                        first,
                        count: ((to - from) / FRAME) as u32,
                    };
                    Self::place(reservation, from, part, sections, os::PAGE_READWRITE)?;
                }
            }
        }
        // What is empty now is one piece again wherever it touches.
        let empties: Vec<u64> = Self::overlapping(reservation, &range)
            .into_iter()
            .filter(|(_, piece)| piece.frame.is_none())
            .map(|(start, _)| start)
            .collect();
        for start in empties {
            if reservation.pieces.contains_key(&start) {
                Self::join_around(reservation, start)?;
            }
        }
        Ok(removed)
    }

    /// Maps `run` at `at`, which must lie in one empty piece.
    fn place(
        reservation: &mut Reservation,
        at: u64,
        run: FrameRun,
        sections: &[Section],
        protection: u32,
    ) -> Result<(), MemError> {
        let range = at..at + run.bytes();
        let (&start, &piece) = reservation
            .pieces
            .range(..=at)
            .next_back()
            .ok_or(MemError::InvalidAddress)?;
        let end = start + piece.bytes;
        if piece.frame.is_some() || range.end > end {
            return Err(MemError::InvalidAddress);
        }
        if (start, end) != (range.start, range.end) {
            // SAFETY: `range` lies inside the empty piece `start..end` and is smaller.
            unsafe { os::split(range.clone())? };
            reservation.pieces.remove(&start);
            for (from, to) in [(start, range.start), (range.end, end)] {
                if from < to {
                    reservation.pieces.insert(
                        from,
                        Piece {
                            bytes: to - from,
                            frame: None,
                        },
                    );
                }
            }
        }
        let section = sections.get(run.bank()).ok_or(MemError::InvalidParameter)?;
        // SAFETY: `range` is now exactly one placeholder of this reservation.
        let mapped = unsafe { section.map(at, run.offset(), run.bytes(), protection) };
        reservation.pieces.insert(
            at,
            Piece {
                bytes: run.bytes(),
                frame: mapped.is_ok().then_some(run.first),
            },
        );
        if mapped.is_err() {
            Self::join_around(reservation, at)?;
        }
        Ok(mapped?)
    }

    /// Maps `run` at `at`, in place of whatever is mapped there. Returns the frames that
    /// were mapped there before.
    pub(crate) fn map(
        &mut self,
        at: u64,
        run: FrameRun,
        sections: &[Section],
        protection: u32,
    ) -> Result<Vec<FrameRun>, MemError> {
        let range = at..at + run.bytes();
        let displaced = self.unmap(range.clone(), sections)?;
        let holder = self.holding(&range)?;
        let reservation = self.reservations.get_mut(&holder).expect("found above");
        // A view cannot reach across a cut, so the run is mapped part by part.
        let mut bounds = vec![range.start];
        bounds.extend(reservation.cuts.range(range.start + 1..range.end));
        bounds.push(range.end);
        for pair in bounds.windows(2) {
            let part = FrameRun {
                first: run.first + ((pair[0] - at) / FRAME) as u32,
                count: ((pair[1] - pair[0]) / FRAME) as u32,
            };
            Self::place(reservation, pair[0], part, sections, protection)?;
        }
        Ok(displaced)
    }

    /// Makes `range` a part of its own in its reservation: what is mapped there is taken
    /// away, and its ends become cuts. Returns the frames that were mapped there.
    pub(crate) fn cut(
        &mut self,
        range: Range<u64>,
        sections: &[Section],
    ) -> Result<Vec<FrameRun>, MemError> {
        let removed = self.unmap(range.clone(), sections)?;
        let holder = self.holding(&range)?;
        let reservation = self.reservations.get_mut(&holder).expect("found above");
        let whole = holder..holder + reservation.bytes;
        for at in [range.start, range.end] {
            if at == whole.start || at == whole.end || reservation.cuts.contains(&at) {
                continue;
            }
            let (&start, &piece) = reservation
                .pieces
                .range(..=at)
                .next_back()
                .ok_or(MemError::InvalidAddress)?;
            let end = start + piece.bytes;
            if start < at {
                // The range was emptied, so a piece that reaches across one of its ends is
                // empty too.
                if piece.frame.is_some() {
                    return Err(MemError::InvalidAddress);
                }
                // SAFETY: `at..end` lies inside the empty piece `start..end` and is smaller.
                unsafe { os::split(at..end)? };
                for (from, to) in [(start, at), (at, end)] {
                    reservation.pieces.insert(
                        from,
                        Piece {
                            bytes: to - from,
                            frame: None,
                        },
                    );
                }
            }
            reservation.cuts.insert(at);
        }
        Ok(removed)
    }

    /// Takes away the cuts strictly inside `range`, and joins the empty pieces they kept
    /// apart.
    pub(crate) fn uncut(&mut self, range: Range<u64>) -> Result<(), MemError> {
        let holder = self.holding(&range)?;
        let reservation = self.reservations.get_mut(&holder).expect("found above");
        let inside: Vec<u64> = reservation
            .cuts
            .range(range.start + 1..range.end)
            .copied()
            .collect();
        for at in &inside {
            reservation.cuts.remove(at);
        }
        for at in inside {
            if reservation
                .pieces
                .get(&at)
                .is_some_and(|piece| piece.frame.is_none())
            {
                Self::join_around(reservation, at)?;
            }
        }
        Ok(())
    }

    /// Gives the addresses of the part that starts at `start` back: a whole reservation, or
    /// one of the parts the title cut it into. Returns the frames that were mapped in it.
    /// What is left of the reservation on either side goes on as a reservation of its own.
    pub(crate) fn release(
        &mut self,
        start: u64,
        sections: &[Section],
    ) -> Result<Vec<FrameRun>, MemError> {
        let part = self.part(start).ok_or(MemError::InvalidAddress)?;
        let removed = self.unmap(part.clone(), sections)?;
        // SAFETY: everything in it was unmapped and joined above, and nothing joins across
        // its ends: one unused placeholder.
        unsafe { os::release(start)? };
        let holder = self.holding(&part)?;
        let reservation = self.reservations.remove(&holder).expect("found above");
        for (from, to) in [(holder, part.start), (part.end, holder + reservation.bytes)] {
            if from < to {
                self.reservations.insert(
                    from,
                    Reservation {
                        bytes: to - from,
                        pieces: reservation
                            .pieces
                            .range(from..to)
                            .map(|(&at, &piece)| (at, piece))
                            .collect(),
                        cuts: reservation.cuts.range(from + 1..to).copied().collect(),
                    },
                );
            }
        }
        Ok(removed)
    }

    /// The parts of banks that `bytes` at `address` lie in, in address order, or `None` if
    /// any of it is not mapped. Parts that follow on in the same bank are one part.
    pub(crate) fn locate(&self, address: u64, bytes: u64) -> Option<Vec<Located>> {
        let range = address..address.checked_add(bytes)?;
        let holder = self.holding(&range).ok()?;
        let reservation = &self.reservations[&holder];
        let mut parts: Vec<Located> = Vec::new();
        for (start, piece) in Self::overlapping(reservation, &range) {
            let run = piece.run()?;
            let from = range.start.max(start);
            let to = range.end.min(start + piece.bytes);
            let part = Located {
                address: from,
                bank: run.bank(),
                offset: run.offset() + (from - start),
                bytes: to - from,
            };
            match parts.last_mut() {
                Some(last) if last.bank == part.bank && last.offset + last.bytes == part.offset => {
                    last.bytes += part.bytes;
                }
                _ => parts.push(part),
            }
        }
        Some(parts)
    }

    /// What is mapped in `range`: each stretch that has frames one after another behind
    /// it, as its first address, its first frame and how many frames.
    pub(crate) fn mapped_in(&self, range: Range<u64>) -> Vec<(u64, u32, u32)> {
        let mut mapped = Vec::new();
        for (&start, reservation) in self.reservations.range(..range.end) {
            if start + reservation.bytes <= range.start {
                continue;
            }
            for (at, piece) in Self::overlapping(reservation, &range) {
                let Some(run) = piece.run() else {
                    continue;
                };
                let from = range.start.max(at);
                let to = range.end.min(at + piece.bytes);
                if from < to {
                    mapped.push((
                        from,
                        run.first + ((from - at) / FRAME) as u32,
                        ((to - from).div_ceil(FRAME)) as u32,
                    ));
                }
            }
        }
        mapped
    }

    /// Every address range at which any frame of `frames` is mapped, each with the first
    /// frame mapped there.
    pub(crate) fn views_of(&self, frames: FrameRun) -> Vec<(Range<u64>, u32)> {
        let wanted = frames.first..frames.first + frames.count;
        let mut views = Vec::new();
        for reservation in self.reservations.values() {
            for (&start, piece) in &reservation.pieces {
                let Some(run) = piece.run() else {
                    continue;
                };
                let from = wanted.start.max(run.first);
                let to = wanted.end.min(run.first + run.count);
                if from < to {
                    let at = start + u64::from(from - run.first) * FRAME;
                    views.push((at..at + u64::from(to - from) * FRAME, from));
                }
            }
        }
        views
    }

    /// Whether any frame of `frames` is mapped anywhere.
    pub(crate) fn is_mapped(&self, frames: FrameRun) -> bool {
        !self.views_of(frames).is_empty()
    }
}
