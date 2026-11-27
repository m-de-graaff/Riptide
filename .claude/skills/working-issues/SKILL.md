---
name: working-issues
description: Use when asked to do, build, work on or continue a Riptide GitHub issue — a pasted github.com/m-de-graaff/Riptide/issues link, "now #4", "do the next issue" — and when an issue needs the RX 7900 XTX, the 7800X3D, a real title or an existing runtime that this machine may not have.
---

# Working a Riptide issue

Riptide is built for one machine (Radeon RX 7900 XTX, Ryzen 7 7800X3D, 32 GB). Issues are
often worked on another one. What this machine can do decides what happens to the issue.

## 1. Read the issue and find its design

```powershell
gh issue view <n> --repo m-de-graaff/Riptide --json title,body,labels,comments
```

The body names its sources ("From:") and its finish line ("Done when:"). Read the design in
`docs/design/` and the plan in `docs/plans/` if it names them. A design or plan in the repo is
already approved: execute it. If there is none, state the design in the pull request and the
crate's README, under a heading that says these decisions were made without a design document.

## 2. Find out which machine this is

```powershell
Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion
(Get-CimInstance Win32_Processor).Name
```

It is the target machine only if the adapter is the RX 7900 XTX.

## 3. Sort the issue into one of three

Go through "Done when" line by line and ask of each: can this machine do it?

| What this machine can do | What happens |
| --- | --- |
| None of it | Reply "I can't do this right now", with one sentence saying what it needs. Build nothing. |
| Some of it | Build that part, verify it here, push it, and comment on the issue. |
| All of it | Build it, verify it, push it, and comment on the issue. |

Code, unit tests and tools that the target machine will run are things this machine can do.
Measurements on the GPU, sessions of a real title or of an existing runtime, and anything
recorded in `docs/measurements.md` are things only the target machine can do.

Numbers taken on another machine are a smoke test. They go in `local/`, are named as
non-target in the comment, and never enter `docs/measurements.md` or an answer to an open
question.

## 4. Build, verify, push

- One branch per issue, from `main`: `<area>/issue-<n>` (`probe/issue-1`, `trace/issue-3`).
- Verify with what the issue's crate has: `cargo test -p <crate>`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check -p <crate>`.
  A script is run for real, in PowerShell 7 and in Windows PowerShell 5.1.
- Commit, push, and open a pull request against `main`. Its body says `Closes #<n>` when every
  "Done when" line is met, and `Refs #<n>` when something is still open.
- Merging is the user's.

## 5. Comment on the issue

One comment, with these parts in this order:

1. **Status line**: `## Status: <built / partly built>, <what is still owed>`, then the branch
   and pull request.
2. **Done**: what exists and how it was verified, with the counts (tests passed, ignored).
3. **Open**: each "Done when" line not yet met, and who or what it waits for.
4. **Decisions or deviations**: anything decided without a design document, and every place
   the work departed from the plan.
5. **To run**: every command the user has to run, each in its own `powershell` code block with
   a line above it saying what it does and what to expect. Start with fetching the branch.

## This laptop's build environment

If `cargo build` fails with `LNK1104: cannot open file 'msvcrt.lib'`, the Visual Studio 18
install lacks the desktop C runtime libraries. Build inside the VS 2022 Build Tools
environment instead:

```powershell
cmd /c '"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" && cargo test -p <crate>'
```
