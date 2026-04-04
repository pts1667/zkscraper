use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{atomic::AtomicBool, Arc},
};

use regex::Regex;
use serde::Deserialize;
use tokio::{
    process::{Child, Command},
    time::{sleep, Duration, Instant},
};

use super::{
    AllyTeamSnapshotRecord, EconomySnapshotRecord, EventRecord, SnapshotRecord,
    WATCHDOG_EOF_GRACE_SECS, WATCHDOG_IDLE_TIMEOUT_SECS, WATCHDOG_POLL_MS,
    WATCHDOG_TOTAL_TIMEOUT_SECS, WIDGET_DISPLAY_NAME, WIDGET_LINK_NAME,
};
use super::{WidgetMeta, SNAPSHOT_FRAMES};

pub(super) async fn run_single_replay(
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
    if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("parse interrupted by Ctrl+C".into());
    }

    super::ensure_no_running_headless_process()?;
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

    let result: Result<_, Box<dyn std::error::Error>> = async {
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
        let allyteam_snapshot_rows =
            read_jsonl::<AllyTeamSnapshotRecord>(&allyteam_snapshots_path)?;
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
    .await;

    let cleanup_result = cleanup_replay_artifacts(widget_target, config_path, capture_dir);
    match (result, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(cleanup_err)) => Err(cleanup_err),
        (Err(err), Err(cleanup_err)) => {
            Err(format!("{err}; also failed to clean replay artifacts: {cleanup_err}").into())
        }
    }
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
    let mut widget_load_race_reported = false;
    let mut eof_seen_at: Option<Instant> = None;
    let mut log_offset = log_offset;
    let mut log_fragment = String::new();
    let mut capture_bytes = capture_size(capture_dir);

    loop {
        if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
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
                || line.contains(&format!("<{}>", WIDGET_LINK_NAME))
            {
                widget_loaded = true;
                eprintln!(
                    "headless watcher: capture widget loaded for replay {}: {}",
                    battle_id, line
                );
            }
            if line.contains("Beginning demo playback")
                || line.starts_with("[t=") && line.contains("[f=0000000]")
                || line.contains("Playback continued")
            {
                replay_started = true;
                eprintln!(
                    "headless watcher: replay start seen for replay {}: {}",
                    battle_id, line
                );
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

        if replay_started && !widget_loaded && !widget_load_race_reported {
            widget_load_race_reported = true;
            eprintln!(
                "headless watcher: replay {} started before the capture widget loaded; continuing to wait",
                battle_id
            );
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

pub(super) fn validate_local_widgets_enabled(
    zk_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = zk_path.join("LuaUI").join("Config").join("ZK_data.lua");
    let config = fs::read_to_string(&config_path)?;
    let has_local_widgets = config.contains("useLocalWidgets = true");
    let has_local_widgets_first = config.contains("useLocalWidgetsFirst = true");

    if !has_local_widgets || !has_local_widgets_first {
        return Err(format!("local widgets are not enabled in {}", config_path.display()).into());
    }

    Ok(())
}

pub(super) fn resolve_engine_binary(
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

pub(super) fn engine_binary_candidates(engine_root: &Path, engine_version: &str) -> Vec<PathBuf> {
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
        &[
            ("linux64", "spring-headless"),
            ("linux64", "spring-headless.exe"),
        ]
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
    config_path: &Path,
    capture_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if widget_target.exists() {
        fs::remove_file(widget_target)?;
    }
    if let Some(replay_temp_dir) = config_path.parent() {
        if replay_temp_dir.exists() {
            fs::remove_dir_all(replay_temp_dir)?;
        }
    }
    if capture_dir.exists() {
        fs::remove_dir_all(capture_dir)?;
    }
    Ok(())
}

pub(super) fn activate_scraper_configs(
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
            if live_path.exists() && files_equal(&live_path, &scraper_path)? {
                swapped.push(live_path);
                continue;
            }
            if !live_path.exists() {
                fs::rename(&backup_path, &live_path)?;
            } else if files_equal(&live_path, &backup_path)? {
                fs::remove_file(&backup_path)?;
            } else {
                // An interrupted prior run can leave the original config in .bak and a
                // stale activated file at the live path. Keep the original backup and
                // replace the live file with the current scraper variant.
                fs::remove_file(&live_path)?;
            }
        }
        if live_path.exists() {
            fs::rename(&live_path, &backup_path)?;
        }
        fs::rename(&scraper_path, &live_path)?;
        swapped.push(live_path);
    }

    Ok(swapped)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(fs::read(left)? == fs::read(right)?)
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

pub(super) fn restore_scraper_configs(
    swapped: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
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
