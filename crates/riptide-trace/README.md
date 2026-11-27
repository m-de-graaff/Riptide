# riptide-trace

Frame timing and a per-stage cost ledger, so every other crate is measured from day one.

## Interface

**Recording.** A session has one `Ledger`. A crate registers its stages once with `ledger.stage(name, cause)` and each thread takes a `Recorder` from `ledger.recorder()`. A recorder touches only its own thread's counters, so recording never contends.

- `recorder.span(stage)` times a stage until the span is dropped. Spans nest, and a span records only its own time, so a Riptide stage inside a title stage is not counted twice.
- `recorder.record(stage, elapsed)` adds time measured elsewhere, such as GPU time from timestamp queries.
- `recorder.count(stage, n)` adds occurrences without time.

**Frames.** `ledger.end_frame(sample)` is called once per presented frame and returns a `FrameReport`: the `Frame` and what each stage cost. `Sample` carries private memory and video memory, which `riptide-mem` and `riptide-gpu` account for. `end_frame_with(duration, sample)` takes a duration measured by the caller.

**Cause.** A stage belongs to one `Cause`: `Paging`, `ShaderCompile`, `RiptideCpu`, `TitleCpu` or `Gpu`. For each cause a frame was held up for as long as the busiest single thread spent in it, capped at the frame's duration. Seven title threads working side by side for 20 ms hold the frame up for 20 ms, not 140. The frame's cause is the largest of the five; a tie goes to the earlier one in that list, and a frame with nothing recorded has none. `frame.riptide_cpu` is Riptide's CPU time summed over all threads, which is what the one-core budget and the 10,000-draw bar are measured against.

**Trace file.** `TraceWriter::create(path)` and `writer.write(&frame)` write one line per frame; `read(path)` reads them back. A line that does not parse is skipped, so a file cut short by a crash still gives every whole frame. The file holds numbers only: a frame index, times, a cause and two memory sizes.

**Summary.** `summarise(&frames)` gives the figures `docs/measurements.md` asks for: median frame time, 1% low (the mean of the slowest 1% of frames), median Riptide CPU time per frame, peak private memory, peak video memory, frames of 100 ms or more, and how many frames each cause was blamed for. It prints as text.

**Readout.** `Readout::push(&frame, draws)` keeps the last 30 seconds of frames, and `readout.figures()` gives what the on-screen statistics window shows: frames a second and frame time as the mean over the last second, draw calls in the newest frame, the 1% low and the longest frame over the last 30 seconds, the newest 240 frame times for the graph, and the mean time per frame each cause held frames up for. `Figures` prints as the window's text lines. The window itself, and the machine's lines in it (graphics card, processor, memory), belong to `riptide-gpu` and `riptide`.

## Depends on

Nothing in this workspace, and no other crates.

## Status

Working: recording, attribution, the trace file, the summary and the readout's figures, with tests on synthetic frames. No crate records into it yet. Recording and replaying guest command streams is not built; it waits for `riptide-gpu`, which defines those streams.
