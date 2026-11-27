# riptide-log

Riptide's session log: one text file per launcher session, written by the launcher and by the runtime it starts.

## Interface

- `Logger::start(dir, source)` opens a new session file in `dir`, named by its start time, and deletes all but the newest `KEEP_SESSIONS` (10). `Logger::append(path, source)` writes to an existing session file; the runtime uses it with the path the launcher passes.
- `logger.log(level, message)` appends one line and flushes. A failed write is dropped: logging never stops the program.
- `sessions(dir)` lists the session files, newest first. `read(path)` returns their lines; a line that does not parse is kept as plain text.
- `redact(text, home)` replaces the user's home folder with `%USERPROFILE%`, for copying a log to share.
- `default_dir()` is `%LOCALAPPDATA%\Riptide\logs`.

A line is `2026-11-27T14:03:22.512Z WARN  launcher  message`. Times are UTC.

## What goes in

Events and their reasons: start-up, settings problems, check results, what Play did, and later the runtime's session (hardware, unimplemented functions, device loss, shader compiling, memory pressure, slow frames, crashes). Never game data or file contents.

## Depends on

Nothing, not even other crates in this workspace.

## Status

Working, used by `riptide-launcher`.
