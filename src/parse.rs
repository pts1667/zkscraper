use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use fastapi::ToSchema;
use flate2::read::GzDecoder;
use indicatif::ProgressBar;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{
    process::{Child, Command},
    time::{sleep, Duration, Instant},
};

const MANIFEST_FILENAME: &str = "replay_manifest.csv";
const WIDGET_LINK_NAME: &str = "zkscraper_replay_snapshot.lua";
const WIDGET_DISPLAY_NAME: &str = "ZKScraper Replay Snapshot";
const ZK_CAPTURE_ROOT: &str = "LuaUI/Logs/zkscraper";
const SNAPSHOT_FRAMES: u32 = 120;
const WATCHDOG_POLL_MS: u64 = 250;
const WATCHDOG_TOTAL_TIMEOUT_SECS: u64 = 300;
const WATCHDOG_IDLE_TIMEOUT_SECS: u64 = 20;
const WATCHDOG_EOF_GRACE_SECS: u64 = 15;

pub struct ParseReplaySettings {
    pub sdfz_in: PathBuf,
    pub zk_path: PathBuf,
    pub snapshot_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ReplayManifestEntry {
    battle_id: u64,
    replay_filename: String,
    game_version: String,
}

#[derive(Debug)]
struct ReplayDemInfo {
    engine_version: String,
    script: String,
    springie_stats: Vec<String>,
    command_history: Vec<CommandRecord>,
}

#[derive(Debug, Default)]
struct ScriptMetadata {
    map_name: Option<String>,
    game_name: Option<String>,
    zksearchtag: Option<String>,
    players: Vec<PlayerMetadata>,
    teams: Vec<TeamMetadata>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ParsedReplay {
    pub battle_id: u64,
    pub replay_filename: String,
    pub game_version: String,
    pub engine_version: String,
    pub map_name: Option<String>,
    pub game_name: Option<String>,
    pub zksearchtag: Option<String>,
    pub players: Vec<PlayerMetadata>,
    pub teams: Vec<TeamMetadata>,
    pub map_size: Option<MapSize>,
    pub global_snapshots: Vec<SnapshotRecord>,
    pub allyteam_snapshots: BTreeMap<u32, Vec<AllyTeamSnapshotRecord>>,
    #[serde(default, alias = "player_snapshots")]
    pub economy_snapshots: BTreeMap<u32, Vec<EconomySnapshotRecord>>,
    pub command_history: Vec<CommandRecord>,
    pub events: Vec<EventRecord>,
    pub springie_stats: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PlayerMetadata {
    pub player_id: u32,
    pub name: Option<String>,
    pub team: Option<u32>,
    pub spectator: bool,
    pub elo: Option<i32>,
    pub lobby_id: Option<u64>,
    pub country_code: Option<String>,
    pub clan: Option<String>,
    pub level: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct TeamMetadata {
    pub team_id: u32,
    pub allyteam: Option<u32>,
    pub teamleader: Option<u32>,
    pub handicap: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct MapSize {
    pub x: u32,
    pub z: u32,
}

#[derive(Debug, Deserialize)]
struct WidgetMeta {
    map_size: MapSize,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SnapshotRecord {
    pub frame: u32,
    pub game_seconds: f32,
    pub units: Vec<UnitSnapshot>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct AllyTeamSnapshotRecord {
    pub allyteam_id: u32,
    pub frame: u32,
    pub game_seconds: f32,
    pub los_units: Vec<UnitSnapshot>,
    pub radar_contacts: Vec<RadarContact>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EconomySnapshotRecord {
    pub team_id: i32,
    pub allyteam_id: i32,
    pub frame: u32,
    pub game_seconds: f32,
    pub economy: EconomySnapshot,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EconomySnapshot {
    pub metal_income: f32,
    pub energy_income: f32,
    pub metal_stored: f32,
    pub energy_stored: f32,
    pub metal_storage: f32,
    pub energy_storage: f32,
    pub metal_pull: f32,
    pub energy_pull: f32,
    pub metal_expense: f32,
    pub energy_expense: f32,
    pub metal_share: f32,
    pub energy_share: f32,
    pub metal_sent: f32,
    pub energy_sent: f32,
    pub metal_received: f32,
    pub energy_received: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct UnitSnapshot {
    pub unit_id: u32,
    pub unit_def_name: String,
    pub team_id: i32,
    pub allyteam_id: i32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub hp: f32,
    pub max_hp: f32,
    pub build_progress: f32,
    pub heading: i32,
    pub experience: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RadarContact {
    pub unit_id: u32,
    pub team_id: i32,
    pub allyteam_id: i32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EventRecord {
    pub event_type: String,
    pub frame: u32,
    pub game_seconds: f32,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct CommandRecord {
    pub frame: u32,
    pub game_seconds: f32,
    pub player_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_id: Option<u32>,
    pub command_id: i32,
    pub options: u32,
    pub params: Vec<f32>,
    pub selected_unit_ids: Vec<i32>,
    pub decoded: DecodedCommand,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct DecodedCommand {
    pub kind: String,
    pub option_flags: CommandOptionFlags,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<DecodedTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub inserted: Option<InsertedCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<RemovedCommand>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct CommandOptionFlags {
    pub alt: bool,
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
    pub right: bool,
    pub internal: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(tag = "type")]
pub enum DecodedTarget {
    Unit { unit_id: i32 },
    Position { x: f32, y: f32, z: f32 },
    Area { x: f32, y: f32, z: f32, radius: f32 },
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct BuildCommand {
    pub unit_def_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facing: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct InsertedCommand {
    pub index: i32,
    pub command_id: i32,
    pub options: u32,
    pub params: Vec<f32>,
    #[schema(no_recursion)]
    pub decoded: Box<DecodedCommand>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RemovedCommand {
    pub params: Vec<f32>,
    pub mode: String,
}

pub async fn parse_replays(
    settings: ParseReplaySettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = settings.zk_path.join("LuaUI").join("Config");
    let swapped_configs = activate_scraper_configs(&config_dir)?;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_signal = interrupted.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupted_signal.store(true, Ordering::SeqCst);
        }
    });

    let parse_future = async {
        validate_local_widgets_enabled(&settings.zk_path)?;

        let manifest = read_manifest(&settings.sdfz_in.join(MANIFEST_FILENAME))?;
        let db = sled::open(&settings.snapshot_path)?;
        let temp_root = settings.snapshot_path.join("_tmp");
        let zk_capture_root = settings.zk_path.join(ZK_CAPTURE_ROOT);
        fs::create_dir_all(&temp_root)?;
        fs::create_dir_all(&zk_capture_root)?;

        let widget_source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("zk")
            .join("Widgets")
            .join("replay_snapshot.lua");
        let widget_target = settings
            .zk_path
            .join("LuaUI")
            .join("Widgets")
            .join(WIDGET_LINK_NAME);

        let pb = ProgressBar::new(manifest.len() as u64);
        let mut failures = Vec::new();
        for entry in manifest {
            if interrupted.load(Ordering::SeqCst) {
                return Err("parse interrupted by Ctrl+C".into());
            }

            let key = entry.battle_id.to_string();
            if db.contains_key(key.as_bytes())? {
                pb.inc(1);
                continue;
            }

            let replay_path = settings.sdfz_in.join(&entry.replay_filename);
            if !replay_path.is_file() {
                eprintln!(
                    "Parse failed for battle {}: replay file from manifest does not exist: {}",
                    entry.battle_id,
                    replay_path.display()
                );
                failures.push(format!("{} ({})", entry.battle_id, entry.replay_filename));
                pb.inc(1);
                continue;
            }

            let replay_outcome: Result<(), Box<dyn std::error::Error>> = async {
                let mut dem_info = read_dem_info(&replay_path)?;
                let mut unit_name_cache = HashMap::new();
                enrich_command_history_with_unit_names(
                    &mut dem_info.command_history,
                    &dem_info.script,
                    Some(&settings.zk_path),
                    &mut unit_name_cache,
                )?;
                let script_metadata = parse_game_script(&dem_info.script)?;
                let headless = resolve_engine_binary(&settings.zk_path, &dem_info.engine_version)?;

                let replay_temp_dir = temp_root.join(entry.battle_id.to_string());
                let capture_dir = zk_capture_root.join(entry.battle_id.to_string());
                fs::create_dir_all(&capture_dir)?;
                let config_path = replay_temp_dir.join("springsettings-headless.cfg");

                let replay_result = run_single_replay(
                    &widget_source,
                    &widget_target,
                    &config_path,
                    &capture_dir,
                    &headless,
                    &settings.zk_path,
                    &replay_path,
                    entry.battle_id,
                    interrupted.clone(),
                )
                .await;

                let cleanup_result =
                    cleanup_replay_artifacts(&widget_target, &replay_temp_dir, &capture_dir);
                let (
                    widget_meta,
                    global_snapshots,
                    allyteam_snapshots,
                    economy_snapshots,
                    events,
                ) =
                    replay_result?;
                cleanup_result?;

                let parsed = ParsedReplay {
                    battle_id: entry.battle_id,
                    replay_filename: entry.replay_filename.clone(),
                    game_version: entry.game_version.clone(),
                    engine_version: dem_info.engine_version,
                    map_name: script_metadata.map_name,
                    game_name: script_metadata.game_name,
                    zksearchtag: script_metadata.zksearchtag,
                    players: script_metadata.players,
                    teams: script_metadata.teams,
                    map_size: Some(widget_meta.map_size),
                    global_snapshots,
                    allyteam_snapshots,
                    economy_snapshots,
                    command_history: dem_info.command_history,
                    events,
                    springie_stats: dem_info.springie_stats,
                };

                let payload = serde_json::to_vec(&parsed)?;
                let compressed = zstd::encode_all(payload.as_slice(), 3)?;
                db.insert(key.as_bytes(), compressed)?;
                db.flush()?;
                Ok(())
            }
            .await;

            if let Err(err) = replay_outcome {
                eprintln!(
                    "Parse failed for battle {} ({}): {}",
                    entry.battle_id, entry.replay_filename, err
                );
                failures.push(format!("{} ({})", entry.battle_id, entry.replay_filename));
            }
            pb.inc(1);
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "failed to parse {} replay(s): {}",
                failures.len(),
                failures.join(", ")
            )
            .into())
        }
    };

    let parse_result: Result<(), Box<dyn std::error::Error>> = parse_future.await;
    signal_task.abort();

    let restore_result = restore_scraper_configs(&swapped_configs);
    match (parse_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(parse_err), Ok(())) => Err(parse_err),
        (Ok(()), Err(restore_err)) => Err(restore_err),
        (Err(parse_err), Err(restore_err)) => Err(format!(
            "parse failed: {parse_err}; also failed to restore scraper configs: {restore_err}"
        )
        .into()),
    }
}

pub async fn backfill_commands(
    sdfz_in: PathBuf,
    snapshot_path: PathBuf,
    zk_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = read_manifest(&sdfz_in.join(MANIFEST_FILENAME))?;
    let db = sled::open(&snapshot_path)?;
    let pb = ProgressBar::new(manifest.len() as u64);
    let mut failures = Vec::new();
    let mut unit_name_cache: HashMap<String, HashMap<u32, String>> = HashMap::new();

    for entry in manifest {
        let key = entry.battle_id.to_string();
        let Some(existing) = db.get(key.as_bytes())? else {
            pb.inc(1);
            continue;
        };

        let replay_path = sdfz_in.join(&entry.replay_filename);
        if !replay_path.is_file() {
            eprintln!(
                "Command backfill failed for battle {}: replay file from manifest does not exist: {}",
                entry.battle_id,
                replay_path.display()
            );
            failures.push(format!("{} ({})", entry.battle_id, entry.replay_filename));
            pb.inc(1);
            continue;
        }

        let backfill_result: Result<(), Box<dyn std::error::Error>> = (|| {
            let mut dem_info = read_dem_info(&replay_path)?;
            enrich_command_history_with_unit_names(
                &mut dem_info.command_history,
                &dem_info.script,
                zk_path.as_deref(),
                &mut unit_name_cache,
            )?;
            let decompressed = zstd::decode_all(existing.as_ref())?;
            let mut parsed: serde_json::Value = serde_json::from_slice(&decompressed)?;
            parsed["command_history"] = serde_json::to_value(&dem_info.command_history)?;
            let payload = serde_json::to_vec(&parsed)?;
            let compressed = zstd::encode_all(payload.as_slice(), 3)?;
            db.insert(key.as_bytes(), compressed)?;
            db.flush()?;
            Ok(())
        })();

        if let Err(err) = backfill_result {
            eprintln!(
                "Command backfill failed for battle {} ({}): {}",
                entry.battle_id, entry.replay_filename, err
            );
            failures.push(format!("{} ({})", entry.battle_id, entry.replay_filename));
        }

        pb.inc(1);
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to backfill commands for {} replay(s): {}",
            failures.len(),
            failures.join(", ")
        )
        .into())
    }
}

async fn run_single_replay(
    widget_source: &Path,
    widget_target: &Path,
    config_path: &Path,
    capture_dir: &Path,
    headless: &Path,
    zk_path: &Path,
    replay_path: &Path,
    battle_id: u64,
    interrupted: Arc<AtomicBool>,
) -> Result<
    (
        WidgetMeta,
        Vec<SnapshotRecord>,
        BTreeMap<u32, Vec<AllyTeamSnapshotRecord>>,
        BTreeMap<u32, Vec<EconomySnapshotRecord>>,
        Vec<EventRecord>,
    ),
    Box<dyn std::error::Error>,
> {
    if interrupted.load(Ordering::SeqCst) {
        return Err("parse interrupted by Ctrl+C".into());
    }

    install_widget(widget_source, widget_target)?;
    let absolute_zk_path = absolute_path(zk_path)?;
    let absolute_config_path = absolute_path(config_path)?;
    let absolute_replay_path = absolute_path(replay_path)?;
    let engine_dir = headless
        .parent()
        .ok_or("could not resolve spring-headless parent directory")?;
    let spring_data = vec![absolute_path(engine_dir)?];
    write_headless_config(config_path, zk_path, capture_dir, battle_id, &spring_data)?;
    let infolog_path = absolute_zk_path.join("infolog.txt");
    let log_offset = fs::metadata(&infolog_path)
        .map(|meta| meta.len())
        .unwrap_or(0);

    let mut child = Command::new(headless)
        .current_dir(&absolute_zk_path)
        .arg("-isolation-dir")
        .arg(&absolute_zk_path)
        .arg("-write-dir")
        .arg(&absolute_zk_path)
        .arg("-config")
        .arg(&absolute_config_path)
        .arg("-only-local")
        .arg(&absolute_replay_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let status = wait_for_headless(
        &mut child,
        &infolog_path,
        log_offset,
        capture_dir,
        battle_id,
        &interrupted,
    )
    .await?;
    if !status.success() {
        return Err(format!(
            "spring-headless exited with status {} for replay {}",
            status,
            absolute_replay_path.display()
        )
        .into());
    }

    let meta_path = capture_dir.join("meta.json");
    let global_snapshots_path = capture_dir.join("global_snapshots.jsonl");
    let allyteam_snapshots_path = capture_dir.join("allyteam_snapshots.jsonl");
    let economy_snapshots_path = capture_dir.join("economy_snapshots.jsonl");
    let events_path = capture_dir.join("events.jsonl");

    if !meta_path.is_file()
        || !global_snapshots_path.is_file()
        || !allyteam_snapshots_path.is_file()
        || !economy_snapshots_path.is_file()
        || !events_path.is_file()
    {
        return Err(format!(
            "headless capture did not produce the expected files under {}",
            capture_dir.display()
        )
        .into());
    }

    let widget_meta: WidgetMeta = serde_json::from_slice(&fs::read(&meta_path)?)?;
    let global_snapshots = read_jsonl::<SnapshotRecord>(&global_snapshots_path)?;
    let allyteam_snapshot_rows = read_jsonl::<AllyTeamSnapshotRecord>(&allyteam_snapshots_path)?;
    let economy_snapshot_rows = read_jsonl::<EconomySnapshotRecord>(&economy_snapshots_path)?;
    let events = read_jsonl::<EventRecord>(&events_path)?;
    let mut allyteam_snapshots: BTreeMap<u32, Vec<AllyTeamSnapshotRecord>> = BTreeMap::new();
    for snapshot in allyteam_snapshot_rows {
        allyteam_snapshots
            .entry(snapshot.allyteam_id)
            .or_default()
            .push(snapshot);
    }
    let mut economy_snapshots: BTreeMap<u32, Vec<EconomySnapshotRecord>> = BTreeMap::new();
    for snapshot in economy_snapshot_rows {
        economy_snapshots
            .entry(snapshot.team_id as u32)
            .or_default()
            .push(snapshot);
    }

    Ok((
        widget_meta,
        global_snapshots,
        allyteam_snapshots,
        economy_snapshots,
        events,
    ))
}

async fn wait_for_headless(
    child: &mut Child,
    infolog_path: &Path,
    log_offset: u64,
    capture_dir: &Path,
    battle_id: u64,
    interrupted: &AtomicBool,
) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
    let started_at = Instant::now();
    let mut last_progress = Instant::now();
    let mut replay_started = false;
    let mut widget_loaded = false;
    let mut eof_seen_at: Option<Instant> = None;
    let mut log_offset = log_offset;
    let mut log_fragment = String::new();
    let mut capture_bytes = capture_size(capture_dir);

    loop {
        if interrupted.load(Ordering::SeqCst) {
            kill_child(child).await?;
            return Err("parse interrupted by Ctrl+C".into());
        }

        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        let mut saw_log_progress = false;
        for line in read_infolog_lines(infolog_path, &mut log_offset, &mut log_fragment)? {
            saw_log_progress = true;
            if line.contains("Loaded widget:  ZKScraper Replay Snapshot")
                || line.contains("<zkscraper_replay_snapshot.lua>")
            {
                widget_loaded = true;
            }
            if line.contains("Beginning demo playback")
                || line.starts_with("[t=") && line.contains("[f=0000000]")
                || line.contains("Playback continued")
            {
                replay_started = true;
            }
            if line.contains("End of demo reached") {
                eof_seen_at = Some(Instant::now());
            }
            if line.contains("<ZKScraper> Failed")
                || line.contains("<ZKScraper> No configured output directory")
                || line.contains("<ZKScraper> Not a replay")
                || line.contains("<ZKScraper> Not spectating")
            {
                kill_child(child).await?;
                return Err(format!("capture widget failed for battle {}", battle_id).into());
            }
        }

        let new_capture_bytes = capture_size(capture_dir);
        let saw_capture_progress = new_capture_bytes > capture_bytes;
        if saw_capture_progress {
            capture_bytes = new_capture_bytes;
        }
        if saw_log_progress || saw_capture_progress {
            last_progress = Instant::now();
        }

        if replay_started && !widget_loaded {
            kill_child(child).await?;
            return Err(format!(
                "replay {} started simulating before the capture widget loaded",
                battle_id
            )
            .into());
        }

        if started_at.elapsed() > Duration::from_secs(WATCHDOG_TOTAL_TIMEOUT_SECS) {
            kill_child(child).await?;
            return Err(format!(
                "watchdog timeout after {}s for replay {}",
                WATCHDOG_TOTAL_TIMEOUT_SECS, battle_id
            )
            .into());
        }

        if replay_started
            && last_progress.elapsed() > Duration::from_secs(WATCHDOG_IDLE_TIMEOUT_SECS)
        {
            kill_child(child).await?;
            return Err(format!(
                "watchdog idle timeout after {}s for replay {}",
                WATCHDOG_IDLE_TIMEOUT_SECS, battle_id
            )
            .into());
        }

        if let Some(eof_seen_at) = eof_seen_at {
            if eof_seen_at.elapsed() > Duration::from_secs(WATCHDOG_EOF_GRACE_SECS) {
                kill_child(child).await?;
                return Err(
                    format!("replay {} reached EOF without a clean shutdown", battle_id).into(),
                );
            }
        }

        sleep(Duration::from_millis(WATCHDOG_POLL_MS)).await;
    }
}

async fn kill_child(child: &mut Child) -> Result<(), Box<dyn std::error::Error>> {
    let _ = child.kill().await;
    let _ = child.wait().await;
    Ok(())
}

fn read_infolog_lines(
    infolog_path: &Path,
    offset: &mut u64,
    fragment: &mut String,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if !infolog_path.is_file() {
        return Ok(Vec::new());
    }

    let mut file = fs::File::open(infolog_path)?;
    file.seek(SeekFrom::Start(*offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    *offset += buf.len() as u64;
    if buf.is_empty() {
        return Ok(Vec::new());
    }

    fragment.push_str(&buf);
    let mut lines = Vec::new();
    while let Some(newline) = fragment.find('\n') {
        let line = fragment[..newline].trim_end_matches('\r').to_string();
        lines.push(line);
        fragment.drain(..=newline);
    }
    Ok(lines)
}

fn capture_size(capture_dir: &Path) -> u64 {
    [
        "meta.json",
        "global_snapshots.jsonl",
        "allyteam_snapshots.jsonl",
        "economy_snapshots.jsonl",
        "events.jsonl",
    ]
        .into_iter()
        .filter_map(|name| fs::metadata(capture_dir.join(name)).ok())
        .map(|meta| meta.len())
        .sum()
}

fn validate_local_widgets_enabled(zk_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = zk_path.join("LuaUI").join("Config").join("ZK_data.lua");
    let config = fs::read_to_string(&config_path)?;
    let has_local_widgets = config.contains("useLocalWidgets = true");
    let has_local_widgets_first = config.contains("useLocalWidgetsFirst = true");

    if !has_local_widgets || !has_local_widgets_first {
        return Err(format!("local widgets are not enabled in {}", config_path.display()).into());
    }

    Ok(())
}

fn read_manifest(path: &Path) -> Result<Vec<ReplayManifestEntry>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let mut entries: Vec<ReplayManifestEntry> = reader.deserialize().collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.battle_id);
    Ok(entries)
}

fn read_dem_info(path: &Path) -> Result<ReplayDemInfo, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let replay = fs::File::open(path)?;
    let mut decoder = GzDecoder::new(replay);
    decoder.read_to_end(&mut bytes)?;

    let engine_version = detect_engine_version(&bytes)?;
    let script = extract_game_script(&bytes)?;
    let springie_stats = extract_springie_stats(&bytes);
    let command_history = extract_command_history(&bytes)?;

    Ok(ReplayDemInfo {
        engine_version,
        script,
        springie_stats,
        command_history,
    })
}

fn extract_command_history(bytes: &[u8]) -> Result<Vec<CommandRecord>, Box<dyn std::error::Error>> {
    let demo_header = parse_demo_header(bytes)?;
    let stream_start = demo_header.header_size + demo_header.script_size;
    let stream_end = if demo_header.demo_stream_size == 0 {
        bytes.len()
    } else {
        stream_start
            .checked_add(demo_header.demo_stream_size)
            .ok_or("demo stream size overflow")?
            .min(bytes.len())
    };

    if stream_start > bytes.len() || stream_start > stream_end {
        return Err("invalid demo stream offsets".into());
    }

    let mut offset = stream_start;
    let mut current_frame = 0u32;
    let mut player_selection: HashMap<u8, Vec<i32>> = HashMap::new();
    let mut commands = Vec::new();

    while offset + 8 <= stream_end {
        let mod_game_time = f32::from_le_bytes(bytes[offset..offset + 4].try_into()?);
        let chunk_len = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        offset += 8;

        if offset + chunk_len > stream_end {
            break;
        }

        let packet = &bytes[offset..offset + chunk_len];
        offset += chunk_len;
        if packet.is_empty() {
            continue;
        }

        match packet[0] {
            1 => {
                if packet.len() >= 5 {
                    current_frame = i32::from_le_bytes(packet[1..5].try_into()?).max(0) as u32;
                }
            }
            2 => {
                current_frame = current_frame.saturating_add(1);
            }
            11 => {
                if let Some(record) =
                    parse_command_packet(packet, current_frame, mod_game_time, &player_selection)?
                {
                    commands.push(record);
                }
            }
            14 => {
                if let Some(record) = parse_ai_command_packet(packet, current_frame, mod_game_time)?
                {
                    commands.push(record);
                }
            }
            15 => {
                commands.extend(parse_ai_commands_packet(
                    packet,
                    current_frame,
                    mod_game_time,
                )?);
            }
            76 => {
                if let Some(record) = parse_ai_command_tracked_packet(
                    packet,
                    current_frame,
                    mod_game_time,
                )? {
                    commands.push(record);
                }
            }
            12 => {
                parse_select_packet(packet, &mut player_selection)?;
            }
            _ => {}
        }
    }

    Ok(commands)
}

#[derive(Debug)]
struct DemoHeader {
    header_size: usize,
    script_size: usize,
    demo_stream_size: usize,
}

fn parse_demo_header(bytes: &[u8]) -> Result<DemoHeader, Box<dyn std::error::Error>> {
    if bytes.len() < 352 {
        return Err("demo file too short for header".into());
    }
    if &bytes[..15] != b"spring demofile" {
        return Err("invalid demo header magic".into());
    }

    let header_size = u32::from_le_bytes(bytes[20..24].try_into()?) as usize;
    let script_size = i32::from_le_bytes(bytes[304..308].try_into()?).max(0) as usize;
    let demo_stream_size = i32::from_le_bytes(bytes[308..312].try_into()?).max(0) as usize;

    Ok(DemoHeader {
        header_size,
        script_size,
        demo_stream_size,
    })
}

fn parse_command_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
    player_selection: &HashMap<u8, Vec<i32>>,
) -> Result<Option<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 9 {
        return Ok(None);
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(9))];
    if packet.len() < 9 {
        return Ok(None);
    }

    let player_id = packet[3];
    let command_id = i32::from_le_bytes(packet[4..8].try_into()?);
    let options = packet[8] as u32;
    let mut params = Vec::new();
    let mut pos = 9;
    while pos + 4 <= packet.len() {
        params.push(f32::from_le_bytes(packet[pos..pos + 4].try_into()?));
        pos += 4;
    }

    let decoded = decode_command(command_id, options, &params);

    Ok(Some(CommandRecord {
        frame: current_frame,
        game_seconds: if mod_game_time.is_finite() && mod_game_time >= 0.0 {
            mod_game_time
        } else {
            current_frame as f32 / 30.0
        },
        player_id: player_id as u32,
        ai_id: None,
        command_id,
        options,
        params,
        selected_unit_ids: player_selection
            .get(&player_id)
            .cloned()
            .unwrap_or_default(),
        decoded,
    }))
}

fn parse_ai_command_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
) -> Result<Option<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 12 {
        return Ok(None);
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(12))];
    if packet.len() < 12 {
        return Ok(None);
    }

    let player_id = packet[3];
    let ai_id = packet[4];
    let unit_id = i16::from_le_bytes(packet[5..7].try_into()?) as i32;
    let command_id = i32::from_le_bytes(packet[7..11].try_into()?);
    let options = packet[11] as u32;
    let mut params = Vec::new();
    let mut pos = 12;
    while pos + 4 <= packet.len() {
        params.push(f32::from_le_bytes(packet[pos..pos + 4].try_into()?));
        pos += 4;
    }

    let decoded = decode_command(command_id, options, &params);
    Ok(Some(CommandRecord {
        frame: current_frame,
        game_seconds: safe_game_seconds(mod_game_time, current_frame),
        player_id: player_id as u32,
        ai_id: Some(ai_id as u32),
        command_id,
        options,
        params,
        selected_unit_ids: vec![unit_id],
        decoded,
    }))
}

fn parse_ai_command_tracked_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
) -> Result<Option<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 16 {
        return Ok(None);
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(16))];
    if packet.len() < 16 {
        return Ok(None);
    }

