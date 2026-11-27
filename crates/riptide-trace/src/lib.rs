//! Frame timing and a per-stage cost ledger, so every other crate is measured from day one.
//! See README.md in this folder.
//!
//! Four parts:
//! - [`Ledger`] and [`Recorder`]: other crates record named stages; [`Ledger::end_frame`]
//!   turns what was recorded into a [`Frame`] with its [`Cause`].
//! - [`TraceWriter`] and [`read`]: the trace file, one line per frame.
//! - [`summarise`]: the figures `docs/measurements.md` asks for.
//! - [`Readout`]: the rolling figures of the on-screen statistics window.

mod readout;

pub use readout::{Figures, GRAPH_FRAMES, NOW_WINDOW, RECENT_WINDOW, Readout};

use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How many stages one ledger can hold. Stages are named in code, so this is a fixed budget.
pub const MAX_STAGES: usize = 64;

/// A frame at least this long counts as a slow frame in a [`Summary`].
pub const SLOW_FRAME: Duration = Duration::from_millis(100);

const CAUSES: usize = 5;
const HEADER: &str = "riptide-trace\t1";

/// What a frame's time went to (decision D11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cause {
    /// Waiting for Windows to read pages back from disk (hard page faults).
    Paging,
    /// Waiting for the driver's shader compiler.
    ShaderCompile,
    /// Riptide's own CPU work.
    RiptideCpu,
    /// The title's CPU work.
    TitleCpu,
    /// The GPU.
    Gpu,
}

impl Cause {
    /// Every cause, in the order used for arrays indexed by cause and for breaking ties.
    pub const ALL: [Cause; CAUSES] = [
        Cause::Paging,
        Cause::ShaderCompile,
        Cause::RiptideCpu,
        Cause::TitleCpu,
        Cause::Gpu,
    ];

    /// The name used in the trace file.
    pub fn name(self) -> &'static str {
        match self {
            Cause::Paging => "paging",
            Cause::ShaderCompile => "shader_compile",
            Cause::RiptideCpu => "riptide_cpu",
            Cause::TitleCpu => "title_cpu",
            Cause::Gpu => "gpu",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Cause::ALL.into_iter().find(|c| c.name() == name)
    }

    /// This cause's position in [`Cause::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// A stage registered with [`Ledger::stage`]. Cheap to copy and keep in a static or a struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage(u8);

/// One thread's counters. Written by its [`Recorder`], read and reset by the ledger.
struct Slots {
    nanos: [AtomicU64; MAX_STAGES],
    counts: [AtomicU64; MAX_STAGES],
}

/// One thread's handle for recording. Get one per thread from [`Ledger::recorder`].
///
/// Recording touches only this thread's counters, so threads never contend.
pub struct Recorder {
    slots: Arc<Slots>,
    /// Time spent in spans that closed inside the span now open on this thread.
    children: Cell<u64>,
}

impl Recorder {
    /// Adds one occurrence of `stage` that took `elapsed`. For time measured elsewhere, such
    /// as GPU time from timestamp queries.
    pub fn record(&self, stage: Stage, elapsed: Duration) {
        self.add(stage, nanos(elapsed), 1);
    }

    /// Adds `n` occurrences of `stage` without any time.
    pub fn count(&self, stage: Stage, n: u64) {
        self.add(stage, 0, n);
    }

    /// Times `stage` until the returned span is dropped.
    ///
    /// Spans nest. A span records only its own time: time in spans opened inside it goes to
    /// those stages. So a Riptide stage inside a title stage is not counted twice.
    pub fn span(&self, stage: Stage) -> Span<'_> {
        Span {
            recorder: self,
            stage,
            outer: self.begin(),
            start: Instant::now(),
        }
    }

    fn add(&self, stage: Stage, nanos: u64, count: u64) {
        let i = stage.0 as usize;
        self.slots.nanos[i].fetch_add(nanos, Relaxed);
        self.slots.counts[i].fetch_add(count, Relaxed);
    }

