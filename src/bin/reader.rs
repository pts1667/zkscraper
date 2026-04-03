use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zkscraper::db::{ReplayDb, ReplaySummary};

#[derive(Parser, Debug)]
#[command(version, about = "Inspect parsed replay records from the sled DB")]
struct Args {
    /// Path to the parsed replay sled DB
    #[arg(long)]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List stored replay keys
    List {
        /// Maximum number of battle IDs to print
        #[arg(long)]
        max_count: Option<usize>,
    },

    /// Show a compact summary for one replay
    Show {
        /// Replay ID key
        #[arg(long)]
        replay_id: String,

        /// Read via the lossy JSON path that normalizes null float fields
        #[arg(long)]
        lossy: bool,
    },

    /// Refresh replay metadata counters and delete any legacy summaries tree
    #[command(name = "refresh-metadata")]
    RefreshMetadata,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    match args.command {
        Command::List { max_count } => {
            for replay_id in list_replay_ids(&args.db, max_count)? {
                println!("{replay_id}");
            }
        }
        Command::Show { replay_id, lossy } => {
            let db = ReplayDb::open(args.db)?;
            if lossy {
                let parsed = db
                    .get_replay_value_lossy(&replay_id)?
                    .ok_or_else(|| format!("replay_id {} not found", replay_id))?;
                print_replay_value_summary(&parsed);
            } else {
                let parsed = db
                    .get_replay(&replay_id)?
                    .ok_or_else(|| format!("replay_id {} not found", replay_id))?;
                print_parsed_replay_summary(&parsed);
            }
        }
        Command::RefreshMetadata => {
            let db = ReplayDb::open(args.db)?;
            let refreshed = db.refresh_metadata()?;
            println!("refreshed_metadata_rows: {refreshed}");
        }
    }

    Ok(())
}

fn print_parsed_replay_summary(parsed: &zkscraper::parse::ParsedReplay) {
    let summary = ReplaySummary::from(parsed);
    let first_snapshot_units = parsed
        .global_snapshots
        .first()
        .map(|snapshot| snapshot.units.len())
        .unwrap_or(0);
    let last_event = parsed
        .events
        .last()
        .map(|event| event.event_type.as_str())
        .unwrap_or("<none>");
    let event_preview = parsed
        .events
        .iter()
        .take(6)
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let player_names = parsed
        .players
        .iter()
        .filter_map(|player| player.name.as_deref())
        .collect::<Vec<_>>()
        .join(", ");

    println!("replay_id: {}", parsed.replay_id);
    println!("battle_id: {:?}", parsed.battle_id);
    println!("replay_filename: {}", parsed.replay_filename);
    println!("game_version: {}", parsed.game_version);
    println!("engine_version: {}", parsed.engine_version);
    println!("map_name: {:?}", parsed.map_name);
    println!("game_name: {:?}", parsed.game_name);
    println!("players: {}", summary.players_count);
    println!("teams: {}", summary.teams_count);
    println!("map_size: {:?}", parsed.map_size);
    println!("global_snapshots: {}", summary.global_snapshots);
    println!(
        "allyteam_snapshot_streams: {}",
        summary.allyteam_snapshot_streams
    );
    println!(
        "allyteam_snapshot_frames: {}",
        summary.allyteam_snapshot_frames
    );
    println!("commands: {}", summary.commands);
    println!("events: {}", summary.events);
    println!(
        "first_snapshot_frame: {}",
        summary
            .first_snapshot_frame
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!(
        "last_snapshot_frame: {}",
        summary
            .last_snapshot_frame
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!("first_snapshot_units: {}", first_snapshot_units);
    println!("last_event_type: {}", last_event);
    println!("event_preview: {}", event_preview);
    println!("player_names: {}", player_names);
}

fn print_replay_value_summary(parsed: &serde_json::Value) {
    let summary = ReplaySummary::from_value(parsed);
    let first_snapshot_units = parsed["global_snapshots"]
        .as_array()
        .and_then(|snapshots| snapshots.first())
        .and_then(|snapshot| snapshot["units"].as_array())
        .map(|units| units.len())
        .unwrap_or(0);
    let last_event = parsed["events"]
        .as_array()
        .and_then(|events| events.last())
        .and_then(|event| event["event_type"].as_str())
        .unwrap_or("<none>");
    let event_preview = parsed["events"]
        .as_array()
        .into_iter()
        .flat_map(|events| events.iter().take(6))
        .filter_map(|event| event["event_type"].as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let player_names = parsed["players"]
        .as_array()
        .into_iter()
        .flat_map(|players| players.iter())
        .filter_map(|player| player["name"].as_str())
        .collect::<Vec<_>>()
        .join(", ");

    println!("replay_id: {}", summary.replay_id);
    println!("battle_id: {:?}", summary.battle_id);
    println!(
        "replay_filename: {}",
        parsed["replay_filename"].as_str().unwrap_or_default()
    );
    println!(
        "game_version: {}",
        parsed["game_version"].as_str().unwrap_or_default()
    );
    println!(
        "engine_version: {}",
        parsed["engine_version"].as_str().unwrap_or_default()
    );
    println!("map_name: {:?}", parsed["map_name"].as_str());
    println!("game_name: {:?}", parsed["game_name"].as_str());
    println!("players: {}", summary.players_count);
    println!("teams: {}", summary.teams_count);
    println!("map_size: {}", parsed["map_size"]);
    println!("global_snapshots: {}", summary.global_snapshots);
    println!(
        "allyteam_snapshot_streams: {}",
        summary.allyteam_snapshot_streams
    );
    println!(
        "allyteam_snapshot_frames: {}",
        summary.allyteam_snapshot_frames
    );
    println!("commands: {}", summary.commands);
    println!("events: {}", summary.events);
    println!(
        "first_snapshot_frame: {}",
        summary
            .first_snapshot_frame
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!(
        "last_snapshot_frame: {}",
        summary
            .last_snapshot_frame
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!("first_snapshot_units: {}", first_snapshot_units);
    println!("last_event_type: {}", last_event);
    println!("event_preview: {}", event_preview);
    println!("player_names: {}", player_names);
}

fn list_replay_ids(
    db_path: &PathBuf,
    max_count: Option<usize>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let db = ReplayDb::open(db_path)?;
    let mut replay_ids = db.replay_ids();
    if let Some(max_count) = max_count {
        replay_ids.truncate(max_count);
    }
    Ok(replay_ids)
}
