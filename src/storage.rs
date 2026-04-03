use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::parse::{
    AllyTeamSnapshotRecord, CommandRecord, EconomySnapshotRecord, EventRecord, MapSize,
    ParsedReplay, PlayerMetadata, SnapshotRecord, TeamMetadata,
};

#[derive(Debug)]
pub struct StorageError(String);

impl StorageError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StorageError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayMetadataRecord {
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
    pub command_history: Vec<CommandRecord>,
    pub events: Vec<EventRecord>,
    pub springie_stats: Vec<String>,
    pub snapshot_frames: Vec<u32>,
    #[serde(default)]
    pub global_snapshot_count: usize,
    #[serde(default)]
    pub allyteam_snapshot_streams: usize,
    #[serde(default)]
    pub allyteam_snapshot_frames: usize,
    #[serde(default)]
    pub first_snapshot_frame: Option<u32>,
    #[serde(default)]
    pub last_snapshot_frame: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFrameRecord {
    pub frame: u32,
    pub global_snapshot: Option<SnapshotRecord>,
    pub allyteam_snapshots: BTreeMap<u32, AllyTeamSnapshotRecord>,
    pub economy_snapshots: BTreeMap<u32, EconomySnapshotRecord>,
}

pub fn encode_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    minicbor_serde::to_vec(value).map_err(|err| StorageError::new(err.to_string()))
}

pub fn decode_cbor<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StorageError> {
    minicbor_serde::from_slice(bytes).map_err(|err| StorageError::new(err.to_string()))
}

pub fn split_replay(replay: &ParsedReplay) -> (ReplayMetadataRecord, Vec<ReplayFrameRecord>) {
    let mut frame_map: BTreeMap<u32, ReplayFrameRecord> = BTreeMap::new();
    for snapshot in &replay.global_snapshots {
        frame_map.insert(
            snapshot.frame,
            ReplayFrameRecord {
                frame: snapshot.frame,
                global_snapshot: Some(snapshot.clone()),
                allyteam_snapshots: BTreeMap::new(),
                economy_snapshots: BTreeMap::new(),
            },
        );
    }
    for (&allyteam_id, snapshots) in &replay.allyteam_snapshots {
        for snapshot in snapshots {
            frame_map
                .entry(snapshot.frame)
                .or_insert_with(|| ReplayFrameRecord {
                    frame: snapshot.frame,
                    global_snapshot: None,
                    allyteam_snapshots: BTreeMap::new(),
                    economy_snapshots: BTreeMap::new(),
                })
                .allyteam_snapshots
                .insert(allyteam_id, snapshot.clone());
        }
    }
    for (&team_id, snapshots) in &replay.economy_snapshots {
        for snapshot in snapshots {
            frame_map
                .entry(snapshot.frame)
                .or_insert_with(|| ReplayFrameRecord {
                    frame: snapshot.frame,
                    global_snapshot: None,
                    allyteam_snapshots: BTreeMap::new(),
                    economy_snapshots: BTreeMap::new(),
                })
                .economy_snapshots
                .insert(team_id, snapshot.clone());
        }
    }

    let snapshot_frames = frame_map.keys().copied().collect::<Vec<_>>();
    (
        ReplayMetadataRecord {
            battle_id: replay.battle_id,
            replay_filename: replay.replay_filename.clone(),
            game_version: replay.game_version.clone(),
            engine_version: replay.engine_version.clone(),
            map_name: replay.map_name.clone(),
            game_name: replay.game_name.clone(),
            zksearchtag: replay.zksearchtag.clone(),
            players: replay.players.clone(),
            teams: replay.teams.clone(),
            map_size: replay.map_size.clone(),
            command_history: replay.command_history.clone(),
            events: replay.events.clone(),
            springie_stats: replay.springie_stats.clone(),
            snapshot_frames,
            global_snapshot_count: replay.global_snapshots.len(),
            allyteam_snapshot_streams: replay.allyteam_snapshots.len(),
            allyteam_snapshot_frames: replay
                .allyteam_snapshots
                .values()
                .map(|snapshots| snapshots.len())
                .sum(),
            first_snapshot_frame: replay
                .global_snapshots
                .first()
                .map(|snapshot| snapshot.frame),
            last_snapshot_frame: replay
                .global_snapshots
                .last()
                .map(|snapshot| snapshot.frame),
        },
        frame_map.into_values().collect(),
    )
}

pub fn assemble_replay(
    metadata: ReplayMetadataRecord,
    frames: Vec<ReplayFrameRecord>,
) -> ParsedReplay {
    let mut global_snapshots = Vec::new();
    let mut allyteam_snapshots: BTreeMap<u32, Vec<AllyTeamSnapshotRecord>> = BTreeMap::new();
    let mut economy_snapshots: BTreeMap<u32, Vec<EconomySnapshotRecord>> = BTreeMap::new();

    for frame in frames {
        if let Some(global_snapshot) = frame.global_snapshot {
            global_snapshots.push(global_snapshot);
        }
        for (allyteam_id, snapshot) in frame.allyteam_snapshots {
            allyteam_snapshots
                .entry(allyteam_id)
                .or_default()
                .push(snapshot);
        }
        for (team_id, snapshot) in frame.economy_snapshots {
            economy_snapshots.entry(team_id).or_default().push(snapshot);
        }
    }

    ParsedReplay {
        battle_id: metadata.battle_id,
        replay_filename: metadata.replay_filename,
        game_version: metadata.game_version,
        engine_version: metadata.engine_version,
        map_name: metadata.map_name,
        game_name: metadata.game_name,
        zksearchtag: metadata.zksearchtag,
        players: metadata.players,
        teams: metadata.teams,
        map_size: metadata.map_size,
        global_snapshots,
        allyteam_snapshots,
        economy_snapshots,
        command_history: metadata.command_history,
        events: metadata.events,
        springie_stats: metadata.springie_stats,
    }
}

pub fn build_partial_replay(
    metadata: ReplayMetadataRecord,
    frame: ReplayFrameRecord,
) -> ParsedReplay {
    assemble_replay(metadata, vec![frame])
}