    /// Opens a span: returns the enclosing span's child time, to hand back to `finish`.
    fn begin(&self) -> u64 {
        self.children.replace(0)
    }

    /// Closes a span that ran for `elapsed` nanoseconds in total.
    fn finish(&self, stage: Stage, elapsed: u64, outer: u64) {
        self.add(stage, elapsed.saturating_sub(self.children.get()), 1);
        self.children.set(outer.saturating_add(elapsed));
    }
}

/// A stage being timed. See [`Recorder::span`].
pub struct Span<'a> {
    recorder: &'a Recorder,
    stage: Stage,
    outer: u64,
    start: Instant,
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        self.recorder
            .finish(self.stage, nanos(self.start.elapsed()), self.outer);
    }
}

/// Figures the ledger cannot measure itself, given at the end of each frame. RAM accounting
/// lives in `riptide-mem` and video-memory accounting in `riptide-gpu` (decision D10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub private_bytes: u64,
    pub video_bytes: u64,
}

/// One frame's figures. This is what a trace file holds, one line per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub index: u64,
    /// Wall-clock time from the previous frame's end to this one's.
    pub duration: Duration,
    /// How long the frame was held up by each cause, indexed by [`Cause::index`]. For each
    /// cause this is the busiest single thread's time, capped at `duration`: seven title
    /// threads working side by side for 20 ms hold the frame up for 20 ms, not 140.
    pub by_cause: [Duration; CAUSES],
    /// The cause with the largest entry in `by_cause`. `None` when nothing was recorded.
    pub cause: Option<Cause>,
    /// Riptide's CPU time summed over all threads. This is what the one-core budget and the
    /// 10,000-draw bar are measured against.
    pub riptide_cpu: Duration,
    pub private_bytes: u64,
    pub video_bytes: u64,
}

impl Frame {
    pub fn time(&self, cause: Cause) -> Duration {
        self.by_cause[cause.index()]
    }

    fn to_line(self) -> String {
        let mut line = format!(
            "{}\t{}\t{}",
            self.index,
            nanos(self.duration),
            self.cause.map_or("-", Cause::name)
        );
        for t in self.by_cause {
            line.push_str(&format!("\t{}", nanos(t)));
        }
        line.push_str(&format!(
            "\t{}\t{}\t{}",
            nanos(self.riptide_cpu),
            self.private_bytes,
            self.video_bytes
        ));
        line
    }

    fn parse(line: &str) -> Option<Frame> {
        let mut fields = line.split('\t');
        let mut number = || fields.next()?.parse::<u64>().ok();
        let index = number()?;
        let duration = Duration::from_nanos(number()?);
        let cause = match fields.next()? {
            "-" => None,
            name => Some(Cause::from_name(name)?),
        };
        let mut number = || fields.next()?.parse::<u64>().ok();
        let mut by_cause = [Duration::ZERO; CAUSES];
        for t in &mut by_cause {
            *t = Duration::from_nanos(number()?);
        }
        let frame = Frame {
            index,
            duration,
            by_cause,
            cause,
            riptide_cpu: Duration::from_nanos(number()?),
            private_bytes: number()?,
            video_bytes: number()?,
        };
        fields.next().is_none().then_some(frame)
    }
}

/// What one stage cost in one frame, summed over all threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageCost {
    pub name: &'static str,
    pub cause: Cause,
    pub time: Duration,
    pub count: u64,
}

/// A finished frame: its figures, and the stages that recorded anything, in the order they
/// were registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameReport {
    pub frame: Frame,
    pub stages: Vec<StageCost>,
}

struct FrameClock {
    next_index: u64,
    started: Instant,
}

/// The ledger every crate records into. One per session; share it by reference or `Arc`.
pub struct Ledger {
    stages: Mutex<Vec<(&'static str, Cause)>>,
    recorders: Mutex<Vec<Arc<Slots>>>,
    clock: Mutex<FrameClock>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Ledger {
            stages: Mutex::new(Vec::new()),
            recorders: Mutex::new(Vec::new()),
            clock: Mutex::new(FrameClock {
                next_index: 0,
                started: Instant::now(),
            }),
        }
    }

