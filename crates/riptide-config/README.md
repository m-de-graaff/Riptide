# riptide-config

Riptide's settings: one TOML file that the launcher writes and the runtime reads. There are no environment variables and no command-line flags for settings.

## Interface

`Settings` and its parts, `default_path()` (`%APPDATA%\Riptide\riptide.toml`), `load`, `save` (atomic), `set_aside` (for a file that will not load) and `Settings::problems()` (values out of range, as sentences).

Keys left out of the file keep their defaults. An unknown key is an error that names the key.

## Depends on

Nothing in this workspace. No window code and no Windows calls, so everything is unit-tested.

## Status

Working. Settings will change as the runtime is built; each is one field here and one widget in `riptide-launcher`.
