# riptide

Command-line front end: the runtime the launcher starts.

## Interface

### `riptide run [--config <file>] [--log <file>]`

Starts the title in the game folder the settings name. The launcher's Play runs it with both arguments (launcher decision L5).

1. Joins the session log: appends to `--log`, or starts a session file of its own in `%LOCALAPPDATA%\Riptide\logs`.
2. Reads the settings from `--config`, or from `%APPDATA%\Riptide\riptide.toml`.
3. Runs the preflight checks and logs each one. If a check blocks, it logs `Not started:` with the reason and exits.
4. Finds the title's image in the game folder, logs how its imports are bound, loads it and runs it.
5. Logs the title's exit code and exits with it.

A called function that Riptide does not implement, and a crash, are both logged with their name or place before the process ends.

| Exit code | Meaning |
| --- | --- |
| the title's own | the title ran and returned |
| 2 | the arguments are wrong |
| 3 | the settings file could not be read or is not valid |
| 4 | a preflight check blocks |
| 5 | the game folder holds no title image, or several candidates |
| 6 | the title image could not be loaded |
| `0xE0524950` | the title called a function Riptide does not implement |

### `riptide census <image>`

Lists every function the image imports and what Riptide binds it to: `riptide` (implemented), `host` (Windows' own function) or `stub` (named and refused if called), with totals. The image is read, never run. Delay-loaded imports are not listed yet.

```text
stub    standin_console.dll!StandInAdd
stub    standin_ordinal.dll!#7
2 imports: 0 implemented by Riptide, 0 passed to the host, 2 stubs
```

## Decisions made without a design document

- **The title is the one `.exe` file directly in the game folder.** How a title taken from a console is laid out on disk is not settled. A folder with no `.exe`, or with several, is refused with the reason, not guessed at.
- **The runtime runs the preflight checks itself**, even when the launcher has just run them: it can be started without the launcher, and memory can have changed in between.
- **The title runs on the runtime's main thread.** Title threads on their own cores come with `riptide-hle`'s thread work.
- **The exit codes above.** The launcher does not read them; they are for scripts and tests.

## Depends on

- `riptide-config`, `riptide-log`, `riptide-preflight`
- `riptide-pe`, `riptide-hle`
- `riptide-gpu`, `riptide-trace` (not used yet)

## Status

`run` and `census` are built. Nothing draws yet, and nearly every console function is a stub, so a real title stops at its first unimplemented call, by name. Piece 8 of the build order.