    /// Registers a stage, or returns the one already registered under `name`.
    ///
    /// # Panics
    /// If `name` is already registered with another cause, or [`MAX_STAGES`] is exceeded.
    /// Both are mistakes in the calling code, not conditions of a session.
    pub fn stage(&self, name: &'static str, cause: Cause) -> Stage {
        let mut stages = lock(&self.stages);
        if let Some(i) = stages.iter().position(|(n, _)| *n == name) {
            assert!(
                stages[i].1 == cause,
                "stage {name:?} is already registered as {:?}",
                stages[i].1
            );
            return Stage(i as u8);
        }
        assert!(stages.len() < MAX_STAGES, "more than {MAX_STAGES} stages");
        stages.push((name, cause));
        Stage((stages.len() - 1) as u8)
    }

    /// A recorder for the calling thread. What it records is kept even if it is dropped
    /// before the frame ends.
    pub fn recorder(&self) -> Recorder {
        let slots = Arc::new(Slots {
            nanos: [const { AtomicU64::new(0) }; MAX_STAGES],
            counts: [const { AtomicU64::new(0) }; MAX_STAGES],
        });
        lock(&self.recorders).push(slots.clone());
        Recorder {
            slots,
            children: Cell::new(0),
        }
    }

    /// Ends the frame: its duration is the time since the previous call (or since the ledger
    /// was made). Call it once per presented frame, from one thread.
    ///
    /// A span still open on another thread is counted in the frame in which it closes.
    pub fn end_frame(&self, sample: Sample) -> FrameReport {
        let duration = {
            let mut clock = lock(&self.clock);
            let now = Instant::now();
            let duration = now - clock.started;
            clock.started = now;
            duration
        };
        self.end_frame_with(duration, sample)
    }

    /// Ends the frame with a duration measured by the caller.
    pub fn end_frame_with(&self, duration: Duration, sample: Sample) -> FrameReport {
        let stages = lock(&self.stages).clone();
        let mut totals = vec![(0u64, 0u64); stages.len()];
        let mut by_cause = [0u64; CAUSES];
        {
            let mut recorders = lock(&self.recorders);
            for slots in recorders.iter() {
                let mut thread = [0u64; CAUSES];
                for (i, (_, cause)) in stages.iter().enumerate() {
                    let time = slots.nanos[i].swap(0, Relaxed);
                    totals[i].0 += time;
                    totals[i].1 += slots.counts[i].swap(0, Relaxed);
                    thread[cause.index()] += time;
                }
                for (held, t) in by_cause.iter_mut().zip(thread) {
                    *held = (*held).max(t);
                }
            }
            // A recorder whose thread has gone has just had its last figures read.
            recorders.retain(|slots| Arc::strong_count(slots) > 1);
        }

        let riptide_cpu: u64 = stages
            .iter()
            .zip(&totals)
            .filter(|((_, cause), _)| *cause == Cause::RiptideCpu)
            .map(|(_, (time, _))| *time)
            .sum();
        let by_cause = by_cause.map(|t| Duration::from_nanos(t).min(duration));
        let index = {
            let mut clock = lock(&self.clock);
            clock.next_index += 1;
            clock.next_index - 1
        };
        FrameReport {
            frame: Frame {
                index,
                duration,
                by_cause,
                cause: attribute(&by_cause),
                riptide_cpu: Duration::from_nanos(riptide_cpu),
                private_bytes: sample.private_bytes,
                video_bytes: sample.video_bytes,
            },
            stages: stages
                .iter()
                .zip(&totals)
                .filter(|(_, (time, count))| *time > 0 || *count > 0)
                .map(|((name, cause), (time, count))| StageCost {
                    name,
                    cause: *cause,
                    time: Duration::from_nanos(*time),
                    count: *count,
                })
                .collect(),
        }
    }
}

