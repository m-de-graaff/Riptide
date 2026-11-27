# Developer setup

What a Windows 11 x64 machine needs to build and test Riptide, and the commands to do it. Any such machine can build and test; only the target machine (RX 7900 XTX, Ryzen 7 7800X3D) can take measurements.

## 1. Install the tools

Git:

```powershell
winget install Git.Git
```

Rustup, which installs and selects Rust toolchains:

```powershell
winget install Rustlang.Rustup
```

The MSVC linker, the C++ x64 libraries and the Windows SDK. Rust links with them:

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools --override "--passive --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

If Visual Studio is already installed, open the Visual Studio Installer instead, choose Modify, and tick the workload "Desktop development with C++". It must include "MSVC x64/x86 build tools" and a Windows 11 SDK.

Open a new terminal afterwards, so `git`, `cargo` and `rustup` are on the path.

## 2. Get the source and build

```powershell
git clone https://github.com/m-de-graaff/Riptide.git
cd Riptide
cargo test --workspace
```

The first `cargo` command downloads the pinned toolchain. `rust-toolchain.toml` names it (Rust 1.97.1, with rustfmt and clippy), so there is nothing to choose. The first build takes a few minutes, most of it the launcher's window library.

Expect every test to pass and a few to be ignored. The ignored ones need the target machine's GPU.

## 3. If linking fails with `cannot open file 'msvcrt.lib'`

```text
LINK : fatal error LNK1104: cannot open file 'msvcrt.lib'
```

Rust found a Visual Studio install that has the linker but not the C++ x64 libraries. There are two ways out:

- Add the workload from step 1 to that install.
- Or, if another install on the machine has the libraries (the 2022 Build Tools from step 1, for example), run cargo inside its environment:

```powershell
cmd /c '"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" && cargo test --workspace'
```

## 4. The checks to run before pushing

CI runs the same four.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask policy
```

`cargo xtask policy` fails if a tracked file looks like a game, console or SDK file. To have git run it before every commit, turn the hook on once per checkout:

```powershell
cargo xtask hooks
```

## 5. Run the launcher

```powershell
cargo run -p riptide-launcher
```

## 6. `cargo build-7800x3d`: only on the target machine

```powershell
cargo build-7800x3d
```

This is the release build for the Ryzen 7 7800X3D: `target-cpu=znver4` with AVX-512, and full link-time optimization. Use it on the target machine, for anything that is measured.

Do not use it anywhere else. The programs it builds crash with an illegal-instruction error on a CPU without Zen 4's instructions, which includes most laptops. Plain `cargo build`, `cargo test` and `cargo build --release` stay portable and are right for every other machine and for CI.

## Where things are kept

| Path | What |
| --- | --- |
| `target/` | Build output. Not tracked. |
| `local/` | Logs, captures and numbers from this machine. Not tracked; they can contain game data. |
| `%APPDATA%\Riptide\riptide.toml` | The launcher's settings. |
| `%LOCALAPPDATA%\Riptide\logs` | Session logs. |
