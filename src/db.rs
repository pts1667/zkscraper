use std::{
    collections::{BTreeSet, HashMap},
    fmt, fs,
    path::Path,
    sync::{Arc, Mutex, RwLock},
};

use fastapi::ToSchema;
use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};

use crate::{
    parse::ParsedReplay,
    storage::{
        assemble_replay, build_partial_replay, decode_cbor, encode_cbor, split_replay,
        ReplayFrameRecord, ReplayMetadataRecord,
    },
};

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
    replay_id_tree: sled::Tree,
    replay_ids: Arc<RwLock<Vec<String>>>,
    summaries: Arc<Mutex<HashMap<String, ReplaySummary>>>,
}

const REPLAY_SUMMARY_TREE: &str = "replay_summaries";
const REPLAY_ID_TREE: &str = "replay_ids";

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ReplaySummary {
    pub replay_id: String,
    pub battle_id: Option<u64>,
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
        let db = open_compressed_db(path)?;
        let replay_id_tree = db
            .open_tree(REPLAY_ID_TREE)
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        let mut replay_ids = Vec::new();
        if replay_id_tree.is_empty() {
            for entry in db.iter() {
                let (key, _) = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
                if let Some(replay_id) = parse_metadata_key(key.as_ref()) {
                    replay_ids.push(replay_id);
                }
            }
        } else {
            for entry in replay_id_tree.iter() {
                let (key, _) = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
                replay_ids.push(
                    std::str::from_utf8(key.as_ref())
                        .map_err(|err| ReplayDbError::new(err.to_string()))?
                        .to_string(),
                );
            }
        }
        replay_ids.sort_by(|left, right| compare_replay_ids(left, right));

        Ok(Self {
            db,
            replay_id_tree,
            replay_ids: Arc::new(RwLock::new(replay_ids)),
            summaries: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn replay_ids(&self) -> Vec<String> {
        self.replay_ids
            .read()
            .map(|replay_ids| replay_ids.clone())
            .unwrap_or_default()
    }

    pub fn total(&self) -> usize {
        self.replay_ids
            .read()
            .map(|replay_ids| replay_ids.len())
            .unwrap_or(0)
    }

    pub fn contains_replay(&self, replay_id: &str) -> Result<bool, ReplayDbError> {
        self.db
            .contains_key(metadata_key(replay_id).as_bytes())
            .map_err(|err| ReplayDbError::new(err.to_string()))
    }

    pub fn put_replay(&self, replay: &ParsedReplay) -> Result<(), ReplayDbError> {
        let (metadata, frames) = split_replay(replay);
        self.clear_existing_frames(&replay.replay_id)?;

        let mut batch = sled::Batch::default();
        batch.insert(
            metadata_key(&replay.replay_id).as_bytes(),
            encode_cbor(&metadata).map_err(|err| ReplayDbError::new(err.to_string()))?,
        );
        for frame in &frames {
            batch.insert(
                frame_key(&replay.replay_id, frame.frame).into_bytes(),
                encode_cbor(frame).map_err(|err| ReplayDbError::new(err.to_string()))?,
            );
        }
        self.db
            .apply_batch(batch)
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        self.replay_id_tree
            .insert(metadata_key(&replay.replay_id).as_bytes(), &[] as &[u8])
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        self.db
            .flush()
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        self.replay_id_tree
            .flush()
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        self.insert_replay_id(replay.replay_id.clone())?;

        self.summaries
            .lock()
            .map_err(|_| ReplayDbError::new("summary cache mutex poisoned"))?
            .insert(replay.replay_id.clone(), ReplaySummary::from(replay));
        Ok(())
    }

    pub fn get_replay(&self, replay_id: &str) -> Result<Option<ParsedReplay>, ReplayDbError> {
        let Some(metadata) = self.get_metadata_record(replay_id)? else {
            return Ok(None);
        };
        let mut frames = Vec::with_capacity(metadata.snapshot_frames.len());
        for frame in &metadata.snapshot_frames {
            let Some(frame_record) = self.get_frame_record(replay_id, *frame)? else {
                return Err(ReplayDbError::new(format!(
                    "replay_id {replay_id} is missing frame row {frame}"
                )));
            };
            frames.push(frame_record);
        }
        Ok(Some(assemble_replay(metadata, frames)))
    }

    pub fn get_replay_lossy(&self, replay_id: &str) -> Result<Option<ParsedReplay>, ReplayDbError> {
        let Some(mut replay) = self.get_replay(replay_id)? else {
            return Ok(None);
        };
        replay.normalize_lossy_floats();
        Ok(Some(replay))
    }

    pub fn get_replay_frame_lossy(
        &self,
        replay_id: &str,
        frame: u32,
    ) -> Result<Option<ParsedReplay>, ReplayDbError> {
        let Some(metadata) = self.get_metadata_record(replay_id)? else {
            return Ok(None);
        };
        let Some(frame_record) = self.get_frame_record(replay_id, frame)? else {
            return Ok(None);
        };
        let mut replay = build_partial_replay(metadata, frame_record);
        replay.normalize_lossy_floats();
        Ok(Some(replay))
    }

    pub fn get_replay_frames(&self, replay_id: &str) -> Result<Option<Vec<u32>>, ReplayDbError> {
        let Some(metadata) = self.get_metadata_record(replay_id)? else {
            return Ok(None);
        };
        Ok(Some(metadata.snapshot_frames))
    }

    pub fn list_replay_summaries(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<ReplayListResponse, ReplayDbError> {
        let replay_ids = self.replay_ids_snapshot()?;
        let items = replay_ids
            .iter()
            .skip(offset)
            .take(limit)
            .map(|replay_id| self.get_replay_summary(replay_id))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ReplayListResponse {
            total: self.total(),
            offset,
            limit,
            items,
        })
    }

    pub fn build_summary_index(&self) -> Result<usize, ReplayDbError> {
        self.refresh_metadata()
    }

    pub fn next_local_replay_id(&self) -> Result<String, ReplayDbError> {
        let replay_ids = self.replay_ids_snapshot()?;
        let next = replay_ids
            .iter()
            .filter_map(|replay_id| replay_id.strip_prefix("local-"))
            .filter_map(|suffix| suffix.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        Ok(format!("local-{next}"))
    }

    fn get_replay_summary(&self, replay_id: &str) -> Result<ReplaySummary, ReplayDbError> {
        if let Some(summary) = self
            .summaries
            .lock()
            .map_err(|_| ReplayDbError::new("summary cache mutex poisoned"))?
            .get(replay_id)
            .cloned()
        {
            return Ok(summary);
        }

        let Some(metadata) = self.get_metadata_record(replay_id)? else {
            return Err(ReplayDbError::new(format!(
                "replay_id {replay_id} disappeared from the replay DB"
            )));
        };
        let summary = ReplaySummary::from_metadata(&metadata);
        self.summaries
            .lock()
            .map_err(|_| ReplayDbError::new("summary cache mutex poisoned"))?
            .insert(replay_id.to_string(), summary.clone());
        Ok(summary)
    }

    fn clear_existing_frames(&self, replay_id: &str) -> Result<(), ReplayDbError> {
        let Some(existing) = self.get_metadata_record(replay_id)? else {
            return Ok(());
        };
        let mut batch = sled::Batch::default();
        for frame in existing.snapshot_frames {
            batch.remove(frame_key(replay_id, frame).into_bytes());
        }
        self.db
            .apply_batch(batch)
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        Ok(())
    }

    fn get_metadata_record(
        &self,
        replay_id: &str,
    ) -> Result<Option<ReplayMetadataRecord>, ReplayDbError> {
        let Some(value) = self
            .db
            .get(metadata_key(replay_id).as_bytes())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
        else {
            return Ok(None);
        };
        decode_cbor(value.as_ref())
            .map(Some)
            .map_err(|err| ReplayDbError::new(err.to_string()))
    }

    fn get_frame_record(
        &self,
        replay_id: &str,
        frame: u32,
    ) -> Result<Option<ReplayFrameRecord>, ReplayDbError> {
        let Some(value) = self
            .db
            .get(frame_key(replay_id, frame).as_bytes())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
        else {
            return Ok(None);
        };
        decode_cbor(value.as_ref())
            .map(Some)
            .map_err(|err| ReplayDbError::new(err.to_string()))
    }

    pub fn refresh_metadata(&self) -> Result<usize, ReplayDbError> {
        let _ = self.db.drop_tree(REPLAY_SUMMARY_TREE);
        self.summaries
            .lock()
            .map_err(|_| ReplayDbError::new("summary cache mutex poisoned"))?
            .clear();
        self.replay_id_tree
            .clear()
            .map_err(|err| ReplayDbError::new(err.to_string()))?;

        let replay_ids = self.replay_ids_snapshot()?;
        let pb = ProgressBar::new(replay_ids.len() as u64);
        pb.set_message("refreshing replay metadata");
        let mut refreshed = 0usize;

        for replay_id in replay_ids {
            let Some(mut metadata) = self.get_metadata_record(&replay_id)? else {
                return Err(ReplayDbError::new(format!(
                    "replay_id {replay_id} disappeared from the replay DB"
                )));
            };
            recompute_metadata_stats(self, &replay_id, &mut metadata)?;
            self.db
                .insert(
                    metadata_key(&replay_id).as_bytes(),
                    encode_cbor(&metadata).map_err(|err| ReplayDbError::new(err.to_string()))?,
                )
                .map_err(|err| ReplayDbError::new(err.to_string()))?;
            self.replay_id_tree
                .insert(metadata_key(&replay_id).as_bytes(), &[] as &[u8])
                .map_err(|err| ReplayDbError::new(err.to_string()))?;
            refreshed += 1;
            pb.inc(1);
        }

        self.db
            .flush()
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        self.replay_id_tree
            .flush()
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        self.replace_replay_ids(self.load_replay_ids_from_tree()?)?;
        pb.finish_with_message(format!("refreshed {refreshed} replay metadata row(s)"));
        Ok(refreshed)
    }

    fn replay_ids_snapshot(&self) -> Result<Vec<String>, ReplayDbError> {
        self.replay_ids
            .read()
            .map(|replay_ids| replay_ids.clone())
            .map_err(|_| ReplayDbError::new("replay id cache rwlock poisoned"))
    }

    fn insert_replay_id(&self, replay_id: String) -> Result<(), ReplayDbError> {
        let mut replay_ids = self
            .replay_ids
            .write()
            .map_err(|_| ReplayDbError::new("replay id cache rwlock poisoned"))?;
        match replay_ids.binary_search_by(|existing| compare_replay_ids(existing, &replay_id)) {
            Ok(_) => {}
            Err(index) => replay_ids.insert(index, replay_id),
        }
        Ok(())
    }

    fn replace_replay_ids(&self, replay_ids: Vec<String>) -> Result<(), ReplayDbError> {
        let mut cached = self
            .replay_ids
            .write()
            .map_err(|_| ReplayDbError::new("replay id cache rwlock poisoned"))?;
        *cached = replay_ids;
        Ok(())
    }

    fn load_replay_ids_from_tree(&self) -> Result<Vec<String>, ReplayDbError> {
        let mut replay_ids = Vec::new();
        for entry in self.replay_id_tree.iter() {
            let (key, _) = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
            replay_ids.push(
                std::str::from_utf8(key.as_ref())
                    .map_err(|err| ReplayDbError::new(err.to_string()))?
                    .to_string(),
            );
        }
        replay_ids.sort_by(|left, right| compare_replay_ids(left, right));
        Ok(replay_ids)
    }
}

fn open_compressed_db(path: impl AsRef<Path>) -> Result<sled::Db, ReplayDbError> {
    sled::Config::default()
        .path(path)
        .use_compression(true)
        .compression_factor(5)
        .open()
        .map_err(|err| ReplayDbError::new(err.to_string()))
}

pub fn migrate_legacy_db(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
) -> Result<(), ReplayDbError> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    if dst.exists() {
        return Err(ReplayDbError::new(format!(
            "destination DB path already exists: {}",
            dst.display()
        )));
    }
    fs::create_dir_all(dst).map_err(|err| ReplayDbError::new(err.to_string()))?;

    let src_db = sled::open(src).map_err(|err| ReplayDbError::new(err.to_string()))?;
    let dst_db = ReplayDb::open(dst)?;
    let total = src_db
        .iter()
        .filter_map(|entry| entry.ok())
        .filter(|(key, _)| parse_metadata_key(key.as_ref()).is_some())
        .count() as u64;
    let pb = ProgressBar::new(total);
    pb.set_message("migrating legacy replay DB");
    let mut migrated = 0usize;

    for entry in src_db.iter() {
        let (key, value) = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
        let Some(_) = parse_metadata_key(key.as_ref()) else {
            continue;
        };
        let replay = decode_legacy_replay(value.as_ref())?;
        dst_db.put_replay(&replay)?;
        migrated += 1;
        pb.inc(1);
    }

    if migrated == 0 {
        pb.finish_and_clear();
        return Err(ReplayDbError::new(
            "source DB contained no legacy replay rows",
        ));
    }
    pb.finish_with_message(format!("migrated {migrated} replay(s)"));
    Ok(())
}

pub fn migrate_replay_id_db(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
) -> Result<(), ReplayDbError> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    fs::create_dir_all(dst).map_err(|err| ReplayDbError::new(err.to_string()))?;

    let src_db = open_compressed_db(src)?;
    let src_replay_id_tree = src_db
        .open_tree(REPLAY_ID_TREE)
        .map_err(|err| ReplayDbError::new(err.to_string()))?;
    let dst_db = open_compressed_db(dst)?;
    let dst_replay_id_tree = dst_db
        .open_tree(REPLAY_ID_TREE)
        .map_err(|err| ReplayDbError::new(err.to_string()))?;

    let total = src_replay_id_tree.len() as u64;
    let pb = ProgressBar::new(total);
    pb.set_message("migrating replay-id DB");
    let mut migrated = 0usize;
    let mut skipped = 0usize;

    for entry in src_replay_id_tree.iter() {
        let (key, _) = entry.map_err(|err| ReplayDbError::new(err.to_string()))?;
        let replay_id = std::str::from_utf8(key.as_ref())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
            .to_string();

        if dst_replay_id_tree
            .contains_key(key.as_ref())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
        {
            skipped += 1;
            pb.inc(1);
            continue;
        }

        let Some(metadata_value) = src_db
            .get(key.as_ref())
            .map_err(|err| ReplayDbError::new(err.to_string()))?
        else {
            return Err(ReplayDbError::new(format!(
                "replay_id {replay_id} is missing metadata row"
            )));
        };
        let mut metadata: ReplayMetadataRecord = decode_cbor(metadata_value.as_ref())
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        if metadata.replay_id.is_empty() {
            metadata.replay_id = replay_id.clone();
        }
        if metadata.battle_id.is_none() {
            metadata.battle_id = replay_id.parse::<u64>().ok();
        }

        let mut batch = sled::Batch::default();
        batch.insert(
            metadata_key(&replay_id).into_bytes(),
            encode_cbor(&metadata).map_err(|err| ReplayDbError::new(err.to_string()))?,
        );
        for frame in &metadata.snapshot_frames {
            let frame_key_value = frame_key(&replay_id, *frame);
            let Some(frame_value) = src_db
                .get(frame_key_value.as_bytes())
                .map_err(|err| ReplayDbError::new(err.to_string()))?
            else {
                return Err(ReplayDbError::new(format!(
                    "replay_id {replay_id} is missing frame row {frame}"
                )));
            };
            batch.insert(frame_key_value.into_bytes(), frame_value);
        }

        dst_db
            .apply_batch(batch)
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        dst_replay_id_tree
            .insert(metadata_key(&replay_id).as_bytes(), &[] as &[u8])
            .map_err(|err| ReplayDbError::new(err.to_string()))?;
        migrated += 1;
        pb.inc(1);
    }

    dst_db
        .flush()
        .map_err(|err| ReplayDbError::new(err.to_string()))?;
    dst_replay_id_tree
        .flush()
        .map_err(|err| ReplayDbError::new(err.to_string()))?;

    if migrated == 0 && skipped == 0 {
        pb.finish_and_clear();
        return Err(ReplayDbError::new("source DB contained no replay rows"));
    }
    pb.finish_with_message(format!(
        "migrated {migrated} replay(s), skipped {skipped} already present replay(s)"
    ));
    Ok(())
}

impl ReplaySummary {
    fn from_metadata(metadata: &ReplayMetadataRecord) -> Self {
        Self {
            replay_id: metadata.replay_id.clone(),
            battle_id: metadata.battle_id,
            replay_filename: metadata.replay_filename.clone(),
            game_version: metadata.game_version.clone(),
            engine_version: metadata.engine_version.clone(),
            map_name: metadata.map_name.clone(),
            game_name: metadata.game_name.clone(),
            players_count: metadata.players.len(),
            teams_count: metadata.teams.len(),
            global_snapshots: metadata.global_snapshot_count,
            allyteam_snapshot_streams: metadata.allyteam_snapshot_streams,
            allyteam_snapshot_frames: metadata.allyteam_snapshot_frames,
            commands: metadata.command_history.len(),
            events: metadata.events.len(),
            first_snapshot_frame: metadata.first_snapshot_frame,
            last_snapshot_frame: metadata.last_snapshot_frame,
        }
    }

}

impl From<&ParsedReplay> for ReplaySummary {
    fn from(replay: &ParsedReplay) -> Self {
        Self {
            replay_id: replay.replay_id.clone(),
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
            first_snapshot_frame: replay
                .global_snapshots
                .first()
                .map(|snapshot| snapshot.frame),
            last_snapshot_frame: replay
                .global_snapshots
                .last()
                .map(|snapshot| snapshot.frame),
        }
    }
}

fn decode_legacy_replay(value: &[u8]) -> Result<ParsedReplay, ReplayDbError> {
    let decompressed =
        zstd::decode_all(value).map_err(|err| ReplayDbError::new(err.to_string()))?;
    let mut replay: ParsedReplay =
        serde_json::from_slice(&decompressed).map_err(|err| ReplayDbError::new(err.to_string()))?;
    replay.normalize_lossy_floats();
    if replay.replay_id.is_empty() {
        replay.replay_id = replay
            .battle_id
            .map(|battle_id| battle_id.to_string())
            .unwrap_or_default();
    }
    Ok(replay)
}

fn metadata_key(replay_id: &str) -> String {
    replay_id.to_string()
}

fn frame_key(replay_id: &str, frame: u32) -> String {
    format!("{replay_id}_frame_{frame}")
}

fn parse_metadata_key(key: &[u8]) -> Option<String> {
    let key = std::str::from_utf8(key).ok()?;
    if key.contains("_frame_") {
        return None;
    }
    Some(key.to_string())
}

fn recompute_metadata_stats(
    db: &ReplayDb,
    replay_id: &str,
    metadata: &mut ReplayMetadataRecord,
) -> Result<(), ReplayDbError> {
    let mut global_snapshot_count = 0usize;
    let mut allyteam_ids = BTreeSet::new();
    let mut allyteam_snapshot_frames = 0usize;
    let mut first_snapshot_frame = None;
    let mut last_snapshot_frame = None;

    for frame in &metadata.snapshot_frames {
        let Some(frame_record) = db.get_frame_record(replay_id, *frame)? else {
            return Err(ReplayDbError::new(format!(
                "replay_id {replay_id} is missing frame row {frame}"
            )));
        };
        if frame_record.global_snapshot.is_some() {
            global_snapshot_count += 1;
            if first_snapshot_frame.is_none() {
                first_snapshot_frame = Some(*frame);
            }
            last_snapshot_frame = Some(*frame);
        }
        allyteam_snapshot_frames += frame_record.allyteam_snapshots.len();
        allyteam_ids.extend(frame_record.allyteam_snapshots.keys().copied());
    }

    metadata.global_snapshot_count = global_snapshot_count;
    metadata.allyteam_snapshot_streams = allyteam_ids.len();
    metadata.allyteam_snapshot_frames = allyteam_snapshot_frames;
    metadata.first_snapshot_frame = first_snapshot_frame;
    metadata.last_snapshot_frame = last_snapshot_frame;
    Ok(())
}

fn compare_replay_ids(left: &str, right: &str) -> std::cmp::Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left_num), Ok(right_num)) => left_num.cmp(&right_num),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

