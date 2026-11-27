# riptide-preflight

Checks, before a title starts, that the machine can carry it, and says why if it cannot.

## Interface

- `gather(&Settings, runtime: &Path) -> Machine` reads the facts from Windows: free RAM, page-file room, the graphics card and its driver version, the power plan, whether the game folder and the runtime exist.
- `judge(&Machine, &Settings) -> Vec<Check>` turns facts into checks, each `Ok`, `Warning` or `Blocking`, with a sentence of detail. Pure, so every threshold is unit-tested.
- `can_play(&[Check])` is false when any check is blocking. `summary(&[Check])` gives the one-line count the launcher shows.

## Depends on

- `riptide-config`

## Status

Working, used by `riptide-launcher`. The runtime will call it too (decision D11). Not checked yet: antivirus scanning of the cache folder, an outdated driver, and the drive the title is on.
