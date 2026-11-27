# riptide-launcher

Riptide's launcher: one window for every setting, in place of flags and environment variables.

## Using it

    cargo run -p riptide-launcher

Five tabs (Game, Display, Performance, Checks, Logs), then **Play** or **Quit**. Every change is saved at once to `%APPDATA%\Riptide\riptide.toml`.

**Play** runs the checks. A blocking check stops it and opens the Checks tab. Otherwise it starts `riptide.exe` from the launcher's own folder with `run --config <file>` and closes. Until the runtime exists, it says so and leaves the settings saved.

A settings file that will not load is moved to `riptide.toml.bad`, and the launcher starts from the defaults and says so.

## Interface

A program, not a library. Its decisions are in `src/logic.rs` and are unit-tested; the window code in `src/app.rs` only draws and edits a `Settings` value.

## Depends on

- `riptide-config`
- `riptide-preflight`
- `riptide-log`

## Status

Working. The settings describe a runtime that does not exist yet, so some will change.
