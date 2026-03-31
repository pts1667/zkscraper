use std::{
    fmt,
    num::NonZeroUsize,
    path::Path,
    sync::{Arc, Mutex},
};

use fastapi::ToSchema;
use lru::LruCache;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::parse::ParsedReplay;

#[derive(Debug)]
pub struct ReplayDbError(String);

impl ReplayDbError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ReplayDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReplayDbError {}

#[derive(Debug, Clone)]
pub struct ReplayDb {
    db: sled::Db,
    battle_ids: Arc<Vec<u64>>,
    summaries: Arc<Vec<ReplaySummary>>,
    replay_json_cache: Option<Arc<Mutex<LruCache<u64, Arc<serde_json::Value>>>>>,
}

const DEFAULT_REPLAY_JSON_CACHE_SIZE: usize = 0;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ReplaySummary {
    pub battle_id: u64,
    pub replay_filename: String,
    pub game_version: String,
    pub engine_version: String,
    pub map_name: Option<String>,
    pub game_name: Option<String>,
    pub players_count: usize,
    pub teams_count: usize,
    pub global_snapshots: usize,
    pub allyteam_snapshot_streams: usize,
    pub allyteam_snapshot_frames: usize,
    pub commands: usize,
    pub events: usize,
    pub first_snapshot_frame: Option<u32>,
    pub last_snapshot_frame: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ReplayListResponse {
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub items: Vec<ReplaySummary>,
}

impl ReplayDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReplayDbError> {
        let db = sled::open(path).map_err(|err| ReplayDbError::new(err.to_string()))?;
        let mut entries = Vec::with_capacity(db.len());
        for entry in db.iter() {
            let (key, value) = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
            entries.push((key.to_vec(), value.to_vec()));
        }

        let mut indexed = entries
            .into_par_iter()
            .map(|(key, value)| {
            let key = std::str::from_utf8(key.as_ref())
                .map_err(|err| ReplayDbError::new(err.to_string()))?;
            let battle_id = key.parse::<u64>().map_err(|err| {
                ReplayDbError::new(format!("invalid battle id key '{key}': {err}"))
            })?;
            let summary = decode_replay_summary(value.as_ref())?;
                Ok((battle_id, summary))
            })
            .collect::<Result<Vec<_>, _>>()?;
        indexed.sort_unstable_by_key(|(battle_id, _)| *battle_id);
        let battle_ids = indexed.iter().map(|(battle_id, _)| *battle_id).collect();
        let summaries = indexed.into_iter().map(|(_, summary)| summary).collect();

        Ok(Self {
            db,
            battle_ids: Arc::new(battle_ids),
            summaries: Arc::new(summaries),
            replay_json_cache: replay_json_cache(),
        })
    }

    pub fn battle_ids(&self) -> &[u64] {
        self.battle_ids.as_slice()
    }

    pub fn total(&self) -> usize {
        self.battle_ids.len()
    }

    pub fn get_replay(&self, battle_id: u64) -> Result<Option<ParsedReplay>, ReplayDbError> {
        let key = battle_id.to_string();
        let Some(value) = self
            .db
            .get(key.as_bytes())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
        else {
            return Ok(None);
        };

        decode_replay(value.as_ref()).map(Some)
    }

    pub fn get_replay_value(
        &self,
        battle_id: u64,
    ) -> Result<Option<serde_json::Value>, ReplayDbError> {
        if let Some(cache) = self.replay_json_cache.as_ref() {
            if let Some(cached) = cache
                .lock()
                .map_err(|_| ReplayDbError::new("replay cache mutex poisoned"))?
                .get(&battle_id)
                .cloned()
            {
                return Ok(Some((*cached).clone()));
            }
        }

        let key = battle_id.to_string();
        let Some(value) = self
            .db
            .get(key.as_bytes())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
        else {
            return Ok(None);
        };

        let parsed = decode_replay_value(value.as_ref())?;
        if let Some(cache) = self.replay_json_cache.as_ref() {
            cache
                .lock()
                .map_err(|_| ReplayDbError::new("replay cache mutex poisoned"))?
                .put(battle_id, Arc::new(parsed.clone()));
        }
        Ok(Some(parsed))
    }

    pub fn get_replay_value_lossy(
        &self,
        battle_id: u64,
    ) -> Result<Option<serde_json::Value>, ReplayDbError> {
        let Some(mut value) = self.get_replay_value(battle_id)? else {
            return Ok(None);
        };
        sanitize_replay_value(&mut value);
        Ok(Some(value))
    }

    pub fn list_replay_summaries(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<ReplayListResponse, ReplayDbError> {
        let items = self
            .summaries
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();

        Ok(ReplayListResponse {
            total: self.total(),
            offset,
            limit,
            items,
        })
    }
}

impl ReplaySummary {
    pub fn from_value(value: &serde_json::Value) -> Self {
        let global_snapshots = value["global_snapshots"].as_array();
        let allyteam_snapshots = value["allyteam_snapshots"].as_object();
        let battle_id = value["battle_id"]
            .as_u64()
            .or_else(|| value["battle_id"].as_str().and_then(|raw| raw.parse().ok()))
            .unwrap_or_default();

        Self {
            battle_id,
            replay_filename: value["replay_filename"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            game_version: value["game_version"].as_str().unwrap_or_default().to_string(),
            engine_version: value["engine_version"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            map_name: value["map_name"].as_str().map(str::to_string),
            game_name: value["game_name"].as_str().map(str::to_string),
            players_count: value["players"].as_array().map_or(0, |items| items.len()),
            teams_count: value["teams"].as_array().map_or(0, |items| items.len()),
            global_snapshots: global_snapshots.map_or(0, |items| items.len()),
            allyteam_snapshot_streams: allyteam_snapshots.map_or(0, |items| items.len()),
            allyteam_snapshot_frames: allyteam_snapshots
                .map(|items| {
                    items.values()
                        .map(|snapshots| snapshots.as_array().map_or(0, |frames| frames.len()))
                        .sum()
                })
                .unwrap_or(0),
            commands: value["command_history"].as_array().map_or(0, |items| items.len()),
            events: value["events"].as_array().map_or(0, |items| items.len()),
            first_snapshot_frame: global_snapshots
                .and_then(|items| items.first())
                .and_then(|snapshot| snapshot["frame"].as_u64())
                .map(|frame| frame as u32),
            last_snapshot_frame: global_snapshots
                .and_then(|items| items.last())
                .and_then(|snapshot| snapshot["frame"].as_u64())
                .map(|frame| frame as u32),
        }
    }
}

impl From<&ParsedReplay> for ReplaySummary {
    fn from(replay: &ParsedReplay) -> Self {
        Self {
            battle_id: replay.battle_id,
            replay_filename: replay.replay_filename.clone(),
            game_version: replay.game_version.clone(),
            engine_version: replay.engine_version.clone(),
            map_name: replay.map_name.clone(),
            game_name: replay.game_name.clone(),
            players_count: replay.players.len(),
            teams_count: replay.teams.len(),
            global_snapshots: replay.global_snapshots.len(),
            allyteam_snapshot_streams: replay.allyteam_snapshots.len(),
            allyteam_snapshot_frames: replay
                .allyteam_snapshots
                .values()
                .map(|snapshots| snapshots.len())
                .sum(),
            commands: replay.command_history.len(),
            events: replay.events.len(),
            first_snapshot_frame: replay.global_snapshots.first().map(|snapshot| snapshot.frame),
            last_snapshot_frame: replay.global_snapshots.last().map(|snapshot| snapshot.frame),
        }
    }
}

fn decode_replay(value: &[u8]) -> Result<ParsedReplay, ReplayDbError> {
    let decompressed =
        zstd::decode_all(value).map_err(|err| ReplayDbError::new(err.to_string()))?;
    let parsed = serde_json::from_slice::<ParsedReplay>(&decompressed)
        .map_err(|err| ReplayDbError::new(err.to_string()))?;
    Ok(parsed)
}

fn decode_replay_value(value: &[u8]) -> Result<serde_json::Value, ReplayDbError> {
    let decompressed =
        zstd::decode_all(value).map_err(|err| ReplayDbError::new(err.to_string()))?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&decompressed)
        .map_err(|err| ReplayDbError::new(err.to_string()))?;
    Ok(parsed)
}

fn decode_replay_summary(value: &[u8]) -> Result<ReplaySummary, ReplayDbError> {
    let parsed = decode_replay_value(value)?;
    Ok(ReplaySummary::from_value(&parsed))
}

fn replay_json_cache() -> Option<Arc<Mutex<LruCache<u64, Arc<serde_json::Value>>>>> {
    let size = std::env::var("ZKSCRAPER_REPLAY_JSON_CACHE_SIZE")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(DEFAULT_REPLAY_JSON_CACHE_SIZE);
    NonZeroUsize::new(size).map(|size| Arc::new(Mutex::new(LruCache::new(size))))
}

fn sanitize_replay_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    sanitize_snapshot_array(object.get_mut("global_snapshots"));
    if let Some(allyteam_snapshots) = object
        .get_mut("allyteam_snapshots")
        .and_then(serde_json::Value::as_object_mut)
    {
        for snapshots in allyteam_snapshots.values_mut() {
            sanitize_allyteam_snapshot_array(Some(snapshots));
        }
    }
    sanitize_command_array(object.get_mut("command_history"));
    sanitize_event_array(object.get_mut("events"));
}

fn sanitize_snapshot_array(value: Option<&mut serde_json::Value>) {
    let Some(items) = value.and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for item in items {
        sanitize_snapshot_value(item);
    }
}

fn sanitize_snapshot_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    sanitize_required_float(object.get_mut("game_seconds"));
    if let Some(units) = object.get_mut("units").and_then(serde_json::Value::as_array_mut) {
        for unit in units {
            sanitize_unit_value(unit);
        }
    }
}

