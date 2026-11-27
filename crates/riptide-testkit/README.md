# riptide-testkit

What the workspace's tests share. It is a dev-dependency only; nothing that ships uses it.

## Interface

- `stand_in_title()` builds the stand-in title (`tests/fixtures/stand-in-title`) from source, once per test program, and returns the path of the image.
- `stand_in_title_bytes()` returns the image's bytes.
- `STAND_IN_TITLE` is the image's file name, `stand-in-title.exe`.

## Depends on

Nothing in this workspace. It runs `cargo` to build the fixture.
