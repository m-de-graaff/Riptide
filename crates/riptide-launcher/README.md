# riptide-launcher

Riptide's launcher: one window for every setting, in place of flags and environment variables.

## Using it

    cargo run -p riptide-launcher

Five tabs (Game, Display, Performance, Checks, Logs), then **Play** or **Quit**. Every change is saved at once to `%APPDATA%\Riptide\riptide.toml`.

**Play** runs the checks. A blocking check stops it and opens the Checks tab. Otherwise it starts `riptide.exe` from the launcher's own folder with `run --config <file>` and closes. Until the runtime exists, it says so and leaves the settings saved.

A settings file that will not load is moved to `riptide.toml.bad`, and the launcher starts from the defaults and says so.

## Interface

A program, not a library. Its decisions are in `src/logic.rs` and are unit-tested; the window code in `src/app.rs` only draws and edits a `Settings` value.

## Icon and release package

- **The icon is drawn by `scripts/make-icon.ps1`**, from code, in the launcher's colours. It writes `assets/riptide.ico`, which `build.rs` builds into the .exe together with the version information, and `assets/icon-64.rgba`, which the window shows in its title bar and on the taskbar. Run the script again only when the drawing changes, and commit both files.
- **`scripts/package.ps1` makes the release package**: `local/dist/riptide-<version>-win-x64.zip`, holding `riptide-launcher.exe`, `riptide.exe` beside it, and a short README. The programs have the C runtime linked in, so the package needs no installer, no Visual C++ redistributable and no Rust toolchain; the script checks that neither imports a C runtime DLL. `-Target7800X3D` makes the package for the target machine.
- A release build has no console window, and the launcher starts the runtime without one.
- If the Windows SDK's resource compiler is missing, the launcher still builds, with a warning, and has no icon or version page.

## Depends on

- `riptide-config`
- `riptide-preflight`
- `riptide-log`

## Status

Working. The settings describe a runtime that does not exist yet, so some will change.
