# Continuing on the target machine

Written 2026-11-27, at the end of a session on the laptop. Paste the prompt below into a new Claude Code session opened in this repository on the target machine. Fill in the game path first.

Delete this file once the pull requests it lists are merged and the first census is done.

## The prompt

```text
/working-issues https://github.com/m-de-graaff/Riptide/issues

This is the target machine (RX 7900 XTX, Ryzen 7 7800X3D). The game build is at: <PATH TO THE GAME FOLDER>. It stays outside the repository; never copy or commit anything from it.

I am the only one working on this project. From now on work directly on main: no branch per issue, no pull requests. Commit finished, verified work to main, push, and comment on the issue. This replaces the "one branch per issue, open a pull request, merging is the user's" step in the working-issues skill; update that skill to say so as your first commit.

Step 0: review and merge the open pull requests. They were all built on a laptop in one session and nobody has reviewed them.
- Review each one before merging. Keep it short where the diff is small and its tests pass here; look harder at the unsafe code: riptide-pe's loader (#52), the stub code generation (#53), the write-guard exception handler (#59) and the page discarding (#60). Fix what you find on main after merging, and tell me what you found.
- Run the tests on this machine before each merge.
- Merge order:
  - Independent, base main: #48 (xtask policy), #49 (CI), #50 (setup doc), #57 (preflight checks), and #56 (statistics figures), which must be squash-merged: its first commit holds build output committed by mistake.
  - Loader stack, bottom first: #52 -> #53 -> #55 -> #58 -> #61 -> #62.
  - Memory stack, bottom first: #47 -> #59 -> #60.
- Expect small conflicts where two stacks touched the same file (README.md, Cargo.lock, Cargo.toml, .gitignore). Resolve them; do not drop either side.
- Delete the merged branches. Afterwards main must pass cargo fmt --all --check, clippy with -D warnings, cargo test --workspace and cargo xtask policy.

Then read the latest comment on each issue: every one has a status line and a "To run" block with exact commands. Do these in order, and show me the output of step 1 before building on it:

1. Census of the real title. Find the title's image in the game folder and run `cargo run -p riptide -- census <image>`. Save the output under local/ (never committed). Report: which modules it imports from; whether Sleep, WaitForSingleObject, CreateThread and the affinity calls come from kernel32.dll or another module; what the runtime-services imports look like (#15); whether it has thread-local data or delay-loaded imports (#51); and how the game folder is laid out (#34 assumes one .exe in the folder).
2. Issue #1: build and run riptide-probe, and record the answers to Q1 to Q5 in docs/measurements.md and the architecture doc, as the probe's plan says.
3. Issue #2: run scripts/record-session.ps1 against one of the existing runtimes in the open world, for Q11.
4. The owed runs, each in its issue's "To run" block: #4 (GPU test in riptide-mem), #13 (1 ms sleep), #12 (core layout; expect title 0x1555, riptide 0xeaaa), #34 (Play in the launcher with the stand-in title), #36 (look at the Checks tab, then set MEASURED_DRIVER from the first measurement).
5. Continue with what the census unblocks: fix the module names functions are bound under, then #11 (console memory functions), #15, #51, and on down the issue list.

Decisions still open; ask me, do not guess: #10 (a called stub stops the title instead of returning an error), #36 (the antivirus warning cannot be cleared), #54 (UI library, what the frame-rate cap is for, what goes in the menu), #39 license, #42.

Rules: numbers from this machine go in docs/measurements.md with the driver version. Stage files by path, never `git add -A`. No attribution lines in commits.
```

## Not on GitHub yet

The release-packaging work for issue #37 (an icon, version information, `scripts/package.ps1`) is on the laptop only, uncommitted, on a local branch `launcher/issue-37`. It built a 2.8 MiB package there. It was not yet checked from an empty folder or under Windows PowerShell 5.1.
