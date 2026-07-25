# combatlog.dev

A privacy-conscious combat log uploader for all RPGLogs games. No telemetry, no analytics, no ads.

Supported websites:

- [WarcraftLogs](https://www.warcraftlogs.com)
- [FFLogs](https://www.fflogs.com/)
- [ESOLogs](https://www.esologs.com/)
- [SWTORLogs](https://www.swtorlogs.com/)
- [FellowshipLogs](https://www.fellowshiplogs.com/)

## Desktop app

This is the easiest option if you just want to upload logs from your own machine.

You can find the installer for your OS on the [Releases](../../releases) page:

- **Windows** — `.msi` or `.exe` installer
- **Linux** — `.deb`, `.rpm`, or `.AppImage`

Credentials stay in local storage on your machine.

## Web UI (self-hosted)

**Requirements:** Docker + Docker Compose.

```bash
git clone git@github.com:thevinter/combatlog.git
cd combatlog
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

| Flag           | Default      | Description                                                                                   |
| -------------- | ------------ | --------------------------------------------------------------------------------------------- |
| `--email`      | _(required)_ | Account email                                                                                 |
| `--password`   | _(required)_ | Account password                                                                              |
| `--game`       | `warcraft`   | `warcraft`, `ff`, `eso`, `swtor`, or `fellowship`                                             |
| `--region`     | `2`          | WoW: 1=US, 2=EU, 3=KR, 4=TW, 5=CN. Other games might use their own codes (usually 1=NA, 2=EU) |
| `--visibility` | `2`          | 0=Public, 1=Private, 2=Unlisted                                                               |
| `--guild-id`   | _none_       | Guild ID to associate the report with                                                         |

## Building the desktop app from source

If you want to build yourself instead of downloading a release:

```bash
cd app
bash scripts/prepare-sidecar.sh            # downloads the Node sidecar for your host
cargo tauri icon src-tauri/icons/icon.png  # first time only
cargo tauri build
```

Needs Rust (stable) and, on Linux, the usual webkit2gtk dev packages.