#[cfg(test)]
mod tests {
    use super::{migrate_legacy_db, ReplayDb};
    use crate::parse::{
        AllyTeamSnapshotRecord, CommandOptionFlags, CommandRecord, DecodedCommand, EconomySnapshot,
        EconomySnapshotRecord, EventPayload, EventRecord, MapSize, ParsedReplay,
        PlayerMetadata, RadarContact, SnapshotRecord, TeamMetadata, UnitSnapshot,
    };

    fn sample_replay(battle_id: u64, first_frame: u32) -> ParsedReplay {
        ParsedReplay {
            replay_id: battle_id.to_string(),
            battle_id: Some(battle_id),
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
            economy_snapshots: std::iter::once((
                0,
                vec![EconomySnapshotRecord {
                    team_id: 0,
                    allyteam_id: 0,
                    frame: first_frame,
                    game_seconds: 5.0,
                    economy: EconomySnapshot {
                        metal_income: 1.0,
                        energy_income: 2.0,
                        metal_stored: 3.0,
                        energy_stored: 4.0,
                        metal_storage: 5.0,
                        energy_storage: 6.0,
                        metal_pull: 7.0,
                        energy_pull: 8.0,
                        metal_expense: 9.0,
                        energy_expense: 10.0,
                        metal_share: 11.0,
                        energy_share: 12.0,
                        metal_sent: 13.0,
                        energy_sent: 14.0,
                        metal_received: 15.0,
                        energy_received: 16.0,
                    },
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
                payload: EventPayload::Object(
                    std::iter::once(("ok".to_string(), EventPayload::Bool(true))).collect(),
                ),
            }],
            springie_stats: vec!["SPRINGIE:stats,test".to_string()],
        }
    }

    #[test]
    fn lists_battle_ids_in_numeric_order() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;
        for battle_id in [10_u64, 2_u64] {
            replay_db.put_replay(&sample_replay(battle_id, battle_id as u32))?;
        }
        drop(replay_db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        assert_eq!(
            replay_db.replay_ids(),
            vec!["2".to_string(), "10".to_string()]
        );

        let page = replay_db.list_replay_summaries(0, 10)?;
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].battle_id, Some(2));
        assert_eq!(page.items[1].battle_id, Some(10));
        Ok(())
    }

