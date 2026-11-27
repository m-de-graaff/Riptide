# riptide-hle

Console API implementations, and mapping title threads onto host cores.

## Interface

So far: the import table, and sleeps and timed waits.

- `Imports` decides what each function a title imports is bound to. It is the `Resolver` that `riptide-pe::load` binds a title with. There are three answers:
  - **riptide**: Riptide's own implementation, added with `implement(module, symbol, address)`;
  - **host**: the host's function of the same module and name, where the console's is the same, added with `pass_through(module, name)`;
  - **stub**: everything else.
- `Imports::census(&image)` lists every import of a parsed image with its binding, sorted, with totals. It loads and runs nothing. `riptide census <image>` prints it.
- A **stub** is a few bytes of generated code, one per import. Calling it logs `unimplemented import module!function was called`, once per import, and raises the exception `UNIMPLEMENTED_IMPORT` (`0xE0524950`), which cannot be continued. Unhandled, it ends the process with that exit code. A stub never returns a value.
- `timers::sleep` and `timers::wait_for_single_object` are the title's `Sleep` and `WaitForSingleObject` (smaller decision S1). They wait on a high-resolution waitable timer, one per thread, so a 1 ms sleep takes about 1 ms and not Windows' 15.6 ms tick. `Imports::new()` binds them to `kernel32.dll!Sleep` and `kernel32.dll!WaitForSingleObject`.

Still to come: the other console functions, threads and core mapping, the file path, runtime services.

## Decisions made without a design document

- **A called stub stops the title.** The rule is that an unimplemented function fails loudly and never returns a guess. Any return value is a guess for some function (a pointer, a handle, a count), so the stub does not return. If a softer failure is ever wanted for chosen functions, that is an implementation of them, not a stub.
- **The table Riptide ships with is nearly empty.** It holds the timer functions. No host function is passed through until someone has judged that the console's version is the same. So today the census of a real title is nearly all stubs, which is the true state. Each console API issue adds its functions. `Imports::empty` gives a table with nothing in it.
- **The timer functions are bound under `kernel32.dll`.** That is the module a Windows program imports them from. Whether a console title names the same module is not known yet; the first census of a real title on the target machine shows it, and a second name is one more `implement` line.
- **A sleep of zero or forever, and a wait of zero or forever, go to Windows unchanged.** Only a real timeout needs the timer.
- **If Windows gives no high-resolution timer, the sleep falls back to Windows' own**, late but not lost.
- **Not covered yet:** `SleepEx`, `WaitForMultipleObjects`, and the timeouts of condition variables and critical sections.
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

Import resolution and 1 ms timers are built. Piece 4 of the build order, together with `riptide-pe`.
