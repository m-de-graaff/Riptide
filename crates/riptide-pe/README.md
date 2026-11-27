# riptide-pe

Loading, mapping and binding the title image.

## Interface

- `Image::parse(bytes)` reads a PE32+ file: sections, imports by module, base relocations, the TLS directory and the unwind table. It maps and runs nothing. A malformed file gives a `ParseError`; the parser never reads outside the bytes it was given.
- `load(name, bytes, &resolver)` maps the image into this process, relocates it if its preferred base is taken, writes the address of every import into the import address table, sets each section's protection, and registers the unwind table with Windows. It runs none of the image's code.
- `Resolver` supplies the address of each imported function, by module and by name or ordinal. `riptide-hle` implements it; a closure works too. If any import has no function, `load` fails with `LoadError::Unresolved`, listing every one as `module!function`.
- `LoadedImage::run()` runs the TLS callbacks and then the entry point on the calling thread, and returns the entry point's result, the title's exit code. It is `unsafe`: the title's code runs natively.
- `report_unhandled_faults()` makes a fault that nothing handles print one line before the process ends, such as `riptide: unhandled fault 0xc0000005 (access violation writing 0x0) at title.exe+0x1a2b`. `describe(address)` gives the `module+0xoffset` part for any address.

## Decisions made without a design document

- **The image is mapped with `VirtualAlloc`, not into `riptide-mem`'s block.** The block exists so the GPU can read title data in place (D1). The GPU does not read the title's code. If the image has to live in the block after all, `Mapping::new` is the one place that changes.
- **Only unhandled faults are reported.** The reporter is an unhandled-exception filter, not a first-chance handler, so faults the title's own handlers or the write guards (D3, D4) deal with never reach it.
- **Every missing import is an error at load.** Stubs that fail by name when called are `riptide-hle`'s to supply through the resolver (issue #10); the loader itself never invents an address.
- **Declared Windows functions, no dependency.** The loader calls eight kernel32 functions and declares them itself in `sys.rs`.

## Refused for now

`load` fails with `LoadError::Unsupported`, naming the feature, for an image with:

- **thread-local data** (a TLS template or zero fill). Callbacks run; a per-thread block for the title's `thread_local` variables is not set up yet.
- **delay-loaded imports.**

Exports, resources and the load configuration directory are not read.

## Tests

`cargo test -p riptide-pe` builds the stand-in title (`tests/fixtures/stand-in-title`) from source, then parses, loads and runs it. The tests check its imports, its exit code, everything it reports, relocation at a second base, the names of missing imports, and, in a child process, the line an unhandled fault prints.

## Depends on

Nothing in this workspace, and no other crate.

## Status

Built for the stand-in title. Piece 4 of the build order, together with `riptide-hle`.
