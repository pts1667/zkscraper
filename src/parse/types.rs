use std::{collections::BTreeMap, path::PathBuf};

use fastapi::ToSchema;
use serde::{Deserialize, Deserializer, Serialize};

fn deserialize_lossy_f32<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<f32>::deserialize(deserializer)?.unwrap_or(0.0))
}

fn deserialize_lossy_vec_f32<'de, D>(deserializer: D) -> Result<Vec<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Vec::<Option<f32>>::deserialize(deserializer)?
        .into_iter()
        .map(|value| value.unwrap_or(0.0))
        .collect())
}

fn normalize_lossy_f32(value: &mut f32) {
    if !value.is_finite() {
        *value = 0.0;
    }
}

fn normalize_lossy_vec_f32(values: &mut [f32]) {
    for value in values {
        normalize_lossy_f32(value);
    }
}

pub struct ParseReplaySettings {
    pub sdfz_in: PathBuf,
    pub zk_path: PathBuf,
    pub snapshot_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReplayManifestEntry {
    pub replay_id: String,
    pub battle_id: Option<u64>,
    pub headless_id: u64,
    pub replay_filename: String,
    pub game_version: String,
}

#[derive(Debug)]
pub(super) struct ReplayDemInfo {
    pub engine_version: String,
    pub script: String,
    pub springie_stats: Vec<String>,
    pub command_history: Vec<CommandRecord>,
}

#[derive(Debug, Default)]
pub(super) struct ScriptMetadata {
    pub map_name: Option<String>,
    pub game_name: Option<String>,
    pub zksearchtag: Option<String>,
    pub players: Vec<PlayerMetadata>,
    pub teams: Vec<TeamMetadata>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WidgetMeta {
    pub map_size: MapSize,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ParsedReplay {
    #[serde(default)]
    pub replay_id: String,
    #[serde(default)]
    pub battle_id: Option<u64>,
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

impl ParsedReplay {
    pub fn normalize_lossy_floats(&mut self) {
        for snapshot in &mut self.global_snapshots {
            snapshot.normalize_lossy_floats();
        }
        for snapshots in self.allyteam_snapshots.values_mut() {
            for snapshot in snapshots {
                snapshot.normalize_lossy_floats();
            }
        }
        for snapshots in self.economy_snapshots.values_mut() {
            for snapshot in snapshots {
                snapshot.normalize_lossy_floats();
            }
        }
        for command in &mut self.command_history {
            command.normalize_lossy_floats();
        }
        for event in &mut self.events {
            event.normalize_lossy_floats();
        }
    }
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

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SnapshotRecord {
    pub frame: u32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub game_seconds: f32,
    pub units: Vec<UnitSnapshot>,
}

impl SnapshotRecord {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.game_seconds);
        for unit in &mut self.units {
            unit.normalize_lossy_floats();
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct AllyTeamSnapshotRecord {
    pub allyteam_id: u32,
    pub frame: u32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub game_seconds: f32,
    pub los_units: Vec<UnitSnapshot>,
    pub radar_contacts: Vec<RadarContact>,
}

impl AllyTeamSnapshotRecord {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.game_seconds);
        for unit in &mut self.los_units {
            unit.normalize_lossy_floats();
        }
        for contact in &mut self.radar_contacts {
            contact.normalize_lossy_floats();
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EconomySnapshotRecord {
    pub team_id: i32,
    pub allyteam_id: i32,
    pub frame: u32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub game_seconds: f32,
    pub economy: EconomySnapshot,
}

impl EconomySnapshotRecord {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.game_seconds);
        self.economy.normalize_lossy_floats();
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EconomySnapshot {
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_income: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_income: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_stored: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_stored: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_storage: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_storage: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_pull: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_pull: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_expense: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_expense: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_share: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_share: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_sent: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_sent: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub metal_received: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub energy_received: f32,
    #[serde(default, deserialize_with = "deserialize_lossy_f32")]
    pub value_damaged: f32,
    #[serde(default, deserialize_with = "deserialize_lossy_f32")]
    pub value_lost: f32,
}

impl EconomySnapshot {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.metal_income);
        normalize_lossy_f32(&mut self.energy_income);
        normalize_lossy_f32(&mut self.metal_stored);
        normalize_lossy_f32(&mut self.energy_stored);
        normalize_lossy_f32(&mut self.metal_storage);
        normalize_lossy_f32(&mut self.energy_storage);
        normalize_lossy_f32(&mut self.metal_pull);
        normalize_lossy_f32(&mut self.energy_pull);
        normalize_lossy_f32(&mut self.metal_expense);
        normalize_lossy_f32(&mut self.energy_expense);
        normalize_lossy_f32(&mut self.metal_share);
        normalize_lossy_f32(&mut self.energy_share);
        normalize_lossy_f32(&mut self.metal_sent);
        normalize_lossy_f32(&mut self.energy_sent);
        normalize_lossy_f32(&mut self.metal_received);
        normalize_lossy_f32(&mut self.energy_received);
        normalize_lossy_f32(&mut self.value_damaged);
        normalize_lossy_f32(&mut self.value_lost);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct UnitSnapshot {
    pub unit_id: u32,
    pub unit_def_name: String,
    pub team_id: i32,
    pub allyteam_id: i32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub x: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub y: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub z: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub hp: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub max_hp: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub build_progress: f32,
    pub heading: i32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub experience: f32,
}

impl UnitSnapshot {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.x);
        normalize_lossy_f32(&mut self.y);
        normalize_lossy_f32(&mut self.z);
        normalize_lossy_f32(&mut self.hp);
        normalize_lossy_f32(&mut self.max_hp);
        normalize_lossy_f32(&mut self.build_progress);
        normalize_lossy_f32(&mut self.experience);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RadarContact {
    pub unit_id: u32,
    pub team_id: i32,
    pub allyteam_id: i32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub x: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub y: f32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub z: f32,
}

impl RadarContact {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.x);
        normalize_lossy_f32(&mut self.y);
        normalize_lossy_f32(&mut self.z);
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EventPayload {
    #[default]
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Array(Vec<EventPayload>),
    Object(BTreeMap<String, EventPayload>),
}

impl From<bool> for EventPayload {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for EventPayload {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<u64> for EventPayload {
    fn from(value: u64) -> Self {
        Self::U64(value)
    }
}

impl From<f64> for EventPayload {
    fn from(value: f64) -> Self {
        Self::F64(value)
    }
}

impl From<String> for EventPayload {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for EventPayload {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<Vec<EventPayload>> for EventPayload {
    fn from(value: Vec<EventPayload>) -> Self {
        Self::Array(value)
    }
}

impl From<BTreeMap<String, EventPayload>> for EventPayload {
    fn from(value: BTreeMap<String, EventPayload>) -> Self {
        Self::Object(value)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EventRecord {
    pub event_type: String,
    pub frame: u32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub game_seconds: f32,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub payload: EventPayload,
}

impl EventRecord {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.game_seconds);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct CommandRecord {
    pub frame: u32,
    #[serde(deserialize_with = "deserialize_lossy_f32")]
    pub game_seconds: f32,
    pub player_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_id: Option<u32>,
    pub command_id: i32,
    pub options: u32,
    #[serde(deserialize_with = "deserialize_lossy_vec_f32")]
    pub params: Vec<f32>,
    pub selected_unit_ids: Vec<i32>,
    pub decoded: DecodedCommand,
}

impl CommandRecord {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_f32(&mut self.game_seconds);
        normalize_lossy_vec_f32(&mut self.params);
        self.decoded.normalize_lossy_floats();
    }
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

impl DecodedCommand {
    fn normalize_lossy_floats(&mut self) {
        if let Some(target) = self.target.as_mut() {
            target.normalize_lossy_floats();
        }
        if let Some(inserted) = self.inserted.as_mut() {
            inserted.normalize_lossy_floats();
        }
        if let Some(removed) = self.removed.as_mut() {
            removed.normalize_lossy_floats();
        }
    }
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
    Position {
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        x: f32,
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        y: f32,
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        z: f32,
    },
    Area {
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        x: f32,
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        y: f32,
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        z: f32,
        #[serde(deserialize_with = "deserialize_lossy_f32")]
        radius: f32,
    },
}

impl DecodedTarget {
    fn normalize_lossy_floats(&mut self) {
        match self {
            Self::Unit { .. } => {}
            Self::Position { x, y, z } => {
                normalize_lossy_f32(x);
                normalize_lossy_f32(y);
                normalize_lossy_f32(z);
            }
            Self::Area { x, y, z, radius } => {
                normalize_lossy_f32(x);
                normalize_lossy_f32(y);
                normalize_lossy_f32(z);
                normalize_lossy_f32(radius);
            }
        }
    }
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
    #[serde(deserialize_with = "deserialize_lossy_vec_f32")]
    pub params: Vec<f32>,
    #[schema(no_recursion)]
    pub decoded: Box<DecodedCommand>,
}

impl InsertedCommand {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_vec_f32(&mut self.params);
        self.decoded.normalize_lossy_floats();
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RemovedCommand {
    #[serde(deserialize_with = "deserialize_lossy_vec_f32")]
    pub params: Vec<f32>,
    pub mode: String,
}

impl RemovedCommand {
    fn normalize_lossy_floats(&mut self) {
        normalize_lossy_vec_f32(&mut self.params);
    }
}
