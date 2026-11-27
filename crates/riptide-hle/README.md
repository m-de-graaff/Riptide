# riptide-hle

Console API implementations, and mapping title threads onto host cores.

## Interface

So far: the import table.

- `Imports` decides what each function a title imports is bound to. It is the `Resolver` that `riptide-pe::load` binds a title with. There are three answers:
  - **riptide**: Riptide's own implementation, added with `implement(module, symbol, address)`;
  - **host**: the host's function of the same module and name, where the console's is the same, added with `pass_through(module, name)`;
  - **stub**: everything else.
- `Imports::census(&image)` lists every import of a parsed image with its binding, sorted, with totals. It loads and runs nothing. `riptide census <image>` prints it.
- A **stub** is a few bytes of generated code, one per import. Calling it logs `unimplemented import module!function was called`, once per import, and raises the exception `UNIMPLEMENTED_IMPORT` (`0xE0524950`), which cannot be continued. Unhandled, it ends the process with that exit code. A stub never returns a value.

Still to come: the console functions themselves, threads and core mapping, timers, the file path, runtime services.

## Decisions made without a design document

- **A called stub stops the title.** The rule is that an unimplemented function fails loudly and never returns a guess. Any return value is a guess for some function (a pointer, a handle, a count), so the stub does not return. If a softer failure is ever wanted for chosen functions, that is an implementation of them, not a stub.
- **The table Riptide ships with is empty.** No console function is implemented yet, and no host function is passed through until someone has judged that the console's version is the same. So today the census of a real title is all stubs, which is the true state. Each console API issue adds its functions.
- **Pass-through is per function, never per module.** A function the table passes through but this Windows lacks becomes a stub.
- **Module names compare without case; function names with it**, as Windows does.
- **Stub code is never writable while executable.** Each block is one page of identical code and one page of data. The code page is filled once and made execute-only; a stub finds its own name through the data page.

## Tests

`cargo test -p riptide-hle` builds the stand-in title, takes its census, binds and runs it through the table, and, in a child process, calls its unbuilt import and checks the one log line and the exit code.

## Depends on

- `riptide-pe`
- `riptide-mem` (not used yet)
- `riptide-trace` (not used yet)

## Status

Import resolution is built. Piece 4 of the build order, together with `riptide-pe`.
