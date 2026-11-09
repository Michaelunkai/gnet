<div align="center">

# ⚙️ gnet

### The self-healing .NET installer for Windows — one native Rust executable, every .NET version, verified on disk.

![Rust](https://img.shields.io/badge/Rust-std--only-orange?logo=rust)
![Platform](https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?logo=windows)
![.NET](https://img.shields.io/badge/.NET-2.1%20→%20latest-512BD4?logo=dotnet)
![Dependencies](https://img.shields.io/badge/crates-0-brightgreen)
![License](https://img.shields.io/badge/license-MIT-blue)

</div>

---

## Why gnet exists

Installing a whole range of .NET runtimes and SDKs on a real-world machine fails in
ways the installers will not tell you about:

| Symptom | Real cause |
|---|---|
| `Successfully installed`, yet `dotnet --list-runtimes` shows nothing | The bundle is *registered* but its files are gone — Burn treats the install as a no-op |
| Installers exit `3` / `1603` | `C:\Program Files\dotnet` is a **dangling junction** to a folder that no longer exists |
| Every version "not visible" after the run | Verification asked a *portable* `dotnet.exe` that only sees its own folder |
| Exit `0` reported while the inner installer failed | winget wraps the installer's exit code |

Scripts that trust exit codes report success while nothing is installed. **gnet never trusts an exit code.**

## How it works

```
                ┌──────────────────────────────────────────┐
                │ 1. heal the machine                       │
                │    dangling junctions · bad DOTNET_ROOT   │
                └────────────────────┬─────────────────────┘
                                     ▼
                ┌──────────────────────────────────────────┐
                │ 2. discover the live catalog from winget  │
                │    Runtime · Desktop · ASP.NET · SDK ·    │
                │    Hosting Bundle · Framework  (any ver.) │
                └────────────────────┬─────────────────────┘
                                     ▼
                ┌──────────────────────────────────────────┐
                │ 3. check the REAL filesystem              │
                │    shared\<fw>\<ver>  ·  sdk\<ver>        │
                └────────────────────┬─────────────────────┘
                                     ▼  only what is missing
   winget --force ─► direct installer ─► /repair ─► /uninstall ─► /install
                                     │   (stop at the first rung that puts files on disk)
                                     ▼
                ┌──────────────────────────────────────────┐
                │ 4. verify again · report · exit code      │
                └──────────────────────────────────────────┘
```

* **Future-proof** — package IDs are parsed from `winget search` at run time, so .NET 11, 12, … are picked up with zero code changes.
* **Truth from disk** — a target is "installed" only when its files exist under a dotnet root.
* **Escalation ladder** — five different strategies per target, with retries on `1618` (another install running).
* **No surprise reboots** — a guard thread aborts any installer-initiated shutdown; gnet only *reports* `3010`.
* **Zero dependencies** — pure `std` Rust, a ~300 KB executable, shells out only to Windows' own `winget`, `reg`, `curl` and `cmd`.
* **Self-elevating** — relaunches itself as administrator when needed.

## Usage

```powershell
gnet                 # heal everything that is missing or broken
gnet --scan-only     # report only, change nothing
gnet --force         # reinstall every target even if healthy
gnet --ids Microsoft.DotNet.SDK.9,Microsoft.DotNet.Runtime.9
gnet --no-winget     # direct-download ladder only
gnet --timeout 3600  # per-installer timeout in seconds
```

Exit codes: `0` everything verified · `3010` verified, reboot pending · `1` something could not be healed.

Reports are written to `Documents\WindowsPowerShell\Gnet\Reports\gnet-<timestamp>.log`.

## Build

```powershell
cd rust
cargo build --release
```

The binary is `target\release\gnet.exe`.

## What it deliberately skips

* **Hosting Bundles** when IIS is not enabled (they require it).
* **.NET Framework 1.1** (cannot exist on 64-bit Windows).
* Preview / RC / Insiders catalog IDs.

## License

MIT