    let player_id = packet[3];
    let ai_id = packet[4];
    let unit_id = i16::from_le_bytes(packet[5..7].try_into()?) as i32;
    let command_id = i32::from_le_bytes(packet[7..11].try_into()?);
    let options = packet[11] as u32;
    let mut params = Vec::new();
    let mut pos = 16;
    while pos + 4 <= packet.len() {
        params.push(f32::from_le_bytes(packet[pos..pos + 4].try_into()?));
        pos += 4;
    }

    let decoded = decode_command(command_id, options, &params);
    Ok(Some(CommandRecord {
        frame: current_frame,
        game_seconds: safe_game_seconds(mod_game_time, current_frame),
        player_id: player_id as u32,
        ai_id: Some(ai_id as u32),
        command_id,
        options,
        params,
        selected_unit_ids: vec![unit_id],
        decoded,
    }))
}

fn parse_ai_commands_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
) -> Result<Vec<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 16 {
        return Ok(Vec::new());
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(16))];
    if packet.len() < 16 {
        return Ok(Vec::new());
    }

    let player_id = packet[3];
    let ai_id = packet[4];
    let mut pos = 11;
    let same_command_id = i32::from_le_bytes(packet[7..11].try_into()?);
    let same_options = packet[11] as u32;
    pos += 1;
    let same_param_size = u16::from_le_bytes(packet[pos..pos + 2].try_into()?) as usize;
    pos += 2;

    if pos + 2 > packet.len() {
        return Ok(Vec::new());
    }
    let unit_count = i16::from_le_bytes(packet[pos..pos + 2].try_into()?).max(0) as usize;
    pos += 2;
    let mut unit_ids = Vec::with_capacity(unit_count);
    for _ in 0..unit_count {
        if pos + 2 > packet.len() {
            return Ok(Vec::new());
        }
        unit_ids.push(i16::from_le_bytes(packet[pos..pos + 2].try_into()?) as i32);
        pos += 2;
    }

    if pos + 2 > packet.len() {
        return Ok(Vec::new());
    }
    let command_count = i16::from_le_bytes(packet[pos..pos + 2].try_into()?).max(0) as usize;
    pos += 2;

    let mut records = Vec::with_capacity(command_count);
    for _ in 0..command_count {
        if pos + 5 > packet.len() {
            break;
        }
        let command_id = same_command_id;
        let options = same_options;
        let param_bytes = same_param_size;
        if pos + param_bytes > packet.len() {
            break;
        }

        let mut params = Vec::new();
        let mut param_pos = pos;
        while param_pos + 4 <= pos + param_bytes {
            params.push(f32::from_le_bytes(
                packet[param_pos..param_pos + 4].try_into()?,
            ));
            param_pos += 4;
        }
        pos += param_bytes;

        let decoded = decode_command(command_id, options, &params);
        records.push(CommandRecord {
            frame: current_frame,
            game_seconds: safe_game_seconds(mod_game_time, current_frame),
            player_id: player_id as u32,
            ai_id: Some(ai_id as u32),
            command_id,
            options,
            params,
            selected_unit_ids: unit_ids.clone(),
            decoded,
        });
    }

    Ok(records)
}