    #[test]
    fn gets_full_replay_by_battle_id() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;
        replay_db.put_replay(&sample_replay(42, 120))?;
        drop(replay_db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        let stored = replay_db.get_replay("42")?.expect("replay should exist");
        assert_eq!(stored.battle_id, Some(42));
        assert_eq!(stored.global_snapshots[0].frame, 120);
        Ok(())
    }

    #[test]
    fn filters_frame_keys_from_battle_listing() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;
        replay_db.put_replay(&sample_replay(42, 120))?;
        drop(replay_db);

        let replay_db = ReplayDb::open(temp_dir.path())?;
        assert_eq!(replay_db.replay_ids(), vec!["42".to_string()]);
        Ok(())
    }

    #[test]
    fn put_replay_updates_live_battle_id_index() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;

        replay_db.put_replay(&sample_replay(10, 120))?;
        replay_db.put_replay(&sample_replay(2, 240))?;

        assert_eq!(
            replay_db.replay_ids(),
            vec!["2".to_string(), "10".to_string()]
        );
        let page = replay_db.list_replay_summaries(0, 10)?;
        assert_eq!(page.total, 2);
        assert_eq!(page.items[0].battle_id, Some(2));
        assert_eq!(page.items[1].battle_id, Some(10));
        Ok(())
    }

    #[test]
    fn loads_lossy_frame_lookup() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;
        replay_db.put_replay(&sample_replay(42, 120))?;

        let replay = replay_db
            .get_replay_frame_lossy("42", 120)?
            .expect("frame should exist");
        assert_eq!(replay.global_snapshots[0].frame, 120);
        assert_eq!(replay.allyteam_snapshots.get(&0).unwrap()[0].frame, 120);
        assert_eq!(replay.economy_snapshots.get(&0).unwrap()[0].frame, 120);
        Ok(())
    }

    #[test]
    fn migrates_legacy_replay_rows() -> Result<(), Box<dyn std::error::Error>> {
        let src_dir = tempfile::tempdir()?;
        let dst_dir = tempfile::tempdir()?;
        let legacy_db = sled::open(src_dir.path())?;
        let replay = sample_replay(88, 240);
        let payload = serde_json::to_vec(&replay)?;
        let compressed = zstd::encode_all(payload.as_slice(), 3)?;
        legacy_db.insert(b"88", compressed)?;
        legacy_db.flush()?;
        drop(legacy_db);

        let dst_path = dst_dir.path().join("migrated");
        migrate_legacy_db(src_dir.path(), &dst_path)?;
        let replay_db = ReplayDb::open(&dst_path)?;
        let replay = replay_db
            .get_replay("88")?
            .expect("migrated replay should exist");
        assert_eq!(replay.battle_id, Some(88));
        assert_eq!(replay.global_snapshots[0].frame, 240);
        Ok(())
    }

    #[test]
    fn loads_lossy_replay_values() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let replay_db = ReplayDb::open(temp_dir.path())?;
        replay_db.put_replay(&sample_replay(88, 120))?;

        let replay = replay_db
            .get_replay_lossy("88")?
            .expect("replay should exist");
        assert_eq!(replay.global_snapshots[0].game_seconds, 5.0);
        assert_eq!(replay.global_snapshots[0].units[0].x, 1.0);
        assert_eq!(replay.command_history[0].params[0], 10.0);
        Ok(())
    }
}
