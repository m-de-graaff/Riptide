# riptide-trace

Frame timing and a per-stage cost ledger, so every other crate is measured from day one.

## Interface

A ledger the other crates record named stages into (start, end, count), a per-frame summary, and a writer for the session log that `docs/measurements.md` results come from. Also records and replays guest command streams locally, for regression tests that need no game.

## Depends on

Nothing in this workspace.

## Status

Not started. Piece 2 of the build order.
