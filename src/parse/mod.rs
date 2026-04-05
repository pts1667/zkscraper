use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use fs2::FileExt;

use crate::db::ReplayDb;
use indicatif::ProgressBar;

mod demo;
mod headless;
mod script;
mod types;

use demo::{enrich_command_history_with_unit_names, read_dem_info};
use headless::{
    resolve_engine_binary, run_single_replay, validate_local_widgets_enabled,
};
use script::parse_game_script;
use types::ReplayManifestEntry;
pub use types::*;

const MANIFEST_FILENAME: &str = "replay_manifest.csv";
const WIDGET_LINK_NAME: &str = "zkscraper_replay_snapshot.lua";
const WIDGET_DISPLAY_NAME: &str = "ZKScraper Replay Snapshot";
const ZK_CAPTURE_ROOT: &str = "LuaUI/Logs/zkscraper";
const SNAPSHOT_FRAMES: u32 = 120;
const WATCHDOG_POLL_MS: u64 = 250;
const WATCHDOG_TOTAL_TIMEOUT_SECS: u64 = 300;
const WATCHDOG_IDLE_TIMEOUT_SECS: u64 = 20;
const WATCHDOG_EOF_GRACE_SECS: u64 = 15;
const HEADLESS_PARSE_LOCK_NAME: &str = ".zkscraper-headless.lock";

#[derive(Debug)]
struct HeadlessParseLock {
    file: fs::File,
}

impl Drop for HeadlessParseLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub async fn parse_replays(
    settings: ParseReplaySettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = ReplayDb::open(&settings.snapshot_path)?;
    parse_replays_into_db(settings, db).await
}

pub async fn parse_replays_into_db(
    settings: ParseReplaySettings,
    db: ReplayDb,
) -> Result<(), Box<dyn std::error::Error>> {
    let _headless_lock = acquire_headless_parse_lock(&settings.zk_path)?;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_signal = interrupted.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupted_signal.store(true, Ordering::SeqCst);
        }
    });

    let parse_future = async {
        validate_local_widgets_enabled(&settings.zk_path)?;
        ensure_no_running_headless_process()?;

        let manifest = sort_manifest_by_replay_size(
            read_manifest(&settings.sdfz_in.join(MANIFEST_FILENAME))?,
            &settings.sdfz_in,
        );
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

            if db.contains_replay(&entry.replay_id)? {
                pb.inc(1);
                continue;
            }

            let replay_path = settings.sdfz_in.join(&entry.replay_filename);
            if !replay_path.is_file() {
                eprintln!(
                    "Parse failed for replay {}: replay file from manifest does not exist: {}",
                    entry.replay_id,
                    replay_path.display()
                );
                failures.push(format!("{} ({})", entry.replay_id, entry.replay_filename));
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

                let replay_temp_dir = temp_root.join(&entry.replay_id);
                let capture_dir = zk_capture_root.join(&entry.replay_id);
                fs::create_dir_all(&capture_dir)?;
                let config_path = replay_temp_dir.join("springsettings-headless.cfg");

                let (widget_meta, global_snapshots, allyteam_snapshots, economy_snapshots, events) =
                    run_single_replay(
                        &widget_source,
                        &widget_target,
                        &config_path,
                        &capture_dir,
                        &headless,
                        &settings.zk_path,
                        &replay_path,
                        entry.headless_id,
                        interrupted.clone(),
                    )
                    .await?;

                let parsed = ParsedReplay {
                    replay_id: entry.replay_id.clone(),
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

                db.put_replay(&parsed)?;
                Ok(())
            }
            .await;

            if let Err(err) = replay_outcome {
                eprintln!(
                    "Parse failed for replay {} ({}): {}",
                    entry.replay_id, entry.replay_filename, err
                );
                failures.push(format!("{} ({})", entry.replay_id, entry.replay_filename));
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

    parse_result
}

pub async fn backfill_commands(
    sdfz_in: PathBuf,
    snapshot_path: PathBuf,
    zk_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = read_manifest(&sdfz_in.join(MANIFEST_FILENAME))?;
    let db = ReplayDb::open(&snapshot_path)?;
    let pb = ProgressBar::new(manifest.len() as u64);
    let mut failures = Vec::new();
    let mut unit_name_cache: HashMap<String, HashMap<u32, String>> = HashMap::new();

    for entry in manifest {
        let Some(mut parsed) = db.get_replay(&entry.replay_id)? else {
            pb.inc(1);
            continue;
        };

        let replay_path = sdfz_in.join(&entry.replay_filename);
        if !replay_path.is_file() {
            eprintln!(
                "Command backfill failed for replay {}: replay file from manifest does not exist: {}",
                entry.replay_id,
                replay_path.display()
            );
            failures.push(format!("{} ({})", entry.replay_id, entry.replay_filename));
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
            parsed.command_history = dem_info.command_history;
            db.put_replay(&parsed)?;
            Ok(())
        })();

        if let Err(err) = backfill_result {
            eprintln!(
                "Command backfill failed for replay {} ({}): {}",
                entry.replay_id, entry.replay_filename, err
            );
            failures.push(format!("{} ({})", entry.replay_id, entry.replay_filename));
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

fn read_manifest(path: &Path) -> Result<Vec<ReplayManifestEntry>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let mut entries: Vec<ReplayManifestEntry> = reader.deserialize().collect::<Result<_, _>>()?;
    entries.sort_by(|left, right| left.replay_id.cmp(&right.replay_id));
    Ok(entries)
}

fn sort_manifest_by_replay_size(
    mut manifest: Vec<ReplayManifestEntry>,
    sdfz_in: &Path,
) -> Vec<ReplayManifestEntry> {
    manifest.sort_by_key(|entry| {
        let replay_path = sdfz_in.join(&entry.replay_filename);
        let size = fs::metadata(&replay_path)
            .map(|metadata| metadata.len())
            .unwrap_or(u64::MAX);
        (size, entry.replay_id.clone())
    });
    manifest
}

fn acquire_headless_parse_lock(
    zk_path: &Path,
) -> Result<HeadlessParseLock, Box<dyn std::error::Error>> {
    let lock_path = zk_path.join(HEADLESS_PARSE_LOCK_NAME);
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    file.try_lock_exclusive().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "another replay parse is already in progress for {}",
                zk_path.display()
            ),
        )
    })?;

    Ok(HeadlessParseLock { file })
}