fn sanitize_allyteam_snapshot_array(value: Option<&mut serde_json::Value>) {
    let Some(items) = value.and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        sanitize_required_float(object.get_mut("game_seconds"));
        if let Some(units) = object
            .get_mut("los_units")
            .and_then(serde_json::Value::as_array_mut)
        {
            for unit in units {
                sanitize_unit_value(unit);
            }
        }
        if let Some(contacts) = object
            .get_mut("radar_contacts")
            .and_then(serde_json::Value::as_array_mut)
        {
            for contact in contacts {
                sanitize_radar_contact_value(contact);
            }
        }
    }
}

fn sanitize_event_array(value: Option<&mut serde_json::Value>) {
    let Some(items) = value.and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        sanitize_required_float(object.get_mut("game_seconds"));
    }
}

fn sanitize_command_array(value: Option<&mut serde_json::Value>) {
    let Some(items) = value.and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for item in items {
        sanitize_command_value(item);
    }
}

fn sanitize_command_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    sanitize_required_float(object.get_mut("game_seconds"));
    sanitize_float_array(object.get_mut("params"));
    if let Some(selected) = object
        .get_mut("decoded")
        .and_then(serde_json::Value::as_object_mut)
    {
        sanitize_decoded_command_value(selected);
    }
}

fn sanitize_decoded_command_value(value: &mut serde_json::Map<String, serde_json::Value>) {
    if let Some(target) = value.get_mut("target").and_then(serde_json::Value::as_object_mut) {
        match target.get("type").and_then(serde_json::Value::as_str) {
            Some("Position") | Some("position") => {
                sanitize_required_float(target.get_mut("x"));
                sanitize_required_float(target.get_mut("y"));
                sanitize_required_float(target.get_mut("z"));
            }
            Some("Area") | Some("area") => {
                sanitize_required_float(target.get_mut("x"));
                sanitize_required_float(target.get_mut("y"));
                sanitize_required_float(target.get_mut("z"));
                sanitize_required_float(target.get_mut("radius"));
            }
            _ => {}
        }
    }

    if let Some(inserted) = value
        .get_mut("inserted")
        .and_then(serde_json::Value::as_object_mut)
    {
        sanitize_float_array(inserted.get_mut("params"));
        if let Some(decoded) = inserted
            .get_mut("decoded")
            .and_then(serde_json::Value::as_object_mut)
        {
            sanitize_decoded_command_value(decoded);
        }
    }

    if let Some(removed) = value
        .get_mut("removed")
        .and_then(serde_json::Value::as_object_mut)
    {
        sanitize_float_array(removed.get_mut("params"));
    }
}

