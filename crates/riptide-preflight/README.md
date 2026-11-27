# riptide-preflight

Checks, before a title starts, that the machine can carry it, and says why if it cannot.

## Interface

- `gather(&Settings, runtime: &Path) -> Machine` reads the facts from Windows: free RAM, page-file room, the graphics card and its driver version, the power plan, whether the game folder and the runtime exist, the game's drive (free space, and whether it is a hard disk), and which antivirus products have real-time scanning on.
- `judge(&Machine, &Settings) -> Vec<Check>` turns facts into checks, each `Ok`, `Warning` or `Blocking`, with a sentence of detail. Pure, so every threshold is unit-tested. `judge_with` takes the measured driver from the caller.
- `can_play(&[Check])` is false when any check is blocking. `summary(&[Check])` gives the one-line count the launcher shows.

## Depends on

- `riptide-config`

## Status

Working, used by `riptide-launcher`. The runtime will call it too (decision D11).

Two checks are limited by what is known:

- **Graphics driver.** `MEASURED_DRIVER` is `None` until `docs/measurements.md` has its first entry, so nothing is compared yet. Set it with each measurement.
- **Antivirus.** Windows shows a product's exclusions only to administrators. The check warns whenever a real-time scanner is active and names the folder to exclude; it cannot see that the exclusion is already made.
