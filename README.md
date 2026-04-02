# zkscraper

Scrapes zero-k.info and gathers replays into snapshots.
Can auto-download missing maps and parses replays using spring-headless.

Currently, can't support maps that generate their terrain through scripts (Violet etc.), though this can be added if requested.

Credits to the original Zero-K scraper, which this was primarily based on:
https://github.com/esainane/zkstats

Todo:
- multiprocess parsing
- 'listen mode' for the web server to process replays as they come in, rather than bulk processing
- web server UI

## AI Disclaimer

Made with GPT-5.4 using Codex, mid-way through development (when I un-abandoned it). No AI generated artwork was used.

## Requirements

- Rust toolchain with `cargo`
- network access to `https://zero-k.info`
- a working local Zero-K portable install
- `mono` on Linux (Zero-K portable install already requires this)

Note: Linux/MacOS builds are untested.

## Build

```powershell
cargo build
```

This repo has multiple binaries:

- `zkscraper`: gather/download/parse pipeline
- `reader`: inspect parsed replay records
- `serve-db`: serve parsed replay records over HTTP with OpenAPI docs

## How to Use

The `pipeline` command is probably what you're looking for.

```powershell
cargo run --release --bin zkscraper -- pipeline `
  --initial-offset 0 `
  --gather-num 100 `
  --zk-path <zero-k install> `
  --out .\target\parsed-db
```

Supports `--temp <dir>` to stage intermediate files somewhere else.
If `--out` already exists, seeds the working DB from it before parsing so the pipeline updates the existing DB rather than starting from scratch.
If a stage fails, preserves the best available parsed DB state at `<out>_fail`.

