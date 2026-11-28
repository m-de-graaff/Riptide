# riptide-hle

Console API implementations, and mapping title threads onto host cores.

## Interface

So far: the import table, sleeps and timed waits, and title threads on their own cores.

- `Imports` decides what each function a title imports is bound to. It is the `Resolver` that `riptide-pe::load` binds a title with. There are three answers:
  - **riptide**: Riptide's own implementation, added with `implement(module, symbol, address)`;
  - **host**: the host's function of the same module and name, where the console's is the same, added with `pass_through(module, name)`;
  - **stub**: everything else.
- `Imports::census(&image)` lists every import of a parsed image with its binding, sorted, with totals. It loads and runs nothing. `riptide census <image>` prints it.
- A **stub** is a few bytes of generated code, one per import. Calling it logs `unimplemented import module!function was called`, once per import, and ends the process with the exit code `UNIMPLEMENTED_IMPORT` (`0xE0524950`). It does not raise an exception, because a title's own handler could take one and carry on. A stub never returns a value.
- `timers::sleep` and `timers::wait_for_single_object` are the title's `Sleep` and `WaitForSingleObject` (smaller decision S1). They wait on a high-resolution waitable timer, one per thread, so a 1 ms sleep takes about 1 ms and not Windows' 15.6 ms tick. `Imports::new()` binds them to `kernel32.dll!Sleep` and `kernel32.dll!WaitForSingleObject`.
- `cores::CoreMap` lays the console's seven title cores over the host's (decision D8). Console core `n` is the first hardware thread of the `n`th physical core, fastest cores first. Riptide's own threads get everything else: the remaining cores and the second hardware thread of the title's cores. On the 7800X3D that is processors 0, 2, 4, ... 12 for the title and 1, 3, ... 13, 14, 15 for Riptide.
- The title's `CreateThread`, `SetThreadAffinityMask`, `SetThreadIdealProcessor` and `GetCurrentProcessorNumber` speak the console's core numbers and are turned into the host's and back. A thread the title creates may run on the title's processors only. `cores::pin_riptide_thread()` keeps one of Riptide's own threads off them.
- `saves::write` and `saves::read` are saves that cannot be half-written (smaller decision S6): a save is written beside the old one, flushed, and renamed over it. Nothing calls them yet; the console's save functions will.

Still to come: the other console functions, the file path, runtime services.

## Decisions made without a design document

- **A called stub stops the title.** The rule is that an unimplemented function fails loudly and never returns a guess. Any return value is a guess for some function (a pointer, a handle, a count), so the stub does not return. If a softer failure is ever wanted for chosen functions, that is an implementation of them, not a stub.
- **The table Riptide ships with is nearly empty.** It holds the timer functions. No host function is passed through until someone has judged that the console's version is the same. So today the census of a real title is nearly all stubs, which is the true state. Each console API issue adds its functions. `Imports::empty` gives a table with nothing in it.
- **The timer functions are bound under `kernel32.dll`.** That is the module a Windows program imports them from. Whether a console title names the same module is not known yet; the first census of a real title on the target machine shows it, and a second name is one more `implement` line.
- **A sleep of zero or forever, and a wait of zero or forever, go to Windows unchanged.** Only a real timeout needs the timer.
- **If Windows gives no high-resolution timer, the sleep falls back to Windows' own**, late but not lost.
- **Not covered yet:** `SleepEx`, `WaitForMultipleObjects`, and the timeouts of condition variables and critical sections.
- **The title has seven cores, numbered 0 to 6.** That is the README's figure for what the console gives a title. A bit for any other core in an affinity mask is dropped, and a mask left empty means all seven.
- **A host with fewer than eight physical cores shares them**, console cores wrapping around. It runs, and it is not what Riptide is built for.
- **A processor that is no console core's** counts as the console core on the same physical core, or else as core 0, when the title asks where it is running.
- **Only the first processor group is read**: 64 logical processors.
- **Not covered yet:** what the title is told about the machine (`GetSystemInfo`, `GetProcessAffinityMask`), thread priorities, and threads the title creates in other ways.
- **Pass-through is per function, never per module.** A function the table passes through but this Windows lacks becomes a stub.
- **Module names compare without case; function names with it**, as Windows does.
- **Stub code is never writable while executable.** Each block is one page of identical code and one page of data. The code page is filled once and made execute-only; a stub finds its own name through the data page.

