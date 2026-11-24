# Windows setup

What VIctory needs on a Windows x64 machine, how to install it, and how to
check it. [`scripts/check-env.ps1`](../scripts/check-env.ps1) checks every
item below and prints the versions it finds; it changes nothing.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check-env.ps1
```

## Requirements

| Item | Why |
|------|-----|
| Windows 10 (2004 or later) or 11, x64 | the loader maps the title through the Windows loader |
| CPU with AVX2 | the title is built for the console's Zen 2. An AMD CPU from Zen 2 on runs its AMD-only instructions (`clzero`, `rdpru`, `mwaitx`, SSE4a, `clwb`) natively; elsewhere they are emulated by exception, which costs time |
| Direct3D 12 GPU with shader model 6.6 and mesh shaders | translated shaders are SM 6.6 DXIL; the world draws with mesh pipelines |
| 64-lane waves (AMD RDNA) | some console compute programs depend on the wave width; 32-lane GPUs need the emulated forms |
| 16 GB RAM or more (32 GB tested), several GiB of free commit at start | the title commits large pools at start; about 24 GB are in use in the world |
| Rust as pinned in `rust-toolchain.toml`, with clippy and rustfmt | build |
| MSVC x64 tools and a Windows 10/11 SDK | linker and system libraries for `x86_64-pc-windows-msvc` |
| `D3DCompiler_47.dll` (in Windows) | compiles the indirect-argument clamp shader at run time |
| Optional: Windows' "Graphics Tools" | the D3D12 debug layer (`VICTORY_D3D12_DEBUG`, `VICTORY_D3D12_GBV`) |
| Optional: `dxil.dll` / `dxc.exe` (in the SDK) | the DXIL validator tests and `victory translate --validate` use |

## Install

All from PowerShell with winget. Installing a package accepts its licence
terms: Rust is MIT/Apache-2.0, Visual Studio Build Tools is under Microsoft's
licence terms.

1. Rust, without a default toolchain: in this repository rustup installs the
   pinned toolchain (with clippy and rustfmt) the first time `cargo` runs.

   ```powershell
   winget install --id Rustlang.Rustup -e --source winget --override "-y --default-toolchain none --profile minimal --default-host x86_64-pc-windows-msvc"
   ```

2. MSVC and the Windows 11 SDK, nothing else (asks for elevation):

   ```powershell
   winget install --id Microsoft.VisualStudio.2022.BuildTools -e --source winget --override "--passive --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100"
   ```

3. The D3D12 debug layer, from an elevated PowerShell:

   ```powershell
   Add-WindowsCapability -Online -Name Tools.Graphics.DirectX~~~~0.0.1.0
   ```

4. This checkout: the asset-policy hook and the local configuration.

   ```powershell
   cargo xtask install-hooks
   copy victory.example.toml victory.toml
   ```

   Then set `game_dir` in `victory.toml` (git-ignored) to your dump. Every
   path in it must be outside the repository; caches and saves default to
   `%LOCALAPPDATA%\victory`.

Open a new terminal after installing, so that `PATH` has `%USERPROFILE%\.cargo\bin`.

## Reference machine

The setup this file was written and checked on (2026-11-24):

| Item | Version |
|------|---------|
| Windows | 11 Pro, build 26300 |
| CPU | AMD Ryzen 7 3700X (Zen 2) |
| GPU | AMD Radeon RX 9070 XT 16 GB, driver 32.0.32029.2010 |
| RAM | 32 GB, commit limit 59 GB |
| rustup / rustc | 1.29.3 / 1.98.1 |
| Visual Studio Build Tools | 2022, 17.14.45 |
| MSVC | 14.44.35207 |
| Windows SDK | 10.0.26100.0 (with `dxil.dll`, `dxc.exe`) |
| Graphics Tools | installed (`d3d12SDKLayers.dll` 10.0.26100.9818) |
