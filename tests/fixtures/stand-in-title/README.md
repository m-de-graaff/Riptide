# Stand-in title

A small program of our own, built as a freestanding PE32+ image, that `riptide-pe` and `riptide-hle` load in tests. It imports the functions under test and reports what they returned, so the loader can be tested without any game or SDK file.

## What it holds

Everything a loader has to handle, and nothing else:

- imports by name from `standin_console.dll`, by ordinal from `standin_ordinal.dll`, and functions from `kernel32.dll`;
- a stored pointer, so the image has base relocations;
- writable data;
- a TLS directory with one callback;
- an unwind table;
- an entry point, `title_entry`, that returns exit code 42.

The module and function names are made up for this fixture. As `riptide-hle` grows, the console functions under test are added here as further imports.

| Import | What it is for |
| --- | --- |
| `StandInReport(id, value)` | Hands one observed value to the test. |
| `StandInAdd(a, b)` | Returns `a + b`; shows arguments and results cross an import correctly. |
| `StandInMode()` | 0 to run to the exit, 1 to write to address zero and fault, 2 to call `StandInUnbuilt`, 3 to call `Sleep(1)` 1,000 times. |
| `StandInUnbuilt()` | A function no test implements, called only in mode 2: what a call to an unbuilt console function looks like. |
| `kernel32.dll!GetCurrentThreadId` | A function whose console version is the host's own. Its result is reported. |
| `kernel32.dll!Sleep` | The title's sleep, called only in mode 3. |
| ordinal 7 of `standin_ordinal.dll` | An import with no name in the image. |

## Building it

Tests build it on demand. To build it by hand:

```powershell
cargo build --manifest-path tests/fixtures/stand-in-title/Cargo.toml
```

The image is `tests/fixtures/stand-in-title/target/debug/stand-in-title.exe`, about 5 KiB. It is never tracked.

## Why it is not a workspace member

It has no standard library and no C runtime, so it needs `panic = "abort"` in every profile. `cargo test` builds workspace members with unwinding panics, which a freestanding image cannot have. So it is its own small package with its own profile, and its `build.rs` passes the linker `/ENTRY:title_entry /NODEFAULTLIB`.

It defines `memcpy`, `memset`, `memcmp`, `_fltused` and `__CxxFrameHandler3` itself, because the core library refers to them and there is no C runtime to supply them.