After this, serve the database over a local web server: [Serve Parsed Data Over HTTP](#serve-parsed-data-over-http)

## 1. Gather Battle IDs

This writes a CSV with:

```text
Battle ID,Version,Map File,Map Extension
```

Example:

```powershell
cargo run --release --bin zkscraper -- gather-battle-ids `
  --initial-offset 0 `
  --gather-num 100 `
  --zk-path <zero-k install> `
  --out .\target\battles.csv
```

Notes:

- `--zk-path` is optional but recommended.
- when `--zk-path` is provided, gather first tries to match maps already installed in `<zk_path>\maps`
- if that guess is not reliable, gather falls back to battle/map page resolution
- the CSV stores the exact downloadable map archive stem and extension, not just the display map name

## 2. Download Maps

This reads the gathered CSV and downloads missing map archives into `<zk_path>\maps`.

```powershell
cargo run --release --bin zkscraper -- download-maps `
  --battle-ids .\target\battles.csv `
  --zk-path <zero-k install>
```

Behavior:

- skips maps already present in `<zk_path>\maps`
- uses the exact `Map File` + `Map Extension` from the CSV
- prints a line before each actual download, including size in MB when available
- falls back to battle/map page resolution if direct archive download fails

## 3. Download Replays

This downloads replay files and writes a replay manifest alongside them.

```powershell
cargo run --release --bin zkscraper -- download-replays `
  --battle-ids .\target\battles.csv `
  --sdfz-path .\target\replays
```

Outputs:

- replay files in `.\target\replays`
- `.\target\replays\replay_manifest.csv`

Manifest format:

```text
battle_id,replay_filename,game_version
```

## 4. Parse Replays

This runs Zero-K headless and stores parsed replays in a `sled` DB using CBOR, with `sled`'s built-in `zstd` compression enabled:

- key `<battle_id>` stores replay metadata, commands, events, and a frame index
- key `<battle_id>_frame_<frame>` stores the snapshots for that exact frame

```powershell
cargo run --release --bin zkscraper -- parse-replays `
  --sdfz-in .\target\replays `
  --zk-path <zero-k install> `
  --snapshot-path .\target\parsed-db
```

Behavior:

- reads `replay_manifest.csv`
- extracts replay metadata from the raw `.sdfz`
- injects a temporary capture widget into the Zero-K install
- runs `spring-headless`
- records global snapshots, per-allyteam FoW snapshots, events, and command history
- writes successful parses into the DB immediately
- skips battle IDs already present in the DB
- continues past per-replay failures and returns a summary error at the end if any replays failed

Important:

- required maps must already exist in `<zk_path>\maps`
- required engine versions must already exist in the host OS engine directory under `<zk_path>\engine\...\<engine_version>`
- parsing is sequential
- do not run multiple parser processes against the same Zero-K install

## Inspect Parsed Data

List stored battle IDs:

```powershell
cargo run --release --bin reader -- --db .\target\parsed-db list
```

Show a compact summary:

```powershell
cargo run --release --bin reader -- --db .\target\parsed-db show --battle-id 2392822
```

## Serve Parsed Data Over HTTP

Run a local read-only API server over the parsed replay DB:

```powershell
cargo run --release --bin serve-db -- --db .\target\parsed-db
```

Defaults:

- binds to `127.0.0.1:3000`
- OpenAPI JSON at `http://127.0.0.1:3000/openapi.json`
- Swagger UI at `http://127.0.0.1:3000/docs`

Optional bind override:

```powershell
cargo run --release --bin serve-db -- --db .\target\parsed-db --host 0.0.0.0 --port 8080
```

Optional map asset support from a local Zero-K install:

```powershell
cargo run --release --bin serve-db -- --db .\target\parsed-db --zk-path <zero-k install>
```

Available endpoints:

- `GET /healthz`
- `GET /replays?offset=0&limit=100`
- `GET /replays/{battle_id}`
- `GET /replays/{battle_id}/frames`
- `GET /replays/{battle_id}?snapshot_frame=240`
- `GET /maps`
- `GET /maps/{map_name}/heightmap.bmp`
- `GET /maps/{map_name}/features`

Response formats:

- replay and map-data endpoints now return `application/cbor`
- `GET /maps/{map_name}/heightmap.bmp` still returns `image/bmp`
- `GET /openapi.json` remains JSON and `GET /docs` remains HTML

Map asset behavior:

- `--zk-path` points at a Zero-K portable install and serves archives from `<zk_path>\maps`
- `GET /maps` lists all map names found in `<zk_path>\maps`
- heightmaps are served as `512x512` greyscale BMP images
- map features return JSON with `metal_spots` and placed `features`
- `.sdz` and `.sd7` map archives are supported
- `.sd7` extraction uses `ZKSCRAPER_7Z_PATH` when set; otherwise it defaults by OS:
  - Windows: `C:\Program Files\7-Zip\7z.exe`
  - Linux: `7z`
  - macOS: `7zz`
- replay JSON caching is disabled by default; set `ZKSCRAPER_REPLAY_JSON_CACHE_SIZE` to a small positive integer to enable an LRU cache by replay count

Replay lookup behavior:

- `GET /replays/{battle_id}` returns the full replay record
- `GET /replays/{battle_id}/frames` returns the ordered snapshot frame index for that replay
- `GET /replays/{battle_id}?snapshot_frame=<frame>` returns the same replay envelope with `global_snapshots`, `allyteam_snapshots`, and `economy_snapshots` filtered to that exact frame
- if the replay exists but no snapshot is present at that frame, the endpoint returns `404`
- full replay reads reconstruct the replay from metadata plus per-frame rows
- `snapshot_frame` lookups read the metadata row and one frame row directly

## Migrate Legacy Databases

Legacy databases that stored one `zstd`-compressed JSON blob per battle must be migrated before use.

```powershell
cargo run --release --bin zkscraper -- migrate-db `
  --src .\target\parsed-db-legacy `
  --dst .\target\parsed-db
```

Behavior:

- reads only the legacy top-level battle rows from `--src`
- writes the new CBOR metadata/frame layout to `--dst` with `sled` compression enabled
- refuses to write into an existing destination path

## Refresh New Databases

If a new-format DB needs its metadata counters refreshed, run:

```powershell
cargo run --release --bin zkscraper -- refresh-db `
  --db .\target\parsed-db
```

Behavior:

- recomputes metadata counters from frame rows
- rebuilds the replay-id index used for fast server startup
- deletes the legacy `replay_summaries` tree if it exists
- shows a progress bar while it runs

## Stored Replay Structure

Each replay is split across:

- `<battle_id>` metadata row
- zero or more `<battle_id>_frame_<frame>` snapshot rows

The metadata row stores:

- `battle_id`
- `replay_filename`
- `game_version`
- `engine_version`
- `map_name`
- `game_name`
- `zksearchtag`
- `players`
- `teams`
- `map_size`
- `command_history`
- `events`
- `springie_stats`
- `snapshot_frames`

Each frame row stores:

- `frame`
- `global_snapshot`
- `allyteam_snapshots`
- `economy_snapshots`

### Global Snapshots

`global_snapshots` is the full-world unit snapshot stream.

Each snapshot contains:

- `frame`
- `game_seconds`
- `units`

Each unit snapshot contains:

- `unit_id`
- `unit_def_name`
- `team_id`
- `allyteam_id`
- `x`, `y`, `z`
- `hp`
- `max_hp`
- `build_progress`
- `heading`
- `experience`

### Per-Allyteam Snapshots

`allyteam_snapshots` is a map keyed by allyteam ID.

Each allyteam snapshot entry contains:

- `allyteam_id`
- `frame`
- `game_seconds`
- `los_units`
- `radar_contacts`

This means different allyteams can have different snapshot contents for the same frame.

`radar_contacts` intentionally do not include unit type or health. They only contain:

- `unit_id`
- `team_id`
- `allyteam_id`
- `x`, `y`, `z`

### Command History

`command_history` is extracted offline from the raw demo stream.

Each command record contains raw fields:

- `frame`
- `game_seconds`
- `player_id`
- `ai_id`
- `command_id`
- `options`
- `params`
- `selected_unit_ids`

and a decoded command object:

- `kind`
- `option_flags`
- `target`
- `state`
- `build`
- `inserted`
- `removed`

Supported decoding includes:

- standard Spring commands such as move, fight, attack, patrol, reclaim, repair, load, unload, stop, wait, repeat, fire state, move state
- negative build command IDs
- several common Zero-K custom commands such as `raw_move`, `raw_build`, `rearm`, `retreat`, `priority`, `morph`, and `jump`
- AI command packets from the raw demo stream

Unknown custom command IDs are still preserved as raw command records.

## Operational Notes

- `parse-replays` stores successful replays even if later replays fail
- rerunning parse will skip already-stored battle IDs
- the parser uses watchdog logic around `spring-headless` to kill stuck or broken runs
- `pipeline` keeps partial parsed DB output at `<out>_fail` when a stage fails

## Troubleshooting

If `parse-replays` fails immediately:

- confirm local widgets are enabled in `ZK_data.lua`
- confirm the required map exists in `<zk_path>\maps`
- confirm the required engine exists under `<zk_path>\engine` for your host OS and replay engine version

If `spring-headless` exits on a replay:

- inspect `<zero-k install>\infolog.txt`
- check whether the replay's map is missing locally
- rerun `download-maps` for the same battle CSV if needed

If `download-maps` reports failures:

- regenerate the battle CSV with the current `gather-battle-ids`
- then rerun `download-maps`

If `pipeline` fails:

- inspect the preserved DB at `<out>_fail` if it was created
- rerun the exact command printed by `pipeline`
- on a parse-stage failure, use the printed `parse-replays` command with `--snapshot-path <out>_fail` to continue from the preserved partial DB

If you want to inspect stored replay records directly:

- use the `reader` binary first
- the DB values are CBOR stored in `sled`, with `sled` applying `zstd` compression at the DB level
