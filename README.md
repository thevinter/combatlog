# combatlog.dev

A privacy-conscious combat log uploader for [WarcraftLogs](https://www.warcraftlogs.com). No telemetry, no analytics, no ads. 

## Web UI (Self-Hosted)

### Requirements

- Docker & Docker Compose

### Local

```bash
docker compose -f docker-compose.local.yml up --build
```

Open [http://localhost:5050](http://localhost:5050) in your browser.

## CLI Script

### Requirements

- Python 3.10+
- Node.js 18+
- `curl_cffi` (`pip install curl_cffi`)

### Usage

```bash
python3 wcl-upload.py WoWCombatLog-041225_203000.txt \
  --email you@example.com \
  --password yourpass
```

### Options

| Flag | Default | Description |
|---|---|---|
| `--email` | *(required)* | WarcraftLogs email |
| `--password` | *(required)* | WarcraftLogs password |
| `--region` | `2` | 1=US, 2=EU, 3=KR, 4=TW, 5=CN |
| `--visibility` | `2` | 0=Public, 1=Private, 2=Unlisted |
| `--guild-id` | *none* | Guild ID to associate the report with |