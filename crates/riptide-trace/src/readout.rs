//! The figures of the on-screen statistics window, kept up to date frame by frame.
//!
//! The window itself is drawn by `riptide-gpu`. This is what it shows: the frame rate now,
//! how bad the worst frames were lately, a strip of recent frame times for the graph, and
//! where the time went.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use crate::{CAUSES, Cause, Frame};

/// The frame rate and frame time shown are the mean over this much time, so the number can
/// be read instead of flickering.
pub const NOW_WINDOW: Duration = Duration::from_secs(1);

/// The 1% low, the longest frame and the shares by cause cover this much time.
pub const RECENT_WINDOW: Duration = Duration::from_secs(30);

/// How many frame times the graph shows.
pub const GRAPH_FRAMES: usize = 240;

/// More frames than this are never kept, however short they are.
const MAX_FRAMES: usize = 1 << 15;

#[derive(Clone, Copy)]
struct Entry {
    duration: Duration,
    by_cause: [Duration; CAUSES],
    draws: u64,
}

/// A rolling record of the last [`RECENT_WINDOW`] of frames.
#[derive(Default)]
pub struct Readout {
    frames: VecDeque<Entry>,
    /// The sum of the durations in `frames`.
    span: Duration,
}

/// What the statistics window shows. Rates are in frames a second, times in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Figures {
    /// Frames a second over the last [`NOW_WINDOW`].
    pub fps: f64,
    /// Mean frame time over the last [`NOW_WINDOW`].
    pub frame_ms: f64,
    /// Draw calls in the newest frame.
    pub draws: u64,
    /// The frame rate of the slowest 1% of frames in the last [`RECENT_WINDOW`].
    pub one_percent_low_fps: f64,
    /// The longest frame in the last [`RECENT_WINDOW`].
    pub max_frame_ms: f64,
    /// The newest frame times, oldest first, at most [`GRAPH_FRAMES`] of them.
    pub graph_ms: Vec<f32>,
    /// Mean time per frame each cause held frames up for over the last [`RECENT_WINDOW`],
    /// indexed by [`Cause::index`].
    pub cause_ms: [f64; CAUSES],
}

