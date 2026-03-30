# zkscraper

Tools for:

- gathering Zero-K battle IDs
- downloading required maps
- downloading replay `.sdfz` files
- parsing replays through a local Zero-K install into a compressed `sled` DB
- optionally backfilling command history into an existing parsed DB from raw replays only

## Requirements

- Windows
- Rust toolchain with `cargo`
- network access to `https://zero-k.info`
- a working local Zero-K portable install

## Zero-K Setup

`parse-replays` injects a temporary local widget into the Zero-K install and runs `spring-headless` against that install.

Local widgets must be enabled in:

`<zero-k install>\LuaUI\Config\ZK_data.lua`

Required settings:

```lua
useLocalWidgets = true
useLocalWidgetsFirst = true
```

## Build

```powershell
cargo build
```

This repo has multiple binaries:

- `zkscraper`: gather/download/parse pipeline
- `reader`: inspect parsed replay records
- `serve-db`: serve parsed replay records over HTTP with OpenAPI docs

## Full Pipeline

Typical end-to-end flow:

1. `gather-battle-ids`
2. `download-maps`
3. `download-replays`
4. `parse-replays`

Optional for old DBs only:

5. `backfill-commands`

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

This runs Zero-K headless and stores one compressed JSON record per battle ID in a `sled` DB.

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
- required engine versions must already exist in `<zk_path>\engine\win64\<engine_version>`
- parsing is sequential
- do not run multiple parser processes against the same Zero-K install

## 5. Backfill Commands

Use this only for replay records that were parsed before command extraction was added.

This does not launch Zero-K headless. It reads raw `.sdfz` files and updates existing DB entries in place.

```powershell
cargo run --release --bin zkscraper -- backfill-commands `
  --sdfz-in .\target\replays `
  --zk-path <zero-k install> `
  --snapshot-path .\target\parsed-db
```

Notes:

- newly parsed replays already include `command_history`
- `--zk-path` is optional for backfill, but recommended
- with `--zk-path`, build commands can be enriched with Zero-K unit names from the installed game archive

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

Available endpoints:

- `GET /healthz`
- `GET /replays?offset=0&limit=100`
- `GET /replays/{battle_id}`

## Stored Replay Structure

Each replay record stores:

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
- `global_snapshots`
- `allyteam_snapshots`
- `command_history`
- `events`
- `springie_stats`

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
- `backfill-commands` updates existing records in place
- the parser uses watchdog logic around `spring-headless` to kill stuck or broken runs

## Troubleshooting

If `parse-replays` fails immediately:

- confirm local widgets are enabled in `ZK_data.lua`
- confirm the required map exists in `<zk_path>\maps`
- confirm the required engine exists in `<zk_path>\engine\win64\<engine_version>`

If `spring-headless` exits on a replay:

- inspect `<zero-k install>\infolog.txt`
- check whether the replay's map is missing locally
- rerun `download-maps` for the same battle CSV if needed

If `download-maps` reports failures:

- regenerate the battle CSV with the current `gather-battle-ids`
- then rerun `download-maps`

If you want to inspect stored replay records directly:

- use the `reader` binary first
- the DB values are `zstd`-compressed JSON stored in `sled`