fn safe_game_seconds(mod_game_time: f32, current_frame: u32) -> f32 {
    if mod_game_time.is_finite() && mod_game_time >= 0.0 {
        mod_game_time
    } else {
        current_frame as f32 / 30.0
    }
}

fn parse_select_packet(
    packet: &[u8],
    player_selection: &mut HashMap<u8, Vec<i32>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if packet.len() < 4 {
        return Ok(());
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(4))];
    if packet.len() < 4 {
        return Ok(());
    }

    let player_id = packet[3];
    let mut selected = Vec::new();
    let mut pos = 4;
    while pos + 2 <= packet.len() {
        selected.push(i16::from_le_bytes(packet[pos..pos + 2].try_into()?) as i32);
        pos += 2;
    }
    player_selection.insert(player_id, selected);
    Ok(())
}

fn decode_command(command_id: i32, options: u32, params: &[f32]) -> DecodedCommand {
    let option_flags = decode_command_options(options);

    if command_id < 0 {
        return DecodedCommand {
            kind: "build".to_string(),
            option_flags,
            target: None,
            state: None,
            build: Some(BuildCommand {
                unit_def_id: command_id.unsigned_abs(),
                unit_name: None,
                x: params.first().copied(),
                y: params.get(1).copied(),
                z: params.get(2).copied(),
                facing: params.get(3).map(|value| *value as i32),
            }),
            inserted: None,
            removed: None,
        };
    }

    let (kind, target, state, inserted, removed) = match command_id {
        0 => ("stop".to_string(), None, None, None, None),
        1 => {
            let inserted = if params.len() >= 3 {
                let nested_command_id = params[1] as i32;
                let nested_options = params[2].max(0.0) as u32;
                let nested_params = params[3..].to_vec();
                Some(InsertedCommand {
                    index: params[0] as i32,
                    command_id: nested_command_id,
                    options: nested_options,
                    params: nested_params.clone(),
                    decoded: Box::new(decode_command(
                        nested_command_id,
                        nested_options,
                        &nested_params,
                    )),
                })
            } else {
                None
            };
            ("insert".to_string(), None, None, inserted, None)
        }
        2 => {
            let mode = if option_flags.alt {
                "remove_by_command_id"
            } else if option_flags.ctrl {
                "remove_by_tag"
            } else {
                "remove"
            };
            (
                "remove".to_string(),
                None,
                None,
                None,
                Some(RemovedCommand {
                    params: params.to_vec(),
                    mode: mode.to_string(),
                }),
            )
        }
        5 => ("wait".to_string(), None, params.first().map(|v| *v as i32), None, None),
        6 => ("timewait".to_string(), None, params.first().map(|v| *v as i32), None, None),
        7 => ("deathwait".to_string(), None, None, None, None),
        8 => ("squadwait".to_string(), None, None, None, None),
        9 => ("gatherwait".to_string(), None, None, None, None),
        10 => ("move".to_string(), decode_target(params), None, None, None),
        15 => ("patrol".to_string(), decode_target(params), None, None, None),
        16 => ("fight".to_string(), decode_target(params), None, None, None),
        20 => ("attack".to_string(), decode_target(params), None, None, None),
        21 => ("area_attack".to_string(), decode_target(params), None, None, None),
        25 => ("guard".to_string(), decode_target(params), None, None, None),
        32 => ("resurrect".to_string(), decode_target(params), None, None, None),
        40 => ("repair".to_string(), decode_target(params), None, None, None),
        45 => ("fire_state".to_string(), None, params.first().map(|v| *v as i32), None, None),
        50 => ("move_state".to_string(), None, params.first().map(|v| *v as i32), None, None),
        55 => ("set_base".to_string(), decode_target(params), None, None, None),
        70 => ("set_wanted_max_speed".to_string(), None, None, None, None),
        65 => ("self_destruct".to_string(), None, None, None, None),
        75 => ("load_units".to_string(), decode_target(params), None, None, None),
        76 => ("load_onto".to_string(), decode_target(params), None, None, None),
        81 => ("unload_units".to_string(), decode_target(params), None, None, None),
        85 => ("onoff".to_string(), None, params.first().map(|v| *v as i32), None, None),
        90 => ("reclaim".to_string(), decode_target(params), None, None, None),
        100 => ("cloak".to_string(), None, params.first().map(|v| *v as i32), None, None),
        115 => ("repeat".to_string(), None, params.first().map(|v| *v as i32), None, None),
        31109 => ("raw_move".to_string(), decode_target(params), None, None, None),
        31110 => ("raw_build".to_string(), decode_target(params), None, None, None),
        31200 => ("want_cloak".to_string(), None, params.first().map(|v| *v as i32), None, None),
        33410 => ("rearm".to_string(), decode_target(params), None, None, None),
        34220 => ("priority".to_string(), None, params.first().map(|v| *v as i32), None, None),
        34223 => ("retreat".to_string(), None, params.first().map(|v| *v as i32), None, None),
        34923 => ("morph".to_string(), None, None, None, None),
        34924 => ("morph_stop".to_string(), None, None, None, None),
        34925 => ("jump".to_string(), decode_target(params), None, None, None),
        _ => (format!("unknown_{}", command_id), decode_target(params), None, None, None),
    };

    DecodedCommand {
        kind,
        option_flags,
        target,
        state,
        build: None,
        inserted,
        removed,
    }
}