## Tests

`cargo test -p riptide-hle` builds the stand-in title, takes its census, binds and runs it through the table, has it sleep 1 ms a thousand times, and, in a child process, calls its unbuilt import and checks the one log line and the exit code. The upper bounds on sleep times are skipped when `CI` is set: a shared runner's timers jitter too much.

## Depends on

- `riptide-pe`
- `riptide-mem` (not used yet)
- `riptide-trace` (not used yet)

## Status

Import resolution, 1 ms timers and the core map are built. Whether the one-core budget holds (open question Q7) has to be measured on the target machine. Piece 4 of the build order, together with `riptide-pe`.

Built since, against the real title: console memory calls on `riptide-mem`, the package drive, DirectStorage's file path (no BCPack, issue #33), and the parts of the runtime services (`xgameruntime.dll`) the title calls on its way to the graphics device: task queues, networking (offline), users (change events only), packages (fully installed), persistent local storage (`%LOCALAPPDATA%\Riptide\titles\<package identity>\local-storage`) and display (no HDR). Every method of those areas that is not built names itself and stops the title when called.

With this the real title runs its start-up, opens its archives and calls `D3D12XboxCreateDevice`, which `riptide-gpu` answers. This crate binds the graphics module's two functions to it and gives it the title's memory and allocator.

Also built for the title's graphics set-up:

- `kernel32.dll!VirtualFree` on title memory. A title frees what `XMemVirtualAlloc` gave it with this, and cuts a reservation into parts with it, so those calls go to `riptide-mem`. Any other address goes to Windows.
- `XMemAllocDefault` hands out title memory, below 2^40, for the kinds of memory the GPU uses. The title's own allocator asks it for the memory of committed graphics resources.
- `xg_xs.dll`, the console's texture-layout library, on `riptide-formats`: the suggested mode, and a texture computer's layout, element offsets and copies into and out of a layout. Its other methods name themselves when called.
- `XboxBvhBuilder.dll`, the module a title builds ray-tracing structures with, as a stand-in: a builder can be made, dropped and given its options, and builds nothing. Its other methods name themselves when called. The real module ships in the title's package and needs its shader programs translated before it can be loaded.

Built for the title's first frames and what it does next:

- **Staying offline.** A title's sockets and HTTP calls are Windows' own, so the calls that start talking to another machine are refused with Windows' no-network errors and written to the session log: connecting a socket, sending a datagram, looking a host name up, opening an HTTP connection. A title reaches no server.
- **Audio, silent.** `AcpHal.dll` (the audio hardware: its heap with two kinds of address, the context arrays, the processor object), `xapu.dll` (the decoder: a packet decodes to silence of the packet's own length) and `chad.dll` (effects: every block comes back silent). Nothing is decoded or mixed, so a title runs without sound.
- **More runtime areas.** Asynchronous calls (`XAsync`) in the task queue area, with task queue ports and queues made of them. One local, offline user with a made-up id and the name "Player", who is an adult and holds no service privileges. The game area (the title id), the system area (the sandbox name), accessibility (a speech synthesizer that speaks nothing, high contrast off), and saves (see the module's own notes for what of it is built).
- **The texture computer's 64 KiB tiling**, which a title maps streaming pages by.

`riptide run` loads three modules from the title's own package beside it, as the title is loaded, and binds the title's imports to their functions: the video player (`Bink2Scarlett.dll`) and the two HTTP libraries (`libHttpClient.GDK.dll`, `XCurl.dll`). They need nothing of the console but what is answered here.