/// The cause that held the frame up longest. A tie goes to the earlier cause in
/// [`Cause::ALL`]; `None` when no cause has any time.
pub fn attribute(by_cause: &[Duration; CAUSES]) -> Option<Cause> {
    let mut best: Option<Cause> = None;
    for cause in Cause::ALL {
        let t = by_cause[cause.index()];
        if t > best.map_or(Duration::ZERO, |b| by_cause[b.index()]) {
            best = Some(cause);
        }
    }
    best
}

/// Writes a trace file: a header line, then one line per frame.
pub struct TraceWriter {
    out: BufWriter<File>,
}

impl TraceWriter {
    /// Creates `path`, replacing a file already there.
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        writeln!(out, "{HEADER}")?;
        Ok(TraceWriter { out })
    }

    pub fn write(&mut self, frame: &Frame) -> io::Result<()> {
        writeln!(self.out, "{}", frame.to_line())
    }

    /// Writes buffered lines to the file. Dropping the writer does this too, but cannot
    /// report a failure.
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Reads a trace file. A line that does not parse is skipped, so a file cut short by a crash
/// still gives every whole frame. A file without the header is an error.
pub fn read(path: &Path) -> io::Result<Vec<Frame>> {
    let mut lines = BufReader::new(File::open(path)?).lines();
    match lines.next().transpose()? {
        Some(header) if header == HEADER => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a riptide-trace file",
            ));
        }
    }
    let mut frames = Vec::new();
    for line in lines {
        if let Some(frame) = Frame::parse(&line?) {
            frames.push(frame);
        }
    }
    Ok(frames)
}

/// A session in the figures `docs/measurements.md` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub frames: usize,
    /// The sum of all frame durations.
    pub session: Duration,
    pub median_frame: Duration,
    /// The mean duration of the slowest 1% of frames (at least one frame).
    pub one_percent_low: Duration,
    pub median_riptide_cpu: Duration,
    pub peak_private_bytes: u64,
    pub peak_video_bytes: u64,
    /// Frames of [`SLOW_FRAME`] or longer.
    pub slow_frames: usize,
    /// How many frames each cause was blamed for, indexed by [`Cause::index`].
    pub frames_by_cause: [usize; CAUSES],
    /// Frames with no recorded cause.
    pub unattributed: usize,
}