fn enrich_command_history_with_unit_names(
    commands: &mut [CommandRecord],
    script: &str,
    zk_path: Option<&Path>,
    cache: &mut HashMap<String, HashMap<u32, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(zk_path) = zk_path else {
        return Ok(());
    };
    let script_meta = parse_game_script(script)?;
    let Some(game_name) = script_meta.game_name else {
        return Ok(());
    };
    let Some(game_version) = game_name.strip_prefix("Zero-K v").map(str::to_string) else {
        return Ok(());
    };

    if !cache.contains_key(&game_version) {
        let names = load_unit_def_names_for_version(zk_path, &game_version)?;
        cache.insert(game_version.clone(), names);
    }

    let Some(unit_names) = cache.get(&game_version) else {
        return Ok(());
    };
    for command in commands {
        enrich_decoded_command_with_unit_names(&mut command.decoded, unit_names);
    }
    Ok(())
}

fn enrich_decoded_command_with_unit_names(
    decoded: &mut DecodedCommand,
    unit_names: &HashMap<u32, String>,
) {
    if let Some(build) = decoded.build.as_mut() {
        if build.unit_name.is_none() {
            build.unit_name = unit_names.get(&build.unit_def_id).cloned();
        }
    }
    if let Some(inserted) = decoded.inserted.as_mut() {
        enrich_decoded_command_with_unit_names(&mut inserted.decoded, unit_names);
    }
}

