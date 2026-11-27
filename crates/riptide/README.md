# riptide

Command-line front end.

## Interface

`riptide run <title folder>` and the commands that inspect a title without running it.

Built so far:

- `riptide census <image>` lists every function the image imports and what Riptide binds it to: `riptide` (implemented), `host` (Windows' own function) or `stub` (named and refused if called), with totals. The image is read, never run. Delay-loaded imports are not listed yet.

```text
stub    standin_console.dll!StandInAdd
stub    standin_ordinal.dll!#7
2 imports: 0 implemented by Riptide, 0 passed to the host, 2 stubs
```

## Depends on

- `riptide-hle`
- `riptide-pe`
- `riptide-gpu`
- `riptide-trace`

## Status

`census` is built. `run` is not started. Piece 8 of the build order.