pub(super) fn ensure_no_running_headless_process() -> Result<(), Box<dyn std::error::Error>> {
    if headless_process_is_running()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "spring-headless.exe is already running; only one replay parse may run at a time",
        )
        .into());
    }
    Ok(())
}

fn headless_process_is_running() -> Result<bool, Box<dyn std::error::Error>> {
    let output = if cfg!(target_os = "windows") {
        Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()?
    } else {
        Command::new("ps")
            .args(["-A", "-o", "comm="])
            .output()?
    };

    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "failed to inspect running processes: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        )
        .into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(target_os = "windows") {
        Ok(stdout.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("\"spring-headless.exe\"") || line.starts_with("\"spring-headless\"")
        }))
    } else {
        Ok(stdout.lines().any(|line| {
            let name = line.trim();
            name == "spring-headless" || name == "spring-headless.exe"
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{acquire_headless_parse_lock, headless::engine_binary_candidates, ParsedReplay};

    #[test]
    fn engine_candidates_include_cross_platform_locations() {
        let candidates = engine_binary_candidates(PathBuf::from("engine").as_path(), "105.0")
            .into_iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();

        assert!(candidates
            .iter()
            .any(|path| path.ends_with("win64/105.0/spring-headless.exe")));
        assert!(candidates
            .iter()
            .any(|path| path.ends_with("linux64/105.0/spring-headless")));
        assert!(candidates
            .iter()
            .any(|path| path.ends_with("macos64/105.0/spring-headless")));
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

    #[test]
    fn headless_parse_lock_is_single_instance() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let _lock = acquire_headless_parse_lock(temp_dir.path())?;

        let err = acquire_headless_parse_lock(temp_dir.path()).unwrap_err();
        assert!(err
            .to_string()
            .contains("another replay parse is already in progress"));
        Ok(())
    }
}