fn load_unit_def_names_for_version(
    zk_path: &Path,
    game_version: &str,
) -> Result<HashMap<u32, String>, Box<dyn std::error::Error>> {
    let archive_hash = resolve_game_archive_hash(zk_path, game_version)?;
    let archive_path = zk_path.join("games").join(format!("{archive_hash}.sdz"));
    if !archive_path.is_file() {
        return Ok(HashMap::new());
    }

    let bytes = fs::read(&archive_path)?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut file = match archive.by_name("gamedata/unitdefs.lua") {
        Ok(file) => file,
        Err(_) => return Ok(HashMap::new()),
    };

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut map = HashMap::new();
    let re = Regex::new(r#"UnitDefs\[(\d+)\]\.name\s*=\s*"([^"]+)""#)?;
    for cap in re.captures_iter(&contents) {
        if let (Some(id), Some(name)) = (cap.get(1), cap.get(2)) {
            if let Ok(id) = id.as_str().parse::<u32>() {
                map.insert(id, name.as_str().to_string());
            }
        }
    }
    Ok(map)
}

fn resolve_game_archive_hash(
    zk_path: &Path,
    game_version: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let versions_path = zk_path
        .join("rapid")
        .join("repos.springrts.com")
        .join("zk")
        .join("versions.gz");
    let file = fs::File::open(versions_path)?;
    let mut decoder = GzDecoder::new(file);
    let mut text = String::new();
    decoder.read_to_string(&mut text)?;

    for line in text.lines() {
        let mut parts = line.splitn(4, ',');
        let _tag = parts.next();
        let hash = parts.next();
        let _unused = parts.next();
        let display_name = parts.next();
        if let (Some(hash), Some(display_name)) = (hash, display_name) {
            if display_name.trim() == format!("Zero-K v{}", game_version) {
                return Ok(hash.trim().to_string());
            }
        }
    }

    Err(format!("could not resolve archive hash for Zero-K {}", game_version).into())
}

fn decode_command_options(options: u32) -> CommandOptionFlags {
    CommandOptionFlags {
        alt: options & 128 != 0,
        ctrl: options & 64 != 0,
        meta: options & 4 != 0,
        shift: options & 32 != 0,
        right: options & 16 != 0,
        internal: options & 8 != 0,
    }
}

fn decode_target(params: &[f32]) -> Option<DecodedTarget> {
    match params.len() {
        1 => Some(DecodedTarget::Unit {
            unit_id: params[0] as i32,
        }),
        3 => Some(DecodedTarget::Position {
            x: params[0],
            y: params[1],
            z: params[2],
        }),
        n if n >= 4 => Some(DecodedTarget::Area {
            x: params[0],
            y: params[1],
            z: params[2],
            radius: params[3],
        }),
        _ => None,
    }
}

fn detect_engine_version(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
    let version_re = Regex::new(r"(20\d{2}\.\d{2}\.\d{2}|105\.[0-9A-Za-z.\-]+)")?;
    let version = version_re
        .captures(&prefix)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or("could not detect replay engine version from dem header")?;
    Ok(version)
}

fn extract_game_script(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let marker = b"[game]";
    let start = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or("could not find [game] script block in dem")?;

    let mut brace_depth = 0i32;
    let mut found_open = false;
    let mut end = None;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        match byte {
            b'{' => {
                brace_depth += 1;
                found_open = true;
            }
            b'}' if found_open => {
                brace_depth -= 1;
                if brace_depth == 0 {
                    end = Some(start + offset + 1);
                    break;
                }
            }
            _ => {}
        }
    }

    let end = end.ok_or("could not determine end of [game] script block")?;
    Ok(String::from_utf8_lossy(&bytes[start..end]).to_string())
}

