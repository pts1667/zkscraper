use std::{collections::BTreeMap, path::PathBuf};

use fastapi::ToSchema;
use serde::{Deserialize, Serialize};

pub struct ParseReplaySettings {
    pub sdfz_in: PathBuf,
    pub zk_path: PathBuf,
    pub snapshot_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReplayManifestEntry {
    pub battle_id: u64,
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