/// Summarises a session. `None` for no frames.
pub fn summarise(frames: &[Frame]) -> Option<Summary> {
    if frames.is_empty() {
        return None;
    }
    let mut durations: Vec<Duration> = frames.iter().map(|f| f.duration).collect();
    durations.sort_unstable();
    let mut riptide: Vec<Duration> = frames.iter().map(|f| f.riptide_cpu).collect();
    riptide.sort_unstable();
    let slowest = frames.len().div_ceil(100);
    let mut frames_by_cause = [0; CAUSES];
    for cause in frames.iter().filter_map(|f| f.cause) {
        frames_by_cause[cause.index()] += 1;
    }
    Some(Summary {
        frames: frames.len(),
        session: durations.iter().sum(),
        median_frame: durations[durations.len() / 2],
        one_percent_low: durations[durations.len() - slowest..]
            .iter()
            .sum::<Duration>()
            / slowest as u32,
        median_riptide_cpu: riptide[riptide.len() / 2],
        peak_private_bytes: frames.iter().map(|f| f.private_bytes).max().unwrap_or(0),
        peak_video_bytes: frames.iter().map(|f| f.video_bytes).max().unwrap_or(0),
        slow_frames: frames.iter().filter(|f| f.duration >= SLOW_FRAME).count(),
        frames_by_cause,
        unattributed: frames.iter().filter(|f| f.cause.is_none()).count(),
    })
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let fps = |d: Duration| {
            if d.is_zero() {
                0.0
            } else {
                1.0 / d.as_secs_f64()
            }
        };
        let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
        writeln!(
            f,
            "frames: {} in {:.1} s",
            self.frames,
            self.session.as_secs_f64()
        )?;
        writeln!(
            f,
            "median frame time: {:.2} ms ({:.1} fps)",
            ms(self.median_frame),
            fps(self.median_frame)
        )?;
        writeln!(
            f,
            "1% low: {:.2} ms ({:.1} fps)",
            ms(self.one_percent_low),
            fps(self.one_percent_low)
        )?;
        writeln!(
            f,
            "Riptide CPU time per frame (median): {:.2} ms",
            ms(self.median_riptide_cpu)
        )?;
        writeln!(
            f,
            "private memory (peak): {:.2} GiB",
            gib(self.peak_private_bytes)
        )?;
        writeln!(
            f,
            "video memory (peak): {:.2} GiB",
            gib(self.peak_video_bytes)
        )?;
        writeln!(
            f,
            "frames over {} ms: {}",
            SLOW_FRAME.as_millis(),
            self.slow_frames
        )?;
        write!(f, "frames by cause:")?;
        for cause in Cause::ALL {
            write!(
                f,
                " {} {}",
                cause.name(),
                self.frames_by_cause[cause.index()]
            )?;
        }
        write!(f, " none {}", self.unattributed)
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Locks a mutex whose data stays valid even if a thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    struct Stages {
        paging: Stage,
        shader: Stage,
        translate: Stage,
        title: Stage,
        gpu: Stage,
    }

    fn ledger() -> (Ledger, Stages) {
        let l = Ledger::new();
        let s = Stages {
            paging: l.stage("page-in", Cause::Paging),
            shader: l.stage("pipeline compile", Cause::ShaderCompile),
            translate: l.stage("translate draw", Cause::RiptideCpu),
            title: l.stage("title code", Cause::TitleCpu),
            gpu: l.stage("gpu frame", Cause::Gpu),
        };
        (l, s)
    }

    #[test]
    fn synthetic_frame_with_known_costs_is_attributed_to_each_cause() {
        // Every frame has the same background costs; one cause is made the largest in turn.
        for cause in Cause::ALL {
            let (l, s) = ledger();
            let r = l.recorder();
            r.record(s.paging, ms(1));
            r.record(s.shader, ms(2));
            r.record(s.translate, ms(3));
            r.record(s.title, ms(4));
            r.record(s.gpu, ms(5));
            let stage = [s.paging, s.shader, s.translate, s.title, s.gpu][cause.index()];
            r.record(stage, ms(20));

            let frame = l.end_frame_with(ms(33), Sample::default()).frame;
            assert_eq!(frame.cause, Some(cause));
            assert_eq!(frame.time(cause), ms(20 + 1 + cause.index() as u64));
            assert_eq!(frame.duration, ms(33));
        }
    }

    #[test]
    fn threads_working_side_by_side_do_not_outweigh_the_gpu() {
        let (l, s) = ledger();
        let title_threads: Vec<Recorder> = (0..7).map(|_| l.recorder()).collect();
        for r in &title_threads {
            r.record(s.title, ms(20));
            r.record(s.translate, ms(1));
        }
        l.recorder().record(s.gpu, ms(30));

        let frame = l.end_frame_with(ms(33), Sample::default()).frame;
        assert_eq!(frame.cause, Some(Cause::Gpu));
        assert_eq!(frame.time(Cause::TitleCpu), ms(20));
        assert_eq!(frame.time(Cause::RiptideCpu), ms(1));
        // The budget figure is the sum over threads.
        assert_eq!(frame.riptide_cpu, ms(7));
    }

    #[test]
    fn a_frame_with_nothing_recorded_has_no_cause() {
        let (l, _) = ledger();
        let report = l.end_frame_with(ms(16), Sample::default());
        assert_eq!(report.frame.cause, None);
        assert!(report.stages.is_empty());
    }

    #[test]
    fn a_tie_goes_to_the_earlier_cause() {
        let mut by_cause = [Duration::ZERO; CAUSES];
        by_cause[Cause::TitleCpu.index()] = ms(10);
        by_cause[Cause::Gpu.index()] = ms(10);
        assert_eq!(attribute(&by_cause), Some(Cause::TitleCpu));
    }

    #[test]
    fn time_held_up_is_capped_at_the_frame() {
        let (l, s) = ledger();
        // A compile that began frames ago and ended in this one.
        l.recorder().record(s.shader, ms(400));
        let frame = l.end_frame_with(ms(50), Sample::default()).frame;
        assert_eq!(frame.time(Cause::ShaderCompile), ms(50));
        assert_eq!(frame.cause, Some(Cause::ShaderCompile));
    }

    #[test]
    fn a_span_records_only_its_own_time() {
        let (l, s) = ledger();
        let r = l.recorder();
        // Title code runs for 10 ms in total; inside it Riptide translates twice, 2 ms each,
        // and the second translation waits 1 ms of its 2 for a page.
        let title = r.begin();
        let first = r.begin();
        r.finish(s.translate, 2_000_000, first);
        let second = r.begin();
        let page = r.begin();
        r.finish(s.paging, 1_000_000, page);
        r.finish(s.translate, 2_000_000, second);
        r.finish(s.title, 10_000_000, title);

        let report = l.end_frame_with(ms(16), Sample::default());
        assert_eq!(report.frame.time(Cause::TitleCpu), ms(6));
        assert_eq!(report.frame.time(Cause::RiptideCpu), ms(3));
        assert_eq!(report.frame.time(Cause::Paging), ms(1));
        assert_eq!(report.frame.riptide_cpu, ms(3));
        assert_eq!(report.frame.cause, Some(Cause::TitleCpu));
    }

    #[test]
    fn stage_costs_are_summed_over_threads_and_reset_each_frame() {
        let (l, s) = ledger();
        let (a, b) = (l.recorder(), l.recorder());
        a.record(s.translate, ms(2));
        a.record(s.translate, ms(1));
        b.record(s.translate, ms(4));
        b.count(s.gpu, 10_000);

        let report = l.end_frame_with(
            ms(16),
            Sample {
                private_bytes: 7,
                video_bytes: 9,
            },
        );
        assert_eq!(
            report.stages,
            vec![
                StageCost {
                    name: "translate draw",
                    cause: Cause::RiptideCpu,
                    time: ms(7),
                    count: 3
                },
                StageCost {
                    name: "gpu frame",
                    cause: Cause::Gpu,
                    time: Duration::ZERO,
                    count: 10_000
                },
            ]
        );
        assert_eq!(
            (
                report.frame.index,
                report.frame.private_bytes,
                report.frame.video_bytes
            ),
            (0, 7, 9)
        );

        let next = l.end_frame_with(ms(16), Sample::default());
        assert_eq!(next.frame.index, 1);
        assert!(next.stages.is_empty());
    }

    #[test]
    fn registering_a_name_twice_gives_the_same_stage() {
        let (l, s) = ledger();
        assert_eq!(l.stage("translate draw", Cause::RiptideCpu), s.translate);
        assert_ne!(l.stage("another", Cause::RiptideCpu), s.translate);
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn registering_a_name_under_another_cause_panics() {
        let (l, _) = ledger();
        l.stage("translate draw", Cause::Gpu);
    }

    #[test]
    fn a_dropped_recorder_keeps_its_figures_for_the_frame_and_is_then_forgotten() {
        let (l, s) = ledger();
        {
            let r = l.recorder();
            r.record(s.title, ms(3));
        }
        assert_eq!(
            l.end_frame_with(ms(16), Sample::default())
                .frame
                .time(Cause::TitleCpu),
            ms(3)
        );
        assert!(lock(&l.recorders).is_empty());
    }

    #[test]
    fn real_spans_on_real_threads_are_measured() {
        let (l, s) = ledger();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let r = l.recorder();
                    let _span = r.span(s.title);
                    std::thread::sleep(ms(5));
                });
            }
        });
        let report = l.end_frame(Sample::default());
        assert_eq!(report.stages.len(), 1);
        assert_eq!(report.stages[0].count, 4);
        assert!(
            report.stages[0].time >= ms(20),
            "{:?}",
            report.stages[0].time
        );
        assert!(report.frame.duration >= ms(5));
        assert!(report.frame.time(Cause::TitleCpu) >= ms(5));
        assert_eq!(report.frame.cause, Some(Cause::TitleCpu));
    }

    fn frame(index: u64, duration: Duration, cause: Option<Cause>, riptide: Duration) -> Frame {
        let mut by_cause = [Duration::ZERO; CAUSES];
        if let Some(c) = cause {
            by_cause[c.index()] = duration;
        }
        Frame {
            index,
            duration,
            by_cause,
            cause,
            riptide_cpu: riptide,
            private_bytes: index << 30,
            video_bytes: index << 20,
        }
    }

    fn temp_file(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("riptide-trace-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_trace_file_reads_back_what_was_written() {
        let path = temp_file("round-trip");
        let frames = vec![
            frame(0, ms(33), Some(Cause::Gpu), ms(2)),
            frame(1, ms(250), Some(Cause::Paging), ms(3)),
            frame(2, ms(16), None, Duration::ZERO),
        ];
        let mut w = TraceWriter::create(&path).unwrap();
        for f in &frames {
            w.write(f).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        let back = read(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.unwrap(), frames);
    }

    #[test]
    fn a_trace_file_cut_short_gives_its_whole_frames() {
        let path = temp_file("cut-short");
        let whole = frame(0, ms(33), Some(Cause::TitleCpu), ms(1));
        std::fs::write(&path, format!("{HEADER}\n{}\n1\t5000\tgp", whole.to_line())).unwrap();
        let back = read(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.unwrap(), vec![whole]);
    }

    #[test]
    fn a_file_without_the_header_is_refused() {
        let path = temp_file("no-header");
        std::fs::write(&path, "2026-11-27T14:03:22.512Z INFO  launcher  hello\n").unwrap();
        let back = read(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_session_is_summarised_into_the_measurement_figures() {
        // 200 frames: 197 at 33 ms, and three slow ones.
        let mut frames: Vec<Frame> = (0..197)
            .map(|i| frame(i, ms(33), Some(Cause::Gpu), ms(2)))
            .collect();
        frames.push(frame(197, ms(100), Some(Cause::Paging), ms(4)));
        frames.push(frame(198, ms(300), Some(Cause::ShaderCompile), ms(4)));
        frames.push(frame(199, ms(40), None, ms(4)));

        let s = summarise(&frames).unwrap();
        assert_eq!(s.frames, 200);
        assert_eq!(s.session, ms(197 * 33 + 100 + 300 + 40));
        assert_eq!(s.median_frame, ms(33));
        // The slowest 1% of 200 frames is two frames: 300 ms and 100 ms.
        assert_eq!(s.one_percent_low, ms(200));
        assert_eq!(s.median_riptide_cpu, ms(2));
        assert_eq!(s.peak_private_bytes, 199 << 30);
        assert_eq!(s.peak_video_bytes, 199 << 20);
        assert_eq!(s.slow_frames, 2);
        assert_eq!(s.frames_by_cause, [1, 1, 0, 0, 197]);
        assert_eq!(s.unattributed, 1);

        let text = s.to_string();
        assert!(
            text.contains("median frame time: 33.00 ms (30.3 fps)"),
            "{text}"
        );
        assert!(text.contains("frames over 100 ms: 2"), "{text}");
        assert!(text.contains("gpu 197 none 1"), "{text}");
    }

    #[test]
    fn no_frames_give_no_summary() {
        assert_eq!(summarise(&[]), None);
    }
}