fn extract_springie_stats(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let Ok(stats_re) = Regex::new(r"SPRINGIE:(stats|award),[^\0\r\n]+") else {
        return Vec::new();
    };

    let mut seen = BTreeMap::<String, ()>::new();
    for captures in stats_re.find_iter(&text) {
        seen.insert(captures.as_str().to_string(), ());
    }
    seen.into_keys().collect()
}

fn parse_game_script(script: &str) -> Result<ScriptMetadata, Box<dyn std::error::Error>> {
    let mut meta = ScriptMetadata::default();
    let mut pending_section: Option<String> = None;
    let mut stack: Vec<String> = Vec::new();
    let mut players: HashMap<u32, PlayerMetadata> = HashMap::new();
    let mut teams: HashMap<u32, TeamMetadata> = HashMap::new();

    for raw_line in script.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            pending_section = Some(line[1..line.len() - 1].to_string());
            continue;
        }

        if line == "{" {
            if let Some(section) = pending_section.take() {
                stack.push(section);
            }
            continue;
        }

        if line == "}" {
            stack.pop();
            continue;
        }

        let Some((key, value)) = parse_script_assignment(line) else {
            continue;
        };

        match stack.as_slice() {
            [game] if game == "game" => match key.as_str() {
                "mapname" => meta.map_name = Some(value),
                "gametype" => meta.game_name = Some(value),
                "zksearchtag" => meta.zksearchtag = Some(value),
                _ => {}
            },
            [game, section] if game == "game" && section.starts_with("player") => {
                let player_id = section["player".len()..].parse::<u32>()?;
                let player = players.entry(player_id).or_insert(PlayerMetadata {
                    player_id,
                    name: None,
                    team: None,
                    spectator: false,
                    elo: None,
                    lobby_id: None,
                    country_code: None,
                    clan: None,
                    level: None,
                });

                match key.as_str() {
                    "name" => player.name = Some(value),
                    "team" => player.team = value.parse().ok(),
                    "spectator" => player.spectator = value == "1",
                    "elo" => player.elo = value.parse().ok(),
                    "lobbyid" => player.lobby_id = value.parse().ok(),
                    "countrycode" => player.country_code = Some(value),
                    "clan" => player.clan = Some(value),
                    "level" => player.level = value.parse().ok(),
                    _ => {}
                }
            }
            [game, section] if game == "game" && section.starts_with("team") => {
                let team_id = section["team".len()..].parse::<u32>()?;
                let team = teams.entry(team_id).or_insert(TeamMetadata {
                    team_id,
                    allyteam: None,
                    teamleader: None,
                    handicap: None,
                });

                match key.as_str() {
                    "allyteam" => team.allyteam = value.parse().ok(),
                    "teamleader" => team.teamleader = value.parse().ok(),
                    "handicap" => team.handicap = value.parse().ok(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let mut player_values: Vec<_> = players.into_values().collect();
    player_values.sort_by_key(|player| player.player_id);
    meta.players = player_values;

    let mut team_values: Vec<_> = teams.into_values().collect();
    team_values.sort_by_key(|team| team.team_id);
    meta.teams = team_values;
    Ok(meta)
}

fn parse_script_assignment(line: &str) -> Option<(String, String)> {
    let line = line.strip_suffix(';').unwrap_or(line);
    let (key, value) = line.split_once('=')?;
    Some((key.trim().to_string(), value.trim().to_string()))
}

fn resolve_engine_binary(
    zk_path: &Path,
    engine_version: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let engine_root = zk_path.join("engine");
    for candidate in engine_binary_candidates(&engine_root, engine_version) {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "could not find a spring-headless binary for engine version {} under {}",
        engine_version,
        engine_root.display()
    )
    .into())
}

fn engine_binary_candidates(engine_root: &Path, engine_version: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut push_candidate = |platform_dir: &str, binary_name: &str| {
        candidates.push(
            engine_root
                .join(platform_dir)
                .join(engine_version)
                .join(binary_name),
        );
    };

    for (platform_dir, binary_name) in preferred_engine_locations() {
        push_candidate(platform_dir, binary_name);
    }

    for platform_dir in ["win64", "linux64", "macos64", "osx64"] {
        push_candidate(platform_dir, "spring-headless");
        push_candidate(platform_dir, "spring-headless.exe");
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn preferred_engine_locations() -> &'static [(&'static str, &'static str)] {
    if cfg!(target_os = "windows") {
        &[("win64", "spring-headless.exe")]
    } else if cfg!(target_os = "linux") {
        &[("linux64", "spring-headless"), ("linux64", "spring-headless.exe")]
    } else if cfg!(target_os = "macos") {
        &[
            ("macos64", "spring-headless"),
            ("osx64", "spring-headless"),
            ("macos64", "spring-headless.exe"),
            ("osx64", "spring-headless.exe"),
        ]
    } else {
        &[("win64", "spring-headless.exe")]
    }
}

fn install_widget(source: &Path, target: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if target.exists() {
        fs::remove_file(target)?;
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    if fs::hard_link(source, target).is_err() {
        fs::copy(source, target)?;
    }

    Ok(())
}

fn write_headless_config(
    config_path: &Path,
    zk_path: &Path,
    capture_dir: &Path,
    battle_id: u64,
    spring_data_roots: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let capture_dir = match absolute_path(capture_dir)?.strip_prefix(&absolute_path(zk_path)?) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => absolute_path(capture_dir)?
            .to_string_lossy()
            .replace('\\', "/"),
    };
    let spring_data = spring_data_roots
        .iter()
        .map(|path| absolute_path(path))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>()
        .join(";");
    let mut config = String::new();
    config.push_str("Version = 2\n");
    config.push_str("XResolution = 8\n");
    config.push_str("YResolution = 8\n");
    config.push_str("XResolutionWindowed = 8\n");
    config.push_str("YResolutionWindowed = 8\n");
    config.push_str("Fullscreen = 0\n");
    config.push_str("WindowBorderless = 0\n");
    config.push_str("Shadows = -1\n");
    config.push_str("Water = 0\n");
    config.push_str("GroundDecals = 0\n");
    config.push_str("GrassDetail = 0\n");
    config.push_str("MaxParticles = 1\n");
    config.push_str("MaxSounds = 0\n");
    config.push_str("Sound = 0\n");
    config.push_str("ShowClock = 0\n");
    config.push_str("ShowPlayerInfo = 0\n");
    config.push_str("CamMode = 1\n");
    config.push_str("VerboseLevel = 10\n");
    config.push_str("LogFlush = 1\n");
    config.push_str("RotateLogFiles = 0\n");
    config.push_str("LuaAutoModWidgets = 0\n");
    config.push_str(&format!("SpringData = {spring_data}\n"));
    config.push_str(&format!("ZKHeadlessBattleId = {battle_id}\n"));
    config.push_str(&format!("ZKHeadlessOutputDir = {capture_dir}\n"));
    config.push_str(&format!("ZKHeadlessSnapshotFrames = {SNAPSHOT_FRAMES}\n"));

    fs::write(config_path, config)?;
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn read_jsonl<T>(path: &Path) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: for<'de> Deserialize<'de>,
{
    let content = fs::read_to_string(path)?;
    let mut items = Vec::new();
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        items.push(serde_json::from_str::<T>(line)?);
    }
    Ok(items)
}

fn cleanup_replay_artifacts(
    widget_target: &Path,
    replay_temp_dir: &Path,
    capture_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if widget_target.exists() {
        fs::remove_file(widget_target)?;
    }
    if replay_temp_dir.exists() {
        fs::remove_dir_all(replay_temp_dir)?;
    }
    if capture_dir.exists() {
        fs::remove_dir_all(capture_dir)?;
    }
    Ok(())
}

fn activate_scraper_configs(
    config_dir: &Path,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    if !config_dir.is_dir() {
        return Ok(Vec::new());
    }

    ensure_default_scraper_configs(config_dir)?;

    let mut swapped = Vec::new();
    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let scraper_path = entry.path();
        let Some(file_name) = scraper_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".lua.scraper") {
            continue;
        }

        let live_name = file_name.trim_end_matches(".scraper");
        let live_path = config_dir.join(live_name);
        let backup_path = config_dir.join(format!("{live_name}.bak"));
        if backup_path.exists() {
            return Err(format!(
                "refusing to overwrite existing backup file {}",
                backup_path.display()
            )
            .into());
        }
        if live_path.exists() {
            fs::rename(&live_path, &backup_path)?;
        }
        fs::rename(&scraper_path, &live_path)?;
        swapped.push(live_path);
    }

    Ok(swapped)
}

fn ensure_default_scraper_configs(config_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let zk_data_path = config_dir.join("ZK_data.lua");
    let zk_data_scraper_path = config_dir.join("ZK_data.lua.scraper");
    if !zk_data_scraper_path.exists() {
        let base = fs::read_to_string(&zk_data_path)?;
        let scraper = ensure_lua_bool_setting(
            &ensure_lua_bool_setting(&base, "useLocalWidgets", true)?,
            "useLocalWidgetsFirst",
            true,
        )?;
        fs::write(&zk_data_scraper_path, scraper)?;
    }

    let order_path = config_dir.join("ZK_order.lua");
    let order_scraper_path = config_dir.join("ZK_order.lua.scraper");
    if !order_scraper_path.exists() {
        let scraper = build_parser_order_scraper(&order_path)?;
        fs::write(&order_scraper_path, scraper)?;
    }

    Ok(())
}

fn ensure_lua_bool_setting(
    content: &str,
    key: &str,
    value: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let value_text = if value { "true" } else { "false" };
    let pattern = format!(r#"(?m)^(\s*{}\s*=\s*)(true|false)"#, regex::escape(key));
    let re = Regex::new(&pattern)?;
    if re.is_match(content) {
        Ok(re
            .replace(content, format!("${{1}}{value_text}"))
            .into_owned())
    } else {
        let mut updated = content.to_string();
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&format!("{key} = {value_text}\n"));
        Ok(updated)
    }
}

fn build_parser_order_scraper(order_path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let existing = if order_path.is_file() {
        fs::read_to_string(order_path)?
    } else {
        String::new()
    };

    let key_re = Regex::new(r#"(?m)^\s*(?:\["([^"]+)"\]|([A-Za-z0-9_]+))\s*="#)?;
    let mut keys = Vec::new();
    for captures in key_re.captures_iter(&existing) {
        let key = captures
            .get(1)
            .or_else(|| captures.get(2))
            .map(|m| m.as_str())
            .unwrap_or_default();
        if key != "version" && key != "lastWidgetDetailLevel" {
            keys.push(key.to_string());
        }
    }
    keys.sort();
    keys.dedup();

    let mut parser_order = String::from("-- Widget Order List  (0 disables a widget)\nreturn {\n");
    parser_order.push_str("\tversion = 8,\n");
    parser_order.push_str("\tlastWidgetDetailLevel = 3,\n");
    for key in keys {
        parser_order.push_str(&format!(
            "\t[\"{}\"] = 0,\n",
            key.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    parser_order.push_str(&format!("\t[\"{}\"] = 1,\n", WIDGET_DISPLAY_NAME));
    parser_order.push_str("}\n");
    Ok(parser_order)
}

fn restore_scraper_configs(swapped: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    for live_path in swapped {
        let Some(file_name) = live_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let scraper_path = live_path.with_file_name(format!("{file_name}.scraper"));
        let backup_path = live_path.with_file_name(format!("{file_name}.bak"));

        if live_path.exists() {
            fs::rename(live_path, &scraper_path)?;
        }
        if backup_path.exists() {
            fs::rename(backup_path, live_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{engine_binary_candidates, ParsedReplay};

    #[test]
    fn engine_candidates_include_cross_platform_locations() {
        let candidates = engine_binary_candidates(PathBuf::from("engine").as_path(), "105.0").into_iter().map(|path| path.to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();

        assert!(candidates.iter().any(|path| path.ends_with("win64/105.0/spring-headless.exe")));
        assert!(candidates.iter().any(|path| path.ends_with("linux64/105.0/spring-headless")));
        assert!(candidates.iter().any(|path| path.ends_with("macos64/105.0/spring-headless")));
    }

    #[test]
    fn parsed_replay_defaults_missing_economy_snapshots() {
        let replay = serde_json::from_str::<ParsedReplay>(
            r#"{
                "battle_id": 1,
                "replay_filename": "test.sdfz",
                "game_version": "1",
                "engine_version": "105",
                "map_name": null,
                "game_name": null,
                "zksearchtag": null,
                "players": [],
                "teams": [],
                "map_size": null,
                "global_snapshots": [],
                "allyteam_snapshots": {},
                "command_history": [],
                "events": [],
                "springie_stats": []
            }"#,
        )
        .unwrap();

        assert!(replay.economy_snapshots.is_empty());
    }

    #[test]
    fn parsed_replay_accepts_legacy_player_snapshots_field() {
        let replay = serde_json::from_str::<ParsedReplay>(
            r#"{
                "battle_id": 1,
                "replay_filename": "test.sdfz",
                "game_version": "1",
                "engine_version": "105",
                "map_name": null,
                "game_name": null,
                "zksearchtag": null,
                "players": [],
                "teams": [],
                "map_size": null,
                "global_snapshots": [],
                "allyteam_snapshots": {},
                "player_snapshots": {},
                "command_history": [],
                "events": [],
                "springie_stats": []
            }"#,
        )
        .unwrap();

        assert!(replay.economy_snapshots.is_empty());
    }
}