fn sanitize_unit_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    sanitize_required_float(object.get_mut("x"));
    sanitize_required_float(object.get_mut("y"));
    sanitize_required_float(object.get_mut("z"));
    sanitize_required_float(object.get_mut("hp"));
    sanitize_required_float(object.get_mut("max_hp"));
    sanitize_required_float(object.get_mut("build_progress"));
    sanitize_required_float(object.get_mut("experience"));
}

fn sanitize_radar_contact_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    sanitize_required_float(object.get_mut("x"));
    sanitize_required_float(object.get_mut("y"));
    sanitize_required_float(object.get_mut("z"));
}

fn sanitize_float_array(value: Option<&mut serde_json::Value>) {
    let Some(items) = value.and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for item in items {
        sanitize_required_float(Some(item));
    }
}

fn sanitize_required_float(value: Option<&mut serde_json::Value>) {
    if let Some(slot) = value {
        if slot.is_null() {
            *slot = serde_json::json!(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayDb;
    use crate::parse::{
        AllyTeamSnapshotRecord, CommandOptionFlags, CommandRecord, DecodedCommand, EventRecord,
        MapSize, ParsedReplay, PlayerMetadata, RadarContact, SnapshotRecord, TeamMetadata,
        UnitSnapshot,
    };

    fn sample_replay(battle_id: u64, first_frame: u32) -> ParsedReplay {
        ParsedReplay {
            battle_id,
            replay_filename: format!("{battle_id}.sdfz"),
            game_version: "1.0".to_string(),
            engine_version: "105.1".to_string(),
            map_name: Some("TestMap".to_string()),
            game_name: Some("Zero-K v1.0".to_string()),
            zksearchtag: Some("tag".to_string()),
            players: vec![PlayerMetadata {
                player_id: 1,
                name: Some("Tester".to_string()),
                team: Some(0),
                spectator: false,
                elo: Some(1500),
                lobby_id: Some(123),
                country_code: Some("GB".to_string()),
                clan: Some("ABC".to_string()),
                level: Some(10),
            }],
            teams: vec![TeamMetadata {
                team_id: 0,
                allyteam: Some(0),
                teamleader: Some(1),
                handicap: Some(0.0),
            }],
            map_size: Some(MapSize { x: 16, z: 16 }),
            global_snapshots: vec![SnapshotRecord {
                frame: first_frame,
                game_seconds: 5.0,
                units: vec![UnitSnapshot {
                    unit_id: 1,
                    unit_def_name: "cloakcon".to_string(),
                    team_id: 0,
                    allyteam_id: 0,
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                    hp: 100.0,
                    max_hp: 100.0,
                    build_progress: 1.0,
                    heading: 0,
                    experience: 0.0,
                }],
            }],
            allyteam_snapshots: std::iter::once((
                0,
                vec![AllyTeamSnapshotRecord {
                    allyteam_id: 0,
                    frame: first_frame,
                    game_seconds: 5.0,
                    los_units: vec![],
                    radar_contacts: vec![RadarContact {
                        unit_id: 2,
                        team_id: 1,
                        allyteam_id: 1,
                        x: 4.0,
                        y: 5.0,
                        z: 6.0,
                    }],
                }],
            ))
            .collect(),
            economy_snapshots: std::collections::BTreeMap::new(),
            command_history: vec![CommandRecord {
                frame: first_frame,
                game_seconds: 5.0,
                player_id: 1,
                ai_id: None,
                command_id: 10,
                options: 0,
                params: vec![10.0, 20.0, 30.0],
                selected_unit_ids: vec![1],
                decoded: DecodedCommand {
                    kind: "move".to_string(),
                    option_flags: CommandOptionFlags {
                        alt: false,
                        ctrl: false,
                        meta: false,
                        shift: false,
                        right: false,
                        internal: false,
                    },
                    target: None,
                    state: None,
                    build: None,
                    inserted: None,
                    removed: None,
                },
            }],
            events: vec![EventRecord {
                event_type: "test".to_string(),
                frame: first_frame,
                game_seconds: 5.0,
                payload: serde_json::json!({"ok": true}),
            }],
            springie_stats: vec!["SPRINGIE:stats,test".to_string()],
        }
    }

    #[test]
    fn lists_battle_ids_in_numeric_order() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp_dir = tempfile::tempdir()?;
        let db = sled::open(temp_dir.path())?;
        for battle_id in [10_u64, 2_u64] {
            let replay = sample_replay(battle_id, battle_id as u32);
            let payload = serde_json::to_vec(&replay)?;
            let compressed = zstd::encode_all(payload.as_slice(), 3)?;
            db.insert(battle_id.to_string().as_bytes(), compressed)?;
        }
        db.flush()?;
        drop(db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        assert_eq!(replay_db.battle_ids(), &[2, 10]);

        let page = replay_db.list_replay_summaries(0, 10)?;
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].battle_id, 2);
        assert_eq!(page.items[1].battle_id, 10);
        Ok(())
    }

    #[test]
    fn gets_full_replay_by_battle_id() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let db = sled::open(temp_dir.path())?;
        let replay = sample_replay(42, 120);
        let payload = serde_json::to_vec(&replay)?;
        let compressed = zstd::encode_all(payload.as_slice(), 3)?;
        db.insert(b"42", compressed)?;
        db.flush()?;
        drop(db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        let stored = replay_db.get_replay(42)?.expect("replay should exist");
        assert_eq!(stored.battle_id, 42);
        assert_eq!(stored.global_snapshots[0].frame, 120);
        Ok(())
    }

    #[test]
    fn lists_summaries_even_when_deep_float_fields_are_null(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let db = sled::open(temp_dir.path())?;
        let replay = serde_json::json!({
            "battle_id": 77,
            "replay_filename": "77.sdfz",
            "game_version": "1.0",
            "engine_version": "105.1",
            "map_name": "TestMap",
            "game_name": "Zero-K v1.0",
            "players": [{"player_id": 1}],
            "teams": [{"team_id": 0}],
            "map_size": {"x": 16, "z": 16},
            "global_snapshots": [{
                "frame": 120,
                "game_seconds": 5.0,
                "units": [{
                    "unit_id": 1,
                    "unit_def_name": "cloakcon",
                    "team_id": 0,
                    "allyteam_id": 0,
                    "x": null,
                    "y": 2.0,
                    "z": 3.0,
                    "hp": 100.0,
                    "max_hp": 100.0,
                    "build_progress": 1.0,
                    "heading": 0,
                    "experience": 0.0
                }]
            }],
            "allyteam_snapshots": {"0": []},
            "command_history": [],
            "events": [],
            "springie_stats": []
        });
        let payload = serde_json::to_vec(&replay)?;
        let compressed = zstd::encode_all(payload.as_slice(), 3)?;
        db.insert(b"77", compressed)?;
        db.flush()?;
        drop(db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        let page = replay_db.list_replay_summaries(0, 10)?;
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].battle_id, 77);
        assert_eq!(page.items[0].global_snapshots, 1);
        Ok(())
    }

    #[test]
    fn gets_lossy_replay_json_when_required_float_fields_are_null(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let db = sled::open(temp_dir.path())?;
        let replay = serde_json::json!({
            "battle_id": 88,
            "replay_filename": "88.sdfz",
            "game_version": "1.0",
            "engine_version": "105.1",
            "map_name": "TestMap",
            "game_name": "Zero-K v1.0",
            "players": [],
            "teams": [],
            "map_size": {"x": 16, "z": 16},
            "global_snapshots": [{
                "frame": 120,
                "game_seconds": null,
                "units": [{
                    "unit_id": 1,
                    "unit_def_name": "cloakcon",
                    "team_id": 0,
                    "allyteam_id": 0,
                    "x": null,
                    "y": null,
                    "z": 3.0,
                    "hp": null,
                    "max_hp": 100.0,
                    "build_progress": null,
                    "heading": 0,
                    "experience": null
                }]
            }],
            "allyteam_snapshots": {"0": [{
                "allyteam_id": 0,
                "frame": 120,
                "game_seconds": null,
                "los_units": [],
                "radar_contacts": [{"unit_id": 2, "team_id": 1, "allyteam_id": 1, "x": null, "y": 5.0, "z": null}]
            }]},
            "command_history": [{
                "frame": 120,
                "game_seconds": null,
                "player_id": 1,
                "ai_id": null,
                "command_id": 10,
                "options": 0,
                "params": [null, 2.0],
                "selected_unit_ids": [1],
                "decoded": {
                    "kind": "move",
                    "option_flags": {"alt": false, "ctrl": false, "meta": false, "shift": false, "right": false, "internal": false},
                    "target": {"type": "Position", "x": null, "y": 2.0, "z": null},
                    "state": null,
                    "build": null,
                    "inserted": null,
                    "removed": null
                }
            }],
            "events": [{"event_type": "test", "frame": 120, "game_seconds": null, "payload": {}}],
            "springie_stats": []
        });
        let payload = serde_json::to_vec(&replay)?;
        let compressed = zstd::encode_all(payload.as_slice(), 3)?;
        db.insert(b"88", compressed)?;
        db.flush()?;
        drop(db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        let replay = replay_db
            .get_replay_value_lossy(88)?
            .expect("replay should exist");
        assert_eq!(replay["global_snapshots"][0]["game_seconds"], 0.0);
        assert_eq!(replay["global_snapshots"][0]["units"][0]["x"], 0.0);
        assert_eq!(replay["command_history"][0]["params"][0], 0.0);
        assert_eq!(replay["command_history"][0]["decoded"]["target"]["x"], 0.0);
        assert_eq!(replay["events"][0]["game_seconds"], 0.0);
        Ok(())
    }
}
