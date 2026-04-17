# combatlog.dev

A privacy-conscious combat log uploader for [WarcraftLogs](https://www.warcraftlogs.com). No telemetry, no analytics, no ads.

## Desktop app

The easiest option if you just want to upload logs from your own machine. Grab the installer for your OS from the [Releases](../../releases) page:

- **Windows** — `.msi` installer
- **Linux** — `.deb`, `.rpm`, or `.AppImage`

Credentials stay in local storage on your machine.

## Web UI (self-hosted)

**Requirements:** Docker + Docker Compose.

```bash
docker compose -f docker-compose.local.yml up --build
```

Then open [http://localhost:5050](http://localhost:5050).

## CLI

**Requirements:**
- Python 3.10+
- Node.js 18+
- `curl_cffi` (`pip install curl_cffi`)

**Usage:**

```bash
python3 wcl-upload.py WoWCombatLog-041225_203000.txt \
  --email you@example.com \
  --password yourpass
```

**Options:**

| Flag | Default | Description |
|---|---|---|
| `--email` | *(required)* | WarcraftLogs email |
| `--password` | *(required)* | WarcraftLogs password |
| `--region` | `2` | 1=US, 2=EU, 3=KR, 4=TW, 5=CN |
| `--visibility` | `2` | 0=Public, 1=Private, 2=Unlisted |
| `--guild-id` | *none* | Guild ID to associate the report with |

## Building the desktop app from source

If you want to build yourself instead of downloading a release:

```bash
cd app
bash scripts/prepare-sidecar.sh            # downloads the Node sidecar for your host
cargo tauri icon src-tauri/icons/icon.png  # first time only
cargo tauri build
```

Needs Rust (stable) and, on Linux, the usual webkit2gtk dev packages.

### Windows (dev mode)

Prereqs:

- [Rust](https://rustup.rs) — `rustup-init.exe`, default `stable-x86_64-pc-windows-msvc` toolchain
- Visual Studio Build Tools with the **Desktop development with C++** workload (provides the MSVC linker)
- WebView2 Runtime (preinstalled on Windows 11 / 10 22H2+)
- `cmake` — `boring-sys` (transitive dep via `rquest`) needs it. `winget install Kitware.CMake`
- `nasm` and Strawberry Perl if the BoringSSL build complains. `winget install NASM.NASM StrawberryPerl.StrawberryPerl`
- `cargo install tauri-cli --version "^2"`

From a PowerShell prompt:

```powershell
cd app
powershell -ExecutionPolicy Bypass -File scripts\prepare-sidecar.ps1
cargo tauri dev
```

Run from native Windows (not WSL) — the overlay needs to draw on top of the Windows WoW client.
