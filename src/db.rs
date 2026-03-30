use std::{
    fmt,
    path::Path,
    sync::Arc,
};

use fastapi::ToSchema;
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
}

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
        let mut battle_ids = Vec::with_capacity(db.len());
        for entry in db.iter().keys() {
            let key = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
            let key = std::str::from_utf8(key.as_ref())
                .map_err(|err| ReplayDbError::new(err.to_string()))?;
            let battle_id = key.parse::<u64>().map_err(|err| {
                ReplayDbError::new(format!("invalid battle id key '{key}': {err}"))
            })?;
            battle_ids.push(battle_id);
        }
        battle_ids.sort_unstable();

        Ok(Self {
            db,
            battle_ids: Arc::new(battle_ids),
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

    pub fn list_replay_summaries(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<ReplayListResponse, ReplayDbError> {
        let items = self
            .battle_ids
            .iter()
            .skip(offset)
            .take(limit)
            .map(|battle_id| {
                self.get_replay(*battle_id)?
                    .map(|replay| ReplaySummary::from(&replay))
                    .ok_or_else(|| {
                        ReplayDbError::new(format!(
                            "battle id {} disappeared while listing replays",
                            battle_id
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ReplayListResponse {
            total: self.total(),
            offset,
            limit,
            items,
        })
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
}