/// A cause as the window names it.
fn label(cause: Cause) -> &'static str {
    match cause {
        Cause::Paging => "Paging",
        Cause::ShaderCompile => "Shader compiling",
        Cause::RiptideCpu => "Riptide CPU",
        Cause::TitleCpu => "Title CPU",
        Cause::Gpu => "GPU",
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn rate(frame: Duration) -> f64 {
    if frame.is_zero() {
        0.0
    } else {
        1.0 / frame.as_secs_f64()
    }
}

impl Readout {
    pub fn new() -> Readout {
        Readout::default()
    }

    /// Adds a finished frame and how many draw calls it made, and forgets frames older than
    /// [`RECENT_WINDOW`].
    pub fn push(&mut self, frame: &Frame, draws: u64) {
        self.frames.push_back(Entry {
            duration: frame.duration,
            by_cause: frame.by_cause,
            draws,
        });
        self.span += frame.duration;
        // Keep the newest frame even if it alone is longer than the window.
        while self.frames.len() > 1
            && (self.frames.len() > MAX_FRAMES
                || self.span - self.frames[0].duration >= RECENT_WINDOW)
        {
            let oldest = self.frames.pop_front().expect("more than one frame");
            self.span -= oldest.duration;
        }
    }

    /// How many frames are kept.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The figures as of the newest frame. `None` before the first frame.
    pub fn figures(&self) -> Option<Figures> {
        let newest = self.frames.back()?;

        // The newest frames that together cover NOW_WINDOW, and at least one.
        let mut now_span = Duration::ZERO;
        let mut now_frames = 0u32;
        for entry in self.frames.iter().rev() {
            now_span += entry.duration;
            now_frames += 1;
            if now_span >= NOW_WINDOW {
                break;
            }
        }
        let mean_now = now_span / now_frames;

        let mut durations: Vec<Duration> = self.frames.iter().map(|e| e.duration).collect();
        durations.sort_unstable();
        let slowest = durations.len().div_ceil(100);
        let one_percent_low = durations[durations.len() - slowest..]
            .iter()
            .sum::<Duration>()
            / slowest as u32;

        let mut cause_ms = [0.0; CAUSES];
        for (index, total) in cause_ms.iter_mut().enumerate() {
            let sum: Duration = self.frames.iter().map(|e| e.by_cause[index]).sum();
            *total = ms(sum) / self.frames.len() as f64;
        }

        let skip = self.frames.len().saturating_sub(GRAPH_FRAMES);
        Some(Figures {
            fps: rate(mean_now),
            frame_ms: ms(mean_now),
            draws: newest.draws,
            one_percent_low_fps: rate(one_percent_low),
            max_frame_ms: ms(*durations.last().expect("at least one frame")),
            graph_ms: self
                .frames
                .iter()
                .skip(skip)
                .map(|e| ms(e.duration) as f32)
                .collect(),
            cause_ms,
        })
    }
}

impl Figures {
    /// The cause that held frames up longest over the last [`RECENT_WINDOW`], with its mean
    /// time per frame. `None` when nothing was recorded.
    pub fn main_cause(&self) -> Option<(Cause, f64)> {
        Cause::ALL
            .into_iter()
            .map(|cause| (cause, self.cause_ms[cause.index()]))
            .filter(|&(_, time)| time > 0.0)
            // A tie goes to the earlier cause, as it does for a single frame.
            .fold(None, |best: Option<(Cause, f64)>, next| match best {
                Some(best) if best.1 >= next.1 => Some(best),
                _ => Some(next),
            })
    }
}

impl fmt::Display for Figures {
    /// The window's text, without the machine's own lines (graphics card, processor,
    /// memory), which come from elsewhere.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{:.1} FPS", self.fps)?;
        writeln!(
            f,
            "{:.1} ms/frame | {} draws/frame",
            self.frame_ms, self.draws
        )?;
        write!(
            f,
            "1% low {:.0} FPS | max {:.0} ms",
            self.one_percent_low_fps, self.max_frame_ms
        )?;
        for cause in Cause::ALL {
            let time = self.cause_ms[cause.index()];
            if time > 0.0 {
                write!(f, "\n{} {time:.1} ms", label(cause))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ms: u64, cause: Option<(Cause, u64)>) -> Frame {
        let mut by_cause = [Duration::ZERO; CAUSES];
        if let Some((cause, time)) = cause {
            by_cause[cause.index()] = Duration::from_millis(time);
        }
        Frame {
            index: 0,
            duration: Duration::from_millis(ms),
            by_cause,
            cause: cause.map(|(cause, _)| cause),
            riptide_cpu: Duration::ZERO,
            private_bytes: 0,
            video_bytes: 0,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn there_are_no_figures_before_the_first_frame() {
        assert_eq!(Readout::new().figures(), None);
        assert!(Readout::new().is_empty());
    }

    #[test]
    fn one_frame_gives_its_own_rate() {
        let mut readout = Readout::new();
        readout.push(&frame(40, None), 1034);
        let figures = readout.figures().unwrap();
        assert!(close(figures.fps, 25.0));
        assert!(close(figures.frame_ms, 40.0));
        assert_eq!(figures.draws, 1034);
        assert!(close(figures.one_percent_low_fps, 25.0));
        assert!(close(figures.max_frame_ms, 40.0));
        assert_eq!(figures.graph_ms, [40.0]);
    }

    #[test]
    fn the_rate_is_the_mean_over_the_last_second_not_the_last_frame() {
        let mut readout = Readout::new();
        // Two seconds at 20 ms, then one second at 40 ms.
        for _ in 0..100 {
            readout.push(&frame(20, None), 0);
        }
        for _ in 0..25 {
            readout.push(&frame(40, None), 0);
        }
        let figures = readout.figures().unwrap();
        assert!(close(figures.fps, 25.0), "{}", figures.fps);
        assert!(close(figures.frame_ms, 40.0));

        // One long frame moves the mean of the last second, it does not replace it. The
        // last second is now the 100 ms frame and 23 frames of 40 ms: 1020 ms in 24 frames.
        readout.push(&frame(100, None), 0);
        let figures = readout.figures().unwrap();
        assert!(
            close(figures.frame_ms, 1020.0 / 24.0),
            "{}",
            figures.frame_ms
        );
    }

    #[test]
    fn the_one_percent_low_is_the_mean_of_the_slowest_hundredth() {
        let mut readout = Readout::new();
        for _ in 0..198 {
            readout.push(&frame(10, None), 0);
        }
        readout.push(&frame(50, None), 0);
        readout.push(&frame(70, None), 0);
        // 200 frames: the slowest two are 50 and 70 ms, mean 60 ms.
        let figures = readout.figures().unwrap();
        assert!(close(figures.one_percent_low_fps, 1000.0 / 60.0));
        assert!(close(figures.max_frame_ms, 70.0));
    }

    #[test]
    fn frames_older_than_the_recent_window_are_forgotten() {
        let mut readout = Readout::new();
        readout.push(&frame(900, None), 0);
        // 30 seconds of 100 ms frames push the 900 ms one out.
        for _ in 0..300 {
            readout.push(&frame(100, None), 0);
        }
        let figures = readout.figures().unwrap();
        assert_eq!(readout.len(), 300);
        assert!(close(figures.max_frame_ms, 100.0));
    }

    #[test]
    fn a_frame_longer_than_the_window_is_still_shown() {
        let mut readout = Readout::new();
        readout.push(&frame(20, None), 0);
        readout.push(&frame(45_000, None), 7);
        let figures = readout.figures().unwrap();
        assert_eq!(readout.len(), 1);
        assert!(close(figures.frame_ms, 45_000.0));
        assert_eq!(figures.draws, 7);
    }

    #[test]
    fn the_graph_holds_the_newest_frames_oldest_first() {
        let mut readout = Readout::new();
        // Ten frames of 5 ms, then a full graph's worth of 8 ms, with one 9 ms frame last.
        // All of it fits in the recent window, so only the graph's own length cuts it.
        for _ in 0..10 {
            readout.push(&frame(5, None), 0);
        }
        for _ in 0..GRAPH_FRAMES - 1 {
            readout.push(&frame(8, None), 0);
        }
        readout.push(&frame(9, None), 0);
        let figures = readout.figures().unwrap();
        assert_eq!(readout.len(), GRAPH_FRAMES + 10);
        assert_eq!(figures.graph_ms.len(), GRAPH_FRAMES);
        assert_eq!(figures.graph_ms[0], 8.0);
        assert_eq!(*figures.graph_ms.last().unwrap(), 9.0);
    }

    #[test]
    fn time_by_cause_is_the_mean_per_frame_and_names_the_main_one() {
        let mut readout = Readout::new();
        readout.push(&frame(40, Some((Cause::Gpu, 30))), 0);
        readout.push(&frame(40, Some((Cause::Gpu, 10))), 0);
        readout.push(&frame(40, Some((Cause::TitleCpu, 12))), 0);
        readout.push(&frame(40, None), 0);
        let figures = readout.figures().unwrap();
        assert!(close(figures.cause_ms[Cause::Gpu.index()], 10.0));
        assert!(close(figures.cause_ms[Cause::TitleCpu.index()], 3.0));
        assert_eq!(
            figures.main_cause().map(|(cause, _)| cause),
            Some(Cause::Gpu)
        );

        let mut idle = Readout::new();
        idle.push(&frame(40, None), 0);
        assert_eq!(idle.figures().unwrap().main_cause(), None);
    }

    #[test]
    fn the_figures_print_as_the_windows_lines() {
        let mut readout = Readout::new();
        for _ in 0..99 {
            readout.push(&frame(38, Some((Cause::Gpu, 30))), 1034);
        }
        readout.push(&frame(69, Some((Cause::Gpu, 30))), 1034);
        let text = readout.figures().unwrap().to_string();
        let lines: Vec<&str> = text.lines().collect();
        // The last second is the 69 ms frame and 25 of 38 ms: 1019 ms in 26 frames.
        assert_eq!(lines[0], "25.5 FPS");
        assert_eq!(lines[1], "39.2 ms/frame | 1034 draws/frame");
        assert_eq!(lines[2], "1% low 14 FPS | max 69 ms");
        assert_eq!(lines[3], "GPU 30.0 ms");
        assert_eq!(lines.len(), 4);
        assert!(lines[0].ends_with(" FPS"));
    }
}
